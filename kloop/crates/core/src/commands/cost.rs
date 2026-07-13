//! `/cost` — the current model and how much of the context window is in use.
//! Only the running context size is tracked today (the usage anchor + a
//! char/4 tail estimate); a cumulative token/dollar total would need
//! per-response accounting the history does not yet keep.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "show model and context-window usage";

pub fn run(history: &History, cfg: &Arc<Config>) -> SlashResult {
    let used = history.estimated_tokens();
    let output = match cfg.context_window {
        Some(window) => {
            let pct = (used as f64 / window as f64 * 100.0).round() as u64;
            format!(
                "model: {}\ncontext: ~{used} / {window} tokens ({pct}%)",
                cfg.model
            )
        }
        None => format!(
            "model: {}\ncontext: ~{used} tokens (window limit off; compaction disabled)",
            cfg.model
        ),
    };
    SlashResult {
        output,
        cleared: false,
    }
}
