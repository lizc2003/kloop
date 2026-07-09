use std::path::PathBuf;
use std::sync::Arc;

use kloop_provider::Provider;

use crate::permissions::Permissions;

/// Everything a turn needs to run. Construction (env parsing, provider
/// selection) is the caller's concern — see the CLI crate.
#[derive(Clone)]
pub struct Config {
    pub provider: Arc<Provider>,
    pub model: String,
    pub system: String,
    pub max_rounds: usize,
    pub offload_dir: PathBuf,
    /// Usable context window in tokens; None disables compaction entirely.
    pub context_window: Option<u64>,
    /// Model to switch to (once per turn) after retries are exhausted.
    pub fallback_model: Option<String>,
    /// Tool-execution gate; the Arc is shared into sub-agent configs so the
    /// session approval cache is inherited.
    pub permissions: Arc<Permissions>,
}
