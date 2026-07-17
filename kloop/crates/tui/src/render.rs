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

/// The status glyph and colour shared by tool rows and sub-agent rows.
fn status_mark(status: &ToolStatus) -> (&'static str, Color) {
    match status {
        ToolStatus::Running => ("…", Color::Yellow),
        ToolStatus::Ok => ("✓", Color::Green),
        ToolStatus::Failed => ("✗", Color::Red),
    }
}

/// The display lines for one cell at `width` columns. Both paths that put a
/// cell on screen go through this — rendering the live tail in the viewport and
/// freezing a finalized cell into native scrollback (`insert_before`) — so a
/// cell looks identical either way. Tool calls collapse to one status row;
/// user/assistant text wraps to the width.
pub fn cell_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut lines: Vec<Line<'static>> = Vec::new();
    match cell {
        Cell::User(text) => {
            // A blank separator opens every user turn (between turns in
            // scrollback; a lone blank at the very top of a session is benign).
            lines.push(Line::default());
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
            let (mark, color) = status_mark(status);
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
            let (mark, color) = status_mark(status);
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
        Cell::System(text) => {
            // Slash-command output: dim, but wrapped in full (not collapsed
            // like a Note) since /help and /cost are multi-line.
            for l in wrap(text, width) {
                lines.push(Line::from(Span::styled(l, DIM)));
            }
        }
    }
    lines
}

/// The uncommitted tail as flat display lines (each cell via [`cell_lines`]).
pub fn transcript_lines(cells: &[Cell], width: usize) -> Vec<Line<'static>> {
    cells.iter().flat_map(|c| cell_lines(c, width)).collect()
}

/// A cell is committable once it can no longer change: everything except a tool
/// or sub-agent row that is still Running (its ✓/✗ has yet to land).
fn is_committable(cell: &Cell) -> bool {
    match cell {
        Cell::Tool { status, .. } | Cell::Agent { status, .. } => *status != ToolStatus::Running,
        _ => true,
    }
}

/// How many leading cells to freeze into scrollback so the uncommitted tail fits
/// an `active_h`-row viewport region. Commits only finalized cells and never the
/// last one (the live cell stays on screen). A still-running cell at the front
/// holds the line — but only until the backlog behind it grows past a few
/// screens, at which point it is force-committed (frozen mid-run) so a stuck
/// tool or background agent can't pin an unbounded tail in memory.
pub fn commit_count(cells: &[Cell], width: usize, active_h: usize) -> usize {
    let active_h = active_h.max(1);
    let heights: Vec<usize> = cells.iter().map(|c| cell_lines(c, width).len()).collect();
    let total: usize = heights.iter().sum();
    if total <= active_h {
        return 0;
    }
    let hard_cap = active_h.saturating_mul(4);
    let mut committed = 0;
    let mut remaining = total;
    while remaining > active_h && committed + 1 < cells.len() {
        if !is_committable(&cells[committed]) && remaining <= hard_cap {
            break;
        }
        remaining -= heights[committed];
        committed += 1;
    }
    committed
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
    if app.fork_picker.is_some() {
        return "rewind: ↑↓ choose a point · Enter to fork · Esc to cancel".into();
    }
    // The permission mode badge leads every non-rewind status: it is always
    // relevant, and plan mode especially must be unmissable.
    let mode = format!("[{}] ", app.mode.label());
    if !app.confirms.is_empty() {
        return format!("{mode}awaiting approval");
    }
    if app.running {
        // Stable while a turn runs — per-event activity (tool rows, notes,
        // thinking) shows in the transcript, not by churning the status bar.
        return format!("{mode}working… (Ctrl+C to interrupt)");
    }
    format!(
        "{mode}session {} — shift+Tab to change mode · Ctrl+R to rewind · Ctrl+D to quit",
        app.session_id
    )
}

pub fn draw(f: &mut Frame, app: &mut App) {
    // The composer is fenced by a horizontal rule above and below it; the
    // status/mode line is the very bottom row (CC's footer order — the mode
    // indicator lives below the input, not above it).
    let [transcript_area, rule_top, input_area, rule_bottom, status_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());

    let width = transcript_area.width as usize;
    let lines = transcript_lines(&app.cells, width.max(1));
    let height = transcript_area.height as usize;
    // Bottom-anchor the uncommitted tail just above the composer. The event loop
    // has already frozen anything that overflowed into native scrollback, so
    // clipping the top here only bites transiently (e.g. a long answer still
    // streaming); a shorter tail pads with blank rows so the newest line sits by
    // the composer. Scrolling back to older output is the terminal's job now.
    let start = lines.len().saturating_sub(height);
    let visible = &lines[start..];
    let pad = height.saturating_sub(visible.len());
    let mut rows = vec![Line::default(); pad];
    rows.extend_from_slice(visible);
    f.render_widget(Paragraph::new(rows), transcript_area);

    // Rules that fence the composer, above and below it.
    let rule = "─".repeat(width);
    f.render_widget(Paragraph::new(rule.clone()).style(DIM), rule_top);
    f.render_widget(Paragraph::new(rule).style(DIM), rule_bottom);

    f.render_widget(
        Paragraph::new(truncate(&status_line(app), width)).style(DIM),
        status_area,
    );

    let input_width = (input_area.width as usize).saturating_sub(2);
    let (visible, x) = input_view(&app.input, app.cursor, input_width.max(2));
    f.render_widget(Paragraph::new(format!("> {visible}")), input_area);

    if !app.confirms.is_empty() {
        draw_confirm(f, app, f.area());
    } else if app.fork_picker.is_some() {
        draw_fork_picker(f, app, f.area());
    } else {
        f.set_cursor_position((input_area.x + 2 + x, input_area.y));
    }
}

/// The rewind picker popup (plan 18): one row per fork point, the cursor row
/// reversed, windowed so the cursor stays visible in a tall list. The bottom
/// border shows the cursor's position in the list.
fn draw_fork_picker(f: &mut Frame, app: &App, area: Rect) {
    let picker = app.fork_picker.as_ref().expect("checked some");
    let popup_w = area.width.saturating_sub(4).clamp(20, 76);
    let inner_w = usize::from(popup_w - 2);
    let rows: Vec<Line> = picker
        .points
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let text = truncate(&format!("#{}  {}", p.seq, p.preview), inner_w);
            let style = if i == picker.cursor {
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            Line::from(Span::styled(text, style))
        })
        .collect();

    // border(2) is the only overhead; window the rows so the cursor is visible.
    let avail = usize::from(area.height);
    let popup_h = (rows.len() + 2).min(avail).max(3);
    let content_h = popup_h - 2;
    let scroll = picker.cursor.saturating_sub(content_h - 1);
    let end = (scroll + content_h).min(rows.len());
    let visible = rows[scroll..end].to_vec();

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h as u16)) / 2,
        width: popup_w,
        height: popup_h as u16,
    };
    let block = Block::bordered()
        .title("rewind — ↑↓ enter esc")
        .title_bottom(
            Line::from(format!("{}/{}", picker.cursor + 1, picker.points.len())).right_aligned(),
        );
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(visible).block(block), popup);
}

fn draw_confirm(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_w = area.width.saturating_sub(4).clamp(20, 76);
    let inner_w = usize::from(popup_w - 2);
    let req = &app.confirms.front().expect("checked non-empty").req;
    let body = confirm_body_lines(req, inner_w);
    // The y/a/p/n options are pinned below the scroll region — the point of the
    // popup is those keys, so they must stay visible however far the diff runs.
    let options = confirm_option_lines(req, inner_w);
    // border(2) + one blank separator + the pinned options.
    let overhead = 3 + options.len();
    let avail = usize::from(area.height);
    let popup_h = (body.len() + overhead).min(avail);
    let content_h = popup_h.saturating_sub(overhead).max(1);
    let (scroll, mut lines, more_above, more_below) =
        window_lines(&body, app.confirm_scroll, content_h);
    app.confirm_scroll = scroll;
    lines.push(Line::default());
    lines.extend(options);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h as u16)) / 2,
        width: popup_w,
        height: popup_h as u16,
    };
    let mut block = Block::bordered().title("approve?");
    if let Some(hint) = scroll_hint(more_above, more_below) {
        block = block.title_bottom(Line::from(hint).right_aligned());
    }
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);
}

/// The scrollable part of a confirm popup: the wrapped description, then (if
/// present) a blank line and the colored diff preview. Pure and testable.
fn confirm_body_lines(
    req: &kloop_core::permissions::ConfirmRequest,
    inner_w: usize,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line> = wrap(&req.description, inner_w)
        .into_iter()
        .map(Line::from)
        .collect();
    if let Some(preview) = &req.preview {
        lines.push(Line::default());
        lines.extend(diff_preview_lines(preview, inner_w));
    }
    lines
}

/// The pinned action line(s): the yellow y/a/p/n key hints.
fn confirm_option_lines(
    req: &kloop_core::permissions::ConfirmRequest,
    inner_w: usize,
) -> Vec<Line<'static>> {
    let options = match &req.remember_rules {
        Some(rules) => format!(
            "y allow once · a allow this session · p always ({}) · n deny",
            rules.join(", ")
        ),
        None => "y allow once · n deny".to_string(),
    };
    wrap(&options, inner_w)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Yellow))))
        .collect()
}

/// Window `lines` to `height` rows at offset `scroll`, clamped to a valid
/// range. Returns the clamped offset (written back so it self-corrects after
/// over-scrolling), the visible slice, and whether more lies above/below (for
/// the scroll hint). The caller pins its own footer after the visible slice.
fn window_lines(
    lines: &[Line<'static>],
    scroll: usize,
    height: usize,
) -> (usize, Vec<Line<'static>>, bool, bool) {
    let height = height.max(1);
    let scroll = scroll.min(lines.len().saturating_sub(height));
    let end = (scroll + height).min(lines.len());
    (
        scroll,
        lines[scroll..end].to_vec(),
        scroll > 0,
        end < lines.len(),
    )
}

/// The bottom-border hint telling the user the popup scrolls and which way.
fn scroll_hint(more_above: bool, more_below: bool) -> Option<String> {
    match (more_above, more_below) {
        (false, false) => None,
        (true, false) => Some(" ↑ more ".into()),
        (false, true) => Some(" ↓ more ".into()),
        (true, true) => Some(" ↑↓ more ".into()),
    }
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
    fn window_lines_slices_by_offset_and_flags_overflow() {
        let lines: Vec<Line<'static>> = (0..10).map(|i| Line::from(i.to_string())).collect();
        let texts = |ls: &[Line]| -> Vec<String> { ls.iter().map(line_text).collect() };

        // Everything fits: no clamp, no scroll needed, no hints.
        let (scroll, vis, up, down) = window_lines(&lines, 0, 10);
        assert_eq!((scroll, up, down), (0, false, false));
        assert_eq!(texts(&vis).len(), 10);

        // A window in the middle: both directions have more.
        let (scroll, vis, up, down) = window_lines(&lines, 3, 4);
        assert_eq!((scroll, up, down), (3, true, true));
        assert_eq!(texts(&vis), vec!["3", "4", "5", "6"]);

        // Scrolled to the very top: only more below.
        let (_, _, up, down) = window_lines(&lines, 0, 4);
        assert_eq!((up, down), (false, true));

        // Over-scrolled: the offset self-corrects to the last full window and
        // the "more below" hint clears.
        let (scroll, vis, up, down) = window_lines(&lines, 999, 4);
        assert_eq!((scroll, up, down), (6, true, false));
        assert_eq!(texts(&vis), vec!["6", "7", "8", "9"]);
    }

    #[test]
    fn scroll_hint_reflects_available_directions() {
        assert_eq!(scroll_hint(false, false), None);
        assert_eq!(scroll_hint(false, true).as_deref(), Some(" ↓ more "));
        assert_eq!(scroll_hint(true, false).as_deref(), Some(" ↑ more "));
        assert_eq!(scroll_hint(true, true).as_deref(), Some(" ↑↓ more "));
    }

    /// The body carries the description and (when present) a blank line plus the
    /// colored diff; the pinned options are a separate, yellow footer.
    #[test]
    fn confirm_body_and_options_split_scrollable_from_pinned() {
        use kloop_core::permissions::ConfirmRequest;
        let req = ConfirmRequest {
            description: "write_file: notes.txt".into(),
            remember_rules: None,
            preview: Some("+1  hello\n+2  world".into()),
        };
        let body: Vec<String> = confirm_body_lines(&req, 40).iter().map(line_text).collect();
        assert_eq!(
            body,
            vec!["write_file: notes.txt", "", "+1  hello", "+2  world"]
        );

        let opts = confirm_option_lines(&req, 40);
        assert_eq!(
            opts.iter().map(line_text).collect::<Vec<_>>(),
            vec!["y allow once · n deny"]
        );
        // Options are yellow so they read as the action bar.
        assert_eq!(opts[0].spans[0].style, Style::new().fg(Color::Yellow));
    }

    /// End-to-end through a real ratatui frame (TestBackend, no TTY): a diff
    /// taller than the popup renders a windowed slice with the options pinned at
    /// the bottom and a `↓ more` hint; scrolling to the end swaps the visible
    /// slice and flips the hint to `↑ more`, options still pinned.
    #[test]
    fn draw_confirm_windows_a_tall_diff_and_pins_the_options() {
        use crate::events::AgentEvent;
        use kloop_core::permissions::ConfirmRequest;
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        use tokio::sync::oneshot;

        let rows = |term: &Terminal<TestBackend>| -> Vec<String> {
            let buf = term.backend().buffer();
            let area = buf.area;
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
                        .collect::<String>()
                })
                .collect()
        };

        let preview = (1..=60)
            .map(|i| format!("+{i}  line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut app = App::new("s".into());
        let (reply, _rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "write_file: big.txt".into(),
                remember_rules: None,
                preview: Some(preview),
            },
            reply,
        });

        let mut term = Terminal::new(TestBackend::new(40, 16)).unwrap();

        // Pinned to the top: early diff lines show, the tail does not, and the
        // options plus a `↓ more` hint are on screen.
        term.draw(|f| draw(f, &mut app)).unwrap();
        let screen = rows(&term).join("\n");
        assert!(
            screen.contains("+1  line 1"),
            "top of diff visible:\n{screen}"
        );
        assert!(
            !screen.contains("+60  line 60"),
            "tail not yet visible:\n{screen}"
        );
        assert!(
            screen.contains("y allow once · n deny"),
            "options pinned:\n{screen}"
        );
        assert!(screen.contains("↓ more"), "down hint shown:\n{screen}");
        assert!(!screen.contains("↑ more"), "no up hint at top:\n{screen}");

        // Over-scroll: the offset self-corrects to the last window, the tail
        // shows, the top scrolls off, and the hint flips — options stay pinned.
        app.confirm_scroll = 999;
        term.draw(|f| draw(f, &mut app)).unwrap();
        let screen = rows(&term).join("\n");
        assert!(app.confirm_scroll < 999, "offset clamped to a valid range");
        assert!(
            screen.contains("+60  line 60"),
            "tail visible after scroll:\n{screen}"
        );
        assert!(
            !screen.contains("+1  line 1"),
            "top scrolled off:\n{screen}"
        );
        assert!(
            screen.contains("y allow once · n deny"),
            "options still pinned:\n{screen}"
        );
        assert!(screen.contains("↑ more"), "up hint shown:\n{screen}");
        assert!(!screen.contains("↓ more"), "no down hint at end:\n{screen}");
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

    /// The status bar leads with the permission-mode badge in every state and
    /// names shift+Tab when idle; plan mode is spelled out so it is unmissable.
    #[test]
    fn status_line_shows_the_mode_badge() {
        use kloop_core::permissions::Mode;
        let mut app = App::new("sess".into());
        assert!(status_line(&app).starts_with("[manual] "));
        assert!(status_line(&app).contains("shift+Tab"));
        app.mode = Mode::Plan;
        let line = status_line(&app);
        assert!(line.starts_with("[plan] "), "{line}");
        app.running = true;
        assert!(status_line(&app).starts_with("[plan] working…"));
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
                "", // blank separator opening the user turn
                "> do the thing",
                "✓ bash {\"command\":\"ls\"}",
                "… bash {}",
                "[compacting history]",
                "done.",
                "all good",
            ]
        );
    }

    /// Nothing overflows: no cell is committed. Once the tail is taller than the
    /// region, the final leading cells are committed — but never the last one,
    /// and a still-running tool at the front holds the line.
    #[test]
    fn commit_count_freezes_the_overflowing_final_prefix() {
        // Five one-line assistant cells; region only 3 rows tall.
        let five: Vec<Cell> = (0..5)
            .map(|i| Cell::Assistant(format!("line {i}")))
            .collect();
        assert_eq!(commit_count(&five, 40, 10), 0, "fits: commit nothing");
        // total 5 > 3: commit the front 2 so the last 3 fit.
        assert_eq!(commit_count(&five, 40, 3), 2);
        // Never commit the last cell even if the region is tiny.
        assert_eq!(commit_count(&five, 40, 1), 4);

        // A running tool at the front is not committable, so it holds the line
        // (and everything behind it) until it finishes — as long as the backlog
        // stays under the force-commit cap.
        let mut cells = vec![Cell::Tool {
            name: "bash".into(),
            summary: "{}".into(),
            status: ToolStatus::Running,
        }];
        cells.extend((0..3).map(|i| Cell::Assistant(format!("l{i}"))));
        assert_eq!(
            commit_count(&cells, 40, 2),
            0,
            "running front pins the tail"
        );
    }

    /// A System cell (slash-command output) renders every line dim and wrapped,
    /// unlike a Note which collapses to one truncated line.
    #[test]
    fn system_cell_wraps_all_lines_dim() {
        let cells = vec![Cell::System(
            "model: test\ncontext: ~20000 / 200000 tokens (10%)".into(),
        )];
        let lines = transcript_lines(&cells, 40);
        let rendered: Vec<(String, Style)> = lines
            .iter()
            .map(|l| (line_text(l), l.spans[0].style))
            .collect();
        assert_eq!(
            rendered,
            vec![
                ("model: test".to_string(), DIM),
                ("context: ~20000 / 200000 tokens (10%)".to_string(), DIM),
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
