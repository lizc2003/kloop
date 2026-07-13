//! Rendering: pure functions from app state to lines (unit-tested), plus the
//! one ratatui draw entry point that owns layout and the confirm popup.

use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;
use ratatui::Frame;
use unicode_width::UnicodeWidthChar;

use kloop_core::tools::TodoStatus;

use crate::app::App;
use crate::app::Cell;
use crate::app::ToolStatus;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);

/// Hard-wrap `text` to `width` display columns (CJK chars count as 2),
/// breaking on newlines and at column boundaries. Never returns an empty vec.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut lines = Vec::new();
    for raw in text.split('\n') {
        let mut line = String::new();
        let mut cols = 0;
        for c in raw.chars() {
            let w = c.width().unwrap_or(0);
            if cols + w > width && !line.is_empty() {
                lines.push(std::mem::take(&mut line));
                cols = 0;
            }
            line.push(c);
            cols += w;
        }
        lines.push(line);
    }
    lines
}

/// Truncate to `width` columns, appending `…` when anything was cut.
pub fn truncate(text: &str, width: usize) -> String {
    let width = width.max(1);
    let mut cols = 0;
    let mut out = String::new();
    for c in text.chars() {
        let w = c.width().unwrap_or(0);
        if cols + w > width.saturating_sub(1) {
            // Might still fit whole if this is the last char; check cheaply.
            let rest_w: usize = text[out.len()..]
                .chars()
                .map(|c| c.width().unwrap_or(0))
                .sum();
            if cols + rest_w <= width {
                out.push_str(&text[out.len()..]);
                return out;
            }
            out.push('…');
            return out;
        }
        out.push(c);
        cols += w;
    }
    out
}

/// The transcript as display lines: the pure core of the UI. Tool calls
/// collapse to one status row; user/assistant text wraps to the width.
pub fn transcript_lines(cells: &[Cell], width: usize) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    for cell in cells {
        match cell {
            Cell::User(text) => {
                if !lines.is_empty() {
                    lines.push(Line::default());
                }
                for (i, l) in wrap(text, width.saturating_sub(2)).into_iter().enumerate() {
                    let prefix = if i == 0 { "> " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(prefix.to_string(), Style::new().fg(Color::Cyan)),
                        Span::styled(l, Style::new().add_modifier(Modifier::BOLD)),
                    ]));
                }
            }
            Cell::Assistant(text) => {
                for l in wrap(text, width) {
                    lines.push(Line::from(l));
                }
            }
            Cell::Thinking(text) => {
                // Collapsed to a one-line dim preview of the latest reasoning
                // line; the stream keeps it moving, the transcript stays calm.
                let last = text.lines().rev().find(|l| !l.trim().is_empty());
                lines.push(Line::from(Span::styled(
                    truncate(&format!("∴ {}", last.unwrap_or("thinking…")), width.max(2)),
                    DIM.add_modifier(Modifier::ITALIC),
                )));
            }
            Cell::Tool {
                name,
                summary,
                status,
            } => {
                let (mark, color) = match status {
                    ToolStatus::Running => ("…", Color::Yellow),
                    ToolStatus::Ok => ("✓", Color::Green),
                    ToolStatus::Failed => ("✗", Color::Red),
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::new().fg(color)),
                    Span::styled(
                        truncate(&format!("{name} {summary}"), width.saturating_sub(2)),
                        DIM,
                    ),
                ]));
            }
            Cell::Agent {
                agent,
                task,
                status,
                tools,
                last_tool,
            } => {
                let (mark, color) = match status {
                    ToolStatus::Running => ("…", Color::Yellow),
                    ToolStatus::Ok => ("✓", Color::Green),
                    ToolStatus::Failed => ("✗", Color::Red),
                };
                // Live: show what it is doing right now; done: a one-line
                // summary (its tool detail was never in the transcript).
                let body = if *status == ToolStatus::Running && *tools > 0 {
                    format!("{agent} {task} — {tools} tools · {last_tool}")
                } else if *status == ToolStatus::Running {
                    format!("{agent} {task}")
                } else {
                    format!("{agent} {task} ({tools} tool uses)")
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{mark} "), Style::new().fg(color)),
                    Span::styled(truncate(&body, width.saturating_sub(2)), DIM),
                ]));
            }
            Cell::Todo(items) => {
                lines.push(Line::from(Span::styled("todos".to_string(), DIM)));
                for item in items {
                    // in_progress shows its activeForm (what's happening now);
                    // the others show the plain content.
                    let (mark, color, text, style) = match item.status {
                        TodoStatus::Completed => ("✓", Color::Green, &item.content, DIM),
                        TodoStatus::InProgress => (
                            "▶",
                            Color::Yellow,
                            &item.active_form,
                            Style::new().add_modifier(Modifier::BOLD),
                        ),
                        TodoStatus::Pending => ("○", Color::DarkGray, &item.content, DIM),
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("  {mark} "), Style::new().fg(color)),
                        Span::styled(truncate(text, width.saturating_sub(4)), style),
                    ]));
                }
            }
            Cell::Note(text) => {
                lines.push(Line::from(Span::styled(
                    truncate(&format!("[{text}]"), width),
                    DIM,
                )));
            }
        }
    }
    lines
}

/// The input line, windowed so the cursor stays visible in `width` columns.
/// Returns the visible slice and the cursor's column within it.
pub fn input_view(input: &str, cursor: usize, width: usize) -> (String, u16) {
    let width = width.max(2);
    let chars: Vec<(char, usize)> = input.chars().map(|c| (c, c.width().unwrap_or(0))).collect();
    let cursor = cursor.min(chars.len());
    // Slide the window start right until the cursor fits inside width-1
    // columns (one column reserved so the cursor can sit past the last char).
    let mut start = 0;
    loop {
        let cursor_cols: usize = chars[start..cursor].iter().map(|(_, w)| w).sum();
        if cursor_cols < width || start >= cursor {
            break;
        }
        start += 1;
    }
    let mut out = String::new();
    let mut cols = 0;
    for (c, w) in &chars[start..] {
        if cols + w > width - 1 {
            break;
        }
        out.push(*c);
        cols += w;
    }
    let x: usize = chars[start..cursor].iter().map(|(_, w)| w).sum();
    (out, x as u16)
}

pub fn status_line(app: &App) -> String {
    if !app.confirms.is_empty() {
        return "awaiting approval".into();
    }
    if app.running {
        let note = app.last_note.as_deref().unwrap_or("");
        return format!("working… {note}  (Ctrl+C to interrupt)");
    }
    format!(
        "session {} — Enter to send · Ctrl+D to quit",
        app.session_id
    )
}

pub fn draw(f: &mut Frame, app: &mut App) {
    let [transcript_area, status_area, input_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let width = transcript_area.width as usize;
    let lines = transcript_lines(&app.cells, width.max(1));
    let height = transcript_area.height as usize;
    app.scroll_up = app.scroll_up.min(lines.len().saturating_sub(height));
    let end = lines.len() - app.scroll_up;
    let start = end.saturating_sub(height);
    f.render_widget(Paragraph::new(lines[start..end].to_vec()), transcript_area);

    f.render_widget(
        Paragraph::new(truncate(&status_line(app), width)).style(DIM),
        status_area,
    );

    let input_width = (input_area.width as usize).saturating_sub(2);
    let (visible, x) = input_view(&app.input, app.cursor, input_width.max(2));
    f.render_widget(Paragraph::new(format!("> {visible}")), input_area);

    if let Some(pending) = app.confirms.front() {
        draw_confirm(f, &pending.req, f.area());
    } else {
        f.set_cursor_position((input_area.x + 2 + x, input_area.y));
    }
}

fn draw_confirm(f: &mut Frame, req: &kloop_core::permissions::ConfirmRequest, area: Rect) {
    let options = match &req.remember_rules {
        Some(rules) => format!(
            "y allow once · a allow this session · p always ({}) · n deny",
            rules.join(", ")
        ),
        None => "y allow once · n deny".to_string(),
    };
    let popup_w = area.width.saturating_sub(4).clamp(20, 76);
    let inner_w = usize::from(popup_w - 2);
    let mut lines: Vec<Line> = wrap(&req.description, inner_w)
        .into_iter()
        .map(Line::from)
        .collect();
    if let Some(preview) = &req.preview {
        lines.push(Line::default());
        lines.extend(diff_preview_lines(preview, inner_w));
    }
    lines.push(Line::default());
    lines.extend(
        wrap(&options, inner_w)
            .into_iter()
            .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Yellow)))),
    );
    let popup_h = (lines.len() as u16 + 2).min(area.height);
    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h)) / 2,
        width: popup_w,
        height: popup_h,
    };
    f.render_widget(Clear, popup);
    f.render_widget(
        Paragraph::new(lines).block(Block::bordered().title("approve?")),
        popup,
    );
}

/// Color a file-change diff preview: additions green, deletions red, context
/// and markers dim. Each source line keeps its color across width wrapping.
fn diff_preview_lines(preview: &str, width: usize) -> Vec<Line<'static>> {
    preview
        .lines()
        .flat_map(|line| {
            let style = match line.chars().next() {
                Some('+') => Style::new().fg(Color::Green),
                Some('-') => Style::new().fg(Color::Red),
                _ => DIM,
            };
            wrap(line, width)
                .into_iter()
                .map(move |frag| Line::from(Span::styled(frag, style)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn wrap_handles_cjk_newlines_and_narrow_width() {
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap("ab\ncd", 10), vec!["ab", "cd"]);
        assert_eq!(wrap("", 10), vec![""]);
        // A double-width char never straddles the boundary.
        assert_eq!(wrap("a你b", 2), vec!["a", "你", "b"]);
        assert_eq!(wrap("abc", 0), vec!["a", "b", "c"]);
    }

    #[test]
    fn truncate_marks_cut_content() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello!", 5), "hell…");
        assert_eq!(truncate("你好世界", 5), "你好…");
    }

    #[test]
    fn diff_preview_colors_lines_by_sign() {
        let lines = diff_preview_lines(" 1  ctx\n-2  old\n+2  new\n⋮", 40);
        let got: Vec<(String, Style)> = lines
            .iter()
            .map(|l| (line_text(l), l.spans[0].style))
            .collect();
        assert_eq!(
            got,
            vec![
                (" 1  ctx".to_string(), DIM),
                ("-2  old".to_string(), Style::new().fg(Color::Red)),
                ("+2  new".to_string(), Style::new().fg(Color::Green)),
                ("⋮".to_string(), DIM),
            ]
        );
    }

    #[test]
    fn transcript_renders_all_cell_kinds() {
        let cells = vec![
            Cell::User("do the thing".into()),
            Cell::Tool {
                name: "bash".into(),
                summary: "{\"command\":\"ls\"}".into(),
                status: ToolStatus::Ok,
            },
            Cell::Tool {
                name: "bash".into(),
                summary: "{}".into(),
                status: ToolStatus::Running,
            },
            Cell::Note("compacting history".into()),
            Cell::Assistant("done.\nall good".into()),
        ];
        let lines = transcript_lines(&cells, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            texts,
            vec![
                "> do the thing",
                "✓ bash {\"command\":\"ls\"}",
                "… bash {}",
                "[compacting history]",
                "done.",
                "all good",
            ]
        );
    }

    /// Agent rows: running shows the live tool preview, finished collapses to
    /// a one-line summary with the tool count.
    #[test]
    fn agent_rows_render_live_preview_and_final_summary() {
        let cells = vec![
            Cell::Agent {
                agent: "agent-1".into(),
                task: "find the bug".into(),
                status: ToolStatus::Running,
                tools: 0,
                last_tool: String::new(),
            },
            Cell::Agent {
                agent: "agent-1".into(),
                task: "find the bug".into(),
                status: ToolStatus::Running,
                tools: 3,
                last_tool: "bash {\"command\":\"cargo test\"}".into(),
            },
            Cell::Agent {
                agent: "agent-1".into(),
                task: "find the bug".into(),
                status: ToolStatus::Ok,
                tools: 3,
                last_tool: "bash {\"command\":\"cargo test\"}".into(),
            },
        ];
        let lines = transcript_lines(&cells, 60);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            texts,
            vec![
                "… agent-1 find the bug",
                "… agent-1 find the bug — 3 tools · bash {\"command\":\"cargo t…",
                "✓ agent-1 find the bug (3 tool uses)",
            ]
        );
    }

    /// A Todo cell renders a header plus one marked line per item, showing
    /// the activeForm for the in_progress item and content for the rest.
    #[test]
    fn todo_cell_renders_a_marked_checklist() {
        use kloop_core::tools::TodoItem;
        let item = |content: &str, active: &str, status: TodoStatus| TodoItem {
            content: content.into(),
            active_form: active.into(),
            status,
        };
        let cells = vec![Cell::Todo(vec![
            item("Parse input", "Parsing input", TodoStatus::Completed),
            item("Run tests", "Running tests", TodoStatus::InProgress),
            item("Write docs", "Writing docs", TodoStatus::Pending),
        ])];
        let lines = transcript_lines(&cells, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            texts,
            vec![
                "todos",
                "  ✓ Parse input",
                "  ▶ Running tests",
                "  ○ Write docs",
            ]
        );
    }

    /// Thinking collapses to one dim line previewing the latest non-empty
    /// reasoning line, however long the accumulated text.
    #[test]
    fn thinking_collapses_to_last_line_preview() {
        let cells = vec![
            Cell::Thinking("first thought\nsecond thought\n  \n".into()),
            Cell::Thinking(String::new()),
        ];
        let lines = transcript_lines(&cells, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["∴ second thought", "∴ thinking…"]);
    }

    #[test]
    fn user_text_wraps_with_continuation_indent_and_spacing() {
        let cells = vec![
            Cell::Assistant("earlier".into()),
            Cell::User("aaaa bbbb".into()),
        ];
        let lines = transcript_lines(&cells, 8);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts, vec!["earlier", "", "> aaaa b", "  bbb"]);
    }

    #[test]
    fn long_tool_rows_collapse_to_one_truncated_line() {
        let cells = vec![Cell::Tool {
            name: "bash".into(),
            summary: "x".repeat(100),
            status: ToolStatus::Failed,
        }];
        let lines = transcript_lines(&cells, 20);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("✗ bash x"));
        assert!(text.ends_with('…'));
    }

    #[test]
    fn input_view_windows_around_the_cursor() {
        // Fits entirely.
        assert_eq!(input_view("abc", 1, 10), ("abc".into(), 1));
        // Cursor at the end of a long input: window shows the tail.
        let (visible, x) = input_view("abcdefghij", 10, 6);
        assert_eq!(visible, "fghij");
        assert_eq!(x, 5);
        // Cursor back at the start: window shows the head.
        let (visible, x) = input_view("abcdefghij", 0, 6);
        assert_eq!(visible, "abcde");
        assert_eq!(x, 0);
        // Wide chars count double.
        let (visible, x) = input_view("你好世界", 4, 5);
        assert_eq!(visible, "世界");
        assert_eq!(x, 4);
    }
}
