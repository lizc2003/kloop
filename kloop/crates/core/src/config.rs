use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kloop_provider::Provider;

use crate::file_state::FileState;
use crate::hooks::Hooks;
use crate::inbox::Inbox;
use crate::permissions::Permissions;
use crate::tools::BackgroundShells;
use crate::tools::BackgroundTasks;
use crate::tools::ToolSource;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SurfaceCapabilities {
    pub questions: bool,
    pub plan_control: bool,
    pub workflow: bool,
    pub worktree: bool,
}

/// Everything a turn needs to run. Construction (env parsing, provider
/// selection) is the caller's concern — see the CLI crate.
#[derive(Clone)]
pub struct Config {
    pub provider: Arc<Provider>,
    pub model: String,
    pub system: String,
    /// Working-directory anchor for THIS agent's tool calls: bash runs here,
    /// relative file/search paths resolve against it, and the permission gate
    /// and OS sandbox key their cwd checks off it. The main agent's is the
    /// process cwd (so behavior is unchanged); a `task {isolation: worktree}`
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
    /// an error ends the turn. Headless `--max-rounds` and an explicit task
    /// `max_rounds` set it for callers that need a runaway bound.
    pub max_rounds: Option<usize>,
    pub offload_dir: PathBuf,
    /// Directory holding session rollout files (`.kloop/sessions`). A sub-agent
    /// the task tool spawns writes its own session file here, named
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
    /// Label identifying whose events these are in the UI: empty for the main
    /// agent, "agent-N" for a sub-agent (stamped by the task tool on its
    /// cloned Config).
    pub agent_label: String,
    /// External command hooks; the shared Arc means sub-agents inherit the
    /// same hook set.
    pub hooks: Arc<Hooks>,
    /// Session-scoped background shell registry (bash run_in_background).
    /// Sub-agents share the parent's through the Config clone; server mode
    /// builds one per thread.
    pub background_shells: Arc<BackgroundShells>,
    /// OS sandbox policy for bash execution; None runs commands bare
    /// (sandbox disabled, platform unsupported, --mock). The permission gate
    /// is independent — approved commands still run inside this sandbox, and
    /// the model escapes per call with disable_sandbox (which faces the same
    /// gate). Sub-agents inherit it with the Config.
    pub sandbox: Option<Arc<crate::sandbox::SandboxPolicy>>,
    /// Named custom agent types the task tool can dispatch to (plan 17
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
    /// The structured task list the model maintains via todo_write (full-table
    /// replace). Session-scoped process state, not history: it survives across
    /// turns within a session and starts empty on resume (the model rebuilds
    /// it from its own todo_write calls replayed in history). Each sub-agent
    /// gets its OWN fresh list — the task tool resets this on the cloned
    /// Config so a sub-agent's planning never touches the parent's.
    pub todos: Arc<std::sync::Mutex<Vec<crate::tools::TodoItem>>>,
    /// Step-boundary injection queue (plans 22, 26, and 51). Items pushed here —
    /// user steering, detached-task results, or a background shell's terminal
    /// notification — are drained at round boundaries (never mid-request) and
    /// recorded as user messages before the next sampling, each with its own
    /// framing. Each sub-agent gets its OWN fresh queue (the task tool resets it
    /// on the cloned Config, like `todos`) so a parent's steering is never drained
    /// by a running sub-agent; a *background* sub-agent instead reinjects into a
    /// clone of the PARENT's queue captured before the reset. A background shell
    /// notifies the inbox of the agent that launched it while keeping command
    /// output in its file. The front-end also holds a clone of this Arc to enqueue
    /// while a turn runs.
    pub inbox: Arc<Inbox>,
    /// Registry of background async tasks — sub-agents (`task {"background":
    /// true}`, plan 26) and programs (`run_program {"background": true}`, plan
    /// 24), which share one lifecycle (detached run, result reinjected into the
    /// inbox). Tracks in-flight tasks for `wait`/`stop_agent` and enforces a
    /// concurrency cap. Shared into sub-agent configs like everything else,
    /// though only the depth-0 agent spawns into it. Kept separate from
    /// `background_shells` on purpose (a shell owns a readable output file and
    /// reinjects only a terminal pointer, not the result body — a different
    /// lifecycle; see [`BackgroundTasks`]).
    pub background_tasks: Arc<BackgroundTasks>,
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
    pub active_worktree: Arc<std::sync::RwLock<Option<crate::worktree::ActiveWorktree>>>,
    /// Session-control tools are capability-gated per frontend. The set is
    /// immutable for a Config so mode changes never churn the provider tool array.
    pub surface: SurfaceCapabilities,
}

impl Config {
    /// Stop every session-scoped detached worker before its frontend/runtime is
    /// torn down. Returns the number that missed the bounded reap deadline.
    pub async fn shutdown_background_work(&self) -> usize {
        let timeout = Duration::from_secs(2);
        let (tasks, shells) = tokio::join!(
            self.background_tasks.shutdown(timeout),
            self.background_shells.shutdown(timeout)
        );
        tasks + shells
    }

    /// The working directory in effect for tool calls right now: the active
    /// worktree's if the session has entered one, else `cwd`. Every tool that
    /// resolves a relative path (or picks a git/search root) reads this, so
    /// `enter_worktree` takes effect immediately.
    pub fn effective_cwd(&self) -> PathBuf {
        self.active_worktree
            .read()
            .unwrap()
            .as_ref()
            .map(|a| a.cwd.clone())
            .unwrap_or_else(|| self.cwd.clone())
    }

    /// The permission gate in effect now — re-anchored at the active worktree
    /// when in one (so acceptEdits keys off the tree), else the base gate.
    pub fn effective_permissions(&self) -> Arc<Permissions> {
        self.active_worktree
            .read()
            .unwrap()
            .as_ref()
            .map(|a| a.permissions.clone())
            .unwrap_or_else(|| self.permissions.clone())
    }

    /// File-observation state in effect now. An entered worktree starts fresh
    /// and does not inherit observations from the main checkout.
    pub fn effective_file_state(&self) -> Arc<FileState> {
        self.active_worktree
            .read()
            .unwrap()
            .as_ref()
            .map(|a| a.file_state.clone())
            .unwrap_or_else(|| self.file_state.clone())
    }

    /// The OS sandbox policy in effect now — with the active worktree added as
    /// a writable root when in one, else the base policy.
    pub fn effective_sandbox(&self) -> Option<Arc<crate::sandbox::SandboxPolicy>> {
        match self.active_worktree.read().unwrap().as_ref() {
            Some(a) => a.sandbox.clone(),
            None => self.sandbox.clone(),
        }
    }

    /// The system prompt in effect now — its working-directory line rewritten
    /// to the active worktree when in one, else the base system.
    pub fn effective_system(&self) -> String {
        self.active_worktree
            .read()
            .unwrap()
            .as_ref()
            .map(|a| a.system.clone())
            .unwrap_or_else(|| self.system.clone())
    }
}
