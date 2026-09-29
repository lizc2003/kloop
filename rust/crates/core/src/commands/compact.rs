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

pub const SUMMARY: &str =
    "summarize and shrink the conversation now; /compact <focus> says what to keep in detail";

/// A longer focus is refused, not cut: truncating would change the user's
/// words without saying so. Slash lines go out with pastes expanded, so a
/// pasted block can land here whole, and a summary request pushed over the
/// window sheds the oldest history to make room — history lost for an
/// instruction. The per-message cap `UserAnchors` keeps is the same size.
const FOCUS_LIMIT_CHARS: usize = 2_000;

/// `args` is the focus: what this one summary should keep in detail. Empty
/// means none.
pub async fn run(
    args: &str,
    history: &mut History,
    cfg: &Arc<Config>,
    ui: &dyn Ui,
    cancel: &CancellationToken,
) -> SlashResult {
    let focus = (!args.is_empty()).then_some(args);
    if let Some(focus) = focus {
        let chars = focus.chars().count();
        if chars > FOCUS_LIMIT_CHARS {
            return SlashResult::message(format!(
                "compaction not started: focus is {chars} chars, limit {FOCUS_LIMIT_CHARS}"
            ));
        }
    }
    // A focus the user typed must be seen to have landed or not: dropping it
    // silently is what `/compact <words>` did before it took any.
    let (applied_suffix, noop_suffix) = match focus {
        Some(_) => (" (with focus)", " (focus not applied)"),
        None => ("", ""),
    };
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
        focus,
        history,
        cancel,
    )
    .await
    {
        Ok(CompactionOutcome::Applied(receipt)) => format!(
            "history compacted: {} summarized, {} kept verbatim{applied_suffix}",
            receipt.summarized, receipt.kept
        ),
        // Not re-summarizing the last summary for a focus: a summary is lossy,
        // and what it dropped cannot be recovered by summarizing it again.
        Ok(CompactionOutcome::NoOp(_)) => {
            format!("history already compacted: nothing new to summarize{noop_suffix}")
        }
        // A failed summary request leaves History untouched; just report why.
        Err(e) => format!("compaction failed: {e:#}"),
    };
    SlashResult::message(output)
}
