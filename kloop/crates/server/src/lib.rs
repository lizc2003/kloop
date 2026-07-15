//! Multi-session JSON-RPC server over stdio, after codex's app-server
//! shape: `thread/*` and `turn/*` methods in, per-thread notifications out,
//! approvals as server→client requests answered by id.
//!
//! Every thread is its own tokio task owning a History (same worker shape as
//! the TUI) plus its own `Permissions` via the per-thread Config the factory
//! builds — session approval caches never leak across threads. All output
//! funnels through one writer task, one JSON object per line.

mod wire;

pub use wire::RequestId;

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
use kloop_core::history::History;
use kloop_core::inbox::Inbox;
use kloop_core::inbox::InboxItem;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::rollout;
use kloop_core::rollout::Rollout;
use kloop_core::Config;
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
    input: String,
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
}

type PendingApprovals = Arc<Mutex<HashMap<RequestId, oneshot::Sender<Decision>>>>;

struct Server {
    out: mpsc::UnboundedSender<Value>,
    threads: HashMap<String, ThreadHandle>,
    pending: PendingApprovals,
    srv_seq: Arc<AtomicU64>,
    factory: ConfigFactory,
    paths: ServerPaths,
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
        let result = match method {
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

    fn handle_approval_response(&mut self, id: RequestId, result: Value) {
        let Some(reply) = self.pending.lock().unwrap().remove(&id) else {
            return self.send(Outgoing::Error {
                id: Some(id),
                code: wire::SERVER_ERROR,
                message: "no pending approval with this id".into(),
            });
        };
        let decision = match result["decision"].as_str() {
            Some("allow") => Decision::Allow,
            Some("allowSession") => Decision::AllowSession,
            Some("allowAlways") => Decision::AllowAlways,
            // "deny", anything unrecognized, or missing: the safe answer.
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
        Ok(json!({"threadId": thread_id}))
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
        Ok(json!({"threadId": thread_id, "messageCount": count}))
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
        Ok(json!({"threadId": new_id, "messageCount": count}))
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
        let input = str_param(params, "input")?;
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
        let cancel = CancellationToken::new();
        *handle.current_cancel.lock().unwrap() = Some(cancel.clone());
        if handle
            .turn_tx
            .send(Turn {
                input: input.to_string(),
                cancel,
            })
            .is_err()
        {
            handle.running.store(false, Ordering::SeqCst);
            return Err((wire::SERVER_ERROR, "thread worker is gone".into()));
        }
        Ok(json!({}))
    }

    /// Enqueue steering text typed while a turn runs (or between turns). Unlike
    /// `turn/start` this never starts a turn and never checks the running flag:
    /// the worker's turn loop drains the inbox at round boundaries, so a steer
    /// pushed during a running turn folds into it, and one pushed while idle is
    /// delivered at the top of the next `turn/start`. There is no autowake in
    /// client-driven server mode, so an idle steer waits for that next turn.
    fn turn_steer(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        let input = str_param(params, "input")?;
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            (
                wire::SERVER_ERROR,
                format!("no active thread '{thread_id}'"),
            )
        })?;
        handle.inbox.push(InboxItem::Steer(input.to_string()));
        Ok(json!({}))
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
        let ui = Arc::new(ThreadUi {
            thread_id: thread_id.clone(),
            out: self.out.clone(),
            pending: self.pending.clone(),
            srv_seq: self.srv_seq.clone(),
        });
        let note_ui = ui.clone();
        let mut cfg = (self.factory)(ui.clone(), Arc::new(move |s: &str| note_ui.note(s)))
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
        ui.notify("turn/started", json!({}));
        // A slash command reads/rewrites History like a turn (hence the
        // single-flight running flag and the turn/started..turn/completed
        // bracket the client already waits on) but is not a model turn: no
        // user message is recorded and nothing is sampled. Its output comes
        // back as a `system` notification instead of `text/delta`; `/clear`
        // also emits `thread/cleared` so the client resets its transcript.
        if commands::is_command(&turn.input) {
            let result = commands::run(&turn.input, &mut history, &cfg, &turn.cancel).await;
            // Clear first, then show the output on the now-blank transcript —
            // matching the TUI order (crates/tui/src/lib.rs). Reversed, a
            // client that resets its transcript on `thread/cleared` would wipe
            // the command output it just received.
            if result.cleared {
                ui.notify("thread/cleared", json!({}));
            }
            if !result.output.is_empty() {
                ui.notify("system", json!({"text": result.output}));
            }
            // A skill invoked as `/name` expands to a prompt: record it and run
            // a turn (streaming via the same notifications as a normal turn)
            // rather than ending here.
            if let Some(prompt) = result.run_turn {
                history.record(Message::user_text(prompt));
                let dyn_ui: Arc<dyn Ui> = ui.clone();
                let outcome = run_turn(&cfg, &mut history, &dyn_ui, &turn.cancel, 0).await;
                running.store(false, Ordering::SeqCst);
                ui.notify("turn/completed", turn_completed_params(&outcome.reason));
                continue;
            }
            running.store(false, Ordering::SeqCst);
            ui.notify("turn/completed", json!({"reason": "completed"}));
            continue;
        }
        history.record(Message::user_text(turn.input));
        let dyn_ui: Arc<dyn Ui> = ui.clone();
        let outcome = run_turn(&cfg, &mut history, &dyn_ui, &turn.cancel, 0).await;
        running.store(false, Ordering::SeqCst);
        ui.notify("turn/completed", turn_completed_params(&outcome.reason));
    }
}

/// The `turn/completed` notification params for a turn's end reason. Shared by
/// the normal-turn path and a `/name`-invoked skill turn.
fn turn_completed_params(reason: &EndReason) -> Value {
    match reason {
        EndReason::Completed => json!({"reason": "completed"}),
        EndReason::MaxRounds => json!({"reason": "maxRounds"}),
        EndReason::Aborted => json!({"reason": "aborted"}),
        EndReason::Error(e) => json!({"reason": "error", "message": e}),
    }
}

/// Per-thread `Ui` + `Approver`: events become thread-tagged notifications,
/// confirmations become `approval/request` server requests answered by id.
struct ThreadUi {
    thread_id: String,
    out: mpsc::UnboundedSender<Value>,
    pending: PendingApprovals,
    srv_seq: Arc<AtomicU64>,
}

impl ThreadUi {
    fn notify(&self, method: &'static str, mut params: Value) {
        params["threadId"] = Value::String(self.thread_id.clone());
        let _ = self
            .out
            .send(Outgoing::Notification { method, params }.to_json());
    }
}

impl Ui for ThreadUi {
    fn text_delta(&self, s: &str) {
        self.notify("text/delta", json!({"text": s}));
    }

    fn note(&self, s: &str) {
        self.notify("note", json!({"text": s}));
    }

    fn tool_start(&self, agent: &str, id: &str, name: &str, summary: &str) {
        let mut params = json!({"callId": id, "name": name, "summary": summary});
        // Only sub-agent calls carry the field; the main agent's stay as
        // before so existing clients see an unchanged shape.
        if !agent.is_empty() {
            params["agent"] = Value::String(agent.to_string());
        }
        self.notify("tool/started", params);
    }

    fn tool_end(&self, agent: &str, id: &str, ok: bool) {
        let mut params = json!({"callId": id, "ok": ok});
        if !agent.is_empty() {
            params["agent"] = Value::String(agent.to_string());
        }
        self.notify("tool/completed", params);
    }

    fn agent_start(&self, agent: &str, task: &str) {
        self.notify("agent/started", json!({"agent": agent, "task": task}));
    }

    fn agent_end(&self, agent: &str, ok: bool) {
        self.notify("agent/completed", json!({"agent": agent, "ok": ok}));
    }

    fn todo_update(&self, agent: &str, todos: &[kloop_core::tools::TodoItem]) {
        let mut params = json!({"todos": todos});
        // A sub-agent's list carries the agent field, like tool notifications.
        if !agent.is_empty() {
            params["agent"] = Value::String(agent.to_string());
        }
        self.notify("todo/updated", params);
    }
}

impl Approver for ThreadUi {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        let id = RequestId::Str(format!(
            "srv-{}",
            self.srv_seq.fetch_add(1, Ordering::SeqCst)
        ));
        let (reply, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.clone(), reply);
        let mut params = json!({"description": req.description});
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
