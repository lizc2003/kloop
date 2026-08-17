//! `/cost` — current context estimate plus durable provider-reported usage.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;

pub const SUMMARY: &str = "show provider route, context, and provider-reported usage";

pub fn run(history: &History, cfg: &Arc<Config>) -> SlashResult {
    let used = history.estimated_tokens();
    let route = cfg.provider_route.public_route();
    let mut output = match cfg.context_window {
        Some(window) => {
            let pct = (used as f64 / window as f64 * 100.0).round() as u64;
            format!(
                "provider: {}\nmodel: {}\nroute revision: {}\ncontext: ~{used} / {window} tokens ({pct}%)",
                route.provider_id, route.model, route.revision
            )
        }
        None => format!(
            "provider: {}\nmodel: {}\nroute revision: {}\ncontext: ~{used} tokens (window limit off; compaction disabled)",
            route.provider_id, route.model, route.revision
        ),
    };
    output.push_str("\nprovider-reported usage by provider/model: ");
    match history.provider_usage().grouped() {
        Ok(groups) if groups.is_empty() => output.push_str("unavailable"),
        Ok(groups) => {
            for group in groups {
                let usage = group.aggregate.usage;
                output.push_str(&format!(
                    "\n  {}/{} [{:?}]: input={} output={} cache-read={} cache-create={} responses={}",
                    group.provider_id,
                    group.model,
                    group.api_family,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_read_input_tokens,
                    usage.cache_creation_input_tokens,
                    group.aggregate.reported_responses,
                ));
            }
        }
        Err(error) => output.push_str(&format!("unavailable ({error})")),
    }
    SlashResult::message(output)
}
