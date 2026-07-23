//! MCP glue: `[mcp.servers.<name>]` config parsing, startup connection with
//! degrade-to-warning, `{server}__{tool}` namespacing, and the `ToolSource`
//! adapter over [`kloop_mcp::McpClient`]. This module is the only place that
//! knows both core's tool seam and the MCP wire crate.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

use kloop_core::tools::SourceOutput;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpClient;
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

/// Parse `[mcp.servers.<name>]` tables from `.kloop/config.toml`. A missing
/// file or missing section is an empty list; a malformed section is an error
/// (silent misconfiguration would look like a vanished server).
pub fn load_mcp_servers(config_path: &Path) -> Result<Vec<McpServerConfig>> {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return Ok(Vec::new());
    };
    let value: toml::Table = raw
        .parse()
        .with_context(|| format!("cannot parse {}", config_path.display()))?;
    let Some(servers) = value.get("mcp").and_then(|m| m.get("servers")) else {
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
) -> Result<BTreeMap<String, String>> {
    let mut headers = http_headers.clone();
    if let Some(var) = bearer_token_env_var {
        let token = std::env::var(var).with_context(|| {
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

/// One connected server exposed through core's tool seam. Names in `defs`
/// are qualified; the map recovers the raw wire name per call.
struct McpToolSource {
    client: McpClient,
    defs: Vec<ToolDef>,
    raw_names: HashMap<String, String>,
    readonly: HashSet<String>,
}

impl ToolSource for McpToolSource {
    fn defs(&self) -> &[ToolDef] {
        &self.defs
    }

    fn is_readonly(&self, tool: &str) -> bool {
        self.readonly.contains(tool)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            let raw = self
                .raw_names
                .get(tool)
                .with_context(|| format!("unknown mcp tool: {tool}"))?;
            // One wire call: the structured CallToolResult for a program, its
            // flattened text for the model-facing tool_result, and — when the
            // result carries a usable image — content blocks so the model sees
            // the picture instead of an `[image: …]` tag.
            let structured = self.client.call_tool_structured(raw, input).await?;
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

/// Namespace a server's advertised tools, dropping (with a warning) any
/// whose qualified name is oversized or collides after sanitization.
fn build_source(
    server: &McpServerConfig,
    client: McpClient,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> McpToolSource {
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
    McpToolSource {
        client,
        defs,
        raw_names,
        readonly,
    }
}

/// Spawn + handshake + tool discovery for every configured server. A failing
/// server degrades to a warning and is skipped — MCP never blocks startup.
pub async fn connect_servers(servers: Vec<McpServerConfig>, warn: &dyn Fn(&str)) -> McpConnections {
    let store = Arc::new(CredentialStore::default_path());
    let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
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
                    let headers = http_headers_for(bearer_token_env_var, http_headers)?;
                    let oauth = if bearer_token_env_var.is_none() {
                        store.session_for(&server.name, url)
                    } else {
                        None
                    };
                    McpClient::http(url.clone(), headers, oauth)?
                }
            };
            client.initialize().await?;
            let advertised = client.list_tools().await?;
            anyhow::Ok((client, advertised))
        };
        match connect.await {
            Ok((client, advertised)) => {
                let count = advertised.len();
                let source = build_source(&server, client, advertised, warn);
                let tools = source
                    .defs
                    .iter()
                    .map(|def| McpToolInfo {
                        name: def.name.clone(),
                        description: def.description.clone(),
                    })
                    .collect();
                warn(&format!(
                    "mcp server '{}': connected, {count} tool(s)",
                    server.name
                ));
                statuses.push(McpServerStatus {
                    name: server.name.clone(),
                    transport,
                    state: McpServerState::Connected,
                    tools,
                    message: None,
                });
                sources.push(Arc::new(source));
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
    McpConnections { sources, statuses }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_config(tag: &str, content: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-mcp-cfg-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn load_mcp_servers_full_round_trip() {
        let path = write_config(
            "full",
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
        let servers = load_mcp_servers(&path).unwrap();
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
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_mcp_servers_missing_file_and_section_are_empty() {
        assert_eq!(
            load_mcp_servers(Path::new("/nonexistent/kloop.toml")).unwrap(),
            vec![]
        );
        let path = write_config("nosection", "[permissions]\nallow = []\n");
        assert_eq!(load_mcp_servers(&path).unwrap(), vec![]);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn load_mcp_servers_rejects_malformed_sections() {
        for (tag, bad) in [
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
            let path = write_config(tag, bad);
            assert!(load_mcp_servers(&path).is_err(), "{tag} should fail");
            let _ = std::fs::remove_dir_all(path.parent().unwrap());
        }
    }

    #[test]
    fn http_headers_resolve_bearer_token_from_env() {
        // Uniquely-named var so the global-env read doesn't race sibling tests.
        let var = format!("KLOOP_TEST_MCP_TOKEN_{}", std::process::id());
        std::env::set_var(&var, "sk-abc");
        let headers = http_headers_for(
            &Some(var.clone()),
            &BTreeMap::from([("X-Tenant".into(), "acme".into())]),
        )
        .unwrap();
        assert_eq!(
            headers,
            BTreeMap::from([
                ("Authorization".into(), "Bearer sk-abc".into()),
                ("X-Tenant".into(), "acme".into()),
            ])
        );
        std::env::remove_var(&var);
        // A referenced-but-unset env var is an error, not a silent no-auth.
        assert!(http_headers_for(&Some(var), &BTreeMap::new()).is_err());
        // No bearer var ⇒ just the static headers.
        assert_eq!(
            http_headers_for(&None, &BTreeMap::from([("A".into(), "b".into())])).unwrap(),
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
        .await;

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
