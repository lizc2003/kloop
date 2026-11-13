//! The stdio transport: newline-delimited JSON (one object per line) over a
//! child process's byte streams. A background reader task owns the read half
//! and routes responses to pending requests by id; everything above the
//! [`Transport`] impl — handshake, pagination, tools/call — lives in `lib.rs`
//! and never sees these types.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use kloop_process_spawn::ProcessSpec;
use kloop_process_spawn::ProcessStdio;
use kloop_process_spawn::ProcessTreeChild;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::oneshot;

use crate::MAX_WIRE_MESSAGE_BYTES;
use crate::McpNotification;
use crate::McpRpcError;
use crate::McpTransportFailure;
use crate::McpTransportHealth;
use crate::McpTransportState;
use crate::Transport;

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value>>>>>;
type SharedWriter = Arc<tokio::sync::Mutex<Box<dyn AsyncWrite + Send + Unpin>>>;

/// `notifications/initialized` and other fire-and-forget writes reuse the
/// handshake budget; a pipe that never drains must not pin the write lock.
const NOTIFY_TIMEOUT: Duration = crate::HANDSHAKE_TIMEOUT;

/// A reader-task refusal write is best effort: it never queues behind a
/// request write and never holds up reading stdout.
const REFUSAL_WRITE_TIMEOUT: Duration = Duration::from_secs(5);

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
pub(crate) struct StdioTransport {
    writer: SharedWriter,
    pending: Pending,
    notifications: tokio::sync::broadcast::Sender<McpNotification>,
    health: tokio::sync::watch::Sender<McpTransportHealth>,
    next_id: AtomicU64,
    reader: Mutex<Option<tokio::task::JoinHandle<()>>>,
    child: Mutex<Option<ProcessTreeChild>>,
}

impl Drop for StdioTransport {
    fn drop(&mut self) {
        self.abort();
    }
}

impl StdioTransport {
    pub(crate) fn spawn(command: &[String], env: &BTreeMap<String, String>) -> Result<Self> {
        let (program, args) = command
            .split_first()
            .context("mcp server command must not be empty")?;
        // The shared process-tree spawner gives the server its own process
        // group / Job Object: shutting down an `npx`-style launcher must take
        // the grandchild server with it, not leave it as an orphan.
        let cwd = std::env::current_dir().context("mcp server working directory")?;
        let resolved = kloop_process_spawn::resolve_program(program);
        let mut spec = ProcessSpec::new(resolved, cwd);
        for arg in args {
            spec.arg(arg);
        }
        for (name, value) in env {
            spec.env(name, value);
        }
        spec.stdin = ProcessStdio::Pipe;
        spec.stdout = ProcessStdio::Pipe;
        spec.stderr = ProcessStdio::Null;
        let mut child = kloop_process_spawn::spawn_process_tree(spec)
            .with_context(|| format!("cannot spawn mcp server '{program}'"))?;
        let stdin = child.take_stdin().context("mcp child stdin unavailable")?;
        let stdout = child
            .take_stdout()
            .context("mcp child stdout unavailable")?;
        Ok(Self::over(stdout, stdin, Some(child)))
    }

    pub(crate) fn over(
        reader: impl AsyncRead + Send + Unpin + 'static,
        writer: impl AsyncWrite + Send + Unpin + 'static,
        child: Option<ProcessTreeChild>,
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
            // One deadline covers queueing behind another write, the write
            // itself, and the response: a child that stops draining stdin
            // must not park this request (and the write lock) forever.
            let deadline = tokio::time::Instant::now() + timeout;
            if let Err(error) = write_line_before(&self.writer, &msg, deadline).await {
                self.close_local(McpTransportFailure::WriteFailed);
                return Err(error);
            }
            match tokio::time::timeout_at(deadline, rx).await {
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
            let deadline = tokio::time::Instant::now() + NOTIFY_TIMEOUT;
            if let Err(error) = write_line_before(&self.writer, &msg, deadline).await {
                self.close_local(McpTransportFailure::WriteFailed);
                return Err(error);
            }
            self.publish_health(McpTransportState::Healthy);
            Ok(())
        })
    }

    fn abort(&self) {
        self.close_local(McpTransportFailure::ExplicitShutdown);
        if let Some(child) = self.child.lock().unwrap().as_ref() {
            let _ = child.killer().terminate();
        }
    }

    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async move {
            self.close_local(McpTransportFailure::ExplicitShutdown);
            let child = self.child.lock().unwrap().take();
            if let Some(mut child) = child {
                let _ = child.terminate_and_wait(Duration::from_secs(5)).await;
            }
        })
    }
}

async fn write_line_before(
    writer: &SharedWriter,
    msg: &Value,
    deadline: tokio::time::Instant,
) -> Result<()> {
    let mut line = msg.to_string();
    line.push('\n');
    let write = async {
        let mut writer = writer.lock().await;
        writer
            .write_all(line.as_bytes())
            .await
            .context("mcp write failed")?;
        writer.flush().await.context("mcp flush failed")
    };
    tokio::time::timeout_at(deadline, write)
        .await
        .map_err(|_| anyhow!("mcp write: server did not accept the request in time"))?
}

/// The reader task's refusal write. It must not wait for the write lock — a
/// request write blocked on a full pipe would otherwise stop us reading, and a
/// server waiting for its stdout to drain would then block the whole link.
/// Losing the refusal only costs a server an answer to a request this client
/// does not support.
async fn write_refusal(writer: &SharedWriter, msg: &Value) -> Option<Result<()>> {
    let mut line = msg.to_string();
    line.push('\n');
    let Ok(mut writer) = writer.try_lock() else {
        return None;
    };
    let write = async {
        writer
            .write_all(line.as_bytes())
            .await
            .context("mcp write failed")?;
        writer.flush().await.context("mcp flush failed")
    };
    Some(
        match tokio::time::timeout(REFUSAL_WRITE_TIMEOUT, write).await {
            Ok(result) => result,
            Err(_) => Ok(()),
        },
    )
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
                if let Some(Err(error)) = write_refusal(&writer, &refusal).await {
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

    /// A server that never reads stdin fills the pipe; the request must fail on
    /// its own deadline instead of parking forever and holding the write lock.
    #[tokio::test]
    async fn blocked_write_times_out_and_closes_the_transport() {
        use super::McpTransportFailure;
        use super::McpTransportState;
        use super::StdioTransport;
        use super::Transport;
        use serde_json::json;
        use std::time::Duration;
        use std::time::Instant;

        // The server half is split and kept, never read: the 64-byte pipe
        // buffer fills and the client's write parks inside the runtime.
        let (client_io, server_io) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(client_io);
        let (_server_reader, _server_writer) = tokio::io::split(server_io);
        let transport = StdioTransport::over(reader, writer, None);
        let mut health = transport.subscribe_health().unwrap();

        let started = Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(5),
            transport.request(
                "tools/call",
                json!({"blob": "x".repeat(64 * 1024)}),
                Duration::from_millis(100),
            ),
        )
        .await
        .expect("request must not park on a full pipe")
        .unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(2), "{error:#}");
        assert!(error.to_string().contains("mcp write"), "{error:#}");
        health.changed().await.unwrap();
        assert_eq!(
            health.borrow_and_update().state,
            McpTransportState::Closed(McpTransportFailure::WriteFailed)
        );

        // The lock is free and the transport is closed, so a later request
        // fails at once instead of queueing behind the dead write.
        let started = Instant::now();
        let error = tokio::time::timeout(
            Duration::from_secs(1),
            transport.request("tools/list", json!({}), Duration::from_millis(100)),
        )
        .await
        .expect("a closed transport must answer immediately")
        .unwrap_err();
        assert!(error.to_string().contains("closed"), "{error:#}");
        assert!(started.elapsed() < Duration::from_millis(500));
    }

    /// The refusal path from the reader task never waits for the write lock:
    /// while a request write holds it, the reader keeps reading instead of
    /// deadlocking against the server.
    #[tokio::test]
    async fn refusal_write_does_not_wait_for_the_write_lock() {
        use super::StdioTransport;
        use super::write_refusal;
        use serde_json::json;
        use std::time::Duration;
        use std::time::Instant;

        let (client_io, _server_io) = tokio::io::duplex(64);
        let (reader, writer) = tokio::io::split(client_io);
        let transport = StdioTransport::over(reader, writer, None);
        let _busy = transport.writer.try_lock().expect("writer is idle");

        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(1),
            write_refusal(
                &transport.writer,
                &json!({"jsonrpc": "2.0", "id": 7, "error": {"code": -32601}}),
            ),
        )
        .await
        .expect("refusal must not block on the lock");
        assert!(outcome.is_none());
        assert!(started.elapsed() < Duration::from_millis(500));
    }
}
