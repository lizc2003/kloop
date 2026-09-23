//! `/cost` — current context estimate plus durable provider-reported usage.

use std::sync::Arc;

use super::SlashResult;
use crate::config::Config;
use crate::history::History;
use crate::usage::{UsageAggregate, UsageOverflow};

pub const SUMMARY: &str = "show provider route, context, and provider-reported usage";

pub fn run(history: &History, cfg: &Arc<Config>) -> SlashResult {
    let used = crate::agent::context_estimate(cfg, history);
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
    let ledger = history.provider_usage();
    match ledger.grouped() {
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
            output.push_str(&cache_hit_line(ledger.aggregate()));
        }
        Err(error) => output.push_str(&format!("unavailable ({error})")),
    }
    SlashResult::message(output)
}

/// The one number the prompt-cache work needs visible mid-session instead of
/// recomputed from a rollout afterwards. It spans every provider/model above,
/// because what it answers is "is this session reading its prefix back".
fn cache_hit_line(aggregate: Result<Option<UsageAggregate>, UsageOverflow>) -> String {
    match aggregate {
        // Unreachable while a group exists, and not worth a panic to say so.
        Ok(None) => String::new(),
        Ok(Some(aggregate)) => {
            let read = aggregate.usage.cache_read_input_tokens;
            let prompt = aggregate.usage.prompt_tokens();
            let rate = match prompt {
                0 => "n/a".to_string(),
                prompt => format!("{}%", (read as f64 / prompt as f64 * 100.0).round() as u64),
            };
            format!("\ncache hit: {read} of {prompt} prompt tokens ({rate})")
        }
        Err(error) => format!("\ncache hit: unavailable ({error})"),
    }
}
