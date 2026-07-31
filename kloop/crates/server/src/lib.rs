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
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::Result;
use serde::Serialize;
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
use kloop_core::interaction::QuestionAnswer;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::interaction::QuestionRequest;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::rollout;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionRuntime;
use kloop_core::rollout::SessionSnapshot;
use kloop_core::rollout::TurnTerminal;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

use wire::Incoming;
use wire::Outgoing;

/// Out-of-band note sink handed to the config factory (rule persistence
/// messages etc.); the server routes these into `note` notifications.
pub type NoteFn = Arc<dyn Fn(&str) + Send + Sync>;

/// Runtime choices fixed when one thread is created. The cwd is always
/// canonicalized by the server before the factory sees it, so every cwd-relative
/// subsystem (permissions, sandbox, project context, skills) can share one anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadStartOptions {
    pub cwd: PathBuf,
    pub model: Option<String>,
}

/// Builds one Config per thread. Called with the thread's resolved runtime
/// choices, approver (routes to `approval/request`), and note sink, so cwd-bound
/// policy and approval caches stay per-thread.
pub type ConfigFactory = Arc<
    dyn Fn(
            ThreadStartOptions,
            Arc<dyn Approver>,
            Option<Arc<dyn Questioner>>,
            NoteFn,
        ) -> Result<Config>
        + Send
        + Sync,
>;

/// One model the engine can start a new thread with. Slice 4 deliberately
/// reports only models the process can name locally; it never fabricates a
/// provider-wide remote catalog.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub display_name: String,
    pub provider: String,
    pub is_default: bool,
}

/// Non-sensitive effective configuration shown to a local protocol client.
/// Provider credentials, MCP headers/env, hook commands, and permission-rule
/// bodies are intentionally absent from this allowlist DTO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    pub cwd: String,
    pub model: Option<String>,
    pub permission_mode: String,
    pub context_window: Option<u64>,
    pub defer_threshold: usize,
    pub sandbox: SandboxConfigInfo,
    pub worktree_enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxConfigInfo {
    pub enabled: bool,
    pub allow_network: bool,
    pub auto_allow: bool,
    pub escalate: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillScope {
    Project,
    User,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillContext {
    Inline,
    Fork,
}

/// Metadata only: a skill body and its allowed-tools list stay private until
/// the normal skill invocation path loads them into a model turn.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: String,
    pub scope: SkillScope,
    pub context: SkillContext,
    pub model: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillsSnapshot {
    pub cwd: String,
    pub skills: Vec<SkillInfo>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum McpTransportKind {
    Stdio,
    Http,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum McpServerState {
    Connected,
    Unavailable,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolInfo {
    pub name: String,
    pub description: String,
}

/// Startup discovery status, not a live health probe. Request handling returns
/// this immutable snapshot and never reconnects or performs network IO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerStatus {
    pub name: String,
    pub transport: McpTransportKind,
    pub state: McpServerState,
    pub tools: Vec<McpToolInfo>,
    pub message: Option<String>,
}

pub type ConfigReader = Arc<dyn Fn(&Path) -> Result<ConfigSnapshot> + Send + Sync>;
pub type SkillsReader = Arc<dyn Fn(&Path) -> Result<SkillsSnapshot> + Send + Sync>;

pub struct ServerPaths {
    pub sessions_dir: PathBuf,
    pub offload_dir: PathBuf,
}

/// Everything the protocol server needs. The CLI owns filesystem/env/network
/// assembly and injects immutable metadata plus safe config/skills callbacks;
/// the server stays a transport/validation layer and never depends on the CLI.
pub struct ServerConfig {
    pub factory: ConfigFactory,
    pub paths: ServerPaths,
    pub models: Vec<ModelInfo>,
    pub mcp_servers: Vec<McpServerStatus>,
    pub config_reader: ConfigReader,
    pub skills_reader: SkillsReader,
}

impl ServerConfig {
    pub fn new(factory: ConfigFactory, paths: ServerPaths) -> Self {
        Self {
            factory,
            paths,
            models: Vec::new(),
            mcp_servers: Vec::new(),
            config_reader: Arc::new(|cwd| {
                Ok(ConfigSnapshot {
                    cwd: cwd.to_string_lossy().to_string(),
                    model: None,
                    permission_mode: "manual".into(),
                    context_window: None,
                    defer_threshold: kloop_core::tools::TOOL_DEFER_THRESHOLD,
                    sandbox: SandboxConfigInfo {
                        enabled: false,
                        allow_network: false,
                        auto_allow: false,
                        escalate: false,
                    },
                    worktree_enabled: false,
                })
            }),
            skills_reader: Arc::new(|cwd| {
                Ok(SkillsSnapshot {
                    cwd: cwd.to_string_lossy().to_string(),
                    skills: Vec::new(),
                    warnings: Vec::new(),
                })
            }),
        }
    }
}

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
        models,
        mcp_servers,
        config_reader,
        skills_reader,
    } = config;
    let (out_tx, out_rx) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(write_loop(output, out_rx));

    let default_cwd =
        std::fs::canonicalize(std::env::current_dir().context("cannot determine server cwd")?)
            .context("cannot canonicalize server cwd")?;
    let mut server = Server {
        out: out_tx,
        threads: HashMap::new(),
        pending: Arc::new(Mutex::new(HashMap::new())),
        srv_seq: Arc::new(AtomicU64::new(1)),
        factory,
        paths,
        models,
        mcp_servers,
        config_reader,
        skills_reader,
        default_cwd,
        initialized: false,
        client_questions: false,
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
    server.pending.lock().unwrap().clear();
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
    /// Canonical base cwd and pinned model for read-only config/skills queries.
    /// These are the persisted thread runtime, not a transient active worktree.
    cwd: PathBuf,
    model: String,
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
    out: mpsc::UnboundedSender<Value>,
    threads: HashMap<String, ThreadHandle>,
    pending: PendingInteractions,
    srv_seq: Arc<AtomicU64>,
    factory: ConfigFactory,
    paths: ServerPaths,
    /// Immutable process-level catalogs/snapshots assembled by the CLI.
    models: Vec<ModelInfo>,
    mcp_servers: Vec<McpServerStatus>,
    /// Safe process-config projection and fresh cwd-scoped skill discovery.
    /// Both callbacks return allowlist DTOs only.
    config_reader: ConfigReader,
    skills_reader: SkillsReader,
    /// Canonical process cwd used when a read method or thread/start omits cwd.
    default_cwd: PathBuf,
    /// Set once the client's `initialize` handshake succeeds. Every other
    /// method is rejected until then, so a version mismatch surfaces
    /// immediately instead of as mysterious downstream failures.
    initialized: bool,
    /// Whether the initialized client opted into `question/request` reverse RPCs.
    client_questions: bool,
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
            (None, Some(id)) => self.handle_interaction_response(id, incoming.result),
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
            "thread/start" => self.thread_start(&params),
            "thread/resume" => self.thread_resume(&params),
            "thread/fork" => self.thread_fork(&params),
            "thread/list" => self.thread_list(&params),
            "thread/read" => self.thread_read(&params),
            "model/list" => self.model_list(&params),
            "config/read" => self.config_read(&params),
            "skills/list" => self.skills_list(&params),
            "mcpServerStatus/list" => self.mcp_server_status_list(&params),
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
        self.client_questions = params
            .get("capabilities")
            .and_then(|capabilities| capabilities.get("questions"))
            .and_then(Value::as_bool)
            .unwrap_or(false);
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
                "questions": true,
                "models": {"list": true},
                "config": {"read": true},
                "skills": {"list": true},
                "mcpServers": {"status": true},
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
            });
        };
        match pending {
            PendingInteraction::Approval(reply) => {
                let decision = match result["decision"].as_str() {
                    Some("accept") => Decision::Allow,
                    Some("acceptForSession") => Decision::AllowSession,
                    Some("acceptAlways") => Decision::AllowAlways,
                    // "decline", "cancel", anything unrecognized, or missing:
                    // the safe answer.
                    _ => Decision::Deny,
                };
                let _ = reply.send(decision);
            }
            PendingInteraction::Question {
                question_index,
                reply,
            } => {
                let outcome = match serde_json::from_value::<wire::QuestionResponse>(result) {
                    Ok(wire::QuestionResponse::Answered {
                        selected,
                        other,
                        notes,
                    }) => QuestionOutcome::Answered(vec![QuestionAnswer {
                        question_index,
                        selected,
                        other,
                        notes,
                    }]),
                    Ok(wire::QuestionResponse::Cancelled) => QuestionOutcome::Cancelled,
                    Err(error) => QuestionOutcome::Unavailable(format!(
                        "malformed question response: {error}"
                    )),
                };
                let _ = reply.send(outcome);
            }
        }
    }

    fn thread_start(&mut self, params: &Value) -> MethodResult {
        let options = parse_thread_start_options(params, &self.default_cwd)?;
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
        let rollout =
            Rollout::new_with_runtime(path, runtime_from_options(&options)).map_err(|e| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot initialize session: {e}"),
                )
            })?;
        let mut history = History::new(self.paths.offload_dir.clone());
        history.attach_rollout(rollout);
        self.spawn_thread(thread_id.clone(), history, options)?;
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
        let snapshot = rollout::load_session_snapshot(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
        let (options, migrate) = resume_options(&snapshot, params, &self.default_cwd)?;
        let (messages, mut rollout) = rollout::resume_session(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume: {e}")))?;
        if migrate {
            rollout
                .append_runtime(&runtime_from_options(&options))
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot migrate session: {e}")))?;
        }
        let count = messages.len();
        let history = History::resume(self.paths.offload_dir.clone(), messages, rollout);
        self.spawn_thread(thread_id.to_string(), history, options.clone())?;
        Ok(json!({
            "thread": thread_runtime_json(thread_id, &options),
            "messageCount": count,
        }))
    }

    /// Fork a session at a cut point into a fresh thread, then spawn it live
    /// (like `thread/resume`) so the client can `turn/start` on it immediately.
    /// `cut` is optional — omit it to fork at the end. A dormant source is read
    /// directly from disk; an active source is rejected so its in-flight turn
    /// cannot be copied before the terminal record makes the prefix complete.
    fn thread_fork(&mut self, params: &Value) -> MethodResult {
        let src_id = str_param(params, "threadId")?;
        if self
            .threads
            .get(src_id)
            .is_some_and(|handle| handle.running.load(Ordering::SeqCst))
        {
            return Err((
                wire::SERVER_ERROR,
                format!("cannot fork thread '{src_id}' while a turn is running"),
            ));
        }
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
        let source_snapshot = rollout::load_session_snapshot(&src)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read source: {e}")))?;
        let (options, migrate_fork) = resume_options(&source_snapshot, params, &self.default_cwd)?;
        let new_path = rollout::fork_session(&src, cut, &self.paths.sessions_dir)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot fork: {e}")))?;
        let new_id = rollout::session_id_of(&new_path);
        let (messages, mut rollout) = rollout::resume_session(&new_path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume fork: {e}")))?;
        if migrate_fork {
            rollout
                .append_runtime(&runtime_from_options(&options))
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot migrate fork: {e}")))?;
        }
        let count = messages.len();
        let history = History::resume(self.paths.offload_dir.clone(), messages, rollout);
        self.spawn_thread(new_id.clone(), history, options.clone())?;
        Ok(json!({
            "thread": thread_runtime_json(&new_id, &options),
            "messageCount": count,
        }))
    }

    fn thread_list(&self, params: &Value) -> MethodResult {
        let limit = match params.get("limit") {
            None | Some(Value::Null) => 50,
            Some(value) => value
                .as_u64()
                .filter(|limit| *limit > 0)
                .map(|limit| limit.min(500) as usize)
                .ok_or((
                    wire::INVALID_PARAMS,
                    "'limit' must be a positive integer".to_string(),
                ))?,
        };
        let offset = match params.get("cursor") {
            None | Some(Value::Null) => 0,
            Some(Value::String(cursor)) => cursor.parse::<usize>().map_err(|_| {
                (
                    wire::INVALID_PARAMS,
                    "'cursor' must be a cursor returned by thread/list".to_string(),
                )
            })?,
            Some(_) => {
                return Err((
                    wire::INVALID_PARAMS,
                    "'cursor' must be a string".to_string(),
                ))
            }
        };
        let paths: Vec<PathBuf> = rollout::sessions_by_recency(&self.paths.sessions_dir)
            .into_iter()
            // Sub-agent transcripts share the sessions dir but are internal;
            // like cc's sidechains and codex's source filter, keep them out
            // of the thread list (still readable by explicit id).
            .filter(|path| !rollout::is_subagent_session(path))
            .collect();
        let mut threads = Vec::new();
        for path in paths.iter().skip(offset).take(limit) {
            let snapshot = match rollout::load_session_snapshot(path) {
                Ok(snapshot) => snapshot,
                Err(_) => continue,
            };
            let runtime = snapshot.runtime.as_ref();
            let thread_id = rollout::session_id_of(path);
            let in_progress = self
                .threads
                .get(&thread_id)
                .is_some_and(|handle| handle.running.load(Ordering::SeqCst));
            let updated_at_ms = path
                .metadata()
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(0);
            threads.push(json!({
                "id": thread_id,
                "messages": snapshot.messages.len(),
                "snippet": rollout::first_user_snippet(&snapshot.messages),
                "cwd": runtime.map(|runtime| runtime.cwd.as_str()),
                "model": runtime.and_then(|runtime| runtime.model.as_deref()),
                "resumable": runtime.is_some(),
                "inProgress": in_progress,
                "forkedFrom": fork_origin_json(path),
                "updatedAtMs": updated_at_ms,
            }));
        }
        let consumed = offset.saturating_add(limit).min(paths.len());
        let next_cursor = (consumed < paths.len()).then(|| consumed.to_string());
        Ok(json!({"threads": threads, "nextCursor": next_cursor}))
    }

    fn thread_read(&self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        let path = rollout::session_path(&self.paths.sessions_dir, thread_id);
        if !path.exists() {
            return Err((wire::SERVER_ERROR, format!("no session '{thread_id}'")));
        }
        let snapshot = rollout::load_session_snapshot(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
        Ok(json!({
            "thread": {
                "id": thread_id,
                "cwd": snapshot.runtime.as_ref().map(|runtime| runtime.cwd.as_str()),
                "model": snapshot.runtime.as_ref().and_then(|runtime| runtime.model.as_deref()),
                "resumable": snapshot.runtime.is_some(),
                "forkedFrom": fork_origin_json(&path),
                "messages": snapshot.messages,
                "terminals": snapshot.terminals,
            }
        }))
    }

    fn model_list(&self, params: &Value) -> MethodResult {
        ensure_empty_params(params, "model/list")?;
        Ok(json!({"models": &self.models}))
    }

    fn config_read(&self, params: &Value) -> MethodResult {
        ensure_known_params(params, "config/read", &["threadId", "cwd"])?;
        let (cwd, pinned_model) = self.read_scope(params)?;
        let mut snapshot = (self.config_reader)(&cwd)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read config: {e:#}")))?;
        // The callback reports the process default; a thread-scoped query must
        // reflect that thread's model pinned in its rollout/runtime instead.
        snapshot.cwd = cwd.to_string_lossy().to_string();
        if let Some(model) = pinned_model {
            snapshot.model = Some(model);
        }
        Ok(json!({"config": snapshot}))
    }

    fn skills_list(&self, params: &Value) -> MethodResult {
        ensure_known_params(params, "skills/list", &["threadId", "cwd", "forceReload"])?;
        match params.get("forceReload") {
            None | Some(Value::Null) | Some(Value::Bool(_)) => {}
            Some(_) => {
                return Err((
                    wire::INVALID_PARAMS,
                    "'forceReload' must be a boolean".into(),
                ))
            }
        }
        let (cwd, _) = self.read_scope(params)?;
        // Discovery is deliberately uncached today, so forceReload and a normal
        // call are both one fresh bounded filesystem scan.
        let mut snapshot = (self.skills_reader)(&cwd)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot list skills: {e:#}")))?;
        snapshot.cwd = cwd.to_string_lossy().to_string();
        Ok(json!(snapshot))
    }

    fn mcp_server_status_list(&self, params: &Value) -> MethodResult {
        ensure_empty_params(params, "mcpServerStatus/list")?;
        Ok(json!({"servers": &self.mcp_servers}))
    }

    /// Resolve the cwd selected by `{threadId? | cwd?}`. With no selector, use
    /// the server's canonical startup cwd. A dormant thread reads its persisted
    /// runtime; legacy sessions without runtime metadata fail closed.
    fn read_scope(&self, params: &Value) -> Result<(PathBuf, Option<String>), (i64, String)> {
        object_params(params, "read method")?;
        let thread_id = params.get("threadId").filter(|value| !value.is_null());
        let cwd = params.get("cwd").filter(|value| !value.is_null());
        if thread_id.is_some() && cwd.is_some() {
            return Err((
                wire::INVALID_PARAMS,
                "supply at most one of 'threadId' or 'cwd'".into(),
            ));
        }
        if let Some(value) = thread_id {
            let thread_id = value.as_str().ok_or((
                wire::INVALID_PARAMS,
                "'threadId' must be a string".to_string(),
            ))?;
            if thread_id.trim().is_empty() {
                return Err((wire::INVALID_PARAMS, "'threadId' must not be empty".into()));
            }
            if let Some(handle) = self.threads.get(thread_id) {
                return Ok((handle.cwd.clone(), Some(handle.model.clone())));
            }
            let path = rollout::session_path(&self.paths.sessions_dir, thread_id);
            if !path.exists() {
                return Err((wire::SERVER_ERROR, format!("no session '{thread_id}'")));
            }
            let snapshot = rollout::load_session_snapshot(&path)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
            let runtime = snapshot.runtime.ok_or((
                wire::SERVER_ERROR,
                format!("session '{thread_id}' has no runtime metadata"),
            ))?;
            let options = options_from_runtime(&runtime)?;
            return Ok((options.cwd, options.model));
        }
        Ok((resolve_cwd(cwd, &self.default_cwd)?, None))
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

    fn spawn_thread(
        &mut self,
        thread_id: String,
        mut history: History,
        options: ThreadStartOptions,
    ) -> Result<(), (i64, String)> {
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
        let questioner: Option<Arc<dyn Questioner>> = self
            .client_questions
            .then(|| ui.clone() as Arc<dyn Questioner>);
        let runtime_cwd = options.cwd.to_string_lossy().to_string();
        let pin_default_model = options.model.is_none();
        let mut cfg = (self.factory)(
            options,
            ui.clone(),
            questioner,
            Arc::new(move |s: &str| note_ui.emit(&Event::Note(s.to_string()))),
        )
        .map_err(|e| (wire::SERVER_ERROR, format!("cannot build config: {e:#}")))?;
        if pin_default_model {
            history
                .append_runtime(SessionRuntime {
                    cwd: runtime_cwd,
                    model: Some(cfg.model.clone()),
                })
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot pin session model: {e}")))?;
        }
        // The factory cannot know which thread it is building for; the hook
        // events' session id is stamped here.
        cfg.session_id = thread_id.clone();
        let handle_cwd = cfg.cwd.clone();
        let handle_model = cfg.model.clone();
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
                cwd: handle_cwd,
                model: handle_model,
            },
        );
        Ok(())
    }
}

type MethodResult = Result<Value, (i64, String)>;

fn object_params(params: &Value, method: &str) -> Result<(), (i64, String)> {
    match params {
        Value::Null | Value::Object(_) => Ok(()),
        _ => Err((
            wire::INVALID_PARAMS,
            format!("{method} params must be an object"),
        )),
    }
}

fn ensure_known_params(
    params: &Value,
    method: &str,
    allowed: &[&str],
) -> Result<(), (i64, String)> {
    object_params(params, method)?;
    let Some(params) = params.as_object() else {
        return Ok(());
    };
    if let Some(key) = params.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err((
            wire::INVALID_PARAMS,
            format!("{method} does not accept parameter '{key}'"),
        ));
    }
    Ok(())
}

fn ensure_empty_params(params: &Value, method: &str) -> Result<(), (i64, String)> {
    object_params(params, method)?;
    if params.as_object().is_some_and(|params| !params.is_empty()) {
        return Err((
            wire::INVALID_PARAMS,
            format!("{method} does not accept parameters"),
        ));
    }
    Ok(())
}

fn resolve_cwd(value: Option<&Value>, default_cwd: &Path) -> Result<PathBuf, (i64, String)> {
    let cwd = match value {
        None | Some(Value::Null) => default_cwd.to_path_buf(),
        Some(Value::String(raw)) if !raw.trim().is_empty() => {
            let path = PathBuf::from(raw);
            let path = if path.is_absolute() {
                path
            } else {
                default_cwd.join(path)
            };
            std::fs::canonicalize(&path).map_err(|e| {
                (
                    wire::INVALID_PARAMS,
                    format!("cannot resolve cwd '{}': {e}", path.display()),
                )
            })?
        }
        Some(Value::String(_)) => {
            return Err((wire::INVALID_PARAMS, "'cwd' must not be empty".into()))
        }
        Some(_) => return Err((wire::INVALID_PARAMS, "'cwd' must be a string".into())),
    };
    if !cwd.is_dir() {
        return Err((
            wire::INVALID_PARAMS,
            format!("cwd '{}' is not a directory", cwd.display()),
        ));
    }
    Ok(cwd)
}

fn runtime_from_options(options: &ThreadStartOptions) -> SessionRuntime {
    SessionRuntime {
        cwd: options.cwd.to_string_lossy().to_string(),
        model: options.model.clone(),
    }
}

fn options_from_runtime(runtime: &SessionRuntime) -> Result<ThreadStartOptions, (i64, String)> {
    let cwd = std::fs::canonicalize(&runtime.cwd).map_err(|e| {
        (
            wire::SERVER_ERROR,
            format!("cannot restore cwd '{}': {e}", runtime.cwd),
        )
    })?;
    if !cwd.is_dir() {
        return Err((
            wire::SERVER_ERROR,
            format!("restored cwd '{}' is not a directory", cwd.display()),
        ));
    }
    Ok(ThreadStartOptions {
        cwd,
        model: runtime.model.clone(),
    })
}

fn resume_options(
    snapshot: &SessionSnapshot,
    params: &Value,
    default_cwd: &Path,
) -> Result<(ThreadStartOptions, bool), (i64, String)> {
    if let Some(runtime) = &snapshot.runtime {
        return Ok((options_from_runtime(runtime)?, false));
    }
    if params.get("cwd").is_none_or(Value::is_null) {
        return Err((
            wire::SERVER_ERROR,
            "session predates runtime metadata; supply its original 'cwd' to migrate it safely"
                .into(),
        ));
    }
    Ok((parse_thread_start_options(params, default_cwd)?, true))
}

fn thread_runtime_json(thread_id: &str, options: &ThreadStartOptions) -> Value {
    json!({
        "id": thread_id,
        "cwd": options.cwd.to_string_lossy(),
        "model": options.model,
        "resumable": true,
    })
}

fn fork_origin_json(path: &Path) -> Value {
    let Some(origin) = rollout::fork_origin(path) else {
        return Value::Null;
    };
    let Some((thread_id, cut)) = origin.rsplit_once('#') else {
        return Value::Null;
    };
    json!({
        "threadId": thread_id,
        "cut": cut.parse::<u64>().ok(),
    })
}

fn parse_thread_start_options(
    params: &Value,
    default_cwd: &Path,
) -> Result<ThreadStartOptions, (i64, String)> {
    let cwd = resolve_cwd(params.get("cwd"), default_cwd)?;

    let model = match params.get("model") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        Some(Value::String(_)) => {
            return Err((wire::INVALID_PARAMS, "'model' must not be empty".into()))
        }
        Some(_) => return Err((wire::INVALID_PARAMS, "'model' must be a string".into())),
    };

    Ok(ThreadStartOptions { cwd, model })
}

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
        history.record_turn_terminal(turn_terminal(&reason));
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
    let remaining = cfg.shutdown_background_work().await;
    if remaining > 0 {
        ui.emit(&Event::Note(format!(
            "{remaining} background task(s) missed the shutdown deadline"
        )));
    }
    // The thread is ending (turn channel closed on server shutdown): tear down
    // its active worktree if the model never exited (dirty kept on its branch,
    // clean removed), so trees don't leak past the session.
    if let Some(note) = kloop_core::worktree::finish_active(&cfg).await {
        ui.emit(&Event::Note(note.trim().to_string()));
    }
}

fn turn_terminal(reason: &EndReason) -> TurnTerminal {
    match reason {
        EndReason::Completed => TurnTerminal {
            status: "completed".into(),
            error: None,
        },
        EndReason::MaxRounds => TurnTerminal {
            status: "maxRounds".into(),
            error: None,
        },
        EndReason::Aborted => TurnTerminal {
            status: "aborted".into(),
            error: None,
        },
        EndReason::Error(error) => TurnTerminal {
            status: "error".into(),
            error: Some(error.clone()),
        },
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
    pending: PendingInteractions,
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

    fn active_turn_id(&self) -> Option<u64> {
        *self.turn.lock().unwrap()
    }

    fn turn_id(&self) -> u64 {
        self.active_turn_id().unwrap_or(0)
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
        self.pending
            .lock()
            .unwrap()
            .insert(id.clone(), PendingInteraction::Approval(reply));
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
        let guard = PendingGuard {
            id,
            pending: self.pending.clone(),
        };
        Box::pin(async move {
            let _guard = guard;
            if !sent {
                return Decision::Deny;
            }
            // Dropped sender (input closed, server shutting down) = deny.
            rx.await.unwrap_or(Decision::Deny)
        })
    }
}

impl Questioner for ThreadUi {
    fn ask(
        &self,
        request: QuestionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = QuestionOutcome> + Send + '_>> {
        Box::pin(async move {
            let mut answers = Vec::with_capacity(request.questions.len());
            for (question_index, question) in request.questions.iter().enumerate() {
                let Some(turn_id) = self.active_turn_id() else {
                    return QuestionOutcome::Unavailable(
                        "question requested outside an active turn".into(),
                    );
                };
                let id = RequestId::Num(self.srv_seq.fetch_add(1, Ordering::SeqCst) as i64);
                let (reply, rx) = oneshot::channel();
                self.pending.lock().unwrap().insert(
                    id.clone(),
                    PendingInteraction::Question {
                        question_index,
                        reply,
                    },
                );
                let guard = PendingGuard {
                    id: id.clone(),
                    pending: self.pending.clone(),
                };
                let params = json!({
                    "threadId": self.thread_id,
                    "turnId": turn_id,
                    "questionIndex": question_index,
                    "question": question,
                });
                if self
                    .out
                    .send(
                        Outgoing::ServerRequest {
                            id,
                            method: "question/request",
                            params,
                        }
                        .to_json(),
                    )
                    .is_err()
                {
                    return QuestionOutcome::Unavailable(
                        "server output channel closed while asking a question".into(),
                    );
                }
                let outcome = rx.await.unwrap_or_else(|_| {
                    QuestionOutcome::Unavailable("question response channel was dropped".into())
                });
                drop(guard);
                match outcome {
                    QuestionOutcome::Answered(mut one) if one.len() == 1 => {
                        answers.push(one.remove(0));
                    }
                    QuestionOutcome::Answered(_) => {
                        return QuestionOutcome::Unavailable(
                            "question response contained the wrong answer count".into(),
                        )
                    }
                    QuestionOutcome::Cancelled => return QuestionOutcome::Cancelled,
                    QuestionOutcome::Unavailable(error) => {
                        return QuestionOutcome::Unavailable(error)
                    }
                }
            }
            match request.validate_answers(&answers) {
                Ok(()) => QuestionOutcome::Answered(answers),
                Err(error) => QuestionOutcome::Unavailable(format!(
                    "client returned an invalid question answer: {error}"
                )),
            }
        })
    }
}
