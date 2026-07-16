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
mod sse;

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use serde_json::json;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::oneshot;

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

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>;
}

/// One connected MCP server. All methods take `&self`; concurrent calls are
/// multiplexed by the transport. Dropping the client tears the transport down
/// (stdio kills the child via kill_on_drop and stops the reader task).
pub struct McpClient {
    transport: Box<dyn Transport>,
}

impl McpClient {
    /// Spawn `command` (argv; `env` is merged on top of the inherited
    /// environment) and speak MCP over its stdio. stderr goes to null: the
    /// TUI owns the terminal, and a chatty server would corrupt it.
    pub fn spawn(command: &[String], env: &BTreeMap<String, String>) -> Result<Self> {
        Ok(Self {
            transport: Box::new(StdioTransport::spawn(command, env)?),
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
        }
    }

    /// Speak MCP to a remote server over streamable HTTP. `headers` are extra
    /// request headers (e.g. `Authorization: Bearer …`, custom headers) — the
    /// CLI resolves any secrets before they reach here. Fails only if a header
    /// name/value is malformed.
    pub fn http(url: String, headers: BTreeMap<String, String>) -> Result<Self> {
        Ok(Self {
            transport: Box::new(http::HttpTransport::new(url, headers)?),
        })
    }

    /// Wrap a pre-built transport — the seam the HTTP module's tests use to
    /// inject a fast retry schedule.
    #[cfg(test)]
    pub(crate) fn from_transport(transport: Box<dyn Transport>) -> Self {
        Self { transport }
    }

    /// The MCP handshake: `initialize`, then the REQUIRED
    /// `notifications/initialized` (strict servers refuse further requests
    /// without it).
    pub async fn initialize(&self) -> Result<()> {
        self.transport
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
        self.transport
            .notify("notifications/initialized", None)
            .await
    }

    /// Full tool list (follows `nextCursor` pagination). Names are the raw
    /// server-side names — namespacing is the caller's concern. The
    /// inputSchema passes through verbatim as the ToolDef schema.
    pub async fn list_tools(&self) -> Result<Vec<kloop_protocol::ToolDef>> {
        let mut tools = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
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
            match result["nextCursor"].as_str() {
                Some(c) => cursor = Some(c.to_string()),
                None => break,
            }
        }
        Ok(tools)
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
        if result["isError"].as_bool().unwrap_or(false) {
            bail!("{}", render_result(&result));
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

/// The stdio transport: newline-delimited JSON over a child process's (or, in
/// tests, a duplex pipe's) byte streams. A background reader task routes
/// responses to pending requests by id.
struct StdioTransport {
    writer: SharedWriter,
    pending: Pending,
    next_id: AtomicU64,
    reader: tokio::task::JoinHandle<()>,
    /// Held only so kill_on_drop fires when the transport is dropped.
    _child: Option<tokio::process::Child>,
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.reader.abort();
    }
}

impl StdioTransport {
    fn spawn(command: &[String], env: &BTreeMap<String, String>) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .context("mcp server command must not be empty")?;
        let mut child = tokio::process::Command::new(program)
            .args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
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
        let reader_task = tokio::spawn(read_loop(reader, writer.clone(), pending.clone()));
        StdioTransport {
            writer,
            pending,
            next_id: AtomicU64::new(1),
            reader: reader_task,
            _child: child,
        }
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
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            let (tx, rx) = oneshot::channel();
            self.pending.lock().unwrap().insert(id, tx);
            let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            if let Err(e) = write_line(&self.writer, &msg).await {
                self.pending.lock().unwrap().remove(&id);
                return Err(e);
            }
            match tokio::time::timeout(timeout, rx).await {
                Ok(Ok(result)) => result,
                Ok(Err(_)) => Err(anyhow!("{method}: connection closed before response")),
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    Err(anyhow!("{method}: no response within {timeout:?}"))
                }
            }
        })
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let msg = match params {
                Some(params) => json!({"jsonrpc": "2.0", "method": method, "params": params}),
                None => json!({"jsonrpc": "2.0", "method": method}),
            };
            write_line(&self.writer, &msg).await
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

/// Reader half: routes responses to pending requests by id, refuses
/// server-to-client requests with -32601 (we advertise no capabilities),
/// ignores notifications, and fails every pending request on EOF.
async fn read_loop(
    reader: impl AsyncRead + Send + Unpin + 'static,
    writer: SharedWriter,
    pending: Pending,
) {
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue; // stray non-JSON output; skip rather than kill the link
        };
        if msg.get("method").is_some() {
            // Server-initiated request (roots/list, sampling, ...): we
            // support none, so answer method-not-found. id-less = a
            // notification; ignored.
            if let Some(id) = msg.get("id") {
                let refusal = json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {"code": -32601, "message": "method not supported by this client"},
                });
                let _ = write_line(&writer, &refusal).await;
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
            Some(err) => Err(anyhow!(
                "mcp error {}: {}",
                err["code"].as_i64().unwrap_or(0),
                err["message"].as_str().unwrap_or("unknown")
            )),
            None => Ok(msg["result"].clone()),
        };
        let _ = tx.send(outcome);
    }
    // EOF or read error: everything still in flight gets a definite answer.
    let stranded: Vec<_> = pending.lock().unwrap().drain().collect();
    for (_, tx) in stranded {
        let _ = tx.send(Err(anyhow!("mcp server closed the connection")));
    }
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

/// Build canonical content blocks from an MCP content array **when it carries
/// at least one usable image** — so the model sees the picture instead of an
/// `[image: …]` tag. Returns `None` for a text-only (or image-free) result,
/// whose flattened-text path stays byte-identical. When an image is present,
/// every non-image item folds into `Text` blocks (rendered as in
/// [`render_item`]), preserving order relative to the images.
pub fn content_blocks(content: &Value) -> Option<Vec<ContentBlock>> {
    let items = content.as_array()?;
    let has_usable_image = items.iter().any(|it| {
        it["type"] == "image" && it["mimeType"].as_str().is_some_and(supported_image_mime)
    });
    if !has_usable_image {
        return None;
    }
    let mut blocks = Vec::new();
    let mut pending = String::new();
    for item in items {
        let mime = item["mimeType"].as_str();
        if item["type"] == "image" && mime.is_some_and(supported_image_mime) {
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
