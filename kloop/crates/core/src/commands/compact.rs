//! `/compact` — summarize and shrink the conversation immediately, instead of
//! waiting for the predictive/reactive triggers in the turn loop.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::SlashResult;
use crate::compact::CompactionOutcome;
use crate::compact::CompactionTrigger;
use crate::compact::compact_once;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "summarize and shrink the conversation now";

pub async fn run(
    history: &mut History,
    cfg: &Arc<Config>,
    cancel: &CancellationToken,
) -> SlashResult {
    if let Err(error) = history.ensure_initial_provider_route(&cfg.provider_route) {
        return SlashResult::message(format!(
            "compaction failed: provider route initialization failed: {error}"
        ));
    }
    let provider_attempt = cfg.provider_route.primary_attempt();
    let output = match compact_once(
        cfg,
        &provider_attempt,
        CompactionTrigger::Manual,
        history,
        cancel,
    )
    .await
    {
        Ok(CompactionOutcome::Applied(receipt)) => format!(
            "history compacted: {} summarized, {} kept verbatim",
            receipt.summarized, receipt.kept
        ),
        Ok(CompactionOutcome::NoOp(_)) => {
            "history already compacted: nothing new to summarize".into()
        }
        // A failed summary request leaves History untouched; just report why.
        Err(e) => format!("compaction failed: {e:#}"),
    };
    SlashResult::message(output)
}
