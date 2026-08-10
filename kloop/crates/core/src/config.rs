use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kloop_provider::Provider;

use crate::agent_mailbox::LocalAgentContext;
use crate::file_state::FileState;
use crate::hooks::Hooks;
use crate::inbox::Inbox;
use crate::permissions::Permissions;
use crate::shell_programs::ShellPrograms;
use crate::tools::BackgroundExecutions;
use crate::tools::BackgroundShells;
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
    pub cwd: PathBuf,
    pub permissions: Arc<Permissions>,
    pub file_state: Arc<FileState>,
    pub sandbox: Option<Arc<crate::sandbox::SandboxPolicy>>,
    pub system: String,
    pub branch: Option<String>,
}

/// Everything a turn needs to run. Construction (env parsing, provider
/// selection) is the caller's concern — see the CLI crate.
pub struct Config {
    pub provider: Arc<Provider>,
    pub model: String,
    pub system: String,
    /// Working-directory anchor for THIS agent's tool calls: bash runs here,
    /// relative file/search paths resolve against it, and the permission gate
    /// and OS sandbox key their cwd checks off it. The main agent's is the
    /// process cwd (so behavior is unchanged); a `run_agent {isolation: worktree}`
    /// sub-agent's is its private git worktree (plan 35), which is how parallel
    /// sub-agents write the same relative path without colliding. `offload_dir`
    /// and `sessions_dir` deliberately do NOT follow — they stay in the main
    /// repo so a sub-agent's offload/session files land alongside the parent's.
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
    pub offload_dir: PathBuf,
    /// Directory holding session rollout files (`.kloop/sessions`). A sub-agent
    /// run_agent spawns a sub-agent that writes its own session file here, named
    /// `{parent session_id}-{agent-N}`, so its transcript is auditable and
    /// separately resumable. Sub-agents inherit the parent's dir with the
    /// Config clone. Empty for ephemeral sessions (mock, tests) — a sub-agent
    /// then stays in-memory like before.
    pub sessions_dir: PathBuf,
    /// Usable context window in tokens; None disables compaction entirely.
    pub context_window: Option<u64>,
    /// Model to switch to (once per turn) after retries are exhausted.
    pub fallback_model: Option<String>,
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
    /// main agent is always None. `read_offloaded` stays available either way.
    pub tool_allowlist: Option<Arc<std::collections::HashSet<String>>>,
    /// Above this many tools (depth-0 view: built-ins + merged sources) the
    /// source tools are deferred: excluded from the request's tool defs and
    /// discoverable via the tool_search tool instead. Built-ins never defer.
    pub defer_threshold: usize,
    /// Deferred tools unlocked by tool_search this session, keyed by the source
    /// definition generation that was searched. A dynamic source refresh makes
    /// an old entry stale, forcing the model to discover the replacement schema
    /// before dispatch. Shared into sub-agent configs so a parent's discoveries
    /// carry over.
    pub unlocked_tools: Arc<std::sync::RwLock<std::collections::HashMap<String, u64>>>,
    /// Root-owned, session-scoped structured task graph. Child Configs retain
    /// this Arc as an internal session service, but depth gates keep Task V2 out
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

    pub fn base_workspace(&self) -> EffectiveWorkspace {
        EffectiveWorkspace {
            identity: self.permissions.identity().clone(),
            cwd: self.cwd.clone(),
            permissions: Arc::clone(&self.permissions),
            file_state: Arc::clone(&self.file_state),
            sandbox: self.sandbox.clone(),
            system: self.system.clone(),
            branch: None,
        }
    }

    pub fn effective_workspace(&self) -> EffectiveWorkspace {
        let active = self.active_worktree.read().unwrap();
        match active.as_ref() {
            Some(active) => EffectiveWorkspace {
                identity: active.permissions.identity().clone(),
                cwd: active.cwd.clone(),
                permissions: Arc::clone(&active.permissions),
                file_state: Arc::clone(&active.file_state),
                sandbox: active.sandbox.clone(),
                system: active.system.clone(),
                branch: Some(active.branch.clone()),
            },
            None => self.base_workspace(),
        }
    }

    /// Build one child agent from a single parent workspace generation. Runtime
    /// and session services remain shared; agent-local and workspace-mutable state
    /// starts fresh.
    pub(crate) fn subagent_from(
        &self,
        workspace: &EffectiveWorkspace,
        max_rounds: Option<usize>,
        agent_id: kloop_protocol::LocalAgentId,
    ) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            model: self.model.clone(),
            system: workspace.system.clone(),
            cwd: workspace.cwd.clone(),
            project_instructions: self.project_instructions.clone(),
            max_rounds,
            offload_dir: self.offload_dir.clone(),
            sessions_dir: self.sessions_dir.clone(),
            context_window: self.context_window,
            fallback_model: self.fallback_model.clone(),
            permissions: Arc::clone(&workspace.permissions),
            questioner: None,
            file_state: Arc::new(FileState::default()),
            tool_sources: self.tool_sources.clone(),
            session_id: self.session_id.clone(),
            local_agent: self.local_agent.child(agent_id),
            hooks: Arc::clone(&self.hooks),
            background_shells: Arc::clone(&self.background_shells),
            shell_programs: Arc::clone(&self.shell_programs),
            powershell_execution_gate: Arc::clone(&self.powershell_execution_gate),
            sandbox: workspace.sandbox.clone(),
            agent_types: Arc::clone(&self.agent_types),
            tool_allowlist: self.tool_allowlist.clone(),
            defer_threshold: self.defer_threshold,
            unlocked_tools: Arc::clone(&self.unlocked_tools),
            tasks: Arc::clone(&self.tasks),
            inbox: Arc::new(Inbox::default()),
            scheduler: Arc::clone(&self.scheduler),
            background_executions: Arc::clone(&self.background_executions),
            program_limits: self.program_limits,
            skills: Arc::clone(&self.skills),
            active_worktree: Arc::new(crate::worktree::ActiveWorktreeState::default()),
            surface: SurfaceCapabilities::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_clone(&self) -> Self {
        Self {
            provider: Arc::clone(&self.provider),
            model: self.model.clone(),
            system: self.system.clone(),
            cwd: self.cwd.clone(),
            project_instructions: self.project_instructions.clone(),
            max_rounds: self.max_rounds,
            offload_dir: self.offload_dir.clone(),
            sessions_dir: self.sessions_dir.clone(),
            context_window: self.context_window,
            fallback_model: self.fallback_model.clone(),
            permissions: Arc::clone(&self.permissions),
            questioner: self.questioner.clone(),
            file_state: Arc::clone(&self.file_state),
            tool_sources: self.tool_sources.clone(),
            session_id: self.session_id.clone(),
            local_agent: self.local_agent.clone(),
            hooks: Arc::clone(&self.hooks),
            background_shells: Arc::clone(&self.background_shells),
            shell_programs: Arc::clone(&self.shell_programs),
            powershell_execution_gate: Arc::clone(&self.powershell_execution_gate),
            sandbox: self.sandbox.clone(),
            agent_types: Arc::clone(&self.agent_types),
            tool_allowlist: self.tool_allowlist.clone(),
            defer_threshold: self.defer_threshold,
            unlocked_tools: Arc::clone(&self.unlocked_tools),
            tasks: Arc::clone(&self.tasks),
            inbox: Arc::clone(&self.inbox),
            scheduler: Arc::clone(&self.scheduler),
            background_executions: Arc::clone(&self.background_executions),
            program_limits: self.program_limits,
            skills: Arc::clone(&self.skills),
            active_worktree: Arc::clone(&self.active_worktree),
            surface: self.surface,
        }
    }

    pub fn set_model(&mut self, model: String) {
        self.model = model;
    }

    pub fn set_max_rounds(&mut self, max_rounds: Option<usize>) {
        self.max_rounds = max_rounds;
    }

    pub fn bind_session(&mut self, session_id: String) -> anyhow::Result<()> {
        self.scheduler.bind_owner(session_id.clone())?;
        self.session_id = session_id;
        Ok(())
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

    /// The working directory in effect for tool calls right now: the active
    /// worktree's if the session has entered one, else `cwd`. Every tool that
    /// resolves a relative path (or picks a git/search root) reads this, so
    /// `enter_worktree` takes effect immediately.
    pub fn effective_cwd(&self) -> PathBuf {
        self.effective_workspace().cwd
    }

    /// The permission gate in effect now — re-anchored at the active worktree
    /// when in one (so acceptEdits keys off the tree), else the base gate.
    pub fn effective_permissions(&self) -> Arc<Permissions> {
        self.effective_workspace().permissions
    }

    /// File-observation state in effect now. An entered worktree starts fresh
    /// and does not inherit observations from the main checkout.
    pub fn effective_file_state(&self) -> Arc<FileState> {
        self.effective_workspace().file_state
    }

    /// The OS sandbox policy in effect now — with the active worktree added as
    /// a writable root when in one, else the base policy.
    pub fn effective_sandbox(&self) -> Option<Arc<crate::sandbox::SandboxPolicy>> {
        self.effective_workspace().sandbox
    }

    /// The system prompt in effect now — its working-directory line rewritten
    /// to the active worktree when in one, else the base system.
    pub fn effective_system(&self) -> String {
        self.effective_workspace().system
    }
}
