//! kloop-mcp — MCP client speaking JSON-RPC 2.0 over one of two transports:
//! a child process's stdio (newline-delimited JSON, one object per line — NOT
//! LSP-style Content-Length framing) or streamable HTTP (one POST per request,
//! replies as `application/json` or a short-lived `text/event-stream`, with
//! `Mcp-Session-Id` sessions; plan 34).
//!
//! This crate speaks the wire protocol only. Tool naming (`{server}__{tool}`),
//! config, secret resolution, and the `ToolSource` adapter live in the CLI;
//! core never depends on this crate.

mod http;
pub mod oauth;
mod sse;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::HashSet;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use base64::Engine;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::oneshot;

use crate::oauth::OAuthSession;

/// Sent in `initialize`. Servers negotiate back the version they support;
/// this client accepts whatever comes back (minimal-client stance).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// `npx -y ...` servers re-resolve against the registry on every start —
/// 13s cold-ish starts measured in the wild. 30s matches both reference
/// implementations' startup timeouts.
pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);
/// tools/list can be slower on servers that generate schemas lazily.
pub(crate) const LIST_TIMEOUT: Duration = Duration::from_secs(30);
/// Matches the built-in bash tool's default budget.
pub(crate) const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Descriptions from OpenAPI-generated servers have been seen at 15-60KB;
/// cap them before they land in every sampling request (cc uses the same
/// limit).
const MAX_DESCRIPTION_CHARS: usize = 2048;
const MAX_LIST_PAGES: usize = 20;
const MAX_LIST_ITEMS: usize = 10_000;
const MAX_LIST_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_WIRE_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const MAX_CALL_CONTENTS: usize = 256;
const MAX_CALL_IMAGE_BYTES: usize = 5 * 1024 * 1024;
const MAX_READ_CONTENTS: usize = 256;
const MAX_READ_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct McpServerCapabilities {
    pub tools_list_changed: bool,
    pub resources: bool,
    pub resources_list_changed: bool,
    pub directory_read: bool,
}

impl McpServerCapabilities {
    fn from_initialize(result: &Value) -> Self {
        let capabilities = &result["capabilities"];
        McpServerCapabilities {
            tools_list_changed: capabilities["tools"]["listChanged"]
                .as_bool()
                .unwrap_or(false),
            resources: !capabilities["resources"].is_null(),
            resources_list_changed: capabilities["resources"]["listChanged"]
                .as_bool()
                .unwrap_or(false),
            directory_read:
                capabilities["extensions"]["io.modelcontextprotocol/skills"]["directoryRead"]
                    .as_bool()
                    .unwrap_or(false),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum McpNotification {
    ToolsListChanged,
    ResourcesListChanged,
    ResourceUpdated { uri: String },
}

/// A bounded, secret-free transport health snapshot. Persistent failures use the
/// `state` axis; HTTP session/auth recovery increment independent revisions so
/// coalesced watch updates cannot lose either edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct McpTransportHealth {
    pub state: McpTransportState,
    pub session_revision: u64,
    pub authentication_revision: u64,
}

impl McpTransportHealth {
    fn healthy() -> Self {
        Self {
            state: McpTransportState::Healthy,
            session_revision: 0,
            authentication_revision: 0,
        }
    }

    fn is_closed(self) -> bool {
        matches!(self.state, McpTransportState::Closed(_))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpTransportState {
    Healthy,
    Degraded(McpTransportFailure),
    Closed(McpTransportFailure),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum McpTransportFailure {
    RequestTimeout,
    WriteFailed,
    ConnectionEof,
    ReadFailed,
    MessageTooLarge,
    ExplicitShutdown,
}

impl McpNotification {
    fn from_message(message: &Value) -> Option<Self> {
        match message["method"].as_str()? {
            "notifications/tools/list_changed" => Some(McpNotification::ToolsListChanged),
            "notifications/resources/list_changed" => Some(McpNotification::ResourcesListChanged),
            "notifications/resources/updated" => Some(McpNotification::ResourceUpdated {
                uri: message["params"]["uri"].as_str()?.to_string(),
            }),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub raw: Value,
}

impl McpResource {
    fn parse(value: &Value) -> Result<Self> {
        Ok(McpResource {
            uri: value["uri"]
                .as_str()
                .context("resource entry has no uri")?
                .to_string(),
            name: value["name"]
                .as_str()
                .context("resource entry has no name")?
                .to_string(),
            description: value["description"].as_str().map(str::to_string),
            mime_type: value["mimeType"].as_str().map(str::to_string),
            raw: value.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpResourceContent {
    pub uri: String,
    pub mime_type: Option<String>,
    pub text: Option<String>,
    pub blob: Option<String>,
    pub raw: Value,
}

impl McpResourceContent {
    fn parse(value: &Value) -> Result<Self> {
        let text = value["text"].as_str().map(str::to_string);
        let blob = value["blob"].as_str().map(str::to_string);
        if let Some(blob) = &blob {
            base64::engine::general_purpose::STANDARD
                .decode(blob)
                .context("resource content blob is not valid base64")?;
        }
        if text.is_none() && blob.is_none() {
            bail!("resource content has neither text nor blob");
        }
        Ok(McpResourceContent {
            uri: value["uri"]
                .as_str()
                .context("resource content has no uri")?
                .to_string(),
            mime_type: value["mimeType"].as_str().map(str::to_string),
            text,
            blob,
            raw: value.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct McpReadResourceResult {
    pub contents: Vec<McpResourceContent>,
    pub raw: Value,
}

#[derive(Debug)]
pub struct McpRpcError {
    pub code: i64,
    pub message: String,
}

impl std::fmt::Display for McpRpcError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "mcp error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for McpRpcError {}

/// A successful `tools/call` response whose MCP `isError` flag is true. This is
/// a tool-level failure, not evidence that the server transport is unhealthy.
#[derive(Debug)]
pub struct McpToolCallError {
    pub message: String,
}

impl std::fmt::Display for McpToolCallError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for McpToolCallError {}

/// The HTTP transport established a new MCP session. The original operation was
/// not replayed because the new capability/catalog must be revalidated first.
#[derive(Clone, Debug)]
pub struct McpSessionReinitialized {
    pub capabilities: McpServerCapabilities,
}

impl std::fmt::Display for McpSessionReinitialized {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(
            "MCP HTTP session was reinitialized; revalidate the server catalog before retrying",
        )
    }
}

impl std::error::Error for McpSessionReinitialized {}

/// A request was rejected for authentication. This contains no response body,
/// endpoint, header or credential data.
#[derive(Debug)]
pub struct McpAuthenticationError;

impl std::fmt::Display for McpAuthenticationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MCP authentication was rejected; authenticate before retrying")
    }
}

impl std::error::Error for McpAuthenticationError {}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type SharedWriter = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// A JSON-RPC message channel under [`McpClient`]: send a request and await its
/// response, or fire a notification. Stdio (a persistent read loop keyed by id)
/// and streamable HTTP (one POST per request) both implement it, so the
/// protocol layer — handshake, pagination, tools/call, content blocks — stays
/// transport-agnostic.
pub(crate) trait Transport: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>>;

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<McpNotification>> {
        None
    }

    fn subscribe_health(&self) -> Option<tokio::sync::watch::Receiver<McpTransportHealth>> {
        None
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;

    fn abort(&self) {}

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// One connected MCP server. All methods take `&self`; concurrent calls are
/// multiplexed by the transport. Dropping the client tears the transport down
/// (stdio kills the child via kill_on_drop and stops the reader task).
pub struct McpClient {
    transport: Box<dyn Transport>,
    capabilities: Mutex<McpServerCapabilities>,
}

impl McpClient {
    /// Spawn `command` (argv; `env` is merged on top of the inherited
    /// environment) and speak MCP over its stdio. stderr goes to null: the
    /// TUI owns the terminal, and a chatty server would corrupt it.
    pub fn spawn(command: &[String], env: &BTreeMap<String, String>) -> Result<Self> {
        Ok(Self {
            transport: Box::new(StdioTransport::spawn(command, env)?),
            capabilities: Mutex::new(McpServerCapabilities::default()),
        })
    }

    /// Wire the client over any byte streams — the testing seam (duplex
    /// pipes in-process) and the guts of [`McpClient::spawn`].
    pub fn over(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        child: Option<tokio::process::Child>,
    ) -> Self {
        Self {
            transport: Box::new(StdioTransport::over(reader, writer, child)),
            capabilities: Mutex::new(McpServerCapabilities::default()),
        }
    }

    /// Speak MCP to a remote server over streamable HTTP. `headers` are extra
    /// static request headers (custom headers, or a static `Authorization:
    /// Bearer …` — the CLI resolves any secrets before they reach here).
    /// `oauth`, when present, injects (and refreshes) the bearer per request
    /// instead — the two auth modes are mutually exclusive by construction.
    /// Fails only if a header name/value is malformed.
    pub fn http(
        url: String,
        headers: BTreeMap<String, String>,
        oauth: Option<Arc<OAuthSession>>,
    ) -> Result<Self> {
        Ok(Self {
            transport: Box::new(http::HttpTransport::new(url, headers, oauth)?),
            capabilities: Mutex::new(McpServerCapabilities::default()),
        })
    }

    /// Wrap a pre-built transport — the seam the HTTP module's tests use to
    /// inject a fast retry schedule.
    #[cfg(test)]
    pub(crate) fn from_transport(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            capabilities: Mutex::new(McpServerCapabilities::default()),
        }
    }

    /// The MCP handshake: `initialize`, then the REQUIRED
    /// `notifications/initialized` (strict servers refuse further requests
    /// without it).
    pub async fn initialize(&self) -> Result<McpServerCapabilities> {
        let result = self
            .transport
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "kloop", "version": env!("CARGO_PKG_VERSION")},
                }),
                HANDSHAKE_TIMEOUT,
            )
            .await
            .context("mcp initialize failed")?;
        let capabilities = McpServerCapabilities::from_initialize(&result);
        *self.capabilities.lock().unwrap() = capabilities.clone();
        self.transport
            .notify("notifications/initialized", None)
            .await?;
        Ok(capabilities)
    }

    pub fn capabilities(&self) -> McpServerCapabilities {
        self.capabilities.lock().unwrap().clone()
    }

    pub fn subscribe_notifications(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<McpNotification>> {
        self.transport.subscribe()
    }

    pub fn subscribe_health(&self) -> Option<tokio::sync::watch::Receiver<McpTransportHealth>> {
        self.transport.subscribe_health()
    }

    /// Stop transport-owned tasks and child processes without waiting. The
    /// lifecycle owner uses this as its synchronous Drop fallback.
    pub fn abort(&self) {
        self.transport.abort();
    }

    /// Explicit transport shutdown. Stdio closes its reader, fails pending
    /// requests and reaps the child; request-scoped HTTP has nothing persistent.
    pub async fn shutdown(&self) {
        self.transport.shutdown().await;
    }

    /// Full tool list (follows `nextCursor` pagination). Names are the raw
    /// server-side names — namespacing is the caller's concern. The
    /// inputSchema passes through verbatim as the ToolDef schema.
    pub async fn list_tools(&self) -> Result<Vec<kloop_protocol::ToolDef>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut total_bytes = 0;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self
                .transport
                .request("tools/list", params, LIST_TIMEOUT)
                .await?;
            let listed = result["tools"]
                .as_array()
                .context("tools/list result has no tools array")?;
            total_bytes += serde_json::to_vec(&result)?.len();
            if total_bytes > MAX_LIST_BYTES || tools.len() + listed.len() > MAX_LIST_ITEMS {
                bail!("tools/list result exceeds the tool catalog budget");
            }
            for tool in listed {
                let name = tool["name"]
                    .as_str()
                    .context("tools/list entry has no name")?;
                let description: String = tool["description"]
                    .as_str()
                    .unwrap_or("")
                    .chars()
                    .take(MAX_DESCRIPTION_CHARS)
                    .collect();
                let schema = match &tool["inputSchema"] {
                    Value::Null => json!({"type": "object"}),
                    other => other.clone(),
                };
                tools.push(kloop_protocol::ToolDef {
                    name: name.to_string(),
                    description,
                    schema,
                });
            }
            let Some(next) = result["nextCursor"].as_str() else {
                return Ok(tools);
            };
            if !seen_cursors.insert(next.to_string()) {
                bail!("tools/list repeated nextCursor '{next}'");
            }
            cursor = Some(next.to_string());
        }
        bail!("tools/list exceeds the {MAX_LIST_PAGES}-page limit")
    }

    async fn list_resources_method(
        &self,
        method: &str,
        base_params: &Value,
    ) -> Result<Vec<McpResource>> {
        let mut resources = Vec::new();
        let mut cursor: Option<String> = None;
        let mut seen_cursors = HashSet::new();
        let mut total_bytes = 0;
        for _ in 0..MAX_LIST_PAGES {
            let mut params = base_params.clone();
            let params_object = params
                .as_object_mut()
                .context("resource list params must be an object")?;
            if let Some(cursor) = &cursor {
                params_object.insert("cursor".into(), Value::String(cursor.clone()));
            }
            let result = self.transport.request(method, params, LIST_TIMEOUT).await?;
            let listed = result["resources"]
                .as_array()
                .with_context(|| format!("{method} result has no resources array"))?;
            total_bytes += serde_json::to_vec(&result)?.len();
            if total_bytes > MAX_LIST_BYTES || resources.len() + listed.len() > MAX_LIST_ITEMS {
                bail!("{method} result exceeds the resource catalog budget");
            }
            for resource in listed {
                resources.push(McpResource::parse(resource)?);
            }
            let Some(next) = result["nextCursor"].as_str() else {
                return Ok(resources);
            };
            if !seen_cursors.insert(next.to_string()) {
                bail!("{method} repeated nextCursor '{next}'");
            }
            cursor = Some(next.to_string());
        }
        bail!("{method} exceeds the {MAX_LIST_PAGES}-page limit")
    }

    pub async fn list_resources(&self) -> Result<Vec<McpResource>> {
        self.list_resources_method("resources/list", &json!({}))
            .await
    }

    pub async fn read_resource(&self, uri: &str) -> Result<McpReadResourceResult> {
        let result = self
            .transport
            .request("resources/read", json!({"uri": uri}), LIST_TIMEOUT)
            .await?;
        let listed = result["contents"]
            .as_array()
            .context("resources/read result has no contents array")?;
        if listed.len() > MAX_READ_CONTENTS || serde_json::to_vec(&result)?.len() > MAX_READ_BYTES {
            bail!("resources/read result exceeds the resource content budget");
        }
        let contents = listed
            .iter()
            .map(McpResourceContent::parse)
            .collect::<Result<Vec<_>>>()?;
        Ok(McpReadResourceResult {
            contents,
            raw: result,
        })
    }

    pub async fn read_resource_directory(&self, uri: &str) -> Result<Vec<McpResource>> {
        self.list_resources_method("resources/directory/read", &json!({"uri": uri}))
            .await
    }

    /// One tools/call with the RAW tool name, returning the full structured
    /// `CallToolResult` (content blocks, `structuredContent`, `_meta`, …).
    /// `isError: true` (a successful JSON-RPC response marking a failed tool
    /// run) surfaces as Err with the rendered content, like a failing built-in.
    pub async fn call_tool_structured(&self, name: &str, arguments: &Value) -> Result<Value> {
        let result = self
            .transport
            .request(
                "tools/call",
                json!({"name": name, "arguments": arguments}),
                CALL_TIMEOUT,
            )
            .await?;
        validate_call_tool_result(&result)?;
        if result["isError"].as_bool().unwrap_or(false) {
            return Err(anyhow::Error::new(McpToolCallError {
                message: render_result(&result),
            }));
        }
        Ok(result)
    }

    /// The same call flattened to the plain text a tool_result carries — the
    /// model-facing path. A program instead gets the structured result above.
    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<String> {
        Ok(render_result(
            &self.call_tool_structured(name, arguments).await?,
        ))
    }
}

fn validate_call_tool_result(result: &Value) -> Result<()> {
    if serde_json::to_vec(result)?.len() > MAX_WIRE_MESSAGE_BYTES {
        bail!("tools/call result exceeds the tool result budget");
    }
    let content = result["content"]
        .as_array()
        .context("tools/call result has no content array")?;
    if content.len() > MAX_CALL_CONTENTS {
        bail!("tools/call result exceeds the content item budget");
    }
    for item in content {
        if item["type"] != "image" {
            continue;
        }
        let data = item["data"]
            .as_str()
            .context("tools/call image content has no base64 data")?;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(data)
            .context("tools/call image data is not valid base64")?;
        if decoded.len() > MAX_CALL_IMAGE_BYTES {
            bail!("tools/call image exceeds the decoded image budget");
        }
    }
    Ok(())
}

struct PendingRequest {
    id: u64,
    pending: Pending,
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

/// The stdio transport: newline-delimited JSON over a child process's (or, in
/// tests, a duplex pipe's) byte streams. A background reader task routes
/// responses to pending requests by id.
struct StdioTransport {
    writer: SharedWriter,
    pending: Pending,
    notifications: tokio::sync::broadcast::Sender<McpNotification>,
    health: tokio::sync::watch::Sender<McpTransportHealth>,
    next_id: AtomicU64,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    child: Mutex<Option<tokio::process::Child>>,
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.abort();
    }
}

impl StdioTransport {
    fn spawn(command: &[String], env: &BTreeMap<String, String>) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .context("mcp server command must not be empty")?;
        let mut process = tokio::process::Command::new(program);
        process
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = kloop_process_spawn::spawn(&mut process)
            .with_context(|| format!("cannot spawn mcp server '{program}'"))?;
        let stdin = child.stdin.take().context("mcp child stdin unavailable")?;
        let stdout = child
            .stdout
            .take()
            .context("mcp child stdout unavailable")?;
        Ok(Self::over(stdout, stdin, Some(child)))
    }

    fn over(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        child: Option<tokio::process::Child>,
    ) -> Self {
        let writer: SharedWriter = Arc::new(tokio::sync::Mutex::new(Box::new(writer)));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (notifications, _) = tokio::sync::broadcast::channel(32);
        let (health, _) = tokio::sync::watch::channel(McpTransportHealth::healthy());
        let reader_task = tokio::spawn(read_loop(
            reader,
            writer.clone(),
            pending.clone(),
            notifications.clone(),
            health.clone(),
        ));
        StdioTransport {
            writer,
            pending,
            notifications,
            health,
            next_id: AtomicU64::new(1),
            reader: Mutex::new(Some(reader_task)),
            child: Mutex::new(child),
        }
    }

    fn publish_health(&self, state: McpTransportState) {
        publish_transport_state(&self.health, state);
    }

    fn close_local(&self, failure: McpTransportFailure) {
        self.publish_health(McpTransportState::Closed(failure));
        if let Some(reader) = self.reader.lock().unwrap().take() {
            reader.abort();
        }
        fail_pending(&self.pending, "mcp transport closed");
    }
}

fn publish_transport_state(
    health: &tokio::sync::watch::Sender<McpTransportHealth>,
    state: McpTransportState,
) {
    health.send_if_modified(|current| {
        if current.is_closed() {
            return false;
        }
        current.state = state;
        true
    });
}

fn fail_pending(pending: &Pending, reason: &str) {
    let stranded: Vec<_> = pending.lock().unwrap().drain().collect();
    for (_, tx) in stranded {
        let _ = tx.send(Err(anyhow!(reason.to_string())));
    }
}

impl Transport for StdioTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            if self.health.borrow().is_closed() {
                bail!("{method}: MCP transport is closed");
            }
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id, tx);
            let _pending = PendingRequest {
                id,
                pending: self.pending.clone(),
            };
            if self.health.borrow().is_closed() {
                bail!("{method}: MCP transport closed while registering the request");
            }
            let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            if let Err(error) = write_line(&self.writer, &msg).await {
                self.close_local(McpTransportFailure::WriteFailed);
                return Err(error);
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(result)) => {
                    self.publish_health(McpTransportState::Healthy);
                    result
                }
                Ok(Err(_)) => {
                    self.publish_health(McpTransportState::Closed(
                        McpTransportFailure::ConnectionEof,
                    ));
                    Err(anyhow!("{method}: connection closed before response"))
                }
                Err(_) => {
                    self.publish_health(McpTransportState::Degraded(
                        McpTransportFailure::RequestTimeout,
                    ));
                    Err(anyhow!("{method}: no response within {timeout:?}"))
                }
            }
        })
    }

    fn subscribe(&self) -> Option<tokio::sync::broadcast::Receiver<McpNotification>> {
        Some(self.notifications.subscribe())
    }

    fn subscribe_health(&self) -> Option<tokio::sync::watch::Receiver<McpTransportHealth>> {
        Some(self.health.subscribe())
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if self.health.borrow().is_closed() {
                bail!("{method}: MCP transport is closed");
            }
            let msg = match params {
                Some(params) => json!({"jsonrpc": "2.0", "method": method, "params": params}),
                None => json!({"jsonrpc": "2.0", "method": method}),
            };
            if let Err(error) = write_line(&self.writer, &msg).await {
                self.close_local(McpTransportFailure::WriteFailed);
                return Err(error);
            }
            self.publish_health(McpTransportState::Healthy);
            Ok(())
        })
    }

    fn abort(&self) {
        self.close_local(McpTransportFailure::ExplicitShutdown);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.start_kill();
        }
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.close_local(McpTransportFailure::ExplicitShutdown);
            let child = self.child.lock().unwrap().take();
            if let Some(mut child) = child {
                let _ = child.kill().await;
            }
        })
    }
}

async fn write_line(writer: &SharedWriter, msg: &Value) -> Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    let mut writer = writer.lock().await;
    writer
        .write_all(line.as_bytes())
        .await
        .context("mcp write failed")?;
    writer.flush().await.context("mcp flush failed")
}

async fn read_bounded_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut line = Vec::new();
    let mut oversized = false;
    loop {
        let (consumed, ended) = {
            let buffer = reader.fill_buf().await?;
            if buffer.is_empty() {
                if oversized {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("MCP message exceeds the {limit}-byte wire limit"),
                    ));
                }
                return if line.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(line))
                };
            }
            let newline = buffer.iter().position(|byte| *byte == b'\n');
            let take = newline.unwrap_or(buffer.len());
            if !oversized {
                if line.len().saturating_add(take) > limit {
                    oversized = true;
                } else {
                    line.extend_from_slice(&buffer[..take]);
                }
            }
            (take + usize::from(newline.is_some()), newline.is_some())
        };
        reader.consume(consumed);
        if ended {
            if oversized {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("MCP message exceeds the {limit}-byte wire limit"),
                ));
            }
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            return Ok(Some(line));
        }
    }
}

/// Reader half: routes responses to pending requests by id, refuses
/// server-to-client requests with -32601, publishes recognized notifications,
/// and fails every pending request on EOF.
async fn read_loop(
    reader: impl AsyncRead + Send + Unpin + 'static,
    writer: SharedWriter,
    pending: Pending,
    notifications: tokio::sync::broadcast::Sender<McpNotification>,
    health: tokio::sync::watch::Sender<McpTransportHealth>,
) {
    let mut reader = BufReader::new(reader);
    let (terminal_failure, terminal_error) = loop {
        let line = match read_bounded_line(&mut reader, MAX_WIRE_MESSAGE_BYTES).await {
            Ok(Some(line)) => line,
            Ok(None) => {
                break (
                    McpTransportFailure::ConnectionEof,
                    "mcp server closed the connection".to_string(),
                );
            }
            Err(error) => {
                let failure = if error.kind() == io::ErrorKind::InvalidData {
                    McpTransportFailure::MessageTooLarge
                } else {
                    McpTransportFailure::ReadFailed
                };
                break (failure, format!("mcp read failed: {error}"));
            }
        };
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
            continue; // stray non-JSON output; skip rather than kill the link
        };
        if msg.get("method").is_some() {
            if let Some(id) = msg.get("id") {
                let refusal = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "method not supported by this client"},
                });
                if let Err(error) = write_line(&writer, &refusal).await {
                    break (
                        McpTransportFailure::WriteFailed,
                        format!("mcp write failed: {error}"),
                    );
                }
            } else if let Some(notification) = McpNotification::from_message(&msg) {
                let _ = notifications.send(notification);
            }
            continue;
        }
        let Some(id) = msg["id"].as_u64() else {
            continue;
        };
        let Some(tx) = pending.lock().unwrap().remove(&id) else {
            continue; // response to a timed-out request; drop
        };
        let outcome = match msg.get("error") {
            Some(err) => Err(anyhow::Error::new(McpRpcError {
                code: err["code"].as_i64().unwrap_or(0),
                message: err["message"].as_str().unwrap_or("unknown").to_string(),
            })),
            None => Ok(msg["result"].clone()),
        };
        let _ = tx.send(outcome);
    };
    // EOF, read failure, or an oversized frame: publish one bounded health
    // transition before failing every in-flight request.
    publish_transport_state(&health, McpTransportState::Closed(terminal_failure));
    fail_pending(&pending, &terminal_error);
}

/// Flatten a `CallToolResult`'s content array into the plain text a tool_result
/// carries — the model-facing rendering. Exposed so the CLI can render the text
/// side of a structured result without a second wire call.
pub fn render_result(result: &Value) -> String {
    render_content(&result["content"])
}

/// Flatten an MCP content array into the plain text a tool_result carries.
/// Text passes through; resources keep their text with a provenance tag;
/// binary payloads degrade to a placeholder (this is the text-only rendering —
/// [`content_blocks`] is what keeps images as real image blocks).
fn render_content(content: &Value) -> String {
    let Some(items) = content.as_array() else {
        return "(no content)".to_string();
    };
    if items.is_empty() {
        return "(no content)".to_string();
    }
    items.iter().map(render_item).collect::<Vec<_>>().join("\n")
}

/// One MCP content item rendered to plain text. An image becomes an
/// `[image: <mime>]` tag here; [`content_blocks`] instead keeps it as an image
/// block when the mime type is one the models accept.
fn render_item(item: &Value) -> String {
    match item["type"].as_str() {
        Some("text") => item["text"].as_str().unwrap_or("").to_string(),
        Some("image") => format!(
            "[image: {}]",
            item["mimeType"].as_str().unwrap_or("unknown type")
        ),
        Some("audio") => format!(
            "[audio: {}]",
            item["mimeType"].as_str().unwrap_or("unknown type")
        ),
        Some("resource") => {
            let uri = item["resource"]["uri"].as_str().unwrap_or("?");
            match item["resource"]["text"].as_str() {
                Some(text) => format!("[resource {uri}]\n{text}"),
                None => format!("[resource {uri}: binary]"),
            }
        }
        Some("resource_link") => {
            format!("[resource link: {}]", item["uri"].as_str().unwrap_or("?"))
        }
        _ => format!("[unsupported content: {item}]"),
    }
}

/// Media types the models accept as image blocks (Anthropic's set). An MCP
/// image with any other mime stays text — a bad block would make the whole
/// request fail, so degrade rather than risk it.
fn supported_image_mime(mime: &str) -> bool {
    matches!(
        mime,
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    )
}

fn usable_image(item: &Value) -> bool {
    item["type"] == "image"
        && item["mimeType"].as_str().is_some_and(supported_image_mime)
        && item["data"].as_str().is_some_and(|data| {
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .is_ok_and(|decoded| decoded.len() <= MAX_CALL_IMAGE_BYTES)
        })
}

/// Build canonical content blocks from an MCP content array **when it carries
/// at least one usable image** — so the model sees the picture instead of an
/// `[image: …]` tag. Returns `None` for a text-only (or image-free) result,
/// whose flattened-text path stays byte-identical. When an image is present,
/// every non-image item folds into `Text` blocks (rendered as in
/// [`render_item`]), preserving order relative to the images.
pub fn content_blocks(content: &Value) -> Option<Vec<ContentBlock>> {
    let items = content.as_array()?;
    let has_usable_image = items.iter().any(usable_image);
    if !has_usable_image {
        return None;
    }
    let mut blocks = Vec::new();
    let mut pending = String::new();
    for item in items {
        let mime = item["mimeType"].as_str();
        if usable_image(item) {
            if !pending.is_empty() {
                blocks.push(ContentBlock::Text {
                    text: std::mem::take(&mut pending),
                });
            }
            blocks.push(ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: mime.unwrap_or_default().to_string(),
                    data: item["data"].as_str().unwrap_or_default().to_string(),
                },
            });
        } else {
            if !pending.is_empty() {
                pending.push('\n');
            }
            pending.push_str(&render_item(item));
        }
    }
    if !pending.is_empty() {
        blocks.push(ContentBlock::Text { text: pending });
    }
    Some(blocks)
}

#[cfg(test)]
mod wire_tests {
    use super::read_bounded_line;
    use tokio::io::AsyncWriteExt;
    use tokio::io::BufReader;

    #[tokio::test]
    async fn bounded_line_rejects_oversized_frames_before_json_parsing() {
        let (mut writer, reader) = tokio::io::duplex(64);
        writer.write_all(b"12345\nnext\n").await.unwrap();
        drop(writer);
        let mut reader = BufReader::new(reader);
        let error = read_bounded_line(&mut reader, 4).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("wire limit"));
    }

    struct FailingWriter;

    impl tokio::io::AsyncWrite for FailingWriter {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            _buf: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "fixture write failure",
            )))
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn failed_server_request_refusal_closes_transport_health() {
        use super::McpTransportFailure;
        use super::McpTransportState;
        use super::StdioTransport;
        use super::Transport;

        let (mut server_writer, client_reader) = tokio::io::duplex(4096);
        let transport = StdioTransport::over(client_reader, FailingWriter, None);
        let mut health = transport.subscribe_health().unwrap();
        server_writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"roots/list\"}\n")
            .await
            .unwrap();
        health.changed().await.unwrap();
        assert_eq!(
            health.borrow_and_update().state,
            McpTransportState::Closed(McpTransportFailure::WriteFailed)
        );
    }

    #[tokio::test]
    async fn dropping_request_future_removes_pending_entry() {
        use super::StdioTransport;
        use super::Transport;
        use serde_json::json;
        use std::sync::Arc;
        use std::time::Duration;
        use tokio::io::AsyncBufReadExt;

        let (client_io, server_io) = tokio::io::duplex(4096);
        let (reader, writer) = tokio::io::split(client_io);
        let (server_reader, _server_writer) = tokio::io::split(server_io);
        let transport = Arc::new(StdioTransport::over(reader, writer, None));
        let request_transport = transport.clone();
        let request = tokio::spawn(async move {
            request_transport
                .request("tools/call", json!({}), Duration::from_secs(60))
                .await
        });
        let mut lines = BufReader::new(server_reader).lines();
        let _ = lines.next_line().await.unwrap().unwrap();
        assert_eq!(transport.pending.lock().unwrap().len(), 1);
        request.abort();
        let _ = request.await;
        tokio::task::yield_now().await;
        assert!(transport.pending.lock().unwrap().is_empty());
    }
}
