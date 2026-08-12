//! `/cost` — current context estimate plus durable provider-reported usage.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "show model, context, and provider-reported usage";

pub fn run(history: &History, cfg: &Arc<Config>) -> SlashResult {
    let used = history.estimated_tokens();
    let mut output = match cfg.context_window {
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
    output.push_str("\nprovider-reported usage across all models: ");
    match history.provider_usage().aggregate() {
        Ok(None) => output.push_str("unavailable"),
        Ok(Some(aggregate)) => {
            let usage = aggregate.usage;
            output.push_str(&format!(
                "\n  input tokens: {}\n  output tokens: {}\n  cache read input tokens: {}\n  cache creation input tokens: {}\n  {} reported responses",
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_input_tokens,
                usage.cache_creation_input_tokens,
                aggregate.reported_responses,
            ));
        }
        Err(error) => output.push_str(&format!("unavailable ({error})")),
    }
    SlashResult::message(output)
}
