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
use kloop_core::tools::TodoItem;
use kloop_core::tools::TodoStatus;
use kloop_protocol::ContentBlock;
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
}

/// A permission prompt currently waiting for a keypress. Prompts queue:
/// concurrent tool batches can ask more than once before the first answer.
#[derive(Debug)]
pub struct PendingConfirm {
    pub req: ConfirmRequest,
    reply: oneshot::Sender<Decision>,
}

/// What the event loop must do after a key was handled; the side-effectful
/// counterpart to the pure state change already applied.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    None,
    /// Send this user text to the agent task (a turn is now running).
    Submit(String),
    /// Cancel the in-flight turn's CancellationToken.
    Interrupt,
    Quit,
}

pub struct App {
    pub session_id: String,
    pub cells: Vec<Cell>,
    pub input: String,
    /// Cursor position in `input`, in chars.
    pub cursor: usize,
    /// How many display lines the transcript view is scrolled up from the
    /// bottom; 0 = pinned to the latest output.
    pub scroll_up: usize,
    pub running: bool,
    pub confirms: VecDeque<PendingConfirm>,
    /// Latest agent note, surfaced in the status line while running.
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
}

impl App {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            cells: Vec::new(),
            input: String::new(),
            cursor: 0,
            scroll_up: 0,
            running: false,
            confirms: VecDeque::new(),
            last_note: None,
            assistant_open: false,
            thinking_open: false,
            tool_cells: HashMap::new(),
            agent_cells: HashMap::new(),
            todo_cell: None,
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
            AgentEvent::Confirm { req, reply } => {
                self.confirms.push_back(PendingConfirm { req, reply });
            }
            AgentEvent::TurnEnded(reason) => {
                self.running = false;
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = None;
                // Any prompt still queued belongs to the turn that just died;
                // dropping the senders resolves them as Deny.
                self.confirms.clear();
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

    pub fn on_key(&mut self, key: KeyEvent) -> Command {
        // A pending permission prompt captures the keyboard.
        if !self.confirms.is_empty() {
            return self.on_confirm_key(key);
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match (key.code, ctrl) {
            (KeyCode::Char('d'), true) => return Command::Quit,
            (KeyCode::Char('c'), true) => {
                if self.running {
                    return Command::Interrupt;
                }
                self.input.clear();
                self.cursor = 0;
            }
            (KeyCode::Enter, _) => {
                let text = self.input.trim().to_string();
                if text.is_empty() || self.running {
                    return Command::None;
                }
                self.input.clear();
                self.cursor = 0;
                self.scroll_up = 0;
                self.cells.push(Cell::User(text.clone()));
                // A new turn starts a fresh todo block instead of mutating the
                // previous turn's (which stays in the transcript as history).
                self.todo_cell = None;
                self.running = true;
                return Command::Submit(text);
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
            (KeyCode::Up, _) => self.scroll_up += 1,
            (KeyCode::Down, _) => self.scroll_up = self.scroll_up.saturating_sub(1),
            (KeyCode::PageUp, _) => self.scroll_up += 10,
            (KeyCode::PageDown, _) => self.scroll_up = self.scroll_up.saturating_sub(10),
            _ => {}
        }
        Command::None
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> Command {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => return Command::Quit,
                // Ctrl+C during a prompt interrupts the whole turn; the
                // dropped reply senders resolve as Deny on the agent side.
                KeyCode::Char('c') => return Command::Interrupt,
                _ => return Command::None,
            }
        }
        let decision = match key.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => Decision::Allow,
            KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Decision::Deny,
            KeyCode::Char('a') | KeyCode::Char('A') => Decision::AllowSession,
            KeyCode::Char('p') | KeyCode::Char('P') => Decision::AllowAlways,
            _ => return Command::None,
        };
        let pending = self.confirms.pop_front().expect("checked non-empty");
        // a/p degrade to allow-once in the gate when the call isn't
        // remember-able, same as the plain REPL.
        let _ = pending.reply.send(decision);
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

        // While running, Enter with new text is ignored (no queueing).
        type_str(&mut app, "next");
        assert_eq!(app.on_key(key(KeyCode::Enter)), Command::None);
        assert_eq!(app.cells.len(), 1);
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

    #[test]
    fn ctrl_c_interrupts_when_running_and_clears_input_when_idle() {
        let mut app = App::new("s".into());
        type_str(&mut app, "draft");
        assert_eq!(app.on_key(ctrl('c')), Command::None);
        assert_eq!(app.input, "");

        app.running = true;
        assert_eq!(app.on_key(ctrl('c')), Command::Interrupt);
        assert_eq!(app.on_key(ctrl('d')), Command::Quit);
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

    #[tokio::test]
    async fn queued_confirms_answer_in_order() {
        let mut app = App::new("s".into());
        let (r1, mut rx1) = oneshot::channel();
        let (r2, mut rx2) = oneshot::channel();
        let req = |d: &str| ConfirmRequest {
            description: d.into(),
            remember_rules: None,
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

    #[tokio::test]
    async fn turn_end_clears_running_state_and_notes_failures() {
        let mut app = App::new("s".into());
        app.running = true;
        let (reply, mut rx) = oneshot::channel::<Decision>();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "x".into(),
                remember_rules: None,
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
