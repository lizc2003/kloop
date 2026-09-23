//! `/context` — where the context goes, segment by segment. `/cost` answers
//! "how full"; this answers "full of what": the system prompt, each injected
//! part, the tool array, and history split by block kind.
//!
//! Every number is the session's one estimate ([`estimate_text_tokens`] and
//! its siblings), so the segments compare with each other but not with the
//! provider's count. When a provider-measured size exists it is shown beside
//! the sum, not mixed in.

use std::sync::Arc;

use kloop_protocol::{ContentBlock, Role, ToolDef};

use super::SlashResult;
use crate::config::Config;
use crate::history::{History, estimate_text_tokens, estimate_tool_def_tokens};

pub const SUMMARY: &str = "show which parts of the prompt the context goes to";

/// How many of the largest tools to name after the tool total.
const LARGEST_TOOLS: usize = 3;

pub fn run(history: &History, cfg: &Arc<Config>) -> SlashResult {
    let workspace = cfg.effective_workspace();
    let mut rows = vec![Row::top(
        "system prompt",
        estimate_text_tokens(&workspace.system),
    )];
    for (label, text) in crate::agent::injected_segments(cfg, &workspace, 0) {
        rows.push(Row::top(label, estimate_text_tokens(&text)));
    }
    let tools = crate::agent::top_level_tool_defs(cfg);
    if let Ok(tools) = &tools {
        rows.push(tool_row(tools));
    }
    let messages = history.messages();
    let history_tokens: u64 = messages
        .iter()
        .map(crate::history::estimate_message_tokens)
        .sum();
    let noun = match messages.len() {
        1 => "message",
        _ => "messages",
    };
    rows.push(Row::top(
        format!("history ({} {noun})", messages.len()),
        history_tokens,
    ));
    rows.extend(history_rows(messages));

    let total: u64 = rows
        .iter()
        .filter(|row| !row.nested)
        .map(|row| row.tokens)
        .sum();
    let mut output = String::from("context by segment (estimated):");
    for row in &rows {
        output.push_str(&row.render());
    }
    output.push_str(&Row::top("total", total).render());
    if let Err(error) = &tools {
        output.push_str(&format!("\ntools: unavailable ({error})"));
    }
    if history.has_usage_anchor() {
        output.push_str(&format!(
            "\nprovider-measured: ~{} tokens (last reported prompt plus what came after)",
            history.estimated_tokens()
        ));
    }
    if let Some(window) = cfg.context_window {
        output.push_str(&format!("\nwindow: {window} tokens"));
    }
    SlashResult::message(output)
}

#[derive(Debug, PartialEq, Eq)]
struct Row {
    label: String,
    tokens: u64,
    detail: String,
    nested: bool,
}

impl Row {
    fn top(label: impl Into<String>, tokens: u64) -> Self {
        Self {
            label: label.into(),
            tokens,
            detail: String::new(),
            nested: false,
        }
    }

    fn render(&self) -> String {
        let (indent, width) = if self.nested {
            ("    ", 22)
        } else {
            ("  ", 24)
        };
        let label = &self.label;
        let tokens = format!("~{}", self.tokens);
        let detail = &self.detail;
        format!("\n{indent}{label:<width$}{tokens:>8}{detail}")
    }
}

fn tool_row(tools: &[ToolDef]) -> Row {
    let mut sized: Vec<(&str, u64)> = tools
        .iter()
        .map(|tool| (tool.name.as_str(), estimate_tool_def_tokens(tool)))
        .collect();
    let tokens = sized.iter().map(|(_, tokens)| tokens).sum();
    // Largest first; ties by name so the line is stable across runs.
    sized.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let largest: Vec<String> = sized
        .iter()
        .take(LARGEST_TOOLS)
        .map(|(name, tokens)| format!("{name} ~{tokens}"))
        .collect();
    let detail = match largest.is_empty() {
        true => String::new(),
        false => format!("  largest: {}", largest.join(", ")),
    };
    Row {
        label: format!("tools ({})", tools.len()),
        tokens,
        detail,
        nested: false,
    }
}

/// History by block kind, zero rows left out. These are per-block estimates,
/// so they come close to the history row above but do not add up to it
/// exactly (the per-message framing is not attributed to any kind).
fn history_rows(messages: &[kloop_protocol::Message]) -> Vec<Row> {
    let kinds = [
        "user text",
        "injected text",
        "assistant text",
        "reasoning",
        "images",
        "tool calls",
        "tool results",
    ];
    let mut tokens = [0u64; 7];
    for message in messages {
        for block in &message.content {
            let kind = match block {
                // Steering, scheduled prompts, reminders and compaction
                // summaries are user-role too; a summary can dwarf what the
                // user actually typed, so it gets its own row.
                ContentBlock::Text { .. } => match (message.role, &message.injected) {
                    (Role::User, None) => 0,
                    (Role::User, Some(_)) => 1,
                    (Role::Assistant, _) => 2,
                },
                ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => 3,
                ContentBlock::Image { .. } => 4,
                ContentBlock::ToolUse { .. } => 5,
                ContentBlock::ToolResult { .. } => 6,
            };
            tokens[kind] += block_tokens(block);
        }
    }
    kinds
        .into_iter()
        .zip(tokens)
        .filter(|(_, tokens)| *tokens > 0)
        .map(|(label, tokens)| Row {
            label: label.to_string(),
            tokens,
            detail: String::new(),
            nested: true,
        })
        .collect()
}

fn block_tokens(block: &ContentBlock) -> u64 {
    serde_json::to_string(block).map_or(0, |json| estimate_text_tokens(&json))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::{Message, ToolResultContent};
    use serde_json::json;

    fn tool(name: &str, description_len: usize) -> ToolDef {
        ToolDef {
            name: name.into(),
            description: "d".repeat(description_len),
            schema: json!({}),
        }
    }

    fn nested(label: &str, tokens: u64) -> Row {
        Row {
            label: label.into(),
            tokens,
            detail: String::new(),
            nested: true,
        }
    }

    #[test]
    fn tool_row_names_the_largest_first_and_breaks_ties_by_name() {
        // name + description + "{}" bytes, divided by four, rounded up.
        let tools = [
            tool("aa", 36),  // 40 bytes -> 10
            tool("bb", 76),  // 80 -> 20
            tool("cc", 36),  // 10, ties with aa
            tool("dd", 116), // 120 -> 30
        ];
        assert_eq!(
            tool_row(&tools),
            Row {
                label: "tools (4)".into(),
                tokens: 70,
                detail: "  largest: dd ~30, bb ~20, aa ~10".into(),
                nested: false,
            }
        );
    }

    #[test]
    fn history_rows_split_by_block_kind_and_leave_out_empty_kinds() {
        let user = ContentBlock::Text {
            text: "question".into(),
        };
        let assistant = ContentBlock::Text {
            text: "answer".into(),
        };
        let call = ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        };
        let result = ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: ToolResultContent::Text("a b c".into()),
            is_error: false,
        };
        let summary = Message::injected(kloop_protocol::Injected::ContextSummary, "summary");
        let messages = [
            summary.clone(),
            Message::user_text("question"),
            Message::assistant(vec![assistant.clone(), call.clone()]),
            Message::tool_results(vec![result.clone()]),
        ];
        // No reasoning and no images: those rows do not appear at all.
        assert_eq!(
            history_rows(&messages),
            vec![
                nested("user text", block_tokens(&user)),
                nested("injected text", block_tokens(&summary.content[0])),
                nested("assistant text", block_tokens(&assistant)),
                nested("tool calls", block_tokens(&call)),
                nested("tool results", block_tokens(&result)),
            ]
        );
    }

    #[test]
    fn run_shows_each_injected_part_and_totals_the_top_level_rows() {
        let base = crate::tools::testutil::TestConfig::new("context-cmd")
            .provider(kloop_provider::Provider::mock(vec![]))
            .models("test-model", &["test-model"])
            .context_window(Some(200_000))
            .build();
        let mut cfg = (*base).clone();
        cfg.project_instructions = Some("i".repeat(400));
        let cfg = Arc::new(cfg);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("hello"));

        let output = run(&history, &cfg).output;
        let lines: Vec<&str> = output.lines().collect();
        assert_eq!(
            lines[..3],
            [
                "context by segment (estimated):",
                Row::top("system prompt", 1)
                    .render()
                    .trim_start_matches('\n'),
                Row::top("project instructions", 100)
                    .render()
                    .trim_start_matches('\n'),
            ]
        );
        assert!(lines[3].starts_with("  tools ("), "{output}");
        let history_row = Row::top(
            "history (1 message)",
            crate::history::estimate_message_tokens(&history.messages()[0]),
        );
        assert_eq!(lines[4], history_row.render().trim_start_matches('\n'));
        assert!(lines[5].starts_with("    user text "), "{output}");

        // The total is the sum of the top-level rows; nested rows are a split
        // of `history`, not an addition to it.
        let tokens = |line: &str| -> u64 {
            let field = line
                .split_whitespace()
                .find(|f| f.starts_with('~'))
                .unwrap();
            field[1..].parse().unwrap()
        };
        let top: u64 = lines[1..5].iter().map(|line| tokens(line)).sum();
        assert_eq!(
            lines[6],
            Row::top("total", top).render().trim_start_matches('\n')
        );
        // No provider response yet, so nothing measured to show beside it.
        assert_eq!(lines[7..], ["window: 200000 tokens"]);
    }

    #[test]
    fn run_shows_the_provider_measured_size_once_one_exists() {
        let cfg = crate::tools::testutil::TestConfig::new("context-cmd-anchor")
            .provider(kloop_provider::Provider::mock(vec![]))
            .models("test-model", &["test-model"])
            .context_window(None)
            .build();
        let mut history = History::new(cfg.offload_dir.clone());
        history.note_usage(12_345);
        let output = run(&history, &cfg).output;
        assert_eq!(
            output.lines().last(),
            Some("provider-measured: ~12345 tokens (last reported prompt plus what came after)")
        );
    }
}
