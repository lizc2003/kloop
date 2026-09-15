//! `/compact` — summarize and shrink the conversation immediately, instead of
//! waiting for the predictive/reactive triggers in the turn loop.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use super::SlashResult;
use crate::agent::Ui;
use crate::compact::CompactionOutcome;
use crate::compact::CompactionTrigger;
use crate::compact::compact_once;
use crate::config::Config;
use crate::event::Event;
use crate::history::History;

pub const SUMMARY: &str = "summarize and shrink the conversation now";

pub async fn run(
    history: &mut History,
    cfg: &Arc<Config>,
    ui: &dyn Ui,
    cancel: &CancellationToken,
) -> SlashResult {
    if let Err(error) = history.ensure_initial_provider_route(&cfg.provider_route) {
        return SlashResult::message(format!(
            "compaction failed: provider route initialization failed: {error}"
        ));
    }
    // A full round-trip (the whole conversation out, a summary back) whose
    // `SlashResult` exists only once it is over: without a line on the live
    // seam the front-end shows a generic spinner over an unchanged screen.
    // Emitted before the outcome is known — a failed or no-op compaction still
    // owes an account of the wait — in the words the automatic triggers use
    // (`agent.rs`).
    ui.emit(&Event::Note("compacting history".into()));
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
