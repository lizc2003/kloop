//! MCP glue: `[mcp.servers.<name>]` config parsing, startup connection with
//! degrade-to-warning, `{server}__{tool}` namespacing, and the `ToolSource`
//! adapter over [`kloop_mcp::McpClient`]. This module is the only place that
//! knows both core's tool seam and the MCP wire crate.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::RwLock;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

use sha2::Digest;
use sha2::Sha256;
use tokio_util::sync::CancellationToken;

use kloop_core::tools::SourceDefinitionState;
use kloop_core::tools::SourceOutput;
use kloop_core::tools::SourceVersion;
use kloop_core::tools::ToolSource;
use kloop_mcp::McpAuthenticationError;
use kloop_mcp::McpClient;
use kloop_mcp::McpNotification;
use kloop_mcp::McpResource;
use kloop_mcp::McpRpcError;
use kloop_mcp::McpServerCapabilities;
use kloop_mcp::McpSessionReinitialized;
use kloop_mcp::McpToolCallError;
use kloop_mcp::McpTransportFailure;
use kloop_mcp::McpTransportHealth;
use kloop_mcp::McpTransportState;
use kloop_mcp::oauth::OAuthSession;
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
    pub readonly_tools: Vec<String>,
}

/// How to reach a server: a local child over stdio, or a remote endpoint over
/// streamable HTTP. `command` vs `url` in the config selects between them
/// (untagged, like codex).
#[derive(Clone, PartialEq, Eq)]
pub enum McpTransport {
    Stdio {
        command: Vec<String>,
        env: BTreeMap<String, String>,
    },
    Http {
        url: String,
        /// A static bearer token, written here in full. `~/.kloop/config.toml`
        /// is where kloop's credentials live — the provider's `auth_header`
        /// and `[web].api_key` are already in this same 0600 file — and a
        /// secret reachable only through an exported variable is a working
        /// setup for whoever knows the variable's name and a silent failure
        /// for everyone else. Mutually exclusive with the OAuth path below.
        bearer_token: Option<String>,
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

/// Never prints a secret. Three fields here can hold one — the bearer token,
/// a static header's value, and a stdio child's environment — and a transport
/// reaches test failures and any future log through `{:?}`. Keys and names
/// stay: *what* is configured is the diagnostic, what it is set to is not.
impl fmt::Debug for McpTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            McpTransport::Stdio { command, env } => f
                .debug_struct("Stdio")
                .field("command", command)
                .field("env", &RedactedValues(env))
                .finish(),
            McpTransport::Http {
                url,
                bearer_token,
                http_headers,
                oauth_client_id,
                oauth_scopes,
            } => f
                .debug_struct("Http")
                .field("url", url)
                .field("bearer_token", &bearer_token.as_ref().map(|_| REDACTED))
                .field("http_headers", &RedactedValues(http_headers))
                .field("oauth_client_id", oauth_client_id)
                .field("oauth_scopes", oauth_scopes)
                .finish(),
        }
    }
}

const REDACTED: &str = "<redacted>";

/// A map printed as its keys, every value replaced.
struct RedactedValues<'a>(&'a BTreeMap<String, String>);

impl fmt::Debug for RedactedValues<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(self.0.keys().map(|key| (key, REDACTED)))
            .finish()
    }
}

impl McpTransport {
    fn kind(&self) -> McpTransportKind {
        match self {
            McpTransport::Stdio { .. } => McpTransportKind::Stdio,
            McpTransport::Http { .. } => McpTransportKind::Http,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct McpEndpointBinding {
    transport: McpTransportKind,
    digest: [u8; 32],
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpLifecycleState {
    Planned,
    Starting,
    Ready,
    Degraded,
    Failed,
    Stale,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpAuthAvailability {
    NotApplicable,
    StaticCredentialConfigured,
    StaticCredentialAvailable,
    StaticCredentialUnavailable,
    Unauthenticated,
    LoginRequired,
    OAuthAvailable,
    ReauthenticationRequired,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct McpCapabilitySummary {
    tools: Option<bool>,
    tools_list_changed: Option<bool>,
    resources: Option<bool>,
    resources_list_changed: Option<bool>,
    directory_read: Option<bool>,
}

impl From<&McpServerCapabilities> for McpCapabilitySummary {
    fn from(capabilities: &McpServerCapabilities) -> Self {
        Self {
            tools: Some(true),
            tools_list_changed: Some(capabilities.tools_list_changed),
            resources: Some(capabilities.resources),
            resources_list_changed: Some(capabilities.resources_list_changed),
            directory_read: Some(capabilities.directory_read),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpRefreshState {
    Unavailable,
    StartupFixed,
    Idle,
    Refreshing,
    Retrying { attempt: u8 },
    RetryExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpHealthResult {
    Unknown,
    Healthy,
    Degraded,
    Failed,
    Stale,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum McpFailureKind {
    Startup,
    Authentication,
    TransportClosed,
    TransportRead,
    TransportWrite,
    RequestTimeout,
    RefreshInProgress,
    Refresh,
    Request,
    NotificationClosed,
    Shutdown,
}

impl McpFailureKind {
    fn reason(self) -> &'static str {
        match self {
            Self::Startup => "startup handshake or tool discovery failed",
            Self::Authentication => "authentication is unavailable; reauthenticate before retrying",
            Self::TransportClosed => "transport connection closed",
            Self::TransportRead => "transport input failed",
            Self::TransportWrite => "transport output failed",
            Self::RequestTimeout => "the server did not respond before the request deadline",
            Self::RefreshInProgress => {
                "tool catalog refresh is in progress; retry after it settles"
            }
            Self::Refresh => {
                "tool catalog refresh failed; the last-known catalog is retained but stale"
            }
            Self::Request => {
                "an MCP request failed; the last-known catalog is retained but unavailable"
            }
            Self::NotificationClosed => "the transport notification channel closed",
            Self::Shutdown => "the MCP lifecycle owner shut the server down",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct McpReadinessReceipt {
    server_identity: String,
    endpoint: McpEndpointBinding,
    auth: McpAuthAvailability,
    capabilities: McpCapabilitySummary,
    catalog_generation: u64,
    readiness_revision: u64,
    lifecycle: McpLifecycleState,
    refresh: McpRefreshState,
    last_health: McpHealthResult,
    failure: Option<McpFailureKind>,
}

impl McpReadinessReceipt {
    fn new(server: &McpServerConfig) -> Self {
        Self {
            server_identity: server.name.clone(),
            endpoint: endpoint_binding(server),
            auth: initial_auth_availability(server),
            capabilities: McpCapabilitySummary::default(),
            catalog_generation: 0,
            readiness_revision: 0,
            lifecycle: McpLifecycleState::Planned,
            refresh: McpRefreshState::Unavailable,
            last_health: McpHealthResult::Unknown,
            failure: None,
        }
    }

    fn advance(&mut self) {
        self.readiness_revision = self
            .readiness_revision
            .checked_add(1)
            .expect("MCP readiness revision exhausted");
    }

    fn is_ready(&self) -> bool {
        self.lifecycle == McpLifecycleState::Ready
    }
}

fn endpoint_binding(server: &McpServerConfig) -> McpEndpointBinding {
    let mut digest = Sha256::new();
    let transport = server.transport.kind();
    match &server.transport {
        McpTransport::Stdio { command, env } => {
            digest.update(b"stdio\0");
            // Arguments may carry credentials. The executable identity and env
            // names bind the endpoint without creating a secret-derived oracle.
            if let Some(program) = command.first() {
                digest.update(program.len().to_be_bytes());
                digest.update(program.as_bytes());
            }
            for name in env.keys() {
                digest.update(name.len().to_be_bytes());
                digest.update(name.as_bytes());
            }
        }
        McpTransport::Http { url, .. } => {
            digest.update(b"http\0");
            if let Ok(parsed) = url::Url::parse(url) {
                digest.update(parsed.scheme().as_bytes());
                if let Some(host) = parsed.host_str() {
                    digest.update(host.as_bytes());
                }
                if let Some(port) = parsed.port_or_known_default() {
                    digest.update(port.to_be_bytes());
                }
                digest.update(b"http-origin\0");
            } else {
                digest.update(b"invalid-url");
            }
        }
    }
    McpEndpointBinding {
        transport,
        digest: digest.finalize().into(),
    }
}

fn initial_auth_availability(server: &McpServerConfig) -> McpAuthAvailability {
    match &server.transport {
        McpTransport::Stdio { .. } => McpAuthAvailability::NotApplicable,
        McpTransport::Http {
            bearer_token: Some(_),
            ..
        } => McpAuthAvailability::StaticCredentialConfigured,
        McpTransport::Http {
            bearer_token: None, ..
        } => McpAuthAvailability::Unauthenticated,
    }
}

struct McpLifecycleInner {
    cancel: CancellationToken,
    sources: Mutex<Vec<Arc<McpToolSource>>>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Drop for McpLifecycleInner {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in self.tasks.get_mut().unwrap().drain(..) {
            task.abort();
        }
        for source in self.sources.get_mut().unwrap() {
            source.abort_client();
        }
    }
}

/// Process-level owner for every MCP monitor, refresh worker and transport.
/// Frontends call `shutdown`; Drop is only the synchronous error-path fallback.
#[derive(Clone)]
pub struct McpLifecycleOwner {
    inner: Arc<McpLifecycleInner>,
}

impl Default for McpLifecycleOwner {
    fn default() -> Self {
        Self {
            inner: Arc::new(McpLifecycleInner {
                cancel: CancellationToken::new(),
                sources: Mutex::new(Vec::new()),
                tasks: Mutex::new(Vec::new()),
            }),
        }
    }
}

impl McpLifecycleOwner {
    fn register_source(&self, source: Arc<McpToolSource>) {
        self.inner.sources.lock().unwrap().push(source);
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        self.inner.tasks.lock().unwrap().push(tokio::spawn(future));
    }

    fn cancel_token(&self) -> CancellationToken {
        self.inner.cancel.clone()
    }

    pub async fn shutdown(&self) {
        self.inner.cancel.cancel();
        let sources = self.inner.sources.lock().unwrap().clone();
        for source in &sources {
            source.close(McpFailureKind::Shutdown);
        }
        for source in &sources {
            source.shutdown_client().await;
        }
        let tasks = std::mem::take(&mut *self.inner.tasks.lock().unwrap());
        for mut task in tasks {
            if tokio::time::timeout(Duration::from_secs(1), &mut task)
                .await
                .is_err()
            {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

/// Runtime tool sources plus the immutable startup-discovery snapshot exposed
/// by the native protocol. Failed configured servers stay visible in statuses
/// and retain an unavailable ToolSource route.
pub struct McpConnections {
    pub sources: Vec<Arc<dyn ToolSource>>,
    pub statuses: Vec<McpServerStatus>,
    pub lifecycle: McpLifecycleOwner,
}

const STDIO_ONLY_KEYS: &[&str] = &["command", "env"];
const HTTP_ONLY_KEYS: &[&str] = &[
    "url",
    "bearer_token",
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
        if !matches!(key.as_str(), "servers" | "defer_threshold") {
            bail!("[mcp] has unknown key '{key}' (servers | defer_threshold)");
        }
    }
    let Some(servers) = mcp.get("servers") else {
        return Ok(Vec::new());
    };
    let servers = servers
        .as_table()
        .context("[mcp.servers] must be a table of server tables")?;
    let mut out = Vec::new();
    let mut route_names = HashMap::new();
    for (name, spec) in servers {
        let component = qualified_name_component(name);
        if let Some(previous) = route_names.insert(component.clone(), name.clone()) {
            bail!(
                "[mcp.servers] names '{previous}' and '{name}' both normalize to '{component}'; rename one server so qualified tool routes stay unique"
            );
        }
        let spec = spec
            .as_table()
            .with_context(|| format!("[mcp.servers.{name}] must be a table"))?;
        out.push(parse_server(name, spec)?);
    }
    Ok(out)
}

fn parse_server(name: &str, spec: &toml::Table) -> Result<McpServerConfig> {
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
            let bearer_token = match spec.get("bearer_token") {
                Some(v) => {
                    let token = v.as_str().with_context(|| {
                        format!("[mcp.servers.{name}].bearer_token must be a string")
                    })?;
                    if token.is_empty() {
                        bail!("[mcp.servers.{name}].bearer_token must not be empty");
                    }
                    Some(token.to_string())
                }
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
            if bearer_token.is_some() && (oauth_client_id.is_some() || !oauth_scopes.is_empty()) {
                bail!(
                    "[mcp.servers.{name}] mixes bearer_token (static bearer) with \
                     oauth_client_id/oauth_scopes; pick one auth mode"
                );
            }
            (
                McpTransport::Http {
                    url,
                    bearer_token,
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
            || key == "readonly_tools";
        if !known {
            bail!("[mcp.servers.{name}] has unknown key '{key}'");
        }
    }

    Ok(McpServerConfig {
        name: name.to_string(),
        transport,
        readonly_tools: str_list("readonly_tools")?,
    })
}

/// Build the extra HTTP request headers for a remote server: the static
/// `http_headers` plus, if a `bearer_token` was configured, the
/// `Authorization: Bearer <token>` it spells. Nothing here can fail any more —
/// the token is in the config or it is not.
fn http_headers_for(
    bearer_token: &Option<String>,
    http_headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    let mut headers = http_headers.clone();
    if let Some(token) = bearer_token {
        headers.insert("Authorization".to_string(), format!("Bearer {token}"));
    }
    headers
}

/// `[mcp].defer_threshold`: the total tool count above which MCP tool
/// definitions are deferred behind `tool_search`. Lower it to exercise
/// deferral against a small server; raise it to effectively switch deferral
/// off. Absent, the built-in threshold applies.
pub(crate) fn load_defer_threshold(root: &toml::Table) -> Result<usize> {
    let Some(value) = root
        .get("mcp")
        .and_then(toml::Value::as_table)
        .and_then(|mcp| mcp.get("defer_threshold"))
    else {
        return Ok(kloop_core::tools::TOOL_DEFER_THRESHOLD);
    };
    let count = value
        .as_integer()
        .filter(|&number| number >= 0)
        .context("[mcp].defer_threshold must be a tool count")?;
    Ok(count as usize)
}

/// `{server}__{tool}` with every character outside `[A-Za-z0-9_]` replaced
/// by `_`. `-` is folded too (unlike cc): kloop's permission-rule parser
/// accepts only alnum + `_` tool names, and persisted allow rules must
/// round-trip.
fn qualified_name_component(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect()
}

pub fn qualified_name(server: &str, tool: &str) -> String {
    format!(
        "{}__{}",
        qualified_name_component(server),
        qualified_name_component(tool)
    )
}

/// One immutable tool-generation snapshot. A tools/list_changed refresh builds
/// the complete replacement first, then publishes it under one write lock.
struct McpToolCatalog {
    generation: u64,
    defs: Arc<[ToolDef]>,
    raw_names: HashMap<String, String>,
    readonly: HashSet<String>,
}

struct McpRuntimeState {
    terminal: bool,
    client: Option<Arc<McpClient>>,
    oauth: Option<Arc<OAuthSession>>,
    dynamic_refresh: bool,
    capabilities: McpServerCapabilities,
    catalog: Arc<McpToolCatalog>,
    receipt: McpReadinessReceipt,
}

/// One configured server exposed through core's tool seam, including startup
/// failures. Calls hold the read side of `refresh_gate` through the wire request;
/// refresh and transport-health transitions take the write side.
struct McpToolSource {
    server: McpServerConfig,
    qualified_prefix: String,
    state: RwLock<McpRuntimeState>,
    refresh_gate: tokio::sync::RwLock<()>,
}

impl McpToolSource {
    fn planned(server: &McpServerConfig) -> Arc<Self> {
        Arc::new(Self {
            server: server.clone(),
            qualified_prefix: format!("{}__", qualified_name_component(&server.name)),
            state: RwLock::new(McpRuntimeState {
                terminal: false,
                client: None,
                oauth: None,
                dynamic_refresh: false,
                capabilities: McpServerCapabilities::default(),
                catalog: Arc::new(build_catalog(server, 0, Vec::new(), &|_| {})),
                receipt: McpReadinessReceipt::new(server),
            }),
            refresh_gate: tokio::sync::RwLock::new(()),
        })
    }

    #[cfg(test)]
    fn receipt(&self) -> McpReadinessReceipt {
        self.state.read().unwrap().receipt.clone()
    }

    fn transition(&self, update: impl FnOnce(&mut McpRuntimeState)) {
        let mut state = self.state.write().unwrap();
        if state.terminal {
            return;
        }
        debug_assert_eq!(state.receipt.server_identity, self.server.name);
        debug_assert_eq!(state.receipt.endpoint, endpoint_binding(&self.server));
        update(&mut state);
        state.receipt.catalog_generation = state.catalog.generation;
        state.receipt.advance();
    }

    fn starting(&self) {
        self.transition(|state| {
            state.receipt.lifecycle = McpLifecycleState::Starting;
            state.receipt.last_health = McpHealthResult::Unknown;
            state.receipt.failure = None;
        });
    }

    fn ready(
        &self,
        client: Arc<McpClient>,
        oauth: Option<Arc<OAuthSession>>,
        capabilities: McpServerCapabilities,
        advertised: Vec<ToolDef>,
        has_notifications: bool,
        warn: &dyn Fn(&str),
    ) {
        let catalog = Arc::new(build_catalog(&self.server, 0, advertised, warn));
        self.transition(|state| {
            state.client = Some(client);
            state.oauth = oauth;
            state.dynamic_refresh = has_notifications;
            state.catalog = catalog;
            state.receipt.auth = if state.oauth.is_some() {
                McpAuthAvailability::OAuthAvailable
            } else if state.receipt.auth == McpAuthAvailability::StaticCredentialConfigured {
                McpAuthAvailability::StaticCredentialAvailable
            } else {
                state.receipt.auth
            };
            state.capabilities = capabilities.clone();
            state.receipt.capabilities = McpCapabilitySummary::from(&capabilities);
            state.receipt.lifecycle = McpLifecycleState::Ready;
            state.receipt.refresh = if has_notifications {
                McpRefreshState::Idle
            } else {
                McpRefreshState::StartupFixed
            };
            state.receipt.last_health = McpHealthResult::Healthy;
            state.receipt.failure = None;
        });
    }

    fn startup_failed(&self, auth: Option<McpAuthAvailability>, failure: McpFailureKind) {
        self.transition(|state| {
            if let Some(auth) = auth {
                state.receipt.auth = auth;
            }
            state.receipt.lifecycle = McpLifecycleState::Failed;
            state.receipt.refresh = McpRefreshState::Unavailable;
            state.receipt.last_health = McpHealthResult::Failed;
            state.receipt.failure = Some(failure);
        });
    }

    fn public_name(&self) -> String {
        self.server
            .name
            .chars()
            .filter(|character| {
                !character.is_control()
                    && !matches!(
                        *character,
                        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{206F}'
                    )
            })
            .take(128)
            .collect()
    }

    fn unavailable_reason_from(&self, receipt: &McpReadinessReceipt) -> String {
        let failure = receipt.failure.unwrap_or(McpFailureKind::Request);
        format!(
            "MCP server \"{}\" is {:?}: {}. Its configured tools are unavailable, not missing.",
            self.public_name(),
            receipt.lifecycle,
            failure.reason()
        )
    }

    fn unavailable_reason(&self) -> String {
        self.unavailable_reason_from(&self.state.read().unwrap().receipt)
    }

    fn abort_client(&self) {
        if let Some(client) = self.state.read().unwrap().client.clone() {
            client.abort();
        }
    }

    async fn shutdown_client(&self) {
        let client = { self.state.read().unwrap().client.clone() };
        if let Some(client) = client {
            client.shutdown().await;
        }
    }

    fn close(&self, failure: McpFailureKind) {
        self.transition(|state| {
            state.terminal = true;
            state.receipt.lifecycle = McpLifecycleState::Closed;
            state.receipt.last_health = McpHealthResult::Closed;
            state.receipt.failure = Some(failure);
        });
    }

    async fn transport_health(&self, health: McpTransportState) {
        match health {
            McpTransportState::Healthy => {
                let recover = {
                    let state = self.state.read().unwrap();
                    matches!(
                        state.receipt.failure,
                        Some(
                            McpFailureKind::TransportClosed
                                | McpFailureKind::TransportRead
                                | McpFailureKind::TransportWrite
                                | McpFailureKind::RequestTimeout
                                | McpFailureKind::Request
                        )
                    ) && state.client.is_some()
                };
                if recover {
                    let _lifecycle = self.refresh_gate.write().await;
                    self.transition(|state| {
                        state.receipt.lifecycle = McpLifecycleState::Ready;
                        state.receipt.refresh = if state.dynamic_refresh {
                            McpRefreshState::Idle
                        } else {
                            McpRefreshState::StartupFixed
                        };
                        state.receipt.last_health = McpHealthResult::Healthy;
                        state.receipt.failure = None;
                    });
                }
            }
            McpTransportState::Degraded(failure) => {
                let _lifecycle = self.refresh_gate.write().await;
                self.transition(|state| {
                    state.receipt.lifecycle = McpLifecycleState::Degraded;
                    state.receipt.last_health = McpHealthResult::Degraded;
                    state.receipt.failure = Some(failure_kind(failure));
                });
            }
            McpTransportState::Closed(failure) => {
                let _lifecycle = self.refresh_gate.write().await;
                self.transition(|state| {
                    state.terminal = true;
                    state.receipt.lifecycle = McpLifecycleState::Closed;
                    state.receipt.last_health = McpHealthResult::Closed;
                    state.receipt.failure = Some(failure_kind(failure));
                });
            }
        }
    }

    async fn authentication_refreshed(&self) {
        let _lifecycle = self.refresh_gate.write().await;
        if self.state.read().unwrap().receipt.failure == Some(McpFailureKind::Authentication) {
            return;
        }
        self.transition(|state| {
            state.receipt.auth = McpAuthAvailability::OAuthAvailable;
            if state.receipt.lifecycle == McpLifecycleState::Ready {
                state.receipt.last_health = McpHealthResult::Healthy;
            }
        });
    }

    async fn notification_closed(&self) {
        let _lifecycle = self.refresh_gate.write().await;
        self.transition(|state| {
            state.terminal = true;
            state.receipt.lifecycle = McpLifecycleState::Closed;
            state.receipt.last_health = McpHealthResult::Closed;
            state.receipt.failure = Some(McpFailureKind::NotificationClosed);
        });
    }

    fn mark_authentication_failed(state: &mut McpRuntimeState) {
        state.receipt.auth = match state.receipt.auth {
            McpAuthAvailability::StaticCredentialConfigured
            | McpAuthAvailability::StaticCredentialAvailable
            | McpAuthAvailability::StaticCredentialUnavailable => {
                McpAuthAvailability::StaticCredentialUnavailable
            }
            McpAuthAvailability::Unauthenticated | McpAuthAvailability::LoginRequired => {
                McpAuthAvailability::LoginRequired
            }
            McpAuthAvailability::OAuthAvailable | McpAuthAvailability::ReauthenticationRequired => {
                McpAuthAvailability::ReauthenticationRequired
            }
            McpAuthAvailability::NotApplicable => McpAuthAvailability::NotApplicable,
        };
        state.receipt.lifecycle = McpLifecycleState::Failed;
        state.receipt.last_health = McpHealthResult::Failed;
        state.receipt.failure = Some(McpFailureKind::Authentication);
    }

    fn classify_request_error(&self, error: anyhow::Error) -> anyhow::Error {
        if error.downcast_ref::<McpRpcError>().is_some()
            || error.downcast_ref::<McpToolCallError>().is_some()
        {
            return error;
        }
        if let Some(session) = error.downcast_ref::<McpSessionReinitialized>() {
            let capabilities = session.capabilities.clone();
            self.transition(|state| {
                state.capabilities = capabilities.clone();
                state.receipt.capabilities = McpCapabilitySummary::from(&capabilities);
                state.receipt.lifecycle = McpLifecycleState::Stale;
                state.receipt.refresh = McpRefreshState::Refreshing;
                state.receipt.last_health = McpHealthResult::Stale;
                state.receipt.failure = Some(McpFailureKind::RefreshInProgress);
            });
            return anyhow::anyhow!(session.to_string());
        }
        let state = self.state.read().unwrap();
        let auth_failed = error.downcast_ref::<McpAuthenticationError>().is_some()
            || state
                .oauth
                .as_ref()
                .is_some_and(|oauth| oauth.needs_relogin());
        drop(state);
        self.transition(|state| {
            if auth_failed {
                Self::mark_authentication_failed(state);
            } else {
                state.receipt.lifecycle = McpLifecycleState::Degraded;
                state.receipt.last_health = McpHealthResult::Degraded;
                state.receipt.failure = Some(McpFailureKind::Request);
            }
        });
        anyhow::anyhow!(self.unavailable_reason())
    }

    fn classify_request<T>(&self, result: Result<T>) -> Result<T> {
        result.map_err(|error| self.classify_request_error(error))
    }

    fn available_capabilities(&self) -> Result<McpServerCapabilities> {
        let state = self.state.read().unwrap();
        if !state.receipt.is_ready() {
            bail!(self.unavailable_reason_from(&state.receipt));
        }
        Ok(state.capabilities.clone())
    }

    fn resource_preflight(&self, directory: bool) -> Result<()> {
        let capabilities = self.available_capabilities()?;
        if !capabilities.resources {
            bail!(
                "Server \"{}\" does not advertise MCP resource support.",
                self.public_name()
            );
        }
        if directory && !capabilities.directory_read {
            bail!(
                "Server \"{}\" does not support directory listing.",
                self.public_name()
            );
        }
        Ok(())
    }

    fn ready_client(&self) -> Result<(Arc<McpClient>, McpServerCapabilities)> {
        let state = self.state.read().unwrap();
        if !state.receipt.is_ready() {
            bail!(self.unavailable_reason_from(&state.receipt));
        }
        let client = state
            .client
            .clone()
            .context("ready MCP source has no client")?;
        Ok((client, state.capabilities.clone()))
    }

    async fn refresh_with_delays(&self, delays: &[Duration]) {
        let _refresh = self.refresh_gate.write().await;
        let client = {
            let state = self.state.read().unwrap();
            if state.client.is_none() || state.receipt.lifecycle == McpLifecycleState::Closed {
                return;
            }
            state.client.clone()
        };
        let Some(client) = client else {
            return;
        };
        self.transition(|state| {
            state.receipt.lifecycle = McpLifecycleState::Stale;
            state.receipt.refresh = McpRefreshState::Refreshing;
            state.receipt.last_health = McpHealthResult::Stale;
            state.receipt.failure = Some(McpFailureKind::RefreshInProgress);
        });

        let mut last_error = None;
        let mut advertised = None;
        for attempt in 0..=delays.len() {
            match client.list_tools().await {
                Ok(tools) => {
                    advertised = Some(tools);
                    break;
                }
                Err(error) => {
                    if let Some(session) = error.downcast_ref::<McpSessionReinitialized>() {
                        let capabilities = session.capabilities.clone();
                        self.transition(|state| {
                            state.capabilities = capabilities.clone();
                            state.receipt.capabilities = McpCapabilitySummary::from(&capabilities);
                        });
                    }
                    last_error = Some(error);
                }
            }
            if let Some(delay) = delays.get(attempt) {
                self.transition(|state| {
                    state.receipt.refresh = McpRefreshState::Retrying {
                        attempt: u8::try_from(attempt + 1).unwrap_or(u8::MAX),
                    };
                });
                tokio::time::sleep(*delay).await;
            }
        }

        if let Some(advertised) = advertised {
            let mut state = self.state.write().unwrap();
            if state.terminal {
                return;
            }
            let generation = state
                .catalog
                .generation
                .checked_add(1)
                .expect("MCP catalog generation exhausted");
            state.catalog = Arc::new(build_catalog(&self.server, generation, advertised, &|_| {}));
            state.receipt.catalog_generation = generation;
            state.receipt.lifecycle = McpLifecycleState::Ready;
            state.receipt.refresh = if state.dynamic_refresh {
                McpRefreshState::Idle
            } else {
                McpRefreshState::StartupFixed
            };
            state.receipt.last_health = McpHealthResult::Healthy;
            state.receipt.failure = None;
            state.receipt.advance();
        } else {
            let explicit_auth_failure = last_error
                .as_ref()
                .is_some_and(|error| error.downcast_ref::<McpAuthenticationError>().is_some());
            let auth_failed = explicit_auth_failure
                || self
                    .state
                    .read()
                    .unwrap()
                    .oauth
                    .as_ref()
                    .is_some_and(|oauth| oauth.needs_relogin());
            self.transition(|state| {
                if auth_failed {
                    Self::mark_authentication_failed(state);
                } else {
                    state.receipt.lifecycle = McpLifecycleState::Degraded;
                    state.receipt.last_health = McpHealthResult::Degraded;
                    state.receipt.failure = Some(McpFailureKind::Refresh);
                }
                state.receipt.refresh = McpRefreshState::RetryExhausted;
            });
            drop(last_error);
        }
    }

    async fn refresh_with_retry(&self) {
        self.refresh_with_delays(&TOOL_REFRESH_RETRY_DELAYS).await;
    }
}

fn failure_kind(failure: McpTransportFailure) -> McpFailureKind {
    match failure {
        McpTransportFailure::RequestTimeout => McpFailureKind::RequestTimeout,
        McpTransportFailure::WriteFailed => McpFailureKind::TransportWrite,
        McpTransportFailure::ConnectionEof => McpFailureKind::TransportClosed,
        McpTransportFailure::ReadFailed | McpTransportFailure::MessageTooLarge => {
            McpFailureKind::TransportRead
        }
        McpTransportFailure::ExplicitShutdown => McpFailureKind::Shutdown,
    }
}

impl ToolSource for McpToolSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        let state = self.state.read().unwrap();
        if state.receipt.is_ready() {
            state.catalog.defs.clone()
        } else {
            Arc::from(Vec::<ToolDef>::new())
        }
    }

    fn definition_generation(&self, _tool: &str) -> u64 {
        self.state.read().unwrap().catalog.generation
    }

    fn readiness_revision(&self, _tool: &str) -> u64 {
        self.state.read().unwrap().receipt.readiness_revision
    }

    fn catalog_version(&self) -> SourceVersion {
        let state = self.state.read().unwrap();
        SourceVersion {
            definition_generation: state.catalog.generation,
            readiness_revision: state.receipt.readiness_revision,
        }
    }

    fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
        let state = self.state.read().unwrap();
        state.receipt.is_ready().then_some(())?;
        state
            .catalog
            .defs
            .iter()
            .find(|def| def.name == tool)
            .cloned()
            .map(|def| (def, state.catalog.generation))
    }

    fn definition_state(&self, tool: &str) -> SourceDefinitionState {
        let state = self.state.read().unwrap();
        if state.receipt.is_ready() {
            return match state.catalog.defs.iter().find(|def| def.name == tool) {
                Some(definition) => SourceDefinitionState::Available {
                    definition: definition.clone(),
                    version: SourceVersion {
                        definition_generation: state.catalog.generation,
                        readiness_revision: state.receipt.readiness_revision,
                    },
                },
                None => SourceDefinitionState::Missing,
            };
        }
        let prefix_matches = tool
            .get(..self.qualified_prefix.len())
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case(&self.qualified_prefix));
        if state.catalog.raw_names.contains_key(tool) || prefix_matches {
            SourceDefinitionState::Unavailable {
                reason: self.unavailable_reason_from(&state.receipt),
            }
        } else {
            SourceDefinitionState::Missing
        }
    }

    fn is_readonly(&self, tool: &str) -> bool {
        let state = self.state.read().unwrap();
        state.receipt.is_ready() && state.catalog.readonly.contains(tool)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        self.call_at_version(tool, input, None)
    }

    fn call_at_generation<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        generation: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        let version = generation.map(|definition_generation| SourceVersion {
            definition_generation,
            readiness_revision: self.readiness_revision(tool),
        });
        self.call_at_version(tool, input, version)
    }

    fn call_at_version<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        version: Option<SourceVersion>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            let _call = self.refresh_gate.read().await;
            let (client, catalog, receipt) = {
                let state = self.state.read().unwrap();
                (
                    state.client.clone(),
                    state.catalog.clone(),
                    state.receipt.clone(),
                )
            };
            if !receipt.is_ready() {
                bail!(self.unavailable_reason_from(&receipt));
            }
            let current = SourceVersion {
                definition_generation: catalog.generation,
                readiness_revision: receipt.readiness_revision,
            };
            if version.is_some_and(|expected| expected != current) {
                bail!(
                    "MCP tool definition or server readiness changed after discovery; run tool_search for {tool} again before retrying"
                );
            }
            let raw = catalog
                .raw_names
                .get(tool)
                .cloned()
                .with_context(|| format!("unknown mcp tool: {tool}"))?;
            let client = client.context("ready MCP source has no client")?;
            let structured =
                self.classify_request(client.call_tool_structured(&raw, input).await)?;
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
    let readonly_raw: HashSet<&str> = server.readonly_tools.iter().map(String::as_str).collect();
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

#[cfg(test)]
fn build_source(
    server: &McpServerConfig,
    client: Arc<McpClient>,
    advertised: Vec<ToolDef>,
    warn: &dyn Fn(&str),
) -> Arc<McpToolSource> {
    let source = McpToolSource::planned(server);
    source.starting();
    source.ready(
        client.clone(),
        None,
        client.capabilities(),
        advertised,
        /*has_notifications=*/ false,
        warn,
    );
    source
}

#[cfg(test)]
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
    lifecycle: &McpLifecycleOwner,
    source: Arc<McpToolSource>,
    mut notifications: tokio::sync::broadcast::Receiver<McpNotification>,
    source_closed: CancellationToken,
) {
    let cancel = lifecycle.cancel_token();
    lifecycle.spawn(async move {
        loop {
            let notification = tokio::select! {
                _ = cancel.cancelled() => break,
                _ = source_closed.cancelled() => break,
                notification = notifications.recv() => notification,
            };
            let refresh_needed = match notification {
                Ok(McpNotification::ToolsListChanged)
                | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => true,
                Ok(McpNotification::ResourcesListChanged)
                | Ok(McpNotification::ResourceUpdated { .. }) => false,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    source.notification_closed().await;
                    source_closed.cancel();
                    source.shutdown_client().await;
                    break;
                }
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
            if closed {
                source.notification_closed().await;
                source_closed.cancel();
                source.shutdown_client().await;
                break;
            }
            source.refresh_with_retry().await;
        }
    });
}

fn spawn_health_monitor(
    lifecycle: &McpLifecycleOwner,
    source: Arc<McpToolSource>,
    mut health: tokio::sync::watch::Receiver<McpTransportHealth>,
    source_closed: CancellationToken,
) {
    let cancel = lifecycle.cancel_token();
    lifecycle.spawn(async move {
        let mut seen = McpTransportHealth {
            state: McpTransportState::Healthy,
            session_revision: 0,
            authentication_revision: 0,
        };
        loop {
            let current = *health.borrow_and_update();
            if current.session_revision > seen.session_revision {
                source.refresh_with_retry().await;
            }
            if current.authentication_revision > seen.authentication_revision {
                source.authentication_refreshed().await;
            }
            source.transport_health(current.state).await;
            seen = current;
            if matches!(current.state, McpTransportState::Closed(_)) {
                source_closed.cancel();
                source.shutdown_client().await;
                break;
            }

            let changed = tokio::select! {
                _ = cancel.cancelled() => break,
                changed = health.changed() => changed,
            };
            if changed.is_err() {
                source.notification_closed().await;
                source_closed.cancel();
                source.shutdown_client().await;
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
    source: Arc<McpToolSource>,
}

impl McpResourceServer {
    fn availability(&self) -> Result<McpServerCapabilities> {
        self.source.available_capabilities()
    }

    fn preflight(&self, directory: bool) -> Result<()> {
        self.source.resource_preflight(directory)
    }

    async fn begin_request(
        &self,
    ) -> Result<(
        tokio::sync::RwLockReadGuard<'_, ()>,
        Arc<McpClient>,
        McpServerCapabilities,
    )> {
        let guard = self.source.refresh_gate.read().await;
        let (client, capabilities) = self.source.ready_client()?;
        Ok((guard, client, capabilities))
    }

    fn classify<T>(&self, result: Result<T>) -> Result<T> {
        self.source.classify_request(result)
    }
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
        let explicit_server = requested.is_some();
        let mut tasks = tokio::task::JoinSet::new();
        for (index, server) in selected.into_iter().enumerate() {
            tasks.spawn(async move {
                let name = server.name.clone();
                let result = async {
                    let (_guard, client, capabilities) = server.begin_request().await?;
                    if !capabilities.resources {
                        if explicit_server {
                            bail!("Server \"{name}\" does not advertise MCP resource support.");
                        }
                        return Ok(None);
                    }
                    server.classify(client.list_resources().await).map(Some)
                }
                .await;
                (index, name, result)
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
                Ok(Some(resources)) => groups.push((index, server, resources)),
                Ok(None) => {}
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

    async fn require_advertised_uri(
        &self,
        server: &McpResourceServer,
        client: &McpClient,
        uri: &str,
    ) -> Result<()> {
        let resources = server
            .classify(client.list_resources().await)
            .with_context(|| format!("cannot refresh resources from server \"{}\"", server.name))?;
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
        let (_guard, client, capabilities) = server.begin_request().await?;
        if !capabilities.resources {
            bail!(
                "Server \"{}\" does not advertise MCP resource support.",
                server.name
            );
        }
        self.require_advertised_uri(&server, &client, uri).await?;
        let read = match server.classify(client.read_resource(uri).await) {
            Ok(read) => read,
            Err(error) => match rpc_code(&error) {
                Some(-32601) => bail!(
                    "Server \"{}\" advertises resource support but does not implement resource reads.",
                    server.name
                ),
                Some(-32002 | -32602) => {
                    let directory_hint = capabilities.directory_read.then_some(
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
        let (_guard, client, capabilities) = server.begin_request().await?;
        if !capabilities.resources {
            bail!(
                "Server \"{}\" does not advertise MCP resource support.",
                server.name
            );
        }
        if !capabilities.directory_read {
            bail!(
                "Server \"{}\" does not support directory listing.",
                server.name
            );
        }
        self.require_advertised_uri(&server, &client, uri).await?;
        let resources = match server.classify(client.read_resource_directory(uri).await) {
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

    fn preflight(&self, tool: &str, input: &Value) -> Result<()> {
        match tool {
            LIST_MCP_RESOURCES => {
                if let Some(server) = input.get("server").and_then(Value::as_str) {
                    return self.server(server)?.preflight(/*directory=*/ false);
                }
                let mut first_unavailable = None;
                for server in &self.servers {
                    match server.availability() {
                        Ok(capabilities) if capabilities.resources => return Ok(()),
                        Ok(_) => {}
                        Err(error) => {
                            first_unavailable.get_or_insert(error);
                        }
                    }
                }
                match first_unavailable {
                    Some(error) => Err(error),
                    None => Ok(()),
                }
            }
            READ_MCP_RESOURCE => {
                let server = required_resource_arg(input, "server", tool)?;
                required_resource_arg(input, "uri", tool)?;
                self.server(server)?.preflight(/*directory=*/ false)
            }
            READ_MCP_RESOURCE_DIR => {
                let server = required_resource_arg(input, "server", tool)?;
                required_resource_arg(input, "uri", tool)?;
                self.server(server)?.preflight(/*directory=*/ true)
            }
            _ => bail!("unknown MCP resource tool: {tool}"),
        }
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

async fn initial_tool_catalog(
    client: &McpClient,
    capabilities: &mut McpServerCapabilities,
) -> Result<Vec<ToolDef>> {
    match client.list_tools().await {
        Ok(advertised) => Ok(advertised),
        Err(error) => match error.downcast_ref::<McpSessionReinitialized>() {
            Some(session) => {
                *capabilities = session.capabilities.clone();
                client.list_tools().await
            }
            None => Err(error),
        },
    }
}

fn classify_startup_failure(
    server: &McpServerConfig,
    error: &anyhow::Error,
    oauth_status_session: Option<&Arc<OAuthSession>>,
    startup_auth: Option<McpAuthAvailability>,
) -> (Option<McpAuthAvailability>, McpFailureKind) {
    let typed_auth = error.downcast_ref::<McpAuthenticationError>().is_some();
    let oauth_reauthentication = oauth_status_session.is_some_and(|oauth| oauth.needs_relogin());
    let failed_auth = if typed_auth {
        // `StaticCredentialUnavailable` used to mean "the env var was not
        // set", which could be known before connecting. A token written in
        // the config is always present, so the state now means the only thing
        // left it can mean: the server rejected the one we have.
        Some(match &server.transport {
            McpTransport::Http {
                bearer_token: Some(_),
                ..
            } => McpAuthAvailability::StaticCredentialUnavailable,
            McpTransport::Http {
                bearer_token: None, ..
            } if oauth_status_session.is_some() => McpAuthAvailability::ReauthenticationRequired,
            McpTransport::Http {
                bearer_token: None, ..
            } => McpAuthAvailability::LoginRequired,
            McpTransport::Stdio { .. } => McpAuthAvailability::NotApplicable,
        })
    } else if oauth_reauthentication {
        Some(McpAuthAvailability::ReauthenticationRequired)
    } else {
        startup_auth
    };
    let failure = if typed_auth || oauth_reauthentication {
        McpFailureKind::Authentication
    } else {
        McpFailureKind::Startup
    };
    (failed_auth, failure)
}

/// Spawn + handshake + tool discovery for every configured server. A failing
/// server degrades to a warning but retains a source-owned unavailable route.
pub async fn connect_servers(
    servers: Vec<McpServerConfig>,
    warn: &dyn Fn(&str),
) -> Result<McpConnections> {
    let store = Arc::new(CredentialStore::default_path()?);
    let lifecycle = McpLifecycleOwner::default();
    let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
    let mut resource_servers = Vec::new();
    let mut statuses = Vec::new();
    for server in servers {
        let transport = server.transport.kind();
        let source = McpToolSource::planned(&server);
        lifecycle.register_source(source.clone());
        source.starting();
        resource_servers.push(McpResourceServer {
            name: server.name.clone(),
            source: source.clone(),
        });
        // A remote server with no static bearer takes the OAuth path: use a
        // stored token if the user has logged in; else connect unauthenticated
        // and, if that's rejected, point them at the login command.
        let oauth_capable = matches!(
            &server.transport,
            McpTransport::Http {
                bearer_token: None,
                ..
            }
        );
        let oauth_session = match &server.transport {
            McpTransport::Http {
                url,
                bearer_token: None,
                ..
            } => store.session_for(&server.name, url),
            _ => Ok(None),
        };
        let startup_auth = if oauth_capable {
            Some(match &oauth_session {
                Ok(Some(_)) => McpAuthAvailability::OAuthAvailable,
                Ok(None) | Err(_) => McpAuthAvailability::LoginRequired,
            })
        } else {
            None
        };
        let oauth_status_session = oauth_session
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .cloned();
        let connect = async {
            let (client, oauth) = match &server.transport {
                McpTransport::Stdio { command, env } => (McpClient::spawn(command, env)?, None),
                McpTransport::Http {
                    url,
                    bearer_token,
                    http_headers,
                    ..
                } => {
                    let headers = http_headers_for(bearer_token, http_headers);
                    let oauth = oauth_session?;
                    (McpClient::http(url.clone(), headers, oauth.clone())?, oauth)
                }
            };
            let client = Arc::new(client);
            let health = client.subscribe_health();
            let mut capabilities = client.initialize().await?;
            let advertised = initial_tool_catalog(&client, &mut capabilities).await?;
            let notifications = capabilities
                .tools_list_changed
                .then(|| client.subscribe_notifications())
                .flatten();
            anyhow::Ok((
                client,
                oauth,
                capabilities,
                notifications,
                health,
                advertised,
            ))
        };
        match connect.await {
            Ok((client, oauth, capabilities, notifications, health, advertised)) => {
                let count = advertised.len();
                let has_notifications = notifications.is_some();
                source.ready(
                    client,
                    oauth,
                    capabilities.clone(),
                    advertised,
                    has_notifications,
                    warn,
                );
                let tools = source
                    .state
                    .read()
                    .unwrap()
                    .catalog
                    .defs
                    .iter()
                    .map(|def| McpToolInfo {
                        name: def.name.clone(),
                        description: def.description.clone(),
                    })
                    .collect();
                let dynamic_message =
                    dynamic_catalog_message(&capabilities, has_notifications).map(str::to_string);
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
                let source_closed = CancellationToken::new();
                if let Some(health) = health {
                    spawn_health_monitor(&lifecycle, source.clone(), health, source_closed.clone());
                }
                if let Some(notifications) = notifications {
                    spawn_tool_refresh(&lifecycle, source.clone(), notifications, source_closed);
                }
            }
            Err(error) => {
                let (failed_auth, failure) = classify_startup_failure(
                    &server,
                    &error,
                    oauth_status_session.as_ref(),
                    startup_auth,
                );
                source.startup_failed(failed_auth, failure);
                let hint = if oauth_capable {
                    format!("; if it needs OAuth, run: kloop mcp login {}", server.name)
                } else {
                    String::new()
                };
                warn(&format!("mcp server '{}' unavailable{hint}", server.name));
                statuses.push(McpServerStatus {
                    name: server.name.clone(),
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
        sources.push(source);
    }
    if !resource_servers.is_empty() {
        sources.push(Arc::new(McpResourceSource::new(resource_servers)));
    }
    Ok(McpConnections {
        sources,
        statuses,
        lifecycle,
    })
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
        let client = Arc::new(McpClient::over(reader, writer, None));
        let server = McpServerConfig {
            name: name.into(),
            transport: McpTransport::Stdio {
                command: vec!["fixture".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        source.starting();
        source.ready(
            client,
            None,
            McpServerCapabilities {
                resources: true,
                ..Default::default()
            },
            Vec::new(),
            /*has_notifications=*/ false,
            &|_| {},
        );
        (
            McpResourceServer {
                name: name.into(),
                source,
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
readonly_tools = ["read_graph", "search_nodes"]

[mcp.servers.fs]
command = ["mcp-fs"]

[mcp.servers.remote]
url = "https://mcp.example.com/mcp"
bearer_token = "sk-example-token"
http_headers = { X-Tenant = "acme" }
readonly_tools = ["search"]

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
                    readonly_tools: vec![],
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
                    readonly_tools: vec!["read_graph".into(), "search_nodes".into()],
                },
                McpServerConfig {
                    name: "oauthy".into(),
                    transport: McpTransport::Http {
                        url: "https://oauth.example.com/mcp".into(),
                        bearer_token: None,
                        http_headers: BTreeMap::new(),
                        oauth_client_id: Some("preconfigured-123".into()),
                        oauth_scopes: vec!["mcp.read".into(), "mcp.write".into()],
                    },
                    readonly_tools: vec![],
                },
                McpServerConfig {
                    name: "remote".into(),
                    transport: McpTransport::Http {
                        url: "https://mcp.example.com/mcp".into(),
                        bearer_token: Some("sk-example-token".into()),
                        http_headers: BTreeMap::from([("X-Tenant".into(), "acme".into())]),
                        oauth_client_id: None,
                        oauth_scopes: vec![],
                    },
                    readonly_tools: vec!["search".into()],
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
            (
                "normalized-name-collision",
                "[mcp.servers.foo-bar]\ncommand = [\"a\"]\n[mcp.servers.foo_bar]\ncommand = [\"b\"]\n",
            ),
            // A token written but left empty is a typo, not "no auth".
            (
                "emptytoken",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = \"\"\n",
            ),
            (
                "tokentype",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = 3\n",
            ),
            // The retired env-var spelling is simply unknown now.
            (
                "tokenenvvar",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token_env_var = \"T\"\n",
            ),
            // Static bearer + OAuth keys is a contradiction.
            (
                "bearerplusoauth",
                "[mcp.servers.x]\nurl = \"https://h/mcp\"\nbearer_token = \"T\"\noauth_client_id = \"c\"\n",
            ),
        ] {
            let root = config(bad);
            assert!(load_mcp_servers(&root).is_err(), "{tag} should fail");
        }
    }

    #[test]
    fn defer_threshold_defaults_and_parses_from_the_mcp_section() {
        assert_eq!(
            load_defer_threshold(&toml::Table::new()).unwrap(),
            kloop_core::tools::TOOL_DEFER_THRESHOLD
        );
        assert_eq!(
            load_defer_threshold(&config("[mcp]\ndefer_threshold = 3\n")).unwrap(),
            3
        );
        // Zero is meaningful: defer everything.
        assert_eq!(
            load_defer_threshold(&config("[mcp]\ndefer_threshold = 0\n")).unwrap(),
            0
        );
        for bad in [
            "[mcp]\ndefer_threshold = -1\n",
            "[mcp]\ndefer_threshold = \"many\"\n",
        ] {
            assert!(load_defer_threshold(&config(bad)).is_err(), "{bad}");
        }
    }

    /// The transport holds the token in plaintext now, so its Debug is what
    /// stands between a failed assertion and a leaked credential.
    #[test]
    fn transport_debug_prints_names_but_never_values() {
        let http = McpTransport::Http {
            url: "https://example.com/mcp".into(),
            bearer_token: Some("sk-SECRET".into()),
            http_headers: BTreeMap::from([("X-Tenant".into(), "TENANT_SECRET".into())]),
            oauth_client_id: Some("client-123".into()),
            oauth_scopes: vec!["mcp.read".into()],
        };
        assert_eq!(
            format!("{http:?}"),
            "Http { url: \"https://example.com/mcp\", bearer_token: Some(\"<redacted>\"), \
             http_headers: {\"X-Tenant\": \"<redacted>\"}, oauth_client_id: Some(\"client-123\"), \
             oauth_scopes: [\"mcp.read\"] }"
        );

        let stdio = McpTransport::Stdio {
            command: vec!["mcp-fs".into()],
            env: BTreeMap::from([("API_KEY".into(), "ENV_SECRET".into())]),
        };
        assert_eq!(
            format!("{stdio:?}"),
            "Stdio { command: [\"mcp-fs\"], env: {\"API_KEY\": \"<redacted>\"} }"
        );
    }

    /// The token is spelled into a header here and nowhere else, so this is
    /// the whole of what the config's `bearer_token` does.
    #[test]
    fn a_configured_bearer_token_becomes_the_authorization_header() {
        assert_eq!(
            http_headers_for(
                &Some("sk-abc".into()),
                &BTreeMap::from([("X-Tenant".into(), "acme".into())]),
            ),
            BTreeMap::from([
                ("Authorization".into(), "Bearer sk-abc".into()),
                ("X-Tenant".into(), "acme".into()),
            ])
        );
        // No token ⇒ just the static headers.
        assert_eq!(
            http_headers_for(&None, &BTreeMap::from([("A".into(), "b".into())])),
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
                readonly_tools: Vec::new(),
            }],
            &warn,
        )
        .await
        .unwrap();

        assert_eq!(connections.sources.len(), 2);
        match connections.sources[0].definition_state("broken__echo") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("unavailable, not missing"), "{reason}");
            }
            state => panic!("failed source was not retained: {state:?}"),
        }
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
            vec!["mcp server 'broken' unavailable".to_string()]
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
    fn readiness_receipt_is_typed_monotonic_and_secret_free() {
        let server = McpServerConfig {
            name: "secure".into(),
            transport: McpTransport::Http {
                url: "https://user:URL_SECRET@example.com/mcp?token=QUERY_SECRET".into(),
                bearer_token: None,
                http_headers: BTreeMap::from([(
                    "Authorization".into(),
                    "Bearer HEADER_SECRET".into(),
                )]),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        let planned = source.receipt();
        assert_eq!(planned.lifecycle, McpLifecycleState::Planned);
        assert_eq!(planned.readiness_revision, 0);
        assert_eq!(planned.endpoint.transport, McpTransportKind::Http);
        let rendered = format!("{planned:?}");
        for secret in ["URL_SECRET", "QUERY_SECRET", "HEADER_SECRET"] {
            assert!(
                !rendered.contains(secret),
                "receipt leaked {secret}: {rendered}"
            );
        }
        let mut other = server.clone();
        if let McpTransport::Http {
            url, http_headers, ..
        } = &mut other.transport
        {
            *url = "https://other:DIFFERENT_SECRET@example.com/mcp/OTHER_PATH_SECRET?token=OTHER"
                .into();
            *http_headers =
                BTreeMap::from([("Authorization".into(), "Bearer OTHER_HEADER_SECRET".into())]);
        }
        assert_eq!(
            endpoint_binding(&server),
            endpoint_binding(&other),
            "URL credentials/query/path and header values must not affect the binding digest"
        );
        let stdio_a = McpServerConfig {
            name: "stdio".into(),
            transport: McpTransport::Stdio {
                command: vec!["runner".into(), "--token=SECRET_A".into()],
                env: BTreeMap::from([("TOKEN".into(), "SECRET_A".into())]),
            },
            readonly_tools: Vec::new(),
        };
        let mut stdio_b = stdio_a.clone();
        if let McpTransport::Stdio { command, env } = &mut stdio_b.transport {
            command[1] = "--token=SECRET_B".into();
            env.insert("TOKEN".into(), "SECRET_B".into());
        }
        assert_eq!(endpoint_binding(&stdio_a), endpoint_binding(&stdio_b));

        source.starting();
        let starting = source.receipt();
        assert_eq!(starting.lifecycle, McpLifecycleState::Starting);
        assert_eq!(starting.readiness_revision, 1);
        source.startup_failed(
            Some(McpAuthAvailability::LoginRequired),
            McpFailureKind::Startup,
        );
        let failed = source.receipt();
        assert_eq!(failed.lifecycle, McpLifecycleState::Failed);
        assert_eq!(failed.auth, McpAuthAvailability::LoginRequired);
        assert_eq!(failed.failure, Some(McpFailureKind::Startup));
        assert_eq!(failed.readiness_revision, 2);
        match source.definition_state("secure__unknown") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("unavailable, not missing"), "{reason}");
                assert!(!reason.contains("URL_SECRET"), "{reason}");
            }
            state => panic!("unexpected route state: {state:?}"),
        }
        match source.definition_state("SECURE__UNKNOWN") {
            SourceDefinitionState::Unavailable { .. } => {}
            state => panic!("case-insensitive unavailable route was {state:?}"),
        }
    }

    #[tokio::test]
    async fn static_bearer_availability_tracks_rejected_credentials() {
        let server = McpServerConfig {
            name: "static".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                bearer_token: Some("sk-static".into()),
                http_headers: BTreeMap::new(),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        };
        // A configured token is always present, so `Unavailable` now has one
        // cause left: the server turned it down.
        let (startup_auth, startup_failure) = classify_startup_failure(
            &server,
            &anyhow::Error::new(McpAuthenticationError),
            None,
            None,
        );
        assert_eq!(
            startup_auth,
            Some(McpAuthAvailability::StaticCredentialUnavailable)
        );
        assert_eq!(startup_failure, McpFailureKind::Authentication);

        let missing = McpToolSource::planned(&server);
        missing.starting();
        missing.startup_failed(
            Some(McpAuthAvailability::StaticCredentialUnavailable),
            McpFailureKind::Authentication,
        );
        let missing_receipt = missing.receipt();
        assert_eq!(
            missing_receipt.auth,
            McpAuthAvailability::StaticCredentialUnavailable
        );
        assert_eq!(
            missing_receipt.failure,
            Some(McpFailureKind::Authentication)
        );

        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let ready = McpToolSource::planned(&server);
        ready.starting();
        ready.ready(
            Arc::new(McpClient::over(reader, writer, None)),
            None,
            McpServerCapabilities::default(),
            Vec::new(),
            /*has_notifications=*/ false,
            &|_| {},
        );
        assert_eq!(
            ready.receipt().auth,
            McpAuthAvailability::StaticCredentialAvailable
        );
        let _ = ready.classify_request_error(anyhow::Error::new(McpAuthenticationError));
        let failed = ready.receipt();
        assert_eq!(failed.lifecycle, McpLifecycleState::Failed);
        assert_eq!(failed.failure, Some(McpFailureKind::Authentication));
        assert_eq!(
            failed.auth,
            McpAuthAvailability::StaticCredentialUnavailable
        );
    }

    #[tokio::test]
    async fn configured_failed_resource_server_is_unavailable_not_missing() {
        let server = McpServerConfig {
            name: "offline".into(),
            transport: McpTransport::Stdio {
                command: vec!["SECRET_COMMAND".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        source.starting();
        source.startup_failed(None, McpFailureKind::Startup);
        let resources = McpResourceSource::new(vec![McpResourceServer {
            name: server.name,
            source,
        }]);
        let preflight = resources
            .preflight(LIST_MCP_RESOURCES, &json!({}))
            .unwrap_err();
        assert!(
            preflight.to_string().contains("unavailable, not missing"),
            "{preflight}"
        );
        let (ready_without_resources, _wire) = resource_pair("tool-only");
        {
            let mut state = ready_without_resources.source.state.write().unwrap();
            state.capabilities.resources = false;
            state.receipt.capabilities.resources = Some(false);
        }
        let mixed =
            McpResourceSource::new(vec![ready_without_resources, resources.servers[0].clone()]);
        let mixed_error = mixed.preflight(LIST_MCP_RESOURCES, &json!({})).unwrap_err();
        assert!(
            mixed_error.to_string().contains("unavailable, not missing"),
            "{mixed_error}"
        );

        let error = match resources.list(Some("offline")).await {
            Ok(_) => panic!("failed server was reported as an empty resource catalog"),
            Err(error) => error,
        };
        let text = format!("{error:#}");
        assert!(text.contains("unavailable, not missing"), "{text}");
        assert!(!text.contains("Server \"offline\" not found"), "{text}");
        assert!(!text.contains("SECRET_COMMAND"), "{text}");

        for error in [
            match resources.read("offline", "fixture://item").await {
                Ok(_) => panic!("failed server allowed resource read"),
                Err(error) => error,
            },
            match resources.directory("offline", "fixture://dir").await {
                Ok(_) => panic!("failed server allowed directory read"),
                Err(error) => error,
            },
        ] {
            let text = format!("{error:#}");
            assert!(text.contains("unavailable, not missing"), "{text}");
            assert!(!text.contains("SECRET_COMMAND"), "{text}");
        }
    }

    #[tokio::test]
    async fn health_monitor_processes_initial_closed_snapshot() {
        let (resource_server, _wire) = resource_pair("initial-closed");
        let source = resource_server.source;
        let client = source.state.read().unwrap().client.clone().unwrap();
        let lifecycle = McpLifecycleOwner::default();
        lifecycle.register_source(source.clone());
        let (_health_tx, health_rx) = tokio::sync::watch::channel(McpTransportHealth {
            state: McpTransportState::Closed(McpTransportFailure::ConnectionEof),
            session_revision: 0,
            authentication_revision: 0,
        });
        spawn_health_monitor(
            &lifecycle,
            source.clone(),
            health_rx,
            CancellationToken::new(),
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while source.receipt().lifecycle != McpLifecycleState::Closed {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(
            source.receipt().failure,
            Some(McpFailureKind::TransportClosed)
        );
        assert!(client.call_tool("echo", &json!({})).await.is_err());
        lifecycle.shutdown().await;
    }

    #[tokio::test]
    async fn lifecycle_owner_explicit_shutdown_closes_routes() {
        let (resource_server, _wire) = resource_pair("shutdown");
        let source = resource_server.source;
        let lifecycle = McpLifecycleOwner::default();
        lifecycle.register_source(source.clone());
        assert_eq!(source.receipt().lifecycle, McpLifecycleState::Ready);

        lifecycle.shutdown().await;

        let closed = source.receipt();
        assert_eq!(closed.lifecycle, McpLifecycleState::Closed);
        assert_eq!(closed.last_health, McpHealthResult::Closed);
        assert_eq!(closed.failure, Some(McpFailureKind::Shutdown));
        match source.definition_state("shutdown__echo") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("shut the server down"), "{reason}");
            }
            state => panic!("shutdown route was exposed as {state:?}"),
        }
    }

    #[tokio::test]
    async fn session_reinitialize_updates_capabilities_and_requires_revalidation() {
        let (client_io, _server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let server = McpServerConfig {
            name: "session".into(),
            transport: McpTransport::Http {
                url: "https://example.com/mcp".into(),
                bearer_token: None,
                http_headers: BTreeMap::new(),
                oauth_client_id: None,
                oauth_scopes: Vec::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = McpToolSource::planned(&server);
        source.starting();
        source.ready(
            Arc::new(McpClient::over(reader, writer, None)),
            None,
            McpServerCapabilities::default(),
            vec![ToolDef {
                name: "echo".into(),
                description: "echo".into(),
                schema: json!({"type": "object"}),
            }],
            /*has_notifications=*/ false,
            &|_| {},
        );

        let error = source.classify_request_error(anyhow::Error::new(McpSessionReinitialized {
            capabilities: McpServerCapabilities {
                resources: true,
                directory_read: true,
                ..Default::default()
            },
        }));
        assert!(error.to_string().contains("revalidate"));
        let receipt = source.receipt();
        assert_eq!(receipt.lifecycle, McpLifecycleState::Stale);
        assert_eq!(receipt.refresh, McpRefreshState::Refreshing);
        assert_eq!(receipt.capabilities.resources, Some(true));
        assert_eq!(receipt.capabilities.directory_read, Some(true));
        assert!(source.defs().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_tool_list_recovers_reinitialized_http_session() {
        use wiremock::Mock;
        use wiremock::MockServer;
        use wiremock::ResponseTemplate;
        use wiremock::matchers::body_partial_json;
        use wiremock::matchers::header;
        use wiremock::matchers::method;

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", "sess-1")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 1,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {},
                            "serverInfo": {"name": "fixture", "version": "1"}
                        }
                    })),
            )
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .insert_header("mcp-session-id", "sess-2")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 3,
                        "result": {
                            "protocolVersion": "2025-06-18",
                            "capabilities": {"resources": {}},
                            "serverInfo": {"name": "fixture", "version": "2"}
                        }
                    })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .and(header("mcp-session-id", "sess-1"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .and(header("mcp-session-id", "sess-2"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "application/json")
                    .set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": 4,
                        "result": {
                            "tools": [{
                                "name": "echo",
                                "description": "echo",
                                "inputSchema": {"type": "object"}
                            }]
                        }
                    })),
            )
            .mount(&server)
            .await;

        let client = McpClient::http(server.uri(), BTreeMap::new(), None).unwrap();
        let mut capabilities = client.initialize().await.unwrap();
        let tools = initial_tool_catalog(&client, &mut capabilities)
            .await
            .unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        assert!(capabilities.resources);
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
            readonly_tools: Vec::new(),
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
                .contains("definition or server readiness changed after discovery")
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
            readonly_tools: Vec::new(),
        };
        let client = resource_server
            .source
            .state
            .read()
            .unwrap()
            .client
            .clone()
            .unwrap();
        let source = build_source(
            &server,
            client,
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
        let refresh = tokio::spawn(async move { refresh_source.refresh_with_retry().await });
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
        refresh.await.unwrap();
        assert_eq!(source.definition_generation("fixture__echo"), 1);
    }

    #[tokio::test]
    async fn closed_receipt_cannot_be_overwritten_by_late_refresh_or_request() {
        let (resource_server, mut wire) = resource_pair("fixture");
        let client = resource_server
            .source
            .state
            .read()
            .unwrap()
            .client
            .clone()
            .unwrap();
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "old".into(),
                schema: json!({"type": "object"}),
            }],
            &|_| {},
        );
        let refresh_source = source.clone();
        let refresh = tokio::spawn(async move { refresh_source.refresh_with_delays(&[]).await });
        let request = wire.recv().await;
        source.close(McpFailureKind::Shutdown);
        wire.respond(
            &request["id"],
            json!({
                "tools": [{
                    "name": "echo",
                    "description": "late",
                    "inputSchema": {"type": "object"}
                }]
            }),
        )
        .await;
        refresh.await.unwrap();
        let _ = source.classify_request_error(anyhow::anyhow!("late request failure"));
        source.transport_health(McpTransportState::Healthy).await;

        let receipt = source.receipt();
        assert_eq!(receipt.lifecycle, McpLifecycleState::Closed);
        assert_eq!(receipt.failure, Some(McpFailureKind::Shutdown));
        assert_eq!(receipt.catalog_generation, 0);
    }

    #[tokio::test]
    async fn refresh_failure_retains_catalog_but_blocks_until_recovery() {
        let (resource_server, mut wire) = resource_pair("fixture");
        let client = resource_server
            .source
            .state
            .read()
            .unwrap()
            .client
            .clone()
            .unwrap();
        let server = McpServerConfig {
            name: "fixture".into(),
            transport: McpTransport::Stdio {
                command: vec!["unused".into()],
                env: BTreeMap::new(),
            },
            readonly_tools: Vec::new(),
        };
        let source = build_source(
            &server,
            client,
            vec![ToolDef {
                name: "echo".into(),
                description: "old".into(),
                schema: json!({"type": "object"}),
            }],
            &|_| {},
        );
        let discovered = match source.definition_state("fixture__echo") {
            SourceDefinitionState::Available { version, .. } => version,
            state => panic!("unexpected initial state: {state:?}"),
        };

        let refresh_source = source.clone();
        let refresh =
            tokio::spawn(
                async move { refresh_source.refresh_with_delays(&[Duration::ZERO]).await },
            );
        for _ in 0..2 {
            let request = wire.recv().await;
            assert_eq!(request["method"], "tools/list");
            wire.respond_error(&request["id"], -32000, "SECRET_UPSTREAM_FAILURE")
                .await;
        }
        refresh.await.unwrap();

        let degraded = source.receipt();
        assert_eq!(degraded.lifecycle, McpLifecycleState::Degraded);
        assert_eq!(degraded.refresh, McpRefreshState::RetryExhausted);
        assert_eq!(degraded.failure, Some(McpFailureKind::Refresh));
        assert_eq!(degraded.catalog_generation, 0);
        assert!(source.defs().is_empty());
        assert_eq!(
            source.state.read().unwrap().catalog.defs[0].name,
            "fixture__echo"
        );
        match source.definition_state("fixture__echo") {
            SourceDefinitionState::Unavailable { reason } => {
                assert!(reason.contains("retained but stale"), "{reason}");
                assert!(!reason.contains("SECRET_UPSTREAM_FAILURE"), "{reason}");
            }
            state => panic!("degraded catalog was exposed as {state:?}"),
        }

        let recovery_source = source.clone();
        let recovery = tokio::spawn(async move { recovery_source.refresh_with_delays(&[]).await });
        let request = wire.recv().await;
        wire.respond(
            &request["id"],
            json!({
                "tools": [{
                    "name": "echo",
                    "description": "new",
                    "inputSchema": {"type": "object"}
                }]
            }),
        )
        .await;
        recovery.await.unwrap();
        let recovered = source.receipt();
        assert_eq!(recovered.lifecycle, McpLifecycleState::Ready);
        assert_eq!(recovered.catalog_generation, 1);
        let current = match source.definition_state("fixture__echo") {
            SourceDefinitionState::Available { version, .. } => version,
            state => panic!("unexpected recovered state: {state:?}"),
        };
        assert_ne!(current, discovered);
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
