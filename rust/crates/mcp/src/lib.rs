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
mod render;
mod sse;
mod stdio;

use std::collections::BTreeMap;
use std::collections::HashSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use base64::Engine;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;

use crate::oauth::OAuthSession;
use crate::stdio::StdioTransport;

pub use crate::render::content_blocks;
pub use crate::render::render_result;

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
