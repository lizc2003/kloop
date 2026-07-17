//! TUI state and its transitions. Everything here is pure with respect to the
//! terminal: agent events and key events come in, transcript cells and
//! [`Command`]s come out. The event loop in lib.rs owns the side effects.

use std::collections::HashMap;
use std::collections::VecDeque;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use kloop_core::agent::EndReason;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::permissions::Mode;
use kloop_core::rollout::ForkPoint;
use kloop_core::tools::TodoItem;
use kloop_core::tools::TodoStatus;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::Message;
use kloop_protocol::Role;
use tokio::sync::oneshot;

use crate::events::AgentEvent;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

/// One transcript entry. Tool calls are collapsed to a single status row —
/// their full output lives in history/offload, not on screen.
///
/// Cells live in [`App::cells`] only while uncommitted (still mutable, or the
/// recent tail shown in the inline viewport). Once a cell is final and scrolls
/// past the top of the viewport, the event loop writes it into the terminal's
/// native scrollback via `insert_before` (plan 38 slice 0) and drops it here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cell {
    User(String),
    Assistant(String),
    /// Model reasoning; accumulates like Assistant but renders collapsed to a
    /// one-line dim preview (full text lives in history, not on screen).
    Thinking(String),
    Tool {
        name: String,
        summary: String,
        status: ToolStatus,
    },
    /// One live row per sub-agent: its tool calls fold into a counter plus a
    /// preview of the latest call instead of separate rows, so parallel
    /// sub-agents never interleave in the transcript.
    Agent {
        agent: String,
        task: String,
        status: ToolStatus,
        tools: usize,
        last_tool: String,
    },
    /// The model's current task list (todo_write). Updated in place within a
    /// turn; a new user turn starts a fresh block.
    Todo(Vec<TodoItem>),
    Note(String),
    /// Output of a slash command — a wrapped, dim multi-line block (unlike a
    /// Note, which collapses to one truncated line).
    System(String),
}

/// A permission prompt currently waiting for a keypress. Prompts queue:
/// concurrent tool batches can ask more than once before the first answer.
#[derive(Debug)]
pub struct PendingConfirm {
    pub req: ConfirmRequest,
    reply: oneshot::Sender<Decision>,
}

/// The open rewind picker (plan 18): the fork targets the worker read off the
/// session file, and which one the cursor sits on. Only present while the user
/// is choosing; selecting or cancelling clears it.
pub struct ForkPicker {
    pub points: Vec<ForkPoint>,
    pub cursor: usize,
}

/// What the event loop must do after a key was handled; the side-effectful
/// counterpart to the pure state change already applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    None,
    /// Send this user text to the agent task (a turn is now running).
    Submit(String),
    /// Ask the worker for this session's rewind targets (Ctrl+R, idle only).
    /// The worker replies with a `ForkPoints` event that opens the picker.
    RequestForkPoints,
    /// Rewind History onto the fork cut at this seq (the picker's selection).
    /// The worker forks, swaps History, and replies with a `Forked` event.
    Fork(u64),
    /// Run this slash-command line (`/help`, `/compact`, …) on the worker,
    /// which owns History. Only produced when idle; the worker replies with a
    /// System block and a TurnEnded that clears the busy state.
    Slash(String),
    /// Enqueue this text into the running turn's steering queue (plan 22): it
    /// is delivered as a user message at the next round boundary, without
    /// interrupting the turn. Only produced while a turn is running.
    Steer(String),
    /// Cycle the permission mode (shift+Tab): the loop applies it to the shared
    /// gate. App state already updated its own `mode` mirror.
    SetMode(Mode),
    /// Cancel the in-flight turn's CancellationToken.
    Interrupt,
    Quit,
}

pub struct App {
    pub session_id: String,
    /// The uncommitted transcript tail: cells still mutating plus the recent
    /// finalized ones the viewport shows. Older finalized cells have left for
    /// native scrollback (see [`Cell`], [`App::drain_committed`]).
    pub cells: Vec<Cell>,
    pub input: String,
    /// Cursor position in `input`, in chars.
    pub cursor: usize,
    pub running: bool,
    pub confirms: VecDeque<PendingConfirm>,
    /// Scroll offset (in display lines) into the active confirm popup's body,
    /// so a diff taller than the popup can be read in full. Reset to 0 when the
    /// front prompt changes; clamped to a valid range at render time.
    pub confirm_scroll: usize,
    /// Latest agent activity (note / tool / sub-agent / todo). No longer churned
    /// into the status line — activity shows in the transcript. Kept as recent
    /// state for the animated status HUD (plan 38 slice 5).
    pub last_note: Option<String>,
    /// Whether the last Assistant cell still accepts text deltas. A tool row,
    /// note, or thinking cell in between closes it so ordering is preserved.
    assistant_open: bool,
    /// Same for the last Thinking cell and thinking deltas.
    thinking_open: bool,
    /// tool_use id -> cells index, to resolve ToolEnd.
    tool_cells: HashMap<String, usize>,
    /// agent label -> cells index of its Agent row.
    agent_cells: HashMap<String, usize>,
    /// Index of the current turn's Todo cell, updated in place as the model
    /// rewrites its list; reset each new user turn so a fresh block starts.
    todo_cell: Option<usize>,
    /// The rewind picker while it is open (Ctrl+R when idle); None otherwise.
    /// While open it captures the keyboard, like a confirm prompt.
    pub fork_picker: Option<ForkPicker>,
    /// The current permission mode, shown in the status bar. A display mirror of
    /// the shared gate: shift+Tab updates it here and via `Command::SetMode`; an
    /// `exit_plan_mode` approval refreshes it via `AgentEvent::ModeChanged`. The
    /// loop seeds it from the real gate before the first draw.
    pub mode: Mode,
    /// Ctrl+C is a two-tap quit (CC parity): the first press arms this and shows
    /// a hint; the next Ctrl+C quits, any other key disarms it.
    pub ctrl_c_exit_armed: bool,
}

impl App {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            cells: Vec::new(),
            input: String::new(),
            cursor: 0,
            running: false,
            confirms: VecDeque::new(),
            confirm_scroll: 0,
            last_note: None,
            assistant_open: false,
            thinking_open: false,
            tool_cells: HashMap::new(),
            agent_cells: HashMap::new(),
            todo_cell: None,
            fork_picker: None,
            mode: Mode::default(),
            ctrl_c_exit_armed: false,
        }
    }

    pub fn apply(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::TextDelta(t) => {
                self.thinking_open = false;
                if self.assistant_open {
                    if let Some(Cell::Assistant(text)) = self.cells.last_mut() {
                        text.push_str(&t);
                        return;
                    }
                }
                self.cells.push(Cell::Assistant(t));
                self.assistant_open = true;
            }
            AgentEvent::ThinkingDelta(t) => {
                self.assistant_open = false;
                if self.thinking_open {
                    if let Some(Cell::Thinking(text)) = self.cells.last_mut() {
                        text.push_str(&t);
                        return;
                    }
                }
                self.cells.push(Cell::Thinking(t));
                self.thinking_open = true;
            }
            AgentEvent::Note(n) => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = Some(n.clone());
                self.cells.push(Cell::Note(n));
            }
            AgentEvent::ToolStart {
                agent,
                id,
                name,
                summary,
            } => {
                if !agent.is_empty() {
                    // A sub-agent's call folds into its Agent row: bump the
                    // counter, refresh the preview. No per-call cell, so
                    // parallel agents cannot interleave.
                    self.last_note = Some(format!("{agent} · {name} {summary}"));
                    if let Some(Cell::Agent {
                        tools, last_tool, ..
                    }) = self.agent_cell(&agent)
                    {
                        *tools += 1;
                        *last_tool = format!("{name} {summary}");
                    }
                    return;
                }
                self.assistant_open = false;
                self.thinking_open = false;
                // todo_write renders as a Todo block via TodoUpdate, not a
                // generic tool row (cc renders the checklist in its place).
                if name == "todo_write" {
                    return;
                }
                self.last_note = Some(format!("{name} {summary}"));
                self.tool_cells.insert(id, self.cells.len());
                self.cells.push(Cell::Tool {
                    name,
                    summary,
                    status: ToolStatus::Running,
                });
            }
            AgentEvent::ToolEnd { agent, id, ok } => {
                // Sub-agent calls have no cell of their own; their agent's
                // row is resolved by AgentEnd.
                if !agent.is_empty() {
                    return;
                }
                if let Some(&i) = self.tool_cells.get(&id) {
                    if let Some(Cell::Tool { status, .. }) = self.cells.get_mut(i) {
                        *status = if ok {
                            ToolStatus::Ok
                        } else {
                            ToolStatus::Failed
                        };
                    }
                }
            }
            AgentEvent::AgentStart { agent, task } => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = Some(format!("{agent} started: {task}"));
                self.agent_cells.insert(agent.clone(), self.cells.len());
                self.cells.push(Cell::Agent {
                    agent,
                    task,
                    status: ToolStatus::Running,
                    tools: 0,
                    last_tool: String::new(),
                });
            }
            AgentEvent::AgentEnd { agent, ok } => {
                if let Some(Cell::Agent { status, .. }) = self.agent_cell(&agent) {
                    *status = if ok {
                        ToolStatus::Ok
                    } else {
                        ToolStatus::Failed
                    };
                }
            }
            AgentEvent::TodoUpdate { todos } => {
                self.assistant_open = false;
                self.thinking_open = false;
                let done = todos
                    .iter()
                    .filter(|t| t.status == TodoStatus::Completed)
                    .count();
                self.last_note = Some(format!("todos {done}/{}", todos.len()));
                // Update this turn's block in place; start one if there is none.
                match self.todo_cell {
                    Some(i) if matches!(self.cells.get(i), Some(Cell::Todo(_))) => {
                        self.cells[i] = Cell::Todo(todos);
                    }
                    _ => {
                        self.todo_cell = Some(self.cells.len());
                        self.cells.push(Cell::Todo(todos));
                    }
                }
            }
            AgentEvent::System(text) => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.cells.push(Cell::System(text));
            }
            AgentEvent::ClearTranscript => {
                // /clear emptied History on the worker; drop the uncommitted
                // view state. Cells already in native scrollback stay visible
                // (inline can't erase scrollback) but are out of the model's
                // context — the System note that follows says so.
                self.cells.clear();
                self.tool_cells.clear();
                self.agent_cells.clear();
                self.todo_cell = None;
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = None;
            }
            // The UI loop intercepts Quit before apply; this arm only keeps the
            // match exhaustive.
            AgentEvent::Quit => {}
            AgentEvent::ForkPoints(points) => {
                if points.is_empty() {
                    self.cells
                        .push(Cell::System("nothing to rewind to yet".into()));
                } else {
                    // Points are oldest-first; the newest turn (bottom of the
                    // list) is the usual rewind target, so start the cursor there.
                    let cursor = points.len() - 1;
                    self.fork_picker = Some(ForkPicker { points, cursor });
                }
            }
            AgentEvent::Forked {
                session_id,
                messages,
            } => {
                // History was swapped to the fork; rebuild the view to match its
                // truncated content, exactly like resuming into a session.
                self.session_id = session_id;
                self.cells = cells_from_history(&messages);
                // cells_from_history tags the tail "resumed session"; relabel it
                // so the transcript says a rewind happened, not a resume.
                if matches!(self.cells.last(), Some(Cell::Note(_))) {
                    self.cells.pop();
                    self.cells.push(Cell::Note(format!(
                        "rewound — {} message(s) kept",
                        messages.len()
                    )));
                }
                self.tool_cells.clear();
                self.agent_cells.clear();
                self.todo_cell = None;
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = None;
                self.fork_picker = None;
            }
            AgentEvent::Confirm { req, reply } => {
                self.confirms.push_back(PendingConfirm { req, reply });
            }
            AgentEvent::ModeChanged(mode) => {
                // exit_plan_mode flipped the gate on the agent side; keep the
                // status-bar badge in step.
                self.mode = mode;
            }
            AgentEvent::TurnEnded(reason) => {
                self.running = false;
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = None;
                // Any prompt still queued belongs to the turn that just died;
                // dropping the senders resolves them as Deny.
                self.confirms.clear();
                self.confirm_scroll = 0;
                // An interrupted turn drops task futures mid-await, so a
                // sub-agent's AgentEnd may never arrive: no row may outlive
                // its turn still spinning.
                for cell in &mut self.cells {
                    if let Cell::Agent { status, .. } = cell {
                        if *status == ToolStatus::Running {
                            *status = ToolStatus::Failed;
                        }
                    }
                }
                match reason {
                    EndReason::Completed => {}
                    EndReason::MaxRounds => {
                        self.cells.push(Cell::Note("stopped: max rounds".into()))
                    }
                    EndReason::Aborted => self.cells.push(Cell::Note("interrupted".into())),
                    EndReason::Error(e) => self.cells.push(Cell::Note(format!("error: {e}"))),
                }
            }
        }
    }

    fn agent_cell(&mut self, agent: &str) -> Option<&mut Cell> {
        let &i = self.agent_cells.get(agent)?;
        self.cells.get_mut(i)
    }

    /// Drop the first `n` cells: the event loop has just written them into the
    /// terminal's native scrollback (`insert_before`). Every index-into-`cells`
    /// map shifts down by `n`; entries that pointed into the committed prefix are
    /// dropped (their cells can no longer be mutated — they are frozen in
    /// scrollback, so a late ToolEnd/AgentEnd for them becomes a harmless no-op).
    pub fn drain_committed(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let n = n.min(self.cells.len());
        self.cells.drain(0..n);
        self.tool_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.agent_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.todo_cell = self.todo_cell.and_then(|i| i.checked_sub(n));
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Command {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+C is a two-tap quit on every surface — main input, confirm popup,
        // rewind picker: the first press arms a hint, the second quits, any other
        // key disarms. Handle it before routing so all three agree (CC parity —
        // Esc does interrupt/dismiss, Ctrl+C exits). Ctrl+D stays immediate.
        let was_armed = std::mem::take(&mut self.ctrl_c_exit_armed);
        if ctrl && key.code == KeyCode::Char('c') {
            if was_armed {
                return Command::Quit;
            }
            self.ctrl_c_exit_armed = true;
            return Command::None;
        }
        // A pending permission prompt captures the keyboard.
        if !self.confirms.is_empty() {
            return self.on_confirm_key(key);
        }
        // So does an open rewind picker.
        if self.fork_picker.is_some() {
            return self.on_fork_key(key);
        }
        match (key.code, ctrl) {
            // Esc interrupts a running turn (CC parity, the advertised key);
            // idle it clears the input line. A confirm popup / rewind picker
            // capture Esc before this (they return early at the top of on_key).
            (KeyCode::Esc, _) => {
                if self.running {
                    return Command::Interrupt;
                }
                self.input.clear();
                self.cursor = 0;
            }
            (KeyCode::Char('r'), true) => {
                // Rewind (plan 18) is idle-only: a running turn owns History, so
                // Ctrl+R is ignored mid-turn. The worker answers with ForkPoints.
                if !self.running {
                    return Command::RequestForkPoints;
                }
            }
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() {
                    return Command::None;
                }
                self.input.clear();
                self.cursor = 0;
                // A slash command runs only when idle; it is not a message, so
                // no User cell and no new todo block. While a turn runs, a
                // '/'-line is just steering text (Ctrl+C stays the hard stop).
                if !self.running && kloop_core::commands::is_command(&text) {
                    self.running = true;
                    return Command::Slash(text);
                }
                self.cells.push(Cell::User(text.clone()));
                if self.running {
                    // Steering: the running turn absorbs this at its next round
                    // boundary. It does not start a new turn, reset the todo
                    // block, or interrupt tools (Ctrl+C stays the hard stop).
                    return Command::Steer(text);
                }
                // A new turn starts a fresh todo block instead of mutating the
                // previous turn's (which stays in the transcript as history).
                self.todo_cell = None;
                self.running = true;
                return Command::Submit(text);
            }
            // shift+Tab cycles the permission mode (manual → accept-edits →
            // plan → manual; bypass is opt-in via the CLI flag only). Allowed
            // any time — the gate reads the mode live per tool call.
            (KeyCode::BackTab, _) => {
                self.mode = self.mode.cycled();
                return Command::SetMode(self.mode);
            }
            (KeyCode::Char(c), false) => {
                let at = byte_index(&self.input, self.cursor);
                self.input.insert(at, c);
                self.cursor += 1;
            }
            (KeyCode::Backspace, _) => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    let at = byte_index(&self.input, self.cursor);
                    self.input.remove(at);
                }
            }
            (KeyCode::Left, _) => self.cursor = self.cursor.saturating_sub(1),
            (KeyCode::Right, _) => {
                self.cursor = (self.cursor + 1).min(self.input.chars().count());
            }
            (KeyCode::Home, _) => self.cursor = 0,
            (KeyCode::End, _) => self.cursor = self.input.chars().count(),
            // Scrolling the transcript is the terminal's job now (inline
            // viewport, plan 38 slice 0): history lives in native scrollback,
            // so the mouse wheel / PageUp reach it directly. The old in-app
            // scroll keys are retired.
            _ => {}
        }
        Command::None
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> Command {
        // Ctrl-modified keys are inert here: Ctrl+C (two-tap quit) is intercepted
        // before routing, and no other Ctrl combo should trigger a y/n/j/k
        // answer. Esc denies the prompt (below); to stop the turn, deny then Esc.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::None;
        }
        // Scroll the popup body (a tall diff) instead of answering. j/k mirror
        // Up/Down for keyboard-home users; the render clamps the offset.
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                self.confirm_scroll = self.confirm_scroll.saturating_sub(1);
                return Command::None;
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.confirm_scroll += 1;
                return Command::None;
            }
            KeyCode::PageUp => {
                self.confirm_scroll = self.confirm_scroll.saturating_sub(10);
                return Command::None;
            }
            KeyCode::PageDown => {
                self.confirm_scroll += 10;
                return Command::None;
            }
            _ => {}
        }
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Decision::Allow,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Decision::Deny,
            KeyCode::Char('a') | KeyCode::Char('A') => Decision::AllowSession,
            KeyCode::Char('p') | KeyCode::Char('P') => Decision::AllowAlways,
            _ => return Command::None,
        };
        let pending = self.confirms.pop_front().expect("checked non-empty");
        // The next queued prompt (if any) starts unscrolled.
        self.confirm_scroll = 0;
        // a/p degrade to allow-once in the gate when the call isn't
        // remember-able, same as the plain REPL.
        let _ = pending.reply.send(decision);
        Command::None
    }

    /// Keys while the rewind picker is open. ↑↓/kj move the cursor, Enter forks
    /// at the selected point, Esc backs out without touching History (Ctrl+C is
    /// the two-tap quit, intercepted before routing here).
    fn on_fork_key(&mut self, key: KeyEvent) -> Command {
        let picker = self.fork_picker.as_mut().expect("checked some");
        // Ctrl-modified keys are inert here (Ctrl+C two-tap quit is intercepted
        // before routing; no Ctrl combo should move the cursor).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::None;
        }
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                picker.cursor = picker.cursor.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                picker.cursor = (picker.cursor + 1).min(picker.points.len() - 1);
            }
            KeyCode::Enter => {
                // The picker is only opened with a non-empty list, so the cursor
                // always indexes a real point.
                let seq = picker.points[picker.cursor].seq;
                self.fork_picker = None;
                return Command::Fork(seq);
            }
            KeyCode::Esc => self.fork_picker = None,
            _ => {}
        }
        Command::None
    }
}

/// Replay a resumed session's history into transcript cells so `--resume`
/// shows the conversation instead of a blank screen. Tool calls collapse to
/// the same status rows the live path produces: paired result's `is_error`
/// decides ✓/✗, and an unpaired call renders as failed (resume repair marks
/// orphans as interrupted errors anyway). Tool-result blocks themselves are
/// skipped — their content is history-internal.
pub fn cells_from_history(messages: &[Message]) -> Vec<Cell> {
    let result_errors: HashMap<&str, bool> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => Some((tool_use_id.as_str(), *is_error)),
            _ => None,
        })
        .collect();
    let mut cells = Vec::new();
    for message in messages {
        for block in &message.content {
            match (message.role, block) {
                (Role::User, ContentBlock::Text { text }) => {
                    cells.push(Cell::User(text.clone()));
                }
                // A user image replays as a placeholder line: the base64 is not
                // shown, only that an image rode this turn.
                (
                    Role::User,
                    ContentBlock::Image {
                        source: ImageSource::Base64 { media_type, .. },
                    },
                ) => {
                    cells.push(Cell::User(format!("[image: {media_type}]")));
                }
                (Role::Assistant, ContentBlock::Text { text }) => {
                    cells.push(Cell::Assistant(text.clone()));
                }
                // Empty thinking text (display=omitted models) has nothing to
                // show; redacted thinking never does.
                (Role::Assistant, ContentBlock::Thinking { thinking, .. })
                    if !thinking.is_empty() =>
                {
                    cells.push(Cell::Thinking(thinking.clone()));
                }
                // A historical todo_write replays as its checklist block, the
                // same shape the live path renders (never a generic tool row).
                (Role::Assistant, ContentBlock::ToolUse { name, input, .. })
                    if name == "todo_write" =>
                {
                    if let Some(items) = kloop_core::tools::parse_todos(input) {
                        cells.push(Cell::Todo(items));
                    }
                }
                (Role::Assistant, ContentBlock::ToolUse { id, name, input }) => {
                    cells.push(Cell::Tool {
                        name: name.clone(),
                        // Same 120-char cap as the live tool_start summary.
                        summary: input.to_string().chars().take(120).collect(),
                        status: match result_errors.get(id.as_str()) {
                            Some(false) => ToolStatus::Ok,
                            Some(true) | None => ToolStatus::Failed,
                        },
                    });
                }
                _ => {}
            }
        }
    }
    if !cells.is_empty() {
        cells.push(Cell::Note(format!(
            "resumed session — {} message(s)",
            messages.len()
        )));
    }
    cells
}

fn byte_index(s: &str, char_index: usize) -> usize {
    s.char_indices()
        .nth(char_index)
        .map(|(i, _)| i)
        .unwrap_or(s.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
    }

    /// shift+Tab cycles the mode (manual → accept-edits → plan → manual),
    /// updating the App's badge and emitting SetMode for the loop to apply; an
    /// exit_plan_mode approval (ModeChanged) refreshes the badge without a key.
    #[test]
    fn shift_tab_cycles_mode_and_mode_changed_syncs_badge() {
        let mut app = App::new("s".into());
        assert_eq!(app.mode, Mode::Manual);
        assert_eq!(
            app.on_key(key(KeyCode::BackTab)),
            Command::SetMode(Mode::AcceptEdits)
        );
        assert_eq!(app.mode, Mode::AcceptEdits);
        assert_eq!(
            app.on_key(key(KeyCode::BackTab)),
            Command::SetMode(Mode::Plan)
        );
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(
            app.on_key(key(KeyCode::BackTab)),
            Command::SetMode(Mode::Manual)
        );
        assert_eq!(app.mode, Mode::Manual);

        // The agent side leaving plan mode syncs the badge with no keypress.
        app.mode = Mode::Plan;
        app.apply(AgentEvent::ModeChanged(Mode::AcceptEdits));
        assert_eq!(app.mode, Mode::AcceptEdits);
    }

    #[test]
    fn deltas_accumulate_until_a_tool_row_splits_them() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::TextDelta("hel".into()));
        app.apply(AgentEvent::TextDelta("lo".into()));
        app.apply(AgentEvent::ToolStart {
            agent: String::new(),
            id: "t1".into(),
            name: "bash".into(),
            summary: "{}".into(),
        });
        app.apply(AgentEvent::TextDelta("world".into()));
        app.apply(AgentEvent::ToolEnd {
            agent: String::new(),
            id: "t1".into(),
            ok: false,
        });

        assert_eq!(
            app.cells,
            vec![
                Cell::Assistant("hello".into()),
                Cell::Tool {
                    name: "bash".into(),
                    summary: "{}".into(),
                    status: ToolStatus::Failed,
                },
                Cell::Assistant("world".into()),
            ]
        );
    }

    /// Sub-agent events fold into one Agent row each: tool calls bump the
    /// counter and preview instead of adding cells, AgentEnd resolves the
    /// status — two parallel agents never interleave rows.
    #[test]
    fn subagent_events_fold_into_one_row_per_agent() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::AgentStart {
            agent: "agent-1".into(),
            task: "find the bug".into(),
        });
        app.apply(AgentEvent::AgentStart {
            agent: "agent-2".into(),
            task: "write the docs".into(),
        });
        // Interleaved tool activity from both agents plus the main agent.
        app.apply(AgentEvent::ToolStart {
            agent: "agent-1".into(),
            id: "t1".into(),
            name: "grep".into(),
            summary: "{\"pattern\":\"bug\"}".into(),
        });
        app.apply(AgentEvent::ToolStart {
            agent: "agent-2".into(),
            id: "t2".into(),
            name: "read_file".into(),
            summary: "{\"path\":\"README\"}".into(),
        });
        app.apply(AgentEvent::ToolEnd {
            agent: "agent-1".into(),
            id: "t1".into(),
            ok: true,
        });
        app.apply(AgentEvent::ToolStart {
            agent: "agent-1".into(),
            id: "t3".into(),
            name: "bash".into(),
            summary: "{\"command\":\"cargo test\"}".into(),
        });
        app.apply(AgentEvent::AgentEnd {
            agent: "agent-1".into(),
            ok: true,
        });
        app.apply(AgentEvent::AgentEnd {
            agent: "agent-2".into(),
            ok: false,
        });

        assert_eq!(
            app.cells,
            vec![
                Cell::Agent {
                    agent: "agent-1".into(),
                    task: "find the bug".into(),
                    status: ToolStatus::Ok,
                    tools: 2,
                    last_tool: "bash {\"command\":\"cargo test\"}".into(),
                },
                Cell::Agent {
                    agent: "agent-2".into(),
                    task: "write the docs".into(),
                    status: ToolStatus::Failed,
                    tools: 1,
                    last_tool: "read_file {\"path\":\"README\"}".into(),
                },
            ]
        );
    }

    /// Committing the front cells to scrollback drops them here and re-bases
    /// every index-into-`cells` map: entries in the committed prefix vanish,
    /// the rest shift down by the committed count.
    #[test]
    fn drain_committed_rebases_index_maps() {
        let mut app = App::new("s".into());
        // Two tool cells and a live todo scattered through the tail.
        app.apply(AgentEvent::ToolStart {
            agent: String::new(),
            id: "t1".into(),
            name: "bash".into(),
            summary: "{}".into(),
        });
        app.apply(AgentEvent::TodoUpdate {
            todos: vec![todo("Do", "Doing", TodoStatus::InProgress)],
        });
        app.apply(AgentEvent::ToolStart {
            agent: String::new(),
            id: "t2".into(),
            name: "grep".into(),
            summary: "{}".into(),
        });
        // cells: [Tool t1 (0), Todo (1), Tool t2 (2)]
        assert_eq!(app.cells.len(), 3);

        app.drain_committed(2);
        // Only Tool t2 remains, now at index 0.
        assert_eq!(app.cells.len(), 1);
        assert_eq!(app.tool_cells.get("t1"), None, "committed cell dropped");
        assert_eq!(app.tool_cells.get("t2"), Some(&0), "survivor shifted down");
        assert_eq!(app.todo_cell, None, "committed todo cell dropped");

        // A late ToolEnd for the now-frozen t1 is a harmless no-op; t2 resolves.
        app.apply(AgentEvent::ToolEnd {
            agent: String::new(),
            id: "t1".into(),
            ok: true,
        });
        app.apply(AgentEvent::ToolEnd {
            agent: String::new(),
            id: "t2".into(),
            ok: true,
        });
        assert_eq!(
            app.cells,
            vec![Cell::Tool {
                name: "grep".into(),
                summary: "{}".into(),
                status: ToolStatus::Ok,
            }]
        );
    }

    /// A sub-agent still Running when the turn dies (interrupt drops the task
    /// future before its AgentEnd) is patched to Failed.
    #[test]
    fn turn_end_fails_agents_left_running() {
        let mut app = App::new("s".into());
        app.running = true;
        app.apply(AgentEvent::AgentStart {
            agent: "agent-1".into(),
            task: "long job".into(),
        });
        app.apply(AgentEvent::TurnEnded(EndReason::Aborted));
        assert_eq!(
            app.cells[0],
            Cell::Agent {
                agent: "agent-1".into(),
                task: "long job".into(),
                status: ToolStatus::Failed,
                tools: 0,
                last_tool: String::new(),
            }
        );
    }

    /// Thinking and answer deltas accumulate into separate cells, in stream
    /// order — a thinking burst between text closes and reopens the answer.
    #[test]
    fn thinking_deltas_get_their_own_cell() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ThinkingDelta("let me".into()));
        app.apply(AgentEvent::ThinkingDelta(" see".into()));
        app.apply(AgentEvent::TextDelta("answer".into()));
        app.apply(AgentEvent::ThinkingDelta("more thought".into()));
        app.apply(AgentEvent::TextDelta("!".into()));

        assert_eq!(
            app.cells,
            vec![
                Cell::Thinking("let me see".into()),
                Cell::Assistant("answer".into()),
                Cell::Thinking("more thought".into()),
                Cell::Assistant("!".into()),
            ]
        );
    }

    fn todo(content: &str, active: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            active_form: active.into(),
            status,
        }
    }

    /// A todo_write call renders as a single Todo block, not a generic tool
    /// row: its ToolStart is suppressed and TodoUpdate owns the cell, updated
    /// in place as the list evolves within a turn.
    #[test]
    fn todo_write_renders_as_a_single_updating_block() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ToolStart {
            agent: String::new(),
            id: "t1".into(),
            name: "todo_write".into(),
            summary: "{\"todos\":[...]}".into(),
        });
        // No tool row appeared for the suppressed call.
        assert!(app.cells.is_empty());

        let first = vec![
            todo("Parse", "Parsing", TodoStatus::InProgress),
            todo("Test", "Testing", TodoStatus::Pending),
        ];
        app.apply(AgentEvent::TodoUpdate {
            todos: first.clone(),
        });
        assert_eq!(app.cells, vec![Cell::Todo(first)]);

        // A second update within the turn replaces the same cell in place.
        let second = vec![
            todo("Parse", "Parsing", TodoStatus::Completed),
            todo("Test", "Testing", TodoStatus::InProgress),
        ];
        app.apply(AgentEvent::TodoUpdate {
            todos: second.clone(),
        });
        assert_eq!(app.cells, vec![Cell::Todo(second)], "updated in place");
        assert_eq!(app.last_note.as_deref(), Some("todos 1/2"));

        // The suppressed ToolEnd is a no-op (no tool cell was tracked).
        app.apply(AgentEvent::ToolEnd {
            agent: String::new(),
            id: "t1".into(),
            ok: true,
        });
        assert_eq!(app.cells.len(), 1);
    }

    /// A new user turn starts a fresh Todo block; the previous turn's stays in
    /// the transcript as history.
    #[test]
    fn new_turn_starts_a_fresh_todo_block() {
        let mut app = App::new("s".into());
        let plan = vec![todo("Step", "Doing step", TodoStatus::InProgress)];
        app.apply(AgentEvent::TodoUpdate {
            todos: plan.clone(),
        });
        app.apply(AgentEvent::TurnEnded(EndReason::Completed));

        // Submit a new turn, then the model writes todos again.
        for c in "next".chars() {
            app.on_key(key(KeyCode::Char(c)));
        }
        app.on_key(key(KeyCode::Enter));
        let plan2 = vec![todo("Other", "Doing other", TodoStatus::Pending)];
        app.apply(AgentEvent::TodoUpdate {
            todos: plan2.clone(),
        });

        assert_eq!(
            app.cells,
            vec![
                Cell::Todo(plan),
                Cell::User("next".into()),
                Cell::Todo(plan2),
            ],
            "the new turn's list is a separate block below the user message"
        );
    }

    /// A sub-agent's todo_update never reaches the loop (dropped in ChannelUi),
    /// so the App only ever sees main-agent TodoUpdate events — but defend the
    /// invariant here too: an empty-agent update is the only one that renders.
    #[test]
    fn resume_replays_todo_write_as_a_checklist_block() {
        use serde_json::json;
        let messages = vec![
            Message::user_text("plan it"),
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "todo_write".into(),
                input: json!({"todos": [
                    {"content": "Parse", "activeForm": "Parsing", "status": "completed"},
                    {"content": "Test", "activeForm": "Testing", "status": "in_progress"},
                ]}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "Updated todo list: 2 item(s)".into(),
                is_error: false,
            }]),
        ];
        let cells = cells_from_history(&messages);
        assert_eq!(
            cells,
            vec![
                Cell::User("plan it".into()),
                Cell::Todo(vec![
                    todo("Parse", "Parsing", TodoStatus::Completed),
                    todo("Test", "Testing", TodoStatus::InProgress),
                ]),
                Cell::Note("resumed session — 3 message(s)".into()),
            ],
            "a historical todo_write replays as its checklist, not a tool row"
        );
    }

    #[test]
    fn typing_editing_and_submit() {
        let mut app = App::new("s".into());
        type_str(&mut app, "你好ab");
        app.on_key(key(KeyCode::Left));
        app.on_key(key(KeyCode::Backspace)); // removes 'a'
        app.on_key(key(KeyCode::Home));
        app.on_key(key(KeyCode::Right));
        type_str(&mut app, "x");
        assert_eq!(app.input, "你x好b");

        let cmd = app.on_key(key(KeyCode::Enter));
        assert_eq!(cmd, Command::Submit("你x好b".into()));
        assert!(app.running);
        assert_eq!(app.input, "");
        assert_eq!(app.cells, vec![Cell::User("你x好b".into())]);

        // While running, Enter steers instead of starting a new turn.
        type_str(&mut app, "next");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Command::Steer("next".into())
        );
        assert_eq!(
            app.cells,
            vec![Cell::User("你x好b".into()), Cell::User("next".into())]
        );
    }

    /// Steering (Enter while a turn runs) queues the text as Command::Steer and
    /// shows it as a User cell, but does not end/restart the turn or reset the
    /// live todo block.
    #[test]
    fn steering_while_running_queues_without_a_new_turn() {
        let mut app = App::new("s".into());
        app.running = true;
        app.todo_cell = Some(0);
        type_str(&mut app, "also do X");
        let cmd = app.on_key(key(KeyCode::Enter));
        assert_eq!(cmd, Command::Steer("also do X".into()));
        assert!(app.running, "steering does not end or restart the turn");
        assert_eq!(app.input, "");
        assert_eq!(app.todo_cell, Some(0), "a steer keeps the live todo block");
        assert_eq!(app.cells, vec![Cell::User("also do X".into())]);
    }

    /// An idle slash line routes to the worker as Command::Slash and marks the
    /// app busy, without pushing a User cell or starting a todo block. While a
    /// turn runs, the same text is steering — Ctrl+C is the only hard stop.
    #[test]
    fn slash_command_routes_only_when_idle() {
        let mut app = App::new("s".into());
        type_str(&mut app, "/help");
        let cmd = app.on_key(key(KeyCode::Enter));
        assert_eq!(cmd, Command::Slash("/help".into()));
        assert!(app.running, "the app shows busy until the worker replies");
        assert_eq!(app.input, "");
        assert!(app.cells.is_empty(), "a command is not a User message");

        // While running, a '/'-line is just steering text, not a command.
        type_str(&mut app, "/cost");
        assert_eq!(
            app.on_key(key(KeyCode::Enter)),
            Command::Steer("/cost".into())
        );
        assert_eq!(app.cells, vec![Cell::User("/cost".into())]);
    }

    /// A command's System output renders as its own cell; ClearTranscript wipes
    /// the transcript view to match History being emptied on the worker.
    #[test]
    fn system_output_and_clear_transcript() {
        let mut app = App::new("s".into());
        app.cells.push(Cell::User("earlier".into()));
        app.tool_cells.insert("t1".into(), 0);
        app.todo_cell = Some(3);

        app.apply(AgentEvent::System(
            "model: x\ncontext: ~0 / 100 tokens (0%)".into(),
        ));
        assert_eq!(
            app.cells.last(),
            Some(&Cell::System(
                "model: x\ncontext: ~0 / 100 tokens (0%)".into()
            ))
        );

        app.apply(AgentEvent::ClearTranscript);
        assert!(app.cells.is_empty());
        assert!(app.tool_cells.is_empty());
        assert_eq!(app.todo_cell, None);
    }

    fn fp(seq: u64, preview: &str) -> ForkPoint {
        ForkPoint {
            seq,
            preview: preview.into(),
        }
    }

    /// Ctrl+R asks for rewind targets only when idle; a running turn owns
    /// History, so it is ignored mid-turn.
    #[test]
    fn ctrl_r_requests_fork_points_only_when_idle() {
        let mut app = App::new("s".into());
        assert_eq!(app.on_key(ctrl('r')), Command::RequestForkPoints);
        app.running = true;
        assert_eq!(app.on_key(ctrl('r')), Command::None);
    }

    /// The picker opens on ForkPoints with the cursor on the newest turn; ↑
    /// moves it, other keys are swallowed (not typed into the input), and Enter
    /// forks at the selected seq and closes the picker.
    #[test]
    fn fork_picker_navigates_and_selects() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "two"), fp(6, "three")]));
        assert_eq!(app.fork_picker.as_ref().unwrap().cursor, 1, "starts newest");

        // A stray character is captured by the picker, not inserted as input.
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "");

        app.on_key(key(KeyCode::Up));
        assert_eq!(app.fork_picker.as_ref().unwrap().cursor, 0);
        assert_eq!(app.on_key(key(KeyCode::Enter)), Command::Fork(4));
        assert!(app.fork_picker.is_none(), "selecting closes the picker");
    }

    /// Esc backs out of the picker without forking.
    #[test]
    fn fork_picker_esc_cancels() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "two")]));
        assert_eq!(app.on_key(key(KeyCode::Esc)), Command::None);
        assert!(app.fork_picker.is_none());
    }

    /// Nothing to rewind to surfaces as a System note, not an empty picker.
    #[test]
    fn empty_fork_points_note_instead_of_picker() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![]));
        assert!(app.fork_picker.is_none());
        assert_eq!(
            app.cells.last(),
            Some(&Cell::System("nothing to rewind to yet".into()))
        );
    }

    /// A completed rewind rebuilds the transcript from the fork's history and
    /// adopts its session id.
    #[test]
    fn forked_rebuilds_transcript_and_adopts_session_id() {
        let mut app = App::new("old".into());
        app.cells.push(Cell::User("stale".into()));
        app.apply(AgentEvent::Forked {
            session_id: "new".into(),
            messages: vec![
                Message::user_text("one"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "done".into(),
                }]),
            ],
        });
        assert_eq!(app.session_id, "new");
        assert_eq!(
            app.cells,
            vec![
                Cell::User("one".into()),
                Cell::Assistant("done".into()),
                Cell::Note("rewound — 2 message(s) kept".into()),
            ]
        );
        assert!(app.fork_picker.is_none());
    }

    #[test]
    fn empty_input_never_submits() {
        let mut app = App::new("s".into());
        assert_eq!(app.on_key(key(KeyCode::Enter)), Command::None);
        type_str(&mut app, "   ");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Command::None);
        assert!(!app.running);
        assert!(app.cells.is_empty());
    }

    /// Ctrl+C is a two-tap quit (CC parity): the first press arms a hint and
    /// leaves the input alone, the second quits, any other key disarms. It no
    /// longer interrupts or clears the input — Esc does both.
    #[test]
    fn ctrl_c_two_tap_quits() {
        let mut app = App::new("s".into());
        type_str(&mut app, "draft");
        // First Ctrl+C arms (no quit, input untouched).
        assert_eq!(app.on_key(ctrl('c')), Command::None);
        assert!(app.ctrl_c_exit_armed);
        assert_eq!(app.input, "draft");
        // Second Ctrl+C quits.
        assert_eq!(app.on_key(ctrl('c')), Command::Quit);

        // Any other key between the taps disarms it.
        app.on_key(ctrl('c'));
        app.on_key(key(KeyCode::Char('x')));
        assert!(!app.ctrl_c_exit_armed, "a non-Ctrl+C key disarms");
        assert_eq!(
            app.on_key(ctrl('c')),
            Command::None,
            "back to the first tap"
        );

        // Works while running too (quit aborts the turn).
        app.on_key(key(KeyCode::Char('y'))); // disarm
        app.running = true;
        assert_eq!(app.on_key(ctrl('c')), Command::None);
        assert_eq!(app.on_key(ctrl('c')), Command::Quit);
        // Ctrl+D is disabled — the only quit path is the two-tap Ctrl+C.
        assert_eq!(app.on_key(ctrl('d')), Command::None);
    }

    /// Esc interrupts a running turn (the advertised key) and clears the input
    /// line when idle.
    #[test]
    fn esc_interrupts_running_and_clears_input_idle() {
        let mut app = App::new("s".into());
        type_str(&mut app, "draft");
        // Idle: Esc clears the line.
        assert_eq!(app.on_key(key(KeyCode::Esc)), Command::None);
        assert_eq!(app.input, "");
        // Running: Esc interrupts.
        app.running = true;
        assert_eq!(app.on_key(key(KeyCode::Esc)), Command::Interrupt);
    }

    /// Ctrl+C is the same two-tap quit inside a popup as in the main input —
    /// intercepted before routing, so the popup is untouched by the first tap
    /// (its own dismissal is Esc). Ctrl+D still quits a popup immediately.
    #[tokio::test]
    async fn ctrl_c_two_tap_quits_from_popups() {
        // Confirm prompt up.
        let mut app = App::new("s".into());
        app.running = true;
        let (reply, _rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "bash: rm x".into(),
                remember_rules: None,
                preview: None,
            },
            reply,
        });
        assert_eq!(app.on_key(ctrl('c')), Command::None, "first tap arms");
        assert!(app.ctrl_c_exit_armed);
        assert!(
            !app.confirms.is_empty(),
            "the prompt is untouched by the tap"
        );
        assert_eq!(app.on_key(ctrl('c')), Command::Quit, "second tap quits");

        // Rewind picker up.
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "one")]));
        assert_eq!(app.on_key(ctrl('c')), Command::None, "first tap arms");
        assert!(
            app.fork_picker.is_some(),
            "the picker is untouched by the tap"
        );
        assert_eq!(app.on_key(ctrl('c')), Command::Quit, "second tap quits");

        // Ctrl+D is disabled in popups too (inert Ctrl combo).
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "one")]));
        assert_eq!(app.on_key(ctrl('d')), Command::None);
    }

    #[tokio::test]
    async fn confirm_prompt_captures_keys_and_replies() {
        let mut app = App::new("s".into());
        app.running = true;
        let (reply, mut rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "bash: git push".into(),
                remember_rules: Some(vec!["bash(git push *)".into()]),
                preview: None,
            },
            reply,
        });

        // Normal typing is captured by the prompt, not the input line.
        app.on_key(key(KeyCode::Char('x')));
        assert_eq!(app.input, "");
        assert!(rx.try_recv().is_err());

        app.on_key(key(KeyCode::Char('a')));
        assert_eq!(rx.try_recv().unwrap(), Decision::AllowSession);
        assert!(app.confirms.is_empty());
    }

    /// While a prompt is up, arrow/j/k/PageUp/PageDown scroll the popup instead
    /// of leaking to the input line, and the offset never goes below zero.
    /// Answering advances to the next queued prompt with the offset reset.
    #[tokio::test]
    async fn confirm_scroll_keys_move_the_popup_and_reset_on_advance() {
        let mut app = App::new("s".into());
        let (r1, _rx1) = oneshot::channel();
        let (r2, _rx2) = oneshot::channel();
        let req = |d: &str| ConfirmRequest {
            description: d.into(),
            remember_rules: None,
            preview: None,
        };
        app.apply(AgentEvent::Confirm {
            req: req("first"),
            reply: r1,
        });
        app.apply(AgentEvent::Confirm {
            req: req("second"),
            reply: r2,
        });

        // Scrolling keys adjust the offset and are captured by the prompt.
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Char('j')));
        assert_eq!(app.confirm_scroll, 2);
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.confirm_scroll, 12);
        app.on_key(key(KeyCode::Up));
        app.on_key(key(KeyCode::Char('k')));
        assert_eq!(app.confirm_scroll, 10);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.confirm_scroll, 0);
        // None of that reached the input line.
        assert_eq!(app.input, "");
        // Below-zero is saturated, not wrapped.
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.confirm_scroll, 0);

        // Scroll into the first diff, then answer: the next prompt starts fresh.
        app.on_key(key(KeyCode::PageDown));
        assert_eq!(app.confirm_scroll, 10);
        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(app.confirms.front().unwrap().req.description, "second");
        assert_eq!(app.confirm_scroll, 0, "the next prompt is unscrolled");
    }

    #[tokio::test]
    async fn queued_confirms_answer_in_order() {
        let mut app = App::new("s".into());
        let (r1, mut rx1) = oneshot::channel();
        let (r2, mut rx2) = oneshot::channel();
        let req = |d: &str| ConfirmRequest {
            description: d.into(),
            remember_rules: None,
            preview: None,
        };
        app.apply(AgentEvent::Confirm {
            req: req("first"),
            reply: r1,
        });
        app.apply(AgentEvent::Confirm {
            req: req("second"),
            reply: r2,
        });

        app.on_key(key(KeyCode::Char('y')));
        assert_eq!(rx1.try_recv().unwrap(), Decision::Allow);
        assert_eq!(app.confirms.front().unwrap().req.description, "second");
        app.on_key(key(KeyCode::Char('n')));
        assert_eq!(rx2.try_recv().unwrap(), Decision::Deny);
    }

    #[test]
    fn history_replays_into_cells_with_tool_status_pairing() {
        use serde_json::json;
        let messages = vec![
            Message::user_text("do two things"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "planning".into(),
                    signature: "sig".into(),
                },
                // display=omitted models: block present, no text to show.
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig2".into(),
                },
                ContentBlock::Text {
                    text: "on it".into(),
                },
                ContentBlock::ToolUse {
                    id: "t1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
                ContentBlock::ToolUse {
                    id: "t2".into(),
                    name: "write_file".into(),
                    input: json!({"path": "x"}),
                },
            ]),
            Message::tool_results(vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "big output not shown".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "declined".into(),
                    is_error: true,
                },
            ]),
            Message::assistant(vec![
                ContentBlock::Text {
                    text: "done".into(),
                },
                // Orphaned call (no result recorded): renders as failed.
                ContentBlock::ToolUse {
                    id: "t3".into(),
                    name: "bash".into(),
                    input: json!({"command": "true"}),
                },
            ]),
        ];
        assert_eq!(
            cells_from_history(&messages),
            vec![
                Cell::User("do two things".into()),
                Cell::Thinking("planning".into()),
                Cell::Assistant("on it".into()),
                Cell::Tool {
                    name: "bash".into(),
                    summary: r#"{"command":"ls"}"#.into(),
                    status: ToolStatus::Ok,
                },
                Cell::Tool {
                    name: "write_file".into(),
                    summary: r#"{"path":"x"}"#.into(),
                    status: ToolStatus::Failed,
                },
                Cell::Assistant("done".into()),
                Cell::Tool {
                    name: "bash".into(),
                    summary: r#"{"command":"true"}"#.into(),
                    status: ToolStatus::Failed,
                },
                Cell::Note("resumed session — 4 message(s)".into()),
            ],
            "tool_result content stays out of the transcript; only status pairs back"
        );

        assert_eq!(
            cells_from_history(&[]),
            vec![],
            "fresh session: no cells, no note"
        );
    }

    /// A resumed user turn carrying an image replays as a placeholder line
    /// (media type, not the base64), alongside its text.
    #[test]
    fn cells_from_history_shows_image_placeholder() {
        let messages = vec![Message::user_with_blocks(
            "what is this",
            vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
            }],
        )];
        assert_eq!(
            cells_from_history(&messages),
            vec![
                Cell::User("what is this".into()),
                Cell::User("[image: image/png]".into()),
                Cell::Note("resumed session — 1 message(s)".into()),
            ]
        );
    }

    #[tokio::test]
    async fn turn_end_clears_running_state_and_notes_failures() {
        let mut app = App::new("s".into());
        app.running = true;
        let (reply, mut rx) = oneshot::channel::<Decision>();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "x".into(),
                remember_rules: None,
                preview: None,
            },
            reply,
        });
        app.apply(AgentEvent::TurnEnded(EndReason::Aborted));

        assert!(!app.running);
        assert!(app.confirms.is_empty());
        // The dropped sender resolves the agent-side future as Deny.
        assert!(rx.try_recv().is_err());
        assert_eq!(app.cells, vec![Cell::Note("interrupted".into())]);

        app.apply(AgentEvent::TurnEnded(EndReason::Error("boom".into())));
        assert_eq!(app.cells[1], Cell::Note("error: boom".into()));
        app.apply(AgentEvent::TurnEnded(EndReason::Completed));
        assert_eq!(app.cells.len(), 2, "completed turns add no note");
    }
}
