//! Multi-session server for kloop's native agent protocol (plan 39): standard
//! JSON-RPC 2.0 over stdio, `thread/*` and `turn/*` methods in, per-thread item
//! events out, approvals as server→client reverse requests answered by id. An
//! `initialize` handshake with version negotiation gates every other method.
//!
//! The wire vocabulary lives in [`wire`]; the core [`kloop_core::event::Event`] stream projects
//! onto it through [`wire::project_event`], shared with the headless `--json`
//! front-end so both speak one vocabulary. Every thread is its own tokio task
//! owning a History (same worker shape as the TUI) plus its own `Permissions`
//! via the per-thread Config the factory builds — session approval caches never
//! leak across threads. All output funnels through one writer task, one JSON
//! object per line.

mod dto;
mod events;
mod methods;
mod params;
mod thread_ui;
mod wire;
mod worker;

pub use dto::ConfigFactory;
pub use dto::ConfigReader;
pub use dto::ConfigSnapshot;
pub use dto::McpServerState;
pub use dto::McpServerStatus;
pub use dto::McpToolInfo;
pub use dto::McpTransportKind;
pub use dto::NoteFn;
pub use dto::SandboxConfigInfo;
pub use dto::ServerConfig;
pub use dto::ServerPaths;
pub use dto::SkillContext;
pub use dto::SkillInfo;
pub use dto::SkillScope;
pub use dto::SkillsReader;
pub use dto::SkillsSnapshot;
pub use dto::ThreadStartOptions;

pub use wire::PROTOCOL_VERSION;
pub use wire::RequestId;
pub use wire::project_event;
pub use wire::turn_completed_params;
pub use wire::turn_started_params;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;

use anyhow::Context as _;
use anyhow::Result;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufRead;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use kloop_core::inbox::Inbox;
use kloop_core::interaction::QuestionAnswer;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::permissions::Decision;
use kloop_core::provider_route::ProviderCatalog;
use kloop_core::session_store::SessionDirs;
use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::ContentBlock;

use events::ThreadProjection;
use params::MethodResult;
use params::SteerError;
use params::steer_refused_data;
use thread_ui::approval_decision;
use wire::Incoming;
use wire::Outgoing;

pub async fn serve_stdio(config: ServerConfig) -> Result<()> {
    serve(tokio::io::stdin(), tokio::io::stdout(), config).await
}

/// Run the server until the input stream closes. Generic over the byte
/// streams so contract tests can drive it over in-memory duplex pipes.
pub async fn serve<R, W>(input: R, output: W, config: ServerConfig) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let ServerConfig {
        factory,
        paths,
        provider_catalog,
        mcp_servers,
        config_reader,
        skills_reader,
    } = config;
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let writer = tokio::spawn(write_loop(output, out_rx));

    // async-io-ok: one canonicalize at startup, before the read loop or any
    // thread exists; there is no other work on this runtime to starve.
    let default_cwd =
        std::fs::canonicalize(std::env::current_dir().context("cannot determine server cwd")?)
            .context("cannot canonicalize server cwd")?;
    let default_dirs = paths
        .store
        .ensure(&default_cwd)
        .context("cannot create the session directory")?;
    let (builds_tx, mut builds_rx) = mpsc::unbounded_channel::<ThreadBuild>();
    let mut server = Server {
        out: out_tx,
        threads: HashMap::new(),
        builds: builds_tx,
        pending_threads: HashMap::new(),
        shutdown: CancellationToken::new(),
        pending: Arc::new(Mutex::new(HashMap::new())),
        reverse_request_seq: Arc::new(AtomicU64::new(1)),
        factory,
        paths,
        provider_catalog,
        mcp_servers,
        config_reader,
        skills_reader,
        default_cwd,
        default_dirs,
        initialized: false,
        client_questions: false,
    };
    let mut frames = FrameReader::new(BufReader::new(input));
    let mut read_error = None;
    loop {
        // A thread config build that finishes while no line is arriving still
        // has to make it back to the loop, so the read competes with it.
        let frame = tokio::select! {
            frame = frames.next_frame(MAX_INPUT_LINE_BYTES) => frame,
            Some(build) = builds_rx.recv() => {
                server.finish_thread_build(build);
                continue;
            }
        };
        let line = match frame {
            Ok(Some(line)) => line,
            Ok(None) => break,
            // A line over the wire limit is a client error, not a reason to
            // tear down every thread. The reader has already discarded the
            // rest of the line, so the stream stays framed.
            Err(error) if error.kind() == std::io::ErrorKind::InvalidData => {
                server.send(Outgoing::Error {
                    id: None,
                    code: wire::PARSE_ERROR,
                    message: error.to_string(),
                    data: None,
                });
                continue;
            }
            Err(error) => {
                read_error = Some(error);
                break;
            }
        };
        let Ok(line) = std::str::from_utf8(&line) else {
            server.send(Outgoing::Error {
                id: None,
                code: wire::PARSE_ERROR,
                message: "unparseable line: input is not UTF-8".into(),
                data: None,
            });
            continue;
        };
        if line.trim().is_empty() {
            continue;
        }
        server.handle_line(line);
    }
    // Input closed or failed. No worker may start another turn from here on:
    // a scheduler delivery that becomes due during the drain would otherwise
    // run for a client that is gone. Cancel the running turns too.
    server.shutdown.cancel();
    // A config build that already started still owes its request an answer: a
    // client may close the pipe right after sending `thread/start`, and a
    // missing response is indistinguishable from a rejected one. Wait for what
    // was accepted, bounded so a slow build cannot hold the server.
    let pending = server.pending_threads.len();
    let drained = tokio::time::timeout(SHUTDOWN_REQUEST_GRACE, async {
        for _ in 0..pending {
            let Some(build) = builds_rx.recv().await else {
                break;
            };
            server.finish_thread_build(build);
        }
    })
    .await
    .is_ok();
    // Whatever is still building cannot be registered now — its handle would
    // have no read loop and would keep the writer alive — so answer it with an
    // error instead of silence.
    for (thread_id, pending) in std::mem::take(&mut server.pending_threads) {
        drop(pending.fork_guard);
        server.send(Outgoing::Error {
            id: Some(pending.request_id),
            code: wire::SERVER_ERROR,
            message: format!(
                "thread '{thread_id}' did not finish starting before the server stopped reading"
            ),
            data: None,
        });
    }
    // A build that lands after this finds no receiver; its handle is dropped
    // with the worker's turn sender.
    drop(builds_rx);
    server.pending.lock().unwrap().clear();
    drop(server);

    // When every accepted build landed, waiting for the writer to see its
    // senders drop is the normal teardown (workers flush their last notes).
    // A build still inside its factory is the exception: it holds a clone of
    // the output sender, so waiting would let it hold shutdown. Give the queue
    // a bounded flush, then stop waiting for it.
    let mut writer = writer;
    if drained {
        writer.await.context("writer task panicked")??;
    } else {
        match tokio::time::timeout(SHUTDOWN_WRITER_GRACE, &mut writer).await {
            Ok(result) => result.context("writer task panicked")??,
            Err(_) => writer.abort(),
        }
    }
    match read_error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

/// How long shutdown lets accepted builds and reads finish and be answered.
const SHUTDOWN_REQUEST_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// How long the writer gets to flush once the read loop and server state are
/// gone. A build still in its factory may outlive this; it must not hold the
/// process through the output channel.
const SHUTDOWN_WRITER_GRACE: std::time::Duration = std::time::Duration::from_secs(2);

/// The largest accepted inbound line. Matching the MCP wire cap keeps one
/// client message from growing server memory without bound; a longer line is
/// drained to its newline and answered with `PARSE_ERROR`, not deserialized.
const MAX_INPUT_LINE_BYTES: usize = 8 * 1024 * 1024;

/// Read newline-delimited frames without ever holding more than `limit` bytes.
/// On overflow the rest of the line is consumed so the next frame is still read
/// from a line boundary.
///
/// The partial frame lives on `self`, not in the future, which is what makes
/// [`Self::next_frame`] cancel-safe: the read loop `select!`s between the next
/// frame and a deferred thread build, and dropping a `next_frame` future must
/// not lose bytes already consumed from the buffered reader.
struct FrameReader<R> {
    reader: R,
    line: Vec<u8>,
    oversized: bool,
}

impl<R: AsyncBufRead + Unpin> FrameReader<R> {
    fn new(reader: R) -> Self {
        Self {
            reader,
            line: Vec::new(),
            oversized: false,
        }
    }

    async fn next_frame(&mut self, limit: usize) -> std::io::Result<Option<Vec<u8>>> {
        loop {
            let (consumed, ended) = {
                let buffer = self.reader.fill_buf().await?;
                if buffer.is_empty() {
                    if self.oversized {
                        self.oversized = false;
                        return Err(oversized_frame(limit));
                    }
                    return if self.line.is_empty() {
                        Ok(None)
                    } else {
                        Ok(Some(trim_carriage_return(std::mem::take(&mut self.line))))
                    };
                }
                let newline = buffer.iter().position(|byte| *byte == b'\n');
                let take = newline.unwrap_or(buffer.len());
                if !self.oversized {
                    if self.line.len().saturating_add(take) > limit {
                        self.oversized = true;
                        // Nothing of an oversized frame is worth keeping; the
                        // line is drained and only the refusal goes out.
                        self.line.clear();
                    } else {
                        self.line.extend_from_slice(&buffer[..take]);
                    }
                }
                (take + usize::from(newline.is_some()), newline.is_some())
            };
            self.reader.consume(consumed);
            if ended {
                if self.oversized {
                    self.oversized = false;
                    return Err(oversized_frame(limit));
                }
                return Ok(Some(trim_carriage_return(std::mem::take(&mut self.line))));
            }
        }
    }
}

fn oversized_frame(limit: usize) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("input line exceeds the {limit}-byte wire limit"),
    )
}

fn trim_carriage_return(mut line: Vec<u8>) -> Vec<u8> {
    if line.last() == Some(&b'\r') {
        line.pop();
    }
    line
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut output: W,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
) -> Result<()> {
    while let Some(mut line) = rx.recv().await {
        line.push(b'\n');
        output.write_all(&line).await?;
        output.flush().await?;
    }
    Ok(())
}

struct Turn {
    /// The turn's user text (a slash command line runs the command instead of
    /// sampling; images force the model path).
    text: String,
    /// Trailing content blocks from a structured `input` (images).
    images: Vec<ContentBlock>,
    /// The turn's id, allocated by `turn/start` and echoed in `turn/started`,
    /// `turn/completed`, and every item event of the turn.
    id: u64,
    cancel: CancellationToken,
}

enum ThreadWorkerMsg {
    Turn(Turn),
    /// An idle delivery turn for the inbox: no client text, one turn bracket,
    /// and the queued items are what it answers.
    Wake {
        id: u64,
        cancel: CancellationToken,
    },
    /// `/provider ...` on the idle-transaction path: no turn bracket and no
    /// turn in the event projection, just the command's bounded result.
    ProviderCommand {
        text: String,
    },
    SwitchProvider {
        provider_id: String,
        model: Option<String>,
        expected_revision: u64,
        request_id: RequestId,
    },
}

struct ThreadWorkerState {
    running: Arc<AtomicBool>,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
    turn_seq: Arc<AtomicU64>,
    inbox: Arc<Inbox>,
    active_route: Arc<Mutex<ActiveProviderRoute>>,
    shutdown: CancellationToken,
}

struct ThreadHandle {
    turn_tx: mpsc::UnboundedSender<ThreadWorkerMsg>,
    running: Arc<AtomicBool>,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// The thread's step-boundary injection queue (a clone of `Config.inbox`,
    /// which the worker's turns drain). `turn/steer` pushes here; the text is
    /// delivered as a user message at the next round boundary of the running
    /// turn, or, when idle, by a delivery turn the worker starts for it. The
    /// server opens its steer window for each turn it runs.
    inbox: Arc<Inbox>,
    /// Allocates monotonic turn ids for this thread (from 1).
    turn_seq: Arc<AtomicU64>,
    /// The currently-running turn's id (shared with the thread's `ThreadUi` so
    /// item events can tag it); `None` between turns.
    turn: Arc<Mutex<Option<u64>>>,
    /// Canonical base cwd and pinned model for read-only config/skills queries.
    /// These are the persisted thread runtime, not a transient active worktree.
    cwd: PathBuf,
    active_route: Arc<Mutex<ActiveProviderRoute>>,
    provider_state: Arc<kloop_core::provider_route::SessionProviderState>,
    /// Generation-scoped public display projection, shared with ThreadUi and
    /// thread/events/sync.
    projection: Arc<ThreadProjection>,
}

/// A thread whose config build finished on the blocking pool (plan 265). The
/// read loop registers it and answers the request that asked for it; a build
/// must not hold up approvals or `turn/interrupt` on other threads.
struct ThreadBuild {
    thread_id: String,
    /// Kept for the resumed/forked response, which names the thread's runtime.
    options: ThreadStartOptions,
    reply: ThreadReply,
    outcome: std::result::Result<ThreadHandle, (i64, String)>,
}

struct PendingThread {
    request_id: RequestId,
    /// The read loop can remove an unregistered fork even if its factory is stuck.
    fork_guard: Option<kloop_core::cleanup::RemoveFileOnDrop>,
}

/// The success response shape the method would have produced synchronously.
enum ThreadReply {
    Started,
    /// `thread/resume` and `thread/fork`: a live thread plus its message count.
    Live {
        message_count: usize,
    },
}

enum PendingInteraction {
    Approval(oneshot::Sender<Decision>),
    Question {
        question_index: usize,
        reply: oneshot::Sender<QuestionOutcome>,
    },
}

type PendingInteractions = Arc<Mutex<HashMap<RequestId, PendingInteraction>>>;

struct PendingGuard {
    id: RequestId,
    pending: PendingInteractions,
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        self.pending.lock().unwrap().remove(&self.id);
    }
}

struct Server {
    out: mpsc::UnboundedSender<Vec<u8>>,
    threads: HashMap<String, ThreadHandle>,
    /// Where a thread whose config is being built on the blocking pool comes
    /// back to the read loop for registration and its response (plan 265).
    builds: mpsc::UnboundedSender<ThreadBuild>,
    /// Thread ids with a build in flight, with the request each will answer.
    /// `threads` cannot answer "already active" for them yet, so a pipelined
    /// resume would otherwise build twice.
    pending_threads: HashMap<String, PendingThread>,
    /// Set when input ends: workers stop taking new turns, and approvals or
    /// questions still waiting for a client that can no longer answer stop.
    shutdown: CancellationToken,
    pending: PendingInteractions,
    reverse_request_seq: Arc<AtomicU64>,
    factory: ConfigFactory,
    paths: ServerPaths,
    /// The immutable provider control-plane authority shared with every Config.
    provider_catalog: Arc<ProviderCatalog>,
    mcp_servers: Vec<McpServerStatus>,
    /// Safe process-config projection and fresh cwd-scoped skill discovery.
    /// Both callbacks return allowlist DTOs only.
    config_reader: ConfigReader,
    skills_reader: SkillsReader,
    /// Canonical process cwd used when a read method or thread/start omits cwd.
    default_cwd: PathBuf,
    /// `default_cwd`'s partition, resolved once: locating a thread by id alone
    /// starts here before falling back to a scan.
    default_dirs: SessionDirs,
    /// Set once the client's `initialize` handshake succeeds. Every other
    /// method is rejected until then, so a version mismatch surfaces
    /// immediately instead of as mysterious downstream failures.
    initialized: bool,
    /// Whether the initialized client opted into `question/request` reverse RPCs.
    client_questions: bool,
}

impl Server {
    fn send(&self, outgoing: Outgoing) {
        let _ = self.out.send(outgoing.into_bytes());
    }

    /// Run a read-only method on the blocking pool and deliver its response
    /// through the same writer. JSON-RPC matches responses by id, so these may
    /// complete out of order relative to later requests.
    fn spawn_read<F>(&self, id: RequestId, work: F)
    where
        F: FnOnce() -> MethodResult + Send + 'static,
    {
        let out = self.out.clone();
        let shutdown = self.shutdown.clone();
        let mut task = tokio::task::spawn_blocking(work);
        tokio::spawn(async move {
            let result = tokio::select! {
                result = &mut task => Some(result),
                _ = shutdown.cancelled() => {
                    tokio::time::timeout(SHUTDOWN_REQUEST_GRACE, &mut task).await.ok()
                }
            };
            let result = match result {
                Some(Ok(result)) => result,
                Some(Err(error)) => Err((wire::SERVER_ERROR, format!("read failed: {error}"))),
                None => {
                    task.abort();
                    Err((
                        wire::SERVER_ERROR,
                        "read did not finish before shutdown".into(),
                    ))
                }
            };
            let outgoing = match result {
                Ok(result) => Outgoing::Response { id, result },
                Err((code, message)) => Outgoing::Error {
                    id: Some(id),
                    code,
                    message,
                    data: None,
                },
            };
            let _ = out.send(outgoing.into_bytes());
        });
    }

    /// Register a thread whose config build just finished (plan 265) and answer
    /// the request that asked for it. Registration precedes the response, so a
    /// client that turns on the id it just received always finds the thread.
    fn finish_thread_build(&mut self, build: ThreadBuild) {
        let Some(PendingThread {
            request_id,
            fork_guard,
        }) = self.pending_threads.remove(&build.thread_id)
        else {
            return;
        };
        match build.outcome {
            Ok(handle) => {
                let route = handle.active_route.lock().unwrap().clone();
                let result = match build.reply {
                    ThreadReply::Started => {
                        json!({"thread": {"id": build.thread_id, "route": route}})
                    }
                    ThreadReply::Live { message_count } => json!({
                        "thread": crate::params::thread_runtime_json(
                            &build.thread_id,
                            &build.options,
                            &route,
                        ),
                        "message_count": message_count,
                    }),
                };
                self.threads.insert(build.thread_id, handle);
                if let Some(mut guard) = fork_guard {
                    guard.disarm();
                }
                self.send(Outgoing::Response {
                    id: request_id,
                    result,
                });
            }
            Err((code, message)) => {
                drop(fork_guard);
                self.send(Outgoing::Error {
                    id: Some(request_id),
                    code,
                    message,
                    data: None,
                });
            }
        }
    }

    fn handle_line(&mut self, line: &str) {
        let incoming: Incoming = match serde_json::from_str(line) {
            Ok(incoming) => incoming,
            Err(e) => {
                return self.send(Outgoing::Error {
                    id: None,
                    code: wire::PARSE_ERROR,
                    message: format!("unparseable line: {e}"),
                    data: None,
                });
            }
        };
        match (incoming.method, incoming.id) {
            (Some(method), Some(id)) => self.handle_request(id, &method, incoming.params),
            // A method without an id has nobody to answer; ignore it.
            (Some(_), None) => {}
            // No method: the client answering one of our approval requests.
            (None, Some(id)) => self.handle_interaction_response(id, incoming.result),
            (None, None) => self.send(Outgoing::Error {
                id: None,
                code: wire::PARSE_ERROR,
                message: "line is neither a request nor a response".into(),
                data: None,
            }),
        }
    }

    fn handle_request(&mut self, id: RequestId, method: &str, params: Value) {
        // The handshake gates everything else: a client that skips `initialize`
        // (or asks for a version we don't speak) is told so at once.
        if method != "initialize" && !self.initialized {
            return self.send(Outgoing::Error {
                id: Some(id),
                code: wire::SERVER_ERROR,
                message: "not initialized; send `initialize` first".into(),
                data: None,
            });
        }
        if method == "turn/steer" {
            let outgoing = match self.turn_steer(&params) {
                Ok(result) => Outgoing::Response { id, result },
                Err(SteerError::Request((code, message))) => Outgoing::Error {
                    id: Some(id),
                    code,
                    message,
                    data: None,
                },
                Err(SteerError::Refused(refused)) => Outgoing::Error {
                    id: Some(id),
                    code: wire::SERVER_ERROR,
                    message: refused.to_string(),
                    data: Some(steer_refused_data(refused)),
                },
            };
            return self.send(outgoing);
        }
        if method == "thread/provider/switch" {
            if let Err(error) = self.queue_thread_provider_switch(id.clone(), &params) {
                self.send(Outgoing::Error {
                    id: Some(id),
                    code: error.code(),
                    message: error.to_string(),
                    data: Some(json!({"kind": error.kind()})),
                });
            }
            return;
        }
        // These read whole transcripts; answer from the blocking pool so a
        // large list cannot hold up approvals or `turn/interrupt`.
        if method == "thread/list" {
            self.start_thread_list(id, &params);
            return;
        }
        if method == "thread/read" {
            self.start_thread_read(id, &params);
            return;
        }
        if method == "config/read" {
            self.start_config_read(id, &params);
            return;
        }
        if method == "skills/list" {
            self.start_skills_list(id, &params);
            return;
        }
        // Thread lifecycle builds a whole Config per thread; the build runs on
        // the blocking pool (plan 265), and its response is sent from here when
        // it lands. Nothing below the checks in these methods needs the read
        // loop, so approvals and `turn/interrupt` keep flowing meanwhile.
        if matches!(method, "thread/start" | "thread/resume" | "thread/fork") {
            let result = match method {
                "thread/start" => self.thread_start(id.clone(), &params),
                "thread/resume" => self.thread_resume(id.clone(), &params),
                _ => self.thread_fork(id.clone(), &params),
            };
            match result {
                Ok(Some(result)) => self.send(Outgoing::Response { id, result }),
                // The build answers later, like the read methods above.
                Ok(None) => {}
                Err((code, message)) => self.send(Outgoing::Error {
                    id: Some(id),
                    code,
                    message,
                    data: None,
                }),
            }
            return;
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "thread/events/sync" => self.thread_events_sync(&params),
            "provider/catalog/read" => self.provider_catalog_read(&params),
            "mcp_server_status/list" => self.mcp_server_status_list(&params),
            "turn/start" => self.turn_start(&params),
            "turn/interrupt" => self.turn_interrupt(&params),
            _ => Err((wire::METHOD_NOT_FOUND, format!("unknown method '{method}'"))),
        };
        match result {
            Ok(result) => self.send(Outgoing::Response { id, result }),
            Err((code, message)) => self.send(Outgoing::Error {
                id: Some(id),
                code,
                message,
                data: None,
            }),
        }
    }

    /// The `initialize` handshake: exchange info + capabilities and negotiate
    /// the protocol version. A version we don't speak is a hard error — no
    /// silent downgrade. Succeeding flips `initialized` so real work can begin.
    fn initialize(&mut self, params: &Value) -> MethodResult {
        let version = params["protocol_version"].as_str().unwrap_or("");
        if version != wire::PROTOCOL_VERSION {
            return Err((
                wire::INVALID_PARAMS,
                format!(
                    "unsupported protocol_version '{version}'; this engine speaks '{}'",
                    wire::PROTOCOL_VERSION
                ),
            ));
        }
        self.client_questions = params
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("questions"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.initialized = true;
        Ok(json!({
            "server_info": {"name": "kloop", "version": env!("CARGO_PKG_VERSION")},
            "protocol_version": wire::PROTOCOL_VERSION,
            "capabilities": {
                "streaming": true,
                "subagents": true,
                "mcp": true,
                "images": true,
                "approvals": {
                    "scopes": ["once", "workspace_session", "project"]
                },
                "questions": true,
                "providers": {"catalog": true, "switch": true},
                "config": {"read": true},
                "skills": {"list": true},
                "mcp_servers": {"status": true},
                "events": {
                    "sequence": true,
                    "sync": true,
                    "snapshot": true,
                },
                "threads": {
                    "list": true,
                    "read": true,
                    "resume": true,
                    "fork": true,
                },
            },
        }))
    }

    fn handle_interaction_response(&mut self, id: RequestId, result: Value) {
        let Some(pending) = self.pending.lock().unwrap().remove(&id) else {
            return self.send(Outgoing::Error {
                id: Some(id),
                code: wire::SERVER_ERROR,
                message: "no pending interaction with this id".into(),
                data: None,
            });
        };
        match pending {
            PendingInteraction::Approval(reply) => {
                let decision = approval_decision(&result);
                let _ = reply.send(decision);
            }
            PendingInteraction::Question {
                question_index,
                reply,
            } => {
                let outcome = match serde_json::from_value::<wire::QuestionResponse>(result) {
                    Ok(wire::QuestionResponse::Answered { selected, other }) => {
                        QuestionOutcome::Answered(vec![QuestionAnswer {
                            question_index,
                            selected,
                            other,
                        }])
                    }
                    Ok(wire::QuestionResponse::Cancelled) => QuestionOutcome::Cancelled,
                    Err(error) => QuestionOutcome::Unavailable(format!(
                        "malformed question response: {error}"
                    )),
                };
                let _ = reply.send(outcome);
            }
        }
    }
}
