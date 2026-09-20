use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::provider_route::FrozenProviderRoute;
use crate::provider_route::ProviderCatalog;

use crate::agent_mailbox::LocalAgentContext;
use crate::file_state::FileState;
use crate::hooks::Hooks;
use crate::inbox::Inbox;
use crate::permissions::Permissions;
use crate::shell_programs::ShellPrograms;
use crate::tools::BackgroundExecutions;
use crate::tools::BackgroundShells;
use crate::tools::DeferredToolUnlocks;
use crate::tools::ToolSource;

#[cfg(not(all(test, windows)))]
pub type PowerShellExecutionGate = tokio::sync::Mutex<()>;

#[cfg(all(test, windows))]
pub struct PowerShellExecutionGate {
    mutex: tokio::sync::Mutex<()>,
    probe: Option<Arc<PowerShellGateProbe>>,
}

#[cfg(all(test, windows))]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PowerShellGateSnapshot {
    pub(crate) active: usize,
    pub(crate) max_active: usize,
    pub(crate) entries: usize,
}

#[cfg(all(test, windows))]
struct PowerShellGateProbe {
    attempted: tokio::sync::mpsc::UnboundedSender<()>,
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
    state: std::sync::Mutex<PowerShellGateSnapshot>,
}

#[cfg(all(test, windows))]
pub(crate) struct PowerShellGateController {
    attempted: tokio::sync::mpsc::UnboundedReceiver<()>,
    entered: tokio::sync::mpsc::UnboundedReceiver<()>,
    release: Arc<tokio::sync::Semaphore>,
    probe: Arc<PowerShellGateProbe>,
}

#[cfg(all(test, windows))]
struct PowerShellProbeEntry {
    probe: Arc<PowerShellGateProbe>,
}

#[cfg(all(test, windows))]
impl Drop for PowerShellProbeEntry {
    fn drop(&mut self) {
        let mut state = self.probe.state.lock().unwrap();
        state.active -= 1;
    }
}

#[cfg(all(test, windows))]
pub(crate) struct PowerShellExecutionGuard<'a> {
    probe: Option<Arc<PowerShellGateProbe>>,
    _mutex: tokio::sync::MutexGuard<'a, ()>,
}

#[cfg(all(test, windows))]
pub(crate) struct PowerShellExecutorProbeGuard {
    _probe_entry: Option<PowerShellProbeEntry>,
}

#[cfg(all(test, windows))]
impl Default for PowerShellExecutionGate {
    fn default() -> Self {
        Self {
            mutex: tokio::sync::Mutex::new(()),
            probe: None,
        }
    }
}

#[cfg(all(test, windows))]
impl PowerShellExecutionGate {
    pub(crate) fn instrumented() -> (Self, PowerShellGateController) {
        let (attempted_tx, attempted_rx) = tokio::sync::mpsc::unbounded_channel();
        let (entered_tx, entered_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let probe = Arc::new(PowerShellGateProbe {
            attempted: attempted_tx,
            entered: entered_tx,
            release: Arc::clone(&release),
            state: std::sync::Mutex::new(PowerShellGateSnapshot::default()),
        });
        (
            Self {
                mutex: tokio::sync::Mutex::new(()),
                probe: Some(Arc::clone(&probe)),
            },
            PowerShellGateController {
                attempted: attempted_rx,
                entered: entered_rx,
                release,
                probe,
            },
        )
    }

    pub(crate) async fn lock(&self) -> PowerShellExecutionGuard<'_> {
        if let Some(probe) = &self.probe {
            let _ = probe.attempted.send(());
        }
        let mutex = self.mutex.lock().await;
        PowerShellExecutionGuard {
            probe: self.probe.clone(),
            _mutex: mutex,
        }
    }

    pub(crate) async fn lock_without_probe(&self) -> tokio::sync::MutexGuard<'_, ()> {
        self.mutex.lock().await
    }
}

#[cfg(all(test, windows))]
impl PowerShellExecutionGuard<'_> {
    pub(crate) async fn enter_executor(&self) -> PowerShellExecutorProbeGuard {
        let probe_entry = if let Some(probe) = &self.probe {
            {
                let mut state = probe.state.lock().unwrap();
                state.active += 1;
                state.max_active = state.max_active.max(state.active);
                state.entries += 1;
            }
            let entry = PowerShellProbeEntry {
                probe: Arc::clone(probe),
            };
            let _ = probe.entered.send(());
            probe
                .release
                .acquire()
                .await
                .expect("PowerShell gate probe release semaphore stays open")
                .forget();
            Some(entry)
        } else {
            None
        };
        PowerShellExecutorProbeGuard {
            _probe_entry: probe_entry,
        }
    }
}

#[cfg(all(test, windows))]
impl PowerShellGateController {
    pub(crate) async fn wait_attempted(&mut self) {
        self.attempted
            .recv()
            .await
            .expect("PowerShell gate attempt sender stays open");
    }

    pub(crate) async fn wait_entered(&mut self) {
        self.entered
            .recv()
            .await
            .expect("PowerShell gate entry sender stays open");
    }

    pub(crate) fn release_one(&self) {
        self.release.add_permits(1);
    }

    pub(crate) fn snapshot(&self) -> PowerShellGateSnapshot {
        *self.probe.state.lock().unwrap()
    }
}

#[cfg(all(test, windows))]
mod powershell_gate_tests {
    use super::*;

    #[tokio::test]
    async fn probe_tracks_executor_scope_independently_of_the_mutex_guard() {
        tokio::time::timeout(Duration::from_secs(5), async {
            let (gate, mut controller) = PowerShellExecutionGate::instrumented();

            let first_gate = gate.lock().await;
            controller.wait_attempted().await;
            let (first_executor, ()) = tokio::join!(first_gate.enter_executor(), async {
                controller.wait_entered().await;
                assert_eq!(
                    controller.snapshot(),
                    PowerShellGateSnapshot {
                        active: 1,
                        max_active: 1,
                        entries: 1,
                    }
                );
                controller.release_one();
            });
            drop(first_gate);

            let second_gate = gate.lock().await;
            controller.wait_attempted().await;
            let (second_executor, ()) = tokio::join!(second_gate.enter_executor(), async {
                controller.wait_entered().await;
                assert_eq!(
                    controller.snapshot(),
                    PowerShellGateSnapshot {
                        active: 2,
                        max_active: 2,
                        entries: 2,
                    }
                );
                controller.release_one();
            });

            drop(second_executor);
            drop(second_gate);
            assert_eq!(controller.snapshot().active, 1);
            drop(first_executor);
            assert_eq!(controller.snapshot().active, 0);
        })
        .await
        .expect("PowerShell executor probe test stalled");
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceCapabilities {
    pub questions: bool,
    pub plan_control: bool,
    /// Whether `run_program` (and its `stop_program`) reach the model. Off by
    /// default: the QuickJS engine stays, but `workflow` is its only model-facing
    /// door. As one of sixteen discrete tools a code runtime is never the locally
    /// cheapest choice — measured at 2 uses across 45 sessions, against 1666 bash
    /// calls — while codex (runtime as the *only* tool) and cc (no runtime, bash
    /// fills the role) are each self-consistent. Flip this back on to restore the
    /// previous surface; nothing is deleted.
    pub program: bool,
    pub workflow: bool,
    pub worktree: bool,
    pub scheduler: bool,
}

/// One coherent workspace generation selected for an operation. The contained
/// permission session and file observations stay live; cwd-coupled ownership
/// cannot be mixed with a later worktree transition after this value is cloned.
#[derive(Clone)]
pub struct EffectiveWorkspace {
    pub identity: crate::project::WorkspaceIdentity,
    pub workspace_epoch: u64,
    pub cwd: PathBuf,
    pub permissions: Arc<Permissions>,
    pub file_state: Arc<FileState>,
    pub sandbox: Option<Arc<crate::sandbox::SandboxPolicy>>,
    pub system: String,
    pub branch: Option<String>,
}

/// Everything a turn needs to run. Construction (env parsing, provider
/// selection) is the caller's concern — see the CLI crate.
/// Where the session's context budget comes from. A `/provider` switch may
/// re-derive a catalog-derived budget for the new (provider, model) pair, but
/// must leave a number the environment named alone.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContextBudgetSource {
    /// `KLOOP_CONTEXT_WINDOW` named it, including `off`.
    Pinned,
    /// Derived from the catalog; `fallback` applies when neither the model nor
    /// the gateway declares a window.
    Catalog { fallback: Option<u64> },
}

///
/// `Clone` is a plain field-by-field copy: every shared service stays shared
/// (the `Arc`s are cloned, not rebuilt), so a clone belongs to the same session
/// as its source. The two constructors below start from a clone and then list
/// only what differs — see [`Config::subagent_from`] for the fields a child
/// agent must NOT share.
#[derive(Clone)]
pub struct Config {
    /// Immutable catalog shared by session supervisors and child admission.
    pub provider_catalog: Arc<ProviderCatalog>,
    /// Operation-owned provider route. Frontends freeze a session route before
    /// constructing the Config used by a turn or manual compaction.
    pub provider_route: FrozenProviderRoute,
    pub system: String,
    /// Working-directory anchor for THIS agent's tool calls: bash runs here,
    /// relative file/search paths resolve against it, and the permission gate
    /// and OS sandbox key their cwd checks off it. The main agent's is the
    /// process cwd (so behavior is unchanged); a `run_agent {isolation: worktree}`
    /// sub-agent's is its private git worktree (plan 35), which is how parallel
    /// sub-agents write the same relative path without colliding. `offload_dir`
    /// and `sessions_dir` deliberately do NOT follow, so a sub-agent's
    /// offload/session files land alongside the parent's (a linked worktree
    /// resolves to the parent's partition anyway — same Git common directory).
    pub cwd: PathBuf,
    /// Assembled project-instructions message (context::assemble_instructions).
    /// Injected as a synthetic first user message into every sampling request
    /// — never recorded to history, so resume rereads fresh files and
    /// compaction cannot swallow it. Sub-agents inherit it with the Config.
    pub project_instructions: Option<String>,
    /// Optional sampling-round guardrail. Interactive/server turns and sub-agents
    /// leave this unset and run until the model finishes, the user interrupts, or
    /// an error ends the turn. Headless `--max-rounds` and an explicit run_agent
    /// `max_rounds` set it for callers that need a runaway bound.
    pub max_rounds: Option<usize>,
    /// Directory holding oversized tool results and background shell output,
    /// `~/.kloop/projects/v1/{project-id}/offload` (see `session_store`).
    pub offload_dir: PathBuf,
    /// Directory holding session rollout files, the `sessions` sibling of
    /// `offload_dir`. A sub-agent spawned by run_agent writes its own session
    /// file here, named `{parent session_id}-{agent-N}`, so its transcript is
    /// auditable and separately resumable. Sub-agents inherit the parent's dir
    /// with the Config clone. Empty for ephemeral sessions (tests) — a
    /// sub-agent then stays in-memory like before.
    pub sessions_dir: PathBuf,
    /// Usable context window in tokens; None disables compaction entirely.
    pub context_window: Option<u64>,
    /// Where `context_window` came from, which decides whether a `/provider`
    /// switch is allowed to move it.
    pub context_budget: ContextBudgetSource,
    /// Tool-execution gate; the Arc is shared into sub-agent configs so the
    /// session approval cache is inherited.
    pub permissions: Arc<Permissions>,
    /// General decision questions for the user. This is deliberately separate
    /// from permission approval: a missing frontend returns unavailable rather
    /// than silently choosing an answer.
    pub questioner: Option<Arc<dyn crate::interaction::Questioner>>,
    /// Session-scoped file observations used to prove that a model-visible Read
    /// still describes the bytes a later Write/Edit would replace. This state is
    /// never persisted. Ordinary Config clones share it within one session;
    /// sub-agents and entered worktrees explicitly receive fresh instances.
    pub file_state: Arc<FileState>,
    /// External tool providers (Web tools, MCP servers), merged after the
    /// built-ins. Shared into sub-agent configs like everything else.
    pub tool_sources: Vec<Arc<dyn ToolSource>>,
    /// Session id surfaced in hook events; empty when the session is
    /// ephemeral (mock, tests). Sub-agents inherit the parent's id.
    pub session_id: String,
    /// Typed local routing identity and session-scoped live Agent directory.
    /// Hooks and turn-owned UI use [`Config::agent_label`] for their legacy
    /// empty-main display projection; routing never infers identity from it.
    pub local_agent: LocalAgentContext,
    /// External command hooks; the shared Arc means sub-agents inherit the
    /// same hook set.
    pub hooks: Arc<Hooks>,
    /// Session-scoped background shell registry (`bash {background:true}`).
    /// Sub-agents share the parent's through the Config clone; server mode
    /// builds one per thread.
    pub background_shells: Arc<BackgroundShells>,
    /// Process-wide shell identities resolved once at CLI startup. Config clones
    /// preserve the same executable paths across server threads, sub-agents,
    /// worktrees and code-mode calls.
    pub shell_programs: Arc<ShellPrograms>,
    /// Session-wide PowerShell exclusivity. Direct calls and foreground/background
    /// code-mode programs share this gate through Config clones; independent
    /// server threads build independent Configs and therefore do not serialize.
    pub powershell_execution_gate: Arc<PowerShellExecutionGate>,
    /// OS sandbox policy for bash execution; None runs commands bare
    /// (sandbox disabled, platform unsupported, --mock). The permission gate
    /// is independent — approved commands still run inside this sandbox, and
    /// the model escapes per call with disable_sandbox (which faces the same
    /// gate). Sub-agents inherit it with the Config.
    pub sandbox: Option<Arc<crate::sandbox::SandboxPolicy>>,
    /// Named custom agent types run_agent can dispatch to (plan 17
    /// slice 2). Empty when none are configured. Shared into sub-agent
    /// configs so a sub-agent could look them up too (though it cannot spawn
    /// further sub-agents).
    pub agent_types: Arc<Vec<crate::agent_type::AgentType>>,
    /// Exact tool-name allowlist for THIS agent; None = the full tool set.
    /// Set only on a sub-agent whose agent_type restricts its tools; the
    /// main agent is always None.
    pub tool_allowlist: Option<Arc<std::collections::HashSet<String>>>,
    /// Above this many tools (depth-0 view: built-ins + merged sources) the
    /// source tools are deferred: excluded from the request's tool defs and
    /// discoverable via the tool_search tool instead. Built-ins never defer.
    pub defer_threshold: usize,
    /// Session-memory deferred-tool capability receipts. Each receipt binds one
    /// discovered source snapshot to the current workspace, policy and Agent
    /// authority; stale bindings fail closed and are never persisted.
    pub unlocked_tools: Arc<DeferredToolUnlocks>,
    /// Root-owned, session-scoped structured task graph. Child Configs retain
    /// this Arc as an internal session service, but depth gates keep Task tools out
    /// of child catalogs and reject forged child calls. It is process state,
    /// not history or durable storage: a fresh Config (including resume) starts
    /// with an empty graph.
    pub tasks: Arc<crate::tools::TaskRegistry>,
    /// Step-boundary injection queue (plans 22, 26, and 51). Items pushed here —
    /// user steering, detached-task results, or a background shell's terminal
    /// notification — are drained at round boundaries (never mid-request) and
    /// recorded as user messages before the next sampling, each with its own
    /// framing. Each sub-agent gets its OWN fresh queue (run_agent resets it
    /// on the cloned Config) so a parent's steering is never drained by a
    /// running sub-agent; a *background* sub-agent instead reinjects into a
    /// clone of the PARENT's queue captured before the reset. A background shell
    /// notifies the inbox of the agent that launched it while keeping command
    /// output in its file. The front-end also holds a clone of this Arc to enqueue
    /// while a turn runs.
    pub inbox: Arc<Inbox>,
    /// Independent owner-scoped cron/dynamic-loop scheduler. The registry is not
    /// a background task or shell: it owns timer/store state and only delivers
    /// typed prompts into `inbox` at step boundaries.
    pub scheduler: Arc<crate::scheduler::Scheduler>,
    /// Registry of detached Agent, Program, and Workflow executions. They share
    /// one lifecycle (own cancellation token, result reinjected into the inbox),
    /// typed resource-specific stop tools, `wait_for_activity`, and an 8-way cap.
    /// Kept separate from `background_shells`, whose result is an output file plus
    /// a terminal inbox pointer rather than a reinjected body.
    pub background_executions: Arc<BackgroundExecutions>,
    /// Resource ceilings for a `run_program` (code-mode) run — engine limits
    /// (memory/stack/cpu burst) plus orchestration caps (max agents/items/
    /// concurrency). Defaults are sensible; the CLI overrides from `[codemode]`
    /// config or `KLOOP_PROGRAM_*` env. Sub-agents inherit it with the Config.
    pub program_limits: kloop_codemode::Limits,
    /// Skills loaded from `<name>/SKILL.md` (plan 28): model-selected reusable
    /// prompt packs. The CLI discovers and parses them; core advertises just
    /// name+description in the injected context (progressive disclosure) and
    /// loads a skill's body only when the `skill` tool triggers it. Empty when
    /// none are configured. Shared into sub-agent configs like `agent_types`.
    pub skills: Arc<Vec<crate::skills::Skill>>,
    /// Session-level active worktree (plan 35 slice 2). None = the session
    /// works in `cwd`; Some = it has `enter_worktree`'d, and the `effective_*`
    /// accessors return the tree's cwd/permissions/sandbox/system instead. A
    /// mutable slot (not a plain field) because enter/exit flip it mid-session,
    /// immediately, without rebuilding the Config. Sub-agents get a FRESH empty
    /// slot — they can't enter/exit; see `clone_for_subagent`.
    pub active_worktree: Arc<crate::worktree::ActiveWorktreeState>,
    /// Session-control tools are capability-gated per frontend. The set is
    /// immutable for a Config so mode changes never churn the provider tool array.
    pub surface: SurfaceCapabilities,
}

impl Config {
    pub fn agent_id(&self) -> &kloop_protocol::LocalAgentId {
        self.local_agent.agent_id()
    }

    pub fn parent_agent_id(&self) -> Option<&kloop_protocol::LocalAgentId> {
        self.local_agent.parent_agent_id()
    }

    pub fn agent_label(&self) -> &str {
        self.local_agent.agent_label()
    }

    /// The agent type this sub-agent was dispatched to, for hook matchers.
    /// None on the main agent and on a sub-agent started without one.
    pub fn agent_type(&self) -> Option<String> {
        self.local_agent.agent_type()
    }

    fn base_workspace_at(&self, workspace_epoch: u64) -> EffectiveWorkspace {
        EffectiveWorkspace {
            identity: self.permissions.identity().clone(),
            workspace_epoch,
            cwd: self.cwd.clone(),
            permissions: Arc::clone(&self.permissions),
            file_state: Arc::clone(&self.file_state),
            sandbox: self.sandbox.clone(),
            system: self.system.clone(),
            branch: None,
        }
    }

    pub fn base_workspace(&self) -> EffectiveWorkspace {
        let current = self.active_worktree.read().unwrap();
        let workspace_epoch = self.active_worktree.transition_epoch();
        drop(current);
        self.base_workspace_at(workspace_epoch)
    }

    pub fn effective_workspace(&self) -> EffectiveWorkspace {
        let active = self.active_worktree.read().unwrap();
        let workspace_epoch = self.active_worktree.transition_epoch();
        match active.as_ref() {
            Some(active) => EffectiveWorkspace {
                identity: active.permissions.identity().clone(),
                workspace_epoch,
                cwd: active.cwd.clone(),
                permissions: Arc::clone(&active.permissions),
                file_state: Arc::clone(&active.file_state),
                sandbox: active.sandbox.clone(),
                system: active.system.clone(),
                branch: Some(active.branch.clone()),
            },
            None => self.base_workspace_at(workspace_epoch),
        }
    }

    /// Build one child agent from a single parent workspace generation. Runtime
    /// and session services remain shared; agent-local and workspace-mutable state
    /// starts fresh.
    ///
    /// The body below is the whole list of what a child does NOT share with its
    /// parent — everything else is inherited by `..self.clone()`. That is also
    /// the default for a field added later: inherited unless someone names it
    /// here. `subagent_contract_tests` at the bottom of this file is where that
    /// decision gets written down, field by field.
    pub(crate) fn subagent_from(
        &self,
        workspace: &EffectiveWorkspace,
        max_rounds: Option<usize>,
        agent_id: kloop_protocol::LocalAgentId,
    ) -> Self {
        Self {
            provider_route: self
                .provider_route
                .child_route(None)
                .expect("inherited provider model remains allowlisted"),
            system: workspace.system.clone(),
            cwd: workspace.cwd.clone(),
            max_rounds,
            permissions: Arc::clone(&workspace.permissions),
            questioner: None,
            file_state: Arc::new(FileState::default()),
            // The child's own cache identity, not the parent's. `prompt_cache_key`
            // exists so requests that share a prefix route together; a sub-agent's
            // prefix (its own system scope, a fresh history) has nothing in common
            // with the parent's, so handing them one key asks the router to pin
            // unrelated conversations to the same backend. It also matches the
            // child's session file, which was already `{parent}-{label}` — the two
            // identities were out of step.
            session_id: if self.session_id.is_empty() {
                String::new()
            } else {
                format!("{}-{}", self.session_id, agent_id)
            },
            local_agent: self.local_agent.child(agent_id),
            sandbox: workspace.sandbox.clone(),
            unlocked_tools: Arc::new(DeferredToolUnlocks::default()),
            inbox: Arc::new(Inbox::default()),
            active_worktree: Arc::new(crate::worktree::ActiveWorktreeState::default()),
            surface: SurfaceCapabilities::default(),
            ..self.clone()
        }
    }

    pub fn clone_with_provider_route(&self, provider_route: FrozenProviderRoute) -> Self {
        // The budget belongs to the (provider, model) pair, not to the session:
        // switching to a provider with a smaller window and keeping the old
        // number means the overflow is only discovered by being rejected.
        let context_window = match self.context_budget {
            ContextBudgetSource::Pinned => self.context_window,
            ContextBudgetSource::Catalog { fallback } => self
                .provider_catalog
                .effective_window(provider_route.provider_id(), provider_route.primary_model())
                .or(fallback),
        };
        Self {
            provider_route,
            context_window,
            ..self.clone()
        }
    }

    /// `Config::clone` under a name that cannot be confused with cloning the
    /// `Arc<Config>` a test usually holds — `ctx.cfg.clone()` would hand back
    /// another handle to the same Config, which is never what a test that is
    /// about to tweak one field wants.
    #[cfg(test)]
    pub(crate) fn test_clone(&self) -> Self {
        self.clone()
    }

    #[cfg(test)]
    pub(crate) fn set_test_provider(&mut self, provider: kloop_provider::Provider) {
        let model = self.provider_route.primary_model().to_string();
        let models = self.provider_route.allowed_models().to_vec();
        let (catalog, route) =
            crate::provider_route::ProviderCatalog::from_provider("test", provider, model, models)
                .expect("test provider route is valid");
        self.provider_catalog = catalog;
        self.provider_route = route;
    }

    #[cfg(test)]
    pub(crate) fn set_test_route_models(&mut self, models: &[&str]) {
        self.provider_route = self.provider_route.with_test_models(models);
    }

    pub fn set_max_rounds(&mut self, max_rounds: Option<usize>) {
        self.max_rounds = max_rounds;
    }

    pub fn bind_session(&mut self, session_id: String) -> anyhow::Result<()> {
        self.scheduler.bind_owner(session_id.clone())?;
        self.session_id = session_id;
        Ok(())
    }

    /// The prompt-cache routing hint for every request this session makes,
    /// sampling and compaction alike: one value for the whole conversation, so
    /// the growing prefix keeps landing on a backend that already holds it.
    /// A sub-agent does not inherit this value: `build_sub_config` derives its
    /// own `{parent}-{agent_id}`, because a child's prefix has nothing in common
    /// with the parent's. `None` until a session is bound (`--mock` and tests),
    /// which sends no field at all.
    pub fn cache_key(&self) -> Option<&str> {
        (!self.session_id.is_empty()).then_some(self.session_id.as_str())
    }

    /// Drop non-durable deferred-tool receipts when the conversation branches or
    /// resets. Same-session compaction deliberately does not call this method.
    pub fn reset_deferred_tool_capabilities(&self) {
        self.unlocked_tools.clear();
    }

    /// Stop every session-scoped detached worker before its frontend/runtime is
    /// torn down. Returns the number that missed the bounded reap deadline.
    pub async fn shutdown_background_work(&self, ui: &Arc<dyn crate::agent::Ui>) -> usize {
        self.local_agent.shutdown(ui);
        let timeout = Duration::from_secs(2);
        let scheduler = self.scheduler.shutdown().await;
        let (tasks, shells) = tokio::join!(
            self.background_executions.shutdown(timeout),
            self.background_shells.shutdown(timeout)
        );
        scheduler + tasks + shells
    }

    /// One field of the workspace in effect right now. These used to go through
    /// [`Config::effective_workspace`], which clones the whole generation —
    /// identity, three Arcs, and the entire system prompt — to hand back a
    /// single member; `effective_cwd` alone is read by every tool that resolves
    /// a relative path.
    fn effective_field<T>(
        &self,
        from_worktree: impl FnOnce(&crate::worktree::ActiveWorktree) -> T,
        from_base: impl FnOnce(&Self) -> T,
    ) -> T {
        match self.active_worktree.read().unwrap().as_ref() {
            Some(active) => from_worktree(active),
            None => from_base(self),
        }
    }

    /// The working directory in effect for tool calls right now: the active
    /// worktree's if the session has entered one, else `cwd`. Every tool that
    /// resolves a relative path (or picks a git/search root) reads this, so
    /// `enter_worktree` takes effect immediately.
    pub fn effective_cwd(&self) -> PathBuf {
        self.effective_field(|active| active.cwd.clone(), |config| config.cwd.clone())
    }

    /// The permission gate in effect now — re-anchored at the active worktree
    /// when in one (so acceptEdits keys off the tree), else the base gate.
    pub fn effective_permissions(&self) -> Arc<Permissions> {
        self.effective_field(
            |active| Arc::clone(&active.permissions),
            |config| Arc::clone(&config.permissions),
        )
    }

    /// File-observation state in effect now. An entered worktree starts fresh
    /// and does not inherit observations from the main checkout.
    pub fn effective_file_state(&self) -> Arc<FileState> {
        self.effective_field(
            |active| Arc::clone(&active.file_state),
            |config| Arc::clone(&config.file_state),
        )
    }

    /// The OS sandbox policy in effect now — with the active worktree added as
    /// a writable root when in one, else the base policy.
    pub fn effective_sandbox(&self) -> Option<Arc<crate::sandbox::SandboxPolicy>> {
        self.effective_field(
            |active| active.sandbox.clone(),
            |config| config.sandbox.clone(),
        )
    }

    /// The system prompt in effect now — its working-directory line rewritten
    /// to the active worktree when in one, else the base system.
    pub fn effective_system(&self) -> String {
        self.effective_field(
            |active| active.system.clone(),
            |config| config.system.clone(),
        )
    }
}

/// `subagent_from` inherits by default (`..self.clone()`), so what a child does
/// NOT share is a short explicit list — and this is where that list is written
/// down. A field added to `Config` later is inherited silently; if it is
/// agent-local state, the assertion that says so belongs here.
#[cfg(test)]
mod subagent_contract_tests {
    use super::*;
    use crate::interaction::QuestionOutcome;
    use crate::interaction::QuestionRequest;
    use crate::interaction::Questioner;
    use std::pin::Pin;

    struct NeverAsked;
    impl Questioner for NeverAsked {
        fn ask(
            &self,
            _: QuestionRequest,
        ) -> Pin<Box<dyn Future<Output = QuestionOutcome> + Send + '_>> {
            unreachable!("the contract test never asks")
        }
    }

    /// A parent whose every resettable field is visibly non-default, so a child
    /// that wrongly inherited one would differ from the expected value rather
    /// than happen to match it.
    fn parent() -> Config {
        let mut cfg = crate::tools::testutil::TestConfig::new("config-subagent-contract")
            .context_window(Some(123_456))
            .build()
            .test_clone();
        cfg.session_id = "sess-1".into();
        cfg.questioner = Some(Arc::new(NeverAsked));
        cfg.project_instructions = Some("parent instructions".into());
        cfg.defer_threshold = 7;
        cfg.surface = SurfaceCapabilities {
            questions: true,
            plan_control: true,
            program: true,
            workflow: true,
            worktree: true,
            scheduler: true,
        };
        cfg
    }

    /// The four carriers of "a child must not pollute its parent": each has to
    /// be a NEW instance, not a shared handle.
    #[test]
    fn subagent_gets_fresh_agent_local_state() {
        let parent = parent();
        let child = parent.subagent_from(&parent.effective_workspace(), None, agent_id());

        assert!(!Arc::ptr_eq(&parent.file_state, &child.file_state));
        assert!(!Arc::ptr_eq(&parent.unlocked_tools, &child.unlocked_tools));
        assert!(!Arc::ptr_eq(&parent.inbox, &child.inbox));
        assert!(!Arc::ptr_eq(
            &parent.active_worktree,
            &child.active_worktree
        ));
    }

    /// Everything else the child resets, by value.
    #[test]
    fn subagent_resets_its_own_identity_and_surface() {
        let parent = parent();
        let workspace = parent.effective_workspace();
        let child = parent.subagent_from(&workspace, Some(3), agent_id());

        assert_eq!(child.session_id, "sess-1-agent-7");
        assert_eq!(child.agent_id().to_string(), "agent-7");
        assert_eq!(child.parent_agent_id(), Some(parent.agent_id()));
        assert_eq!(child.max_rounds, Some(3));
        assert!(child.questioner.is_none());
        assert_eq!(child.surface, SurfaceCapabilities::default());
        assert_ne!(parent.surface, SurfaceCapabilities::default());
        // The child is pinned to the ONE workspace generation the parent chose,
        // not to whatever `self` reads at call time.
        assert_eq!(child.system, workspace.system);
        assert_eq!(child.cwd, workspace.cwd);
        assert!(Arc::ptr_eq(&child.permissions, &workspace.permissions));
        assert_eq!(
            child.sandbox.as_ref().map(Arc::as_ptr),
            workspace.sandbox.as_ref().map(Arc::as_ptr)
        );
        // Its route is its own child route, not the parent's frozen one.
        assert_eq!(child.provider_route.primary_model(), "mock");
    }

    /// An ephemeral parent has no session file, so its child gets no derived id
    /// either — `{parent}-{agent}` off an empty parent would be a fake session.
    #[test]
    fn subagent_of_an_unbound_session_stays_unbound() {
        let mut parent = parent();
        parent.session_id = String::new();
        let child = parent.subagent_from(&parent.effective_workspace(), None, agent_id());
        assert_eq!(child.session_id, "");
        assert_eq!(child.cache_key(), None);
    }

    /// The default direction for a field nobody thought about: inherited. These
    /// are the session services and settings a child is meant to share.
    #[test]
    fn subagent_inherits_every_shared_service_and_setting() {
        let parent = parent();
        let child = parent.subagent_from(&parent.effective_workspace(), None, agent_id());

        assert!(Arc::ptr_eq(
            &parent.provider_catalog,
            &child.provider_catalog
        ));
        assert!(Arc::ptr_eq(&parent.permissions, &child.permissions));
        assert!(Arc::ptr_eq(&parent.hooks, &child.hooks));
        assert!(Arc::ptr_eq(
            &parent.background_shells,
            &child.background_shells
        ));
        assert!(Arc::ptr_eq(&parent.shell_programs, &child.shell_programs));
        assert!(Arc::ptr_eq(
            &parent.powershell_execution_gate,
            &child.powershell_execution_gate
        ));
        assert!(Arc::ptr_eq(&parent.agent_types, &child.agent_types));
        assert!(Arc::ptr_eq(&parent.tasks, &child.tasks));
        assert!(Arc::ptr_eq(&parent.scheduler, &child.scheduler));
        assert!(Arc::ptr_eq(
            &parent.background_executions,
            &child.background_executions
        ));
        assert!(Arc::ptr_eq(&parent.skills, &child.skills));

        assert_eq!(child.project_instructions, parent.project_instructions);
        assert_eq!(child.offload_dir, parent.offload_dir);
        assert_eq!(child.sessions_dir, parent.sessions_dir);
        assert_eq!(child.context_window, parent.context_window);
        assert_eq!(child.defer_threshold, parent.defer_threshold);
        assert_eq!(child.tool_allowlist, parent.tool_allowlist);
        assert_eq!(child.tool_sources.len(), parent.tool_sources.len());
    }

    /// A route swap changes exactly one field and shares everything else — a
    /// `/model` mid-session must not hand the session a new inbox or file state.
    #[test]
    fn provider_route_clone_changes_only_the_route() {
        let cfg = parent();
        let swapped = cfg.clone_with_provider_route(cfg.provider_route.clone());

        assert!(Arc::ptr_eq(&cfg.file_state, &swapped.file_state));
        assert!(Arc::ptr_eq(&cfg.inbox, &swapped.inbox));
        assert!(Arc::ptr_eq(&cfg.unlocked_tools, &swapped.unlocked_tools));
        assert!(Arc::ptr_eq(&cfg.active_worktree, &swapped.active_worktree));
        assert_eq!(swapped.session_id, cfg.session_id);
        assert_eq!(swapped.surface, cfg.surface);
        assert!(swapped.questioner.is_some());
    }

    fn agent_id() -> kloop_protocol::LocalAgentId {
        "agent-7".parse().unwrap()
    }
}

/// The compaction budget belongs to the (provider, model) pair, not to the
/// session: `/provider` rebuilds the Config, and if the budget did not move with
/// it, switching to a smaller window would only be discovered by being rejected.
#[cfg(test)]
mod context_budget_tests {
    use super::*;
    use crate::provider_route::ModelKnowledge;
    use crate::provider_route::ProviderCatalogEntry;
    use kloop_provider::Provider;
    use std::collections::BTreeMap;

    fn catalog() -> Arc<ProviderCatalog> {
        let fingerprint = Provider::mock(Vec::new()).endpoint_fingerprint();
        let entry = |id: &str, gateway: Option<u64>| ProviderCatalogEntry {
            id: id.into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            endpoint_fingerprint: fingerprint.clone(),
            default_model: format!("{id}-model"),
            models: vec![format!("{id}-model")],
            context_window: gateway,
            availability: kloop_protocol::ProviderAvailabilityCode::Ready,
            default_effort: None,
            sends_thinking: true,
            factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
        };
        Arc::new(
            ProviderCatalog::new(vec![
                entry("wide", Some(1_000_000)),
                entry("narrow", Some(258_400)),
                entry("bare", None),
            ])
            .unwrap()
            .with_model_knowledge(BTreeMap::from([(
                "wide-model".to_string(),
                ModelKnowledge {
                    context_window: Some(400_000),
                    efforts: None,
                    thinking_budgets: None,
                },
            )])),
        )
    }

    #[test]
    fn switching_provider_re_derives_the_budget_unless_the_env_pinned_it() {
        let catalog = catalog();
        let route = |id: &str| catalog.initial_route(id, None).unwrap();

        let mut cfg = crate::tools::testutil::TestConfig::new("context-budget")
            .build()
            .test_clone();
        cfg.provider_catalog = Arc::clone(&catalog);
        cfg.provider_route = route("wide");
        cfg.context_budget = ContextBudgetSource::Catalog {
            fallback: Some(200_000),
        };
        cfg.context_window = catalog.effective_window("wide", "wide-model");

        // The model takes 400k, the gateway would allow 1M: the model is the cap.
        assert_eq!(cfg.context_window, Some(400_000));
        // Switching carries the budget to the new pair rather than keeping 400k.
        assert_eq!(
            cfg.clone_with_provider_route(route("narrow"))
                .context_window,
            Some(258_400)
        );
        // Neither side declares anything for `bare`, so the fallback applies —
        // it is a floor for the undeclared case, never a third term in the min.
        assert_eq!(
            cfg.clone_with_provider_route(route("bare")).context_window,
            Some(200_000)
        );
        // A number the environment named survives the switch untouched.
        cfg.context_budget = ContextBudgetSource::Pinned;
        assert_eq!(
            cfg.clone_with_provider_route(route("narrow"))
                .context_window,
            Some(400_000)
        );
    }
}
