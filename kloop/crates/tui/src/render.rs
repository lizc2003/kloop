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

use kloop_core::event::AgentMessageStatus;
use kloop_core::event::BackgroundTaskKind;
use kloop_core::event::BackgroundTaskStatus;

use std::time::Duration;

use crate::app::App;
use crate::app::Cell;
use crate::app::PendingInteraction;
use crate::app::PendingQuestion;
use crate::app::QuestionPhase;
use crate::app::ToolStatus;
use crate::menu;
use crate::menu::Popup;

const DIM: Style = Style::new().add_modifier(Modifier::DIM);
/// kloop's brand accent: a vivid cyan-blue marks the agent's own presence —
/// the session banner, the working spinner, and the mode badge. Interactive
/// input/selection/status stays ANSI cyan, green marks success/additions, red
/// marks errors/deletions, and secondary text is dim.
const BRAND: Color = Color::Rgb(79, 179, 200);

/// Wall-clock timing the event loop feeds each frame (the pure `App` has no
/// clock, plan 38 slice 5). `elapsed`/`thinking` are the running turn's and the
/// live thinking block's durations (None when not active); `phase` drives the
/// spinner/shimmer (`elapsed_ms / anim::STEP_MS`); `reduced_motion` freezes them.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hud {
    pub elapsed: Option<Duration>,
    pub thinking: Option<Duration>,
    pub phase: usize,
    pub reduced_motion: bool,
}

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

/// The one-line thinking display (plan 38 slice 5): a `∗` gutter + a CC-style
/// verb and elapsed, dim italic. `live_secs = Some(n)` renders the present-tense
/// running form (`∗ Thinking… (Xs)`); otherwise it is sealed — `∗ Thought for Xs`
/// with `sealed_secs`, or a bare `∗ Thought` when there is no timing (resumed).
fn thinking_line(sealed_secs: Option<u64>, live_secs: Option<u64>, width: usize) -> Line<'static> {
    let body = match live_secs {
        Some(s) => format!("∗ Thinking… ({})", crate::anim::format_elapsed(s)),
        None => match sealed_secs {
            Some(s) => format!("∗ Thought for {}", crate::anim::format_elapsed(s)),
            None => "∗ Thought".to_string(),
        },
    };
    Line::from(Span::styled(
        truncate(&body, width.max(2)),
        DIM.add_modifier(Modifier::ITALIC),
    ))
}

/// The status glyph and colour shared by tool rows and sub-agent rows. Running
/// is a cyan status indicator (styles.md), success green, failure red.
fn status_mark(status: &ToolStatus) -> (&'static str, Color) {
    match status {
        ToolStatus::Running => ("…", Color::Cyan),
        ToolStatus::Ok => ("✓", Color::Green),
        ToolStatus::Failed => ("✗", Color::Red),
    }
}

fn background_status_mark(status: BackgroundTaskStatus) -> (&'static str, Color) {
    match status {
        BackgroundTaskStatus::Running => ("●", Color::Cyan),
        BackgroundTaskStatus::Completed => ("✓", Color::Green),
        BackgroundTaskStatus::Failed => ("✗", Color::Red),
        BackgroundTaskStatus::Cancelled => ("■", Color::Gray),
    }
}

fn agent_message_status(status: AgentMessageStatus) -> (&'static str, &'static str, Color) {
    match status {
        AgentMessageStatus::Queued => ("●", "Queued", Color::Cyan),
        AgentMessageStatus::Delivered => ("✓", "Delivered", Color::Green),
        AgentMessageStatus::Undeliverable => ("✗", "Undeliverable", Color::Red),
    }
}

fn background_kind(kind: BackgroundTaskKind) -> &'static str {
    match kind {
        BackgroundTaskKind::Shell => "Shell",
        BackgroundTaskKind::Agent => "Agent",
        BackgroundTaskKind::Program => "Program",
        BackgroundTaskKind::Workflow => "Workflow",
    }
}

fn background_status(status: BackgroundTaskStatus) -> &'static str {
    match status {
        BackgroundTaskStatus::Running => "Running",
        BackgroundTaskStatus::Completed => "Completed",
        BackgroundTaskStatus::Failed => "Failed",
        BackgroundTaskStatus::Cancelled => "Cancelled",
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
            // Sealed assistant message: render the whole thing as markdown. The
            // live streaming path (draw) renders the still-open last cell via
            // `markdown::assistant_stream_lines` instead — see `commit_count`
            // never freezes the last cell, so a committed Assistant is always
            // sealed and safe to parse in full.
            lines.extend(crate::markdown::markdown_lines(text, width));
        }
        Cell::Thinking { seconds, .. } => {
            // Sealed reasoning: a CC-style verb + elapsed, not the text (plan 38
            // slice 5). The live block is rendered by `visible_transcript` with a
            // running clock; here it is frozen with its final time.
            lines.push(thinking_line(*seconds, None, width));
        }
        Cell::Tool {
            name,
            input,
            status,
            output,
        } => {
            // Human-readable header + result preview (plan 38 slice 2).
            lines.extend(crate::toolrow::tool_cell_lines(
                name,
                input,
                *status,
                output.as_deref(),
                width,
            ));
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
        Cell::BackgroundTask(task) => {
            let (mark, color) = background_status_mark(task.status);
            let title = format!("{}({})", background_kind(task.kind), task.description);
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::new().fg(color)),
                Span::styled(
                    truncate(&title, width.saturating_sub(2)),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]));
            let mut identity = format!("{} · {}", background_status(task.status), task.id);
            if let Some(run_id) = &task.run_id {
                identity.push_str(&format!(" · resumable as {run_id}"));
            }
            lines.push(Line::from(Span::styled(
                truncate(&format!("  {identity}"), width),
                DIM,
            )));
            if let Some(detail) = &task.detail {
                let label = if task.kind == BackgroundTaskKind::Workflow
                    && task.status == BackgroundTaskStatus::Running
                {
                    "Phase: "
                } else {
                    ""
                };
                lines.push(Line::from(Span::styled(
                    truncate(&format!("  {label}{detail}"), width),
                    DIM,
                )));
            }
            if let Some(output_path) = &task.output_path {
                lines.push(Line::from(Span::styled(
                    truncate(&format!("  output: {output_path}"), width),
                    DIM,
                )));
            }
        }
        Cell::AgentMessage(message) => {
            let (mark, status, color) = agent_message_status(message.status);
            let title = format!("Message from Agent · {}", message.from);
            lines.push(Line::from(vec![
                Span::styled(format!("{mark} "), Style::new().fg(color)),
                Span::styled(
                    truncate(&title, width.saturating_sub(2)),
                    Style::new().add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(Span::styled(
                truncate(
                    &format!("  {status} to {} · {}", message.to, message.id),
                    width,
                ),
                DIM,
            )));
            lines.push(Line::from(Span::styled(
                truncate(&format!("  {}", message.summary), width),
                DIM,
            )));
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
        Cell::SessionHeader {
            model,
            cwd,
            branch,
            mode,
        } => {
            lines.extend(session_header_lines(
                model,
                cwd,
                branch.as_deref(),
                mode,
                width,
            ));
        }
    }
    lines
}

/// The opening session banner (plan 38 slice 6): a rounded box (`╭─╮ │ ╰─╯`) in
/// the brand colour, titled `>_ kloop`, listing the model, cwd, branch (omitted
/// off a repo), and starting mode as dim-label / default-value rows. The box
/// width fits the content, capped so it never spans an ultra-wide terminal.
fn session_header_lines(
    model: &str,
    cwd: &str,
    branch: Option<&str>,
    mode: &str,
    width: usize,
) -> Vec<Line<'static>> {
    const MAX_W: usize = 72;
    const LABEL_W: usize = 8; // "branch" + padding, the widest label
    let mut fields: Vec<(&str, &str)> = vec![("model", model), ("cwd", cwd)];
    if let Some(b) = branch {
        fields.push(("branch", b));
    }
    fields.push(("mode", mode));

    // Inner width = the widest of the title and the label+value rows, capped to
    // the terminal (minus the two border columns) and MAX_W.
    let title = ">_ kloop";
    let content_w = fields
        .iter()
        .map(|(_, v)| LABEL_W + display_width(v))
        .chain(std::iter::once(display_width(title)))
        .max()
        .unwrap_or(0);
    let cap = width.saturating_sub(2).clamp(1, MAX_W);
    let inner = content_w.min(cap);

    let brand = Style::new().fg(BRAND);
    let mut lines = Vec::new();
    // Top border.
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner + 2)),
        brand,
    )));
    // Title row (bold brand), one space of padding inside the border.
    lines.push(boxed_row(
        vec![Span::styled(
            truncate(title, inner),
            brand.add_modifier(Modifier::BOLD),
        )],
        inner,
        brand,
    ));
    // Field rows: dim label column, default-weight value.
    for (label, value) in fields {
        let label_span = Span::styled(format!("{label:<LABEL_W$}"), DIM);
        let value_span = Span::raw(truncate(value, inner.saturating_sub(LABEL_W).max(1)));
        lines.push(boxed_row(vec![label_span, value_span], inner, brand));
    }
    // Bottom border.
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner + 2)),
        brand,
    )));
    lines
}

/// One `│ …content… │` row of the session banner: the brand verticals with a
/// space of padding, the content spans padded on the right to `inner` columns.
fn boxed_row(content: Vec<Span<'static>>, inner: usize, brand: Style) -> Line<'static> {
    let used: usize = content.iter().map(|s| display_width(&s.content)).sum();
    let mut spans = vec![Span::styled("│ ".to_string(), brand)];
    spans.extend(content);
    if used < inner {
        spans.push(Span::raw(" ".repeat(inner - used)));
    }
    spans.push(Span::styled(" │".to_string(), brand));
    Line::from(spans)
}

/// The uncommitted tail as flat display lines (each cell via [`cell_lines`]),
/// every cell sealed. A test-only convenience for asserting on a fixed slice of
/// cells; the draw path uses [`visible_transcript`], which additionally streams
/// the open last cell.
#[cfg(test)]
pub fn transcript_lines(cells: &[Cell], width: usize) -> Vec<Line<'static>> {
    cells.iter().flat_map(|c| cell_lines(c, width)).collect()
}

/// The uncommitted tail for the on-screen viewport: like [`transcript_lines`],
/// but the last cell gets a live treatment when it is still streaming. An
/// Assistant renders through the streaming safe-boundary buffer (a half-formed
/// markdown block shows raw instead of reflowing each frame); a Thinking block
/// shows a running clock (`hud.thinking`). Only the last cell can be streaming
/// (any other event seals it), so these are the sole special cases.
pub fn visible_transcript(app: &App, hud: &Hud, width: usize) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let last = app.cells.len().saturating_sub(1);
    for (i, cell) in app.cells.iter().enumerate() {
        match cell {
            Cell::Assistant(text) if i == last && app.streaming_assistant() => {
                lines.extend(crate::markdown::assistant_stream_lines(text, width));
            }
            Cell::Thinking { .. } if i == last && app.streaming_thinking() => {
                let secs = hud.thinking.map(|d| d.as_secs()).unwrap_or(0);
                lines.push(thinking_line(None, Some(secs), width));
            }
            _ => lines.extend(cell_lines(cell, width)),
        }
    }
    lines
}

/// A cell is committable once it can no longer change: everything except a tool
/// or sub-agent row that is still Running (its ✓/✗ has yet to land).
fn is_committable(cell: &Cell) -> bool {
    match cell {
        Cell::Tool { status, .. } | Cell::Agent { status, .. } => *status != ToolStatus::Running,
        Cell::BackgroundTask(task) => task.status != BackgroundTaskStatus::Running,
        Cell::AgentMessage(message) => message.status != AgentMessageStatus::Queued,
        _ => true,
    }
}

/// How many leading cells to freeze into scrollback so the uncommitted tail fits
/// an `active_h`-row viewport region. Commits only finalized cells and never the
/// last one. A live assistant/reasoning cell is never committed even when it is
/// no longer last; its completion must still update the original cell by id. A
/// still-running tool/sub-agent row at the front holds the line until the backlog
/// grows past a few screens, then may be frozen mid-run so it cannot pin an
/// unbounded tail.
pub fn commit_count(
    cells: &[Cell],
    width: usize,
    active_h: usize,
    mut display_cell_live: impl FnMut(usize) -> bool,
) -> usize {
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
        if display_cell_live(committed) {
            break;
        }
        if !is_committable(&cells[committed]) && remaining <= hard_cap {
            break;
        }
        remaining -= heights[committed];
        committed += 1;
    }
    committed
}

/// The on-screen height of the composer at `width`: its wrapped rows (already
/// capped inside the composer) plus one row for the attachment line when images
/// are pending. Both [`draw`] and the event loop's overflow-commit size the
/// bottom chrome from this.
pub fn composer_height(app: &App, width: usize) -> usize {
    let rows = app.composer.view(width).rows.len();
    let attach = usize::from(!app.composer.attachments().is_empty());
    (rows + attach).max(1)
}

/// The dim `📎 a.png, b.png` line above the composer when images are attached.
fn attachment_line(labels: &[String], width: usize) -> Line<'static> {
    Line::from(Span::styled(
        truncate(&format!("📎 {}", labels.join(", ")), width),
        DIM,
    ))
}

/// Whether [`activity_line`] will render a row. Used by `commit_overflow` for
/// its layout reservation, where no `Hud` is available; mirrors the conditions
/// in `activity_line`.
pub fn has_activity_line(app: &App) -> bool {
    app.ctrl_c_exit_armed || app.interaction_active() || app.running
}

/// The dynamic "what's happening now" line, shown at the BOTTOM of the
/// transcript (just above the composer) where it is most prominent — the eye
/// lands here, not on the footer (plan 38 slice 5). Running: an animated spinner
/// + a shimmering verb + `(elapsed · esc to interrupt)`. `None` when idle.
pub fn activity_line(app: &App, hud: &Hud) -> Option<Line<'static>> {
    if app.ctrl_c_exit_armed {
        // Unmissable, right above the composer where Ctrl+C was pressed.
        return Some(Line::from("press Ctrl+C again to exit".to_string()));
    }
    if let Some(interaction) = app.interactions.front() {
        let text = match interaction {
            PendingInteraction::Confirm { .. } => "awaiting your approval",
            PendingInteraction::Question(_) => "awaiting your answer",
        };
        return Some(Line::from(text.to_string()));
    }
    if !app.running {
        return None;
    }
    let glyph = crate::anim::spinner_glyph(hud.phase, hud.reduced_motion);
    // The working spinner is kloop's brand presence (CC's brand-coloured spinner).
    let mut spans = vec![Span::styled(format!("{glyph} "), Style::new().fg(BRAND))];
    spans.extend(crate::anim::shimmer_spans(
        "Working",
        hud.phase,
        hud.reduced_motion,
    ));
    let secs = hud.elapsed.map(|d| d.as_secs()).unwrap_or(0);
    spans.push(Span::styled(
        format!(
            " ({} · esc to interrupt)",
            crate::anim::format_elapsed(secs)
        ),
        DIM,
    ));
    Some(Line::from(spans))
}

/// The stable bottom bar: the permission-mode badge and key hints on the left,
/// the system status (model + context gauge) flush right. Only the badge, the
/// running/idle hint set, and the (per-turn) context gauge change — never per
/// event — so the bar barely moves. Live activity lives in [`activity_line`].
pub fn footer_line(app: &App, width: usize) -> Line<'static> {
    if let Some(interaction) = app.interactions.front() {
        let hint = match interaction {
            PendingInteraction::Confirm { .. } => "approval: y allow · n/esc deny · ↑↓ scroll",
            PendingInteraction::Question(question) => match question.phase {
                QuestionPhase::Select if question.current().multi_select => {
                    "question: ↑↓ choose · Space toggle · Enter submit · Esc cancel"
                }
                QuestionPhase::Select => "question: ↑↓ choose · Enter select · Esc cancel",
                QuestionPhase::Other => "question: type Other · Enter submit · Esc cancel",
                QuestionPhase::Notes => "question: optional notes · Enter submit · Esc cancel",
            },
        };
        return Line::from(Span::styled(hint.to_string(), DIM));
    }
    if app.fork_picker.is_some() {
        return Line::from(Span::styled(
            "rewind: ↑↓ choose a point · Enter to fork · Esc to cancel".to_string(),
            DIM,
        ));
    }
    if app.popup.is_some() {
        return Line::from(Span::styled(
            "↑↓ choose · Tab/⏎ complete · Esc cancel".to_string(),
            DIM,
        ));
    }
    // The mode badge carries the brand accent (CC's brand-coloured mode line);
    // the hints stay dim so only the badge draws the eye.
    let badge = format!("[{}]  ", app.mode.label());
    let hints = if app.running {
        "esc to interrupt · Ctrl+C to exit".to_string()
    } else {
        "shift+Tab to change mode · Ctrl+R to rewind · Ctrl+C to exit".to_string()
    };
    // Right-aligned system status; dropped if the row is too narrow to fit it
    // after the badge + hints (those matter more).
    let right = system_status(app);
    let lw = display_width(&badge) + display_width(&hints);
    let rw = display_width(&right);
    let badge_span = Span::styled(badge.clone(), Style::new().fg(BRAND));
    if !right.is_empty() && lw + 3 + rw <= width {
        let pad = width - lw - rw;
        Line::from(vec![
            badge_span,
            Span::styled(hints, DIM),
            Span::styled(" ".repeat(pad), DIM),
            Span::styled(right, DIM),
        ])
    } else if display_width(&badge) < width {
        Line::from(vec![
            badge_span,
            Span::styled(truncate(&hints, width - display_width(&badge)), DIM),
        ])
    } else {
        Line::from(Span::styled(
            truncate(&badge, width),
            Style::new().fg(BRAND),
        ))
    }
}

/// The footer's right-hand status: model name and the context gauge
/// (`~used/window (pct%)`, or `~used tok` with the window off). Empty when no
/// model is known (mock/tests).
fn system_status(app: &App) -> String {
    if app.model.is_empty() {
        return String::new();
    }
    match app.context_window {
        Some(window) if window > 0 => {
            let pct = (app.context_used as f64 / window as f64 * 100.0).round() as u64;
            format!("{} · {}% ctx", app.model, pct.min(100))
        }
        _ => format!("{} · ~{} tok", app.model, app.context_used),
    }
}

pub fn draw(f: &mut Frame, app: &mut App, hud: &Hud) {
    // Bottom-up: a stable footer (mode + hints), the multi-line composer fenced
    // by a rule above and below, and the transcript — whose last line carries
    // the live activity status (CC's information architecture: the dynamic
    // "what's happening" sits by the composer where it is seen, the footer stays
    // still). The composer's height is dynamic (it grows with the input).
    let full = f.area();
    let width = full.width as usize;
    let view = app.composer.view(width.max(1));
    let labels: Vec<String> = app.composer.attachments().to_vec();
    let attach_h = u16::from(!labels.is_empty());
    let composer_h = (view.rows.len() as u16 + attach_h).max(1);
    let [transcript_area, rule_top, input_area, rule_bottom, footer_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(composer_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(full);

    let mut lines = visible_transcript(app, hud, width.max(1));
    // The activity status is the last transcript line — rendered here, never a
    // cell, so it is never frozen into scrollback. A blank spacer sets it off.
    if let Some(activity) = activity_line(app, hud) {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(activity);
    }
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

    // Stable footer at the very bottom (mode + hints left, system status right).
    f.render_widget(Paragraph::new(footer_line(app, width)), footer_area);

    // Composer: the attachment line (if any) above the wrapped input rows.
    let mut comp_rows: Vec<Line> = Vec::new();
    if attach_h == 1 {
        comp_rows.push(attachment_line(&labels, width));
    }
    comp_rows.extend(view.rows);
    f.render_widget(Paragraph::new(comp_rows), input_area);

    if matches!(
        app.interactions.front(),
        Some(PendingInteraction::Confirm { .. })
    ) {
        draw_confirm(f, app, full);
    } else if matches!(
        app.interactions.front(),
        Some(PendingInteraction::Question(_))
    ) {
        draw_question(f, app, full);
    } else if app.fork_picker.is_some() {
        draw_fork_picker(f, app, full);
    } else {
        // A completion menu (if open) floats just above the composer; the cursor
        // stays in the input, since the user is still typing the query.
        if let Some(popup) = &app.popup {
            draw_menu(f, popup, rule_top, width);
        }
        f.set_cursor_position((
            input_area.x + view.cursor_col,
            input_area.y + attach_h + view.cursor_row,
        ));
    }
}

/// The slash/file completion menu (plan 38 slice 4): a borderless flat list
/// floating directly above the composer's top rule, the highlighted row
/// reversed (theme-safe, like the rewind picker). Grows upward from the rule so
/// the most-relevant top rows sit nearest the input.
fn draw_menu(f: &mut Frame, popup: &Popup, rule_top: Rect, width: usize) {
    let lines = menu_lines(popup, width, menu::MENU_ROWS);
    let height = lines.len() as u16;
    if height == 0 {
        return;
    }
    let y = rule_top.y.saturating_sub(height);
    let area = Rect {
        x: rule_top.x,
        y,
        width: rule_top.width,
        height,
    };
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines), area);
}

/// The menu's rows for one width, windowed to `max_rows` around the cursor. The
/// selected row is reversed across its full width; others show the label plus a
/// dim detail. Pure and testable.
pub fn menu_lines(popup: &Popup, width: usize, max_rows: usize) -> Vec<Line<'static>> {
    let n = popup.items.len();
    if n == 0 || width == 0 {
        return Vec::new();
    }
    let visible = n.min(max_rows);
    // Window so the cursor row stays in view as the list scrolls.
    let start = popup.cursor.saturating_sub(visible - 1).min(n - visible);
    (start..start + visible)
        .map(|i| {
            let item = &popup.items[i];
            let selected = i == popup.cursor;
            let text = if item.detail.is_empty() {
                item.label.clone()
            } else {
                format!("{}  {}", item.label, item.detail)
            };
            if selected {
                // Reversed across the whole width: pad the text so the highlight
                // fills the row.
                let padded = pad(&text, width);
                Line::from(Span::styled(
                    padded,
                    Style::new().add_modifier(Modifier::REVERSED),
                ))
            } else {
                // Label at default weight, detail dim; truncated to width.
                let label = truncate(&item.label, width);
                let label_w = display_width(&label);
                let mut spans = vec![Span::raw(label)];
                if !item.detail.is_empty() && label_w + 2 < width {
                    let rest = truncate(&format!("  {}", item.detail), width - label_w);
                    spans.push(Span::styled(rest, DIM));
                }
                Line::from(spans)
            }
        })
        .collect()
}

/// Right-pad `text` with spaces to `width` display columns (truncating first if
/// it is already wider), so a reversed highlight fills the whole row.
fn pad(text: &str, width: usize) -> String {
    let mut s = truncate(text, width);
    let w = display_width(&s);
    if w < width {
        s.push_str(&" ".repeat(width - w));
    }
    s
}

/// Display width of `s` in terminal columns (CJK counts as 2).
fn display_width(s: &str) -> usize {
    s.chars().map(|c| c.width().unwrap_or(0)).sum()
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

fn draw_question(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_w = area.width.saturating_sub(4).clamp(24, 84);
    let inner_w = usize::from(popup_w - 2);
    let Some(PendingInteraction::Question(question)) = app.interactions.front() else {
        return;
    };
    let title = format!(
        "question {}/{} · {}",
        question.question_index + 1,
        question.req.questions.len(),
        question.current().header
    );
    let body = question_body_lines(question, inner_w);
    let (footer, editor_prefix) = question_footer_lines(question, inner_w);
    let overhead = 3 + footer.len();
    let avail = usize::from(area.height);
    let popup_h = (body.len() + overhead).min(avail).max(overhead.min(avail));
    let content_h = popup_h.saturating_sub(overhead).max(1);
    let (scroll, mut lines, more_above, more_below) =
        window_lines(&body, app.confirm_scroll, content_h);
    app.confirm_scroll = scroll;
    lines.push(Line::default());
    lines.extend(footer);

    let popup = Rect {
        x: area.x + (area.width.saturating_sub(popup_w)) / 2,
        y: area.y + (area.height.saturating_sub(popup_h as u16)) / 2,
        width: popup_w,
        height: popup_h as u16,
    };
    let mut block = Block::bordered().title(title);
    if let Some(hint) = scroll_hint(more_above, more_below) {
        block = block.title_bottom(Line::from(hint).right_aligned());
    }
    f.render_widget(Clear, popup);
    f.render_widget(Paragraph::new(lines).block(block), popup);

    if let Some(prefix_width) = editor_prefix {
        let editor_width = display_width(&question.editor);
        let x = popup
            .x
            .saturating_add(1)
            .saturating_add((prefix_width + editor_width).min(inner_w) as u16);
        let y = popup.y.saturating_add(popup.height.saturating_sub(2));
        f.set_cursor_position((x, y));
    }
}

fn question_body_lines(question: &PendingQuestion, inner_w: usize) -> Vec<Line<'static>> {
    let current = question.current();
    let mut lines: Vec<Line<'static>> = wrap(&current.question, inner_w)
        .into_iter()
        .map(Line::from)
        .collect();
    lines.push(Line::default());
    for (index, option) in current.options.iter().enumerate() {
        let selected = question.selected.contains(&index);
        let marker = if current.multi_select {
            if selected {
                "[x]"
            } else {
                "[ ]"
            }
        } else if selected {
            "(●)"
        } else {
            "( )"
        };
        let text = format!("{marker} {} — {}", option.label, option.description);
        let style = if question.phase == QuestionPhase::Select && question.cursor == index {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        for fragment in wrap(&text, inner_w) {
            lines.push(Line::from(Span::styled(fragment, style)));
        }
    }
    let other_style =
        if question.phase == QuestionPhase::Select && question.cursor == current.options.len() {
            Style::new().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
    lines.push(Line::from(Span::styled(
        "Other — type a custom answer".to_string(),
        other_style,
    )));
    if let Some(preview) = question.selected_preview() {
        lines.push(Line::default());
        lines.push(Line::from(Span::styled("Preview", DIM)));
        lines.extend(
            preview
                .lines()
                .flat_map(|line| wrap(line, inner_w))
                .map(Line::from),
        );
    }
    lines
}

fn question_footer_lines(
    question: &PendingQuestion,
    inner_w: usize,
) -> (Vec<Line<'static>>, Option<usize>) {
    match question.phase {
        QuestionPhase::Select => {
            let hint = if question.current().multi_select {
                "↑↓ choose · Space toggle · Enter submit · Esc cancel"
            } else {
                "↑↓ choose · Enter select · Esc cancel"
            };
            (
                wrap(hint, inner_w)
                    .into_iter()
                    .map(|line| Line::from(Span::styled(line, Style::new().fg(Color::Cyan))))
                    .collect(),
                None,
            )
        }
        QuestionPhase::Other => {
            let prefix = "Other > ";
            (
                vec![Line::from(vec![
                    Span::styled(prefix, Style::new().fg(Color::Cyan)),
                    Span::raw(truncate(
                        &question.editor,
                        inner_w.saturating_sub(prefix.len()),
                    )),
                ])],
                Some(display_width(prefix)),
            )
        }
        QuestionPhase::Notes => {
            let prefix = "Notes (optional) > ";
            (
                vec![Line::from(vec![
                    Span::styled(prefix, Style::new().fg(Color::Cyan)),
                    Span::raw(truncate(
                        &question.editor,
                        inner_w.saturating_sub(prefix.len()),
                    )),
                ])],
                Some(display_width(prefix)),
            )
        }
    }
}

fn draw_confirm(f: &mut Frame, app: &mut App, area: Rect) {
    let popup_w = area.width.saturating_sub(4).clamp(20, 76);
    let inner_w = usize::from(popup_w - 2);
    let Some(PendingInteraction::Confirm { req, .. }) = app.interactions.front() else {
        return;
    };
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
        // A GitHub-style `+N -M` summary above the diff body (plan 38 slice 6).
        if let Some(stats) = diff_stats_line(preview) {
            lines.push(stats);
        }
        lines.extend(diff_preview_lines(preview, inner_w));
    }
    lines
}

/// The `+N -M` change summary for a diff preview: additions green, deletions
/// red. `None` when the preview has no +/- lines (e.g. an oversized-overwrite
/// note), so no summary row is shown.
fn diff_stats_line(preview: &str) -> Option<Line<'static>> {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in preview.lines() {
        match line.chars().next() {
            Some('+') => added += 1,
            Some('-') => removed += 1,
            _ => {}
        }
    }
    if added == 0 && removed == 0 {
        return None;
    }
    Some(Line::from(vec![
        Span::styled(format!("+{added}"), Style::new().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("-{removed}"), Style::new().fg(Color::Red)),
    ]))
}

/// The pinned action line(s): the yellow y/a/p/n key hints.
fn confirm_option_lines(
    req: &kloop_core::permissions::ConfirmRequest,
    inner_w: usize,
) -> Vec<Line<'static>> {
    let mut options = Vec::new();
    if req
        .approval_scopes
        .contains(&kloop_core::permissions::ApprovalScope::Once)
    {
        options.push("y allow once");
    }
    if req
        .approval_scopes
        .contains(&kloop_core::permissions::ApprovalScope::WorkspaceSession)
    {
        options.push("a allow this workspace session");
    }
    if req
        .approval_scopes
        .contains(&kloop_core::permissions::ApprovalScope::Project)
    {
        options.push("p allow this project across sessions and linked worktrees");
    }
    options.push("n deny");
    let options = options.join(" · ");
    // Cyan action bar — an input tip prompting the choice (styles.md), not yellow.
    wrap(&options, inner_w)
        .into_iter()
        .map(|l| Line::from(Span::styled(l, Style::new().fg(Color::Cyan))))
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
    fn brand_accent_is_vivid_cyan_blue() {
        assert_eq!(BRAND, Color::Rgb(79, 179, 200));
        assert_ne!(BRAND, Color::Magenta);
        assert_ne!(BRAND, Color::Cyan);
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
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: Some("+1  hello\n+2  world".into()),
        };
        let body: Vec<String> = confirm_body_lines(&req, 40).iter().map(line_text).collect();
        // A `+N -M` stats summary (plan 38 slice 6) precedes the diff body.
        assert_eq!(
            body,
            vec![
                "write_file: notes.txt",
                "",
                "+2 -0",
                "+1  hello",
                "+2  world"
            ]
        );

        let opts = confirm_option_lines(&req, 40);
        assert_eq!(
            opts.iter().map(line_text).collect::<Vec<_>>(),
            vec!["y allow once · n deny"]
        );
        // Options are cyan (an input-tip action bar, styles.md), not yellow.
        assert_eq!(opts[0].spans[0].style, Style::new().fg(Color::Cyan));

        let all = ConfirmRequest {
            approval_scopes: vec![
                kloop_core::permissions::ApprovalScope::Once,
                kloop_core::permissions::ApprovalScope::WorkspaceSession,
                kloop_core::permissions::ApprovalScope::Project,
            ],
            remember_rules: Some(vec!["write_file(src/**)".into()]),
            ..req
        };
        assert_eq!(
            confirm_option_lines(&all, 200)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec!["y allow once · a allow this workspace session · p allow this project across sessions and linked worktrees · n deny"]
        );
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
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                remember_rules: None,
                preview: Some(preview),
            },
            reply,
        });

        let mut term = Terminal::new(TestBackend::new(40, 16)).unwrap();

        // Pinned to the top: early diff lines show, the tail does not, and the
        // options plus a `↓ more` hint are on screen.
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
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
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
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

    /// The menu highlights the cursor row (reversed) and windows a long list to
    /// keep the cursor visible near the bottom (rows nearest the composer).
    #[test]
    fn menu_lines_highlight_cursor_and_window_long_lists() {
        let items: Vec<menu::MenuItem> = (0..20)
            .map(|i| menu::MenuItem {
                label: format!("/cmd{i}"),
                detail: format!("desc {i}"),
                insert: format!("/cmd{i}"),
            })
            .collect();
        let popup = Popup {
            kind: menu::PopupKind::Slash,
            query: String::new(),
            items,
            cursor: 12,
        };
        let lines = menu_lines(&popup, 40, 8);
        assert_eq!(lines.len(), 8, "windowed to max_rows");
        // The cursor row (12) is the last visible row, and it is reversed.
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.last().unwrap().starts_with("/cmd12"),
            "cursor row last: {texts:?}"
        );
        assert!(
            lines.last().unwrap().spans[0]
                .style
                .add_modifier
                .contains(Modifier::REVERSED),
            "selected row reversed"
        );
        // A non-selected row is not reversed and carries a dim detail span.
        assert!(!lines[0].spans[0]
            .style
            .add_modifier
            .contains(Modifier::REVERSED));
    }

    /// End-to-end (TestBackend): an open menu floats directly above the composer
    /// (its top rule), not in the footer, and shows the candidates.
    #[test]
    fn draw_floats_the_menu_above_the_composer() {
        let popup = Popup {
            kind: menu::PopupKind::Slash,
            query: "co".into(),
            items: vec![
                menu::MenuItem {
                    label: "/cost".into(),
                    detail: "session cost".into(),
                    insert: "/cost".into(),
                },
                menu::MenuItem {
                    label: "/compact".into(),
                    detail: "compact history".into(),
                    insert: "/compact".into(),
                },
            ],
            cursor: 0,
        };
        let mut app = App::new("s".into());
        app.popup = Some(popup);
        // Type the trigger into the composer so the cursor sits in the input.
        app.composer.paste("/co");

        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut term = Terminal::new(TestBackend::new(40, 12)).unwrap();
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
        let buf = term.backend().buffer();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
                    .collect::<String>()
            })
            .collect();
        let screen = rows.join("\n");
        assert!(screen.contains("/cost"), "menu candidate shown:\n{screen}");
        assert!(
            screen.contains("/compact"),
            "menu candidate shown:\n{screen}"
        );
        // The menu sits above the composer's `›` prompt row.
        let menu_row = rows.iter().position(|r| r.contains("/cost")).unwrap();
        let prompt_row = rows.iter().position(|r| r.contains("›")).unwrap();
        assert!(menu_row < prompt_row, "menu above composer:\n{screen}");
    }

    /// The session banner (plan 38 slice 6): a rounded brand-coloured box titled
    /// `>_ kloop`, one dim-label row per field, branch present when on a repo.
    #[test]
    fn session_header_renders_a_branded_box_with_fields() {
        let lines = session_header_lines(
            "claude-sonnet-4-6",
            "~/work/kloop",
            Some("main"),
            "manual",
            80,
        );
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts[0].starts_with('╭') && texts[0].ends_with('╮'),
            "{texts:?}"
        );
        assert!(
            texts.last().unwrap().starts_with('╰') && texts.last().unwrap().ends_with('╯'),
            "{texts:?}"
        );
        assert!(texts[1].contains(">_ kloop"), "{texts:?}");
        let has = |k: &str, v: &str| texts.iter().any(|t| t.contains(k) && t.contains(v));
        assert!(has("model", "claude-sonnet-4-6"), "{texts:?}");
        assert!(has("cwd", "~/work/kloop"), "{texts:?}");
        assert!(has("branch", "main"), "{texts:?}");
        assert!(has("mode", "manual"), "{texts:?}");
        // The border and title carry the brand accent, the title is bold.
        assert_eq!(lines[0].spans[0].style.fg, Some(BRAND));
        assert!(lines[1].spans[1]
            .style
            .add_modifier
            .contains(Modifier::BOLD));
    }

    /// Off a git repo the branch row is omitted (the header is built with
    /// `branch = None`); the other rows still render.
    #[test]
    fn session_header_omits_branch_off_a_repo() {
        let texts: Vec<String> = session_header_lines("m", "/tmp/x", None, "plan", 80)
            .iter()
            .map(line_text)
            .collect();
        assert!(!texts.iter().any(|t| t.contains("branch")), "{texts:?}");
        assert!(
            texts
                .iter()
                .any(|t| t.contains("cwd") && t.contains("/tmp/x")),
            "{texts:?}"
        );
        assert!(texts.iter().any(|t| t.contains("plan")), "{texts:?}");
    }

    /// The `+N -M` diff summary counts `+`/`-` lines (green/red) and is absent
    /// when the preview has no diff lines (e.g. an oversized-overwrite note).
    #[test]
    fn diff_stats_counts_additions_and_deletions() {
        let line = diff_stats_line(" 1  ctx\n-2  old\n+2  new\n+3  more").unwrap();
        let spans: Vec<(String, Option<Color>)> = line
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style.fg))
            .collect();
        assert_eq!(spans[0], ("+2".to_string(), Some(Color::Green)));
        assert_eq!(spans[2], ("-1".to_string(), Some(Color::Red)));
        assert!(diff_stats_line("(overwriting existing file, 999 bytes)").is_none());
    }

    /// End-to-end (TestBackend): the session banner inserted as the first cell
    /// renders its box and fields into the viewport.
    #[test]
    fn draw_shows_the_session_header() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut app = App::new("s".into());
        app.cells.insert(
            0,
            Cell::SessionHeader {
                model: "sonnet-5".into(),
                cwd: "~/work/kloop".into(),
                branch: Some("main".into()),
                mode: "manual".into(),
            },
        );
        let mut term = Terminal::new(TestBackend::new(60, 14)).unwrap();
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
        let buf = term.backend().buffer();
        let screen: String = (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(""))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains(">_ kloop"), "{screen}");
        assert!(screen.contains("sonnet-5"), "{screen}");
        assert!(screen.contains("main"), "{screen}");
        assert!(screen.contains('╭') && screen.contains('╯'), "{screen}");
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

    /// The footer leads with the mode badge and names shift+Tab when idle; the
    /// live "what's happening" text lives in the activity line, not the footer.
    #[test]
    fn footer_shows_mode_badge_activity_shows_state() {
        use kloop_core::permissions::Mode;
        let hud = Hud::default();
        let mut app = App::new("sess".into());
        // Idle: footer has the badge + hints; no activity line.
        assert!(line_text(&footer_line(&app, 80)).starts_with("[manual]  "));
        assert!(line_text(&footer_line(&app, 80)).contains("shift+Tab"));
        // The badge carries the brand accent; the hints stay dim.
        let footer = footer_line(&app, 80);
        assert_eq!(footer.spans[0].style.fg, Some(BRAND));
        assert_eq!(footer.spans[1].style, DIM);
        assert!(activity_line(&app, &hud).is_none());
        assert!(!has_activity_line(&app));

        app.mode = Mode::Plan;
        assert!(line_text(&footer_line(&app, 80)).starts_with("[plan]  "));

        // Running: the activity line shows the verb, footer switches to the
        // interrupt hint (still leading with the badge, no per-event churn).
        app.running = true;
        assert!(has_activity_line(&app));
        let activity = line_text(&activity_line(&app, &hud).expect("running shows activity"));
        assert!(activity.contains("Working"), "{activity}");
        let footer = line_text(&footer_line(&app, 80));
        assert!(footer.starts_with("[plan]  "), "{footer}");
        assert!(footer.contains("esc to interrupt"), "{footer}");

        // Armed / awaiting-approval take over the activity line, not the footer.
        app.ctrl_c_exit_armed = true;
        assert_eq!(
            line_text(&activity_line(&app, &hud).unwrap()),
            "press Ctrl+C again to exit"
        );
    }

    /// The footer's right side carries the model + context gauge when Config
    /// seeded them; it is dropped on a narrow row so the hints survive.
    #[test]
    fn footer_shows_context_gauge_on_the_right() {
        let app = App::new("s".into()).with_context("sonnet-5".into(), Some(200_000), 40_000);
        let footer = line_text(&footer_line(&app, 100));
        assert!(footer.contains("sonnet-5"), "{footer}");
        assert!(footer.contains("20% ctx"), "{footer}"); // 40k / 200k
                                                         // Too narrow for the status: only the hints render.
        let narrow = line_text(&footer_line(&app, 30));
        assert!(!narrow.contains("sonnet-5"), "{narrow}");
        assert!(narrow.contains("["), "the mode badge still shows: {narrow}");
    }

    /// The activity line while running: an animated spinner glyph, a shimmering
    /// verb, and the elapsed + interrupt hint.
    #[test]
    fn activity_line_shows_spinner_and_elapsed() {
        let mut app = App::new("s".into());
        app.running = true;
        let hud = Hud {
            elapsed: Some(Duration::from_secs(65)),
            phase: 2,
            ..Default::default()
        };
        let line = activity_line(&app, &hud).unwrap();
        let text = line_text(&line);
        assert!(text.contains("Working"), "{text}");
        assert!(text.contains("1m05s"), "{text}");
        assert!(text.contains("esc to interrupt"), "{text}");
    }

    /// The thinking cell shows a verb + elapsed: present tense with a running
    /// clock, past tense once sealed, and a bare verb when there is no timing.
    #[test]
    fn thinking_cell_shows_verb_and_elapsed() {
        // Live (in the streaming path): present tense + running clock.
        assert_eq!(
            line_text(&thinking_line(None, Some(8), 40)),
            "∗ Thinking… (8s)"
        );
        // Sealed with a time.
        let sealed = cell_lines(
            &Cell::Thinking {
                text: "…".into(),
                seconds: Some(12),
            },
            40,
        );
        assert_eq!(line_text(&sealed[0]), "∗ Thought for 12s");
        // Sealed without timing (resumed): a bare verb.
        let bare = cell_lines(
            &Cell::Thinking {
                text: "…".into(),
                seconds: None,
            },
            40,
        );
        assert_eq!(line_text(&bare[0]), "∗ Thought");
    }

    #[test]
    fn transcript_renders_all_cell_kinds() {
        let cells = vec![
            Cell::User("do the thing".into()),
            Cell::Tool {
                name: "bash".into(),
                input: "{\"command\":\"ls\"}".into(),
                status: ToolStatus::Ok,
                output: None,
            },
            Cell::Tool {
                name: "bash".into(),
                input: "{}".into(),
                status: ToolStatus::Running,
                output: None,
            },
            Cell::Note("compacting history".into()),
            // Assistant text now renders as markdown: a single newline inside a
            // paragraph is a soft break and reflows to a space.
            Cell::Assistant("done.\nall good".into()),
        ];
        let lines = transcript_lines(&cells, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(
            texts,
            vec![
                "", // blank separator opening the user turn
                "> do the thing",
                // Human-readable tool rows (plan 38 slice 2): verb + argument,
                // status-marked (● running / ✓ ok / ✗ fail).
                "✓ Bash $ ls",
                "● Bash $ ",
                "[compacting history]",
                "done. all good",
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
        assert_eq!(
            commit_count(&five, 40, 10, |_| false),
            0,
            "fits: commit nothing"
        );
        // total 5 > 3: commit the front 2 so the last 3 fit.
        assert_eq!(commit_count(&five, 40, 3, |_| false), 2);
        // Never commit the last cell even if the region is tiny.
        assert_eq!(commit_count(&five, 40, 1, |_| false), 4);
        // A non-last display item can still receive deltas/completion by id and
        // must never be frozen into immutable scrollback.
        assert_eq!(commit_count(&five, 40, 1, |index| index == 0), 0);

        // A running tool at the front is not committable, so it holds the line
        // (and everything behind it) until it finishes — as long as the backlog
        // stays under the force-commit cap.
        let mut cells = vec![Cell::Tool {
            name: "bash".into(),
            input: "{}".into(),
            status: ToolStatus::Running,
            output: None,
        }];
        cells.extend((0..3).map(|i| Cell::Assistant(format!("l{i}"))));
        assert_eq!(
            commit_count(&cells, 40, 2, |_| false),
            0,
            "running front pins the tail"
        );
    }

    #[test]
    fn running_background_row_pins_tail_until_hard_cap_forces_freeze() {
        let running = Cell::BackgroundTask(kloop_core::event::BackgroundTask {
            id: "agent-1".into(),
            run_id: None,
            kind: BackgroundTaskKind::Agent,
            description: "long audit".into(),
            status: BackgroundTaskStatus::Running,
            output_path: None,
            detail: None,
        });
        let mut under_cap = vec![running.clone()];
        under_cap.extend((0..3).map(|i| Cell::Assistant(format!("l{i}"))));
        assert_eq!(commit_count(&under_cap, 40, 2, |_| false), 0);

        let mut over_cap = vec![running];
        over_cap.extend((0..10).map(|i| Cell::Assistant(format!("l{i}"))));
        assert!(
            commit_count(&over_cap, 40, 2, |_| false) > 0,
            "the hard cap must prevent an unbounded mutable tail"
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

    #[test]
    fn background_rows_render_typed_identity_phase_and_terminal_artifact() {
        let workflow = Cell::BackgroundTask(kloop_core::event::BackgroundTask {
            id: "workflow-3".into(),
            run_id: Some("wf_3".into()),
            kind: BackgroundTaskKind::Workflow,
            description: "review changes".into(),
            status: BackgroundTaskStatus::Running,
            output_path: None,
            detail: Some("Verify 2/4".into()),
        });
        let workflow_lines = cell_lines(&workflow, 80);
        assert_eq!(
            workflow_lines.iter().map(line_text).collect::<Vec<_>>(),
            vec![
                "● Workflow(review changes)",
                "  Running · workflow-3 · resumable as wf_3",
                "  Phase: Verify 2/4",
            ]
        );
        assert_eq!(workflow_lines[0].spans[0].style.fg, Some(Color::Cyan));

        let program = Cell::BackgroundTask(kloop_core::event::BackgroundTask {
            id: "program-2".into(),
            run_id: Some("run-2".into()),
            kind: BackgroundTaskKind::Program,
            description: "run checks".into(),
            status: BackgroundTaskStatus::Failed,
            output_path: Some("/tmp/run-2/error.txt".into()),
            detail: Some("exit code 1".into()),
        });
        let program_lines = cell_lines(&program, 80);
        assert_eq!(
            program_lines.iter().map(line_text).collect::<Vec<_>>(),
            vec![
                "✗ Program(run checks)",
                "  Failed · program-2 · resumable as run-2",
                "  exit code 1",
                "  output: /tmp/run-2/error.txt",
            ]
        );
        assert_eq!(program_lines[0].spans[0].style.fg, Some(Color::Red));
    }

    #[test]
    fn agent_message_row_shows_route_id_summary_and_typed_status() {
        let cell = Cell::AgentMessage(kloop_core::event::AgentMessageUpdate {
            id: "message-12".parse().unwrap(),
            from: "agent-4".parse().unwrap(),
            to: "main".parse().unwrap(),
            summary: "review shutdown ordering".into(),
            status: AgentMessageStatus::Delivered,
        });
        let lines = cell_lines(&cell, 80);
        assert_eq!(
            lines.iter().map(line_text).collect::<Vec<_>>(),
            vec![
                "✓ Message from Agent · agent-4",
                "  Delivered to main · message-12",
                "  review shutdown ordering",
            ]
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn background_status_style_depends_only_on_typed_status() {
        for (status, mark, color) in [
            (BackgroundTaskStatus::Running, "●", Color::Cyan),
            (BackgroundTaskStatus::Completed, "✓", Color::Green),
            (BackgroundTaskStatus::Failed, "✗", Color::Red),
            (BackgroundTaskStatus::Cancelled, "■", Color::Gray),
        ] {
            let cell = Cell::BackgroundTask(kloop_core::event::BackgroundTask {
                id: "agent-1".into(),
                run_id: None,
                kind: BackgroundTaskKind::Agent,
                description: "detail says failed cancelled completed".into(),
                status,
                output_path: None,
                detail: Some("running failed cancelled completed".into()),
            });
            let lines = cell_lines(&cell, 80);
            assert!(line_text(&lines[0]).starts_with(mark));
            assert_eq!(lines[0].spans[0].style.fg, Some(color));
        }
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
    fn long_tool_rows_truncate_the_header_to_one_line() {
        let input = format!(r#"{{"command":"{}"}}"#, "x".repeat(100));
        let cells = vec![Cell::Tool {
            name: "bash".into(),
            input,
            status: ToolStatus::Failed,
            output: None,
        }];
        let lines = transcript_lines(&cells, 20);
        assert_eq!(lines.len(), 1);
        let text = line_text(&lines[0]);
        assert!(text.starts_with("✗ Bash $ x"), "{text}");
        assert!(text.ends_with('…'));
    }
}
