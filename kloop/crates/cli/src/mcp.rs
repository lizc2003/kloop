//! MCP glue: `[mcp.servers.<name>]` config parsing, startup connection with
//! degrade-to-warning, `{server}__{tool}` namespacing, and the `ToolSource`
//! adapter over [`kloop_mcp::McpClient`]. This module is the only place that
//! knows both core's tool seam and the MCP wire crate.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

use kloop_core::tools::SourceOutput;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpClient;
use kloop_mcp::McpNotification;
use kloop_mcp::McpResource;
use kloop_mcp::McpRpcError;
use kloop_mcp::McpServerCapabilities;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::ToolDef;
use kloop_server::McpServerState;
use kloop_server::McpServerStatus;
use kloop_server::McpToolInfo;
use kloop_server::McpTransportKind;

use crate::mcp_auth::CredentialStore;

/// Model-visible tool names must satisfy the providers' `[a-zA-Z0-9_-]`
/// pattern AND kloop's permission-rule grammar (alnum + `_` only, so a
/// "p"-persisted allow rule parses back on the next start). 64 is the
/// stricter (OpenAI-compat) length limit.
const MAX_TOOL_NAME_LEN: usize = 64;
const MAX_AGGREGATE_RESOURCE_ITEMS: usize = 10_000;
const MAX_AGGREGATE_RESOURCE_BYTES: usize = 4 * 1024 * 1024;
const TOOL_REFRESH_RETRY_DELAYS: [Duration; 2] =
    [Duration::from_millis(100), Duration::from_millis(500)];
const DYNAMIC_CATALOG_UNSUPPORTED: &str = "server advertises tools/list_changed, but this transport has no notification stream; the startup tool catalog will remain fixed";

fn dynamic_catalog_message(
    capabilities: &McpServerCapabilities,
    has_notifications: bool,
) -> Option<&'static str> {
    (capabilities.tools_list_changed && !has_notifications).then_some(DYNAMIC_CATALOG_UNSUPPORTED)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct McpServerConfig {
    pub name: String,
    pub transport: McpTransport,
    /// Raw (un-prefixed) tool names the user vouches are read-only: eligible
    /// for concurrent dispatch. Everything else runs serial.
    pub readonly: Vec<String>,
}

/// How to reach a server: a local child over stdio, or a remote endpoint over
/// streamable HTTP. `command` vs `url` in the config selects between them
/// (untagged, like codex).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        /// Name of the env var holding a static bearer token — never the token
        /// itself (secrets don't belong in config; codex form). Mutually
        /// exclusive with the OAuth path below.
        bearer_token_env_var: Option<String>,
        /// Static extra request headers.
        http_headers: BTreeMap<String, String>,
        /// OAuth (plan 34b): a preconfigured client_id to skip dynamic client
        /// registration. Only meaningful on the OAuth path (no static bearer).
        oauth_client_id: Option<String>,
        /// OAuth: scopes to request at login, overriding those advertised by
        /// discovery. Empty = let discovery/the server decide.
        oauth_scopes: Vec<String>,
    },
}

impl McpTransport {
    fn kind(&self) -> McpTransportKind {
        match self {
            McpTransport::Stdio { .. } => McpTransportKind::Stdio,
            McpTransport::Http { .. } => McpTransportKind::Http,
        }
    }
}

/// Runtime tool sources plus the immutable startup-discovery snapshot exposed
/// by the native protocol. Failed configured servers stay visible in statuses
/// even though they contribute no ToolSource.
pub struct McpConnections {
    pub sources: Vec<Arc<dyn ToolSource>>,
    pub statuses: Vec<McpServerStatus>,
}

const STDIO_ONLY_KEYS: &[&str] = &["command", "env"];
const HTTP_ONLY_KEYS: &[&str] = &[
    "url",
    "bearer_token_env_var",
    "http_headers",
    "oauth_client_id",
    "oauth_scopes",
];

/// Parse `[mcp.servers.<name>]` tables from the global user config. A missing
/// section is an empty list; malformed or unknown keys are errors because a
/// silent drop would look like a vanished server.
pub fn load_mcp_servers(root: &toml::Table) -> Result<Vec<McpServerConfig>> {
    let Some(mcp) = root.get("mcp") else {
        return Ok(Vec::new());
    };
    let mcp = mcp.as_table().context("[mcp] must be a table")?;
    for key in mcp.keys() {
        if key != "servers" {
            bail!("[mcp] has unknown key '{key}' (servers)");
        }
    }
    let Some(servers) = mcp.get("servers") else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_table()
        .context("[mcp.servers] must be a table of server tables")?;
    let mut out = Vec::new();
    for (name, spec) in servers {
        let spec = spec
            .as_table()
            .with_context(|| format!("[mcp.servers.{name}] must be a table"))?;
        out.push(parse_server(name, spec)?);
    }
    Ok(out)
}

fn parse_server(name: &str, spec: &toml::Table) -> Result<McpServerConfig> {
    // Secrets belong in an env var referenced by name, never inline.
    if spec.contains_key("bearer_token") {
        bail!(
            "[mcp.servers.{name}] has plaintext 'bearer_token'; use \
             bearer_token_env_var = \"<ENV_VAR_NAME>\" (secrets don't belong in config)"
        );
    }
    let str_list = |key: &str| -> Result<Vec<String>> {
        let Some(entries) = spec.get(key) else {
            return Ok(Vec::new());
        };
        entries
            .as_array()
            .and_then(|list| {
                list.iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .with_context(|| format!("[mcp.servers.{name}].{key} must be a string array"))
    };
    let str_table = |key: &str| -> Result<BTreeMap<String, String>> {
        let mut map = BTreeMap::new();
        if let Some(table) = spec.get(key) {
            let table = table
                .as_table()
                .with_context(|| format!("[mcp.servers.{name}].{key} must be a table"))?;
            for (k, v) in table {
                let v = v
                    .as_str()
                    .with_context(|| format!("[mcp.servers.{name}].{key}.{k} must be a string"))?;
                map.insert(k.clone(), v.to_string());
            }
        }
        Ok(map)
    };

    let has_command = spec.contains_key("command");
    let has_url = spec.contains_key("url");
    let (transport, allowed_extra): (McpTransport, &[&str]) = match (has_command, has_url) {
        (true, true) => {
            bail!("[mcp.servers.{name}] sets both 'command' and 'url'; pick one transport")
        }
        (false, false) => {
            bail!("[mcp.servers.{name}] must set 'command' (stdio) or 'url' (remote http)")
        }
        (true, false) => {
            let command = str_list("command")?;
            if command.is_empty() {
                bail!("[mcp.servers.{name}].command must not be empty");
            }
            (
                McpTransport::Stdio {
                    command,
                    env: str_table("env")?,
                },
                HTTP_ONLY_KEYS,
            )
        }
        (false, true) => {
            let url = spec["url"]
                .as_str()
                .with_context(|| format!("[mcp.servers.{name}].url must be a string"))?
                .to_string();
            let bearer_token_env_var = match spec.get("bearer_token_env_var") {
                Some(v) => Some(
                    v.as_str()
                        .with_context(|| {
                            format!("[mcp.servers.{name}].bearer_token_env_var must be a string")
                        })?
                        .to_string(),
                ),
                None => None,
            };
            let oauth_client_id = match spec.get("oauth_client_id") {
                Some(v) => Some(
                    v.as_str()
                        .with_context(|| {
                            format!("[mcp.servers.{name}].oauth_client_id must be a string")
                        })?
                        .to_string(),
                ),
                None => None,
            };
            let oauth_scopes = str_list("oauth_scopes")?;
            // Static bearer and OAuth are two different auth modes; OAuth keys
            // under a static-bearer server would silently do nothing.
            if bearer_token_env_var.is_some()
                && (oauth_client_id.is_some() || !oauth_scopes.is_empty())
            {
                bail!(
                    "[mcp.servers.{name}] mixes bearer_token_env_var (static bearer) with \
                     oauth_client_id/oauth_scopes; pick one auth mode"
                );
            }
            (
                McpTransport::Http {
                    url,
                    bearer_token_env_var,
                    http_headers: str_table("http_headers")?,
                    oauth_client_id,
                    oauth_scopes,
                },
                STDIO_ONLY_KEYS,
            )
        }
    };

    for key in spec.keys() {
        if allowed_extra.contains(&key.as_str()) {
            bail!("[mcp.servers.{name}] has '{key}', which does not apply to this transport",);
        }
        let known = STDIO_ONLY_KEYS.contains(&key.as_str())
            || HTTP_ONLY_KEYS.contains(&key.as_str())
            || key == "readonly";
        if !known {
            bail!("[mcp.servers.{name}] has unknown key '{key}'");
        }
    }

    Ok(McpServerConfig {
        name: name.to_string(),
        transport,
        readonly: str_list("readonly")?,
    })
}

/// Build the extra HTTP request headers for a remote server: the static
/// `http_headers` plus, if `bearer_token_env_var` is set, an `Authorization:
/// Bearer <token>` resolved from the process environment (a missing env var is
/// an error — a silently-unauthenticated request would just 401).
fn http_headers_for(
    bearer_token_env_var: &Option<String>,
    http_headers: &BTreeMap<String, String>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<BTreeMap<String, String>> {
    let mut headers = http_headers.clone();
    if let Some(var) = bearer_token_env_var {
        let token = env(var).with_context(|| {
            format!("bearer_token_env_var '{var}' is not set in the environment")
        })?;
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }
    Ok(headers)
}

/// `{server}__{tool}` with every character outside `[A-Za-z0-9_]` replaced
/// by `_`. `-` is folded too (unlike cc): kloop's permission-rule parser
/// accepts only alnum + `_` tool names, and persisted allow rules must
/// round-trip.
pub fn qualified_name(server: &str, tool: &str) -> String {
    let sanitize = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    format!("{}__{}", sanitize(server), sanitize(tool))
}

/// One immutable tool-generation snapshot. A tools/list_changed refresh builds
/// the complete replacement first, then publishes it under one write lock.
struct McpToolCatalog {
    generation: u64,
    defs: Arc<[ToolDef]>,
    raw_names: HashMap<String, String>,
    readonly: HashSet<String>,
}

/// One connected server exposed through core's tool seam. Calls hold the read
/// side of `refresh_gate` through the wire request; refresh takes the write side
/// before listing and publishing. Thus an old-generation call either finishes
/// before publication or observes the replacement and rejects as stale.
struct McpToolSource {
    client: Arc<McpClient>,
    server: McpServerConfig,
    catalog: RwLock<Arc<McpToolCatalog>>,
    refresh_gate: tokio::sync::RwLock<()>,
}

impl McpToolSource {
    async fn refresh(&self) -> Result<()> {
        let _refresh = self.refresh_gate.write().await;
        let advertised = self.client.list_tools().await?;
        let mut catalog = self.catalog.write().unwrap();
        let generation = catalog.generation.wrapping_add(1);
        let next = Arc::new(build_catalog(&self.server, generation, advertised, &|_| {}));
        *catalog = next;
        Ok(())
    }
}

impl ToolSource for McpToolSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        self.catalog.read().unwrap().defs.clone()
    }

    fn definition_generation(&self, _tool: &str) -> u64 {
        self.catalog.read().unwrap().generation
    }

    fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
        let catalog = self.catalog.read().unwrap();
        catalog
            .defs
            .iter()
            .find(|def| def.name == tool)
            .cloned()
            .map(|def| (def, catalog.generation))
    }

    fn is_readonly(&self, tool: &str) -> bool {
        self.catalog.read().unwrap().readonly.contains(tool)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        self.call_at_generation(tool, input, None)
    }

    fn call_at_generation<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        generation: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            let _call = self.refresh_gate.read().await;
            let catalog = self.catalog.read().unwrap().clone();
            if generation.is_some_and(|generation| generation != catalog.generation) {
                bail!(
                    "MCP tool definition changed after discovery; run tool_search for {tool} again before retrying"
                );
            }
            let raw = catalog
                .raw_names
                .get(tool)
                .cloned()
                .with_context(|| format!("unknown mcp tool: {tool}"))?;
            let structured = self.client.call_tool_structured(&raw, input).await?;
            let text = kloop_mcp::render_result(&structured);
            let blocks = kloop_mcp::content_blocks(&structured["content"]);
            Ok(SourceOutput {
                text,
                blocks,
                structured: Some(structured),
            })
        })
    }
}

/// Namespace a server's advertised tools, dropping any oversized or sanitized
/// collision. The same builder is used at startup and for every refresh.
fn build_catalog(
    server: &McpServerConfig,
    generation: u64,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> McpToolCatalog {
    let readonly_raw: HashSet<&str> = server.readonly.iter().map(String::as_str).collect();
    let mut defs = Vec::new();
    let mut raw_names = HashMap::new();
    let mut readonly = HashSet::new();
    for mut def in advertised {
        let qualified = qualified_name(&server.name, &def.name);
        if qualified.len() > MAX_TOOL_NAME_LEN {
            warn(&format!(
                "mcp server '{}': tool name '{qualified}' exceeds {MAX_TOOL_NAME_LEN} chars; skipped",
                server.name
            ));
            continue;
        }
        if raw_names.contains_key(&qualified) {
            warn(&format!(
                "mcp server '{}': tool name collision on '{qualified}' after sanitization; \
                 the later tool is skipped",
                server.name
            ));
            continue;
        }
        if readonly_raw.contains(def.name.as_str()) {
            readonly.insert(qualified.clone());
        }
        raw_names.insert(qualified.clone(), std::mem::take(&mut def.name));
        def.name = qualified;
        defs.push(def);
    }
    McpToolCatalog {
        generation,
        defs: Arc::from(defs),
        raw_names,
        readonly,
    }
}

fn build_source(
    server: &McpServerConfig,
    client: Arc<McpClient>,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> Arc<McpToolSource> {
    Arc::new(McpToolSource {
        client,
        server: server.clone(),
        catalog: RwLock::new(Arc::new(build_catalog(server, 0, advertised, warn))),
        refresh_gate: tokio::sync::RwLock::new(()),
    })
}

async fn retry_tool_refresh_with_delays<F, Fut>(delays: &[Duration], mut refresh: F) -> Result<()>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<()>>,
{
    let mut last_error = None;
    for attempt in 0..=delays.len() {
        match refresh().await {
            Ok(()) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
        if let Some(delay) = delays.get(attempt) {
            tokio::time::sleep(*delay).await;
        }
    }
    Err(last_error.expect("refresh loop always attempts at least once"))
}

fn spawn_tool_refresh(
    source: Arc<McpToolSource>,
    mut notifications: tokio::sync::broadcast::Receiver<McpNotification>,
) {
    tokio::spawn(async move {
        loop {
            let refresh_needed = match notifications.recv().await {
                Ok(McpNotification::ToolsListChanged)
                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                Ok(McpNotification::ResourcesListChanged)
                | Ok(McpNotification::ResourceUpdated { .. }) => false,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if !refresh_needed {
                continue;
            }

            let mut closed = false;
            loop {
                match notifications.try_recv() {
                    Ok(_) | Err(tokio::sync::broadcast::error::TryRecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::TryRecvError::Empty) => break,
                    Err(tokio::sync::broadcast::error::TryRecvError::Closed) => {
                        closed = true;
                        break;
                    }
                }
            }
            let _ = retry_tool_refresh_with_delays(&TOOL_REFRESH_RETRY_DELAYS, || source.refresh())
                .await;
            if closed {
                break;
            }
        }
    });
}

const LIST_MCP_RESOURCES: &str = "list_mcp_resources";
const READ_MCP_RESOURCE: &str = "read_mcp_resource";
const READ_MCP_RESOURCE_DIR: &str = "read_mcp_resource_dir";

#[derive(Clone)]
struct McpResourceServer {
    name: String,
    client: Arc<McpClient>,
    capabilities: McpServerCapabilities,
}

struct McpResourceSource {
    defs: Arc<[ToolDef]>,
    servers: Vec<McpResourceServer>,
}

impl McpResourceSource {
    fn new(servers: Vec<McpResourceServer>) -> Self {
        let defs = vec![
            ToolDef {
                name: LIST_MCP_RESOURCES.into(),
                description: "List resources advertised by configured MCP servers. Pass server to filter; omit it to aggregate every resources-capable server. Returned entries preserve standard MCP fields and add server.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {"type": "string", "description": "Optional MCP server name"}
                    },
                    "additionalProperties": false
                }),
            },
            ToolDef {
                name: READ_MCP_RESOURCE.into(),
                description: "Read one currently advertised MCP resource by server and URI. The call requires normal external-tool approval. Text is returned inline; supported image blobs become image blocks; other blobs are reported without injecting base64 into the model context.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {"type": "string"},
                        "uri": {"type": "string"}
                    },
                    "required": ["server", "uri"],
                    "additionalProperties": false
                }),
            },
            ToolDef {
                name: READ_MCP_RESOURCE_DIR.into(),
                description: "List direct children of a currently advertised MCP directory resource through the io.modelcontextprotocol/skills directoryRead extension. The call requires normal external-tool approval and the listing is not recursive.".into(),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "server": {"type": "string"},
                        "uri": {"type": "string"}
                    },
                    "required": ["server", "uri"],
                    "additionalProperties": false
                }),
            },
        ];
        McpResourceSource {
            defs: Arc::from(defs),
            servers,
        }
    }

    fn server(&self, requested: &str) -> Result<McpResourceServer> {
        let normalized = normalize_server_name(requested);
        self.servers
            .iter()
            .find(|server| {
                server.name == requested || normalize_server_name(&server.name) == normalized
            })
            .cloned()
            .with_context(|| {
                let available = self
                    .servers
                    .iter()
                    .map(|server| server.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Server \"{requested}\" not found. Available servers: {available}")
            })
    }

    async fn list(&self, requested: Option<&str>) -> Result<SourceOutput> {
        let selected: Vec<McpResourceServer> = match requested {
            Some(name) => vec![self.server(name)?],
            None => self.servers.clone(),
        };
        let mut tasks = tokio::task::JoinSet::new();
        for (index, server) in selected.into_iter().enumerate() {
            tasks.spawn(async move {
                let result = server.client.list_resources().await;
                (index, server.name, result)
            });
        }
        let mut groups = Vec::new();
        let mut failures = Vec::new();
        loop {
            let next = tasks.join_next().await;
            let Some(result) = next else {
                break;
            };
            let (index, server, result) = result.context("MCP resource listing task failed")?;
            match result {
                Ok(resources) => groups.push((index, server, resources)),
                Err(error) => failures.push((index, server, error)),
            }
        }
        groups.sort_by_key(|(index, _, _)| *index);
        failures.sort_by_key(|(index, _, _)| *index);
        if groups.is_empty() && failures.len() == 1 {
            let (_, server, error) = failures.pop().unwrap();
            return Err(error)
                .with_context(|| format!("MCP resource listing failed for server \"{server}\""));
        }
        if groups.is_empty() && !failures.is_empty() {
            let names = failures
                .iter()
                .map(|(_, server, _)| server.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            bail!("MCP resource listing failed for every selected server: {names}");
        }

        let mut values = Vec::new();
        let mut total_bytes = 0;
        for (_, server, resources) in groups {
            for resource in &resources {
                let value = resource_value(resource, &server);
                total_bytes += serde_json::to_vec(&value)?.len();
                if values.len() >= MAX_AGGREGATE_RESOURCE_ITEMS
                    || total_bytes > MAX_AGGREGATE_RESOURCE_BYTES
                {
                    bail!("aggregated MCP resource catalog exceeds the tool result budget");
                }
                values.push(value);
            }
        }
        let failed_servers = failures
            .iter()
            .map(|(_, server, _)| server.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let partial_failure = (!failed_servers.is_empty())
            .then(|| format!("\n\nResource listing failed for server(s): {failed_servers}"));
        if values.is_empty() {
            return Ok(SourceOutput {
                text: format!(
                    "No resources found. MCP servers may still provide tools even if they have no resources.{}",
                    partial_failure.as_deref().unwrap_or("")
                ),
                blocks: None,
                structured: Some(Value::Array(Vec::new())),
            });
        }
        Ok(SourceOutput {
            text: format!(
                "{}{}",
                serde_json::to_string(&values)?,
                partial_failure.as_deref().unwrap_or("")
            ),
            blocks: None,
            structured: Some(Value::Array(values)),
        })
    }

    async fn require_advertised_uri(&self, server: &McpResourceServer, uri: &str) -> Result<()> {
        let resources =
            server.client.list_resources().await.with_context(|| {
                format!("cannot refresh resources from server \"{}\"", server.name)
            })?;
        if resources.iter().any(|resource| resource.uri == uri) {
            return Ok(());
        }
        bail!(
            "Resource URI is not currently advertised by server \"{}\": {uri}. Re-run list_mcp_resources to refresh.",
            server.name
        )
    }

    async fn read(&self, server_name: &str, uri: &str) -> Result<SourceOutput> {
        let server = self.server(server_name)?;
        self.require_advertised_uri(&server, uri).await?;
        let read = match server.client.read_resource(uri).await {
            Ok(read) => read,
            Err(error) => match rpc_code(&error) {
                Some(-32601) => bail!(
                    "Server \"{}\" advertises resource support but does not implement resource reads.",
                    server.name
                ),
                Some(-32002 | -32602) => {
                    let directory_hint = server.capabilities.directory_read.then_some(
                        " If the URI is a directory resource, use read_mcp_resource_dir instead.",
                    );
                    bail!(
                        "Resource not found: {uri} — it may have been deleted or the URI is stale. Re-run list_mcp_resources to refresh.{}",
                        directory_hint.unwrap_or("")
                    )
                }
                _ => return Err(error),
            },
        };
        let mut rendered = Vec::new();
        let mut images = Vec::new();
        for content in &read.contents {
            if let Some(text) = &content.text {
                rendered.push(json!({
                    "uri": content.uri,
                    "mimeType": content.mime_type,
                    "text": text,
                }));
                continue;
            }
            let mime = content
                .mime_type
                .as_deref()
                .unwrap_or("application/octet-stream");
            if supported_image_mime(mime) {
                if let Some(blob) = &content.blob {
                    images.push(ContentBlock::Image {
                        source: ImageSource::Base64 {
                            media_type: mime.to_string(),
                            data: blob.clone(),
                        },
                    });
                }
                rendered.push(json!({
                    "uri": content.uri,
                    "mimeType": content.mime_type,
                    "text": format!("[binary image resource from {} at {}]", server.name, content.uri),
                }));
            } else {
                rendered.push(json!({
                    "uri": content.uri,
                    "mimeType": content.mime_type,
                    "text": format!("Binary resource from {} at {} was not injected into model context", server.name, content.uri),
                }));
            }
        }
        let value = json!({"contents": rendered});
        let text = serde_json::to_string(&value)?;
        let blocks = if images.is_empty() {
            None
        } else {
            let mut blocks = vec![ContentBlock::Text { text: text.clone() }];
            blocks.extend(images);
            Some(blocks)
        };
        Ok(SourceOutput {
            text,
            blocks,
            structured: Some(read.raw),
        })
    }

    async fn directory(&self, server_name: &str, uri: &str) -> Result<SourceOutput> {
        let server = self.server(server_name)?;
        if !server.capabilities.directory_read {
            bail!(
                "Server \"{}\" does not support directory listing.",
                server.name
            );
        }
        self.require_advertised_uri(&server, uri).await?;
        let resources = match server.client.read_resource_directory(uri).await {
            Ok(resources) => resources,
            Err(error) if rpc_code(&error) == Some(-32602) => {
                bail!(
                    "Not a directory resource: {uri}. If it is a file resource, use read_mcp_resource instead."
                )
            }
            Err(error) => return Err(error),
        };
        let values: Vec<Value> = resources
            .iter()
            .map(|resource| {
                json!({
                    "uri": clean_resource_text(&resource.uri),
                    "name": clean_resource_text(&resource.name),
                    "mimeType": resource.mime_type.as_deref().map(clean_resource_text),
                })
            })
            .collect();
        let value = json!({"resources": values});
        let listing = if resources.is_empty() {
            "Directory is empty.".to_string()
        } else {
            let names = resources
                .iter()
                .map(|resource| {
                    let suffix = if resource.mime_type.as_deref() == Some("inode/directory") {
                        "/"
                    } else {
                        ""
                    };
                    format!("{}{suffix}", clean_resource_text(&resource.name))
                })
                .collect::<Vec<_>>()
                .join("\n");
            format!("Directory listing ({} entries):\n{names}", resources.len())
        };
        Ok(SourceOutput {
            text: format!("{listing}\n\n{}", serde_json::to_string(&value)?),
            blocks: None,
            structured: Some(value),
        })
    }
}

impl ToolSource for McpResourceSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        self.defs.clone()
    }

    fn should_defer(&self, _tool: &str) -> bool {
        true
    }

    fn is_readonly(&self, _tool: &str) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            match tool {
                LIST_MCP_RESOURCES => {
                    let server = input.get("server").and_then(Value::as_str);
                    self.list(server).await
                }
                READ_MCP_RESOURCE => {
                    let server = required_resource_arg(input, "server", tool)?;
                    let uri = required_resource_arg(input, "uri", tool)?;
                    self.read(server, uri).await
                }
                READ_MCP_RESOURCE_DIR => {
                    let server = required_resource_arg(input, "server", tool)?;
                    let uri = required_resource_arg(input, "uri", tool)?;
                    self.directory(server, uri).await
                }
                _ => bail!("unknown MCP resource tool: {tool}"),
            }
        })
    }
}

fn required_resource_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .with_context(|| format!("{tool}: missing required string argument '{key}'"))
}

fn normalize_server_name(name: &str) -> String {
    name.chars()
        .filter(|character| character.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn resource_value(resource: &McpResource, server: &str) -> Value {
    let mut value = resource.raw.clone();
    if let Some(object) = value.as_object_mut() {
        object.insert("server".into(), Value::String(server.to_string()));
    }
    value
}

fn rpc_code(error: &anyhow::Error) -> Option<i64> {
    error
        .downcast_ref::<McpRpcError>()
        .map(|rpc_error| rpc_error.code)
}

fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn clean_resource_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    *character,
                    '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
                )
        })
        .collect()
}

/// Spawn + handshake + tool discovery for every configured server. A failing
/// server degrades to a warning and is skipped — MCP never blocks startup.
pub async fn connect_servers(
    servers: Vec<McpServerConfig>,
    warn: &dyn Fn(&str),
) -> Result<McpConnections> {
    let store = Arc::new(CredentialStore::default_path()?);
    let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
    let mut resource_servers = Vec::new();
    let mut statuses = Vec::new();
    for server in servers {
        let transport = server.transport.kind();
        // A remote server with no static bearer takes the OAuth path: use a
        // stored token if the user has logged in; else connect unauthenticated
        // and, if that's rejected, point them at the login command.
        let oauth_capable = matches!(
            &server.transport,
            McpTransport::Http {
                bearer_token_env_var: None,
                ..
            }
        );
        let connect = async {
            let client = match &server.transport {
                McpTransport::Stdio { command, env } => McpClient::spawn(command, env)?,
                McpTransport::Http {
                    url,
                    bearer_token_env_var,
                    http_headers,
                    ..
                } => {
                    let headers = http_headers_for(bearer_token_env_var, http_headers, &|name| {
                        std::env::var(name).ok()
                    })?;
                    let oauth = if bearer_token_env_var.is_none() {
                        store.session_for(&server.name, url)?
                    } else {
                        None
                    };
                    McpClient::http(url.clone(), headers, oauth)?
                }
            };
            let client = Arc::new(client);
            let capabilities = client.initialize().await?;
            let notifications = capabilities
                .tools_list_changed
                .then(|| client.subscribe_notifications())
                .flatten();
            let advertised = client.list_tools().await?;
            anyhow::Ok((client, capabilities, notifications, advertised))
        };
        match connect.await {
            Ok((client, capabilities, notifications, advertised)) => {
                let count = advertised.len();
                let source = build_source(&server, client.clone(), advertised, warn);
                let tools = source
                    .defs()
                    .iter()
                    .map(|def| McpToolInfo {
                        name: def.name.clone(),
                        description: def.description.clone(),
                    })
                    .collect();
                let dynamic_message =
                    dynamic_catalog_message(&capabilities, notifications.is_some())
                        .map(str::to_string);
                if let Some(message) = &dynamic_message {
                    warn(&format!("mcp server '{}': {message}", server.name));
                }
                warn(&format!(
                    "mcp server '{}': connected, {count} tool(s)",
                    server.name
                ));
                statuses.push(McpServerStatus {
                    name: server.name.clone(),
                    transport,
                    state: McpServerState::Connected,
                    tools,
                    message: dynamic_message,
                });
                if capabilities.resources {
                    resource_servers.push(McpResourceServer {
                        name: server.name.clone(),
                        client,
                        capabilities,
                    });
                }
                if let Some(notifications) = notifications {
                    spawn_tool_refresh(source.clone(), notifications);
                }
                sources.push(source);
            }
            Err(_) => {
                let hint = if oauth_capable {
                    format!("; if it needs OAuth, run: kloop mcp login {}", server.name)
                } else {
                    String::new()
                };
                warn(&format!(
                    "mcp server '{}' unavailable, skipped{hint}",
                    server.name
                ));
                statuses.push(McpServerStatus {
                    name: server.name,
                    transport,
                    state: McpServerState::Unavailable,
                    tools: Vec::new(),
                    // Never reflect the raw anyhow chain onto the protocol or
                    // warning stream: Desktop forwards stderr into the WebView,
                    // and errors may quote URL credentials, commands, or env.
                    message: Some("connection or tool discovery failed; see engine log".into()),
                });
            }
        }
    }
    if !resource_servers.is_empty() {
        sources.push(Arc::new(McpResourceSource::new(resource_servers)));
    }
    Ok(McpConnections { sources, statuses })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ResourceServerEnd {
        lines: tokio::io::Lines<tokio::io::BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        writer: tokio::io::WriteHalf<tokio::io::DuplexStream>,
    }

    impl ResourceServerEnd {
        async fn recv(&mut self) -> Value {
            let line = self
                .lines
                .next_line()
                .await
                .unwrap()
                .expect("client closed unexpectedly");
            serde_json::from_str(&line).unwrap()
        }

        async fn send(&mut self, message: Value) {
            use tokio::io::AsyncWriteExt;
            let mut line = message.to_string();
            line.push('\n');
            self.writer.write_all(line.as_bytes()).await.unwrap();
        }

        async fn respond(&mut self, id: &Value, result: Value) {
            self.send(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                .await;
        }

        async fn respond_error(&mut self, id: &Value, code: i64, message: &str) {
            self.send(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": code, "message": message}
            }))
            .await;
        }
    }

    fn resource_pair(name: &str) -> (McpResourceServer, ResourceServerEnd) {
        use tokio::io::AsyncBufReadExt;
        let (client_io, server_io) = tokio::io::duplex(1 << 16);
        let (reader, writer) = tokio::io::split(client_io);
        let (server_reader, server_writer) = tokio::io::split(server_io);
        (
            McpResourceServer {
                name: name.into(),
                client: Arc::new(McpClient::over(reader, writer, None)),
                capabilities: McpServerCapabilities {
                    resources: true,
                    ..Default::default()
                },
            },
            ResourceServerEnd {
                lines: tokio::io::BufReader::new(server_reader).lines(),
                writer: server_writer,
            },
        )
    }

    fn config(content: &str) -> toml::Table {
        content.parse().unwrap()
    }

    #[test]
    fn load_mcp_servers_full_round_trip() {
        let root = config(
            r#"
[permissions]
allow = ["bash(ls *)"]

[mcp.servers.memory]
command = ["npx", "-y", "@modelcontextprotocol/server-memory"]
env = { NODE_ENV = "production" }
readonly = ["read_graph", "search_nodes"]

[mcp.servers.fs]
command = ["mcp-fs"]

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
bearer_token_env_var = "EXAMPLE_MCP_TOKEN"
http_headers = { X-Tenant = "acme" }
readonly = ["search"]

[mcp.servers.oauthy]
url = "https://oauth.example.com/mcp"
oauth_client_id = "preconfigured-123"
oauth_scopes = ["mcp.read", "mcp.write"]
"#,
        );
        let servers = load_mcp_servers(&root).unwrap();
        assert_eq!(
            servers,
            vec![
                McpServerConfig {
                    name: "fs".into(),
                    transport: McpTransport::Stdio {
                        command: vec!["mcp-fs".into()],
                        env: BTreeMap::new(),
                    },
                    readonly: vec![],
                },
                McpServerConfig {
                    name: "memory".into(),
                    transport: McpTransport::Stdio {
                        command: vec![
                            "npx".into(),
                            "-y".into(),
                            "@modelcontextprotocol/server-memory".into()
                        ],
                        env: BTreeMap::from([("NODE_ENV".into(), "production".into())]),
                    },
                    readonly: vec!["read_graph".into(), "search_nodes".into()],
                },
                McpServerConfig {
                    name: "oauthy".into(),
                    transport: McpTransport::Http {
                        url: "https://oauth.example.com/mcp".into(),
                        bearer_token_env_var: None,
                        http_headers: BTreeMap::new(),
                        oauth_client_id: Some("preconfigured-123".into()),
                        oauth_scopes: vec!["mcp.read".into(), "mcp.write".into()],
                    },
                    readonly: vec![],
                },
                McpServerConfig {
                    name: "remote".into(),
                    transport: McpTransport::Http {
                        url: "https://mcp.example.com/mcp".into(),
                        bearer_token_env_var: Some("EXAMPLE_MCP_TOKEN".into()),
                        http_headers: BTreeMap::from([("X-Tenant".into(), "acme".into())]),
                        oauth_client_id: None,
                        oauth_scopes: vec![],
                    },
                    readonly: vec!["search".into()],
                },
            ]
        );
    }

    #[test]
    fn load_mcp_servers_missing_section_is_empty() {
        assert_eq!(load_mcp_servers(&toml::Table::new()).unwrap(), vec![]);
        let root = config("[permissions]\nallow = []\n");
        assert_eq!(load_mcp_servers(&root).unwrap(), vec![]);
    }

    #[test]
    fn load_mcp_servers_rejects_malformed_sections() {
        for (tag, bad) in [
            ("mcp-section", "mcp = 3\n"),
            ("mcp-unknown", "[mcp]\nother = true\n"),
            ("notransport", "[mcp.servers.x]\nenv = {}\n"),
            ("emptycmd", "[mcp.servers.x]\ncommand = []\n"),
            ("cmdstr", "[mcp.servers.x]\ncommand = \"npx\"\n"),
            (
                "unknown",
                "[mcp.servers.x]\ncommand = [\"a\"]\ntimeout = 5\n",
            ),
            (
                "badenv",
                "[mcp.servers.x]\ncommand = [\"a\"]\nenv = { K = 1 }\n",
            ),
            // command + url together, and stdio-only/http-only key crossover.
            (
                "both",
                "[mcp.servers.x]\ncommand = [\"a\"]\nurl = \"https://h/mcp\"\n",
            ),
            (
                "envonhttp",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nenv = { K = \"v\" }\n",
            ),
            (
                "headersonstdio",
                "[mcp.servers.x]\ncommand = [\"a\"]\nhttp_headers = { X = \"y\" }\n",
            ),
            // Plaintext secret is refused, pointing at bearer_token_env_var.
            (
                "plaintext",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = \"sk-secret\"\n",
            ),
            // Static bearer + OAuth keys is a contradiction.
            (
                "bearerplusoauth",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token_env_var = \"T\"\noauth_client_id = \"c\"\n",
            ),
        ] {
            let root = config(bad);
            assert!(load_mcp_servers(&root).is_err(), "{tag} should fail");
        }
    }

    #[test]
    fn http_headers_resolve_bearer_token_from_env() {
        let var = "KLOOP_TEST_MCP_TOKEN".to_string();
        let env = |name: &str| (name == var).then(|| "sk-abc".to_string());
        let headers = http_headers_for(
            &Some(var.clone()),
            &BTreeMap::from([("X-Tenant".into(), "acme".into())]),
            &env,
        )
        .unwrap();
        assert_eq!(
            headers,
            BTreeMap::from([
                ("Authorization".into(), "Bearer sk-abc".into()),
                ("X-Tenant".into(), "acme".into()),
            ])
        );
        // A referenced-but-unset env var is an error, not a silent no-auth.
        let missing_env = |_: &str| None;
        assert!(http_headers_for(&Some(var), &BTreeMap::new(), &missing_env).is_err());
        // No bearer var ⇒ just the static headers.
        assert_eq!(
            http_headers_for(
                &None,
                &BTreeMap::from([("A".into(), "b".into())]),
                &missing_env,
            )
            .unwrap(),
            BTreeMap::from([("A".into(), "b".into())])
        );
    }

    #[tokio::test]
    async fn failed_connections_remain_visible_without_leaking_details() {
        let warnings = std::sync::Mutex::new(Vec::new());
        let warn = |warning: &str| warnings.lock().unwrap().push(warning.to_string());
        let connections = connect_servers(
            vec![McpServerConfig {
                name: "broken".into(),
                transport: McpTransport::Stdio {
                    command: vec!["/definitely/missing/kloop-mcp-secret".into()],
                    env: BTreeMap::from([("TOKEN".into(), "SUPER-SECRET".into())]),
                },
                readonly: Vec::new(),
            }],
            &warn,
        )
        .await
        .unwrap();

        assert!(connections.sources.is_empty());
        assert_eq!(
            connections.statuses,
            vec![McpServerStatus {
                name: "broken".into(),
                transport: McpTransportKind::Stdio,
                state: McpServerState::Unavailable,
                tools: Vec::new(),
                message: Some("connection or tool discovery failed; see engine log".into()),
            }]
        );
        let wire = serde_json::to_string(&connections.statuses).unwrap();
        assert!(!wire.contains("SUPER-SECRET"));
        assert!(!wire.contains("kloop-mcp-secret"));
        let warnings = warnings.lock().unwrap();
        assert_eq!(
            *warnings,
            vec!["mcp server 'broken' unavailable, skipped".to_string()]
        );
        assert!(!warnings[0].contains("SUPER-SECRET"));
        assert!(!warnings[0].contains("kloop-mcp-secret"));
    }

    #[tokio::test]
    async fn resource_reads_require_a_currently_advertised_uri() {
        let (server, mut wire) = resource_pair("fixture");
        let source = McpResourceSource::new(vec![server]);
        let server_task = tokio::spawn(async move {
            let request = wire.recv().await;
            assert_eq!(request["method"], "resources/list");
            wire.respond(
                &request["id"],
                json!({
                    "resources": [{
                        "uri": "fixture://note",
                        "name": "note",
                        "mimeType": "text/plain"
                    }]
                }),
            )
            .await;
        });

        let error = match source.read("fixture", "fixture://secret").await {
            Ok(_) => panic!("unadvertised URI unexpectedly reached resources/read"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("not currently advertised"));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn resource_listing_propagates_single_server_errors() {
        let (server, mut wire) = resource_pair("broken");
        let source = McpResourceSource::new(vec![server]);
        let server_task = tokio::spawn(async move {
            let request = wire.recv().await;
            assert_eq!(request["method"], "resources/list");
            wire.respond_error(&request["id"], -32601, "not implemented")
                .await;
        });

        let error = match source.list(Some("broken")).await {
            Ok(_) => panic!("server failure was reported as an empty catalog"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("resource listing failed for server \"broken\"")
        );
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn aggregate_resource_listing_preserves_partial_failures() {
        let (good, mut good_wire) = resource_pair("good");
        let (bad, mut bad_wire) = resource_pair("bad");
        let source = McpResourceSource::new(vec![good, bad]);
        let good_task = tokio::spawn(async move {
            let request = good_wire.recv().await;
            good_wire
                .respond(
                    &request["id"],
                    json!({
                        "resources": [{
                            "uri": "fixture://note",
                            "name": "note",
                            "mimeType": "text/plain"
                        }]
                    }),
                )
                .await;
        });
        let bad_task = tokio::spawn(async move {
            let request = bad_wire.recv().await;
            bad_wire
                .respond_error(&request["id"], -32000, "temporarily unavailable")
                .await;
        });

        let output = source.list(None).await.unwrap();
        assert!(output.text.contains("\"server\":\"good\""));
        assert!(
            output
                .text
                .contains("Resource listing failed for server(s): bad")
        );
        assert_eq!(
            output.structured,
            Some(Value::Array(vec![json!({
                "uri": "fixture://note",
                "name": "note",
                "mimeType": "text/plain",
                "server": "good"
            })]))
        );
        good_task.await.unwrap();
        bad_task.await.unwrap();
    }

    #[test]
    fn dynamic_catalog_boundary_is_explicit_without_notifications() {
        let capabilities = McpServerCapabilities {
            tools_list_changed: true,
            ..Default::default()
        };
        assert_eq!(
            dynamic_catalog_message(&capabilities, false),
            Some(DYNAMIC_CATALOG_UNSUPPORTED)
        );
        assert_eq!(dynamic_catalog_message(&capabilities, true), None);
        assert_eq!(
            dynamic_catalog_message(&McpServerCapabilities::default(), false),
            None
        );
    }

    #[tokio::test]
    async fn mcp_source_rejects_calls_against_a_stale_definition_generation() {
        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let client = Arc::new(McpClient::over(reader, writer, None));
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly: Vec::new(),
        };
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "echo".into(),
                schema: json!({"type": "object"}),
            }],
            &|_| {},
        );

        let error = match source
            .call_at_generation("fixture__echo", &json!({}), Some(1))
            .await
        {
            Ok(_) => panic!("stale generation unexpectedly dispatched"),
            Err(error) => error,
        };
        assert!(
            error
                .to_string()
                .contains("definition changed after discovery")
        );
    }

    #[tokio::test]
    async fn refresh_publication_waits_for_in_flight_generation_calls() {
        let (resource_server, mut wire) = resource_pair("fixture");
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly: Vec::new(),
        };
        let source = build_source(
            &server,
            resource_server.client,
            vec![ToolDef {
                name: "echo".into(),
                description: "old".into(),
                schema: json!({"type": "object", "properties": {"old": {"type": "string"}}}),
            }],
            &|_| {},
        );

        let call_source = source.clone();
        let call = tokio::spawn(async move {
            call_source
                .call_at_generation("fixture__echo", &json!({"old": "x"}), Some(0))
                .await
        });
        let call_request = wire.recv().await;
        assert_eq!(call_request["method"], "tools/call");

        let refresh_source = source.clone();
        let refresh = tokio::spawn(async move { refresh_source.refresh().await });
        assert!(
            tokio::time::timeout(Duration::from_millis(50), wire.recv())
                .await
                .is_err(),
            "refresh must not publish or list while an old-generation call holds the gate"
        );

        wire.respond(
            &call_request["id"],
            json!({"content": [{"type": "text", "text": "old-result"}]}),
        )
        .await;
        let output = call.await.unwrap().unwrap();
        assert_eq!(output.text, "old-result");

        let list_request = wire.recv().await;
        assert_eq!(list_request["method"], "tools/list");
        wire.respond(
            &list_request["id"],
            json!({
                "tools": [{
                    "name": "echo",
                    "description": "new",
                    "inputSchema": {
                        "type": "object",
                        "properties": {"new": {"type": "string"}}
                    }
                }]
            }),
        )
        .await;
        refresh.await.unwrap().unwrap();
        assert_eq!(source.definition_generation("fixture__echo"), 1);
    }

    #[tokio::test]
    async fn tool_refresh_retries_transient_failures_with_a_bound() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        retry_tool_refresh_with_delays(&[Duration::ZERO, Duration::ZERO], move || {
            let seen = seen.clone();
            async move {
                let attempt = seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if attempt < 2 {
                    bail!("transient refresh failure");
                }
                Ok(())
            }
        })
        .await
        .unwrap();
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 3);

        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = attempts.clone();
        let error = retry_tool_refresh_with_delays(&[Duration::ZERO], move || {
            let seen = seen.clone();
            async move {
                seen.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                bail!("permanent refresh failure")
            }
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("permanent refresh failure"));
        assert_eq!(attempts.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn qualified_name_sanitizes_to_rule_safe_charset() {
        assert_eq!(qualified_name("memory", "read_graph"), "memory__read_graph");
        // '-' and '.' fold to '_' so persisted allow rules parse back.
        assert_eq!(
            qualified_name("github.com", "create-issue"),
            "github_com__create_issue"
        );
        assert_eq!(qualified_name("a b", "t!"), "a_b__t_");
    }
}
