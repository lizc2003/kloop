//! Human-readable tool rows for the transcript (plan 38 slice 2).
//!
//! A tool call renders as a status-marked header — `● Bash $ ls -la`,
//! `Read src/main.rs`, `Write notes.txt (12 lines)`, `Grep TODO in src`, an MCP
//! `server__tool` verbatim — over a preview of its result indented under a `└`
//! gutter (double-limited by lines and chars, the rest left in the session).
//! `edit_file` shows a one-line `- old` / `+ new` diff from its input instead.
//!
//! Pure formatting from `(name, input-json, status, output)` to styled lines, so
//! it is unit-tested without a terminal. The input arrives as the raw JSON
//! string (see [`crate::events::AgentEvent::ToolStart`]); this module parses it.

use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

use crate::app::ToolStatus;
use crate::text_layout::display_width;
use crate::text_layout::truncate;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
/// Preview budget under a tool row: at most this many lines and characters, then
/// a "full result in session" hint. Kept small — the transcript is a summary,
/// the full output lives in history/offload.
const PREVIEW_MAX_LINES: usize = 5;
const PREVIEW_MAX_CHARS: usize = 400;
/// Left gutter width so the preview lines up under the header's label column.
const GUTTER: &str = "  └ ";
const GUTTER_CONT: &str = "    ";

/// The full display of one tool cell: the header row plus its preview/diff.
pub fn tool_cell_lines(
    name: &str,
    input: &str,
    status: ToolStatus,
    output: Option<&str>,
    width: usize,
) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines = vec![header_line(name, input, status, width)];
    // An edit shows the change it is making (from its input); every other tool
    // previews its result.
    if name == "edit_file" {
        lines.extend(edit_diff_lines(input, width));
    } else if let Some(out) = output {
        lines.extend(preview_lines(out, status == ToolStatus::Failed, width));
    }
    lines
}

/// `● Verb detail` — the mark colours by status, the verb is bold, the detail dim
/// and truncated to fit.
fn header_line(name: &str, input: &str, status: ToolStatus, width: usize) -> Line<'static> {
    let (mark, color) = tool_mark(status);
    let (verb, detail) = tool_label(name, input);
    let mut spans = vec![
        Span::styled(format!("{mark} "), Style::new().fg(color)),
        Span::styled(verb.clone(), Style::new().add_modifier(Modifier::BOLD)),
    ];
    if !detail.is_empty() {
        // mark+space (2) + verb + one space before the detail.
        let used = 2 + display_width(&verb) + 1;
        spans.push(Span::raw(" ".to_string()));
        spans.push(Span::styled(
            truncate(&clean(&detail), width.saturating_sub(used).max(1)),
            DIM,
        ));
    }
    Line::from(spans)
}

fn tool_mark(status: ToolStatus) -> (&'static str, Color) {
    match status {
        // Running is a cyan status indicator (styles.md), not yellow.
        ToolStatus::Running => ("●", Color::Cyan),
        ToolStatus::Ok => ("✓", Color::Green),
        ToolStatus::Failed => ("✗", Color::Red),
    }
}

/// Map a tool call to a human verb and a one-line argument detail. Well-known
/// tools get a friendly verb and their key argument; anything else (MCP
/// `server__tool`, less common builtins) keeps its raw name with the input
/// compacted to one line.
fn tool_label(name: &str, input: &str) -> (String, String) {
    let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match name {
        "bash" => {
            let cmd = s("command");
            let bg = v
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let detail = if bg {
                format!("$ {cmd}  (background)")
            } else {
                format!("$ {cmd}")
            };
            ("Bash".into(), detail)
        }
        "powershell" => ("PowerShell".into(), format!("PS> {}", s("command"))),
        "read_file" => ("Read".into(), path_of(&v)),
        "write_file" => {
            let n = v
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |c| c.lines().count().max(1));
            ("Write".into(), format!("{} ({n} lines)", path_of(&v)))
        }
        "edit_file" => ("Edit".into(), path_of(&v)),
        "grep" => {
            let pat = s("pattern");
            match v.get("path").and_then(Value::as_str) {
                Some(p) if !p.is_empty() => ("Grep".into(), format!("{pat}  in {p}")),
                _ => ("Grep".into(), pat),
            }
        }
        "glob" => ("Glob".into(), s("pattern")),
        "web_fetch" => ("Fetch".into(), s("url")),
        "web_search" => ("Search".into(), s("query")),
        "read_offloaded" => ("Read".into(), s("path")),
        "bash_output" => ("BashOutput".into(), s("bash_id")),
        "stop_bash" => ("StopBash".into(), s("bash_id")),
        "run_agent" => {
            let description = s("description");
            let detail = if description.is_empty() {
                s("prompt")
            } else {
                description
            };
            ("Run Agent".into(), detail)
        }
        "run_program" => {
            let description = s("description");
            let detail = if description.is_empty() {
                s("source")
            } else {
                description
            };
            ("Run Program".into(), detail)
        }
        "workflow" => {
            let script_path = s("script_path");
            let detail = if script_path.is_empty() {
                "inline script".into()
            } else {
                script_path
            };
            ("Workflow".into(), detail)
        }
        "wait_for_activity" => ("Wait for activity".into(), String::new()),
        "stop_agent" => ("Stop Agent".into(), s("agent_id")),
        "stop_program" => ("Stop Program".into(), s("program_id")),
        "stop_workflow" => ("Stop Workflow".into(), s("workflow_id")),
        "tool_search" => ("ToolSearch".into(), s("query")),
        "skill" => ("Skill".into(), s("name")),
        // call_tool wraps a real tool name; show that so the row reads as the
        // tool it actually runs.
        "call_tool" => {
            let inner = s("tool_name");
            let detail = v.get("params").map(compact_value).unwrap_or_default();
            (
                if inner.is_empty() {
                    "Tool".into()
                } else {
                    inner
                },
                detail,
            )
        }
        // Everything else keeps its own name; the input, compacted, is the detail.
        other => (other.to_string(), compact_value(&v)),
    }
}

/// A one-line `verb detail` preview of a tool call, for the folded sub-agent row
/// (which shows the latest call inline rather than a full cell).
pub fn tool_preview(name: &str, input: &str) -> String {
    let (verb, detail) = tool_label(name, input);
    if detail.is_empty() {
        verb
    } else {
        format!("{verb} {detail}")
    }
}

/// The `- old` / `+ new` first-line diff of an edit, from its input.
fn edit_diff_lines(input: &str, width: usize) -> Vec<Line<'static>> {
    let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let old = first_line(v.get("old_string").and_then(Value::as_str).unwrap_or(""));
    let new = first_line(v.get("new_string").and_then(Value::as_str).unwrap_or(""));
    if old.is_empty() && new.is_empty() {
        return Vec::new();
    }
    let content_w = width.saturating_sub(display_width(GUTTER) + 2).max(1);
    vec![
        diff_line(GUTTER, "- ", old, Color::Red, content_w),
        diff_line(GUTTER_CONT, "+ ", new, Color::Green, content_w),
    ]
}

fn diff_line(gutter: &str, sign: &str, text: &str, color: Color, width: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled(gutter.to_string(), DIM),
        Span::styled(
            format!("{sign}{}", truncate(&clean(text), width)),
            Style::new().fg(color),
        ),
    ])
}

/// The result preview under a tool row: the first few lines, each truncated to
/// width, double-limited by line count and total chars, with a hint when more
/// was cut. Errors render red, normal output dim.
fn preview_lines(out: &str, is_error: bool, width: usize) -> Vec<Line<'static>> {
    let body = out.trim_end_matches(['\n', ' ', '\t']);
    if body.trim().is_empty() {
        return Vec::new();
    }
    let content_w = width.saturating_sub(display_width(GUTTER)).max(1);
    let style = if is_error {
        Style::new().fg(Color::Red)
    } else {
        DIM
    };
    let src: Vec<&str> = body.split('\n').collect();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut chars_used = 0;
    for raw in &src {
        if lines.len() >= PREVIEW_MAX_LINES
            || (!lines.is_empty() && chars_used >= PREVIEW_MAX_CHARS)
        {
            break;
        }
        let text = truncate(&clean(raw), content_w);
        chars_used += text.chars().count();
        let gutter = if lines.is_empty() {
            GUTTER
        } else {
            GUTTER_CONT
        };
        lines.push(Line::from(vec![
            Span::styled(gutter.to_string(), DIM),
            Span::styled(text, style),
        ]));
    }
    if src.len() > lines.len() {
        lines.push(Line::from(Span::styled(
            format!("{GUTTER_CONT}… full result in session"),
            DIM,
        )));
    }
    lines
}

/// The "path" argument, tolerating the `file_path` alias some MCP tools use.
fn path_of(v: &Value) -> String {
    v.get("path")
        .or_else(|| v.get("file_path"))
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

/// Compact a JSON value to a single line for a generic tool detail: a lone
/// string field shows bare, otherwise the whitespace-collapsed JSON.
fn compact_value(v: &Value) -> String {
    if let Value::String(s) = v {
        return s.clone();
    }
    v.to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn first_line(text: &str) -> &str {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("")
}

/// Make a snippet safe for a single transcript row: tabs (read_file numbers its
/// lines `{n}\t{line}`, and code indents with them) collapse to a space instead
/// of ratatui's zero-width overlap, and any other control char is dropped.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c == '\t' { ' ' } else { c })
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }
    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(text).collect()
    }

    #[test]
    fn bash_row_shows_command_with_status_mark() {
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"ls -la"}"#,
            ToolStatus::Running,
            None,
            40,
        );
        assert_eq!(texts(&lines), vec!["● Bash $ ls -la"]);
        // Running mark is cyan (status indicator), verb bold.
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Cyan));
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn powershell_row_keeps_the_original_script() {
        let lines = tool_cell_lines(
            "powershell",
            r#"{"command":"Write-Output '你好'"}"#,
            ToolStatus::Running,
            None,
            60,
        );
        assert_eq!(texts(&lines), vec!["● PowerShell PS> Write-Output '你好'"]);
    }

    #[test]
    fn background_bash_is_flagged() {
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"sleep 9","background":true}"#,
            ToolStatus::Ok,
            Some("bg-1 started"),
            60,
        );
        assert_eq!(text(&lines[0]), "✓ Bash $ sleep 9  (background)");
    }

    #[test]
    fn read_write_grep_labels() {
        let read = tool_cell_lines(
            "read_file",
            r#"{"path":"src/main.rs"}"#,
            ToolStatus::Ok,
            None,
            40,
        );
        assert_eq!(text(&read[0]), "✓ Read src/main.rs");

        let write = tool_cell_lines(
            "write_file",
            r#"{"path":"a.txt","content":"one\ntwo\nthree"}"#,
            ToolStatus::Ok,
            None,
            40,
        );
        assert_eq!(text(&write[0]), "✓ Write a.txt (3 lines)");

        let grep = tool_cell_lines(
            "grep",
            r#"{"pattern":"TODO","path":"src"}"#,
            ToolStatus::Ok,
            None,
            40,
        );
        assert_eq!(text(&grep[0]), "✓ Grep TODO  in src");
    }

    #[test]
    fn edit_shows_first_line_diff_from_input() {
        let lines = tool_cell_lines(
            "edit_file",
            r#"{"path":"x.rs","old_string":"let x = 1;","new_string":"let x = 2;"}"#,
            ToolStatus::Ok,
            Some("applied"),
            40,
        );
        assert_eq!(
            texts(&lines),
            vec!["✓ Edit x.rs", "  └ - let x = 1;", "    + let x = 2;"]
        );
        // Deletion red, addition green.
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn output_preview_double_limits_and_hints_more() {
        let out = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"seq 20"}"#,
            ToolStatus::Ok,
            Some(&out),
            40,
        );
        // Header + PREVIEW_MAX_LINES preview + a "more" hint.
        assert_eq!(text(&lines[0]), "✓ Bash $ seq 20");
        assert_eq!(text(&lines[1]), "  └ line 1");
        assert_eq!(lines.len(), 1 + PREVIEW_MAX_LINES + 1);
        assert_eq!(text(lines.last().unwrap()), "    … full result in session");
    }

    #[test]
    fn failed_tool_previews_error_in_red() {
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"false"}"#,
            ToolStatus::Failed,
            Some("command failed: exit 1"),
            40,
        );
        assert_eq!(text(&lines[0]), "✗ Bash $ false");
        assert_eq!(text(&lines[1]), "  └ command failed: exit 1");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
    }

    #[test]
    fn empty_output_has_no_preview() {
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"true"}"#,
            ToolStatus::Ok,
            Some("   \n"),
            40,
        );
        assert_eq!(lines.len(), 1, "blank output shows no preview rows");
    }

    #[test]
    fn mcp_and_unknown_tools_keep_their_name() {
        let lines = tool_cell_lines(
            "srv__lookup",
            r#"{"q":"weather"}"#,
            ToolStatus::Ok,
            None,
            40,
        );
        assert_eq!(text(&lines[0]), r#"✓ srv__lookup {"q":"weather"}"#);
    }

    /// read_file numbers lines with a tab (`{n}\t{line}`); the preview collapses
    /// tabs to spaces (ratatui renders a tab zero-width) and drops other control
    /// chars, so the row stays aligned.
    #[test]
    fn preview_sanitizes_tabs_and_control_chars() {
        let out = "1\t[workspace]\n2\tresolver = \"2\"";
        let lines = tool_cell_lines(
            "read_file",
            r#"{"path":"Cargo.toml"}"#,
            ToolStatus::Ok,
            Some(out),
            40,
        );
        assert_eq!(text(&lines[0]), "✓ Read Cargo.toml");
        assert_eq!(text(&lines[1]), "  └ 1 [workspace]");
        assert_eq!(text(&lines[2]), "    2 resolver = \"2\"");
    }

    #[test]
    fn background_tools_use_product_labels_and_display_descriptions() {
        let rows = [
            (
                "run_agent",
                r#"{"prompt":"private long prompt","description":"inspect logs"}"#,
                "✓ Run Agent inspect logs",
            ),
            (
                "run_agent",
                r#"{"prompt":"fallback prompt"}"#,
                "✓ Run Agent fallback prompt",
            ),
            (
                "run_program",
                r#"{"source":"return privateSource","description":"compile assets"}"#,
                "✓ Run Program compile assets",
            ),
            (
                "run_program",
                r#"{"source":"return fallbackSource"}"#,
                "✓ Run Program return fallbackSource",
            ),
            (
                "workflow",
                r#"{"script":"return 1","description":"ignored top-level"}"#,
                "✓ Workflow inline script",
            ),
            (
                "workflow",
                r#"{"script_path":"/tmp/wf/script.js","description":"ignored"}"#,
                "✓ Workflow /tmp/wf/script.js",
            ),
            (
                "stop_agent",
                r#"{"agent_id":"agent-1"}"#,
                "✓ Stop Agent agent-1",
            ),
            (
                "stop_program",
                r#"{"program_id":"program-1"}"#,
                "✓ Stop Program program-1",
            ),
            (
                "stop_workflow",
                r#"{"workflow_id":"workflow-1"}"#,
                "✓ Stop Workflow workflow-1",
            ),
            (
                "wait_for_activity",
                r#"{"timeout_ms":30000}"#,
                "✓ Wait for activity",
            ),
        ];
        for (name, input, expected) in rows {
            let lines = tool_cell_lines(name, input, ToolStatus::Ok, None, 80);
            assert_eq!(text(&lines[0]), expected);
        }
    }

    #[test]
    fn long_detail_truncates_to_width() {
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"echo aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"}"#,
            ToolStatus::Ok,
            None,
            20,
        );
        let t = text(&lines[0]);
        assert!(t.starts_with("✓ Bash $ echo"), "{t}");
        assert!(t.ends_with('…'), "truncated: {t}");
    }
}
