use std::path::PathBuf;
use std::sync::Arc;

use kloop_provider::Provider;

use crate::hooks::Hooks;
use crate::inbox::Inbox;
use crate::permissions::Permissions;
use crate::tools::BackgroundShells;
use crate::tools::BackgroundTasks;
use crate::tools::ToolSource;

/// Everything a turn needs to run. Construction (env parsing, provider
/// selection) is the caller's concern — see the CLI crate.
#[derive(Clone)]
pub struct Config {
    pub provider: Arc<Provider>,
    pub model: String,
    pub system: String,
    /// Assembled project-instructions message (context::assemble_instructions).
    /// Injected as a synthetic first user message into every sampling request
    /// — never recorded to history, so resume rereads fresh files and
    /// compaction cannot swallow it. Sub-agents inherit it with the Config.
    pub project_instructions: Option<String>,
    pub max_rounds: usize,
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
    /// External tool providers (MCP servers), merged after the built-ins.
    /// Shared into sub-agent configs like everything else.
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
    pub agent_types: Arc<Vec<crate::agents::AgentType>>,
    /// Exact tool-name allowlist for THIS agent; None = the full tool set.
    /// Set only on a sub-agent whose agent_type restricts its tools; the
    /// main agent is always None. `read_offloaded` stays available either way.
    pub tool_allowlist: Option<Arc<std::collections::HashSet<String>>>,
    /// Above this many tools (depth-0 view: built-ins + merged sources) the
    /// source tools are deferred: excluded from the request's tool defs and
    /// discoverable via the tool_search tool instead. Built-ins never defer.
    pub defer_threshold: usize,
    /// Deferred tools unlocked by tool_search this session. Unlocking never
    /// changes the tool defs sent to the model (the array stays byte-stable
    /// for the prompt cache) — it only opens the dispatch gate; the model
    /// works from the schema returned in the tool_search result. Shared into
    /// sub-agent configs so a parent's discoveries carry over.
    pub unlocked_tools: Arc<std::sync::RwLock<std::collections::HashSet<String>>>,
    /// The structured task list the model maintains via todo_write (full-table
    /// replace). Session-scoped process state, not history: it survives across
    /// turns within a session and starts empty on resume (the model rebuilds
    /// it from its own todo_write calls replayed in history). Each sub-agent
    /// gets its OWN fresh list — the task tool resets this on the cloned
    /// Config so a sub-agent's planning never touches the parent's.
    pub todos: Arc<std::sync::Mutex<Vec<crate::tools::TodoItem>>>,
    /// Step-boundary injection queue (plan 22 + 26). Items pushed here — user
    /// steering typed while the turn runs, or a background sub-agent's result —
    /// are drained at round boundaries (never mid-request) and recorded as user
    /// messages before the next sampling, each with its own framing. cc and
    /// codex independently converge on this: enqueue-not-interrupt, delivered
    /// only between steps. Each sub-agent gets its OWN fresh queue (the task
    /// tool resets it on the cloned Config, like `todos`) so a parent's steering
    /// is never drained by a running sub-agent; a *background* sub-agent instead
    /// reinjects into a clone of the PARENT's queue captured before the reset.
    /// The front-end holds a clone of this Arc to enqueue while a turn runs.
    pub inbox: Arc<Inbox>,
    /// Registry of background async tasks — sub-agents (`task {"background":
    /// true}`, plan 26) and programs (`run_program {"background": true}`, plan
    /// 24), which share one lifecycle (detached run, result reinjected into the
    /// inbox). Tracks in-flight tasks for `wait`/`stop_agent` and enforces a
    /// concurrency cap. Shared into sub-agent configs like everything else,
    /// though only the depth-0 agent spawns into it. Kept separate from
    /// `background_shells` on purpose (a shell delivers via an output file, not
    /// a reinjected result — a different lifecycle; see [`BackgroundTasks`]).
    pub background_tasks: Arc<BackgroundTasks>,
    /// Resource ceilings for a `run_program` (code-mode) run — engine limits
    /// (memory/stack/cpu burst) plus orchestration caps (max agents/items/
    /// concurrency). Defaults are sensible; the CLI overrides from `[codemode]`
    /// config or `AGENT_PROGRAM_*` env. Sub-agents inherit it with the Config.
    pub program_limits: kloop_codemode::Limits,
}
