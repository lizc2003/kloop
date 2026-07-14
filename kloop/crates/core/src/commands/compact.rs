//! `/compact` — summarize and shrink the conversation immediately, instead of
//! waiting for the predictive/reactive triggers in the turn loop.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::SlashResult;
use crate::compact::run_compaction;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "summarize and shrink the conversation now";

pub async fn run(
    history: &mut History,
    cfg: &Arc<Config>,
    cancel: &CancellationToken,
) -> SlashResult {
    let output = match run_compaction(cfg, &cfg.model, history, cancel).await {
        Ok(stats) => format!(
            "history compacted: {} summarized, {} kept verbatim",
            stats.summarized, stats.kept
        ),
        // A too-short history or a failed summary request leaves History
        // untouched (run_compaction's invariant); just report why.
        Err(e) => format!("compaction failed: {e:#}"),
    };
    SlashResult {
        output,
        cleared: false,
    }
}
