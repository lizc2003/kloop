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
use kloop_core::permissions::ConfirmPreview;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::tools::TodoItem;
use kloop_core::tools::TodoSnapshot;
use kloop_core::tools::TodoStatus;
use kloop_protocol::RoutePickerStage;

use std::time::Duration;

use crate::app::App;
use crate::app::Cell;
use crate::app::ForkPicker;
use crate::app::PendingInteraction;
use crate::app::PendingQuestion;
use crate::app::PlanStatus;
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

const TODO_PANEL_MAX_ROWS: usize = 8;
const TODO_PANEL_COMPLETED_LIMIT: usize = 3;

#[derive(Debug)]
pub struct LiveChromeLayout {
    pub activity_visible: bool,
    pub activity_spacer: bool,
    /// A known slash command is parked in the composer while the turn runs; its
    /// hint renders as one more row under the activity line.
    pub held_command_visible: bool,
    pub todo_lines: Vec<Line<'static>>,
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
            + usize::from(self.held_command_visible)
            + self.todo_lines.len()
            // The panel carries one blank row above it, separating it from the
            // transcript.
            + self.panel.as_ref().map_or(0, |panel| panel.lines.len() + 1)
    }
}

/// A choice panel never takes more than this many rows, however tall the
/// terminal: a long diff scrolls (PgUp/PgDn) rather than pushing the
/// conversation that led to the prompt off screen.
const PANEL_MAX_ROWS: usize = 20;

fn todo_panel_allowed(app: &App) -> bool {
    app.show_todos
        && app.interactions.is_empty()
        && app.fork_picker.is_none()
        && app.provider_picker.is_none()
        && app.popup.is_none()
        && app.live_todos().is_some()
}

/// Compute all mutable chrome that lives between transcript cells and the
/// composer. Draw and native-scrollback commit use this exact helper so a todo
/// row can never be counted on screen but omitted from the frozen-height budget.
pub fn live_chrome_layout(app: &App, viewport: Rect) -> LiveChromeLayout {
    let width = usize::from(viewport.width).max(1);
    let terminal_height = usize::from(viewport.height);
    let activity_visible = has_activity_line(app);
    let activity_spacer = activity_visible && !app.cells.is_empty();
    let held_command_visible = has_held_command_line(app);
    let activity_rows = usize::from(activity_visible)
        + usize::from(activity_spacer)
        + usize::from(held_command_visible);
    let fixed_bottom = 2 + composer_height(app, width) + 1;
    let transcript_capacity = terminal_height.saturating_sub(fixed_bottom).max(1);
    // The panel is the user's whole job while it is up, so it is served before
    // the todo list — but never all the way to the top: two rows are held back
    // so the separator and at least one line of transcript survive.
    let panel = active_panel(app, width).map(|panel| {
        let rows = transcript_capacity
            .saturating_sub(activity_rows + 2)
            .clamp(1, PANEL_MAX_ROWS);
        choice::panel_lines(&panel, width, rows, app.panel_scroll)
    });
    let panel_rows = panel.as_ref().map_or(0, |panel| panel.lines.len() + 1);
    let max_todo_rows = transcript_capacity
        .saturating_sub(activity_rows)
        .saturating_sub(panel_rows)
        .saturating_sub(1)
        .min(TODO_PANEL_MAX_ROWS);
    let todo_lines = if todo_panel_allowed(app) && max_todo_rows > 0 {
        todo_panel_lines(
            app.live_todos().expect("allowed list exists"),
            width,
            max_todo_rows,
        )
    } else {
        Vec::new()
    };
    LiveChromeLayout {
        activity_visible,
        activity_spacer,
        held_command_visible,
        todo_lines,
        panel,
    }
}

fn todo_line(todo: &TodoItem, first: bool, width: usize) -> Line<'static> {
    // `⎿` is one column wide (neutral width), so the continuation indent is two
    // spaces — three would push every row after the first one column right of
    // the glyph it is meant to line up under.
    let prefix = if first { "⎿ " } else { "  " };
    let (glyph, glyph_style, subject_style) = match todo.status {
        TodoStatus::InProgress => (
            "◼",
            Style::new().fg(Color::Cyan),
            Style::new().add_modifier(Modifier::BOLD),
        ),
        TodoStatus::Pending => ("◻", DIM, DIM),
        TodoStatus::Completed => (
            "✔",
            Style::new().fg(Color::Green),
            DIM.add_modifier(Modifier::CROSSED_OUT),
        ),
    };
    let fixed_width = display_width(prefix) + display_width(glyph) + 1;
    let body_width = width.saturating_sub(fixed_width);
    Line::from(vec![
        Span::styled(prefix.to_string(), DIM),
        Span::styled(glyph.to_string(), glyph_style),
        Span::raw(" "),
        Span::styled(truncate(&todo.subject, body_width.max(1)), subject_style),
    ])
}

fn todo_summary_line(label: String, first: bool, width: usize) -> Line<'static> {
    let prefix = if first { "⎿ " } else { "  " };
    let budget = width.saturating_sub(display_width(prefix)).max(1);
    Line::from(vec![
        Span::styled(prefix.to_string(), DIM),
        Span::styled(truncate(&label, budget), DIM),
    ])
}

fn visible_todo_counts(unfinished: usize, completed: usize, cap: usize) -> (usize, usize) {
    let mut best = (0, 0);
    for visible_unfinished in 0..=unfinished.min(cap) {
        for visible_completed in 0..=completed.min(TODO_PANEL_COMPLETED_LIMIT).min(cap) {
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

pub fn todo_panel_lines(
    snapshot: &TodoSnapshot,
    width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if snapshot.todos.is_empty() || max_rows == 0 || width < 8 {
        return Vec::new();
    }
    let mut in_progress = Vec::new();
    let mut pending = Vec::new();
    let mut completed = Vec::new();
    for todo in &snapshot.todos {
        match todo.status {
            TodoStatus::InProgress => in_progress.push(todo),
            TodoStatus::Pending => pending.push(todo),
            TodoStatus::Completed => completed.push(todo),
        }
    }
    let unfinished = in_progress.into_iter().chain(pending).collect::<Vec<_>>();
    let cap = max_rows.min(TODO_PANEL_MAX_ROWS);
    let (visible_unfinished, visible_completed) =
        visible_todo_counts(unfinished.len(), completed.len(), cap);
    if visible_unfinished == 0
        && visible_completed == 0
        && usize::from(!unfinished.is_empty()) + usize::from(!completed.is_empty()) > cap
    {
        return Vec::new();
    }

    let mut lines = Vec::new();
    for todo in unfinished.iter().take(visible_unfinished) {
        lines.push(todo_line(todo, lines.is_empty(), width));
    }
    let hidden_unfinished = unfinished.len().saturating_sub(visible_unfinished);
    if hidden_unfinished > 0 {
        lines.push(todo_summary_line(
            format!("… +{hidden_unfinished} unfinished"),
            lines.is_empty(),
            width,
        ));
    }
    for todo in completed.iter().take(visible_completed) {
        lines.push(todo_line(todo, lines.is_empty(), width));
    }
    let hidden_completed = completed.len().saturating_sub(visible_completed);
    if hidden_completed > 0 {
        lines.push(todo_summary_line(
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
            // The whole message as markdown. A message still streaming as the
            // last cell is drawn by [`shown_cells`] through the streaming render
            // instead, which also says how much of it may freeze.
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
        Cell::Plan { text, status } => {
            // Markdown, like the sealed assistant cell — a plan is prose with
            // lists and code in it, and colouring it by leading +/- (what the
            // popup used to do to it) paints every list item as a deletion.
            // The bar and title keep it from reading as something the model
            // merely said: this one is waiting on an answer.
            lines.push(Line::default());
            // The same `▌ title` a panel header wears (`choice::header_line`):
            // one block with a name on it, which is what this is.
            let title = Style::new().fg(BRAND).add_modifier(Modifier::BOLD);
            lines.push(Line::from(vec![
                Span::styled("▌ ", title),
                Span::styled("Plan", title),
            ]));
            lines.extend(crate::markdown::markdown_lines(text, width));
            // Both answers are marked, not just the refusal: the cell is on
            // screen before there is an answer at all, so "no mark" already
            // means "not answered yet" and cannot also mean "approved".
            let outcome = match status {
                PlanStatus::Pending => None,
                PlanStatus::Approved => Some(("✓", "approved", Color::Green)),
                PlanStatus::Declined => Some(("✗", "not approved — still planning", Color::Yellow)),
            };
            if let Some((mark, label, color)) = outcome {
                lines.push(Line::from(vec![
                    Span::styled(format!("  {mark} "), Style::new().fg(color)),
                    Span::styled(truncate(label, width.saturating_sub(4)), DIM),
                ]));
            }
        }
        Cell::SessionHeader {
            version,
            model,
            cwd,
            branch,
            mode,
        } => {
            lines.extend(session_header_lines(
                version,
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
/// the brand colour, titled `>_ kloop` with the build stamp beside it, listing
/// the model, cwd, branch (omitted off a repo), and starting mode as dim-label
/// / default-value rows. The box width fits the content, capped so it never
/// spans an ultra-wide terminal.
fn session_header_lines(
    version: &str,
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

    // The build stamp rides on the title row (plan 161) rather than claiming a
    // label row of its own: it identifies the binary, not the session, and the
    // banner already costs five rows of a first screen.
    let title = ">_ kloop";
    let stamp = (!version.is_empty()).then(|| format!("  {version}"));
    let title_w = display_width(title) + stamp.as_deref().map_or(0, display_width);

    // Inner width = the widest of the title and the label+value rows, capped to
    // what the terminal leaves after a row's chrome, and to MAX_W.
    let content_w = fields
        .iter()
        .map(|(_, v)| LABEL_W + display_width(v))
        .chain(std::iter::once(title_w))
        .max()
        .unwrap_or(0);
    // A row costs four columns of chrome, not two: `│ ` … ` │`. Subtracting only
    // the borders let a box that fills the cap overflow its terminal by the two
    // padding columns and wrap (visible once the title row got longer, plan 161).
    let cap = width.saturating_sub(4).clamp(1, MAX_W);
    let inner = content_w.min(cap);

    let brand = Style::new().fg(BRAND);
    let mut lines = Vec::new();
    // Top border.
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner + 2)),
        brand,
    )));
    // Title row (bold brand), one space of padding inside the border. A stamp
    // cut in half would name a commit that is not the one this was built from,
    // so a box too narrow for the whole of it drops it instead of truncating.
    let mut title_spans = vec![Span::styled(
        truncate(title, inner),
        brand.add_modifier(Modifier::BOLD),
    )];
    if let Some(stamp) = stamp.filter(|_| inner >= title_w) {
        title_spans.push(Span::styled(stamp, DIM));
    }
    lines.push(boxed_row(title_spans, inner, brand));
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

/// How a cell may leave the live tail for native scrollback as a whole
/// (plan 215). Its settled lines may freeze whatever this says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Leave {
    /// It can still change in ways that move its lines.
    Never,
    /// Nothing about it can change any more — or what can, its in-place status
    /// rows, is written as a row of its own once it has left
    /// (`frozen_background_tasks` / `frozen_agent_messages`).
    Whole,
    /// A running tool or sub-agent row: it holds its place until the tail from
    /// it on is [`HARD_CAP_SCREENS`] screens tall, then goes so it cannot pin an
    /// unbounded tail. Nothing grows behind it while it runs — the model is
    /// waiting on it — so the cap is a backstop, not the common case.
    AtCap,
}

/// How many screens of tail a running tool or sub-agent row may hold back.
const HARD_CAP_SCREENS: usize = 4;

/// One cell's part in the freeze: how tall it is on screen, how many of its
/// leading lines can no longer change, and whether it may leave whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Freezable {
    pub height: usize,
    pub settled: usize,
    pub leave: Leave,
}

/// One cell as the viewport draws it. The draw and the freeze both take cells
/// from [`shown_cells`], so the lines written to scrollback are the lines that
/// were on screen (plan 74: what is drawn and what is committed have one source).
pub struct ShownCell {
    pub lines: Vec<Line<'static>>,
    pub settled: usize,
    pub leave: Leave,
}

impl ShownCell {
    fn done(lines: Vec<Line<'static>>) -> Self {
        let settled = lines.len();
        Self {
            lines,
            settled,
            leave: Leave::Whole,
        }
    }

    pub fn freezable(&self) -> Freezable {
        Freezable {
            height: self.lines.len(),
            settled: self.settled,
            leave: self.leave,
        }
    }
}

/// The uncommitted tail cell by cell, as the viewport shows it. The last cell
/// gets a live treatment while it streams: an Assistant renders through
/// [`crate::markdown::assistant_stream`], a Thinking block shows a running
/// clock (`thinking_secs`). Only the last cell streams (any other event seals
/// it), but an answer the model has not closed yet can still grow or be
/// replaced wherever it stands, so it only ever freezes what is settled.
pub fn shown_cells(app: &App, width: usize, thinking_secs: u64) -> Vec<ShownCell> {
    let last = app.cells.len().saturating_sub(1);
    app.cells
        .iter()
        .enumerate()
        .map(|(i, cell)| match cell {
            Cell::Assistant(text) if app.display_cell_live(i) => {
                let (streamed, settled) = crate::markdown::assistant_stream(text, width);
                let lines = if i == last && app.streaming_assistant() {
                    streamed
                } else {
                    cell_lines(cell, width)
                };
                ShownCell {
                    lines,
                    settled,
                    leave: Leave::Never,
                }
            }
            Cell::Thinking { .. } if app.display_cell_live(i) => {
                let lines = if i == last && app.streaming_thinking() {
                    vec![thinking_line(None, Some(thinking_secs), width)]
                } else {
                    cell_lines(cell, width)
                };
                ShownCell {
                    lines,
                    settled: 0,
                    leave: Leave::Never,
                }
            }
            Cell::Tool { status, .. } | Cell::Agent { status, .. }
                if *status == ToolStatus::Running =>
            {
                ShownCell {
                    lines: cell_lines(cell, width),
                    settled: 0,
                    leave: Leave::AtCap,
                }
            }
            // Replaced in place while live, so none of it is settled — but
            // once frozen its terminal update is appended as a row of its own,
            // so it may go whenever the tail needs the room.
            Cell::BackgroundTask(task) if task.status == BackgroundTaskStatus::Running => {
                ShownCell {
                    lines: cell_lines(cell, width),
                    settled: 0,
                    leave: Leave::Whole,
                }
            }
            Cell::AgentMessage(message) if message.status == AgentMessageStatus::Queued => {
                ShownCell {
                    lines: cell_lines(cell, width),
                    settled: 0,
                    leave: Leave::Whole,
                }
            }
            // The text is fixed on arrival; the answer only appends its mark
            // below it, and that has to land on this cell.
            Cell::Plan {
                status: PlanStatus::Pending,
                ..
            } => {
                let lines = cell_lines(cell, width);
                ShownCell {
                    settled: lines.len(),
                    lines,
                    leave: Leave::Never,
                }
            }
            _ => ShownCell::done(cell_lines(cell, width)),
        })
        .collect()
}

/// The uncommitted tail for the on-screen viewport, flattened from
/// [`shown_cells`]. The head cell resumes one line below whatever of it is
/// already in scrollback.
pub fn visible_transcript(app: &App, hud: &Hud, width: usize) -> Vec<Line<'static>> {
    let secs = hud.thinking.map(|d| d.as_secs()).unwrap_or(0);
    let mut skip = app.head_skip(width);
    let mut lines = Vec::new();
    for cell in shown_cells(app, width, secs) {
        lines.extend(cell.lines.into_iter().skip(skip));
        skip = 0;
    }
    lines
}

/// What to freeze so the live tail fits an `active_h`-row region: how many
/// leading cells leave whole, and how many leading lines of the new head are
/// then in scrollback. `frozen` is how many of the current head's lines already
/// are.
///
/// Every rendered line is either on screen or in scrollback (plan 215). The
/// draw bottom-anchors the tail and clips its top, so whatever overflows must
/// be frozen — and exactly that much, so the tail is never stranded behind a
/// screen of blank rows (plan 99). A line may freeze once it is settled; a cell
/// leaves whole once its [`Leave`] allows it. The one exception to the rule is
/// a cell whose unsettled part is itself taller than the region — in practice a
/// table still being written that is taller than the screen.
///
/// A status row that must not split (nothing of it settled) leaves whole even
/// when that frees more rows than needed — a few blank rows at the top of the
/// viewport beat a clipped one. Otherwise the last cell never leaves whole:
/// what overflows is always less than what is live, so it only gives up its
/// settled prefix.
pub fn freeze_target(cells: &[Freezable], frozen: usize, active_h: usize) -> (usize, usize) {
    let active_h = active_h.max(1);
    let total: usize = cells.iter().map(|cell| cell.height).sum();
    let mut live = total.saturating_sub(frozen);
    if live <= active_h {
        return (0, frozen);
    }
    let mut over = live - active_h;
    let mut skip = frozen;
    for (index, cell) in cells.iter().enumerate() {
        let rest = cell.height.saturating_sub(skip);
        let leaves = match cell.leave {
            Leave::Never => false,
            Leave::Whole => true,
            Leave::AtCap => live > HARD_CAP_SCREENS * active_h,
        };
        if leaves {
            if rest <= over {
                over -= rest;
                live -= rest;
                skip = 0;
                continue;
            }
            if cell.settled < cell.height {
                return (index + 1, 0);
            }
        }
        let take = cell.settled.saturating_sub(skip).min(over);
        return (index, skip + take);
    }
    (cells.len(), 0)
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

/// Whether [`held_command_line`] will render a row — the reservation in
/// [`live_chrome_layout`] must agree with the draw site, exactly as for the
/// activity line above.
pub fn has_held_command_line(app: &App) -> bool {
    !app.interaction_active() && app.held_command().is_some()
}

/// A known slash command sitting in the composer while a turn runs. It cannot
/// run mid-turn — commands own session state and the front-end already treats
/// the turn as busy — so Enter is a no-op that leaves the draft in place, and
/// this row says when it will go. Derived, not armed: the hint is true exactly
/// while the command is in the composer, so it needs no disarming and cannot go
/// stale if the user edits the line away.
fn held_command_line(app: &App, width: usize) -> Option<Line<'static>> {
    if !has_held_command_line(app) {
        return None;
    }
    let name = app
        .held_command()
        .expect("checked by has_held_command_line");
    Some(Line::from(truncate(
        &format!("/{name} runs when this turn ends — press Enter again then"),
        width,
    )))
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
    if app.live_todos().is_some() {
        hints.push_str(if app.show_todos {
            " · ctrl+t to hide todos"
        } else {
            " · ctrl+t to show todos"
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
    // Activity and todos are mutable live chrome, never transcript Cells. The
    // shared layout helper above also supplies commit_overflow's reserve.
    if chrome.activity_visible {
        if chrome.activity_spacer {
            lines.push(Line::default());
        }
        if let Some(activity) = activity_line(app, hud) {
            lines.push(activity);
        }
    }
    if let Some(held) = held_command_line(app, width.max(1)) {
        lines.push(held);
    }
    lines.extend(chrome.todo_lines);
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
    // Other phase). A list panel leaves it hidden: the composer is not
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
    // Only a file change is popup content. A plan was written into the
    // transcript when the prompt arrived (plan 194), so the panel here is one
    // question and two answers — and shrinks to about a third of its old
    // height, which is the conversation it stops covering up.
    if let Some(ConfirmPreview::FileChange(diff)) = &req.preview {
        if let Some(stats) = diff_stats_line(diff) {
            body.push(stats);
        }
        body.extend(diff_preview_lines(diff, width));
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

    fn todo(subject: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            subject: subject.into(),
            status,
        }
    }

    fn todo_snapshot(todos: Vec<TodoItem>) -> TodoSnapshot {
        TodoSnapshot { revision: 1, todos }
    }

    #[test]
    fn brand_accent_is_vivid_cyan_blue() {
        assert_eq!(BRAND, Color::Rgb(79, 179, 200));
        assert_ne!(BRAND, Color::Magenta);
        assert_ne!(BRAND, Color::Cyan);
    }

    #[test]
    fn todo_panel_sorts_styles_and_collapses_completed() {
        let snapshot = todo_snapshot(vec![
            todo("Done one", TodoStatus::Completed),
            todo("Active", TodoStatus::InProgress),
            todo("Ready", TodoStatus::Pending),
            todo("Next", TodoStatus::Pending),
            todo("Done five", TodoStatus::Completed),
            todo("Done six", TodoStatus::Completed),
            todo("Done seven", TodoStatus::Completed),
        ]);
        let lines = todo_panel_lines(&snapshot, 80, TODO_PANEL_MAX_ROWS);
        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "⎿ ◼ Active",
                "  ◻ Ready",
                "  ◻ Next",
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
    fn todo_panel_hard_cap_preserves_accurate_group_summaries_and_width() {
        let mut todos = (1..=12)
            .map(|id| todo(&format!("未完成任务{id}"), TodoStatus::Pending))
            .collect::<Vec<_>>();
        todos.extend((13..=17).map(|id| todo(&format!("已完成任务{id}"), TodoStatus::Completed)));
        let snapshot = todo_snapshot(todos);
        let lines = todo_panel_lines(&snapshot, 18, TODO_PANEL_MAX_ROWS);
        let texts = lines.iter().map(line_text).collect::<Vec<_>>();
        assert_eq!(lines.len(), TODO_PANEL_MAX_ROWS);
        assert!(texts.iter().any(|line| line == "  … +6 unfinished"));
        assert!(texts.iter().any(|line| line == "  … +5 completed"));
        assert!(
            lines
                .iter()
                .all(|line| display_width(&line_text(line)) <= 18),
            "{texts:?}"
        );

        let completed_only = todo_snapshot(
            (1..=5)
                .map(|id| todo(&format!("Done {id}"), TodoStatus::Completed))
                .collect(),
        );
        assert_eq!(
            todo_panel_lines(&completed_only, 40, 8)
                .iter()
                .map(line_text)
                .collect::<Vec<_>>(),
            vec!["⎿ ✔ Done 1", "  ✔ Done 2", "  ✔ Done 3", "  … +2 completed"]
        );
    }

    #[test]
    fn a_finished_graph_leaves_the_chrome_and_the_footer_hint_with_the_turn() {
        let mut app = App::new("s".into());
        app.todos = Some(todo_snapshot(vec![todo("Done", TodoStatus::Completed)]));
        let viewport = Rect::new(0, 0, 80, 24);
        assert_eq!(live_chrome_layout(&app, viewport).todo_lines.len(), 1);
        assert!(line_text(&footer_line(&app, 140)).contains("ctrl+t to hide todos"));

        app.apply(crate::events::AgentEvent::Core(
            kloop_core::event::Event::TurnEnded(kloop_core::agent::EndReason::Completed),
        ));
        assert!(
            live_chrome_layout(&app, viewport).todo_lines.is_empty(),
            "the retired panel gives its rows back to the transcript"
        );
        assert!(!line_text(&footer_line(&app, 140)).contains("ctrl+t"));
    }

    /// A known command parked in the composer mid-turn gets its own row under
    /// the activity line, reserved by the shared layout and gone once the line
    /// is edited away (or the turn ends).
    #[test]
    fn a_held_command_gets_a_hint_row_and_reserves_it() {
        let hud = Hud::default();
        let viewport = Rect::new(0, 0, 80, 24);
        let mut app = App::new("s".into()).with_commands(vec![crate::menu::CommandInfo {
            name: "compact".into(),
            description: "compact the session".into(),
        }]);
        app.cells.push(Cell::Assistant("transcript".into()));
        app.running = true;
        assert!(!has_held_command_line(&app), "nothing parked yet");

        app.composer.insert_char('/');
        for c in "compact".chars() {
            app.composer.insert_char(c);
        }
        assert!(has_held_command_line(&app));
        let chrome = live_chrome_layout(&app, viewport);
        assert!(chrome.held_command_visible);
        // The activity line (1) + its spacer (1) + the hint (1).
        assert_eq!(chrome.reserved_rows(), 3);

        // Editing the `/` away drops the row and gives the space back.
        app.composer.home();
        app.composer.delete();
        assert!(!has_held_command_line(&app));
        assert_eq!(live_chrome_layout(&app, viewport).reserved_rows(), 2);

        // An unknown name is not a command, so it is steering text, no row.
        app.composer.clear();
        for c in "/nope this is a path".chars() {
            app.composer.insert_char(c);
        }
        assert_eq!(app.composer.text(), "/nope this is a path");
        assert!(!has_held_command_line(&app));
        assert_eq!(live_chrome_layout(&app, viewport).reserved_rows(), 2);

        // An overlay owns the keyboard, and the hint would be a lie while it is up.
        app.composer.clear();
        for c in "/compact".chars() {
            app.composer.insert_char(c);
        }
        assert_eq!(app.composer.text(), "/compact");
        assert!(has_held_command_line(&app));
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
        assert!(app.interaction_active());
        assert!(!has_held_command_line(&app));
        assert!(!live_chrome_layout(&app, viewport).held_command_visible);
        let _ = hud;
    }

    #[test]
    fn live_chrome_hides_todos_for_overlays_and_tiny_terminals() {
        let mut app = App::new("s".into());
        app.cells.push(Cell::Assistant("transcript".into()));
        app.todos = Some(todo_snapshot(vec![todo("Visible", TodoStatus::Pending)]));
        let normal = live_chrome_layout(&app, Rect::new(0, 0, 80, 24));
        assert_eq!(normal.todo_lines.len(), 1);
        assert_eq!(normal.reserved_rows(), 1);

        app.fork_picker = Some(crate::app::ForkPicker {
            points: Vec::new(),
            cursor: 0,
        });
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .todo_lines
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
                .todo_lines
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
                .todo_lines
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
                .todo_lines
                .is_empty()
        );
        app.popup = None;

        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 5))
                .todo_lines
                .is_empty()
        );
        app.show_todos = false;
        assert!(
            live_chrome_layout(&app, Rect::new(0, 0, 80, 24))
                .todo_lines
                .is_empty()
        );
    }

    #[test]
    fn draw_keeps_activity_then_todos_immediately_above_composer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let mut app = App::new("s".into());
        app.cells.push(Cell::Assistant("history".into()));
        app.running = true;
        app.todos = Some(todo_snapshot(vec![
            todo("Done one", TodoStatus::Completed),
            todo("Active", TodoStatus::InProgress),
            todo(
                "需要处理一个非常非常非常非常长的中文任务标题并且还要再长一点",
                TodoStatus::Pending,
            ),
            todo("Done four", TodoStatus::Completed),
            todo("Done five", TodoStatus::Completed),
            todo("Done six", TodoStatus::Completed),
            todo("Done seven", TodoStatus::Completed),
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
        let first_todo = rows
            .iter()
            .position(|row| row.contains("⎿ ◼ Active"))
            .expect("first todo row");
        let cjk = rows
            .iter()
            .position(|row| row.contains('需'))
            .unwrap_or_else(|| panic!("CJK todo row: {rows:#?}"));
        assert!(rows[cjk].contains('…'), "CJK subject truncates: {rows:#?}");
        let completed_summary = rows
            .iter()
            .position(|row| row.contains("… +2 completed"))
            .expect("completed folding row");
        let rule = rows
            .iter()
            .enumerate()
            .skip(cjk + 1)
            .find(|(_, row)| row.trim_matches('─').is_empty() && row.contains('─'))
            .map(|(index, _)| index)
            .expect("composer top rule");
        assert!(
            activity < first_todo
                && first_todo < cjk
                && cjk < completed_summary
                && completed_summary < rule
        );
        assert_eq!(
            app.cells,
            vec![Cell::Assistant("history".into())],
            "Todo projection is not a transcript Cell"
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
        app.todos = Some(todo_snapshot(vec![todo("Toggle me", TodoStatus::Pending)]));
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();

        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();
        let visible = screen(&terminal);
        assert!(visible.contains("⎿ ◻ Toggle me"), "{visible}");
        assert!(visible.contains("ctrl+t to hide todos"), "{visible}");

        app.on_key(80, KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        terminal
            .draw(|frame| draw(frame, &mut app, &Hud::default()))
            .unwrap();
        let hidden = screen(&terminal);
        assert!(!hidden.contains("Toggle me"), "{hidden}");
        assert!(hidden.contains("ctrl+t to show todos"), "{hidden}");

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
            preview: Some(ConfirmPreview::FileChange("+1  hello\n+2  world".into())),
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

    /// A plan takes the other road: it goes into the transcript when the prompt
    /// arrives and the panel keeps only the question. Before plan 194 it was
    /// popup body — eight rows of it, coloured as a diff, with a `+0 -N` summary
    /// counting its bullets as deletions.
    #[test]
    fn a_plan_goes_to_the_transcript_and_leaves_the_panel_a_question() {
        use crate::events::AgentEvent;

        let plan = "## Rewrite the parser\n\n- read the parser\n- rewrite it\n- delete the old one";
        let mut app = App::new("s".into());
        let (reply, _rx) = tokio::sync::oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "Exit plan mode and start on this plan?".into(),
                title: Some("Exit plan mode".into()),
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                preview: Some(ConfirmPreview::Plan(plan.into())),
                ..Default::default()
            },
            reply,
        });

        let panel = active_panel(&app, 60).expect("the prompt owns the keyboard");
        assert!(panel.body.is_empty(), "the plan is not popup content");
        let rows = choice::panel_lines(&panel, 60, PANEL_MAX_ROWS, 0)
            .lines
            .iter()
            .map(line_text)
            .collect::<Vec<_>>();
        assert_eq!(
            rows,
            vec![
                "▌ Exit plan mode",
                "",
                "Exit plan mode and start on this plan?",
                "",
                "Do you want to proceed?",
                "",
                "> 1. Yes",
                "  2. No, and tell kloop what to do differently",
                "",
                "Enter select · ↑↓ move · 1-9 pick · Esc deny",
            ],
            "the panel is one question and two answers — ten rows, not twenty"
        );

        // The whole plan is in the transcript, as markdown: every source line
        // is there, and nothing is coloured like a deleted diff line.
        let cell = app.cells.last().expect("the plan is a transcript cell");
        let lines = cell_lines(cell, 60);
        let rendered = lines.iter().map(line_text).collect::<Vec<_>>().join("\n");
        for source in [
            "Rewrite the parser",
            "read the parser",
            "rewrite it",
            "delete the old one",
        ] {
            assert!(
                rendered.contains(source),
                "{source} missing from:\n{rendered}"
            );
        }
        assert!(
            !rendered.contains("-0") && !rendered.contains("+0"),
            "no diff summary:\n{rendered}"
        );
        assert!(
            !lines
                .iter()
                .flat_map(|line| &line.spans)
                .any(|span| span.style.fg == Some(Color::Red)),
            "a bullet is not a deletion"
        );
    }

    /// Each outcome gets a mark, not only the refusal: the cell is on screen
    /// before there is an answer, so a plain plan already means "not answered
    /// yet" and cannot also mean "approved".
    #[test]
    fn a_plan_cell_marks_how_it_ended() {
        let last = |status| {
            cell_lines(
                &Cell::Plan {
                    text: "do the thing".into(),
                    status,
                },
                40,
            )
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .pop()
            .unwrap()
        };
        assert_eq!(last(PlanStatus::Pending), "do the thing");
        assert_eq!(last(PlanStatus::Approved), "  ✓ approved");
        assert_eq!(
            last(PlanStatus::Declined),
            "  ✗ not approved — still planning"
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
                preview: Some(ConfirmPreview::FileChange(preview)),
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

    /// The same frame, for a plan: the one that started plan 194 was 47 display
    /// rows and got eight of them, in a panel that left the conversation one
    /// line. Here the whole plan is on screen and the panel is ten rows.
    #[test]
    fn draw_shows_a_tall_plan_in_the_transcript_under_a_ten_row_panel() {
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

        let mut plan = String::from("## Rewrite the parser\n\n");
        for i in 1..=40 {
            plan.push_str(&format!("- step {i}\n"));
        }
        let mut app = App::new("s".into());
        app.cells.push(Cell::Assistant("earlier turn".into()));
        let (reply, _rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "Exit plan mode and start on this plan?".into(),
                title: Some("Exit plan mode".into()),
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                preview: Some(ConfirmPreview::Plan(plan)),
                ..Default::default()
            },
            reply,
        });

        let mut term = Terminal::new(TestBackend::new(64, 60)).unwrap();
        term.draw(|f| draw(f, &mut app, &Hud::default())).unwrap();
        let rendered = rows(&term);
        let screen = rendered.join("\n");

        // Every step of the plan, first to last, plus its heading.
        assert!(screen.contains("Rewrite the parser"), "{screen}");
        for i in 1..=40 {
            assert!(screen.contains(&format!("step {i}")), "step {i}:\n{screen}");
        }
        assert!(screen.contains("▌ Plan"), "titled as a plan:\n{screen}");
        // The panel is the question and the answers, and nothing scrolls in it.
        let panel_top = rendered
            .iter()
            .position(|row| row.contains("▌ Exit plan mode"))
            .expect("the panel is on screen");
        let hint = rendered
            .iter()
            .position(|row| row.contains("Enter select"))
            .expect("the hint is on screen");
        assert_eq!(hint + 1 - panel_top, 10, "a ten-row panel:\n{screen}");
        assert!(!screen.contains("PgUp/PgDn scroll"), "{screen}");
        assert!(!screen.contains("-0"), "no diff summary:\n{screen}");
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
    /// `>_ kloop` with the build stamp beside the title (plan 161), one
    /// dim-label row per field, branch present when on a repo.
    #[test]
    fn session_header_renders_a_branded_box_with_fields() {
        let lines = session_header_lines(
            "v0.1.0 (2319ea3)",
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
        assert!(texts[1].contains("v0.1.0 (2319ea3)"), "{texts:?}");
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
        let texts: Vec<String> = session_header_lines("v0.1.0", "m", "/tmp/x", None, "plan", 80)
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

    /// A terminal too narrow for the whole build stamp drops it rather than
    /// cutting it (plan 161): half a sha names a different commit. The title
    /// itself stays, and the box does not grow past the terminal.
    #[test]
    fn session_header_drops_the_build_stamp_before_truncating_it() {
        let texts: Vec<String> =
            session_header_lines("v0.1.0 (2319ea3)", "m", "/x", None, "plan", 20)
                .iter()
                .map(line_text)
                .collect();
        assert!(texts[1].contains(">_ kloop"), "{texts:?}");
        assert!(!texts[1].contains("v0.1.0"), "{texts:?}");
        assert!(!texts[1].contains("2319"), "{texts:?}");
        assert!(texts.iter().all(|t| display_width(t) <= 20), "{texts:?}");
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
                version: "v0.1.0 (2319ea3)".into(),
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

        app.todos = Some(todo_snapshot(vec![todo("Visible", TodoStatus::Pending)]));
        assert!(line_text(&footer_line(&app, 140)).contains("ctrl+t to hide todos"));
        app.show_todos = false;
        assert!(line_text(&footer_line(&app, 140)).contains("ctrl+t to show todos"));
        app.show_todos = true;

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
    fn freezable(height: usize, settled: usize, leave: Leave) -> Freezable {
        Freezable {
            height,
            settled,
            leave,
        }
    }

    /// A cell that is done: every line settled, free to leave.
    fn done(height: usize) -> Freezable {
        freezable(height, height, Leave::Whole)
    }

    /// Each case: the tail, how much of its head is already frozen, the live
    /// region's height, and (cells leaving whole, new head's frozen lines).
    #[test]
    fn freeze_target_freezes_exactly_what_overflows() {
        let running_row = freezable(1, 0, Leave::AtCap);
        // A background task or queued message: replaced in place, so nothing
        // settled, but free to go.
        let status_row = freezable(3, 0, Leave::Whole);
        let streaming = |height, settled| freezable(height, settled, Leave::Never);
        let waiting_plan = |height| freezable(height, height, Leave::Never);
        let live_thinking = freezable(1, 0, Leave::Never);
        let one_line = |n| vec![done(1); n];
        type Case = (&'static str, Vec<Freezable>, usize, usize, (usize, usize));
        let cases: Vec<Case> = vec![
            ("fits", vec![done(2), done(3)], 0, 10, (0, 0)),
            (
                "fits below a frozen seam",
                vec![done(20), done(1)],
                16,
                5,
                (0, 16),
            ),
            ("front cells leave whole", one_line(5), 0, 3, (2, 0)),
            (
                "a tiny region keeps only the last",
                one_line(5),
                0,
                1,
                (4, 0),
            ),
            (
                "a tall finished cell splits by line",
                vec![done(20), done(1)],
                0,
                5,
                (0, 16),
            ),
            (
                "only the new overflow is added to the seam",
                vec![done(20), done(1), done(2)],
                16,
                5,
                (0, 18),
            ),
            (
                "a fully frozen head leaves and the next one splits",
                vec![done(20), done(1), done(10)],
                20,
                5,
                (2, 5),
            ),
            (
                "plan 99: a tall message is split, not stranding the tail",
                vec![done(1), done(1), done(8), done(1)],
                0,
                5,
                (2, 4),
            ),
            (
                "a streaming answer freezes up to its settled lines",
                vec![done(2), streaming(30, 12)],
                0,
                10,
                (1, 12),
            ),
            (
                "a streaming answer freezes only what overflows",
                vec![streaming(30, 25)],
                0,
                10,
                (0, 20),
            ),
            (
                "an open answer holds back the cells behind it",
                vec![streaming(10, 4), done(5)],
                0,
                3,
                (0, 4),
            ),
            (
                "settled below the seam freezes nothing more",
                vec![streaming(10, 2), done(10)],
                5,
                5,
                (0, 5),
            ),
            (
                "live thinking holds its place",
                vec![live_thinking, done(10)],
                0,
                3,
                (0, 0),
            ),
            (
                "a running tool row pins the tail under the cap",
                [vec![running_row], one_line(3)].concat(),
                0,
                2,
                (0, 0),
            ),
            (
                "past four screens the running row goes",
                [vec![running_row], one_line(10)].concat(),
                0,
                2,
                (9, 0),
            ),
            (
                "the cap counts the tail from the running row, not what left above it",
                [vec![done(50), running_row], one_line(3)].concat(),
                0,
                2,
                (1, 0),
            ),
            (
                "a background row goes when the tail needs the room",
                [vec![status_row], one_line(3)].concat(),
                0,
                2,
                (2, 0),
            ),
            (
                "a row that cannot split goes whole, freeing more than needed",
                [vec![status_row], one_line(2)].concat(),
                0,
                4,
                (1, 0),
            ),
            (
                "a waiting plan as the last cell gives up its overflow",
                vec![done(2), waiting_plan(25)],
                0,
                10,
                (1, 15),
            ),
            (
                "the last cell never leaves whole",
                vec![done(30)],
                0,
                1,
                (0, 29),
            ),
            (
                "unless it is a row that cannot split, taller than the region",
                vec![status_row],
                0,
                2,
                (1, 0),
            ),
        ];
        let got: Vec<(&str, (usize, usize))> = cases
            .iter()
            .map(|(name, cells, frozen, active_h, _)| {
                (*name, freeze_target(cells, *frozen, *active_h))
            })
            .collect();
        let want: Vec<(&str, (usize, usize))> = cases
            .iter()
            .map(|(name, _, _, _, want)| (*name, *want))
            .collect();
        assert_eq!(got, want);
    }

    /// What each kind of cell claims (plan 215 §4.3): its on-screen height,
    /// how much of it is settled, and whether it may leave whole.
    #[test]
    fn shown_cells_say_what_each_cell_may_freeze() {
        use kloop_core::event::Delta;
        use kloop_core::event::Event;

        let width = 40;
        let tool = |status| Cell::Tool {
            name: "Bash".into(),
            input: r#"{"command":"ls"}"#.into(),
            status,
            output: None,
        };
        let task = |status| {
            Cell::BackgroundTask(kloop_core::event::BackgroundTask {
                id: "agent-1".into(),
                run_id: None,
                kind: BackgroundTaskKind::Agent,
                description: "long audit".into(),
                status,
                output_path: None,
                detail: None,
            })
        };
        let plan = |status| Cell::Plan {
            text: "- one\n- two".into(),
            status,
        };
        let mut app = App::new("s".into());
        app.cells = vec![
            Cell::User("q".into()),
            tool(ToolStatus::Running),
            tool(ToolStatus::Ok),
            Cell::Agent {
                agent: "explorer".into(),
                task: "look".into(),
                status: ToolStatus::Running,
                tools: 0,
                last_tool: String::new(),
            },
            task(BackgroundTaskStatus::Running),
            task(BackgroundTaskStatus::Completed),
            Cell::AgentMessage(kloop_core::event::AgentMessageUpdate {
                id: "message-12".parse().unwrap(),
                from: "agent-4".parse().unwrap(),
                to: "main".parse().unwrap(),
                summary: "review".into(),
                status: AgentMessageStatus::Queued,
            }),
            plan(PlanStatus::Pending),
            plan(PlanStatus::Approved),
            Cell::Thinking {
                text: "…".into(),
                seconds: Some(3),
            },
        ];
        // Thinking still open but no longer last, then an answer streaming.
        app.apply(crate::events::AgentEvent::Core(Event::ItemDelta {
            id: "r".into(),
            delta: Delta::Reasoning("hmm".into()),
        }));
        app.apply(crate::events::AgentEvent::Core(Event::ItemDelta {
            id: "m".into(),
            delta: Delta::Text("# Title\n\nsettled para\n\nstill form".into()),
        }));
        let got: Vec<(usize, usize, Leave)> = shown_cells(&app, width, 0)
            .iter()
            .map(|cell| (cell.lines.len(), cell.settled, cell.leave))
            .collect();
        let height = |cell: &Cell| cell_lines(cell, width).len();
        let all = |i: usize| (height(&app.cells[i]), height(&app.cells[i]));
        let none = |i: usize| (height(&app.cells[i]), 0);
        let with = |(h, s): (usize, usize), leave| (h, s, leave);
        assert_eq!(
            got,
            vec![
                with(all(0), Leave::Whole),
                with(none(1), Leave::AtCap),
                with(all(2), Leave::Whole),
                with(none(3), Leave::AtCap),
                with(none(4), Leave::Whole),
                with(all(5), Leave::Whole),
                with(none(6), Leave::Whole),
                with(all(7), Leave::Never),
                with(all(8), Leave::Whole),
                with(all(9), Leave::Whole),
                (1, 0, Leave::Never),
                // Title, gap, paragraph settled; gap and raw tail not.
                (5, 3, Leave::Never),
            ]
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
