//! `/clear` — empty the conversation and start fresh. cc/claw semantics: this
//! empties the *current* session (an append-only compacted-to-nothing marker,
//! which resume replays to empty), it does not fork a new session file.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "clear the conversation and start fresh";

pub fn run(history: &mut History, cfg: &Arc<Config>) -> SlashResult {
    // Replacing with an empty history writes a compacted marker; the usage
    // anchor is dropped and the next turn starts on a blank context.
    history.replace_all(Vec::new());
    // Process-state that lives outside History resets too. Keep the task ID
    // high-water mark so a running peer can never observe an ID being reused.
    cfg.tasks.clear();
    cfg.inbox.drain();
    SlashResult::cleared_message("conversation cleared")
}
