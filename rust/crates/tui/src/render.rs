//! Rendering: pure functions from app state to lines (unit-tested), plus the
//! one ratatui draw entry point that owns layout and the confirm popup.

use ratatui::Frame;
use ratatui::layout::Constraint;
use ratatui::layout::Layout;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Clear;
use ratatui::widgets::Paragraph;

use kloop_core::event::AgentMessageStatus;
use kloop_core::event::BackgroundTaskKind;
use kloop_core::event::BackgroundTaskStatus;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::tools::TaskGraphSnapshot;
use kloop_core::tools::TaskGraphTask;
use kloop_core::tools::TaskStatus;
use kloop_protocol::RoutePickerStage;

use std::collections::HashSet;
use std::time::Duration;

use crate::app::App;
use crate::app::Cell;
use crate::app::ForkPicker;
use crate::app::PendingInteraction;
use crate::app::PendingQuestion;
use crate::app::ProviderPicker;
use crate::app::QuestionPhase;
use crate::app::ToolStatus;
use crate::app::confirm_choices;
use crate::choice;
use crate::menu;
use crate::menu::Popup;
use crate::text_layout::display_width;
use crate::text_layout::truncate;
use crate::text_layout::wrap;

pub(crate) const DIM: Style = Style::new().add_modifier(Modifier::DIM);
/// kloop's brand accent: a vivid cyan-blue marks the agent's own presence —
/// the session banner, the working spinner, and the mode badge. Interactive
/// input/selection/status stays ANSI cyan, green marks success/additions, red
/// marks errors/deletions, and secondary text is dim.
pub(crate) const BRAND: Color = Color::Rgb(79, 179, 200);

/// Rows a single [`Cell::Note`] may claim once wrapped. Provider errors carry
/// the response body (bounded at 64 KiB upstream), which would otherwise push
/// the whole turn out of the viewport.
const NOTE_MAX_LINES: usize = 10;

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

const TASK_PANEL_MAX_ROWS: usize = 8;
const TASK_PANEL_COMPLETED_LIMIT: usize = 3;

#[derive(Debug)]
pub struct LiveChromeLayout {
    pub activity_visible: bool,
    pub activity_spacer: bool,
    pub task_lines: Vec<Line<'static>>,
    /// The inline choice panel — an approval, a question, a picker — laid out to
    /// the rows it may occupy. None when nothing owns the keyboard. It is chrome
    /// like the rest: it never enters native scrollback, and its rows must be in
    /// the frozen-height budget or a commit would scroll it off (plan 99/103).
    pub panel: Option<choice::Layout>,
}

impl LiveChromeLayout {
    pub fn reserved_rows(&self) -> usize {
        usize::from(self.activity_visible)
            + usize::from(self.activity_spacer)
            + self.task_lines.len()
            // The panel carries one blank row above it, separating it from the
            // transcript.
            + self.panel.as_ref().map_or(0, |panel| panel.lines.len() + 1)
    }
}

/// A choice panel never takes more than this many rows, however tall the
/// terminal: a long diff or plan scrolls (PgUp/PgDn) rather than pushing the
/// conversation that led to the prompt off screen.
const PANEL_MAX_ROWS: usize = 20;

fn task_panel_allowed(app: &App) -> bool {
    app.show_task_graph
        && app.interactions.is_empty()
        && app.fork_picker.is_none()
        && app.provider_picker.is_none()
        && app.popup.is_none()
        && app.live_task_graph().is_some()
}

/// Compute all mutable chrome that lives between transcript cells and the
/// composer. Draw and native-scrollback commit use this exact helper so a Task
/// row can never be counted on screen but omitted from the frozen-height budget.
pub fn live_chrome_layout(app: &App, viewport: Rect) -> LiveChromeLayout {
    let width = usize::from(viewport.width).max(1);
    let terminal_height = usize::from(viewport.height);
    let activity_visible = has_activity_line(app);
    let activity_spacer = activity_visible && !app.cells.is_empty();
    let activity_rows = usize::from(activity_visible) + usize::from(activity_spacer);
    let fixed_bottom = 2 + composer_height(app, width) + 1;
    let transcript_capacity = terminal_height.saturating_sub(fixed_bottom).max(1);
    // The panel is the user's whole job while it is up, so it is served before
    // the task list — but never all the way to the top: two rows are held back
    // so the separator and at least one line of transcript survive.
    let panel = active_panel(app, width).map(|panel| {
        let rows = transcript_capacity
            .saturating_sub(activity_rows + 2)
            .clamp(1, PANEL_MAX_ROWS);
        choice::panel_lines(&panel, width, rows, app.panel_scroll)
    });
    let panel_rows = panel.as_ref().map_or(0, |panel| panel.lines.len() + 1);
    let max_task_rows = transcript_capacity
        .saturating_sub(activity_rows)
        .saturating_sub(panel_rows)
        .saturating_sub(1)
        .min(TASK_PANEL_MAX_ROWS);
    let task_lines = if task_panel_allowed(app) && max_task_rows > 0 {
        task_panel_lines(
            app.live_task_graph().expect("allowed graph exists"),
            width,
            max_task_rows,
        )
    } else {
        Vec::new()
    };
    LiveChromeLayout {
        activity_visible,
        activity_spacer,
        task_lines,
        panel,
    }
}

fn open_blockers<'a>(task: &'a TaskGraphTask, completed: &HashSet<&str>) -> Vec<&'a str> {
    let mut blockers = task
        .blocked_by
        .iter()
        .map(String::as_str)
        .filter(|id| !completed.contains(id))
        .collect::<Vec<_>>();
    blockers.sort_by_key(|id| id.parse::<u64>().unwrap_or(u64::MAX));
    blockers
}

fn task_line(
    task: &TaskGraphTask,
    completed: &HashSet<&str>,
    first: bool,
    width: usize,
) -> Line<'static> {
    // `⎿` is one column wide (neutral width), so the continuation indent is two
    // spaces — three would push every row after the first one column right of
    // the glyph it is meant to line up under.
    let prefix = if first { "⎿ " } else { "  " };
    let (glyph, glyph_style, subject_style) = match task.status {
        TaskStatus::InProgress => (
            "◼",
            Style::new().fg(Color::Cyan),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        TaskStatus::Pending => ("◻", DIM, DIM),
        TaskStatus::Completed => (
            "✔",
            Style::new().fg(Color::Green),
            DIM.add_modifier(Modifier::CROSSED_OUT),
        ),
    };
    let fixed_width = display_width(prefix) + display_width(glyph) + 1;
    let body_width = width.saturating_sub(fixed_width);
    let blockers = if task.status == TaskStatus::Pending {
        open_blockers(task, completed)
    } else {
        Vec::new()
    };
    let hint = (!blockers.is_empty()).then(|| {
        format!(
            " › blocked by {}",
            blockers
                .iter()
                .map(|id| format!("#{id}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });

    let (subject, hint) = match hint {
        None => (truncate(&task.subject, body_width.max(1)), None),
        Some(hint) => {
            let hint_budget = display_width(&hint).min((body_width / 2).max(1));
            let hint = truncate(&hint, hint_budget);
            let subject_budget = body_width.saturating_sub(display_width(&hint)).max(1);
            (truncate(&task.subject, subject_budget), Some(hint))
        }
    };
    let mut spans = vec![
        Span::styled(prefix.to_string(), DIM),
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" "),
        Span::styled(subject, subject_style),
    ];
    if let Some(hint) = hint {
        spans.push(Span::styled(hint, DIM));
    }
    Line::from(spans)
}

fn task_summary_line(label: String, first: bool, width: usize) -> Line<'static> {
    let prefix = if first { "⎿ " } else { "  " };
    let budget = width.saturating_sub(display_width(prefix)).max(1);
    Line::from(vec![
        Span::styled(prefix.to_string(), DIM),
        Span::styled(truncate(&label, budget), DIM),
    ])
}

fn visible_task_counts(unfinished: usize, completed: usize, cap: usize) -> (usize, usize) {
    let mut best = (0, 0);
    for visible_unfinished in 0..=unfinished.min(cap) {
        for visible_completed in 0..=completed.min(TASK_PANEL_COMPLETED_LIMIT).min(cap) {
            let rows = visible_unfinished
                + visible_completed
                + usize::from(visible_unfinished < unfinished)
                + usize::from(visible_completed < completed);
            if rows <= cap && (visible_unfinished, visible_completed) > best {
                best = (visible_unfinished, visible_completed);
            }
        }
    }
    best
}

pub fn task_panel_lines(
    snapshot: &TaskGraphSnapshot,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if snapshot.tasks.is_empty() || max_rows == 0 || width < 8 {
        return Vec::new();
    }
    let completed_ids = snapshot
        .tasks
        .iter()
        .filter(|task| task.status == TaskStatus::Completed)
        .map(|task| task.id.as_str())
        .collect::<HashSet<_>>();
    let mut in_progress = Vec::new();
    let mut pending = Vec::new();
    let mut blocked = Vec::new();
    let mut completed = Vec::new();
    for task in &snapshot.tasks {
        match task.status {
            TaskStatus::InProgress => in_progress.push(task),
            TaskStatus::Pending if open_blockers(task, &completed_ids).is_empty() => {
                pending.push(task)
            }
            TaskStatus::Pending => blocked.push(task),
            TaskStatus::Completed => completed.push(task),
        }
    }
    let unfinished = in_progress
        .into_iter()
        .chain(pending)
        .chain(blocked)
        .collect::<Vec<_>>();
    let cap = max_rows.min(TASK_PANEL_MAX_ROWS);
    let (visible_unfinished, visible_completed) =
        visible_task_counts(unfinished.len(), completed.len(), cap);
    if visible_unfinished == 0
        && visible_completed == 0
        && usize::from(!unfinished.is_empty()) + usize::from(!completed.is_empty()) > cap
    {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for task in unfinished.iter().take(visible_unfinished) {
        lines.push(task_line(task, &completed_ids, lines.is_empty(), width));
    }
    let hidden_unfinished = unfinished.len().saturating_sub(visible_unfinished);
    if hidden_unfinished > 0 {
        lines.push(task_summary_line(
            format!("… +{hidden_unfinished} unfinished"),
            lines.is_empty(),
            width,
        ));
    }
    for task in completed.iter().take(visible_completed) {
        lines.push(task_line(task, &completed_ids, lines.is_empty(), width));
    }
    let hidden_completed = completed.len().saturating_sub(visible_completed);
    if hidden_completed > 0 {
        lines.push(task_summary_line(
            format!("… +{hidden_completed} completed"),
            lines.is_empty(),
            width,
        ));
    }
    debug_assert!(lines.len() <= cap);
    lines
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
        Cell::TurnEnd(seconds) => {
            // A dim full-width rule with the turn's elapsed set into its left
            // end: the closing counterpart to the running `Working (…)` line,
            // and the seam between two turns in scrollback.
            let label = format!("── Worked for {} ", crate::anim::format_elapsed(*seconds));
            let rule = match width.checked_sub(display_width(&label)) {
                Some(tail) => format!("{label}{}", "─".repeat(tail)),
                None => truncate(&label, width),
            };
            lines.push(Line::default());
            lines.push(Line::from(Span::styled(rule, DIM)));
        }
        Cell::Note(text) => {
            // Notes carry provider failures, whose text routinely runs past the
            // terminal width, so wrap instead of cutting the tail off. The body
            // of an HTTP error is only bounded at 64 KiB upstream, so cap the
            // rows a single note may claim and say how many were dropped.
            let wrapped = wrap(&format!("[{text}]"), width.saturating_sub(1).max(1));
            let dropped = wrapped.len().saturating_sub(NOTE_MAX_LINES);
            for (i, l) in wrapped.into_iter().take(NOTE_MAX_LINES).enumerate() {
                let indent = if i == 0 { "" } else { " " };
                lines.push(Line::from(Span::styled(format!("{indent}{l}"), DIM)));
            }
            if dropped > 0 {
                lines.push(Line::from(Span::styled(
                    format!(" …(+{dropped} more line(s))"),
                    DIM,
                )));
            }
        }
        Cell::System(text) => {
            // Slash-command output: dim, and wrapped in full — /help and /cost
            // are multi-line by design, so no line cap like a Note's.
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
        let mut rendered = match cell {
            Cell::Assistant(text) if i == last && app.streaming_assistant() => {
                crate::markdown::assistant_stream_lines(text, width)
            }
            Cell::Thinking { .. } if i == last && app.streaming_thinking() => {
                let secs = hud.thinking.map(|d| d.as_secs()).unwrap_or(0);
                vec![thinking_line(None, Some(secs), width)]
            }
            _ => cell_lines(cell, width),
        };
        // A head cell too tall for the viewport has had its overflowing prefix
        // frozen into scrollback ([`head_freeze_lines`]); the viewport resumes
        // the same cell one line below the seam.
        if i == 0 {
            let skip = app.head_skip(width).min(rendered.len());
            rendered.drain(..skip);
        }
        lines.extend(rendered);
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
        // Keep the live tail at least a viewport tall: never commit a cell when
        // doing so would strand a short remainder (e.g. a tall final message
        // trailed by a one-line note on resume) behind a full-screen blank pad
        // (plan 99). The last cell is already excluded by the loop condition.
        if remaining - heights[committed] < active_h {
            break;
        }
        remaining -= heights[committed];
        committed += 1;
    }
    committed
}

/// How many of the head cell's leading rendered lines belong in native
/// scrollback so the live tail fits `active_h` rows — the line-level remainder
/// [`commit_count`] cannot take, because it only ever moves whole cells and
/// never the last one. Returns the new total (never below `frozen`).
///
/// One cell can be taller than the whole viewport: a replayed final answer, a
/// long tool output. Committing it whole would leave a one-line note alone
/// behind a full-screen blank pad (plan 99), so `commit_count` keeps it — and
/// then [`draw`] bottom-anchors the tail and clips the top, which is content the
/// user can never reach: it is neither on screen nor in scrollback. Freezing
/// exactly the overflow instead keeps the seam continuous — scrollback ends
/// where the viewport begins — and nothing is dropped.
///
/// The head is left alone while it can still change: a cell whose lines may
/// re-wrap (a code fence closing, a table gaining a row) must not have half of
/// it already nailed into scrollback. The last cell is never touched at all,
/// for the same reason [`commit_count`] leaves it.
pub fn head_freeze_lines(
    cells: &[Cell],
    width: usize,
    active_h: usize,
    frozen: usize,
    head_live: bool,
) -> usize {
    let active_h = active_h.max(1);
    if cells.len() < 2 || head_live || !is_committable(&cells[0]) {
        return frozen;
    }
    let head_h = cell_lines(&cells[0], width).len();
    let rest: usize = cells[1..].iter().map(|c| cell_lines(c, width).len()).sum();
    let live = (head_h + rest).saturating_sub(frozen);
    if live <= active_h {
        return frozen;
    }
    // Freeze only what overflows, and never past the head's own last line:
    // whole cells after it are `commit_count`'s business.
    (frozen + (live - active_h)).min(head_h)
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
    // A choice panel says what it is waiting for in its own header, so the
    // activity row would only repeat it — and the panel needs the rows more.
    app.ctrl_c_exit_armed || app.esc_clear_armed || (app.running && !app.interaction_active())
}

/// What Esc does from here, so the two hint lines never advertise a key that
/// would do something else: a draft in the composer is cleared first, and only
/// an empty composer lets Esc reach the running turn.
fn esc_action(app: &App) -> &'static str {
    if app.composer.is_blank() {
        "esc to interrupt"
    } else {
        "esc esc to clear input"
    }
}

/// The dynamic "what's happening now" line, shown at the BOTTOM of the
/// transcript (just above the composer) where it is most prominent — the eye
/// lands here, not on the footer (plan 38 slice 5). Running: an animated spinner
/// + a shimmering verb + `(elapsed · what esc does)`. `None` when idle.
pub fn activity_line(app: &App, hud: &Hud) -> Option<Line<'static>> {
    if app.ctrl_c_exit_armed {
        // Unmissable, right above the composer where Ctrl+C was pressed.
        return Some(Line::from("press Ctrl+C again to exit".to_string()));
    }
    // Same slot, same shape: the draft is still there until the second press.
    if app.esc_clear_armed {
        return Some(Line::from("press esc again to clear input".to_string()));
    }
    if app.interaction_active() || !app.running {
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
            " ({} · {})",
            crate::anim::format_elapsed(secs),
            esc_action(app)
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
    // A choice panel prints its own key hint on its last row; the footer stays
    // still underneath it rather than saying the same thing in other words.
    if app.popup.is_some() {
        return Line::from(Span::styled(
            "↑↓ choose · Tab/⏎ complete · Esc cancel".to_string(),
            DIM,
        ));
    }
    // The mode badge carries the brand accent (CC's brand-coloured mode line);
    // the hints stay dim so only the badge draws the eye.
    let badge = format!("[{}]  ", app.mode.label());
    let mut hints = if app.running {
        format!("{} · Ctrl+C to exit", esc_action(app))
    } else {
        "shift+Tab to change mode · Ctrl+R to rewind · Ctrl+C to exit".to_string()
    };
    if app.live_task_graph().is_some() {
        hints.push_str(if app.show_task_graph {
            " · ctrl+t to hide tasks"
        } else {
            " · ctrl+t to show tasks"
        });
    }
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

/// The footer's right-hand status: active provider/model/revision and the
/// context gauge. During an operation it deliberately reads the frozen route;
/// only after terminal settlement does it return to the idle selection.
fn system_status(app: &App) -> String {
    let route = app.display_route();
    let identity = route.map(|route| {
        // Effort only earns footer width when it is actually being sent.
        let effort = route
            .effort
            .map(|effort| format!(" · {effort}"))
            .unwrap_or_default();
        format!(
            "{} / {} · r{}{effort}",
            route.provider_id, route.model, route.revision
        )
    });
    if identity.is_none() && app.model.is_empty() {
        return String::new();
    }
    let identity = identity.unwrap_or_else(|| app.model.clone());
    match app.context_window {
        Some(window) if window > 0 => {
            let pct = (app.context_used as f64 / window as f64 * 100.0).round() as u64;
            format!("{identity} · {}% ctx", pct.min(100))
        }
        _ => format!("{identity} · ~{} tok", app.context_used),
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
    let labels: Vec<String> = app
        .composer
        .attachments()
        .iter()
        .map(|attachment| attachment.label.clone())
        .collect();
    let attach_h = u16::from(!labels.is_empty());
    let composer_h = (view.rows.len() as u16 + attach_h).max(1);
    let chrome = live_chrome_layout(app, full);
    let [
        transcript_area,
        rule_top,
        input_area,
        rule_bottom,
        footer_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(composer_h),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(full);

    let mut lines = visible_transcript(app, hud, width.max(1));
    // Activity and tasks are mutable live chrome, never transcript Cells. The
    // shared layout helper above also supplies commit_overflow's reserve.
    if chrome.activity_visible {
        if chrome.activity_spacer {
            lines.push(Line::default());
        }
        if let Some(activity) = activity_line(app, hud) {
            lines.push(activity);
        }
    }
    lines.extend(chrome.task_lines);
    // The choice panel closes the transcript: a blank row, then its own rows, so
    // it sits directly on the composer's top rule — the eye is already there.
    let panel_rows = chrome.panel.as_ref().map_or(0, |panel| panel.lines.len());
    if let Some(panel) = &chrome.panel {
        lines.push(Line::default());
        lines.extend(panel.lines.iter().cloned());
    }
    // Write the clamped body offset back, so over-scrolling self-corrects.
    app.panel_scroll = chrome.panel.as_ref().map_or(0, |panel| panel.scroll);
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

    // The panel takes the cursor only while it is taking text (a question's
    // Other / Notes phase). A list panel leaves it hidden: the composer is not
    // where the next keystroke goes.
    if let Some((row, column)) = chrome.panel.as_ref().and_then(|panel| panel.editor_cursor) {
        let y = transcript_area
            .bottom()
            .saturating_sub(panel_rows as u16)
            .saturating_add(row as u16);
        f.set_cursor_position((
            (transcript_area.x + column as u16).min(full.right().saturating_sub(1)),
            y.min(full.bottom().saturating_sub(1)),
        ));
    } else if chrome.panel.is_none() {
        // A completion menu (if open) floats just above the composer; the cursor
        // stays in the input, since the user is still typing the query.
        if let Some(popup) = &app.popup {
            draw_menu(f, popup, rule_top, width);
        }
        let cursor_col = view.cursor_col.min(input_area.width.saturating_sub(1));
        let cursor_row = (attach_h + view.cursor_row).min(input_area.height.saturating_sub(1));
        f.set_cursor_position((
            (input_area.x + cursor_col).min(full.right().saturating_sub(1)),
            (input_area.y + cursor_row).min(full.bottom().saturating_sub(1)),
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
/// cursor row is marked and coloured the same way a choice panel marks its own
/// (`> `, brand accent), so every list in the TUI reads alike. Pure and testable.
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
            let marker = if selected { "> " } else { "  " };
            let label_style = if selected {
                Style::new().fg(BRAND)
            } else {
                Style::default()
            };
            let text_w = width.saturating_sub(display_width(marker));
            let label = truncate(&item.label, text_w);
            let label_w = display_width(&label);
            let mut spans = vec![
                Span::styled(marker.to_string(), label_style),
                Span::styled(label, label_style),
            ];
            if !item.detail.is_empty() && label_w + 2 < text_w {
                let rest = truncate(&format!("  {}", item.detail), text_w - label_w);
                spans.push(Span::styled(rest, DIM));
            }
            Line::from(spans)
        })
        .collect()
}

/// The panel for whichever surface owns the keyboard, in the same precedence
/// the key router uses. None when the composer has it.
fn active_panel(app: &App, width: usize) -> Option<choice::Panel> {
    if let Some(interaction) = app.interactions.front() {
        return Some(match interaction {
            PendingInteraction::Confirm { req, cursor, .. } => confirm_panel(req, *cursor, width),
            PendingInteraction::Question(question) => question_panel(question, width),
        });
    }
    if let Some(picker) = &app.provider_picker {
        return Some(provider_panel(app, picker));
    }
    app.fork_picker.as_ref().map(fork_panel)
}

/// An approval: what is about to run, why it is being asked, and the numbered
/// answers. The rows come from [`confirm_choices`], which is also what the key
/// handler maps a press through — one list, one meaning.
fn confirm_panel(req: &ConfirmRequest, cursor: usize, width: usize) -> choice::Panel {
    // Structured fields when core supplied them, the flat description otherwise:
    // a request built by another frontend still has to render.
    let acted_on = req.detail.as_ref().filter(|_| req.title.is_some());
    let mut subject: Vec<Line<'static>> = wrap(acted_on.unwrap_or(&req.description), width)
        .into_iter()
        .map(Line::from)
        .collect();
    if let Some(notice) = &req.notice {
        // Why the gate stopped here — a hazard, a sub-agent, a missing sandbox.
        // Yellow and marked: this is the line that should give a fast "yes"
        // pause, and colour alone is a weak signal on some terminal themes.
        //
        // `⚠` is Neutral width, but terminals that give it emoji presentation
        // draw it two columns wide. Budget two either way (indenting the wrap
        // to match) so the row can never overflow into a spurious extra line.
        const MARK: &str = "⚠ ";
        const MARK_W: usize = 2;
        subject.extend(
            wrap(notice, width.saturating_sub(MARK_W).max(1))
                .into_iter()
                .enumerate()
                .map(|(index, line)| {
                    let lead = if index == 0 {
                        MARK.to_string()
                    } else {
                        " ".repeat(MARK_W)
                    };
                    Line::from(vec![
                        Span::styled(lead, Style::new().fg(Color::Yellow)),
                        Span::styled(line, Style::new().fg(Color::Yellow)),
                    ])
                }),
        );
    }
    let mut body: Vec<Line<'static>> = Vec::new();
    if let Some(preview) = &req.preview {
        if let Some(stats) = diff_stats_line(preview) {
            body.push(stats);
        }
        body.extend(diff_preview_lines(preview, width));
    }
    let items = confirm_choices(req)
        .into_iter()
        .map(|row| choice::Item {
            label: row.label,
            detail: row.detail,
        })
        .collect();
    choice::Panel {
        header: req
            .title
            .clone()
            .unwrap_or_else(|| "Permission needed".to_string()),
        subject,
        body,
        prompt: Some("Do you want to proceed?".to_string()),
        items,
        cursor,
        checked: Vec::new(),
        multi_select: false,
        hint: "Enter select · ↑↓ move · 1-9 pick · Esc deny".to_string(),
        editor: None,
    }
}

/// A model question. The two trailing rows are the escape hatches the model's
/// fixed options cannot cover: answer in your own words, or set the question
/// aside and just talk.
fn question_panel(question: &PendingQuestion, width: usize) -> choice::Panel {
    let current = question.current();
    let total = question.req.questions.len();
    let header = if total > 1 {
        format!(
            "{} ({}/{})",
            current.header,
            question.question_index + 1,
            total
        )
    } else {
        current.header.clone()
    };
    let mut body: Vec<Line<'static>> = Vec::new();
    if let Some(preview) = question.selected_preview() {
        body.extend(
            preview
                .lines()
                .flat_map(|line| wrap(line, width))
                .map(Line::from),
        );
    }
    let mut items: Vec<choice::Item> = current
        .options
        .iter()
        .map(|option| choice::Item::with_detail(option.label.clone(), option.description.clone()))
        .collect();
    items.push(choice::Item::new("Type something else"));
    items.push(choice::Item::new("Chat about this instead"));
    let (hint, editor) = match question.phase {
        QuestionPhase::Select if current.multi_select => (
            "Space toggle · Enter submit · ↑↓ move · 1-9 pick · Esc cancel",
            None,
        ),
        QuestionPhase::Select => ("Enter select · ↑↓ move · 1-9 pick · Esc cancel", None),
        QuestionPhase::Other => (
            "Enter submit · Esc cancel",
            Some(choice::Editor {
                prefix: "Your answer > ".to_string(),
                text: question.editor.clone(),
            }),
        ),
        QuestionPhase::Notes => (
            "Enter submit · Esc cancel",
            Some(choice::Editor {
                prefix: "Notes (optional) > ".to_string(),
                text: question.editor.clone(),
            }),
        ),
    };
    choice::Panel {
        header,
        subject: Vec::new(),
        body,
        prompt: Some(current.question.clone()),
        items,
        cursor: question.cursor,
        checked: question.selected.clone(),
        multi_select: current.multi_select,
        hint: hint.to_string(),
        editor,
    }
}

fn provider_panel(app: &App, picker: &ProviderPicker) -> choice::Panel {
    // Esc at the entry stage closes the panel, so the hint has to say so: the
    // same key is "back" two stages in and "cancel" at the one the command
    // opened.
    let hint = format!(
        "Enter select · ↑↓ move · 1-9 pick · Esc {}",
        if picker.stage == picker.catalog.stage {
            "cancel"
        } else {
            "back"
        }
    );
    match picker.stage {
        RoutePickerStage::Provider => choice::Panel {
            header: "Provider".to_string(),
            items: picker
                .catalog
                .providers
                .iter()
                .map(|provider| {
                    choice::Item::with_detail(
                        provider.id.clone(),
                        format!(
                            "{} · {:?}",
                            app.picker_model(provider),
                            provider.availability
                        ),
                    )
                })
                .collect(),
            cursor: picker.provider_cursor,
            prompt: Some("Which provider?".to_string()),
            hint,
            ..choice::Panel::default()
        },
        RoutePickerStage::Model => choice::Panel {
            header: format!("Model · {}", picker.provider().id),
            items: picker
                .models()
                .iter()
                .map(|model| {
                    if model == &picker.provider().default_model {
                        choice::Item::with_detail(model.clone(), "default")
                    } else {
                        choice::Item::new(model.clone())
                    }
                })
                .collect(),
            cursor: picker.model_cursor,
            prompt: Some("Which model?".to_string()),
            hint,
            ..choice::Panel::default()
        },
        RoutePickerStage::Effort => choice::Panel {
            header: format!("Effort · {}", picker.model()),
            items: picker
                .efforts()
                .into_iter()
                .map(|choice| {
                    choice::Item::with_detail(
                        kloop_protocol::ReasoningEffort::choice_str(choice).to_string(),
                        effort_detail(choice),
                    )
                })
                .collect(),
            cursor: picker.effort_cursor,
            prompt: Some("How much reasoning?".to_string()),
            hint,
            ..choice::Panel::default()
        },
    }
}

/// One line of plain language per level, kept to a single row so seven of them
/// still fit above the composer. `unset` and `none` are the pair people read as
/// synonyms and are not — measured 2026-09-17, a model asked with no field still
/// reasoned where `none` zeroed it — so their two lines say which is which.
/// `max` carries the warning its own measurement earned: two runs of one
/// question came back eight times apart.
fn effort_detail(choice: Option<kloop_protocol::ReasoningEffort>) -> &'static str {
    use kloop_protocol::ReasoningEffort;
    match choice {
        None => "send no effort field — the provider's own default applies",
        Some(ReasoningEffort::None) => "do no reasoning at all",
        Some(ReasoningEffort::Low) => "think briefly",
        Some(ReasoningEffort::Medium) => "think a moderate amount",
        Some(ReasoningEffort::High) => "think hard",
        Some(ReasoningEffort::XHigh) => "think harder",
        Some(ReasoningEffort::Max) => "think longest — measured cost is unpredictable",
    }
}

fn fork_panel(picker: &ForkPicker) -> choice::Panel {
    choice::Panel {
        header: "Rewind".to_string(),
        items: picker
            .points
            .iter()
            .map(|point| {
                choice::Item::with_detail(point.preview.clone(), format!("#{}", point.seq))
            })
            .collect(),
        cursor: picker.cursor,
        prompt: Some("Rewind the conversation to which point?".to_string()),
        hint: "Enter rewind · ↑↓ move · 1-9 pick · Esc cancel".to_string(),
        ..choice::Panel::default()
    }
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

    fn completion_target(kind: menu::PopupKind, query: &str) -> menu::CompletionTarget {
        let cursor = crate::text_layout::ByteOffset::new(query.len() + 1);
        menu::CompletionTarget {
            kind,
            query: query.into(),
            range: crate::text_layout::TextRange::new(crate::text_layout::ByteOffset::ZERO, cursor),
            cursor,
        }
    }

    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A cell exactly `rows` display lines tall: a System cell renders one row
    /// per source line, so the row index doubles as the line index.
    fn tall_cell(rows: usize) -> Cell {
        Cell::System(
            (0..rows)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    fn task(id: u64, subject: &str, status: TaskStatus, blocked_by: &[u64]) -> TaskGraphTask {
        TaskGraphTask {
            id: id.to_string(),
            subject: subject.into(),
            status,
            blocked_by: blocked_by.iter().map(u64::to_string).collect(),
            blocks: Vec::new(),
        }
    }

    fn task_graph(tasks: Vec<TaskGraphTask>) -> TaskGraphSnapshot {
        TaskGraphSnapshot { revision: 1, tasks }
    }

    #[test]
    fn brand_accent_is_vivid_cyan_blue() {
        assert_eq!(BRAND, Color::Rgb(79, 179, 200));
        assert_ne!(BRAND, Color::Magenta);
        assert_ne!(BRAND, Color::Cyan);
    }

    #[test]
    fn task_panel_sorts_styles_blocks_and_collapses_completed() {
        let snapshot = task_graph(vec![
            task(1, "Done one", TaskStatus::Completed, &[]),
            task(2, "Active", TaskStatus::InProgress, &[]),
            task(3, "Ready", TaskStatus::Pending, &[1]),
            task(4, "Blocked", TaskStatus::Pending, &[2, 1]),
            task(5, "Done five", TaskStatus::Completed, &[]),
            task(6, "Done six", TaskStatus::Completed, &[]),
            task(7, "Done seven", TaskStatus::Completed, &[]),
        ]);
        let lines = task_panel_lines(&snapshot, 80, TASK_PANEL_MAX_ROWS);
        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "⎿ ◼ Active",
                "  ◻ Ready",
                "  ◻ Blocked › blocked by #2",
                "  ✔ Done one",
                "  ✔ Done five",
                "  ✔ Done six",
                "  … +1 completed",
            ]
        );
        // Every row's glyph starts in the same display column: the `⎿` gutter is
        // one column wide, so the continuation rows indent by two, not three.
        assert!(
            texts.iter().all(|text| {
                let gutter = text
                    .chars()
                    .take_while(|ch| *ch == ' ' || *ch == '⎿')
                    .collect::<String>();
                display_width(&gutter) == 2
            }),
            "{texts:?}"
        );
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Cyan));
        assert!(
            lines[3].spans[3]
                .style
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        assert!(lines[3].spans[3].style.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn task_panel_hard_cap_preserves_accurate_group_summaries_and_width() {
        let mut tasks = (1..=12)
            .map(|id| task(id, &format!("未完成任务{id}"), TaskStatus::Pending, &[]))
            .collect::<Vec<_>>();
        tasks.extend(
            (13..=17).map(|id| task(id, &format!("已完成任务{id}"), TaskStatus::Completed, &[])),
        );
        let snapshot = task_graph(tasks);
        let lines = task_panel_lines(&snapshot, 18, TASK_PANEL_MAX_ROWS);
        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(lines.len(), TASK_PANEL_MAX_ROWS);
        assert!(texts.iter().any(|line| line == "  … +6 unfinished"));
        assert!(texts.iter().any(|line| line == "  … +5 completed"));
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line_text(line)) <= 18),
            "{texts:?}"
        );

        let completed_only = task_graph(
            (1..=5)
                .map(|id| task(id, &format!("Done {id}"), TaskStatus::Completed, &[]))
                .collect(),
        );
        assert_eq!(
            task_panel_lines(&completed_only, 40, 8)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec!["⎿ ✔ Done 1", "  ✔ Done 2", "  ✔ Done 3", "  … +2 completed"]
        );
    }

    #[test]
    fn a_finished_graph_leaves_the_chrome_and_the_footer_hint_with_the_turn() {
        let mut app = App::new("s".into());
        app.task_graph = Some(task_graph(vec![task(
            1,
            "Done",
            TaskStatus::Completed,
            &[],
        )]));
        let viewport = Rect::new(0, 0, 80, 24);
        assert_eq!(live_chrome_layout(&app, viewport).task_lines.len(), 1);
        assert!(line_text(&footer_line(&app, 140)).contains("ctrl+t to hide tasks"));

        app.apply(crate::events::AgentEvent::Core(
            kloop_core::event::Event::TurnEnded(kloop_core::agent::EndReason::Completed),
        ));
        assert!(
            live_chrome_layout(&app, viewport).task_lines.is_empty(),
            "the retired panel gives its rows back to the transcript"
        );
        assert!(!line_text(&footer_line(&app, 140)).contains("ctrl+t"));
    }

    #[test]
    fn live_chrome_hides_tasks_for_overlays_and_tiny_terminals() {
        let mut app = App::new("s".into());
        app.cells.push(Cell::Assistant("transcript".into()));
        app.task_graph = Some(task_graph(vec![task(
            1,
            "Visible",
            TaskStatus::Pending,
            &[],
        )]));
        let normal = live_chrome_layout(&app, Rect::new(0, 0, 80, 24));
        assert_eq!(normal.task_lines.len(), 1);
        assert_eq!(normal.reserved_rows(), 1);

        app.fork_picker = Some(crate::app::ForkPicker {
            points: Vec::new(),
            cursor: 0,
        });
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .task_lines
                .is_empty()
        );
        app.fork_picker = None;

        let (confirm_reply, _confirm_rx) = tokio::sync::oneshot::channel();
        app.apply(crate::events::AgentEvent::Confirm {
            req: kloop_core::permissions::ConfirmRequest {
                description: "confirm".into(),
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                remember_rules: None,
                preview: None,
                ..Default::default()
            },
            reply: confirm_reply,
        });
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .task_lines
                .is_empty()
        );
        app.interactions.clear();

        let (question_reply, _question_rx) = tokio::sync::oneshot::channel();
        app.apply(crate::events::AgentEvent::Question {
            req: kloop_core::interaction::QuestionRequest {
                questions: vec![kloop_core::interaction::Question {
                    question: "Choose?".into(),
                    header: "Choice".into(),
                    options: vec![kloop_core::interaction::QuestionOption {
                        label: "One".into(),
                        description: "first".into(),
                        preview: None,
                    }],
                    multi_select: false,
                }],
                metadata: None,
            },
            reply: question_reply,
        });
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .task_lines
                .is_empty()
        );
        app.interactions.clear();

        app.popup = Some(Popup {
            target: completion_target(menu::PopupKind::Slash, ""),
            items: vec![menu::MenuItem {
                label: "/help".into(),
                detail: "help".into(),
                insert: "/help".into(),
            }],
            cursor: 0,
        });
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .task_lines
                .is_empty()
        );
        app.popup = None;

        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 5))
                .task_lines
                .is_empty()
        );
        app.show_task_graph = false;
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .task_lines
                .is_empty()
        );
    }

    #[test]
    fn draw_keeps_activity_then_tasks_immediately_above_composer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new("s".into());
        app.cells.push(Cell::Assistant("history".into()));
        app.running = true;
        app.task_graph = Some(task_graph(vec![
            task(1, "Done one", TaskStatus::Completed, &[]),
            task(2, "Active", TaskStatus::InProgress, &[]),
            task(
                3,
                "需要处理一个非常非常长的中文任务标题",
                TaskStatus::Pending,
                &[2],
            ),
            task(4, "Done four", TaskStatus::Completed, &[]),
            task(5, "Done five", TaskStatus::Completed, &[]),
            task(6, "Done six", TaskStatus::Completed, &[]),
            task(7, "Done seven", TaskStatus::Completed, &[]),
        ]));
        let mut terminal = Terminal::new(TestBackend::new(50, 18)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let rows = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(""))
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        let activity = rows
            .iter()
            .position(|row| row.contains("Working"))
            .expect("activity row");
        let first_task = rows
            .iter()
            .position(|row| row.contains("⎿ ◼ Active"))
            .expect("first task row");
        let blocked = rows
            .iter()
            .position(|row| row.contains('需') && row.contains("blocked by #2"))
            .unwrap_or_else(|| panic!("blocked CJK task row: {rows:#?}"));
        assert!(
            rows[blocked].contains('…'),
            "CJK subject truncates: {rows:#?}"
        );
        let completed_summary = rows
            .iter()
            .position(|row| row.contains("… +2 completed"))
            .expect("completed folding row");
        let rule = rows
            .iter()
            .enumerate()
            .skip(blocked + 1)
            .find(|(_, row)| row.trim_matches('─').is_empty() && row.contains('─'))
            .map(|(index, _)| index)
            .expect("composer top rule");
        assert!(
            activity < first_task
                && first_task < blocked
                && blocked < completed_summary
                && completed_summary < rule
        );
        assert_eq!(
            app.cells,
            vec![Cell::Assistant("history".into())],
            "Task projection is not a transcript Cell"
        );
    }

    #[test]
    fn draw_ctrl_t_toggles_panel_and_footer_hint_together() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        fn screen(terminal: &Terminal<TestBackend>) -> String {
            let buffer = terminal.backend().buffer();
            (0..buffer.area.height)
                .map(|y| {
                    (0..buffer.area.width)
                        .map(|x| buffer.cell((x, y)).map(|cell| cell.symbol()).unwrap_or(""))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        let mut app = App::new("s".into());
        app.cells.push(Cell::Assistant("history".into()));
        app.task_graph = Some(task_graph(vec![task(
            1,
            "Toggle me",
            TaskStatus::Pending,
            &[],
        )]));
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();

        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();
        let visible = screen(&terminal);
        assert!(visible.contains("⎿ ◻ Toggle me"), "{visible}");
        assert!(visible.contains("ctrl+t to hide tasks"), "{visible}");

        app.on_key(80, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();
        let hidden = screen(&terminal);
        assert!(!hidden.contains("Toggle me"), "{hidden}");
        assert!(hidden.contains("ctrl+t to show tasks"), "{hidden}");

        app.on_key(80, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();
        assert!(screen(&terminal).contains("⎿ ◻ Toggle me"));
    }

    #[test]
    fn wrap_handles_cjk_newlines_and_narrow_width() {
        assert_eq!(wrap("你好世界", 4), vec!["你好", "世界"]);
        assert_eq!(wrap("ab\ncd", 10), vec!["ab", "cd"]);
        assert_eq!(wrap("", 10), vec![""]);
        // A wide grapheme never straddles the boundary or splits into codepoints.
        assert_eq!(wrap("a你b", 2), vec!["a", "你", "b"]);
        assert_eq!(wrap("a👩🏽‍💻b", 2), vec!["a", "👩🏽‍💻", "b"]);
        assert_eq!(wrap("ae\u{301}b", 2), vec!["ae\u{301}", "b"]);
        assert_eq!(wrap("abc", 0), vec!["a", "b", "c"]);
    }

    /// The turn closes on a dim full-width rule carrying its elapsed — the idle
    /// counterpart to the running `Working (…)` line, and the seam between two
    /// turns once both are in scrollback.
    #[test]
    fn turn_end_rule_carries_the_elapsed_and_fills_the_width() {
        let lines = cell_lines(&Cell::TurnEnd(725), 40);
        assert_eq!(line_text(&lines[0]), "", "a blank line sets the rule apart");
        let rule = line_text(&lines[1]);
        assert!(rule.starts_with("── Worked for 12m05s ─"), "{rule}");
        assert_eq!(display_width(&rule), 40);
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::DIM));
        // A terminal too narrow for the label truncates instead of overflowing.
        let narrow = cell_lines(&Cell::TurnEnd(5), 8);
        assert!(display_width(&line_text(&narrow[1])) <= 8);
    }

    /// A provider failure arrives as a note; its tail has to survive the wrap
    /// instead of being cut, with continuation rows aligned under the bracket.
    #[test]
    fn note_wraps_long_text_instead_of_truncating() {
        let cell = Cell::Note(
            "error: provider http error: openai-responses http 403: \
             {\"error\":{\"code\":\"model_not_allowed\"}}"
                .into(),
        );
        let lines = cell_lines(&cell, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(texts.len() > 1, "{texts:?}");
        assert!(texts[0].starts_with("[error: provider"), "{texts:?}");
        assert!(texts[1].starts_with(' '), "{texts:?}");
        assert!(texts.last().unwrap().ends_with("}}]"), "{texts:?}");
        assert!(
            texts
                .concat()
                .replace(" ", "")
                .contains("model_not_allowed")
        );
        for text in &texts {
            assert!(display_width(text) <= 40, "{text:?}");
        }
    }

    /// A pathological error body (the upstream cap is 64 KiB) must not push the
    /// turn out of the viewport: cap the rows and say how many were dropped.
    #[test]
    fn note_caps_rows_and_reports_the_dropped_ones() {
        let lines = cell_lines(&Cell::Note("x".repeat(1000)), 50);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts.len(), NOTE_MAX_LINES + 1);
        assert_eq!(texts.last().unwrap(), " …(+11 more line(s))");
    }

    #[test]
    fn truncate_marks_cut_content() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 5), "hello");
        assert_eq!(truncate("hello!", 5), "hell…");
        assert_eq!(truncate("你好世界", 5), "你好…");
        assert_eq!(truncate("👩🏽‍💻x", 2), "…");
        assert_eq!(truncate("e\u{301}x", 1), "…");
    }

    /// An approval reads as a header, the one thing being acted on, why it is
    /// being asked, and numbered answers — not one flat line of tags.
    #[test]
    fn confirm_panel_splits_subject_notice_and_numbered_answers() {
        use kloop_core::permissions::ApprovalScope;
        let req = ConfirmRequest {
            description: "[destructive] [no sandbox] bash: rm -rf build".into(),
            title: Some("Bash command".into()),
            detail: Some("rm -rf build".into()),
            notice: Some("destructive · no OS sandbox".into()),
            approval_scopes: vec![ApprovalScope::Once, ApprovalScope::WorkspaceSession],
            remember_rules: Some(vec!["bash(rm *)".into()]),
            preview: None,
        };
        let panel = confirm_panel(&req, 0, 60);
        assert_eq!(panel.header, "Bash command");
        assert_eq!(
            panel.subject.iter().map(line_text).collect::<Vec<_>>(),
            vec!["rm -rf build", "⚠ destructive · no OS sandbox"]
        );
        // The notice is the row that should slow down a reflexive yes.
        assert_eq!(
            panel.subject[1].spans[0].style,
            Style::new().fg(Color::Yellow)
        );
        assert_eq!(
            panel
                .items
                .iter()
                .map(|item| item.label.as_str())
                .collect::<Vec<_>>(),
            vec![
                "Yes",
                "Yes, and don't ask again this workspace session",
                "No, and tell kloop what to do differently",
            ]
        );

        let text = choice::panel_lines(&panel, 60, 20, 0)
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("▌ Bash command"), "{text}");
        assert!(text.contains("Do you want to proceed?"), "{text}");
        assert!(text.contains("> 1. Yes"), "cursor row marked:\n{text}");
        assert!(
            text.contains("  2. Yes, and don't ask again this workspace session"),
            "{text}"
        );
        // The remembered rule rides under the row that would remember it.
        assert!(text.contains("bash(rm *)"), "{text}");
        assert!(
            text.contains("  3. No, and tell kloop what to do differently"),
            "{text}"
        );
        assert!(
            text.contains("Enter select · ↑↓ move · 1-9 pick · Esc deny"),
            "{text}"
        );
    }

    /// A notice too long for the width wraps under its own mark rather than
    /// back to column zero, and the mark's width is budgeted whether or not the
    /// terminal draws `⚠` as an emoji.
    #[test]
    fn a_long_notice_wraps_under_its_mark() {
        let req = ConfirmRequest {
            description: "bash: x".into(),
            title: Some("Bash command".into()),
            detail: Some("x".into()),
            notice: Some("the OS sandbox blocked this — run it without the sandbox?".into()),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
        };
        let rows: Vec<String> = confirm_panel(&req, 0, 30)
            .subject
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(rows[0], "x");
        assert!(rows[1].starts_with("⚠ the OS sandbox"), "{rows:?}");
        assert!(rows.len() > 2, "the notice wrapped: {rows:?}");
        assert!(
            rows[2].starts_with("  ") && !rows[2].starts_with("   "),
            "continuation indents under the mark: {rows:?}"
        );
        // Two columns are budgeted for the mark, so every row still fits even
        // where the terminal draws it wide.
        assert!(rows.iter().all(|row| display_width(row) <= 30), "{rows:?}");
    }

    /// A request built without the structured fields (another frontend, an older
    /// caller) still renders: the flat description becomes the body.
    #[test]
    fn confirm_panel_falls_back_to_the_flat_description() {
        let req = ConfirmRequest {
            description: "bash: ls".into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            ..Default::default()
        };
        let panel = confirm_panel(&req, 0, 60);
        assert_eq!(panel.header, "Permission needed");
        assert_eq!(
            panel.subject.iter().map(line_text).collect::<Vec<_>>(),
            vec!["bash: ls"]
        );
    }

    /// The diff preview keeps its `+N -M` summary and colouring inside the
    /// panel body, where it scrolls.
    #[test]
    fn confirm_panel_body_carries_the_diff_summary() {
        let req = ConfirmRequest {
            description: "write_file: notes.txt".into(),
            title: Some("Write file".into()),
            detail: Some("notes.txt".into()),
            notice: None,
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: Some("+1  hello\n+2  world".into()),
        };
        let panel = confirm_panel(&req, 0, 40);
        assert_eq!(
            panel.subject.iter().map(line_text).collect::<Vec<_>>(),
            vec!["notes.txt"]
        );
        assert_eq!(
            panel.body.iter().map(line_text).collect::<Vec<_>>(),
            vec!["+2 -0", "+1  hello", "+2  world"]
        );
    }

    /// End-to-end through a real ratatui frame (TestBackend, no TTY): the panel
    /// is inline chrome sitting on the composer's top rule — no border, nothing
    /// cleared out from under the transcript. A diff taller than the panel
    /// windows, the options and hint stay put, and PgDn-style over-scroll
    /// self-corrects to the last window.
    #[test]
    fn draw_puts_the_panel_on_the_composer_and_scrolls_a_tall_diff() {
        use crate::events::AgentEvent;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
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
        app.cells.push(Cell::Assistant("earlier turn".into()));
        let (reply, _rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "write_file: big.txt".into(),
                title: Some("Write file".into()),
                detail: Some("big.txt".into()),
                notice: None,
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                remember_rules: None,
                preview: Some(preview),
            },
            reply,
        });

        let mut term = Terminal::new(TestBackend::new(64, 20)).unwrap();
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
        let rendered = rows(&term);
        let screen = rendered.join("\n");

        // The hint is the transcript's last row: 20 rows less the footer, the
        // two rules and a one-line composer leaves rows 0..=15 for it.
        assert!(
            rendered[15].contains("Enter select"),
            "hint sits on the composer's rule:\n{screen}"
        );
        assert!(
            rendered[13].contains("2. No, and tell kloop"),
            "options directly above the hint:\n{screen}"
        );
        assert!(
            screen.contains("big.txt"),
            "the file being written stays pinned:\n{screen}"
        );
        // Inline, not a popup: no border box, and the transcript is still there.
        assert!(!screen.contains('╭'), "no popup border:\n{screen}");
        assert!(
            screen.contains("earlier turn"),
            "transcript keeps its context:\n{screen}"
        );
        assert!(
            screen.contains("+1  line 1") && !screen.contains("+60  line 60"),
            "top of the diff visible, tail not:\n{screen}"
        );
        assert!(
            screen.contains("PgUp/PgDn scroll"),
            "the body advertises how to scroll:\n{screen}"
        );

        // Over-scroll self-corrects to the last window; the options stay put.
        app.panel_scroll = 999;
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
        let rendered = rows(&term);
        let screen = rendered.join("\n");
        assert!(app.panel_scroll < 999, "offset clamped to a valid range");
        assert!(
            screen.contains("+60  line 60") && !screen.contains("+1  line 1"),
            "tail visible, top scrolled off:\n{screen}"
        );
        assert!(
            rendered[15].contains("Enter select"),
            "hint still pinned:\n{screen}"
        );
    }

    /// A model question renders through the same panel: its own header, the
    /// question above numbered options with their descriptions underneath, and
    /// the two escape hatches the model's fixed options cannot cover.
    #[test]
    fn question_panel_numbers_options_and_offers_both_escape_hatches() {
        use kloop_core::interaction::Question;
        use kloop_core::interaction::QuestionOption;
        use kloop_core::interaction::QuestionRequest;

        let mut app = App::new("s".into());
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.apply(crate::events::AgentEvent::Question {
            req: QuestionRequest {
                questions: vec![Question {
                    question: "Which way?".into(),
                    header: "Next step".into(),
                    options: vec![
                        QuestionOption {
                            label: "Upgrade recovery".into(),
                            description: "swap the reused target for a fresh one".into(),
                            preview: None,
                        },
                        QuestionOption {
                            label: "Guardrail only".into(),
                            description: "block the riskiest path first".into(),
                            preview: None,
                        },
                    ],
                    multi_select: false,
                }],
                metadata: None,
            },
            reply,
        });

        let panel = active_panel(&app, 60).expect("a question owns the keyboard");
        assert_eq!(panel.header, "Next step");
        assert_eq!(panel.prompt.as_deref(), Some("Which way?"));
        assert_eq!(
            choice::panel_lines(&panel, 60, 30, 0)
                .lines
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec![
                "▌ Next step",
                "",
                "Which way?",
                "",
                "> 1. Upgrade recovery",
                "     swap the reused target for a fresh one",
                "  2. Guardrail only",
                "     block the riskiest path first",
                "  3. Type something else",
                "  4. Chat about this instead",
                "",
                "Enter select · ↑↓ move · 1-9 pick · Esc cancel",
            ]
        );
    }

    /// The rewind picker is the same panel, so its keys are the same keys.
    #[test]
    fn fork_panel_lists_rewind_points_the_same_way() {
        let mut app = App::new("s".into());
        app.fork_picker = Some(ForkPicker {
            points: vec![
                kloop_core::rollout::ForkPoint {
                    seq: 3,
                    preview: "fix the parser".into(),
                },
                kloop_core::rollout::ForkPoint {
                    seq: 7,
                    preview: "add the panel".into(),
                },
            ],
            cursor: 1,
        });
        let panel = active_panel(&app, 60).expect("the picker owns the keyboard");
        assert_eq!(
            choice::panel_lines(&panel, 60, 30, 0)
                .lines
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec![
                "▌ Rewind",
                "",
                "Rewind the conversation to which point?",
                "",
                "  1. fix the parser",
                "     #3",
                "> 2. add the panel",
                "     #7",
                "",
                "Enter rewind · ↑↓ move · 1-9 pick · Esc cancel",
            ]
        );
    }

    /// The menu marks the cursor row the way a choice panel does (`> `, brand)
    /// and windows a long list to keep the cursor visible near the bottom (the
    /// rows nearest the composer).
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
            target: completion_target(menu::PopupKind::Slash, ""),
            items,
            cursor: 12,
        };
        let lines = menu_lines(&popup, 40, 8);
        assert_eq!(lines.len(), 8, "windowed to max_rows");
        // The cursor row (12) is the last visible row, and it is reversed.
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert!(
            texts.last().unwrap().starts_with("> /cmd12"),
            "cursor row last and marked: {texts:?}"
        );
        assert_eq!(
            lines.last().unwrap().spans[0].style,
            Style::new().fg(BRAND),
            "selected row carries the brand accent"
        );
        // A non-selected row is indented to match and carries a dim detail span.
        assert!(texts[0].starts_with("  /cmd"), "{texts:?}");
        assert_eq!(lines[0].spans[0].style, Style::default());
        assert_eq!(lines[0].spans[2].style, DIM);
    }

    /// End-to-end (TestBackend): an open menu floats directly above the composer
    /// (its top rule), not in the footer, and shows the candidates.
    #[test]
    fn draw_floats_the_menu_above_the_composer() {
        let popup = Popup {
            target: completion_target(menu::PopupKind::Slash, "co"),
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

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
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
        assert!(
            lines[1].spans[1]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
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
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
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
    fn exact_width_cursor_renders_on_the_next_composer_row() {
        use ratatui::backend::Backend;

        let backend = ratatui::backend::TestBackend::new(6, 8);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut app = App::new("cursor".into());
        app.composer.paste("abcd");
        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();

        let cursor = terminal.backend_mut().get_cursor_position().unwrap();
        assert_eq!(cursor, ratatui::layout::Position::new(2, 5));
        assert_eq!(terminal.backend().buffer()[(5, 4)].symbol(), "d");
    }

    #[test]
    fn ultra_narrow_cursor_is_clamped_inside_the_frame() {
        use ratatui::backend::Backend;

        for width in [1, 2] {
            let backend = ratatui::backend::TestBackend::new(width, 5);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            let mut app = App::new(format!("width-{width}"));
            terminal
                .draw(|frame| draw(frame, &mut app, &Hud::default()))
                .unwrap();

            let cursor = terminal.backend_mut().get_cursor_position().unwrap();
            assert!(cursor.x < width, "width={width}, cursor={cursor:?}");
            assert!(cursor.y < 5, "width={width}, cursor={cursor:?}");
        }
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

        app.task_graph = Some(task_graph(vec![task(
            1,
            "Visible",
            TaskStatus::Pending,
            &[],
        )]));
        assert!(line_text(&footer_line(&app, 140)).contains("ctrl+t to hide tasks"));
        app.show_task_graph = false;
        assert!(line_text(&footer_line(&app, 140)).contains("ctrl+t to show tasks"));
        app.show_task_graph = true;

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

    #[test]
    fn footer_shows_selected_route_and_preserves_frozen_route_while_running() {
        let old = kloop_protocol::ActiveProviderRoute {
            revision: 4,
            provider_id: "alpha".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: "a-model".into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        };
        let new = kloop_protocol::ActiveProviderRoute {
            revision: 5,
            provider_id: "beta".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: "b-model".into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        };
        let mut app = App::new("s".into())
            .with_context("legacy-model".into(), Some(100), 25)
            .with_route(old.clone());
        assert!(system_status(&app).contains("alpha / a-model · r4"));

        app.running = true;
        app.freeze_selected_route();
        app.apply(crate::events::AgentEvent::ProviderChanged(new));
        assert!(system_status(&app).contains("alpha / a-model · r4"));

        app.apply(crate::events::AgentEvent::Core(
            kloop_core::event::Event::TurnEnded(kloop_core::agent::EndReason::Completed),
        ));
        assert!(system_status(&app).contains("beta / b-model · r5"));
    }

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
                "✓ Bash",
                "  $ ls",
                // Arguments still streaming: no command row at all, rather than
                // an empty one.
                "● Bash",
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

    /// Regression (plan 99): resuming a session builds short leading cells, a
    /// tall final assistant message, then a one-line note. Committing the tall
    /// message would leave only the note live behind a full-viewport blank pad —
    /// the exact `kloop -c` blank-gap bug. The commit must stop early so the live
    /// tail still fills the viewport.
    #[test]
    fn commit_keeps_a_tall_final_message_live_instead_of_a_blank_pad() {
        let width = 40;
        let active_h = 5;
        // A System cell renders one row per source line, so this is 8 rows tall.
        let tall = Cell::System(
            (0..8)
                .map(|i| format!("row {i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        let cells = vec![
            Cell::Assistant("intro 0".into()),
            Cell::Assistant("intro 1".into()),
            tall,
            Cell::Note("resumed session — 52 message(s)".into()),
        ];
        let n = commit_count(&cells, width, active_h, |_| false);
        assert!(
            n <= 2,
            "must not freeze the tall final message: committed {n}"
        );
        let live_tail: usize = cells[n..].iter().map(|c| cell_lines(c, width).len()).sum();
        assert!(
            live_tail >= active_h,
            "live tail {live_tail} must fill the {active_h}-row viewport",
        );
    }

    /// The other half of the plan-99 rule: `commit_count` keeps the tall final
    /// message live, and this freezes the lines of it that overflow the viewport
    /// anyway — otherwise `draw` bottom-anchors the tail and clips a top that is
    /// in no scrollback either, so a resumed answer's opening is unreachable.
    #[test]
    fn head_freeze_takes_exactly_the_tall_head_overflow() {
        let width = 40;
        let active_h = 5;
        // 20 rows of message, then the one-line resume note: 16 rows overflow.
        let cells = vec![
            tall_cell(20),
            Cell::Note("resumed session — 32 message(s)".into()),
        ];
        assert_eq!(
            commit_count(&cells, width, active_h, |_| false),
            0,
            "the tall message stays live (plan 99)"
        );
        let frozen = head_freeze_lines(&cells, width, active_h, 0, /*head_live=*/ false);
        assert_eq!(frozen, 16);
        // Seam: scrollback ends on the last frozen line, the viewport opens on
        // the next one, and together they are the whole message.
        let head = cell_lines(&cells[0], width);
        assert_eq!(line_text(&head[frozen - 1]), "row 15");
        assert_eq!(line_text(&head[frozen]), "row 16");
        let live: usize = head.len() - frozen + cell_lines(&cells[1], width).len();
        assert_eq!(live, active_h, "the live tail fills the viewport exactly");
    }

    /// Freezing is per frame and cumulative: a later turn pushes more rows in,
    /// and only the newly overflowing ones are added to what is already frozen.
    #[test]
    fn head_freeze_adds_only_the_new_overflow() {
        let width = 40;
        let cells = vec![
            tall_cell(20),
            Cell::Note("resumed session — 32 message(s)".into()),
            Cell::User("and now what".into()),
        ];
        // The User cell renders a leading blank plus its text: two more rows.
        assert_eq!(
            head_freeze_lines(&cells, width, 5, 16, /*head_live=*/ false),
            18
        );
    }

    /// A head that can still change keeps its lines: half a cell nailed into
    /// scrollback cannot be re-wrapped when the rest of it lands.
    #[test]
    fn head_freeze_leaves_a_settled_or_fitting_head_alone() {
        let width = 40;
        let active_h = 5;
        let tail = Cell::Note("resumed session — 32 message(s)".into());
        let running = Cell::Tool {
            name: "Bash".into(),
            input: "{}".into(),
            status: ToolStatus::Running,
            output: None,
        };
        let cases: Vec<(&str, Vec<Cell>, bool, usize)> = vec![
            (
                "fits the viewport",
                vec![tall_cell(3), tail.clone()],
                false,
                0,
            ),
            ("head is the last cell", vec![tall_cell(20)], false, 0),
            (
                "head still streaming",
                vec![tall_cell(20), tail.clone()],
                true,
                0,
            ),
            ("head still running", vec![running, tall_cell(20)], false, 0),
        ];
        let frozen: Vec<(&str, usize)> = cases
            .iter()
            .map(|(name, cells, head_live, _)| {
                (
                    *name,
                    head_freeze_lines(cells, width, active_h, 0, *head_live),
                )
            })
            .collect();
        assert_eq!(
            frozen,
            cases
                .iter()
                .map(|(name, _, _, want)| (*name, *want))
                .collect::<Vec<_>>()
        );
    }

    /// The viewport picks the head cell up one line past the seam — and a prefix
    /// frozen at another width says nothing about this wrapping, so it is ignored
    /// rather than cutting blind.
    #[test]
    fn visible_transcript_resumes_the_head_below_the_frozen_seam() {
        let mut app = App::new("s".into());
        app.cells = vec![tall_cell(4), Cell::Note("resumed session".into())];
        app.freeze_head_lines(40, 3);
        let shown = |app: &App, width: usize| -> Vec<String> {
            visible_transcript(app, &Hud::default(), width)
                .iter()
                .map(line_text)
                .collect()
        };
        assert_eq!(shown(&app, 40), vec!["row 3", "[resumed session]"]);
        assert_eq!(
            shown(&app, 30),
            vec!["row 0", "row 1", "row 2", "row 3", "[resumed session]"],
            "a freeze from another width does not cut this one"
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
    fn long_tool_rows_truncate_to_one_line_each() {
        let input = format!(r#"{{"command":"{}"}}"#, "x".repeat(100));
        let cells = vec![Cell::Tool {
            name: "bash".into(),
            input,
            status: ToolStatus::Failed,
            output: None,
        }];
        let lines = transcript_lines(&cells, 20);
        assert_eq!(lines.len(), 2, "header + command, neither of them wrapping");
        assert_eq!(line_text(&lines[0]), "✗ Bash");
        let text = line_text(&lines[1]);
        assert!(text.starts_with("  $ x"), "{text}");
        assert!(text.ends_with('…'));
    }
}
