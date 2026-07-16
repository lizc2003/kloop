//! Streamable HTTP transport (MCP spec 2025-03-26 "Streamable HTTP", plus the
//! 2025-06-18 `MCP-Protocol-Version` header). One POST per JSON-RPC request to
//! a single endpoint; the server replies either as `application/json` (a single
//! message) or `text/event-stream` (a short-lived SSE stream that eventually
//! carries our response). Sessions ride the `Mcp-Session-Id` header: the server
//! assigns one on `initialize`, the client echoes it thereafter, and a `404`
//! for a session-bearing request means the session expired → re-handshake once.
//!
//! Transport details (POST/Accept, session header, SSE framing) follow the
//! official spec; the two reference implementations are only cross-checks
//! (plan 10's lesson: a reference's framing can be wrong).

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use futures::StreamExt;
use reqwest::header::HeaderMap;
use reqwest::header::HeaderName;
use reqwest::header::HeaderValue;
use reqwest::header::ACCEPT;
use reqwest::header::CONTENT_TYPE;
use serde_json::json;
use serde_json::Value;

use crate::sse::SseParser;
use crate::Transport;

/// codex's schedule: back off 250ms, then 1s, then one final attempt (3
/// tries total). Only 408/429/5xx and transient network errors retry.
const DEFAULT_RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(250), Duration::from_secs(1)];

/// `notifications/initialized` and re-handshake POSTs are quick; reuse the
/// handshake budget for them.
const NOTIFY_TIMEOUT: Duration = crate::HANDSHAKE_TIMEOUT;

pub(crate) struct HttpTransport {
    client: reqwest::Client,
    url: String,
    /// Caller-supplied extras (Authorization, custom headers), sent on every
    /// request. Content-Type/Accept/session/version are added per request.
    base_headers: HeaderMap,
    next_id: AtomicU64,
    /// Server-assigned session id (from the `initialize` response header),
    /// echoed on every subsequent request. `None` until assigned / after a
    /// session expires.
    session_id: Mutex<Option<String>>,
    /// Version negotiated in the `initialize` result body, echoed via
    /// `MCP-Protocol-Version` on later requests (spec 2025-06-18).
    protocol_version: Mutex<Option<String>>,
    /// The last `initialize` params, replayed to recover an expired session.
    init_params: Mutex<Option<Value>>,
    /// Serializes session recovery so a burst of concurrent 404s re-handshakes
    /// once, not once per caller.
    reinit_lock: tokio::sync::Mutex<()>,
    retry_delays: Vec<Duration>,
}

impl HttpTransport {
    pub(crate) fn new(url: String, headers: BTreeMap<String, String>) -> Result<Self> {
        Self::with_retry(url, headers, DEFAULT_RETRY_DELAYS.to_vec())
    }

    fn with_retry(
        url: String,
        headers: BTreeMap<String, String>,
        retry_delays: Vec<Duration>,
    ) -> Result<Self> {
        let mut base_headers = HeaderMap::new();
        for (name, value) in headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .with_context(|| format!("invalid mcp http header name '{name}'"))?;
            let value = HeaderValue::from_str(&value)
                .with_context(|| format!("invalid value for mcp http header '{name}'"))?;
            base_headers.insert(name, value);
        }
        Ok(HttpTransport {
            client: reqwest::Client::new(),
            url,
            base_headers,
            next_id: AtomicU64::new(1),
            session_id: Mutex::new(None),
            protocol_version: Mutex::new(None),
            init_params: Mutex::new(None),
            reinit_lock: tokio::sync::Mutex::new(()),
            retry_delays,
        })
    }

    /// POST one JSON-RPC message. `expect_id` is the request id whose response
    /// we await (`None` for a notification — the server 202s with no body). On
    /// a session-expired 404 the caller re-handshakes; other retryable failures
    /// back off per the schedule; the rest are terminal.
    async fn post_with_retry(
        &self,
        body: &Value,
        expect_id: Option<u64>,
        timeout: Duration,
    ) -> std::result::Result<Option<Value>, PostError> {
        let mut attempt = 0;
        loop {
            match self.post_once(body, expect_id, timeout).await {
                Ok(v) => return Ok(v),
                Err(Attempt::SessionExpired) => return Err(PostError::SessionExpired),
                Err(Attempt::Fatal(e)) => return Err(PostError::Other(e)),
                Err(Attempt::Retryable(e)) => {
                    let Some(delay) = self.retry_delays.get(attempt).copied() else {
                        return Err(PostError::Other(e));
                    };
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    async fn post_once(
        &self,
        body: &Value,
        expect_id: Option<u64>,
        timeout: Duration,
    ) -> std::result::Result<Option<Value>, Attempt> {
        match tokio::time::timeout(timeout, self.post_inner(body, expect_id)).await {
            Ok(r) => r,
            Err(_) => Err(Attempt::Fatal(anyhow!(
                "mcp http: no response within {timeout:?}"
            ))),
        }
    }

    async fn post_inner(
        &self,
        body: &Value,
        expect_id: Option<u64>,
    ) -> std::result::Result<Option<Value>, Attempt> {
        let mut req = self
            .client
            .post(&self.url)
            .headers(self.base_headers.clone())
            .header(CONTENT_TYPE, "application/json")
            .header(ACCEPT, "application/json, text/event-stream")
            .json(body);
        let had_session = {
            let session = self.session_id.lock().unwrap();
            if let Some(sid) = session.as_deref() {
                req = req.header("Mcp-Session-Id", sid);
                true
            } else {
                false
            }
        };
        if let Some(ver) = self.protocol_version.lock().unwrap().clone() {
            req = req.header("MCP-Protocol-Version", ver);
        }

        let resp = req
            .send()
            .await
            .map_err(|e| Attempt::Retryable(anyhow!("mcp http request failed: {e}")))?;

        if let Some(sid) = resp
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
        }

        let status = resp.status();
        if !status.is_success() {
            let code = status.as_u16();
            if code == 404 && had_session {
                return Err(Attempt::SessionExpired);
            }
            let text = resp.text().await.unwrap_or_default();
            let msg = anyhow!("mcp http {status}: {text}");
            // 401/403 and other 4xx are terminal (retrying won't help); the
            // server-error and transient-load codes retry.
            if code == 408 || code == 429 || (500..600).contains(&code) {
                return Err(Attempt::Retryable(msg));
            }
            return Err(Attempt::Fatal(msg));
        }

        // A notification (no id awaited): a 202 with no body is the success.
        let Some(want) = expect_id else {
            return Ok(None);
        };

        let ctype = resp
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let message = if ctype.contains("text/event-stream") {
            read_sse_message(resp, want).await?
        } else {
            let bytes = resp
                .bytes()
                .await
                .map_err(|e| Attempt::Fatal(anyhow!("mcp http: reading body failed: {e}")))?;
            if bytes.is_empty() {
                return Err(Attempt::Fatal(anyhow!("mcp http: empty response body")));
            }
            let value: Value = serde_json::from_slice(&bytes)
                .map_err(|e| Attempt::Fatal(anyhow!("mcp http: invalid JSON response: {e}")))?;
            pick_message(&value, want).ok_or_else(|| {
                Attempt::Fatal(anyhow!("mcp http: no response for request {want}"))
            })?
        };
        self.finish(message).map(Some)
    }

    /// Extract the result from a JSON-RPC response, mirroring the stdio path: a
    /// JSON-RPC `error` object surfaces as a terminal error; the negotiated
    /// protocol version (present only on the `initialize` result) is captured.
    fn finish(&self, message: Value) -> std::result::Result<Value, Attempt> {
        if let Some(err) = message.get("error") {
            return Err(Attempt::Fatal(anyhow!(
                "mcp error {}: {}",
                err["code"].as_i64().unwrap_or(0),
                err["message"].as_str().unwrap_or("unknown")
            )));
        }
        let result = message.get("result").cloned().unwrap_or(Value::Null);
        if let Some(ver) = result.get("protocolVersion").and_then(|v| v.as_str()) {
            *self.protocol_version.lock().unwrap() = Some(ver.to_string());
        }
        Ok(result)
    }

    /// Replay the stored `initialize` handshake to recover an expired session.
    /// Serialized so a burst of 404s re-handshakes once: if another caller
    /// already refreshed the session (it differs from the one that failed),
    /// this is a no-op.
    async fn reinitialize(&self, failed_session: &Option<String>) -> Result<()> {
        let _guard = self.reinit_lock.lock().await;
        if *self.session_id.lock().unwrap() != *failed_session {
            return Ok(());
        }
        let params = self
            .init_params
            .lock()
            .unwrap()
            .clone()
            .context("mcp session expired but no initialize params to replay")?;
        *self.session_id.lock().unwrap() = None;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let init = json!({"jsonrpc": "2.0", "id": id, "method": "initialize", "params": params});
        self.post_with_retry(&init, Some(id), crate::HANDSHAKE_TIMEOUT)
            .await
            .map_err(PostError::into_anyhow)
            .context("mcp re-initialize failed")?;
        let notif = json!({"jsonrpc": "2.0", "method": "notifications/initialized"});
        self.post_with_retry(&notif, None, NOTIFY_TIMEOUT)
            .await
            .map_err(PostError::into_anyhow)?;
        Ok(())
    }
}

impl Transport for HttpTransport {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Result<Value>> + Send + 'a>> {
        Box::pin(async move {
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            if method == "initialize" {
                *self.init_params.lock().unwrap() = Some(params.clone());
            }
            let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
            let session_before = self.session_id.lock().unwrap().clone();
            let result = match self.post_with_retry(&body, Some(id), timeout).await {
                Ok(v) => v,
                Err(PostError::SessionExpired) => {
                    self.reinitialize(&session_before).await?;
                    self.post_with_retry(&body, Some(id), timeout)
                        .await
                        .map_err(PostError::into_anyhow)?
                }
                Err(other) => return Err(other.into_anyhow()),
            };
            result.context("mcp http: request produced no response")
        })
    }

    fn notify<'a>(
        &'a self,
        method: &'a str,
        params: Option<Value>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            let body = match params {
                Some(params) => json!({"jsonrpc": "2.0", "method": method, "params": params}),
                None => json!({"jsonrpc": "2.0", "method": method}),
            };
            self.post_with_retry(&body, None, NOTIFY_TIMEOUT)
                .await
                .map_err(PostError::into_anyhow)?;
            Ok(())
        })
    }
}

/// Outcome of the full retry loop.
enum PostError {
    SessionExpired,
    Other(anyhow::Error),
}

impl PostError {
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            PostError::SessionExpired => anyhow!("mcp session expired"),
            PostError::Other(e) => e,
        }
    }
}

/// Outcome of one HTTP attempt.
enum Attempt {
    SessionExpired,
    Retryable(anyhow::Error),
    Fatal(anyhow::Error),
}

/// Read an SSE response stream until the JSON-RPC message answering `want`
/// arrives. Server→client requests/notifications interleaved before it are
/// ignored (we advertise no capabilities). The stream is consumed
/// incrementally so keep-alives don't force buffering the whole body.
async fn read_sse_message(
    resp: reqwest::Response,
    want: u64,
) -> std::result::Result<Value, Attempt> {
    let mut parser = SseParser::default();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk
            .map_err(|e| Attempt::Fatal(anyhow!("mcp http: reading event stream failed: {e}")))?;
        for data in parser.feed(&chunk) {
            let Ok(value) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if let Some(message) = pick_message(&value, want) {
                return Ok(message);
            }
        }
    }
    Err(Attempt::Fatal(anyhow!(
        "mcp http: event stream ended before responding to request {want}"
    )))
}

/// Pick the JSON-RPC message answering `want` from a single message or a batch
/// array. A response carries `result` or `error`; server-initiated requests
/// (which have a `method`) are skipped.
fn pick_message(value: &Value, want: u64) -> Option<Value> {
    let matches = |m: &Value| -> bool {
        m.get("method").is_none()
            && m["id"].as_u64() == Some(want)
            && (m.get("result").is_some() || m.get("error").is_some())
    };
    match value {
        Value::Array(items) => items.iter().find(|m| matches(m)).cloned(),
        m if matches(m) => Some(m.clone()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::McpClient;
    use serde_json::json;
    use wiremock::matchers::body_partial_json;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Request;
    use wiremock::Respond;
    use wiremock::ResponseTemplate;

    fn client(url: String, headers: BTreeMap<String, String>) -> McpClient {
        // Zero backoff keeps the retry tests fast.
        let transport =
            HttpTransport::with_retry(url, headers, vec![Duration::ZERO, Duration::ZERO]).unwrap();
        McpClient::from_transport(Box::new(transport))
    }

    fn init_result() -> Value {
        json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "serverInfo": {"name": "mock", "version": "0"},
        })
    }

    /// Wraps `result` in a JSON-RPC response that echoes the POST's own id —
    /// real servers correlate by id, and a fixed id would break the client's
    /// response routing across incrementing request ids. Optionally sets the
    /// `Mcp-Session-Id` response header.
    struct RpcResult {
        result: Value,
        session: Option<&'static str>,
    }

    impl RpcResult {
        fn new(result: Value) -> Self {
            RpcResult {
                result,
                session: None,
            }
        }
        fn with_session(result: Value, session: &'static str) -> Self {
            RpcResult {
                result,
                session: Some(session),
            }
        }
    }

    impl Respond for RpcResult {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body: Value = serde_json::from_slice(&request.body).unwrap_or(json!({}));
            let id = body.get("id").cloned().unwrap_or(json!(1));
            let mut t =
                ResponseTemplate::new(200).insert_header("content-type", "application/json");
            if let Some(s) = self.session {
                t = t.insert_header("mcp-session-id", s);
            }
            t.set_body_json(json!({"jsonrpc": "2.0", "id": id, "result": self.result.clone()}))
        }
    }

    /// Full handshake + paginated tools/list over HTTP, asserting the session
    /// id (from the initialize response header) and the negotiated protocol
    /// version are echoed on the follow-up requests.
    #[tokio::test]
    async fn handshake_and_paginated_list_echo_session_and_version() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(RpcResult::with_session(init_result(), "sess-1"))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(
                json!({"method": "notifications/initialized"}),
            ))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
        // Page 2 (carries the cursor) — must also carry the session + version.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"params": {"cursor": "page-2"}})))
            .and(header("mcp-session-id", "sess-1"))
            .and(header("mcp-protocol-version", "2025-06-18"))
            .respond_with(RpcResult::new(json!({"tools": [
                {"name": "echo", "description": "d", "inputSchema": {"type": "object"}}
            ]})))
            .with_priority(1)
            .mount(&server)
            .await;
        // Page 1 (no cursor) — session echoed, no cursor yet.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/list"})))
            .and(header("mcp-session-id", "sess-1"))
            .respond_with(RpcResult::new(
                json!({"tools": [{"name": "bare"}], "nextCursor": "page-2"}),
            ))
            .with_priority(2)
            .mount(&server)
            .await;

        let client = client(server.uri(), BTreeMap::new());
        client.initialize().await.unwrap();
        let tools = client.list_tools().await.unwrap();
        assert_eq!(tools.len(), 2);
        assert_eq!(tools[0].name, "bare");
        assert_eq!(tools[1].name, "echo");
    }

    /// tools/call over an SSE response: the JSON-RPC response arrives as an
    /// `event: message` frame after a keep-alive comment, and an image content
    /// block renders through to the flattened text. (No prior initialize, so
    /// this call's id is 1 — matching the static frame.)
    #[tokio::test]
    async fn call_tool_over_sse_response() {
        let server = MockServer::start().await;
        let sse = "\
: keep-alive\n\n\
event: message\n\
data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"content\":[{\"type\":\"text\",\"text\":\"hi\"},{\"type\":\"image\",\"data\":\"aGk=\",\"mimeType\":\"image/png\"}]}}\n\n";
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(sse.as_bytes().to_vec(), "text/event-stream"),
            )
            .mount(&server)
            .await;

        let client = client(server.uri(), BTreeMap::new());
        let structured = client
            .call_tool_structured("echo", &json!({"x": 1}))
            .await
            .unwrap();
        assert_eq!(crate::render_result(&structured), "hi\n[image: image/png]");
        let blocks = crate::content_blocks(&structured["content"]).unwrap();
        assert!(matches!(
            blocks[1],
            kloop_protocol::ContentBlock::Image { .. }
        ));
    }

    /// A bearer/custom header supplied to the transport rides every request.
    #[tokio::test]
    async fn base_headers_are_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header("authorization", "Bearer secret-token"))
            .respond_with(RpcResult::new(json!({"tools": []})))
            .mount(&server)
            .await;

        let headers = BTreeMap::from([("Authorization".into(), "Bearer secret-token".into())]);
        let client = client(server.uri(), headers);
        // No matching mock without the header ⇒ this only succeeds if it was sent.
        assert_eq!(client.list_tools().await.unwrap().len(), 0);
    }

    /// 503 twice, then success — the backoff schedule retries (3 tries total).
    #[tokio::test]
    async fn retries_5xx_then_succeeds() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503))
            .up_to_n_times(2)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(RpcResult::new(json!({"tools": []})))
            .with_priority(2)
            .mount(&server)
            .await;

        let client = client(server.uri(), BTreeMap::new());
        assert!(client.list_tools().await.is_ok());
    }

    /// 401 is terminal — one request, no retry, surfaced as an error.
    #[tokio::test]
    async fn does_not_retry_401() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(401).set_body_string("unauthorized"))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(server.uri(), BTreeMap::new());
        let err = client.list_tools().await.unwrap_err();
        assert!(err.to_string().contains("401"), "got: {err}");
        server.verify().await;
    }

    /// A session-bearing request that 404s triggers exactly one re-handshake,
    /// after which the retried call carries the fresh session id.
    #[tokio::test]
    async fn session_expiry_reinitializes_once() {
        let server = MockServer::start().await;
        // First initialize → sess-1; second (recovery) → sess-2.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(RpcResult::with_session(init_result(), "sess-1"))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "initialize"})))
            .respond_with(RpcResult::with_session(init_result(), "sess-2"))
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
        // tools/call under the stale session → 404; under the fresh one → ok.
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .and(header("mcp-session-id", "sess-1"))
            .respond_with(ResponseTemplate::new(404).set_body_string("session expired"))
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(body_partial_json(json!({"method": "tools/call"})))
            .and(header("mcp-session-id", "sess-2"))
            .respond_with(RpcResult::new(
                json!({"content": [{"type": "text", "text": "recovered"}]}),
            ))
            .with_priority(2)
            .mount(&server)
            .await;

        let client = client(server.uri(), BTreeMap::new());
        client.initialize().await.unwrap();
        let out = client.call_tool("echo", &json!({})).await.unwrap();
        assert_eq!(out, "recovered");
    }
}
