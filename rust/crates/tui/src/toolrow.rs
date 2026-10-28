//! Human-readable tool rows for the transcript (plan 38 slice 2).
//!
//! A tool call renders as a status-marked header — `● Bash`, `Read src/main.rs`,
//! `Write notes.txt (12 lines)`, `Grep TODO in src`, an MCP `server__tool`
//! verbatim — over a preview of its result indented under a `└` gutter
//! (double-limited by lines and chars, the rest left in the session). A shell
//! tool puts the command it runs on a row of its own between the two.
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
/// The shell-command row sits between the header and the preview, its prompt in
/// the `└` column so a screen of shell calls reads as one list.
const COMMAND_GUTTER: &str = "  ";

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
    // The command a shell tool runs gets a row of its own: sharing the header
    // with the model's summary truncated it away exactly when it was longest.
    if let Some(command) = tool_command(name, input) {
        lines.push(command_line(&command, width));
    }
    // An edit shows the change it is making (from its input); every other tool
    // previews its result.
    if name == "edit_file" {
        lines.extend(edit_diff_lines(input, width));
    } else if let Some(out) = output {
        if name == "ask_user_question" && status != ToolStatus::Failed {
            lines.extend(ask_result_lines(input, out, width));
        } else {
            lines.extend(preview_lines(out, status == ToolStatus::Failed, width));
        }
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

/// Map a tool call to a human verb and a one-line argument detail. Every
/// built-in has an arm (`every_builtin_tool_has_a_row_of_its_own` guards that);
/// the fallback is for source tools — an MCP `server__tool` keeps the name the
/// user configured, with its input compacted to one line.
fn tool_label(name: &str, input: &str) -> (String, String) {
    let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
    match name {
        // The header carries the model's one-line summary (empty is fine); the
        // command itself is [`tool_command`]'s row underneath.
        "bash" => ("Bash".into(), s("description")),
        "powershell" => ("PowerShell".into(), s("description")),
        "read_file" => ("Read".into(), path_of(&v)),
        "write_file" => {
            let n = v
                .get("content")
                .and_then(Value::as_str)
                .map_or(0, |c| c.lines().count().max(1));
            ("Write".into(), format!("{} ({n} lines)", path_of(&v)))
        }
        "edit_file" => ("Edit".into(), path_of(&v)),
        "notebook_edit" => {
            let mode = v
                .get("edit_mode")
                .and_then(Value::as_str)
                .unwrap_or("replace");
            let path = v
                .get("notebook_path")
                .and_then(Value::as_str)
                .unwrap_or("?");
            ("Notebook".into(), format!("{path} ({mode})"))
        }
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
        "bash_output" => ("Output".into(), s("bash_id")),
        "stop_bash" => ("Stop".into(), s("bash_id")),
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
        // The mailbox: who it goes to, and the summary the sender wrote for
        // exactly this kind of one-line preview.
        "send_message" => {
            let to = s("to");
            let summary = s("summary");
            let detail = if summary.is_empty() {
                to
            } else {
                format!("{to}  {summary}")
            };
            ("Message".into(), detail)
        }
        "list_agents" => ("List Agents".into(), String::new()),
        // The panel above the composer already carries the list; the row only
        // has to say how long it now is. The verb is just `Todo`: `write` is the
        // tail of the raw tool name, so it stays in the dim detail instead of
        // wearing the bold the verb reserves for the tool it is.
        "todo_write" => {
            let detail = match v.get("todos").and_then(Value::as_array).map_or(0, Vec::len) {
                0 => "write cleared".into(),
                1 => "write 1 todo".into(),
                count => format!("write {count} todos"),
            };
            ("Todo".into(), detail)
        }
        "cron_create" => ("Cron create".into(), s("cron")),
        "cron_delete" => ("Cron delete".into(), s("id")),
        "cron_list" => ("Cron list".into(), String::new()),
        "schedule_wakeup" => {
            let detail = if v.get("stop").and_then(Value::as_bool).unwrap_or(false) {
                "stop".into()
            } else {
                let delay = v
                    .get("delay_seconds")
                    .and_then(Value::as_f64)
                    .map(|seconds| format!("{}s", seconds.round() as i64))
                    .unwrap_or_default();
                format!("{delay}  {}", s("reason")).trim().to_string()
            };
            ("Schedule wakeup".into(), detail)
        }
        // The panel's short title, not the question text: that runs to 1000
        // characters and truncates mid-sentence on a row, while the header is
        // written to identify the question in one line.
        "ask_user_question" => {
            let questions = v.get("questions").and_then(Value::as_array);
            let detail = match questions.map(Vec::as_slice) {
                Some([one]) => one
                    .get("header")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
                Some(many) if !many.is_empty() => format!("{} questions", many.len()),
                _ => String::new(),
            };
            ("Ask".into(), detail)
        }
        "enter_plan_mode" => ("Enter plan mode".into(), String::new()),
        // The plan itself is the result preview under the row, not the detail.
        "exit_plan_mode" => ("Exit plan mode".into(), String::new()),
        "enter_worktree" => {
            let name = s("name");
            let detail = if name.is_empty() { s("path") } else { name };
            ("Enter worktree".into(), detail)
        }
        "exit_worktree" => ("Exit worktree".into(), s("action")),
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

/// The shell line a `bash` / `powershell` call runs, prompt included; `None` for
/// every other tool. Kept out of [`tool_label`] so the command owns a full-width
/// row of its own instead of competing with the summary for the header.
fn tool_command(name: &str, input: &str) -> Option<String> {
    let v: Value = serde_json::from_str(input).unwrap_or(Value::Null);
    // A call still streaming its arguments has no command yet; an empty `$ ` row
    // would be a row that says nothing.
    let command = v.get("command").and_then(Value::as_str)?;
    match name {
        "bash" => {
            let background = if v
                .get("background")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "  (background)"
            } else {
                ""
            };
            Some(format!("$ {command}{background}"))
        }
        "powershell" => Some(format!("PS> {command}")),
        _ => None,
    }
}

/// The shell-command row: dim, indented to the preview's gutter, one line.
pub(crate) fn command_line(command: &str, width: usize) -> Line<'static> {
    let content_w = width.saturating_sub(display_width(COMMAND_GUTTER)).max(1);
    Line::from(vec![
        Span::raw(COMMAND_GUTTER.to_string()),
        Span::styled(truncate(&clean(command), content_w), DIM),
    ])
}

/// A one-line `verb detail` preview of a tool call, for the folded sub-agent row
/// (which shows the latest call inline rather than a full cell). One row is all
/// there is here, so a shell command joins the line instead of taking its own.
pub fn tool_preview(name: &str, input: &str) -> String {
    let (verb, detail) = tool_label(name, input);
    [verb, detail, tool_command(name, input).unwrap_or_default()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
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

/// The result text core writes for a cancelled question, matched so the row can
/// say that in one line instead of previewing the model-facing instructions.
const CANCELLED_RESULT: &str = "The user cancelled the questions without answering.";

/// What core puts between an answer and the preview the user saw for it.
const SELECTED_PREVIEW: &str = " selected preview:\n";

/// One answered question as the row shows it: the question's own short header
/// and the labels (plus any typed-in text) the user chose.
struct AskAnswer {
    header: String,
    answer: String,
}

/// What came of the question, from the result core wrote: the answers, a
/// one-line cancel, or — when the result is neither — the plain output preview.
fn ask_result_lines(input: &str, out: &str, width: usize) -> Vec<Line<'static>> {
    if let Some(answers) = ask_answers(input, out) {
        let named = answers.len() > 1;
        return answers
            .iter()
            .enumerate()
            .map(|(index, answer)| {
                let gutter = if index == 0 { GUTTER } else { GUTTER_CONT };
                let label = if named {
                    Some(answer.header.as_str())
                } else {
                    None
                };
                answer_line(gutter, label, &answer.answer, width)
            })
            .collect();
    }
    if out.trim_start().starts_with(CANCELLED_RESULT) {
        return vec![answer_line(GUTTER, None, "cancelled — no answer", width)];
    }
    preview_lines(out, false, width)
}

/// Pair every question in the call's input with the answer core wrote into the
/// result. The answer is the JSON string that follows `"<question>"=`, so the
/// match rests on the question text the call already carries rather than on the
/// sentence around it. `None` when the result is not an answered form (an error,
/// or a bounded result that lost its tail) and the row should fall back.
fn ask_answers(input: &str, out: &str) -> Option<Vec<AskAnswer>> {
    let v: Value = serde_json::from_str(input).ok()?;
    let questions = v.get("questions")?.as_array()?;
    if questions.is_empty() {
        return None;
    }
    let mut read = 0;
    let mut answers = Vec::with_capacity(questions.len());
    for (position, question) in questions.iter().enumerate() {
        let text = question.get("question")?.as_str()?;
        let needle = format!("{}=", serde_json::to_string(text).ok()?);
        let at = read + out.get(read..)?.find(&needle)? + needle.len();
        let mut values = serde_json::Deserializer::from_str(out.get(at..)?).into_iter::<String>();
        let answer = values.next()?.ok()?;
        read = at + values.byte_offset();
        // A selected preview is free text copied out of the option, so it can
        // quote a later question verbatim — including its `"<question>"=`.
        // Skip it by the text the input already carries before searching for
        // the next answer, or that search would read the preview's copy.
        if position + 1 < questions.len()
            && let Some(body) = out.get(read..)?.strip_prefix(SELECTED_PREVIEW)
        {
            let preview = question
                .get("options")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|option| option.get("preview").and_then(Value::as_str))
                .filter(|preview| body.starts_with(preview))
                .max_by_key(|preview| preview.len())?;
            read += SELECTED_PREVIEW.len() + preview.len();
        }
        let header = question
            .get("header")
            .and_then(Value::as_str)
            .filter(|header| !header.trim().is_empty())
            .unwrap_or(text)
            .to_string();
        answers.push(AskAnswer { header, answer });
    }
    Some(answers)
}

/// One answer row. Multiple questions name the question first, dim, and the
/// choice after it in the row's own weight, so the eye lands on what was chosen
/// rather than on the boilerplate the result sentence used to start with.
fn answer_line(gutter: &str, label: Option<&str>, answer: &str, width: usize) -> Line<'static> {
    let content_w = width.saturating_sub(display_width(GUTTER)).max(1);
    let mut spans = vec![Span::styled(gutter.to_string(), DIM)];
    let mut budget = content_w;
    if let Some(label) = label {
        // Half the row at most: a long header must leave room for the answer.
        let label = truncate(&format!("{} → ", clean(label)), (content_w / 2).max(1));
        budget = content_w.saturating_sub(display_width(&label)).max(1);
        spans.push(Span::styled(label, DIM));
    }
    spans.push(Span::raw(truncate(&clean(answer), budget)));
    Line::from(spans)
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

/// Make a snippet safe for a single transcript row: every whitespace char (tabs,
/// which `read_file` numbers its lines with and code indents with; newlines,
/// which a heredoc command is full of) collapses to a space instead of ratatui's
/// zero-width overlap or — for a newline — silently gluing the two sides
/// together. Any other control char is dropped.
fn clean(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
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
        assert_eq!(texts(&lines), vec!["● Bash", "  $ ls -la"]);
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
        assert_eq!(
            texts(&lines),
            vec!["● PowerShell", "  PS> Write-Output '你好'"]
        );
    }

    #[test]
    fn powershell_row_shows_the_description_above_the_script() {
        let lines = tool_cell_lines(
            "powershell",
            r#"{"command":"Get-Location","description":"Check the path"}"#,
            ToolStatus::Ok,
            None,
            60,
        );
        assert_eq!(
            texts(&lines),
            vec!["✓ PowerShell Check the path", "  PS> Get-Location"]
        );
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
        assert_eq!(
            texts(&lines[..2]),
            vec!["✓ Bash", "  $ sleep 9  (background)"]
        );
    }

    /// A raw `find … -exec` is what the user must otherwise parse; the model's
    /// summary heads the row and the command keeps a full-width row of its own
    /// underneath, described or not.
    #[test]
    fn bash_row_leads_with_the_description_and_keeps_the_command_below() {
        let described = tool_cell_lines(
            "bash",
            r#"{"command":"find . -name '*.tmp' -delete","description":"Delete temp files"}"#,
            ToolStatus::Ok,
            None,
            80,
        );
        assert_eq!(
            texts(&described),
            vec![
                "✓ Bash Delete temp files",
                "  $ find . -name '*.tmp' -delete"
            ]
        );

        let background = tool_cell_lines(
            "bash",
            r#"{"command":"sleep 9","background":true,"description":"Wait a bit"}"#,
            ToolStatus::Ok,
            None,
            80,
        );
        assert_eq!(
            texts(&background),
            vec!["✓ Bash Wait a bit", "  $ sleep 9  (background)"]
        );

        let plain = tool_cell_lines(
            "bash",
            r#"{"command":"find . -name '*.tmp' -delete"}"#,
            ToolStatus::Ok,
            None,
            80,
        );
        assert_eq!(
            texts(&plain),
            vec!["✓ Bash", "  $ find . -name '*.tmp' -delete"]
        );
    }

    /// The long command used to lose its tail to the header's truncation and its
    /// newlines to `clean`, which glued `EOF` onto the next line's first word.
    /// Now it owns the full width, and a newline reads as the space it is.
    #[test]
    fn a_heredoc_command_keeps_full_width_and_does_not_glue_its_lines() {
        let lines = tool_cell_lines(
            "bash",
            r#"{"command":"cat > go.mod <<'EOF'\nmodule check451b\nEOF","description":"Experiment: ClientCode survives the 451 rename"}"#,
            ToolStatus::Ok,
            None,
            60,
        );
        assert_eq!(
            texts(&lines),
            vec![
                "✓ Bash Experiment: ClientCode survives the 451 rename",
                "  $ cat > go.mod <<'EOF' module check451b EOF",
            ]
        );
    }

    /// The folded sub-agent row has one line for everything, so there the
    /// command joins the summary instead of taking a row of its own.
    #[test]
    fn folded_preview_keeps_summary_and_command_on_one_line() {
        assert_eq!(
            tool_preview(
                "bash",
                r#"{"command":"sleep 9","background":true,"description":"Wait a bit"}"#
            ),
            "Bash Wait a bit $ sleep 9  (background)"
        );
        assert_eq!(tool_preview("bash", r#"{"command":"ls"}"#), "Bash $ ls");
        assert_eq!(
            tool_preview("read_file", r#"{"path":"src/main.rs"}"#),
            "Read src/main.rs"
        );
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
        // Header + command + PREVIEW_MAX_LINES preview + a "more" hint.
        assert_eq!(texts(&lines[..2]), vec!["✓ Bash", "  $ seq 20"]);
        assert_eq!(text(&lines[2]), "  └ line 1");
        assert_eq!(lines.len(), 2 + PREVIEW_MAX_LINES + 1);
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
        assert_eq!(texts(&lines[..2]), vec!["✗ Bash", "  $ false"]);
        assert_eq!(text(&lines[2]), "  └ command failed: exit 1");
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        assert_eq!(lines[2].spans[1].style.fg, Some(Color::Red));
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
        assert_eq!(
            texts(&lines),
            vec!["✓ Bash", "  $ true"],
            "blank output shows no preview rows"
        );
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

    /// The rows that used to fall through to the raw-name fallback: a session
    /// tool rendered as `todo_write {"todos":[…]}` told the
    /// user less than its own name would have.
    #[test]
    fn session_tools_render_as_a_verb_and_the_thing_they_act_on() {
        let rows = [
            (
                "notebook_edit",
                r#"{"notebook_path":"analysis.ipynb","cell_id":"c1","new_source":"1+1"}"#,
                "✓ Notebook analysis.ipynb (replace)",
            ),
            (
                "notebook_edit",
                r#"{"notebook_path":"analysis.ipynb","edit_mode":"insert","cell_type":"code","new_source":"1+1"}"#,
                "✓ Notebook analysis.ipynb (insert)",
            ),
            (
                "send_message",
                r#"{"to":"agent-2","summary":"close race","message":"the long body"}"#,
                "✓ Message agent-2  close race",
            ),
            (
                "send_message",
                r#"{"to":"main","message":"the long body"}"#,
                "✓ Message main",
            ),
            ("list_agents", "{}", "✓ List Agents"),
            (
                "todo_write",
                r#"{"todos":[{"subject":"drain the queue","status":"in_progress"}]}"#,
                "✓ Todo write 1 todo",
            ),
            (
                "todo_write",
                r#"{"todos":[{"subject":"a","status":"completed"},{"subject":"b","status":"pending"}]}"#,
                "✓ Todo write 2 todos",
            ),
            ("todo_write", r#"{"todos":[]}"#, "✓ Todo write cleared"),
            (
                "cron_create",
                r#"{"cron":"0 9 * * 1","prompt":"weekly sweep"}"#,
                "✓ Cron create 0 9 * * 1",
            ),
            ("cron_delete", r#"{"id":"cron-2"}"#, "✓ Cron delete cron-2"),
            ("cron_list", "{}", "✓ Cron list"),
            (
                "schedule_wakeup",
                r#"{"delay_seconds":1200,"reason":"watching CI","prompt":"/loop check"}"#,
                "✓ Schedule wakeup 1200s  watching CI",
            ),
            (
                "schedule_wakeup",
                r#"{"stop":true}"#,
                "✓ Schedule wakeup stop",
            ),
            (
                "ask_user_question",
                r#"{"questions":[{"question":"Which rail?","header":"Rail","options":[]}]}"#,
                "✓ Ask Rail",
            ),
            ("enter_plan_mode", "{}", "✓ Enter plan mode"),
            (
                "exit_plan_mode",
                r#"{"plan":"a very long markdown plan"}"#,
                "✓ Exit plan mode",
            ),
            (
                "enter_worktree",
                r#"{"name":"feature-x"}"#,
                "✓ Enter worktree feature-x",
            ),
            (
                "enter_worktree",
                r#"{"path":".kloop/worktrees/old"}"#,
                "✓ Enter worktree .kloop/worktrees/old",
            ),
            (
                "exit_worktree",
                r#"{"action":"remove","discard_changes":true}"#,
                "✓ Exit worktree remove",
            ),
        ];
        for (name, input, expected) in rows {
            let lines = tool_cell_lines(name, input, ToolStatus::Ok, None, 80);
            assert_eq!(text(&lines[0]), expected, "{name}");
        }
    }

    /// `Todo` is the verb, so it is bold; `write` is the tail of the raw
    /// `todo_write` name and stays dim with the count — the row reads the same
    /// as before, but nothing past `Todo` wears the verb's emphasis.
    #[test]
    fn todo_row_highlights_only_the_verb() {
        let lines = tool_cell_lines(
            "todo_write",
            r#"{"todos":[{"subject":"a","status":"pending"}]}"#,
            ToolStatus::Ok,
            None,
            80,
        );
        assert_eq!(text(&lines[0]), "✓ Todo write 1 todo");
        assert_eq!(lines[0].spans[1].content.as_ref(), "Todo");
        assert!(
            lines[0].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[3].content.as_ref(), "write 1 todo");
        assert!(
            !lines[0].spans[3]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    /// The result sentence is written for the model, and it quotes the full
    /// question text before the choice — on a row that made the answer, the one
    /// thing worth reading, the first thing truncated away. The row now names
    /// the question by its own short header and shows what was picked.
    #[test]
    fn answered_question_shows_the_choice_under_its_header() {
        let input = r#"{"questions":[{"question":"Which layout?","header":"Layout","options":[{"label":"Rows","description":"stack"},{"label":"Columns","description":"split"}],"multi_select":false}]}"#;
        let out = "Your questions have been answered: \"Which layout?\"=\"Columns\". You can now continue with these answers in mind.";
        let lines = tool_cell_lines("ask_user_question", input, ToolStatus::Ok, Some(out), 80);
        assert_eq!(texts(&lines), vec!["✓ Ask Layout", "  └ Columns"]);
    }

    /// Several questions get a row each: the header on every line, the choice
    /// in the row's own weight. A chosen preview follows the answer in the
    /// result and must not leak into the next line.
    #[test]
    fn every_answered_question_gets_a_line_and_ignores_the_selected_preview() {
        let input = r#"{"questions":[
            {"question":"Which layout?","header":"Layout","options":[{"label":"Rows","description":"stack"},{"label":"Columns","description":"split","preview":"COL"}]},
            {"question":"Which colors?","header":"Colors","options":[{"label":"Red","description":"warm"},{"label":"Blue","description":"cool"}]}
        ]}"#;
        let out = "The user answered: \"Which layout?\"=\"Columns\" selected preview:\nCOL, \"Which colors?\"=\"Red, my own words\". Read the answers carefully — they may request clarification, changes, or that you not proceed — and follow what they actually say.";
        let lines = tool_cell_lines("ask_user_question", input, ToolStatus::Ok, Some(out), 80);
        assert_eq!(
            texts(&lines),
            vec![
                "✓ Ask 2 questions",
                "  └ Layout → Columns",
                "    Colors → Red, my own words",
            ]
        );
        // The choice is not dimmed like a tool's output preview.
        assert!(lines[1].spans.last().unwrap().style.add_modifier.is_empty());
    }

    /// The question text is quoted into the result as JSON, so a question that
    /// itself contains quotes and newlines is still paired with its answer.
    #[test]
    fn a_question_with_quotes_and_newlines_still_pairs_with_its_answer() {
        let input =
            r#"{"questions":[{"question":"Use \"v2\"\nnow?","header":"Version","options":[]}]}"#;
        let out = "Your questions have been answered: \"Use \\\"v2\\\"\\nnow?\"=\"Yes\". You can now continue with these answers in mind.";
        let lines = tool_cell_lines("ask_user_question", input, ToolStatus::Ok, Some(out), 80);
        assert_eq!(texts(&lines), vec!["✓ Ask Version", "  └ Yes"]);
    }

    /// A selected preview is free text copied from the option, so it can quote
    /// the next question — the pairing must skip it by its known text instead
    /// of finding the next `"<question>"=` inside it and showing that instead
    /// of what the user picked.
    #[test]
    fn a_preview_quoting_the_next_question_does_not_become_its_answer() {
        let input = r#"{"questions":[
            {"question":"Which layout?","header":"Layout","options":[
                {"label":"Rows","description":"stack","preview":"see \"Which colors?\"=\"Red\" above"},
                {"label":"Columns","description":"split"}
            ]},
            {"question":"Which colors?","header":"Colors","options":[
                {"label":"Red","description":"warm"},
                {"label":"Blue","description":"cool"}
            ]}
        ]}"#;
        let out = "Your questions have been answered: \"Which layout?\"=\"Rows\" selected preview:\nsee \"Which colors?\"=\"Red\" above, \"Which colors?\"=\"Blue\". You can now continue with these answers in mind.";
        let lines = tool_cell_lines("ask_user_question", input, ToolStatus::Ok, Some(out), 80);
        assert_eq!(
            texts(&lines),
            vec![
                "✓ Ask 2 questions",
                "  └ Layout → Rows",
                "    Colors → Blue",
            ]
        );
    }

    #[test]
    fn a_cancelled_question_says_so_in_one_line() {
        let input =
            r#"{"questions":[{"question":"Which layout?","header":"Layout","options":[]}]}"#;
        let out = "The user cancelled the questions without answering. Do not assume a choice; continue only if a safe default follows from existing requirements, otherwise explain what remains blocked.";
        let lines = tool_cell_lines("ask_user_question", input, ToolStatus::Ok, Some(out), 80);
        assert_eq!(
            texts(&lines),
            vec!["✓ Ask Layout", "  └ cancelled — no answer"]
        );
    }

    /// A failed call — no questioner in this frontend, a validation error — is
    /// not an answer; it keeps the ordinary (red) output preview.
    #[test]
    fn a_failed_question_keeps_the_error_preview() {
        let input =
            r#"{"questions":[{"question":"Which layout?","header":"Layout","options":[]}]}"#;
        let out = "ask_user_question: no interactive user is available in this frontend";
        let lines = tool_cell_lines(
            "ask_user_question",
            input,
            ToolStatus::Failed,
            Some(out),
            80,
        );
        assert_eq!(
            texts(&lines),
            vec![
                "✗ Ask Layout",
                "  └ ask_user_question: no interactive user is available in this frontend"
            ]
        );
        assert_eq!(lines[1].spans[1].style.fg, Some(Color::Red));
    }

    /// A long header gives way to the answer instead of pushing it off the row.
    #[test]
    fn a_long_question_header_leaves_room_for_the_answer() {
        let input = r#"{"questions":[
            {"question":"Which rail?","header":"A header that runs the whole width","options":[]},
            {"question":"Which colors?","header":"Colors","options":[]}
        ]}"#;
        let out = "Your questions have been answered: \"Which rail?\"=\"Left\", \"Which colors?\"=\"Red\". You can now continue with these answers in mind.";
        let lines = tool_cell_lines("ask_user_question", input, ToolStatus::Ok, Some(out), 40);
        assert!(display_width(&text(&lines[1])) <= 40);
        assert!(text(&lines[1]).contains("…"), "{}", text(&lines[1]));
        assert!(text(&lines[1]).ends_with("Left"), "{}", text(&lines[1]));
    }

    /// The fallback arm — raw name plus compacted JSON — is right for an MCP
    /// tool the user named themselves and wrong for every built-in. Nothing
    /// here is exempt: the guard walks the catalog, so a built-in added later
    /// fails this test until it has a row of its own.
    #[test]
    fn every_builtin_tool_has_a_row_of_its_own() {
        for name in kloop_core::tools::builtin_tool_names() {
            let (verb, _) = tool_label(name, "{}");
            assert_ne!(verb, name, "{name} renders as its raw name");
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
        let t = text(&lines[1]);
        assert!(t.starts_with("  $ echo"), "{t}");
        assert!(t.ends_with('…'), "truncated: {t}");
        assert_eq!(display_width(&t), 20, "the command row fills the width");
    }
}
