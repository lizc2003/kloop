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

mod events;
mod wire;

pub use wire::PROTOCOL_VERSION;
pub use wire::RequestId;
pub use wire::project_event;
pub use wire::turn_completed_params;
pub use wire::turn_started_params;

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncRead;
use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use kloop_core::Config;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent::run_turn;
use kloop_core::commands;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_core::history::ProviderSwitchError;
use kloop_core::inbox::Inbox;
use kloop_core::inbox::InboxItem;
use kloop_core::interaction::QuestionAnswer;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::interaction::QuestionRequest;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::ApprovalScope;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::provider_route::ProviderCatalog;
use kloop_core::provider_route::SwitchError;
use kloop_core::rollout;
use kloop_core::rollout::Rollout;
use kloop_core::rollout::SessionRuntime;
use kloop_core::rollout::SessionSnapshot;
use kloop_core::rollout::TurnTerminal;
use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

use events::EventsSyncParams;
use events::RecoverySource;
use events::ThreadProjection;
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
    pub provider_id: Option<String>,
    pub model: Option<String>,
}

/// Builds one Config per thread. Called with the thread's resolved runtime
/// choices, approver (routes to `approval/request`), and note sink, so cwd-bound
/// policy and approval caches stay per-thread.
pub type ConfigFactory = Arc<
    dyn Fn(
            ThreadStartOptions,
            Arc<ProviderCatalog>,
            Arc<dyn Approver>,
            Option<Arc<dyn Questioner>>,
            NoteFn,
        ) -> Result<Config>
        + Send
        + Sync,
>;

/// Non-sensitive effective configuration shown to a local protocol client.
/// Provider credentials, MCP headers/env, hook commands, and permission-rule
/// bodies are intentionally absent from this allowlist DTO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigSnapshot {
    pub cwd: String,
    pub route: Option<ActiveProviderRoute>,
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
/// assembly and injects one immutable provider catalog. Thread configs, catalog
/// reads, and switch validation all consume that same instance.
pub struct ServerConfig {
    pub factory: ConfigFactory,
    pub paths: ServerPaths,
    pub provider_catalog: Arc<ProviderCatalog>,
    pub mcp_servers: Vec<McpServerStatus>,
    pub config_reader: ConfigReader,
    pub skills_reader: SkillsReader,
}

impl ServerConfig {
    pub fn new(factory: ConfigFactory, paths: ServerPaths) -> Self {
        let (provider_catalog, _) = ProviderCatalog::from_provider(
            "mock",
            kloop_provider::Provider::mock(Vec::new()),
            "mock",
            vec!["mock".into()],
            None,
        )
        .expect("built-in server provider catalog is valid");
        Self {
            factory,
            paths,
            provider_catalog,
            mcp_servers: Vec::new(),
            config_reader: Arc::new(|cwd| {
                Ok(ConfigSnapshot {
                    cwd: cwd.to_string_lossy().to_string(),
                    route: None,
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
        provider_catalog,
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
        reverse_request_seq: Arc::new(AtomicU64::new(1)),
        factory,
        paths,
        provider_catalog,
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
        let mut current_cancel = handle.current_cancel.lock().unwrap();
        if let Some(cancel) = current_cancel.take() {
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
    /// Scheduler/background delivery turn: drain the typed inbox without
    /// recording an empty ordinary user message.
    delivery_only: bool,
}

enum ThreadWorkerMsg {
    Turn(Turn),
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
    provider_state: Arc<kloop_core::provider_route::SessionProviderState>,
}

struct ThreadHandle {
    turn_tx: mpsc::UnboundedSender<ThreadWorkerMsg>,
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
    active_route: Arc<Mutex<ActiveProviderRoute>>,
    provider_state: Arc<kloop_core::provider_route::SessionProviderState>,
    /// Generation-scoped public display projection, shared with ThreadUi and
    /// thread/events/sync.
    projection: Arc<ThreadProjection>,
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
        let result = match method {
            "initialize" => self.initialize(&params),
            "thread/start" => self.thread_start(&params),
            "thread/resume" => self.thread_resume(&params),
            "thread/fork" => self.thread_fork(&params),
            "thread/list" => self.thread_list(&params),
            "thread/read" => self.thread_read(&params),
            "thread/events/sync" => self.thread_events_sync(&params),
            "provider/catalog/read" => self.provider_catalog_read(&params),
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
                data: None,
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
                "approvals": {
                    "scopes": ["once", "workspaceSession", "project"]
                },
                "questions": true,
                "providers": {"catalog": true, "switch": true},
                "config": {"read": true},
                "skills": {"list": true},
                "mcpServers": {"status": true},
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
        // Claim the id atomically before writing the runtime. A timestamp
        // collision must never truncate another server's session.
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot create session directory: {e}"),
                )
            })?;
        }
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot claim session: {e}")))?;
        let runtime = runtime_from_options(&options);
        let rollout = match Rollout::new_with_runtime_pending_route(path.clone(), runtime.clone()) {
            Ok(rollout) => rollout,
            Err(error) => {
                let _ = std::fs::remove_file(&path);
                return Err((
                    wire::SERVER_ERROR,
                    format!("cannot initialize session: {error}"),
                ));
            }
        };
        let mut history = History::new(self.paths.offload_dir.clone());
        history.attach_rollout(rollout);
        let seed = SessionSnapshot {
            messages: Vec::new(),
            runtime: Some(runtime),
            terminals: Vec::new(),
            provider_routes: history.provider_routes().to_vec(),
        };
        if let Err(error) = self.spawn_thread(
            thread_id.clone(),
            history,
            options,
            RecoverySource::Fresh,
            seed,
        ) {
            let _ = std::fs::remove_file(&path);
            return Err(error);
        }
        let route = self
            .threads
            .get(&thread_id)
            .expect("spawned thread is registered")
            .active_route
            .lock()
            .unwrap()
            .clone();
        Ok(json!({"thread": {"id": thread_id, "route": route}}))
    }

    fn thread_resume(&mut self, params: &Value) -> MethodResult {
        let thread_id = str_param(params, "threadId")?;
        if self.threads.contains_key(thread_id) {
            return Err((
                wire::SERVER_ERROR,
                format!("thread '{thread_id}' is already active"),
            ));
        }
        let path = rollout::checked_session_path(&self.paths.sessions_dir, thread_id)
            .map_err(|e| (wire::INVALID_PARAMS, format!("invalid thread id: {e}")))?;
        if !path.exists() {
            return Err((wire::SERVER_ERROR, format!("no session '{thread_id}'")));
        }
        let inspected = rollout::inspect_session(&path)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
        let options = resume_options(&inspected.snapshot(), params, &self.default_cwd)?;
        let resumed = inspected
            .recover()
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot resume: {e}")))?;
        let count = resumed.messages.len();
        let seed = resumed.snapshot.clone();
        let history = History::resume(self.paths.offload_dir.clone(), resumed);
        self.spawn_thread(
            thread_id.to_string(),
            history,
            options.clone(),
            RecoverySource::Resumed,
            seed,
        )?;
        let route = self
            .threads
            .get(thread_id)
            .expect("spawned thread is registered")
            .active_route
            .lock()
            .unwrap()
            .clone();
        Ok(json!({
            "thread": thread_runtime_json(thread_id, &options, &route),
            "messageCount": count,
        }))
    }
    /// Fork a session at a cut point into a fresh thread, then spawn it live
    /// (like `thread/resume`) so the client can `turn/start` on it immediately.
    /// `cut` is optional — omit it to fork at the end. A dormant source is
    /// read directly from disk; an active source is rejected so its in-flight
    /// turn cannot be copied before the terminal record makes the prefix complete.
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
        let src = rollout::checked_session_path(&self.paths.sessions_dir, src_id)
            .map_err(|e| (wire::INVALID_PARAMS, format!("invalid thread id: {e}")))?;
        if !src.exists() {
            return Err((wire::SERVER_ERROR, format!("no session '{src_id}'")));
        }
        rollout::inspect_session(&src)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read source: {e}")))?;
        let new_path = rollout::fork_session(&src, cut, &self.paths.sessions_dir)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot fork: {e}")))?;
        let new_id = rollout::session_id_of(&new_path);
        let resumed = match rollout::inspect_session(&new_path).and_then(|read| read.recover()) {
            Ok(resumed) => resumed,
            Err(error) => {
                let _ = std::fs::remove_file(&new_path);
                return Err((wire::SERVER_ERROR, format!("cannot resume fork: {error}")));
            }
        };
        let options = resume_options(&resumed.snapshot, params, &self.default_cwd)?;
        let count = resumed.messages.len();
        let seed = resumed.snapshot.clone();
        let history = History::resume(self.paths.offload_dir.clone(), resumed);
        if let Err(error) = self.spawn_thread(
            new_id.clone(),
            history,
            options.clone(),
            RecoverySource::Resumed,
            seed,
        ) {
            let _ = std::fs::remove_file(&new_path);
            return Err(error);
        }
        let route = self
            .threads
            .get(&new_id)
            .expect("spawned fork is registered")
            .active_route
            .lock()
            .unwrap()
            .clone();
        Ok(json!({
            "thread": thread_runtime_json(&new_id, &options, &route),
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
                ));
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
            let route = snapshot
                .provider_routes
                .last()
                .map(|route| ActiveProviderRoute {
                    revision: route.revision,
                    effort: self.provider_catalog.default_effort(&route.provider_id),
                    provider_id: route.provider_id.clone(),
                    api_family: route.api_family,
                    model: route.primary_model.clone(),
                    continuity: route.continuity,
                });
            threads.push(json!({
                "id": thread_id,
                "messages": snapshot.messages.len(),
                "snippet": rollout::first_user_snippet(&snapshot.messages),
                "cwd": runtime.map(|runtime| runtime.cwd.as_str()),
                "route": route,
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
        let path = rollout::checked_session_path(&self.paths.sessions_dir, thread_id)
            .map_err(|e| (wire::INVALID_PARAMS, format!("invalid thread id: {e}")))?;
        if !path.exists() {
            return Err((wire::SERVER_ERROR, format!("no session '{thread_id}'")));
        }
        let snapshot = events::into_public_session_snapshot(
            rollout::load_session_snapshot(&path)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?,
        );
        let route = snapshot
            .provider_routes
            .last()
            .map(|route| ActiveProviderRoute {
                revision: route.revision,
                // Effort never enters the durable timeline, so a route read off
                // disk reports the effort a session there would start at.
                effort: self.provider_catalog.default_effort(&route.provider_id),
                provider_id: route.provider_id.clone(),
                api_family: route.api_family,
                model: route.primary_model.clone(),
                continuity: route.continuity,
            });
        Ok(json!({
            "thread": {
                "id": thread_id,
                "cwd": snapshot.runtime.as_ref().map(|runtime| runtime.cwd.as_str()),
                "route": route,
                "resumable": snapshot.runtime.is_some(),
                "forkedFrom": fork_origin_json(&path),
                "messages": snapshot.messages,
                "terminals": snapshot.terminals,
            }
        }))
    }

    fn thread_events_sync(&self, params: &Value) -> MethodResult {
        let params =
            EventsSyncParams::parse(params).map_err(|message| (wire::INVALID_PARAMS, message))?;
        let handle = self.threads.get(&params.thread_id).ok_or_else(|| {
            (
                wire::INVALID_PARAMS,
                format!(
                    "thread '{}' is not active; resume it before syncing events",
                    params.thread_id
                ),
            )
        })?;
        let sync = handle
            .projection
            .sync(params.event_cursor.as_ref())
            .map_err(|error| (wire::INVALID_PARAMS, error.to_string()))?;
        serde_json::to_value(sync).map_err(|error| {
            (
                wire::SERVER_ERROR,
                format!("cannot encode event sync: {error}"),
            )
        })
    }

    fn provider_catalog_read(&self, params: &Value) -> MethodResult {
        ensure_empty_params(params, "provider/catalog/read")?;
        Ok(json!({"providers": self.provider_catalog.descriptors()}))
    }

    fn queue_thread_provider_switch(
        &self,
        request_id: RequestId,
        params: &Value,
    ) -> Result<(), SwitchRequestError> {
        ensure_known_params(
            params,
            "thread/provider/switch",
            &["threadId", "providerId", "model", "expectedRouteRevision"],
        )
        .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?;
        let thread_id = str_param(params, "threadId")
            .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?;
        let provider_id = str_param(params, "providerId")
            .map_err(|(_, message)| SwitchRequestError::InvalidParams(message))?
            .to_string();
        let model = match params.get("model") {
            None | Some(Value::Null) => None,
            Some(Value::String(model)) if !model.trim().is_empty() => Some(model.clone()),
            Some(Value::String(_)) => {
                return Err(SwitchRequestError::InvalidParams(
                    "'model' must not be empty".into(),
                ));
            }
            Some(_) => {
                return Err(SwitchRequestError::InvalidParams(
                    "'model' must be a string".into(),
                ));
            }
        };
        let expected_revision = params["expectedRouteRevision"].as_u64().ok_or_else(|| {
            SwitchRequestError::InvalidParams(
                "'expectedRouteRevision' must be a positive integer".into(),
            )
        })?;
        if expected_revision == 0 {
            return Err(SwitchRequestError::InvalidParams(
                "'expectedRouteRevision' must be a positive integer".into(),
            ));
        }
        let handle = self.threads.get(thread_id).ok_or_else(|| {
            SwitchRequestError::UnknownThread(format!("unknown or inactive thread '{thread_id}'"))
        })?;
        if let Err(error) = handle
            .provider_state
            .preflight(&provider_id, model.as_deref())
        {
            let kind = match &error {
                SwitchError::UnknownProvider(_) => "unknown_provider",
                SwitchError::UnknownModel { .. } => "unknown_model",
                SwitchError::Unavailable { .. } => "provider_unavailable",
                SwitchError::StaleRevision { .. } => "stale_revision",
                SwitchError::EffortUnsupported { .. } => "unsupported_effort",
                SwitchError::RouteDrift(_)
                | SwitchError::RevisionExhausted
                | SwitchError::InvalidRevision
                | SwitchError::InvalidTimeline
                | SwitchError::InvalidInheritedProviderModel => "invalid_route",
            };
            return Err(SwitchRequestError::Target {
                message: error.to_string(),
                kind,
            });
        }
        if handle
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return Err(SwitchRequestError::Busy(format!(
                "thread '{thread_id}' already has an active operation"
            )));
        }
        if handle
            .turn_tx
            .send(ThreadWorkerMsg::SwitchProvider {
                provider_id,
                model,
                expected_revision,
                request_id,
            })
            .is_err()
        {
            handle.running.store(false, Ordering::SeqCst);
            return Err(SwitchRequestError::WorkerUnavailable);
        }
        Ok(())
    }
    fn config_read(&self, params: &Value) -> MethodResult {
        ensure_known_params(params, "config/read", &["threadId", "cwd"])?;
        let (cwd, pinned_route) = self.read_scope(params)?;
        let mut snapshot = (self.config_reader)(&cwd)
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot read config: {e:#}")))?;
        snapshot.cwd = cwd.to_string_lossy().to_string();
        if let Some(route) = pinned_route {
            snapshot.route = Some(route);
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
                ));
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
    fn read_scope(
        &self,
        params: &Value,
    ) -> Result<(PathBuf, Option<ActiveProviderRoute>), (i64, String)> {
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
                return Ok((
                    handle.cwd.clone(),
                    Some(handle.active_route.lock().unwrap().clone()),
                ));
            }
            let path = rollout::checked_session_path(&self.paths.sessions_dir, thread_id)
                .map_err(|e| (wire::INVALID_PARAMS, format!("invalid thread id: {e}")))?;
            if !path.exists() {
                return Err((wire::SERVER_ERROR, format!("no session '{thread_id}'")));
            }
            let snapshot = rollout::load_session_snapshot(&path)
                .map_err(|e| (wire::SERVER_ERROR, format!("cannot read session: {e}")))?;
            let route = snapshot.provider_routes.last().ok_or((
                wire::SERVER_ERROR,
                format!("session '{thread_id}' has no provider route timeline"),
            ))?;
            let public_route = ActiveProviderRoute {
                revision: route.revision,
                effort: self.provider_catalog.default_effort(&route.provider_id),
                provider_id: route.provider_id.clone(),
                api_family: route.api_family,
                model: route.primary_model.clone(),
                continuity: route.continuity,
            };
            let runtime = snapshot.runtime.ok_or((
                wire::SERVER_ERROR,
                format!("session '{thread_id}' has no runtime metadata"),
            ))?;
            let options = options_from_runtime(&runtime)?;
            return Ok((options.cwd, Some(public_route)));
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
        handle
            .projection
            .record_input(Some(turn_id), &params["input"]);
        if handle
            .turn_tx
            .send(ThreadWorkerMsg::Turn(Turn {
                text,
                images,
                id: turn_id,
                cancel,
                delivery_only: false,
            }))
            .is_err()
        {
            handle.running.store(false, Ordering::SeqCst);
            *handle.current_cancel.lock().unwrap() = None;
            *handle.turn.lock().unwrap() = None;
            handle.projection.discard_turn(turn_id);
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
        let input = Value::String(text.clone());
        handle.inbox.push(InboxItem::Steer(text));
        let turn_id = *handle.turn.lock().unwrap();
        handle.projection.record_input(turn_id, &input);
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
        recovery_source: RecoverySource,
        mut seed: SessionSnapshot,
    ) -> Result<(), (i64, String)> {
        let projection = Arc::new(
            ThreadProjection::new(
                thread_id.clone(),
                options.cwd.to_string_lossy().to_string(),
                ActiveProviderRoute {
                    revision: 0,
                    provider_id: "pending".into(),
                    api_family: kloop_protocol::ProviderApiFamily::Mock,
                    model: options.model.clone().unwrap_or_default(),
                    continuity: kloop_protocol::ReasoningContinuity::Preserved,
                    effort: None,
                },
                SessionSnapshot {
                    messages: Vec::new(),
                    runtime: None,
                    terminals: Vec::new(),
                    provider_routes: Vec::new(),
                },
                recovery_source,
            )
            .map_err(|e| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot create event generation: {e}"),
                )
            })?,
        );
        // The running turn's id, shared between the ThreadUi (which tags item
        // events with it) and the handle (which `turn/steer` reads).
        let turn = Arc::new(Mutex::new(None));
        let ui = Arc::new(ThreadUi {
            thread_id: thread_id.clone(),
            out: self.out.clone(),
            pending: self.pending.clone(),
            reverse_request_seq: self.reverse_request_seq.clone(),
            turn: turn.clone(),
            projection: projection.clone(),
        });
        let note_ui = ui.clone();
        let questioner: Option<Arc<dyn Questioner>> = self
            .client_questions
            .then(|| ui.clone() as Arc<dyn Questioner>);
        let mut cfg = (self.factory)(
            options,
            Arc::clone(&self.provider_catalog),
            ui.clone(),
            questioner,
            Arc::new(move |s: &str| note_ui.emit(&Event::Note(s.to_string()))),
        )
        .map_err(|e| (wire::SERVER_ERROR, format!("cannot build config: {e:#}")))?;
        history
            .ensure_initial_provider_route(&cfg.provider_route)
            .map_err(|error| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot persist initial provider route: {error}"),
                )
            })?;
        seed.provider_routes = history.provider_routes().to_vec();
        // The factory cannot know which thread it is building for; the hook
        // events' session id is stamped here.
        cfg.bind_session(thread_id.clone())
            .map_err(|e| (wire::SERVER_ERROR, format!("cannot bind session: {e:#}")))?;
        projection.refresh_seed(
            cfg.cwd.to_string_lossy().to_string(),
            cfg.provider_route.public_route(),
            seed,
        );
        let handle_cwd = cfg.cwd.clone();
        let active_route = Arc::new(Mutex::new(cfg.provider_route.public_route()));
        let provider_state = Arc::new(
            kloop_core::provider_route::SessionProviderState::from_timeline(
                Arc::clone(&cfg.provider_catalog),
                history.provider_routes(),
            )
            .map_err(|error| {
                (
                    wire::SERVER_ERROR,
                    format!("cannot restore provider route state: {error}"),
                )
            })?,
        );
        let (turn_tx, turn_rx) = mpsc::unbounded_channel();
        let running = Arc::new(AtomicBool::new(false));
        let current_cancel = Arc::new(Mutex::new(None));
        let turn_seq = Arc::new(AtomicU64::new(1));
        let cfg = Arc::new(cfg);
        // The steering/scheduler queue the worker's turns drain.
        let inbox = cfg.inbox.clone();
        let worker_state = ThreadWorkerState {
            running: running.clone(),
            current_cancel: current_cancel.clone(),
            turn_seq: turn_seq.clone(),
            inbox: inbox.clone(),
            active_route: active_route.clone(),
            provider_state: provider_state.clone(),
        };
        tokio::spawn(thread_worker(cfg, history, ui, turn_rx, worker_state));
        self.threads.insert(
            thread_id,
            ThreadHandle {
                turn_tx,
                running,
                current_cancel,
                inbox,
                turn_seq,
                turn,
                cwd: handle_cwd,
                active_route,
                provider_state,
                projection,
            },
        );
        Ok(())
    }
}

type MethodResult = Result<Value, (i64, String)>;

#[derive(Debug)]
enum SwitchRequestError {
    InvalidParams(String),
    UnknownThread(String),
    Target { message: String, kind: &'static str },
    Busy(String),
    WorkerUnavailable,
}

impl SwitchRequestError {
    fn code(&self) -> i64 {
        match self {
            Self::InvalidParams(_) | Self::UnknownThread(_) | Self::Target { .. } => {
                wire::INVALID_PARAMS
            }
            Self::Busy(_) | Self::WorkerUnavailable => wire::SERVER_ERROR,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Self::InvalidParams(_) => "invalid_params",
            Self::UnknownThread(_) => "unknown",
            Self::Target { kind, .. } => kind,
            Self::Busy(_) => "busy",
            Self::WorkerUnavailable => "unavailable",
        }
    }
}

impl std::fmt::Display for SwitchRequestError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidParams(message) | Self::UnknownThread(message) | Self::Busy(message) => {
                formatter.write_str(message)
            }
            Self::Target { message, .. } => formatter.write_str(message),
            Self::WorkerUnavailable => formatter.write_str("thread worker is unavailable"),
        }
    }
}

fn switch_error_kind(error: &ProviderSwitchError) -> &'static str {
    match error {
        ProviderSwitchError::Route(SwitchError::UnknownProvider(_)) => "unknown_provider",
        ProviderSwitchError::Route(SwitchError::UnknownModel { .. }) => "unknown_model",
        ProviderSwitchError::Route(SwitchError::Unavailable { .. }) => "provider_unavailable",
        ProviderSwitchError::Route(SwitchError::StaleRevision { .. }) => "stale_revision",
        ProviderSwitchError::Route(SwitchError::EffortUnsupported { .. }) => "unsupported_effort",
        ProviderSwitchError::Persistence(_) => "route_persistence_failed",
        ProviderSwitchError::History(_) => "history_projection_failed",
        ProviderSwitchError::Route(
            SwitchError::RouteDrift(_)
            | SwitchError::RevisionExhausted
            | SwitchError::InvalidRevision
            | SwitchError::InvalidTimeline
            | SwitchError::InvalidInheritedProviderModel,
        ) => "invalid_route",
    }
}

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
            return Err((wire::INVALID_PARAMS, "'cwd' must not be empty".into()));
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
        provider_id: None,
        model: None,
    })
}

fn resume_options(
    snapshot: &SessionSnapshot,
    _params: &Value,
    _default_cwd: &Path,
) -> Result<ThreadStartOptions, (i64, String)> {
    let runtime = snapshot.runtime.as_ref().ok_or((
        wire::SERVER_ERROR,
        "session is missing runtime metadata".into(),
    ))?;
    let route = snapshot.provider_routes.last().ok_or((
        wire::SERVER_ERROR,
        "session is missing its provider route timeline".into(),
    ))?;
    let mut options = options_from_runtime(runtime)?;
    options.provider_id = Some(route.provider_id.clone());
    options.model = Some(route.primary_model.clone());
    Ok(options)
}

fn thread_runtime_json(
    thread_id: &str,
    options: &ThreadStartOptions,
    route: &ActiveProviderRoute,
) -> Value {
    json!({
        "id": thread_id,
        "cwd": options.cwd.to_string_lossy(),
        "route": route,
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
            return Err((wire::INVALID_PARAMS, "'model' must not be empty".into()));
        }
        Some(_) => return Err((wire::INVALID_PARAMS, "'model' must be a string".into())),
    };

    let provider_id = match params.get("providerId") {
        None | Some(Value::Null) => None,
        Some(Value::String(raw)) if !raw.trim().is_empty() => Some(raw.trim().to_string()),
        Some(Value::String(_)) => {
            return Err((
                wire::INVALID_PARAMS,
                "'providerId' must not be empty".into(),
            ));
        }
        Some(_) => return Err((wire::INVALID_PARAMS, "'providerId' must be a string".into())),
    };

    Ok(ThreadStartOptions {
        cwd,
        provider_id,
        model,
    })
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

/// Owns this thread's History for its whole life. Client turns and idle
/// scheduler deliveries share one single-flight bracket and one monotonic id
/// allocator; a scheduled prompt never borrows the id of the turn that created it.
async fn thread_worker(
    mut cfg: Arc<Config>,
    mut history: History,
    ui: Arc<ThreadUi>,
    mut turns: mpsc::UnboundedReceiver<ThreadWorkerMsg>,
    state: ThreadWorkerState,
) {
    let ThreadWorkerState {
        running,
        current_cancel,
        turn_seq,
        inbox,
        active_route,
        provider_state,
    } = state;
    let mut inbox_activity = inbox.subscribe_activity();
    loop {
        let message = tokio::select! {
            turn = turns.recv() => {
                let Some(turn) = turn else { break };
                turn
            }
            activity = inbox_activity.changed() => {
                if activity.is_err() {
                    break;
                }
                if inbox.is_empty()
                    || running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    continue;
                }
                let id = turn_seq.fetch_add(1, Ordering::SeqCst);
                *ui.turn.lock().unwrap() = Some(id);
                let cancel = CancellationToken::new();
                *current_cancel.lock().unwrap() = Some(cancel.clone());
                ThreadWorkerMsg::Turn(Turn {
                    text: String::new(),
                    images: Vec::new(),
                    id,
                    cancel,
                    delivery_only: true,
                })
            }
        };
        let turn = match message {
            ThreadWorkerMsg::Turn(turn) => turn,
            ThreadWorkerMsg::SwitchProvider {
                provider_id,
                model,
                expected_revision,
                request_id,
            } => {
                let result = history
                    .switch_provider(
                        &provider_state,
                        expected_revision,
                        &provider_id,
                        model.as_deref(),
                    )
                    .map(|outcome| {
                        let (route, continuity) = match outcome {
                            kloop_core::provider_route::SwitchOutcome::NoOp(route) => {
                                let continuity = route.public_route().continuity;
                                (route, continuity)
                            }
                            kloop_core::provider_route::SwitchOutcome::Changed {
                                route,
                                continuity,
                            } => {
                                ui.projection.update_provider_route(route.public_route());
                                ui.notify(
                                    "thread/provider/changed",
                                    json!({
                                        "route": route.public_route(),
                                        "continuity": continuity,
                                    }),
                                );
                                (route, continuity)
                            }
                        };
                        cfg = Arc::new(cfg.clone_with_provider_route(route.clone()));
                        let public_route = route.public_route();
                        *active_route.lock().unwrap() = public_route.clone();
                        (public_route, continuity)
                    });
                let outgoing = match result {
                    Ok((route, continuity)) => Outgoing::Response {
                        id: request_id,
                        result: json!({"route": route, "continuity": continuity}),
                    },
                    Err(error) => Outgoing::Error {
                        id: Some(request_id),
                        code: wire::SERVER_ERROR,
                        message: error.to_string(),
                        data: Some(json!({"kind": switch_error_kind(&error)})),
                    },
                };
                running.store(false, Ordering::SeqCst);
                let _ = ui.out.send(outgoing.to_json());
                continue;
            }
        };
        if turn.images.is_empty()
            && let Some(args) = provider_command_args(&turn.text)
        {
            let output = run_provider_command(
                &args,
                &mut cfg,
                &provider_state,
                &active_route,
                &mut history,
                &ui,
            );
            ui.notify("system", json!({"text": output}));
            *current_cancel.lock().unwrap() = None;
            running.store(false, Ordering::SeqCst);
            *ui.turn.lock().unwrap() = None;
            continue;
        }
        ui.notify("turn/started", wire::turn_started_params(turn.id));
        let reason = run_turn_or_command(
            &mut cfg,
            &provider_state,
            &active_route,
            &mut history,
            &ui,
            &turn,
        )
        .await;
        history.record_turn_terminal(turn_terminal(&reason));
        ui.emit(&Event::Usage(history.estimated_tokens()));
        ui.notify(
            "turn/completed",
            wire::turn_completed_params(turn.id, &reason),
        );
        *current_cancel.lock().unwrap() = None;
        running.store(false, Ordering::SeqCst);
        *ui.turn.lock().unwrap() = None;
    }
    let shutdown_ui: Arc<dyn Ui> = ui.clone();
    let remaining = cfg.shutdown_background_work(&shutdown_ui).await;
    if remaining > 0 {
        ui.emit(&Event::Note(format!(
            "{remaining} background task(s) missed the shutdown deadline"
        )));
    }
    let active_worktree = kloop_core::worktree::finish_active(&cfg).await;
    if let Some(note) = active_worktree {
        ui.emit(&Event::Note(note.trim().to_string()));
    }
}

fn turn_terminal(reason: &EndReason) -> TurnTerminal {
    TurnTerminal {
        status: reason.terminal_status().into(),
        error: reason.terminal_error().map(ToString::to_string),
        typed_error: reason.terminal_error().cloned(),
    }
}

fn provider_command_args(text: &str) -> Option<Vec<&str>> {
    let rest = text.strip_prefix("/provider")?;
    if rest
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    Some(rest.split_whitespace().collect())
}

fn run_provider_command(
    args: &[&str],
    cfg: &mut Arc<Config>,
    provider_state: &kloop_core::provider_route::SessionProviderState,
    active_route: &Arc<Mutex<ActiveProviderRoute>>,
    history: &mut History,
    ui: &Arc<ThreadUi>,
) -> String {
    let Some(provider_id) = args.first().copied() else {
        let route = provider_state.active_route();
        return format!(
            "active: {} {} (revision {})",
            route.provider_id, route.model, route.revision
        );
    };
    if args.len() > 2 {
        return "usage: /provider <provider> [model]".into();
    }
    let model = args.get(1).copied();
    let expected_revision = provider_state.active_route().revision;
    match history.switch_provider(provider_state, expected_revision, provider_id, model) {
        Ok(kloop_core::provider_route::SwitchOutcome::NoOp(route)) => format!(
            "provider unchanged: {} {} (revision {})",
            route.provider_id(),
            route.primary_model(),
            route.revision()
        ),
        Ok(kloop_core::provider_route::SwitchOutcome::Changed { route, continuity }) => {
            let public_route = route.public_route();
            let provider_name = route.provider_id().to_string();
            let model_name = route.primary_model().to_string();
            let revision = route.revision();
            *cfg = Arc::new(cfg.clone_with_provider_route(route));
            *active_route.lock().unwrap() = public_route.clone();
            ui.projection.update_provider_route(public_route.clone());
            ui.notify(
                "thread/provider/changed",
                json!({"route": public_route, "continuity": continuity}),
            );
            format!(
                "provider switched: {provider_name} {model_name} (revision {revision}, reasoning continuity: {continuity:?})",
            )
        }
        Err(error) => format!("provider switch failed: {error}"),
    }
}

/// Run one turn's work and return its end reason, without touching the turn
/// bracket (the caller owns that). A slash command line reads/rewrites History
/// like a turn but is not a model turn: no user message is recorded and nothing
/// is sampled — its output comes back as a `system` notification, and `/clear`
/// also emits `thread/cleared` so the client resets its transcript. Images (or
/// any non-command text) take the model path.
async fn run_turn_or_command(
    cfg: &mut Arc<Config>,
    provider_state: &kloop_core::provider_route::SessionProviderState,
    active_route: &Arc<Mutex<ActiveProviderRoute>>,
    history: &mut History,
    ui: &Arc<ThreadUi>,
    turn: &Turn,
) -> EndReason {
    if turn.delivery_only {
        let dyn_ui: Arc<dyn Ui> = ui.clone();
        return run_turn(cfg, history, &dyn_ui, &turn.cancel, 0)
            .await
            .reason;
    }
    if turn.images.is_empty() && commands::is_command(&turn.text) {
        let result = commands::run_with_provider_state(
            &turn.text,
            history,
            cfg,
            provider_state,
            &turn.cancel,
        )
        .await;
        if result.route_changed {
            let route = provider_state.active_route();
            *cfg = Arc::new(cfg.clone_with_provider_route(provider_state.freeze()));
            *active_route.lock().unwrap() = route.clone();
            ui.projection.update_provider_route(route);
        }
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
    let reason = run_turn(cfg, history, &dyn_ui, &turn.cancel, 0).await;
    reason.reason
}

/// Per-thread `Ui` + `Approver`: events become thread-tagged notifications,
/// confirmations become `approval/request` server requests answered by id.
struct ThreadUi {
    thread_id: String,
    out: mpsc::UnboundedSender<Value>,
    pending: PendingInteractions,
    reverse_request_seq: Arc<AtomicU64>,
    /// The running turn's id (shared with the handle), used to tag item events.
    turn: Arc<Mutex<Option<u64>>>,
    projection: Arc<ThreadProjection>,
}

impl ThreadUi {
    fn notify(&self, method: &'static str, params: Value) {
        let out = self.out.clone();
        let _ = self.projection.publish(method, params, move |params| {
            let _ = out.send(Outgoing::Notification { method, params }.to_json());
        });
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

fn approval_decision(result: &Value) -> Decision {
    match result["decision"].as_str() {
        Some("accept") => Decision::Allow(ApprovalScope::Once),
        Some("acceptForSession") => Decision::Allow(ApprovalScope::WorkspaceSession),
        Some("acceptForProject") => Decision::Allow(ApprovalScope::Project),
        // decline, cancel, the removed acceptAlways token, anything unknown, or
        // a missing decision all fail closed.
        _ => Decision::Deny,
    }
}

fn approval_scope_name(scope: ApprovalScope) -> &'static str {
    match scope {
        ApprovalScope::Once => "once",
        ApprovalScope::WorkspaceSession => "workspaceSession",
        ApprovalScope::Project => "project",
    }
}

impl Approver for ThreadUi {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        // Server reverse-request ids live in the server's own integer counter
        // space (§ wire): a no-method response always answers one of ours.
        let id = RequestId::Num(self.reverse_request_seq.fetch_add(1, Ordering::SeqCst) as i64);
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
        let approval_scopes = req
            .approval_scopes
            .iter()
            .copied()
            .map(approval_scope_name)
            .collect::<Vec<_>>();
        let mut params = json!({
            "turnId": self.turn_id(),
            "kind": kind,
            "description": req.description,
            "approvalScopes": approval_scopes,
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
            let decision = rx.await;
            decision.unwrap_or(Decision::Deny)
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
                let id =
                    RequestId::Num(self.reverse_request_seq.fetch_add(1, Ordering::SeqCst) as i64);
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
                        );
                    }
                    QuestionOutcome::Cancelled => return QuestionOutcome::Cancelled,
                    QuestionOutcome::Unavailable(error) => {
                        return QuestionOutcome::Unavailable(error);
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

#[cfg(test)]
mod approval_response_tests {
    use super::*;

    #[test]
    fn scoped_approval_tokens_map_exactly_and_legacy_fails_closed() {
        assert_eq!(
            approval_decision(&json!({"decision": "accept"})),
            Decision::Allow(ApprovalScope::Once)
        );
        assert_eq!(
            approval_decision(&json!({"decision": "acceptForSession"})),
            Decision::Allow(ApprovalScope::WorkspaceSession)
        );
        assert_eq!(
            approval_decision(&json!({"decision": "acceptForProject"})),
            Decision::Allow(ApprovalScope::Project)
        );
        for result in [
            json!({"decision": "decline"}),
            json!({"decision": "cancel"}),
            json!({"decision": "acceptAlways"}),
            json!({"decision": "unknown"}),
            json!({}),
        ] {
            assert_eq!(approval_decision(&result), Decision::Deny);
        }
    }
}
