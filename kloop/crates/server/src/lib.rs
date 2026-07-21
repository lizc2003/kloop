//! Multi-session server for kloop's native agent protocol (plan 39): standard
//! JSON-RPC 2.0 over stdio, `thread/*` and `turn/*` methods in, per-thread item
//! events out, approvals as server→client reverse requests answered by id. An
//! `initialize` handshake with version negotiation gates every other method.
//!
//! The wire vocabulary lives in [`wire`]; the core [`Event`] stream projects
//! onto it through [`wire::project_event`], shared with the headless `--json`
//! front-end so both speak one vocabulary. Every thread is its own tokio task
//! owning a History (same worker shape as the TUI) plus its own `Permissions`
//! via the per-thread Config the factory builds — session approval caches never
//! leak across threads. All output funnels through one writer task, one JSON
//! object per line.

mod wire;

pub use wire::project_event;
pub use wire::turn_completed_params;
pub use wire::turn_started_params;
pub use wire::RequestId;
pub use wire::PROTOCOL_VERSION;

use std::collections::HashMap;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::commands;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_core::inbox::Inbox;
use kloop_core::inbox::InboxItem;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::rollout;
use kloop_core::rollout::Rollout;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

use wire::Incoming;
use wire::Outgoing;

/// Out-of-band note sink handed to the config factory (rule persistence
/// messages etc.); the server routes these into `note` notifications.
pub type NoteFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Builds one Config per thread. Called with that thread's approver (routes
/// to `approval/request`) and note sink, so permission gates and their
/// session caches stay per-thread.
pub type ConfigFactory = Arc<dyn Fn(Arc<dyn Approver>, NoteFn) -> Result<Config> + Send + Sync>;

pub struct ServerPaths {
    pub sessions_dir: PathBuf,
    pub offload_dir: PathBuf,
}

pub async fn serve_stdio(factory: ConfigFactory, paths: ServerPaths) -> Result<()> {
    serve(tokio::io::stdin(), tokio::io::stdout(), factory, paths).await
}

/// Run the server until the input stream closes. Generic over the byte
/// streams so contract tests can drive it over in-memory duplex pipes.
pub async fn serve<R, W>(
    input: R,
    output: W,
    factory: ConfigFactory,
    paths: ServerPaths,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Send + Unpin + 'static,
{
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(write_loop(output, out_rx));

    let mut server = Server {
        out: out_tx,
        threads: HashMap::new(),
        pending: Arc::new(Mutex::new(HashMap::new())),
        srv_seq: Arc::new(AtomicU64::new(1)),
        factory,
        paths,
        initialized: false,
    };
    let mut lines = BufReader::new(input).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        server.handle_line(&line);
    }
    // Input closed: cancel in-flight turns so workers wind down, then drop
    // our sender so the writer drains and exits once every worker is gone.
    for handle in server.threads.values() {
        if let Some(cancel) = handle.current_cancel.lock().unwrap().take() {
            cancel.cancel();
        }
    }
    drop(server);
    writer.await.context("writer task panicked")?
}

async fn write_loop<W: AsyncWrite + Unpin>(
    mut output: W,
    mut rx: mpsc::UnboundedReceiver<Value>,
) -> Result<()> {
    while let Some(value) = rx.recv().await {
        let mut line = serde_json::to_vec(&value)?;
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

struct ThreadHandle {
    turn_tx: mpsc::UnboundedSender<Turn>,
    running: Arc<AtomicBool>,
    current_cancel: Arc<Mutex<Option<CancellationToken>>>,
    /// The thread's step-boundary injection queue (a clone of `Config.inbox`,
    /// which the worker's turns drain). `turn/steer` pushes here; the text is
    /// delivered as a user message at the next round boundary — during a
    /// running turn, or at the start of the next `turn/start` if idle.
    inbox: Arc<Inbox>,
    /// Allocates monotonic turn ids for this thread (from 1).
    turn_seq: Arc<AtomicU64>,
    /// The currently-running turn's id (shared with the thread's `ThreadUi` so
    /// item events can tag it); `None` between turns.
    turn: Arc<Mutex<Option<u64>>>,
}

type PendingApprovals = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Decision>>>>;

struct Server {
    out: mpsc::UnboundedSender<Value>,
    threads: HashMap<String, ThreadHandle>,
    pending: PendingApprovals,
    srv_seq: Arc<AtomicU64>,
    factory: ConfigFactory,
    paths: ServerPaths,
    /// Set once the client's `initialize` handshake succeeds. Every other
    /// method is rejected until then, so a version mismatch surfaces
    /// immediately instead of as mysterious downstream failures.
    initialized: bool,
}

impl Server {
    fn send(&self, outgoing: Outgoing) {
        let _ = self.out.send(outgoing.to_json());
    }

    fn handle_line(&mut self, line: &str) {
        let incoming: Incoming = match serde_json::from_str(line) {
            Ok(incoming) => incoming,
            Err(e) => {
                return self.send(Outgoing::Error {
                    id: None,
                    code: wire::PARSE_ERROR,
                    message: format!("unparseable line: {e}"),
                })
            }
        };
        match (incoming.method, incoming.id) {
            (Some(method), Some(id)) => self.handle_request(id, &method, incoming.params),
            // A method without an id has nobody to answer; ignore it.
            (Some(_), None) => {}
            // No method: the client answering one of our approval requests.
            (None, Some(id)) => self.handle_approval_response(id, incoming.result),
            (None, None) => self.send(Outgoing::Error {
                id: None,
                code: wire::PARSE_ERROR,
                message: "line is neither a request nor a response".into(),
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
            });
        }
        let result = match method {
            "initialize" => self.initialize(&params),
            "thread/start" => self.thread_start(),
            "thread/resume" => self.thread_resume(&params),
            "thread/fork" => self.thread_fork(&params),
            "thread/list" => self.thread_list(),
            "turn/start" => self.turn_start(&params),
            "turn/steer" => self.turn_steer(&params),
            "turn/interrupt" => self.turn_interrupt(&params),
            _ => Err((wire::METHOD_NOT_FOUND, format!("unknown method '{method}'"))),
        };
        match result {
            Ok(result) => self.send(Outgoing::Response { id, result }),
            Err((code, message)) => self.send(Outgoing::Error {
                id: Some(id),
                code,
                message,
            }),
        }
    }

    /// The `initialize` handshake: exchange info + capabilities and negotiate
    /// the protocol version. A version we don't speak is a hard error — no
    /// silent downgrade. Succeeding flips `initialized` so real work can begin.
    fn initialize(&mut self, params: &Value) -> MethodResult {
        let version = params["protocolVersion"].as_str().unwrap_or("");
        if version != wire::PROTOCOL_VERSION {
            return Err((
                wire::INVALID_PARAMS,
                format!(
                    "unsupported protocolVersion '{version}'; this engine speaks '{}'",
                    wire::PROTOCOL_VERSION
                ),
            ));
        }
        self.initialized = true;
        Ok(json!({
            "serverInfo": {"name": "kloop", "version": env!("CARGO_PKG_VERSION")},
            "protocolVersion": wire::PROTOCOL_VERSION,
            "capabilities": {
                "streaming": true,
                "subagents": true,
                "mcp": true,
                "images": true,
                "approvals": true,
            },
        }))
    }

    fn handle_approval_response(&mut self, id: RequestId, result: Value) {
        let Some(reply) = self.pending.lock().unwrap().remove(&id) else {
            return self.send(Outgoing::Error {
                id: Some(id),
                code: wire::SERVER_ERROR,
                message: "no pending approval with this id".into(),
            });
        };
        let decision = match result["decision"].as_str() {
            Some("accept") => Decision::Allow,
            Some("acceptForSession") => Decision::AllowSession,
            Some("acceptAlways") => Decision::AllowAlways,
            // "decline", "cancel", anything unrecognized, or missing: the safe
            // answer (a client that wants to stop the turn sends turn/interrupt).
            _ => Decision::Deny,
        };
        let _ = reply.send(decision);
    }

    fn thread_start(&mut self) -> MethodResult {
        let thread_id = rollout::new_session_id(&self.paths.sessions_dir);
        let path = rollout::session_path(&self.paths.sessions_dir, &thread_id);
        // Claim the id on disk right away: rollout files are otherwise created
        // lazily on first append, so two thread/starts within one second
        // would both be handed the same timestamp id.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        std::fs::File::create(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot create session: {e}")))?;
        let mut history = History::new(self.paths.offload_dir.clone());
        history.attach_rollout(Rollout::new(path));
        self.spawn_thread(thread_id.clone(), history)?;
        Ok(json!({"thread": {"id": thread_id}}))
    }

    fn thread_resume(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        if self.threads.contains_key(thread_id) {
            return Err((
                wire::SERVER_ERROR,
                format!("thread '{thread_id}' is already active"),
            ));
        }
        let path = rollout::session_path(&self.paths.sessions_dir, thread_id);
        if !path.exists() {
            return Err((wire::SERVER_ERROR, format!("no session '{thread_id}'")));
        }
        let (messages, rollout) = rollout::resume_session(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume: {e}")))?;
        let count = messages.len();
        let history = History::resume(self.paths.offload_dir.clone(), messages, rollout);
        self.spawn_thread(thread_id.to_string(), history)?;
        Ok(json!({"thread": {"id": thread_id}, "messageCount": count}))
    }

    /// Fork a session at a cut point into a fresh thread, then spawn it live
    /// (like `thread/resume`) so the client can `turn/start` on it immediately.
    /// `cut` is optional — omit it to fork at the end. The source need not be an
    /// active thread; forking reads the file directly, so a client can branch a
    /// dormant history. `fork_session` copies the prefix and records cross-file
    /// lineage; an illegal cut comes back as an error listing the legal points.
    fn thread_fork(&mut self, params: &Value) -> MethodResult {
        let src_id = str_param(params, "threadId")?;
        let cut = match params.get("cut") {
            None | Some(Value::Null) => None,
            Some(v) => Some(v.as_u64().ok_or((
                wire::INVALID_PARAMS,
                "'cut' must be a non-negative integer".to_string(),
            ))?),
        };
        let src = rollout::session_path(&self.paths.sessions_dir, src_id);
        if !src.exists() {
            return Err((wire::SERVER_ERROR, format!("no session '{src_id}'")));
        }
        let new_path = rollout::fork_session(&src, cut, &self.paths.sessions_dir)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot fork: {e}")))?;
        let new_id = rollout::session_id_of(&new_path);
        let (messages, rollout) = rollout::resume_session(&new_path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume fork: {e}")))?;
        let count = messages.len();
        let history = History::resume(self.paths.offload_dir.clone(), messages, rollout);
        self.spawn_thread(new_id.clone(), history)?;
        Ok(json!({"thread": {"id": new_id}, "messageCount": count}))
    }

    fn thread_list(&self) -> MethodResult {
        let threads: Vec<Value> = rollout::sessions_by_recency(&self.paths.sessions_dir)
            .iter()
            // Sub-agent transcripts share the sessions dir but are internal;
            // like cc's sidechains and codex's source filter, keep them out
            // of the thread list (still resumable by explicit id).
            .filter(|path| !rollout::is_subagent_session(path))
            .filter_map(|path| {
                let messages = rollout::load_session(path).ok()?;
                Some(json!({
                    "id": rollout::session_id_of(path),
                    "messages": messages.len(),
                    "snippet": rollout::first_user_snippet(&messages),
                }))
            })
            .collect();
        Ok(json!({"threads": threads}))
    }

    fn turn_start(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        let (text, images) = parse_input(params)?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            (
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            )
        })?;
        if handle.running.swap(true, Ordering::SeqCst) {
            return Err((
                wire::SERVER_ERROR,
                format!("a turn is already running on thread '{thread_id}'"),
            ));
        }
        // Allocate the turn id and publish it before the worker picks up the
        // turn, so a `turn/steer` racing in sees the running turn's id.
        let turn_id = handle.turn_seq.fetch_add(1, Ordering::SeqCst);
        *handle.turn.lock().unwrap() = Some(turn_id);
        let cancel = CancellationToken::new();
        *handle.current_cancel.lock().unwrap() = Some(cancel.clone());
        if handle
            .turn_tx
            .send(Turn {
                text,
                images,
                id: turn_id,
                cancel,
            })
            .is_err()
        {
            handle.running.store(false, Ordering::SeqCst);
            *handle.turn.lock().unwrap() = None;
            return Err((wire::SERVER_ERROR, "thread worker is gone".into()));
        }
        Ok(json!({"turn": {"id": turn_id}}))
    }

    /// Enqueue steering text typed while a turn runs (or between turns). Unlike
    /// `turn/start` this never starts a turn and never checks the running flag:
    /// the worker's turn loop drains the inbox at round boundaries, so a steer
    /// pushed during a running turn folds into it, and one pushed while idle is
    /// delivered at the top of the next `turn/start`. There is no autowake in
    /// client-driven server mode, so an idle steer waits for that next turn.
    /// Returns the running turn's id, or null when idle. (`expectedTurnId`, if
    /// the client sends it, is accepted and ignored for now.)
    fn turn_steer(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        let (text, _images) = parse_input(params)?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            (
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            )
        })?;
        handle.inbox.push(InboxItem::Steer(text));
        let turn_id = *handle.turn.lock().unwrap();
        Ok(json!({"turnId": turn_id}))
    }

    fn turn_interrupt(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            (
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            )
        })?;
        if let Some(cancel) = handle.current_cancel.lock().unwrap().as_ref() {
            cancel.cancel();
        }
        Ok(json!({}))
    }

    fn spawn_thread(&mut self, thread_id: String, history: History) -> Result<(), (i64, String)> {
        // The running turn's id, shared between the ThreadUi (which tags item
        // events with it) and the handle (which `turn/steer` reads).
        let turn = Arc::new(Mutex::new(None));
        let ui = Arc::new(ThreadUi {
            thread_id: thread_id.clone(),
            out: self.out.clone(),
            pending: self.pending.clone(),
            srv_seq: self.srv_seq.clone(),
            turn: turn.clone(),
        });
        let note_ui = ui.clone();
        let mut cfg = (self.factory)(
            ui.clone(),
            Arc::new(move |s: &str| note_ui.emit(&Event::Note(s.to_string()))),
        )
        .map_err(|e| (wire::SERVER_ERROR, format!("cannot build config: {e:#}")))?;
        // The factory cannot know which thread it is building for; the hook
        // events' session id is stamped here.
        cfg.session_id = thread_id.clone();
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(false));
        let cfg = Arc::new(cfg);
        // The steering queue the worker's turns drain; `turn/steer` pushes here.
        let inbox = cfg.inbox.clone();
        tokio::spawn(thread_worker(cfg, history, ui, turn_rx, running.clone()));
        self.threads.insert(
            thread_id,
            ThreadHandle {
                turn_tx,
                running,
                current_cancel: Arc::new(Mutex::new(None)),
                inbox,
                turn_seq: Arc::new(AtomicU64::new(1)),
                turn,
            },
        );
        Ok(())
    }
}

type MethodResult = Result<Value, (i64, String)>;

fn str_param<'a>(params: &'a Value, key: &str) -> Result<&'a str, (i64, String)> {
    params[key].as_str().ok_or_else(|| {
        (
            wire::INVALID_PARAMS,
            format!("missing string param '{key}'"),
        )
    })
}

/// Parse a `turn/start` (or `turn/steer`) `input` into its user text and any
/// trailing content blocks (images). The structured form is an array of
/// content parts — `{type:"text",text}` and `{type:"image",source:{…}}`, the
/// canonical block shapes — and text parts join with newlines. A bare string is
/// accepted too as a single text part, for a client that only sends text.
fn parse_input(params: &Value) -> Result<(String, Vec<ContentBlock>), (i64, String)> {
    let input = &params["input"];
    if let Some(s) = input.as_str() {
        return Ok((s.to_string(), Vec::new()));
    }
    let Some(parts) = input.as_array() else {
        return Err((
            wire::INVALID_PARAMS,
            "missing 'input' (a string or an array of content parts)".into(),
        ));
    };
    let mut text_parts = Vec::new();
    let mut images = Vec::new();
    for part in parts {
        let block: ContentBlock = serde_json::from_value(part.clone())
            .map_err(|e| (wire::INVALID_PARAMS, format!("bad input part: {e}")))?;
        match block {
            ContentBlock::Text { text } => text_parts.push(text),
            // Non-text blocks (images) ride along as trailing content.
            other => images.push(other),
        }
    }
    Ok((text_parts.join("\n"), images))
}

/// Owns this thread's History for its whole life; turns run strictly one at
/// a time (turn/start enforces single-flight via the running flag).
async fn thread_worker(
    cfg: Arc<Config>,
    mut history: History,
    ui: Arc<ThreadUi>,
    mut turns: mpsc::UnboundedReceiver<Turn>,
    running: Arc<AtomicBool>,
) {
    while let Some(turn) = turns.recv().await {
        // The turn id (allocated by turn/start) tags the bracket and every item
        // event of the turn; it is already published on the shared `turn` slot
        // for `turn/steer` to read.
        ui.notify("turn/started", wire::turn_started_params(turn.id));
        let reason = run_turn_or_command(&cfg, &mut history, &ui, &turn).await;
        // Report the post-turn context size, then close the bracket. Usage
        // routes through `emit` so it projects like any other event.
        ui.emit(&Event::Usage(history.estimated_tokens()));
        ui.notify(
            "turn/completed",
            wire::turn_completed_params(turn.id, &reason),
        );
        running.store(false, Ordering::SeqCst);
        *ui.turn.lock().unwrap() = None;
    }
    // The thread is ending (turn channel closed on server shutdown): tear down
    // its active worktree if the model never exited (dirty kept on its branch,
    // clean removed), so trees don't leak past the session.
    if let Some(note) = kloop_core::worktree::finish_active(&cfg).await {
        ui.emit(&Event::Note(note.trim().to_string()));
    }
}

/// Run one turn's work and return its end reason, without touching the turn
/// bracket (the caller owns that). A slash command line reads/rewrites History
/// like a turn but is not a model turn: no user message is recorded and nothing
/// is sampled — its output comes back as a `system` notification, and `/clear`
/// also emits `thread/cleared` so the client resets its transcript. Images (or
/// any non-command text) take the model path.
async fn run_turn_or_command(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<ThreadUi>,
    turn: &Turn,
) -> EndReason {
    if turn.images.is_empty() && commands::is_command(&turn.text) {
        let result = commands::run(&turn.text, history, cfg, &turn.cancel).await;
        // `/exit` is a client-side concept: quitting one thread must never stop
        // a multi-session server. Relay a note instead of acting on it.
        if result.quit {
            ui.notify(
                "system",
                json!({"text": "/exit is for interactive sessions; the server keeps running — close the connection to end this one"}),
            );
            return EndReason::Completed;
        }
        // Clear first, then show the output on the now-blank transcript —
        // matching the TUI order (crates/tui/src/lib.rs). Reversed, a client
        // that resets its transcript on `thread/cleared` would wipe the command
        // output it just received.
        if result.cleared {
            ui.notify("thread/cleared", json!({}));
        }
        if !result.output.is_empty() {
            ui.notify("system", json!({"text": result.output}));
        }
        // A skill invoked as `/name` expands to a prompt: record it and run a
        // turn (streaming via the same notifications as a normal turn).
        if let Some(prompt) = result.run_turn {
            history.record(Message::user_text(prompt));
            let dyn_ui: Arc<dyn Ui> = ui.clone();
            return run_turn(cfg, history, &dyn_ui, &turn.cancel, 0)
                .await
                .reason;
        }
        return EndReason::Completed;
    }
    let msg = if turn.images.is_empty() {
        Message::user_text(turn.text.clone())
    } else {
        Message::user_with_blocks(turn.text.clone(), turn.images.clone())
    };
    history.record(msg);
    let dyn_ui: Arc<dyn Ui> = ui.clone();
    run_turn(cfg, history, &dyn_ui, &turn.cancel, 0)
        .await
        .reason
}

/// Per-thread `Ui` + `Approver`: events become thread-tagged notifications,
/// confirmations become `approval/request` server requests answered by id.
struct ThreadUi {
    thread_id: String,
    out: mpsc::UnboundedSender<Value>,
    pending: PendingApprovals,
    srv_seq: Arc<AtomicU64>,
    /// The running turn's id (shared with the handle), used to tag item events.
    turn: Arc<Mutex<Option<u64>>>,
}

impl ThreadUi {
    fn notify(&self, method: &'static str, mut params: Value) {
        params["threadId"] = Value::String(self.thread_id.clone());
        let _ = self
            .out
            .send(Outgoing::Notification { method, params }.to_json());
    }

    fn turn_id(&self) -> u64 {
        self.turn.lock().unwrap().unwrap_or(0)
    }
}

impl Ui for ThreadUi {
    /// Project the core [`Event`] stream onto the native wire via the shared
    /// [`wire::project_event`]. The turn bracket (`turn/started`/`turn/completed`)
    /// is constructed by the worker, which owns the turn id.
    fn emit(&self, ev: &Event) {
        if let Some((method, params)) = wire::project_event(ev, self.turn_id()) {
            self.notify(method, params);
        }
    }
}

impl Approver for ThreadUi {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        // Server reverse-request ids live in the server's own integer counter
        // space (§ wire): a no-method response always answers one of ours.
        let id = RequestId::Num(self.srv_seq.fetch_add(1, Ordering::SeqCst) as i64);
        let (reply, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), reply);
        // A file change carries a diff preview; nothing else does — so the
        // preview's presence is exactly the command/fileChange discriminant.
        let kind = if req.preview.is_some() {
            "fileChange"
        } else {
            "command"
        };
        let mut params = json!({
            "turnId": self.turn_id(),
            "kind": kind,
            "description": req.description,
        });
        if let Some(rules) = &req.remember_rules {
            params["rememberRules"] = json!(rules);
        }
        if let Some(preview) = &req.preview {
            params["preview"] = Value::String(preview.clone());
        }
        params["threadId"] = Value::String(self.thread_id.clone());
        let sent = self
            .out
            .send(
                Outgoing::ServerRequest {
                    id: id.clone(),
                    method: "approval/request",
                    params,
                }
                .to_json(),
            )
            .is_ok();
        let pending = self.pending.clone();
        Box::pin(async move {
            if !sent {
                pending.lock().unwrap().remove(&id);
                return Decision::Deny;
            }
            // Dropped sender (input closed, server shutting down) = deny.
            let decision = rx.await.unwrap_or(Decision::Deny);
            pending.lock().unwrap().remove(&id);
            decision
        })
    }
}
