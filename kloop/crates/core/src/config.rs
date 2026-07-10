use std::path::PathBuf;
use std::sync::Arc;

use kloop_provider::Provider;

use crate::hooks::Hooks;
use crate::permissions::Permissions;
use crate::tools::BackgroundShells;
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
    /// External command hooks; the shared Arc means sub-agents inherit the
    /// same hook set.
    pub hooks: Arc<Hooks>,
    /// Session-scoped background shell registry (bash run_in_background).
    /// Sub-agents share the parent's through the Config clone; server mode
    /// builds one per thread.
    pub background_shells: Arc<BackgroundShells>,
}
