//! `/clear` — empty the conversation and start fresh. cc/claw semantics: this
//! empties the *current* session (an append-only compacted-to-nothing marker,
//! which resume replays to empty), it does not fork a new session file.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "clear the conversation and start fresh";

pub fn run(history: &mut History, cfg: &Arc<Config>) -> SlashResult {
    // Advance the list revision before touching the other session state. This is
    // the reset fence the TUI uses to reject any delayed pre-clear snapshot.
    let (_, snapshot) = match cfg.todos.clear() {
        Ok(cleared) => cleared,
        Err(error) => return SlashResult::message(format!("clear failed: {error:#}")),
    };
    // Replacing with an empty history writes a compacted marker; the usage
    // anchor is dropped and the next turn starts on a blank context.
    history.replace_all(Vec::new());
    cfg.reset_deferred_tool_capabilities();
    cfg.inbox.drain();
    SlashResult::cleared_message("conversation cleared", snapshot)
}
