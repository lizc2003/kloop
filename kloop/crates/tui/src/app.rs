//! TUI state and its transitions. Everything here is pure with respect to the
//! terminal: agent events and key events come in, transcript cells and
//! [`Command`]s come out. The event loop in lib.rs owns the side effects.

use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use crossterm::event::KeyCode;
use crossterm::event::KeyEvent;
use crossterm::event::KeyModifiers;
use kloop_core::agent::EndReason;
use kloop_core::event::AgentMessageStatus;
use kloop_core::event::AgentMessageUpdate;
use kloop_core::event::BackgroundTask;
use kloop_core::event::BackgroundTaskStatus;
use kloop_core::event::Delta;
use kloop_core::event::Event;
use kloop_core::event::Item;
use kloop_core::event::ItemStatus;
use kloop_core::inbox::Replayed;
use kloop_core::interaction::QuestionAnswer;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::interaction::QuestionRequest;
use kloop_core::permissions::ApprovalScope;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::permissions::Mode;
use kloop_core::rollout::ForkPoint;
use kloop_core::tools::TaskGraphSnapshot;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::Message;
use kloop_protocol::Role;
use tokio::sync::oneshot;

use crate::composer::Composer;
use crate::events::AgentEvent;
use crate::menu;

fn canonicalize_paste_newlines(text: &str) -> String {
    let mut canonical = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(character) = chars.next() {
        if character == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            canonical.push('\n');
        } else {
            canonical.push(character);
        }
    }
    canonical
}

fn is_composer_newline_key(key: &KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    (key.code == KeyCode::Enter && (shift || alt)) || (key.code == KeyCode::Char('j') && ctrl)
}

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
    /// Model reasoning. The text accumulates like Assistant but is not shown;
    /// the cell renders as a CC-style verb + elapsed (`∗ Thinking… (Xs)` live,
    /// `∗ Thought for Xs` once sealed). `seconds` is stamped by the event loop
    /// when the block closes — None while live or when resumed (no timing on
    /// disk).
    Thinking {
        text: String,
        seconds: Option<u64>,
    },
    Tool {
        name: String,
        /// The tool input as a JSON string; the renderer (`toolrow`) formats a
        /// human-readable row from it (`● Bash $ ls`, `Read file.rs`, …).
        input: String,
        status: ToolStatus,
        /// The result text preview (bounded in core), filled in at ToolEnd and
        /// shown under the row. None while running or when there was no output.
        output: Option<String>,
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
    /// Session-scoped detached work owns a lifecycle independent of the turn that
    /// launched it, so it must not reuse the turn-owned Agent row.
    BackgroundTask(BackgroundTask),
    /// Local peer-message delivery state, keyed by message-N and independent of
    /// the sender/recipient Agent execution lifecycle.
    AgentMessage(AgentMessageUpdate),
    /// The rule that closes a turn, carrying its wall-clock seconds. A turn
    /// that simply stops producing output leaves the screen looking like it is
    /// still working; this is where the eye stops, and it is what separates one
    /// turn from the next once both are in scrollback.
    TurnEnd(u64),
    Note(String),
    /// Output of a slash command — a wrapped, dim multi-line block (unlike a
    /// Note, which collapses to one truncated line).
    System(String),
    /// The opening session banner (plan 38 slice 6): a rounded box with the
    /// brand title plus the model, cwd, branch, and starting mode. Built once in
    /// `run` with display-ready strings (the git/env reads happen there, keeping
    /// the renderer pure) and prepended to the transcript, so it is the first
    /// thing that scrolls into scrollback.
    SessionHeader {
        model: String,
        cwd: String,
        branch: Option<String>,
        mode: String,
    },
}

/// One modal interaction owns the terminal at a time. Permission approvals and
/// general questions share this FIFO so concurrent tool calls cannot interleave.
#[derive(Debug)]
pub enum PendingInteraction {
    Confirm {
        req: ConfirmRequest,
        reply: oneshot::Sender<Decision>,
        /// Which row of [`confirm_choices`] the panel highlights. Per prompt,
        /// so a queued one always opens on its own safe default (Yes).
        cursor: usize,
    },
    Question(PendingQuestion),
}

/// One row of an approval panel: the answer it sends, how it reads, and the
/// single letter that picks it without moving the cursor. Built here, from the
/// request's authoritative `approval_scopes`, so the rendered list and the key
/// handler's mapping cannot drift apart — and so no row can offer a scope core
/// would reject.
pub struct ConfirmChoice {
    pub decision: Decision,
    pub label: String,
    pub detail: Option<String>,
    pub key: char,
}

pub fn confirm_choices(req: &ConfirmRequest) -> Vec<ConfirmChoice> {
    let rules = req
        .remember_rules
        .as_ref()
        .filter(|rules| !rules.is_empty())
        .map(|rules| rules.join(", "));
    let mut choices: Vec<ConfirmChoice> = req
        .approval_scopes
        .iter()
        .map(|scope| {
            let (label, detail, key) = match scope {
                ApprovalScope::Once => ("Yes", rules.clone(), 'y'),
                ApprovalScope::WorkspaceSession => (
                    "Yes, and don't ask again this workspace session",
                    rules.clone(),
                    'a',
                ),
                ApprovalScope::Project => (
                    "Yes, and don't ask again in this project",
                    Some(match &rules {
                        Some(rules) => {
                            format!("{rules} · across sessions and linked worktrees")
                        }
                        None => "across sessions and linked worktrees".to_string(),
                    }),
                    'p',
                ),
            };
            ConfirmChoice {
                decision: Decision::Allow(*scope),
                label: label.to_string(),
                detail: match scope {
                    // The Once row needs no rule echo: nothing is remembered.
                    ApprovalScope::Once => None,
                    _ => detail,
                },
                key,
            }
        })
        .collect();
    // Denial is always the last row, so Esc has one fixed meaning everywhere.
    choices.push(ConfirmChoice {
        decision: Decision::Deny,
        label: "No, and tell kloop what to do differently".to_string(),
        detail: None,
        key: 'n',
    });
    choices
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuestionPhase {
    Select,
    Other,
    Notes,
}

#[derive(Debug)]
pub struct PendingQuestion {
    pub req: QuestionRequest,
    pub question_index: usize,
    pub cursor: usize,
    pub selected: Vec<usize>,
    pub answers: Vec<QuestionAnswer>,
    pub phase: QuestionPhase,
    pub editor: String,
    pending_other: Option<String>,
    reply: oneshot::Sender<QuestionOutcome>,
}

impl PendingQuestion {
    fn new(req: QuestionRequest, reply: oneshot::Sender<QuestionOutcome>) -> Self {
        Self {
            req,
            question_index: 0,
            cursor: 0,
            selected: Vec::new(),
            answers: Vec::new(),
            phase: QuestionPhase::Select,
            editor: String::new(),
            pending_other: None,
            reply,
        }
    }

    pub fn current(&self) -> &kloop_core::interaction::Question {
        &self.req.questions[self.question_index]
    }

    pub fn selected_preview(&self) -> Option<&str> {
        self.selected
            .first()
            .and_then(|index| self.current().options.get(*index))
            .and_then(|option| option.preview.as_deref())
    }
}

/// The open rewind picker (plan 18): the fork targets the worker read off the
/// session file, and which one the cursor sits on. Only present while the user
/// is choosing; selecting or cancelling clears it.
pub struct ForkPicker {
    pub points: Vec<ForkPoint>,
    pub cursor: usize,
}

pub struct ProviderPicker {
    pub providers: Vec<kloop_protocol::ProviderDescriptor>,
    pub provider_cursor: usize,
    pub model_cursor: Option<usize>,
}

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
    /// Read an image off the OS clipboard (Ctrl+V) and attach it. The read is a
    /// side effect, so it runs in the event loop, not here.
    PasteClipboardImage,
    /// The composer's current `@`-token asks for file candidates: the loop walks
    /// the tree (an I/O search) and feeds matches back via
    /// [`App::set_file_results`]. Carries the exact target identity.
    SearchFiles(menu::CompletionTarget),
    Quit,
}

pub struct App {
    pub session_id: String,
    /// The uncommitted transcript tail: cells still mutating plus the recent
    /// finalized ones the viewport shows. Older finalized cells have left for
    /// native scrollback (see [`Cell`], [`App::drain_committed`]).
    pub cells: Vec<Cell>,
    /// Latest immutable projection of the root-owned task graph. `None` means the
    /// startup seed has not arrived yet; an accepted revision-0 empty snapshot is
    /// therefore distinct from uninitialized state.
    pub task_graph: Option<TaskGraphSnapshot>,
    /// Pure display preference. Snapshot updates, turns, rewinds, and `/clear`
    /// never reset it; Ctrl+T is its only mutator.
    pub show_task_graph: bool,
    /// Set when a turn ends: the panel steps off the composer, though the
    /// snapshot — and the registry records behind it — stay. The next accepted
    /// snapshot clears it. Plan 74 keeps a finished graph queryable, so this
    /// retires the display only; it is not a reset, and it never touches
    /// `show_task_graph`.
    task_panel_retired: bool,
    /// The multi-line input widget (plan 38 slice 3): text, cursor, input
    /// history, paste placeholders, and image attachments.
    pub composer: Composer,
    /// Images attached to the just-submitted turn, taken by the event loop to
    /// build the user message. Held here rather than on `Command::Submit`
    /// because `ContentBlock` isn't `Eq` (Command derives it).
    submit_images: Vec<ContentBlock>,
    pub running: bool,
    pub interactions: VecDeque<PendingInteraction>,
    /// Scroll offset (in display lines) into the active choice panel's body, so
    /// a diff or plan taller than the panel can be read in full. Reset to 0 when
    /// the front panel changes; clamped to a valid range at render time.
    pub panel_scroll: usize,
    /// Latest Agent activity (note / tool / sub-agent). No longer churned
    /// into the status line — activity shows in the transcript. Kept as recent
    /// state for the animated status HUD (plan 38 slice 5).
    pub last_note: Option<String>,
    /// Whether the last Assistant cell still accepts text deltas.
    assistant_open: bool,
    /// Same for the last Thinking cell and thinking deltas.
    thinking_open: bool,
    /// Turn-local native item ids to their live assistant/reasoning cells.
    assistant_cells: HashMap<String, usize>,
    reasoning_cells: HashMap<String, usize>,
    /// tool_use id -> cells index, to resolve ToolEnd.
    tool_cells: HashMap<String, usize>,
    /// agent label -> cells index of its Agent row.
    agent_cells: HashMap<String, usize>,
    /// Execution id -> mutable lifecycle row still present in the live tail.
    background_task_cells: HashMap<String, usize>,
    /// Running rows forced into native scrollback by the hard cap. Their terminal
    /// update becomes one linked follow-up row because scrollback is immutable.
    frozen_background_tasks: HashSet<String>,
    /// Message id -> mutable queued row still present in the live tail.
    agent_message_cells: HashMap<String, usize>,
    /// Queued message rows frozen into immutable native scrollback.
    frozen_agent_messages: HashSet<String>,
    /// The rewind picker while it is open (Ctrl+R when idle); None otherwise.
    /// While open it captures the keyboard, like a confirm prompt.
    pub fork_picker: Option<ForkPicker>,
    pub provider_picker: Option<ProviderPicker>,
    /// The current permission mode, shown in the status bar. A display mirror of
    /// the shared gate: shift+Tab updates it here and via `Command::SetMode`; an
    /// `exit_plan_mode` approval refreshes it via `Event::ModeChanged`. The
    /// loop seeds it from the real gate before the first draw.
    pub mode: Mode,
    /// Ctrl+C is a two-tap quit (CC parity): the first press arms this and shows
    /// a hint; the next Ctrl+C quits, any other key disarms it.
    pub ctrl_c_exit_armed: bool,
    /// The open completion popup (slash `/` or file `@`), or None. At most one is
    /// open at a time; it floats above the composer and captures navigation /
    /// accept / cancel keys (plan 38 slice 4).
    pub popup: Option<menu::Popup>,
    /// The slash-command catalog (built-ins + skills/commands), used to filter
    /// the `/` menu. Seeded once at startup via [`App::with_commands`].
    commands: Vec<menu::CommandInfo>,
    /// Current session working-directory projection, refreshed by CwdChanged.
    pub cwd: String,
    /// Current worktree branch, or None in the original checkout.
    pub branch: Option<String>,
    /// Compatibility mirror for legacy callers. Route state below is authoritative.
    pub model: String,
    /// The idle user selection. It is the only route projection changed by a
    /// successful provider switch; in-flight work uses [`frozen_route`].
    pub selected_route: Option<kloop_protocol::ActiveProviderRoute>,
    /// Route snapshot for the currently running turn/operation, if any.
    pub frozen_route: Option<kloop_protocol::ActiveProviderRoute>,
    /// Last successful model choice per logical provider, used by the picker
    /// when a provider is selected without an explicit model.
    pub remembered_models: HashMap<String, String>,
    /// Estimated context tokens in use (footer gauge), refreshed by the worker's
    /// `Usage` events after each turn.
    pub context_used: u64,
    /// The context window, if any (footer gauge denominator). Static per session.
    pub context_window: Option<u64>,
}

impl App {
    pub fn new(session_id: String) -> Self {
        Self {
            session_id,
            cells: Vec::new(),
            task_graph: None,
            show_task_graph: true,
            task_panel_retired: false,
            composer: Composer::new(),
            submit_images: Vec::new(),
            running: false,
            interactions: VecDeque::new(),
            panel_scroll: 0,
            last_note: None,
            assistant_open: false,
            thinking_open: false,
            assistant_cells: HashMap::new(),
            reasoning_cells: HashMap::new(),
            tool_cells: HashMap::new(),
            agent_cells: HashMap::new(),
            background_task_cells: HashMap::new(),
            frozen_background_tasks: HashSet::new(),
            agent_message_cells: HashMap::new(),
            frozen_agent_messages: HashSet::new(),
            fork_picker: None,
            provider_picker: None,
            mode: Mode::default(),
            ctrl_c_exit_armed: false,
            popup: None,
            commands: Vec::new(),
            cwd: String::new(),
            branch: None,
            model: String::new(),
            selected_route: None,
            frozen_route: None,
            remembered_models: HashMap::new(),
            context_used: 0,
            context_window: None,
        }
    }

    /// Seed the slash-command catalog for the `/` menu (built-ins + skills).
    pub fn with_commands(mut self, commands: Vec<menu::CommandInfo>) -> Self {
        self.commands = commands;
        self
    }

    pub fn with_working_directory(mut self, cwd: String, branch: Option<String>) -> Self {
        self.cwd = cwd;
        self.branch = branch;
        self
    }

    /// Seed the footer's system status (model + context window) from Config. The
    /// used-token estimate then arrives via `Usage` events.
    pub fn with_context(mut self, model: String, window: Option<u64>, used: u64) -> Self {
        self.model = model;
        self.context_window = window;
        self.context_used = used;
        self
    }

    pub fn with_route(mut self, route: kloop_protocol::ActiveProviderRoute) -> Self {
        self.model = route.model.clone();
        self.selected_route = Some(route);
        self
    }

    pub(crate) fn display_route(&self) -> Option<&kloop_protocol::ActiveProviderRoute> {
        if self.running {
            self.frozen_route.as_ref().or(self.selected_route.as_ref())
        } else {
            self.selected_route.as_ref()
        }
    }

    pub fn freeze_selected_route(&mut self) {
        self.freeze_selected_route_inner();
    }

    fn freeze_selected_route_inner(&mut self) {
        if self.frozen_route.is_none() {
            self.frozen_route = self.selected_route.clone();
        }
    }

    fn clear_frozen_route(&mut self) {
        self.frozen_route = None;
    }

    /// Remember the model only after a successful provider transition. A late
    /// ProviderChanged for an older revision cannot overwrite newer selection.
    fn accept_selected_route(&mut self, route: kloop_protocol::ActiveProviderRoute) {
        if self
            .selected_route
            .as_ref()
            .is_none_or(|current| route.revision >= current.revision)
        {
            self.remembered_models
                .insert(route.provider_id.clone(), route.model.clone());
            self.model = route.model.clone();
            self.selected_route = Some(route);
        }
    }

    fn remembered_or_default_model(&self, provider: &kloop_protocol::ProviderDescriptor) -> String {
        self.remembered_models
            .get(&provider.id)
            .filter(|model| provider.models.contains(model))
            .cloned()
            .unwrap_or_else(|| provider.default_model.clone())
    }

    pub fn picker_model(&self, provider: &kloop_protocol::ProviderDescriptor) -> String {
        self.remembered_or_default_model(provider)
    }

    pub fn apply(&mut self, event: AgentEvent) {
        match event {
            // Agent output — the single core Event stream (plan 39).
            AgentEvent::Core(ev) => self.apply_core(ev),
            AgentEvent::System(text) => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.cells.push(Cell::System(text));
            }
            AgentEvent::ProviderChanged(route) => {
                self.accept_selected_route(route.clone());
                self.cells.push(Cell::System(format!(
                    "provider: {} {} (revision {})",
                    route.provider_id, route.model, route.revision
                )));
            }
            AgentEvent::RouteFrozen(route) => {
                if self.running {
                    self.frozen_route = Some(route);
                }
            }
            AgentEvent::ProviderPicker(providers) => {
                if providers.is_empty() {
                    self.cells
                        .push(Cell::System("no configured providers".into()));
                } else {
                    self.provider_picker = Some(ProviderPicker {
                        providers,
                        provider_cursor: 0,
                        model_cursor: None,
                    });
                }
            }
            AgentEvent::ClearTranscript => {
                // /clear emptied History on the worker; drop the uncommitted
                // view state. Cells already in native scrollback stay visible
                // (inline can't erase scrollback) but are out of the model's
                // context — the System note that follows says so.
                self.cells.clear();
                self.tool_cells.clear();
                self.agent_cells.clear();
                self.background_task_cells.clear();
                self.frozen_background_tasks.clear();
                self.agent_message_cells.clear();
                self.frozen_agent_messages.clear();
                self.assistant_cells.clear();
                self.reasoning_cells.clear();
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
                route,
            } => {
                // History was swapped to the fork; rebuild the view to match its
                // truncated content, exactly like resuming into a session.
                self.session_id = session_id;
                self.accept_selected_route(route);
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
                self.background_task_cells.clear();
                self.frozen_background_tasks.clear();
                self.agent_message_cells.clear();
                self.frozen_agent_messages.clear();
                self.assistant_cells.clear();
                self.reasoning_cells.clear();
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = None;
                self.fork_picker = None;
            }
            AgentEvent::Confirm { req, reply } => {
                self.interactions.push_back(PendingInteraction::Confirm {
                    req,
                    reply,
                    cursor: 0,
                });
            }
            AgentEvent::Question { req, reply } => {
                self.interactions
                    .push_back(PendingInteraction::Question(PendingQuestion::new(
                        req, reply,
                    )));
            }
        }
    }

    /// Project one core [`Event`] onto the transcript. Message and reasoning
    /// items are driven by their deltas (their start/complete events only bound
    /// the stream — the sealing is done by the next event clearing the open
    /// flag, exactly as before plan 39); tool calls and sub-agents map to status
    /// rows.
    fn apply_core(&mut self, ev: Event) {
        match ev {
            Event::ItemStarted {
                id,
                item: Item::AssistantMessage { text, .. },
            } => {
                if let Some(&index) = self.assistant_cells.get(&id) {
                    if let Some(Cell::Assistant(current)) = self.cells.get_mut(index) {
                        *current = text;
                    }
                } else {
                    self.assistant_cells.insert(id, self.cells.len());
                    self.cells.push(Cell::Assistant(text));
                }
                self.refresh_display_streaming();
            }
            Event::ItemDelta {
                id,
                delta: Delta::Text(text),
            } => {
                let index = match self.assistant_cells.get(&id).copied() {
                    Some(index) => index,
                    None => {
                        let index = self.cells.len();
                        self.assistant_cells.insert(id, index);
                        self.cells.push(Cell::Assistant(String::new()));
                        index
                    }
                };
                if let Some(Cell::Assistant(current)) = self.cells.get_mut(index) {
                    current.push_str(&text);
                }
                self.refresh_display_streaming();
            }
            Event::ItemCompleted {
                id,
                item: Item::AssistantMessage { text, .. },
            } => {
                match self.assistant_cells.remove(&id) {
                    Some(index) => {
                        if let Some(Cell::Assistant(current)) = self.cells.get_mut(index) {
                            *current = text;
                        }
                    }
                    None if !text.is_empty() => self.cells.push(Cell::Assistant(text)),
                    None => {}
                }
                self.refresh_display_streaming();
            }
            Event::ItemStarted {
                id,
                item: Item::Reasoning { text, .. },
            } => {
                if let Some(&index) = self.reasoning_cells.get(&id) {
                    if let Some(Cell::Thinking { text: current, .. }) = self.cells.get_mut(index) {
                        *current = text;
                    }
                } else {
                    self.reasoning_cells.insert(id, self.cells.len());
                    self.cells.push(Cell::Thinking {
                        text,
                        seconds: None,
                    });
                }
                self.refresh_display_streaming();
            }
            Event::ItemDelta {
                id,
                delta: Delta::Reasoning(text),
            } => {
                let index = match self.reasoning_cells.get(&id).copied() {
                    Some(index) => index,
                    None => {
                        let index = self.cells.len();
                        self.reasoning_cells.insert(id, index);
                        self.cells.push(Cell::Thinking {
                            text: String::new(),
                            seconds: None,
                        });
                        index
                    }
                };
                if let Some(Cell::Thinking { text: current, .. }) = self.cells.get_mut(index) {
                    current.push_str(&text);
                }
                self.refresh_display_streaming();
            }
            Event::ItemCompleted {
                id,
                item: Item::Reasoning { text, .. },
            } => {
                match self.reasoning_cells.remove(&id) {
                    Some(index) => {
                        if let Some(Cell::Thinking { text: current, .. }) =
                            self.cells.get_mut(index)
                        {
                            *current = text;
                        }
                    }
                    None if !text.is_empty() => self.cells.push(Cell::Thinking {
                        text,
                        seconds: None,
                    }),
                    None => {}
                }
                self.refresh_display_streaming();
            }
            // Program output and the turn bracket have no dedicated transcript
            // cell.
            Event::ItemDelta {
                delta: Delta::Output(_),
                ..
            }
            | Event::TurnStarted => {}
            Event::TaskGraphUpdated(snapshot) => {
                let accept = self
                    .task_graph
                    .as_ref()
                    .is_none_or(|current| snapshot.revision > current.revision);
                if accept {
                    self.task_graph = Some(snapshot);
                    self.task_panel_retired = false;
                }
            }
            Event::ItemStarted {
                id,
                item: Item::ToolCall {
                    agent, name, input, ..
                },
            } => {
                let input = input.to_string();
                // A one-line "verb detail" preview used by the note stream and
                // the folded sub-agent row (the full cell is formatted at render).
                let preview = crate::toolrow::tool_preview(&name, &input);
                if !agent.is_empty() {
                    // A sub-agent's call folds into its Agent row: bump the
                    // counter, refresh the preview. No per-call cell, so
                    // parallel agents cannot interleave.
                    self.last_note = Some(format!("{agent} · {preview}"));
                    if let Some(Cell::Agent {
                        tools, last_tool, ..
                    }) = self.agent_cell(&agent)
                    {
                        *tools += 1;
                        *last_tool = preview;
                    }
                    return;
                }
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = Some(preview);
                self.tool_cells.insert(id, self.cells.len());
                self.cells.push(Cell::Tool {
                    name,
                    input,
                    status: ToolStatus::Running,
                    output: None,
                });
            }
            Event::ItemCompleted {
                id,
                item:
                    Item::ToolCall {
                        agent,
                        status,
                        output,
                        ..
                    },
            } => {
                // Sub-agent calls have no cell of their own; their agent's
                // row is resolved by its own completion.
                if !agent.is_empty() {
                    return;
                }
                if let Some(&i) = self.tool_cells.get(&id)
                    && let Some(Cell::Tool {
                        status: cell_status,
                        output: out,
                        ..
                    }) = self.cells.get_mut(i)
                {
                    *cell_status = if status == ItemStatus::Completed {
                        ToolStatus::Ok
                    } else {
                        ToolStatus::Failed
                    };
                    // Keep the preview for the transcript; empty output
                    // leaves the row a single line.
                    if let Some(text) = output {
                        *out = Some(text);
                    }
                }
            }
            Event::ItemStarted {
                item: Item::SubAgent { label, task, .. },
                ..
            } => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = Some(format!("{label} started: {task}"));
                self.agent_cells.insert(label.clone(), self.cells.len());
                self.cells.push(Cell::Agent {
                    agent: label,
                    task,
                    status: ToolStatus::Running,
                    tools: 0,
                    last_tool: String::new(),
                });
            }
            Event::ItemCompleted {
                item: Item::SubAgent { label, status, .. },
                ..
            } => {
                if let Some(Cell::Agent { status: cell, .. }) = self.agent_cell(&label) {
                    *cell = if status == ItemStatus::Completed {
                        ToolStatus::Ok
                    } else {
                        ToolStatus::Failed
                    };
                }
            }
            Event::BackgroundTaskUpdated(task) => {
                self.assistant_open = false;
                self.thinking_open = false;
                if let Some(note) = Event::BackgroundTaskUpdated(task.clone()).as_note() {
                    self.last_note = Some(note);
                }
                let terminal = task.status != BackgroundTaskStatus::Running;
                if let Some(index) = self.background_task_cells.get(&task.id).copied() {
                    if matches!(self.cells.get(index), Some(Cell::BackgroundTask(_))) {
                        self.cells[index] = Cell::BackgroundTask(task.clone());
                        if terminal {
                            self.background_task_cells.remove(&task.id);
                        }
                        return;
                    }
                    self.background_task_cells.remove(&task.id);
                }
                if self.frozen_background_tasks.contains(&task.id) {
                    if !terminal {
                        return;
                    }
                    self.frozen_background_tasks.remove(&task.id);
                    self.cells.push(Cell::BackgroundTask(task));
                    return;
                }
                let id = task.id.clone();
                let index = self.cells.len();
                self.cells.push(Cell::BackgroundTask(task));
                if !terminal {
                    self.background_task_cells.insert(id, index);
                }
            }
            Event::AgentMessageUpdated(message) => {
                self.assistant_open = false;
                self.thinking_open = false;
                if let Some(note) = Event::AgentMessageUpdated(message.clone()).as_note() {
                    self.last_note = Some(note);
                }
                let id = message.id.to_string();
                let terminal = message.status != AgentMessageStatus::Queued;
                if let Some(index) = self.agent_message_cells.get(&id).copied() {
                    if matches!(self.cells.get(index), Some(Cell::AgentMessage(_))) {
                        self.cells[index] = Cell::AgentMessage(message.clone());
                        if terminal {
                            self.agent_message_cells.remove(&id);
                        }
                        return;
                    }
                    self.agent_message_cells.remove(&id);
                }
                if self.frozen_agent_messages.contains(&id) {
                    if !terminal {
                        return;
                    }
                    self.frozen_agent_messages.remove(&id);
                    self.cells.push(Cell::AgentMessage(message));
                    return;
                }
                let index = self.cells.len();
                self.cells.push(Cell::AgentMessage(message));
                if !terminal {
                    self.agent_message_cells.insert(id, index);
                }
            }
            Event::ScheduledTaskUpdated(task) => {
                let event = Event::ScheduledTaskUpdated(task);
                if let Some(note) = event.as_note() {
                    self.assistant_open = false;
                    self.thinking_open = false;
                    self.last_note = Some(note.clone());
                    self.cells.push(Cell::Note(note));
                }
            }
            Event::Note(n) => {
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = Some(n.clone());
                self.cells.push(Cell::Note(n));
            }
            Event::CwdChanged { cwd, branch } => {
                let note = match &branch {
                    Some(branch) => format!("working directory → {cwd} (branch {branch})"),
                    None => format!("working directory → {cwd}"),
                };
                self.cwd = cwd;
                self.branch = branch;
                self.assistant_open = false;
                self.thinking_open = false;
                self.last_note = Some(note.clone());
                self.cells.push(Cell::Note(note));
            }
            Event::ModeChanged(mode) => {
                // exit_plan_mode flipped the gate on the agent side; keep the
                // status-bar badge in step.
                self.mode = mode;
            }
            Event::Usage(used) => {
                // The worker's post-turn context estimate; the footer gauge reads
                // it against the (static) window.
                self.context_used = used;
            }
            Event::TurnEnded(reason) => {
                self.running = false;
                self.clear_frozen_route();
                self.assistant_open = false;
                self.thinking_open = false;
                self.assistant_cells.clear();
                self.reasoning_cells.clear();
                self.last_note = None;
                // Any prompt still queued belongs to the turn that just died;
                // dropping the senders resolves them as Deny.
                self.interactions.clear();
                self.panel_scroll = 0;
                // The panel tracks a turn in flight, not a standing checklist:
                // it leaves the composer with the turn that raised it, finished
                // or not. Keeping an unfinished graph pinned there was the
                // common case — a model that has delivered its answer rarely
                // goes back to tick its own boxes — and between turns it read as
                // work still running. The next task update brings it back.
                self.task_panel_retired = true;
                // An interrupted turn drops task futures mid-await, so a
                // sub-agent's completion may never arrive: no row may outlive
                // its turn still spinning.
                for cell in &mut self.cells {
                    if let Cell::Agent { status, .. } = cell
                        && *status == ToolStatus::Running
                    {
                        *status = ToolStatus::Failed;
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

    /// The task graph as the chrome above the composer sees it: `None` while the
    /// graph is empty or the panel has retired, even though the snapshot is still
    /// held. The panel, Ctrl+T and the footer hint all ask this one question, so
    /// they can never disagree about whether there is a panel to toggle.
    pub fn live_task_graph(&self) -> Option<&TaskGraphSnapshot> {
        self.task_graph
            .as_ref()
            .filter(|snapshot| !snapshot.tasks.is_empty() && !self.task_panel_retired)
    }

    fn refresh_display_streaming(&mut self) {
        let last = self.cells.len().checked_sub(1);
        self.assistant_open =
            last.is_some_and(|last| self.assistant_cells.values().any(|index| *index == last));
        self.thinking_open =
            last.is_some_and(|last| self.reasoning_cells.values().any(|index| *index == last));
    }

    pub(crate) fn display_cell_live(&self, index: usize) -> bool {
        self.assistant_cells.values().any(|value| *value == index)
            || self.reasoning_cells.values().any(|value| *value == index)
    }

    /// Whether the last cell is an Assistant cell still receiving text deltas.
    /// The renderer streams that one cell with a safe-boundary buffer (so a
    /// half-formed markdown block shows raw, not reflowing); every other
    /// Assistant cell is sealed and renders as full markdown. A streaming
    /// Assistant is always the last cell — any other event closes it — so this
    /// is the single place the live/sealed distinction lives.
    pub fn streaming_assistant(&self) -> bool {
        self.assistant_open && matches!(self.cells.last(), Some(Cell::Assistant(_)))
    }

    /// The mirror of [`streaming_assistant`] for a live thinking block: the last
    /// cell is a Thinking still receiving deltas. The event loop renders this one
    /// with a running elapsed; any other cell is sealed and shows its final time.
    pub fn streaming_thinking(&self) -> bool {
        self.thinking_open && matches!(self.cells.last(), Some(Cell::Thinking { .. }))
    }

    /// Stamp the just-closed thinking block with how long it ran, so the frozen
    /// cell can show `∗ Thought for Xs`. Applies to the newest unsealed Thinking
    /// cell — at most one is open at a time, and it is the one just closed.
    pub fn seal_thinking(&mut self, seconds: u64) {
        if let Some(Cell::Thinking { seconds: s, .. }) = self
            .cells
            .iter_mut()
            .rev()
            .find(|c| matches!(c, Cell::Thinking { seconds: None, .. }))
        {
            *s = Some(seconds);
        }
    }

    /// Close the transcript's turn with its wall-clock elapsed. The event loop
    /// owns the clock (the `App` has none), so it stamps this the moment
    /// `running` drops — after any end-of-turn note, so the rule sits last.
    pub fn seal_turn(&mut self, seconds: u64) {
        self.cells.push(Cell::TurnEnd(seconds));
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
        for cell in &self.cells[..n] {
            match cell {
                Cell::BackgroundTask(task) if task.status == BackgroundTaskStatus::Running => {
                    self.frozen_background_tasks.insert(task.id.clone());
                }
                Cell::AgentMessage(message) if message.status == AgentMessageStatus::Queued => {
                    self.frozen_agent_messages.insert(message.id.to_string());
                }
                _ => {}
            }
        }
        self.cells.drain(0..n);
        self.tool_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.agent_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.background_task_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.agent_message_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.assistant_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
        self.reasoning_cells.retain(|_, i| {
            *i = i.wrapping_sub(n);
            *i < self.cells.len()
        });
    }

    pub fn on_key(&mut self, composer_width: usize, key: KeyEvent) -> Command {
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
        if ctrl
            && matches!(key.code, KeyCode::Char('t') | KeyCode::Char('T'))
            && self.live_task_graph().is_some()
        {
            self.show_task_graph = !self.show_task_graph;
            return Command::None;
        }
        // A pending interaction captures the keyboard.
        if !self.interactions.is_empty() {
            return self.on_interaction_key(key);
        }
        if self.provider_picker.is_some() {
            return self.on_provider_key(key);
        }
        // So does an open rewind picker.
        if self.fork_picker.is_some() {
            return self.on_fork_key(key);
        }
        // A newline inside the composer instead of a submit: Ctrl+J (the reliable
        // LF), or Shift/Alt+Enter where the terminal distinguishes it. Handle it
        // before completion accept so an open menu cannot steal Shift+Enter.
        if is_composer_newline_key(&key) {
            self.composer.insert_newline();
            return self.after_edit();
        }
        // An open completion popup (slash `/` or file `@`) captures navigation /
        // accept / cancel; every other key falls through to edit the composer,
        // after which the tail re-syncs the popup (re-filter or close).
        if self.popup.is_some()
            && let Some(cmd) = self.on_popup_key(key)
        {
            return cmd;
        }
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        // Ctrl+V / Alt+V pastes an image off the OS clipboard (the terminal keeps
        // Cmd+V for its own text paste, so a distinct key like codex / CC). The
        // read is a side effect the loop performs.
        if (ctrl || alt) && matches!(key.code, KeyCode::Char('v') | KeyCode::Char('V')) {
            return Command::PasteClipboardImage;
        }
        match (key.code, ctrl) {
            // Esc interrupts a running turn (CC parity, the advertised key);
            // idle it clears the composer. A confirm popup / rewind picker
            // capture Esc before this (they return early at the top of on_key).
            (KeyCode::Esc, _) => {
                if self.running {
                    return Command::Interrupt;
                }
                self.composer.clear();
            }
            (KeyCode::Char('r'), true) => {
                // Rewind (plan 18) is idle-only: a running turn owns History, so
                // Ctrl+R is ignored mid-turn. The worker answers with ForkPoints.
                if !self.running {
                    return Command::RequestForkPoints;
                }
            }
            (KeyCode::Enter, _) => return self.on_enter(),
            // shift+Tab cycles the permission mode (manual → accept-edits →
            // plan → manual; bypass is opt-in via the CLI flag only). Allowed
            // any time — the gate reads the mode live per tool call.
            (KeyCode::BackTab, _) => {
                self.mode = self.mode.cycled();
                return Command::SetMode(self.mode);
            }
            // Up/Down move the composer cursor, or step through input history at
            // the first/last line (plan 38 slice 3).
            (KeyCode::Up, _) => self.composer.up(composer_width),
            (KeyCode::Down, _) => self.composer.down(composer_width),
            (KeyCode::Char(c), false) => self.composer.insert_char(c),
            (KeyCode::Backspace, _) => self.composer.backspace(),
            (KeyCode::Delete, _) => self.composer.delete(),
            (KeyCode::Left, _) => self.composer.left(),
            (KeyCode::Right, _) => self.composer.right(),
            (KeyCode::Home, _) => self.composer.home(),
            (KeyCode::End, _) => self.composer.end(),
            // Scrolling the transcript is the terminal's job now (inline
            // viewport, plan 38 slice 0): history lives in native scrollback,
            // so the mouse wheel / PageUp reach it directly. The old in-app
            // scroll keys are retired.
            _ => return Command::None,
        }
        // A composer-editing key ran: re-sync the completion popup from the new
        // text/cursor (open/refilter a `/`/`@` menu, request a file search, or
        // close it). Non-editing arms above return directly and skip this.
        self.after_edit()
    }

    /// Re-derive the completion popup from the composer after an edit. Returns
    /// [`Command::SearchFiles`] when an `@`-token needs the loop to walk the
    /// tree; otherwise resolves the slash menu (or closes the popup) here and
    /// returns [`Command::None`].
    fn after_edit(&mut self) -> Command {
        match menu::detect_trigger(self.composer.text(), self.composer.cursor(), !self.running) {
            Some(target) if target.kind == menu::PopupKind::Slash => {
                let items = menu::slash_items(&self.commands, &target.query);
                self.popup = (!items.is_empty()).then_some(menu::Popup {
                    target,
                    items,
                    cursor: 0,
                });
                Command::None
            }
            // The loop searches and calls `set_file_results`; leave the current
            // popup (if any) until the results land so the menu doesn't flicker.
            Some(target) => Command::SearchFiles(target),
            None => {
                self.popup = None;
                Command::None
            }
        }
    }

    /// A key while a completion popup is open. `Some(cmd)` when the popup consumed
    /// it (navigation / accept / cancel); `None` to let it fall through to
    /// composer editing (which then re-syncs via [`App::after_edit`]).
    fn on_popup_key(&mut self, key: KeyEvent) -> Option<Command> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Up => {
                self.popup.as_mut()?.move_up();
                Some(Command::None)
            }
            KeyCode::Down => {
                self.popup.as_mut()?.move_down();
                Some(Command::None)
            }
            KeyCode::Char('p') if ctrl => {
                self.popup.as_mut()?.move_up();
                Some(Command::None)
            }
            KeyCode::Char('n') if ctrl => {
                self.popup.as_mut()?.move_down();
                Some(Command::None)
            }
            // Tab / Enter complete the highlighted entry into the composer; Enter
            // does not submit while the menu is open (plan: Tab/Enter 补全).
            KeyCode::Tab | KeyCode::Enter => {
                self.accept_popup();
                Some(Command::None)
            }
            // Esc closes the menu without touching the composer (a running turn's
            // interrupt / idle clear is a second Esc, once the menu is gone).
            KeyCode::Esc => {
                self.popup = None;
                Some(Command::None)
            }
            _ => None,
        }
    }

    /// Insert the highlighted entry's replacement token and close the popup. The
    /// completed token carries a trailing space, so its trigger no longer fires.
    fn accept_popup(&mut self) {
        if let Some(popup) = self.popup.take() {
            let current =
                menu::detect_trigger(self.composer.text(), self.composer.cursor(), !self.running);
            if current.as_ref() != Some(&popup.target) {
                return;
            }
            if let Some(item) = popup.selected() {
                self.composer
                    .replace_range(popup.target.range, popup.target.cursor, &item.insert);
            }
        }
    }

    /// Fill the file menu with the loop's search results for `target`. Ignored if
    /// the composer has moved on, so a late result cannot reopen a same-query
    /// token at another byte range; an empty result closes the popup.
    pub fn set_file_results(&mut self, target: menu::CompletionTarget, paths: Vec<String>) {
        let current =
            menu::detect_trigger(self.composer.text(), self.composer.cursor(), !self.running);
        if current.as_ref() != Some(&target) || target.kind != menu::PopupKind::File {
            return;
        }
        let items = menu::file_items(paths);
        self.popup = (!items.is_empty()).then_some(menu::Popup {
            target,
            items,
            cursor: 0,
        });
    }

    /// Enter: submit the composer (or run a slash command / steer a running
    /// turn). Newline-insert (Shift/Alt+Enter, Ctrl+J) is handled before this.
    fn on_enter(&mut self) -> Command {
        if self.composer.is_blank() {
            return Command::None;
        }
        // The compact display text (placeholders intact) — shown in the
        // transcript and used for slash detection; the submission carries the
        // expanded text.
        let display = self.composer.text().trim().to_string();
        // A slash command runs only when idle; it is not a message, so no User
        // cell. While a turn runs, a '/'-line is steering.
        if !self.running && !display.is_empty() && kloop_core::commands::is_command(&display) {
            let _ = self.composer.submit_text();
            self.running = true;
            self.freeze_selected_route();
            return Command::Slash(display);
        }
        if self.running {
            // Steering: only the text rides the running turn (absorbed at its
            // next round boundary). Attached images stay on the composer for the
            // next FRESH turn — a steer has no image channel, so taking them here
            // (and showing a `[image: …]` cell) would drop them silently. An
            // image-only Enter has nothing to steer, so it is a no-op that leaves
            // the image attached. No new turn starts.
            return match self.composer.submit_text() {
                Some(text) => {
                    self.cells.push(Cell::User(display));
                    Command::Steer(text)
                }
                None => Command::None,
            };
        }
        let labels: Vec<String> = self
            .composer
            .attachments()
            .iter()
            .map(|attachment| attachment.label.clone())
            .collect();
        let sub = self.composer.submit().expect("checked not blank");
        self.submit_images = sub.images;
        if !display.is_empty() {
            self.cells.push(Cell::User(display));
        }
        // Each attached image replays as a placeholder line, like a resumed one.
        for label in &labels {
            self.cells.push(Cell::User(format!("[image: {label}]")));
        }
        self.running = true;
        self.freeze_selected_route();
        Command::Submit(sub.text)
    }

    /// Take the images attached to the turn just submitted (the event loop builds
    /// the user message with them). Empty for a text-only turn.
    pub fn take_submit_images(&mut self) -> Vec<ContentBlock> {
        std::mem::take(&mut self.submit_images)
    }

    /// A bracketed-paste of text (the event loop routes image-file pastes to
    /// [`App::attach_image`] instead).
    pub fn paste_text(&mut self, s: &str) -> Command {
        let text = canonicalize_paste_newlines(s);
        if let Some(PendingInteraction::Question(question)) = self.interactions.front_mut()
            && matches!(question.phase, QuestionPhase::Other | QuestionPhase::Notes)
        {
            question.editor.push_str(&text);
            return Command::None;
        }
        self.composer.paste(&text);
        self.after_edit()
    }

    pub fn interaction_active(&self) -> bool {
        !self.interactions.is_empty()
    }

    pub fn question_editor_active(&self) -> bool {
        matches!(
            self.interactions.front(),
            Some(PendingInteraction::Question(PendingQuestion {
                phase: QuestionPhase::Other | QuestionPhase::Notes,
                ..
            }))
        )
    }

    fn on_interaction_key(&mut self, key: KeyEvent) -> Command {
        match self.interactions.front() {
            Some(PendingInteraction::Confirm { .. }) => self.on_confirm_key(key),
            Some(PendingInteraction::Question(_)) => self.on_question_key(key),
            None => Command::None,
        }
    }

    /// Attach a pasted image (the loop already read + validated it into a block).
    pub fn attach_image(&mut self, label: String, block: ContentBlock) {
        self.composer.attach_image(label, block);
    }

    fn on_confirm_key(&mut self, key: KeyEvent) -> Command {
        // Ctrl-modified keys are inert here: Ctrl+C (two-tap quit) is intercepted
        // before routing, and no other Ctrl combo should answer a prompt.
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::None;
        }
        // Paging scrolls the panel body (a tall diff or plan); the arrows belong
        // to the option list now, so a long preview is read with PgUp/PgDn.
        match key.code {
            KeyCode::PageUp => {
                self.panel_scroll = self.panel_scroll.saturating_sub(10);
                return Command::None;
            }
            KeyCode::PageDown => {
                self.panel_scroll += 10;
                return Command::None;
            }
            _ => {}
        }
        let Some(PendingInteraction::Confirm { req, cursor, .. }) = self.interactions.front_mut()
        else {
            return Command::None;
        };
        let choices = confirm_choices(req);
        let last = choices.len() - 1;
        let decision = match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                *cursor = cursor.saturating_sub(1);
                None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                *cursor = (*cursor + 1).min(last);
                None
            }
            KeyCode::Enter => Some(choices[(*cursor).min(last)].decision),
            // Esc is the deny row, which is always last.
            KeyCode::Esc => Some(choices[last].decision),
            // The row numbers the panel prints, and the y/a/p/n letters a
            // returning user's fingers already know.
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let row = c.to_digit(10).expect("ascii digit") as usize - 1;
                choices.get(row).map(|choice| choice.decision)
            }
            KeyCode::Char(c) => choices
                .iter()
                .find(|choice| choice.key == c.to_ascii_lowercase())
                .map(|choice| choice.decision),
            _ => None,
        };
        let Some(decision) = decision else {
            return Command::None;
        };
        let pending = self.interactions.pop_front().expect("checked non-empty");
        let PendingInteraction::Confirm { reply, .. } = pending else {
            unreachable!("interaction type changed while handling approval")
        };
        // The next queued prompt (if any) starts unscrolled.
        self.panel_scroll = 0;
        let _ = reply.send(decision);
        Command::None
    }

    fn on_question_key(&mut self, key: KeyEvent) -> Command {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::None;
        }
        if key.code == KeyCode::Esc {
            self.resolve_question(QuestionOutcome::Cancelled);
            return Command::None;
        }

        let Some(PendingInteraction::Question(question)) = self.interactions.front_mut() else {
            return Command::None;
        };
        // A row number acts as "move there, then confirm" — or, on a
        // multi-select's own options, as "tick that row".
        let key = match key.code {
            KeyCode::Char(c)
                if question.phase == QuestionPhase::Select && c.is_ascii_digit() && c != '0' =>
            {
                let row = c.to_digit(10).expect("ascii digit") as usize - 1;
                let options = question.current().options.len();
                // The two rows past the options are Other and Chat about this.
                if row > options + 1 {
                    return Command::None;
                }
                let tick = question.current().multi_select && row < options;
                question.cursor = row;
                KeyEvent::new(
                    if tick {
                        KeyCode::Char(' ')
                    } else {
                        KeyCode::Enter
                    },
                    key.modifiers,
                )
            }
            _ => key,
        };
        let mut outcome = None;
        match question.phase {
            QuestionPhase::Select => {
                let option_count = question.current().options.len();
                match key.code {
                    KeyCode::Up | KeyCode::Char('k') => {
                        question.cursor = question.cursor.saturating_sub(1);
                    }
                    KeyCode::Down | KeyCode::Char('j') => {
                        // Two rows live past the options: Other, then Chat.
                        question.cursor = (question.cursor + 1).min(option_count + 1);
                    }
                    KeyCode::PageUp => {
                        self.panel_scroll = self.panel_scroll.saturating_sub(10);
                    }
                    KeyCode::PageDown => {
                        self.panel_scroll += 10;
                    }
                    KeyCode::Char(' ') if question.current().multi_select => {
                        if question.cursor < option_count {
                            if let Some(position) = question
                                .selected
                                .iter()
                                .position(|index| *index == question.cursor)
                            {
                                question.selected.remove(position);
                            } else {
                                question.selected.push(question.cursor);
                                question.selected.sort_unstable();
                            }
                        }
                    }
                    KeyCode::Enter if question.cursor == option_count => {
                        question.phase = QuestionPhase::Other;
                        question.editor.clear();
                    }
                    // The last row is Esc made visible: leave the question
                    // unanswered and hand the turn back to plain conversation.
                    KeyCode::Enter if question.cursor > option_count => {
                        outcome = Some(QuestionOutcome::Cancelled);
                    }
                    KeyCode::Enter if question.current().multi_select => {
                        if question.selected.is_empty() {
                            question.selected.push(question.cursor);
                        }
                        outcome = finish_question(question, None, None);
                    }
                    KeyCode::Enter => {
                        question.selected.clear();
                        question.selected.push(question.cursor);
                        if question.selected_preview().is_some() {
                            question.phase = QuestionPhase::Notes;
                            question.editor.clear();
                        } else {
                            outcome = finish_question(question, None, None);
                        }
                    }
                    _ => {}
                }
            }
            QuestionPhase::Other | QuestionPhase::Notes => match key.code {
                KeyCode::Char(c) => question.editor.push(c),
                KeyCode::Backspace => {
                    question.editor.pop();
                }
                KeyCode::Enter if question.phase == QuestionPhase::Other => {
                    let value = question.editor.trim().to_string();
                    if !value.is_empty() {
                        question.pending_other = Some(value.clone());
                        outcome = finish_question(question, Some(value), None);
                    }
                }
                KeyCode::Enter => {
                    let value = question.editor.trim();
                    let notes = (!value.is_empty()).then(|| value.to_string());
                    let other = question.pending_other.take();
                    outcome = finish_question(question, other, notes);
                }
                _ => {}
            },
        }
        if let Some(outcome) = outcome {
            self.resolve_question(outcome);
        }
        Command::None
    }

    fn resolve_question(&mut self, outcome: QuestionOutcome) {
        let Some(PendingInteraction::Question(question)) = self.interactions.pop_front() else {
            return;
        };
        self.panel_scroll = 0;
        let _ = question.reply.send(outcome);
    }
    fn on_provider_key(&mut self, key: KeyEvent) -> Command {
        let remembered_models = self.remembered_models.clone();
        let picker = self.provider_picker.as_mut().expect("checked some");
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::None;
        }
        let selecting_model = picker.model_cursor.is_some();
        // A row number acts as "move there, then Enter": the panel prints those
        // numbers, so they have to answer.
        let key = match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let row = c.to_digit(10).expect("ascii digit") as usize - 1;
                let rows = if selecting_model {
                    picker.providers[picker.provider_cursor].models.len()
                } else {
                    picker.providers.len()
                };
                if row >= rows {
                    return Command::None;
                }
                match picker.model_cursor.as_mut() {
                    Some(cursor) => *cursor = row,
                    None => picker.provider_cursor = row,
                }
                KeyEvent::new(KeyCode::Enter, key.modifiers)
            }
            _ => key,
        };
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(cursor) = picker.model_cursor.as_mut() {
                    *cursor = cursor.saturating_sub(1);
                } else {
                    picker.provider_cursor = picker.provider_cursor.saturating_sub(1);
                }
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(cursor) = picker.model_cursor.as_mut() {
                    let models = &picker.providers[picker.provider_cursor].models;
                    *cursor = (*cursor + 1).min(models.len() - 1);
                } else {
                    picker.provider_cursor =
                        (picker.provider_cursor + 1).min(picker.providers.len() - 1);
                }
            }
            KeyCode::Enter if selecting_model => {
                let provider = &picker.providers[picker.provider_cursor];
                let model = &provider.models[picker.model_cursor.unwrap_or(0)];
                let command = format!("/provider {} {}", provider.id, model);
                self.provider_picker = None;
                return Command::Slash(command);
            }
            KeyCode::Enter => {
                let provider = &picker.providers[picker.provider_cursor];
                let model = remembered_models
                    .get(&provider.id)
                    .filter(|model| provider.models.contains(model))
                    .unwrap_or(&provider.default_model);
                let cursor = provider
                    .models
                    .iter()
                    .position(|candidate| candidate == model)
                    .unwrap_or(0);
                picker.model_cursor = Some(cursor);
            }
            KeyCode::Esc if selecting_model => picker.model_cursor = None,
            KeyCode::Esc => self.provider_picker = None,
            _ => {}
        }
        Command::None
    }

    /// at the selected point, Esc backs out without touching History (Ctrl+C is
    /// the two-tap quit, intercepted before routing here).
    fn on_fork_key(&mut self, key: KeyEvent) -> Command {
        let picker = self.fork_picker.as_mut().expect("checked some");
        // Ctrl-modified keys are inert here (Ctrl+C two-tap quit is intercepted
        // before routing; no Ctrl combo should move the cursor).
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Command::None;
        }
        // Same as everywhere else: the printed row numbers pick directly.
        let key = match key.code {
            KeyCode::Char(c) if c.is_ascii_digit() && c != '0' => {
                let row = c.to_digit(10).expect("ascii digit") as usize - 1;
                if row >= picker.points.len() {
                    return Command::None;
                }
                picker.cursor = row;
                KeyEvent::new(KeyCode::Enter, key.modifiers)
            }
            _ => key,
        };
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

fn finish_question(
    question: &mut PendingQuestion,
    other: Option<String>,
    notes: Option<String>,
) -> Option<QuestionOutcome> {
    question.answers.push(QuestionAnswer {
        question_index: question.question_index,
        selected: std::mem::take(&mut question.selected),
        other,
        notes,
    });
    if question.question_index + 1 < question.req.questions.len() {
        question.question_index += 1;
        question.cursor = 0;
        question.phase = QuestionPhase::Select;
        question.editor.clear();
        question.pending_other = None;
        return None;
    }
    let answers = std::mem::take(&mut question.answers);
    Some(match question.req.validate_answers(&answers) {
        Ok(()) => QuestionOutcome::Answered(answers),
        Err(error) => QuestionOutcome::Unavailable(format!(
            "TUI produced an invalid question answer: {error}"
        )),
    })
}

/// Replay a resumed session's history into transcript cells so `--resume`
/// shows the conversation instead of a blank screen. Tool calls collapse to
/// the same status rows the live path produces: paired result's `is_error`
/// decides ✓/✗, and an unpaired call renders as failed (resume repair marks
/// orphans as interrupted errors anyway). Tool-result blocks themselves are
/// skipped — their content is history-internal.
pub fn cells_from_history(messages: &[Message]) -> Vec<Cell> {
    // Pair each tool_use with its result: the is_error decides ✓/✗ and the
    // (bounded) content becomes the preview under the row, same as the live path.
    let results: HashMap<&str, (bool, String)> = messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                content,
            } => Some((
                tool_use_id.as_str(),
                (*is_error, content.as_text().chars().take(4000).collect()),
            )),
            _ => None,
        })
        .collect();
    let mut cells = Vec::new();
    for message in messages {
        for block in &message.content {
            match (message.role, block) {
                (Role::User, ContentBlock::Text { text }) => {
                    // Not every user-role message is the user talking: the inbox
                    // reinjects sub-agent results, background output and steering
                    // as user text so the model folds them in. Replaying those
                    // verbatim shows a page of machine framing as if it had been
                    // typed — a background sub-agent's summary alone runs to
                    // thousands of characters.
                    match kloop_core::inbox::replayed(text) {
                        Some(Replayed::UserText(typed)) => {
                            cells.push(Cell::User(typed.to_string()))
                        }
                        Some(Replayed::Note(note)) => cells.push(Cell::Note(note)),
                        None => cells.push(Cell::User(text.clone())),
                    }
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
                    // No timing on disk, so a resumed block shows `∗ Thought`
                    // without an elapsed — and a second identical line says
                    // nothing the first did not. A turn often holds several
                    // blocks, so they fold into one row (keeping every block's
                    // text, which the row does not show but a future expander
                    // would).
                    match cells.last_mut() {
                        Some(Cell::Thinking {
                            text,
                            seconds: None,
                        }) => {
                            text.push_str("\n\n");
                            text.push_str(thinking);
                        }
                        _ => cells.push(Cell::Thinking {
                            text: thinking.clone(),
                            seconds: None,
                        }),
                    }
                }
                (Role::Assistant, ContentBlock::ToolUse { id, name, input }) => {
                    let (status, output) = match results.get(id.as_str()) {
                        Some((false, text)) => (ToolStatus::Ok, Some(text.clone())),
                        Some((true, text)) => (ToolStatus::Failed, Some(text.clone())),
                        // Orphaned call (killed session): failed, no result yet.
                        None => (ToolStatus::Failed, None),
                    };
                    cells.push(Cell::Tool {
                        name: name.clone(),
                        input: input.to_string(),
                        status,
                        output,
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

#[cfg(test)]
mod tests {
    use super::*;

    // Constructors for the core events the tests drive through `apply` — since
    // plan 39 the worker wraps every `Ui::emit` in `AgentEvent::Core`.
    fn text_delta(s: &str) -> AgentEvent {
        text_delta_for("m", s)
    }
    fn text_delta_for(id: &str, s: &str) -> AgentEvent {
        AgentEvent::Core(Event::ItemDelta {
            id: id.into(),
            delta: Delta::Text(s.into()),
        })
    }
    fn thinking_delta(s: &str) -> AgentEvent {
        AgentEvent::Core(Event::ItemDelta {
            id: "r".into(),
            delta: Delta::Reasoning(s.into()),
        })
    }
    fn tool_start(agent: &str, id: &str, name: &str, input: &str) -> AgentEvent {
        AgentEvent::Core(Event::ItemStarted {
            id: id.into(),
            item: Item::ToolCall {
                agent: agent.into(),
                name: name.into(),
                input: serde_json::from_str(input).unwrap(),
                status: ItemStatus::InProgress,
                output: None,
            },
        })
    }
    fn tool_end(agent: &str, id: &str, ok: bool, output: &str) -> AgentEvent {
        AgentEvent::Core(Event::ItemCompleted {
            id: id.into(),
            item: Item::ToolCall {
                agent: agent.into(),
                name: String::new(),
                input: serde_json::Value::Null,
                status: if ok {
                    ItemStatus::Completed
                } else {
                    ItemStatus::Failed
                },
                output: (!output.is_empty()).then(|| output.to_string()),
            },
        })
    }
    fn agent_start(agent: &str, task: &str) -> AgentEvent {
        AgentEvent::Core(Event::ItemStarted {
            id: agent.into(),
            item: Item::SubAgent {
                label: agent.into(),
                task: task.into(),
                status: ItemStatus::InProgress,
            },
        })
    }
    fn agent_end(agent: &str, ok: bool) -> AgentEvent {
        AgentEvent::Core(Event::ItemCompleted {
            id: agent.into(),
            item: Item::SubAgent {
                label: agent.into(),
                task: String::new(),
                status: if ok {
                    ItemStatus::Completed
                } else {
                    ItemStatus::Failed
                },
            },
        })
    }
    fn task_snapshot(revision: u64, subject: &str) -> TaskGraphSnapshot {
        TaskGraphSnapshot {
            revision,
            tasks: vec![kloop_core::tools::TaskGraphTask {
                id: "1".into(),
                subject: subject.into(),
                status: kloop_core::tools::TaskStatus::Pending,
                blocked_by: Vec::new(),
                blocks: Vec::new(),
            }],
        }
    }

    fn completed_task_snapshot(revision: u64, subject: &str) -> TaskGraphSnapshot {
        let mut snapshot = task_snapshot(revision, subject);
        snapshot.tasks[0].status = kloop_core::tools::TaskStatus::Completed;
        snapshot
    }

    fn usage(u: u64) -> AgentEvent {
        AgentEvent::Core(Event::Usage(u))
    }

    fn turn_ended(reason: EndReason) -> AgentEvent {
        AgentEvent::Core(Event::TurnEnded(reason))
    }
    fn mode_changed(mode: Mode) -> AgentEvent {
        AgentEvent::Core(Event::ModeChanged(mode))
    }

    fn background_update(
        id: &str,
        kind: kloop_core::event::BackgroundTaskKind,
        run_id: Option<&str>,
        status: kloop_core::event::BackgroundTaskStatus,
        detail: Option<&str>,
        output_path: Option<&str>,
    ) -> AgentEvent {
        AgentEvent::Core(Event::BackgroundTaskUpdated(
            kloop_core::event::BackgroundTask {
                id: id.into(),
                run_id: run_id.map(str::to_string),
                kind,
                description: "inspect logs".into(),
                status,
                output_path: output_path.map(str::to_string),
                detail: detail.map(str::to_string),
            },
        ))
    }

    #[test]
    fn background_updates_upsert_one_session_owned_lifecycle_row() {
        use kloop_core::event::BackgroundTaskKind;
        use kloop_core::event::BackgroundTaskStatus;

        let mut app = App::new("s".into());
        app.apply(background_update(
            "workflow-4",
            BackgroundTaskKind::Workflow,
            Some("wf_4"),
            BackgroundTaskStatus::Running,
            None,
            None,
        ));
        app.apply(background_update(
            "workflow-4",
            BackgroundTaskKind::Workflow,
            Some("wf_4"),
            BackgroundTaskStatus::Running,
            Some("Verify"),
            None,
        ));
        app.apply(background_update(
            "workflow-4",
            BackgroundTaskKind::Workflow,
            Some("wf_4"),
            BackgroundTaskStatus::Completed,
            None,
            Some("/tmp/result.json"),
        ));

        assert_eq!(
            app.cells,
            vec![Cell::BackgroundTask(BackgroundTask {
                id: "workflow-4".into(),
                run_id: Some("wf_4".into()),
                kind: BackgroundTaskKind::Workflow,
                description: "inspect logs".into(),
                status: BackgroundTaskStatus::Completed,
                output_path: Some("/tmp/result.json".into()),
                detail: None,
            })]
        );
        assert!(app.background_task_cells.is_empty());
        assert!(app.frozen_background_tasks.is_empty());
    }

    #[test]
    fn frozen_background_running_updates_are_ignored_and_terminal_is_linked() {
        use kloop_core::event::BackgroundTaskKind;
        use kloop_core::event::BackgroundTaskStatus;

        let mut app = App::new("s".into());
        app.apply(background_update(
            "program-2",
            BackgroundTaskKind::Program,
            Some("run-2"),
            BackgroundTaskStatus::Running,
            None,
            None,
        ));
        app.cells.push(Cell::Assistant("later".into()));
        app.drain_committed(1);
        assert_eq!(app.cells, vec![Cell::Assistant("later".into())]);
        assert!(app.background_task_cells.is_empty());
        assert!(app.frozen_background_tasks.contains("program-2"));

        app.apply(background_update(
            "program-2",
            BackgroundTaskKind::Program,
            Some("run-2"),
            BackgroundTaskStatus::Running,
            Some("still running"),
            None,
        ));
        assert_eq!(app.cells, vec![Cell::Assistant("later".into())]);

        app.apply(background_update(
            "program-2",
            BackgroundTaskKind::Program,
            Some("run-2"),
            BackgroundTaskStatus::Failed,
            Some("exit 1"),
            Some("/tmp/program.err"),
        ));
        assert_eq!(app.cells.len(), 2);
        assert!(matches!(
            &app.cells[1],
            Cell::BackgroundTask(task)
                if task.id == "program-2"
                    && task.run_id.as_deref() == Some("run-2")
                    && task.status == BackgroundTaskStatus::Failed
        ));
        assert!(!app.frozen_background_tasks.contains("program-2"));
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    fn front_confirm_cursor(app: &App) -> usize {
        match app.interactions.front() {
            Some(PendingInteraction::Confirm { cursor, .. }) => *cursor,
            _ => panic!("expected a queued confirmation"),
        }
    }

    fn front_confirm_description(app: &App) -> &str {
        match app.interactions.front() {
            Some(PendingInteraction::Confirm { req, .. }) => &req.description,
            _ => panic!("expected a queued confirmation"),
        }
    }

    fn question_request(multi_select: bool, preview: Option<&str>) -> QuestionRequest {
        QuestionRequest {
            questions: vec![kloop_core::interaction::Question {
                question: "Which option?".into(),
                header: "Choice".into(),
                options: vec![
                    kloop_core::interaction::QuestionOption {
                        label: "A".into(),
                        description: "first".into(),
                        preview: preview.map(str::to_string),
                    },
                    kloop_core::interaction::QuestionOption {
                        label: "B".into(),
                        description: "second".into(),
                        preview: None,
                    },
                ],
                multi_select,
            }],
            metadata: None,
        }
    }

    fn type_str(app: &mut App, s: &str) {
        for c in s.chars() {
            app.on_key(80, key(KeyCode::Char(c)));
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
            app.on_key(80, key(KeyCode::BackTab)),
            Command::SetMode(Mode::AcceptEdits)
        );
        assert_eq!(app.mode, Mode::AcceptEdits);
        assert_eq!(
            app.on_key(80, key(KeyCode::BackTab)),
            Command::SetMode(Mode::Plan)
        );
        assert_eq!(app.mode, Mode::Plan);
        assert_eq!(
            app.on_key(80, key(KeyCode::BackTab)),
            Command::SetMode(Mode::Manual)
        );
        assert_eq!(app.mode, Mode::Manual);

        // The agent side leaving plan mode syncs the badge with no keypress.
        app.mode = Mode::Plan;
        app.apply(mode_changed(Mode::AcceptEdits));
        assert_eq!(app.mode, Mode::AcceptEdits);
    }

    #[test]
    fn task_graph_revisions_replace_atomically_and_ctrl_t_is_display_only() {
        let mut app = App::new("s".into());
        app.apply(text_delta("streaming"));
        let cells = app.cells.clone();
        assert!(app.streaming_assistant());

        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            0, "Seed",
        ))));
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 0);
        assert_eq!(app.cells, cells);
        assert!(app.streaming_assistant());

        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            2, "Newest",
        ))));
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            1, "Stale",
        ))));
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            2,
            "Duplicate",
        ))));
        assert_eq!(app.task_graph.as_ref().unwrap().tasks[0].subject, "Newest");
        assert_eq!(app.cells, cells);
        assert!(app.streaming_assistant());

        assert!(app.show_task_graph);
        assert_eq!(app.on_key(80, ctrl('t')), Command::None);
        assert!(!app.show_task_graph);
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 2);
        assert_eq!(app.on_key(80, ctrl('t')), Command::None);
        assert!(app.show_task_graph);

        app.apply(turn_ended(EndReason::Completed));
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 2);
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(
            TaskGraphSnapshot {
                revision: 3,
                tasks: Vec::new(),
            },
        )));
        assert!(app.task_graph.as_ref().unwrap().tasks.is_empty());
        assert!(app.show_task_graph);
        assert_eq!(app.on_key(80, ctrl('t')), Command::None);
        assert!(app.show_task_graph, "an empty graph has no toggle target");
    }

    #[test]
    fn the_panel_retires_with_the_turn_that_raised_it() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            1, "Open",
        ))));
        assert!(app.live_task_graph().is_some(), "still up mid-turn");
        // An unfinished graph goes too: the model that stopped answering is not
        // going to come back and tick its own boxes.
        app.apply(turn_ended(EndReason::Aborted));
        assert!(app.live_task_graph().is_none());

        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(
            completed_task_snapshot(2, "Open"),
        )));
        assert!(app.live_task_graph().is_some());
        app.apply(turn_ended(EndReason::Completed));
        assert!(app.live_task_graph().is_none());

        // Display-only: the snapshot, its revision fence and the Ctrl+T
        // preference all survive, so a stale snapshot still loses and cannot
        // bring the panel back.
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 2);
        assert!(app.show_task_graph);
        assert_eq!(app.on_key(80, ctrl('t')), Command::None);
        assert!(app.show_task_graph, "a retired panel has no toggle target");
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            1, "Stale",
        ))));
        assert!(app.live_task_graph().is_none());

        // The next accepted snapshot — the next epoch's first task — brings it
        // back with no keypress.
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            3,
            "Next epoch",
        ))));
        assert_eq!(
            app.live_task_graph()
                .map(|graph| graph.tasks[0].subject.as_str()),
            Some("Next epoch")
        );
    }

    #[test]
    fn deltas_accumulate_until_a_tool_row_splits_them() {
        let mut app = App::new("s".into());
        app.apply(text_delta("hel"));
        app.apply(text_delta("lo"));
        app.apply(tool_start("", "t1", "bash", "{}"));
        app.apply(text_delta_for("m2", "world"));
        app.apply(tool_end("", "t1", false, ""));

        assert_eq!(
            app.cells,
            vec![
                Cell::Assistant("hello".into()),
                Cell::Tool {
                    name: "bash".into(),
                    input: "{}".into(),
                    status: ToolStatus::Failed,
                    output: None,
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
        app.apply(agent_start("agent-1", "find the bug"));
        app.apply(agent_start("agent-2", "write the docs"));
        // Interleaved tool activity from both agents plus the main agent.
        app.apply(tool_start("agent-1", "t1", "grep", "{\"pattern\":\"bug\"}"));
        app.apply(tool_start(
            "agent-2",
            "t2",
            "read_file",
            "{\"path\":\"README\"}",
        ));
        app.apply(tool_end("agent-1", "t1", true, ""));
        app.apply(tool_start(
            "agent-1",
            "t3",
            "bash",
            "{\"command\":\"cargo test\"}",
        ));
        app.apply(agent_end("agent-1", true));
        app.apply(agent_end("agent-2", false));

        assert_eq!(
            app.cells,
            vec![
                Cell::Agent {
                    agent: "agent-1".into(),
                    task: "find the bug".into(),
                    status: ToolStatus::Ok,
                    tools: 2,
                    // The folded preview is the human-readable form now.
                    last_tool: "Bash $ cargo test".into(),
                },
                Cell::Agent {
                    agent: "agent-2".into(),
                    task: "write the docs".into(),
                    status: ToolStatus::Failed,
                    tools: 1,
                    last_tool: "Read README".into(),
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
        app.apply(tool_start("", "t1", "bash", "{}"));
        app.cells.push(Cell::Assistant("middle".into()));
        app.apply(tool_start("", "t2", "grep", "{}"));
        assert_eq!(app.cells.len(), 3);

        app.drain_committed(2);
        assert_eq!(app.cells.len(), 1);
        assert_eq!(app.tool_cells.get("t1"), None, "committed cell dropped");
        assert_eq!(app.tool_cells.get("t2"), Some(&0), "survivor shifted down");

        // A late ToolEnd for the now-frozen t1 is a harmless no-op; t2 resolves.
        app.apply(tool_end("", "t1", true, ""));
        app.apply(tool_end("", "t2", true, ""));
        assert_eq!(
            app.cells,
            vec![Cell::Tool {
                name: "grep".into(),
                input: "{}".into(),
                status: ToolStatus::Ok,
                output: None,
            }]
        );
    }

    /// A sub-agent still Running when the turn dies (interrupt drops the task
    /// future before its AgentEnd) is patched to Failed.
    #[test]
    fn turn_end_fails_agents_left_running() {
        let mut app = App::new("s".into());
        app.running = true;
        app.apply(agent_start("agent-1", "long job"));
        app.apply(turn_ended(EndReason::Aborted));
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

    #[test]
    fn turn_end_does_not_fail_session_owned_background_rows() {
        let mut app = App::new("s".into());
        app.running = true;
        app.apply(background_update(
            "agent-9",
            kloop_core::event::BackgroundTaskKind::Agent,
            None,
            BackgroundTaskStatus::Running,
            None,
            None,
        ));
        app.apply(turn_ended(EndReason::Aborted));
        assert!(matches!(
            &app.cells[0],
            Cell::BackgroundTask(task) if task.status == BackgroundTaskStatus::Running
        ));
        assert_eq!(app.background_task_cells.get("agent-9"), Some(&0));
    }

    /// Thinking and answer deltas are routed by item id even when the two
    /// channels interleave; each lifecycle owns one cell.
    #[test]
    fn interleaved_display_deltas_keep_one_cell_per_item() {
        let mut app = App::new("s".into());
        app.apply(thinking_delta("let me"));
        app.apply(thinking_delta(" see"));
        app.apply(text_delta("answer"));
        app.apply(thinking_delta(" more thought"));
        app.apply(text_delta("!"));

        assert_eq!(
            app.cells,
            vec![
                Cell::Thinking {
                    text: "let me see more thought".into(),
                    seconds: None
                },
                Cell::Assistant("answer!".into()),
            ]
        );
    }

    #[test]
    fn interleaved_display_completion_updates_the_original_cells() {
        let mut app = App::new("s".into());
        app.apply(text_delta("a"));
        app.apply(thinking_delta("r"));
        app.apply(text_delta("b"));
        app.apply(AgentEvent::Core(Event::ItemCompleted {
            id: "m".into(),
            item: Item::AssistantMessage {
                text: "ab".into(),
                status: ItemStatus::Completed,
            },
        }));
        app.apply(AgentEvent::Core(Event::ItemCompleted {
            id: "r".into(),
            item: Item::Reasoning {
                text: "r".into(),
                status: ItemStatus::Failed,
            },
        }));

        assert_eq!(
            app.cells,
            vec![
                Cell::Assistant("ab".into()),
                Cell::Thinking {
                    text: "r".into(),
                    seconds: None,
                },
            ]
        );
    }

    #[test]
    fn forward_delete_routes_to_a_whole_grapheme() {
        let mut app = App::new("s".into());
        type_str(&mut app, "a👩🏽‍💻z");
        app.on_key(80, key(KeyCode::Home));
        app.on_key(80, key(KeyCode::Right));
        assert_eq!(app.on_key(80, key(KeyCode::Delete)), Command::None);
        assert_eq!(app.composer.text(), "az");
        app.on_key(80, key(KeyCode::End));
        assert_eq!(app.on_key(80, key(KeyCode::Delete)), Command::None);
        assert_eq!(app.composer.text(), "az");
    }

    #[test]
    fn paste_newlines_are_canonicalized_without_trimming_content() {
        for (input, expected) in [
            ("abc\ndef", "abc\ndef"),
            ("abc\r\ndef", "abc\ndef"),
            ("abc\rdef", "abc\ndef"),
            (" abc\r\ndef \n", " abc\ndef \n"),
        ] {
            assert_eq!(canonicalize_paste_newlines(input), expected);

            let mut app = App::new("s".into());
            assert_eq!(app.paste_text(input), Command::None);
            assert_eq!(app.composer.text(), expected);
            assert_eq!(
                app.on_key(80, key(KeyCode::Enter)),
                Command::Submit(expected.into())
            );
        }
    }

    #[test]
    fn newline_shortcuts_insert_without_submit_then_enter_submits_multiline() {
        let shortcuts = [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new(KeyCode::Char('j'), KeyModifiers::CONTROL),
        ];

        for shortcut in shortcuts {
            let mut app = App::new("s".into());
            type_str(&mut app, "first");
            assert_eq!(app.on_key(80, shortcut), Command::None);
            assert_eq!(app.composer.text(), "first\n");
            assert!(!app.running);
            assert!(app.cells.is_empty());

            type_str(&mut app, "second");
            assert_eq!(
                app.on_key(80, key(KeyCode::Enter)),
                Command::Submit("first\nsecond".into())
            );
            assert_eq!(app.cells, vec![Cell::User("first\nsecond".into())]);
        }
    }

    #[test]
    fn vertical_navigation_uses_the_current_composer_width() {
        let mut app = App::new("s".into());
        type_str(&mut app, "history");
        app.composer.submit();
        type_str(&mut app, "abcdefgh");
        app.on_key(6, key(KeyCode::Up));
        assert_eq!(app.composer.text(), "abcdefgh", "soft row, not history");
        app.on_key(6, key(KeyCode::Up));
        assert_eq!(app.composer.text(), "abcdefgh", "soft row, not history");
        app.on_key(6, key(KeyCode::Up));
        assert_eq!(app.composer.text(), "history");
    }

    #[test]
    fn typing_editing_and_submit() {
        let mut app = App::new("s".into());
        type_str(&mut app, "你好ab");
        app.on_key(80, key(KeyCode::Left));
        app.on_key(80, key(KeyCode::Backspace)); // removes 'a'
        app.on_key(80, key(KeyCode::Home));
        app.on_key(80, key(KeyCode::Right));
        type_str(&mut app, "x");
        assert_eq!(app.composer.text(), "你x好b");

        let cmd = app.on_key(80, key(KeyCode::Enter));
        assert_eq!(cmd, Command::Submit("你x好b".into()));
        assert!(app.running);
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.cells, vec![Cell::User("你x好b".into())]);

        // While running, Enter steers instead of starting a new turn.
        type_str(&mut app, "next");
        assert_eq!(
            app.on_key(80, key(KeyCode::Enter)),
            Command::Steer("next".into())
        );
        assert_eq!(
            app.cells,
            vec![Cell::User("你x好b".into()), Cell::User("next".into())]
        );
    }

    /// Steering (Enter while a turn runs) queues the text as Command::Steer and
    /// shows it as a User cell without ending or restarting the turn.
    #[test]
    fn steering_while_running_queues_without_a_new_turn() {
        let mut app = App::new("s".into());
        app.running = true;
        app.apply(AgentEvent::Core(Event::TaskGraphUpdated(task_snapshot(
            4,
            "Retained while steering",
        ))));
        app.show_task_graph = false;
        type_str(&mut app, "also do X");
        let cmd = app.on_key(80, key(KeyCode::Enter));
        assert_eq!(cmd, Command::Steer("also do X".into()));
        assert!(app.running, "steering does not end or restart the turn");
        assert_eq!(app.composer.text(), "");
        assert_eq!(app.cells, vec![Cell::User("also do X".into())]);
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 4);
        assert_eq!(
            app.task_graph.as_ref().unwrap().tasks[0].subject,
            "Retained while steering"
        );
        assert!(!app.show_task_graph);
    }

    /// Steering with an image attached: only the text steers the running turn;
    /// the image is NOT sent and NOT shown as an `[image: …]` cell, but stays on
    /// the composer so the next fresh turn carries it. Regression — the image
    /// used to be stranded in submit_images (never delivered) while a misleading
    /// placeholder cell claimed it rode the turn.
    #[test]
    fn steering_keeps_the_image_for_the_next_fresh_turn() {
        let block = ContentBlock::Image {
            source: ImageSource::Base64 {
                media_type: "image/png".into(),
                data: "aGk=".into(),
            },
        };
        let mut app = App::new("s".into());
        app.running = true;
        app.attach_image("shot.png".into(), block.clone());
        type_str(&mut app, "keep going");
        assert_eq!(
            app.on_key(80, key(KeyCode::Enter)),
            Command::Steer("keep going".into())
        );
        // No [image:] cell, and the steer carries no image.
        assert_eq!(app.cells, vec![Cell::User("keep going".into())]);
        assert!(app.take_submit_images().is_empty(), "steer sends no image");
        assert_eq!(
            app.composer.attachments()[0].label,
            "shot.png",
            "image kept for a fresh turn"
        );

        // The turn ends; a fresh Enter now delivers the still-attached image.
        app.running = false;
        assert_eq!(
            app.on_key(80, key(KeyCode::Enter)),
            Command::Submit(String::new())
        );
        assert_eq!(
            app.take_submit_images(),
            vec![block],
            "the fresh turn carries the image"
        );
    }

    /// An idle slash line routes to the worker as Command::Slash and marks the
    /// app busy without pushing a User cell. While a turn runs, the same text is
    /// steering — Ctrl+C is the only hard stop.
    #[test]
    fn slash_command_routes_only_when_idle() {
        let mut app = App::new("s".into());
        type_str(&mut app, "/help");
        let cmd = app.on_key(80, key(KeyCode::Enter));
        assert_eq!(cmd, Command::Slash("/help".into()));
        assert!(app.running, "the app shows busy until the worker replies");
        assert_eq!(app.composer.text(), "");
        assert!(app.cells.is_empty(), "a command is not a User message");

        // While running, a '/'-line is just steering text, not a command.
        type_str(&mut app, "/cost");
        assert_eq!(
            app.on_key(80, key(KeyCode::Enter)),
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

        app.apply(AgentEvent::System(
            "model: x\ncontext: ~0 / 100 tokens (0%)".into(),
        ));
        assert_eq!(
            app.cells.last(),
            Some(&Cell::System(
                "model: x\ncontext: ~0 / 100 tokens (0%)".into()
            ))
        );

        app.background_task_cells.insert("agent-8".into(), 0);
        app.frozen_background_tasks.insert("program-8".into());
        app.task_graph = Some(task_snapshot(4, "Keep until fenced"));
        app.show_task_graph = false;
        app.apply(AgentEvent::ClearTranscript);
        assert!(app.cells.is_empty());
        assert!(app.tool_cells.is_empty());
        assert!(app.background_task_cells.is_empty());
        assert!(app.frozen_background_tasks.is_empty());
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 4);
        assert!(!app.show_task_graph);

        // A terminal update arriving after clear has no stale row to mutate, so it
        // starts a fresh linked lifecycle row rather than disappearing.
        app.apply(background_update(
            "program-8",
            kloop_core::event::BackgroundTaskKind::Program,
            Some("run-8"),
            kloop_core::event::BackgroundTaskStatus::Cancelled,
            Some("session shutdown"),
            None,
        ));
        assert!(matches!(
            app.cells.as_slice(),
            [Cell::BackgroundTask(task)]
                if task.id == "program-8"
                    && task.status == BackgroundTaskStatus::Cancelled
        ));
    }

    #[test]
    fn provider_picker_selects_provider_then_model_and_updates_status() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ProviderPicker(vec![
            kloop_protocol::ProviderDescriptor {
                id: "a".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                default_model: "a1".into(),
                models: vec!["a1".into(), "a2".into()],
                fallback_model: None,
                availability: kloop_protocol::ProviderAvailabilityCode::Ready,
            },
        ]));
        assert!(app.provider_picker.is_some());
        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
        assert_eq!(app.on_key(80, key(KeyCode::Down)), Command::None);
        assert_eq!(
            app.on_key(80, key(KeyCode::Enter)),
            Command::Slash("/provider a a2".into())
        );
        assert!(app.provider_picker.is_none());

        app.apply(AgentEvent::ProviderChanged(
            kloop_protocol::ActiveProviderRoute {
                revision: 2,
                provider_id: "a".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                model: "a2".into(),
                continuity: kloop_protocol::ReasoningContinuity::Preserved,
                effort: None,
            },
        ));
        assert_eq!(app.model, "a2");
    }

    #[test]
    fn provider_picker_remembers_successful_model_selection() {
        let mut app = App::new("s".into());
        let provider = kloop_protocol::ProviderDescriptor {
            id: "a".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            default_model: "a1".into(),
            models: vec!["a1".into(), "a2".into()],
            fallback_model: None,
            availability: kloop_protocol::ProviderAvailabilityCode::Ready,
        };
        app.apply(AgentEvent::ProviderChanged(
            kloop_protocol::ActiveProviderRoute {
                revision: 2,
                provider_id: "a".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                model: "a2".into(),
                continuity: kloop_protocol::ReasoningContinuity::Preserved,
                effort: None,
            },
        ));
        app.apply(AgentEvent::ProviderPicker(vec![provider]));

        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
        assert_eq!(app.provider_picker.as_ref().unwrap().model_cursor, Some(1));
    }

    #[test]
    fn selected_route_changes_while_running_without_rewriting_frozen_route() {
        let old = kloop_protocol::ActiveProviderRoute {
            revision: 1,
            provider_id: "old".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: "old-model".into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        };
        let new = kloop_protocol::ActiveProviderRoute {
            revision: 2,
            provider_id: "new".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: "new-model".into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        };
        let mut app = App::new("s".into()).with_route(old.clone());
        app.running = true;
        app.freeze_selected_route();
        app.apply(AgentEvent::ProviderChanged(new.clone()));

        assert_eq!(app.selected_route, Some(new));
        assert_eq!(app.frozen_route, Some(old.clone()));
        assert_eq!(app.display_route(), Some(&old));

        app.apply(turn_ended(EndReason::Completed));
        assert_eq!(app.frozen_route, None);
        assert_eq!(app.display_route(), app.selected_route.as_ref());
    }

    #[test]
    fn frozen_route_event_beats_late_selected_route_change_until_terminal() {
        let old = kloop_protocol::ActiveProviderRoute {
            revision: 3,
            provider_id: "a".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: "a1".into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        };
        let new = kloop_protocol::ActiveProviderRoute {
            revision: 4,
            provider_id: "b".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            model: "b1".into(),
            continuity: kloop_protocol::ReasoningContinuity::Preserved,
            effort: None,
        };
        let mut app = App::new("s".into()).with_route(old.clone());
        app.running = true;
        app.apply(AgentEvent::RouteFrozen(old.clone()));
        app.apply(AgentEvent::ProviderChanged(new.clone()));
        app.apply(usage(123));

        assert_eq!(app.selected_route, Some(new));
        assert_eq!(app.frozen_route, Some(old.clone()));
        assert_eq!(app.display_route(), Some(&old));
        assert_eq!(app.context_used, 123);
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
        assert_eq!(app.on_key(80, ctrl('r')), Command::RequestForkPoints);
        app.running = true;
        assert_eq!(app.on_key(80, ctrl('r')), Command::None);
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
        app.on_key(80, key(KeyCode::Char('x')));
        assert_eq!(app.composer.text(), "");

        app.on_key(80, key(KeyCode::Up));
        assert_eq!(app.fork_picker.as_ref().unwrap().cursor, 0);
        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::Fork(4));
        assert!(app.fork_picker.is_none(), "selecting closes the picker");
    }

    /// The panel prints row numbers, so the numbers pick — here and in every
    /// other list. A number with no row is inert.
    #[test]
    fn fork_picker_rows_answer_to_their_numbers() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "two"), fp(6, "three")]));
        app.on_key(80, key(KeyCode::Char('9')));
        assert!(app.fork_picker.is_some(), "no ninth row to pick");
        assert_eq!(app.on_key(80, key(KeyCode::Char('1'))), Command::Fork(4));
        assert!(app.fork_picker.is_none());
    }

    /// Esc backs out of the picker without forking.
    #[test]
    fn fork_picker_esc_cancels() {
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "two")]));
        assert_eq!(app.on_key(80, key(KeyCode::Esc)), Command::None);
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
        app.background_task_cells.insert("agent-old".into(), 0);
        app.frozen_background_tasks.insert("program-old".into());
        app.task_graph = Some(task_snapshot(7, "Shared registry"));
        app.show_task_graph = false;
        app.apply(AgentEvent::Forked {
            session_id: "new".into(),
            messages: vec![
                Message::user_text("one"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "done".into(),
                }]),
            ],
            route: kloop_protocol::ActiveProviderRoute {
                revision: 1,
                provider_id: "mock".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                model: "mock-model".into(),
                continuity: kloop_protocol::ReasoningContinuity::Preserved,
                effort: None,
            },
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
        assert!(app.background_task_cells.is_empty());
        assert!(app.frozen_background_tasks.is_empty());
        assert_eq!(app.task_graph.as_ref().unwrap().revision, 7);
        assert!(!app.show_task_graph);
    }

    #[test]
    fn empty_input_never_submits() {
        let mut app = App::new("s".into());
        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
        type_str(&mut app, "   ");
        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
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
        assert_eq!(app.on_key(80, ctrl('c')), Command::None);
        assert!(app.ctrl_c_exit_armed);
        assert_eq!(app.composer.text(), "draft");
        // Second Ctrl+C quits.
        assert_eq!(app.on_key(80, ctrl('c')), Command::Quit);

        // Any other key between the taps disarms it.
        app.on_key(80, ctrl('c'));
        app.on_key(80, key(KeyCode::Char('x')));
        assert!(!app.ctrl_c_exit_armed, "a non-Ctrl+C key disarms");
        assert_eq!(
            app.on_key(80, ctrl('c')),
            Command::None,
            "back to the first tap"
        );

        // Works while running too (quit aborts the turn).
        app.on_key(80, key(KeyCode::Char('y'))); // disarm
        app.running = true;
        assert_eq!(app.on_key(80, ctrl('c')), Command::None);
        assert_eq!(app.on_key(80, ctrl('c')), Command::Quit);
        // Ctrl+D is disabled — the only quit path is the two-tap Ctrl+C.
        assert_eq!(app.on_key(80, ctrl('d')), Command::None);
    }

    /// Ctrl+V and Alt+V request an OS-clipboard image paste (the loop performs
    /// the read); the reserved terminal Cmd+V is unaffected.
    #[test]
    fn ctrl_or_alt_v_requests_clipboard_image() {
        let mut app = App::new("s".into());
        assert_eq!(app.on_key(80, ctrl('v')), Command::PasteClipboardImage);
        let alt_v = KeyEvent::new(KeyCode::Char('v'), KeyModifiers::ALT);
        assert_eq!(app.on_key(80, alt_v), Command::PasteClipboardImage);
        // A plain 'v' just types.
        app.on_key(80, key(KeyCode::Char('v')));
        assert_eq!(app.composer.text(), "v");
    }

    /// Esc interrupts a running turn (the advertised key) and clears the input
    /// line when idle.
    #[test]
    fn esc_interrupts_running_and_clears_input_idle() {
        let mut app = App::new("s".into());
        type_str(&mut app, "draft");
        // Idle: Esc clears the line.
        assert_eq!(app.on_key(80, key(KeyCode::Esc)), Command::None);
        assert_eq!(app.composer.text(), "");
        // Running: Esc interrupts.
        app.running = true;
        assert_eq!(app.on_key(80, key(KeyCode::Esc)), Command::Interrupt);
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
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                remember_rules: None,
                preview: None,
                ..Default::default()
            },
            reply,
        });
        assert_eq!(app.on_key(80, ctrl('c')), Command::None, "first tap arms");
        assert!(app.ctrl_c_exit_armed);
        assert!(
            !app.interactions.is_empty(),
            "the prompt is untouched by the tap"
        );
        assert_eq!(app.on_key(80, ctrl('c')), Command::Quit, "second tap quits");

        // Rewind picker up.
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "one")]));
        assert_eq!(app.on_key(80, ctrl('c')), Command::None, "first tap arms");
        assert!(
            app.fork_picker.is_some(),
            "the picker is untouched by the tap"
        );
        assert_eq!(app.on_key(80, ctrl('c')), Command::Quit, "second tap quits");

        // Ctrl+D is disabled in popups too (inert Ctrl combo).
        let mut app = App::new("s".into());
        app.apply(AgentEvent::ForkPoints(vec![fp(4, "one")]));
        assert_eq!(app.on_key(80, ctrl('d')), Command::None);
    }

    #[tokio::test]
    async fn confirm_prompt_captures_keys_and_replies() {
        let mut app = App::new("s".into());
        app.running = true;
        let (reply, mut rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "bash: git push".into(),
                approval_scopes: vec![
                    kloop_core::permissions::ApprovalScope::Once,
                    kloop_core::permissions::ApprovalScope::WorkspaceSession,
                    kloop_core::permissions::ApprovalScope::Project,
                ],
                remember_rules: Some(vec!["bash(git push *)".into()]),
                preview: None,
                ..Default::default()
            },
            reply,
        });

        // Normal typing is captured by the prompt, not the input line.
        app.on_key(80, key(KeyCode::Char('x')));
        assert_eq!(app.composer.text(), "");
        assert!(rx.try_recv().is_err());

        app.on_key(80, key(KeyCode::Char('a')));
        assert_eq!(
            rx.try_recv().unwrap(),
            Decision::Allow(kloop_core::permissions::ApprovalScope::WorkspaceSession)
        );
        assert!(app.interactions.is_empty());
    }

    /// Arrows walk the option rows; paging scrolls the body. Answering one
    /// prompt leaves the next queued one on its own default, unscrolled.
    #[tokio::test]
    async fn panel_keys_move_the_cursor_page_scrolls_and_both_reset_on_advance() {
        let mut app = App::new("s".into());
        let (r1, _rx1) = oneshot::channel();
        let (r2, _rx2) = oneshot::channel();
        let req = |d: &str| ConfirmRequest {
            description: d.into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
            ..Default::default()
        };
        app.apply(AgentEvent::Confirm {
            req: req("first"),
            reply: r1,
        });
        app.apply(AgentEvent::Confirm {
            req: req("second"),
            reply: r2,
        });

        // Arrows (and j/k) walk the option rows — Yes, then the deny row.
        app.on_key(80, key(KeyCode::Down));
        assert_eq!(front_confirm_cursor(&app), 1);
        app.on_key(80, key(KeyCode::Char('j')));
        assert_eq!(front_confirm_cursor(&app), 1, "the last row is the floor");
        app.on_key(80, key(KeyCode::Char('k')));
        assert_eq!(front_confirm_cursor(&app), 0);
        assert_eq!(app.panel_scroll, 0, "moving the cursor never scrolls");

        // Paging scrolls the body (a tall diff or plan) instead.
        app.on_key(80, key(KeyCode::PageDown));
        assert_eq!(app.panel_scroll, 10);
        app.on_key(80, key(KeyCode::PageUp));
        assert_eq!(app.panel_scroll, 0);
        // Below zero saturates rather than wrapping.
        app.on_key(80, key(KeyCode::PageUp));
        assert_eq!(app.panel_scroll, 0);
        // None of that reached the input line.
        assert_eq!(app.composer.text(), "");

        // Move and scroll, then answer: the queued prompt starts fresh.
        app.on_key(80, key(KeyCode::PageDown));
        app.on_key(80, key(KeyCode::Down));
        app.on_key(80, key(KeyCode::Char('y')));
        assert_eq!(front_confirm_description(&app), "second");
        assert_eq!(app.panel_scroll, 0, "the next prompt is unscrolled");
        assert_eq!(
            front_confirm_cursor(&app),
            0,
            "and opens on its own safe default"
        );
    }

    /// The panel's row numbers, Enter on the cursor row, and Esc all answer;
    /// a number past the last row is inert rather than answering something else.
    #[tokio::test]
    async fn approval_answers_by_number_enter_and_esc() {
        let scopes = vec![
            kloop_core::permissions::ApprovalScope::Once,
            kloop_core::permissions::ApprovalScope::WorkspaceSession,
            kloop_core::permissions::ApprovalScope::Project,
        ];
        let confirm = |app: &mut App, scopes: &[kloop_core::permissions::ApprovalScope]| {
            let (reply, rx) = oneshot::channel();
            app.apply(AgentEvent::Confirm {
                req: ConfirmRequest {
                    description: "bash: git push".into(),
                    approval_scopes: scopes.to_vec(),
                    remember_rules: Some(vec!["bash(git push *)".into()]),
                    ..Default::default()
                },
                reply,
            });
            rx
        };

        // Four rows: three scopes, then deny. "3" is the project row.
        let mut app = App::new("s".into());
        let rx = confirm(&mut app, &scopes);
        app.on_key(80, key(KeyCode::Char('3')));
        assert_eq!(
            rx.await,
            Ok(Decision::Allow(
                kloop_core::permissions::ApprovalScope::Project
            ))
        );

        // Enter answers whatever the cursor is on.
        let rx = confirm(&mut app, &scopes);
        app.on_key(80, key(KeyCode::Down));
        app.on_key(80, key(KeyCode::Enter));
        assert_eq!(
            rx.await,
            Ok(Decision::Allow(
                kloop_core::permissions::ApprovalScope::WorkspaceSession
            ))
        );

        // Esc is the deny row, wherever the cursor happens to be.
        let rx = confirm(&mut app, &scopes);
        app.on_key(80, key(KeyCode::Down));
        app.on_key(80, key(KeyCode::Esc));
        assert_eq!(rx.await, Ok(Decision::Deny));

        // A number with no row is inert — the prompt is still waiting.
        let rx = confirm(&mut app, &[kloop_core::permissions::ApprovalScope::Once]);
        app.on_key(80, key(KeyCode::Char('9')));
        assert_eq!(front_confirm_description(&app), "bash: git push");
        app.on_key(80, key(KeyCode::Char('2')));
        assert_eq!(rx.await, Ok(Decision::Deny), "row 2 of two is deny");
    }

    #[tokio::test]
    async fn queued_confirms_answer_in_order() {
        let mut app = App::new("s".into());
        let (r1, mut rx1) = oneshot::channel();
        let (r2, mut rx2) = oneshot::channel();
        let req = |d: &str| ConfirmRequest {
            description: d.into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
            ..Default::default()
        };
        app.apply(AgentEvent::Confirm {
            req: req("first"),
            reply: r1,
        });
        app.apply(AgentEvent::Confirm {
            req: req("second"),
            reply: r2,
        });

        app.on_key(80, key(KeyCode::Char('a')));
        assert!(rx1.try_recv().is_err());
        assert_eq!(front_confirm_description(&app), "first");
        app.on_key(80, key(KeyCode::Char('y')));
        assert_eq!(
            rx1.try_recv().unwrap(),
            Decision::Allow(kloop_core::permissions::ApprovalScope::Once)
        );
        assert_eq!(front_confirm_description(&app), "second");
        app.on_key(80, key(KeyCode::Char('n')));
        assert_eq!(rx2.try_recv().unwrap(), Decision::Deny);
    }

    #[tokio::test]
    async fn question_single_preview_notes_and_cancel_round_trip() {
        let mut app = App::new("s".into());
        let (reply, mut rx) = oneshot::channel();
        app.apply(AgentEvent::Question {
            req: question_request(false, Some("preview A")),
            reply,
        });

        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
        let Some(PendingInteraction::Question(question)) = app.interactions.front() else {
            panic!("expected question interaction");
        };
        assert_eq!(question.phase, QuestionPhase::Notes);
        assert_eq!(question.selected_preview(), Some("preview A"));
        app.paste_text("ship\r\nit");
        app.on_key(80, key(KeyCode::Enter));
        assert_eq!(
            rx.try_recv().unwrap(),
            QuestionOutcome::Answered(vec![QuestionAnswer {
                question_index: 0,
                selected: vec![0],
                other: None,
                notes: Some("ship\nit".into()),
            }])
        );
        assert!(app.interactions.is_empty());

        let (reply, mut rx) = oneshot::channel();
        app.apply(AgentEvent::Question {
            req: question_request(false, None),
            reply,
        });
        app.on_key(80, key(KeyCode::Esc));
        assert_eq!(rx.try_recv().unwrap(), QuestionOutcome::Cancelled);
    }

    #[tokio::test]
    async fn question_multi_other_and_mixed_interactions_share_fifo() {
        let mut app = App::new("s".into());
        let (confirm_reply, mut confirm_rx) = oneshot::channel();
        let (question_reply, mut question_rx) = oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "first approval".into(),
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                remember_rules: None,
                preview: None,
                ..Default::default()
            },
            reply: confirm_reply,
        });
        app.apply(AgentEvent::Question {
            req: question_request(true, None),
            reply: question_reply,
        });

        app.on_key(80, key(KeyCode::Char('y')));
        assert_eq!(
            confirm_rx.try_recv().unwrap(),
            Decision::Allow(kloop_core::permissions::ApprovalScope::Once)
        );
        assert!(matches!(
            app.interactions.front(),
            Some(PendingInteraction::Question(_))
        ));

        // Toggle A, move to Other, enter free text, then submit both.
        app.on_key(80, key(KeyCode::Char(' ')));
        app.on_key(80, key(KeyCode::Down));
        app.on_key(80, key(KeyCode::Down));
        app.on_key(80, key(KeyCode::Enter));
        assert!(app.question_editor_active());
        app.paste_text("custom");
        assert_eq!(app.composer.text(), "", "paste stays in the modal editor");
        app.on_key(80, key(KeyCode::Enter));
        assert_eq!(
            question_rx.try_recv().unwrap(),
            QuestionOutcome::Answered(vec![QuestionAnswer {
                question_index: 0,
                selected: vec![0],
                other: Some("custom".into()),
                notes: None,
            }])
        );
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
                Cell::Thinking {
                    text: "planning".into(),
                    seconds: None
                },
                Cell::Assistant("on it".into()),
                Cell::Tool {
                    name: "bash".into(),
                    input: r#"{"command":"ls"}"#.into(),
                    status: ToolStatus::Ok,
                    output: Some("big output not shown".into()),
                },
                Cell::Tool {
                    name: "write_file".into(),
                    input: r#"{"path":"x"}"#.into(),
                    status: ToolStatus::Failed,
                    output: Some("declined".into()),
                },
                Cell::Assistant("done".into()),
                Cell::Tool {
                    name: "bash".into(),
                    input: r#"{"command":"true"}"#.into(),
                    status: ToolStatus::Failed,
                    output: None,
                },
                Cell::Note("resumed session — 4 message(s)".into()),
            ],
            "a resumed tool call pairs back its status and result preview; an orphan is failed with no output"
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

    /// A replayed transcript shows the conversation, not the plumbing: an inbox
    /// reinjection collapses to one note (the sub-agent summary it carries runs
    /// to thousands of characters and was never addressed to the reader), and a
    /// run of untimed thinking blocks folds into the single `∗ Thought` row that
    /// says everything each of them would.
    #[test]
    fn replay_collapses_injections_and_thinking_runs() {
        let reinjected = kloop_core::inbox::InboxItem::SubAgentResult {
            label: "agent-1".into(),
            summary: "P1: the silent path skips validation".into(),
        }
        .into_message();
        let steered =
            kloop_core::inbox::InboxItem::Steer("also check the poller".into()).into_message();
        let messages = vec![
            Message::user_text("review these commits"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "first".into(),
                    signature: String::new(),
                },
                ContentBlock::Thinking {
                    thinking: "second".into(),
                    signature: String::new(),
                },
            ]),
            Message::user_text(&reinjected),
            Message::user_text(&steered),
            Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]),
        ];
        assert_eq!(
            cells_from_history(&messages),
            vec![
                Cell::User("review these commits".into()),
                Cell::Thinking {
                    text: "first\n\nsecond".into(),
                    seconds: None,
                },
                Cell::Note("sub-agent result · [Agent agent-1]".into()),
                Cell::User("also check the poller".into()),
                Cell::Assistant("done".into()),
                Cell::Note("resumed session — 5 message(s)".into()),
            ]
        );
    }

    #[test]
    fn agent_message_lifecycle_is_separate_and_upserts_by_message_id() {
        let mut app = App::new("s".into());
        let update = |status| {
            Event::AgentMessageUpdated(AgentMessageUpdate {
                id: "message-8".parse().unwrap(),
                from: "agent-2".parse().unwrap(),
                to: "main".parse().unwrap(),
                summary: "inspect close".into(),
                status,
            })
        };
        app.apply_core(update(AgentMessageStatus::Queued));
        assert_eq!(app.cells.len(), 1);
        assert_eq!(app.agent_message_cells.get("message-8"), Some(&0));
        assert!(app.background_task_cells.is_empty());

        app.apply_core(update(AgentMessageStatus::Delivered));
        assert_eq!(app.cells.len(), 1);
        assert!(matches!(
            &app.cells[0],
            Cell::AgentMessage(message) if message.status == AgentMessageStatus::Delivered
        ));
        assert!(app.agent_message_cells.is_empty());
        assert!(app.background_task_cells.is_empty());
    }

    #[test]
    fn frozen_agent_message_gets_one_linked_terminal_row() {
        let mut app = App::new("s".into());
        let message = |status| {
            Event::AgentMessageUpdated(AgentMessageUpdate {
                id: "message-9".parse().unwrap(),
                from: "agent-3".parse().unwrap(),
                to: "main".parse().unwrap(),
                summary: "check ordering".into(),
                status,
            })
        };
        app.apply_core(message(AgentMessageStatus::Queued));
        app.drain_committed(1);
        assert!(app.cells.is_empty());
        assert!(app.frozen_agent_messages.contains("message-9"));
        app.apply_core(message(AgentMessageStatus::Undeliverable));
        assert_eq!(app.cells.len(), 1);
        assert!(!app.frozen_agent_messages.contains("message-9"));
    }

    #[tokio::test]
    async fn turn_end_clears_running_state_and_notes_failures() {
        let mut app = App::new("s".into());
        app.running = true;
        let (reply, mut rx) = oneshot::channel::<Decision>();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "x".into(),
                approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
                remember_rules: None,
                preview: None,
                ..Default::default()
            },
            reply,
        });
        app.apply(turn_ended(EndReason::Aborted));

        assert!(!app.running);
        assert!(app.interactions.is_empty());
        // The dropped sender resolves the agent-side future as Deny.
        assert!(rx.try_recv().is_err());
        assert_eq!(app.cells, vec![Cell::Note("interrupted".into())]);

        app.apply(turn_ended(EndReason::Error("boom".into())));
        assert_eq!(app.cells[1], Cell::Note("error: boom".into()));
        app.apply(turn_ended(EndReason::Completed));
        assert_eq!(app.cells.len(), 2, "completed turns add no note");
    }

    // --- completion popups (plan 38 slice 4) ---------------------------------

    fn app_with_commands() -> App {
        let commands = ["help", "cost", "compact", "clear", "exit"]
            .iter()
            .map(|n| menu::CommandInfo {
                name: n.to_string(),
                description: format!("the {n} command"),
            })
            .collect();
        App::new("s".into()).with_commands(commands)
    }

    /// Typing `/` then a prefix opens a filtered slash menu; Enter completes the
    /// highlighted command into the composer (with a trailing space) and closes
    /// the menu without submitting.
    #[test]
    fn slash_menu_opens_filters_and_enter_completes_without_submitting() {
        let mut app = app_with_commands();
        type_str(&mut app, "/co");
        let popup = app.popup.as_ref().expect("slash menu open");
        assert_eq!(popup.target.kind, menu::PopupKind::Slash);
        let labels: Vec<&str> = popup.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["/cost", "/compact"]);

        // Down highlights the second, and is captured (no submit, no history).
        assert_eq!(app.on_key(80, key(KeyCode::Down)), Command::None);
        assert_eq!(app.popup.as_ref().unwrap().cursor, 1);

        // Enter completes it into the composer and closes the menu — it does not
        // start a turn (that is a second Enter).
        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
        assert!(app.popup.is_none(), "menu closed after accept");
        assert_eq!(app.composer.text(), "/compact ");
        assert!(!app.running, "accept did not submit");
    }

    /// Shift+Enter is a composer edit even while completion is open; it must not
    /// accept the highlighted entry. Bare Enter remains the completion key.
    #[test]
    fn shift_enter_in_an_open_menu_inserts_newline_without_accepting() {
        let mut app = app_with_commands();
        type_str(&mut app, "/co");
        assert!(app.popup.is_some());

        assert_eq!(
            app.on_key(80, KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT),),
            Command::None
        );
        assert_eq!(app.composer.text(), "/co\n");
        assert!(app.popup.is_none(), "newline ends the completion token");
        assert!(!app.running);
    }

    /// Esc closes an open menu but leaves the composer text alone (a running
    /// interrupt / idle clear is the next Esc, once the menu is gone).
    #[test]
    fn esc_closes_the_menu_without_clearing_the_composer() {
        let mut app = app_with_commands();
        type_str(&mut app, "/he");
        assert!(app.popup.is_some());
        assert_eq!(app.on_key(80, key(KeyCode::Esc)), Command::None);
        assert!(app.popup.is_none());
        assert_eq!(app.composer.text(), "/he", "composer untouched");
    }

    /// An unknown `/name` shows no menu, so Enter still runs it (and gets the
    /// unknown-command reply from the worker).
    #[test]
    fn no_menu_for_an_unmatched_slash_prefix() {
        let mut app = app_with_commands();
        type_str(&mut app, "/zzz");
        assert!(app.popup.is_none());
        assert_eq!(
            app.on_key(80, key(KeyCode::Enter)),
            Command::Slash("/zzz".into())
        );
    }

    /// The slash menu is suppressed while a turn runs — a `/` line is steering.
    #[test]
    fn slash_menu_suppressed_while_running() {
        let mut app = app_with_commands();
        app.running = true;
        type_str(&mut app, "/co");
        assert!(app.popup.is_none());
    }

    /// Typing an `@`-token asks the loop to search; feeding results opens a file
    /// menu, and Enter completes the chosen path (prefix included).
    #[test]
    fn at_token_requests_search_then_completes_a_file() {
        let mut app = app_with_commands();
        // Each keystroke of the token re-issues the search with the new target.
        type_str(&mut app, "see @sr");
        let target = match app.on_key(80, key(KeyCode::Char('c'))) {
            Command::SearchFiles(target) => {
                assert_eq!(target.query, "src");
                target
            }
            other => panic!("expected file search, got {other:?}"),
        };

        app.set_file_results(target, vec!["src/main.rs".into(), "src/lib.rs".into()]);
        let popup = app.popup.as_ref().expect("file menu open");
        assert_eq!(popup.target.kind, menu::PopupKind::File);
        assert_eq!(popup.items[0].label, "src/main.rs");

        assert_eq!(app.on_key(80, key(KeyCode::Enter)), Command::None);
        assert!(app.popup.is_none());
        assert_eq!(app.composer.text(), "see @src/main.rs ");
    }

    #[test]
    fn bracketed_paste_at_token_requests_file_search() {
        let mut app = app_with_commands();
        let target = match app.paste_text("see @src") {
            Command::SearchFiles(target) => target,
            other => panic!("expected file search, got {other:?}"),
        };
        assert_eq!(target.kind, menu::PopupKind::File);
        assert_eq!(target.query, "src");
        assert_eq!(target.range.start().get(), "see ".len());
        assert_eq!(target.range.end().get(), "see @src".len());
    }

    /// A late result is matched by the whole target, not only its query, and an
    /// empty result closes the menu.
    #[test]
    fn file_results_ignore_stale_targets_and_close_on_empty() {
        let mut app = app_with_commands();
        type_str(&mut app, "@ab");
        let first = menu::detect_trigger(app.composer.text(), app.composer.cursor(), true).unwrap();

        app.composer.clear();
        type_str(&mut app, "see @ab");
        let second =
            menu::detect_trigger(app.composer.text(), app.composer.cursor(), true).unwrap();
        assert_eq!(first.query, second.query);
        assert_ne!(first.range, second.range);

        app.set_file_results(first, vec!["stale.rs".into()]);
        assert!(app.popup.is_none(), "same-query stale range ignored");
        app.set_file_results(second.clone(), vec![]);
        assert!(app.popup.is_none());
        app.set_file_results(second, vec!["abc.rs".into()]);
        assert!(app.popup.is_some());
    }

    /// While the menu is open, Up/Down drive the cursor instead of the composer's
    /// history, and a printable key still edits the composer and refilters.
    #[test]
    fn menu_captures_navigation_but_typing_still_edits_and_refilters() {
        let mut app = app_with_commands();
        type_str(&mut app, "/c");
        assert_eq!(app.popup.as_ref().unwrap().items.len(), 3); // cost, compact, clear
        // A printable key falls through: edits the composer, refilters the menu.
        type_str(&mut app, "o");
        assert_eq!(app.composer.text(), "/co");
        let labels: Vec<&str> = app
            .popup
            .as_ref()
            .unwrap()
            .items
            .iter()
            .map(|i| i.label.as_str())
            .collect();
        assert_eq!(labels, vec!["/cost", "/compact"]);
    }

    // --- HUD state (plan 38 slice 5) -----------------------------------------

    /// A Usage event refreshes the footer's context estimate; the window/model
    /// stay put (seeded once).
    #[test]
    fn usage_event_updates_context_estimate() {
        let mut app = App::new("s".into()).with_context("m".into(), Some(1000), 100);
        assert_eq!(app.context_used, 100);
        app.apply(usage(250));
        assert_eq!(app.context_used, 250);
        assert_eq!(app.context_window, Some(1000));
        assert_eq!(app.model, "m");
    }

    /// A live thinking block is the streaming last cell; sealing it stamps the
    /// elapsed into that cell so the frozen render shows the final time.
    #[test]
    fn thinking_streams_then_seals_with_elapsed() {
        let mut app = App::new("s".into());
        app.apply(thinking_delta("pon"));
        app.apply(thinking_delta("dering"));
        assert!(
            app.streaming_thinking(),
            "the open block is the streaming last cell"
        );

        // A text delta closes the block; the loop seals it with its elapsed.
        app.apply(text_delta("answer"));
        assert!(!app.streaming_thinking());
        app.seal_thinking(9);
        assert_eq!(
            app.cells[0],
            Cell::Thinking {
                text: "pondering".into(),
                seconds: Some(9),
            }
        );
    }

    #[test]
    fn cwd_event_updates_current_projection_and_note() {
        let mut app =
            App::new("s".into()).with_working_directory("/repo".into(), Some("main".into()));
        app.apply(AgentEvent::Core(Event::CwdChanged {
            cwd: "/repo/.kloop/worktrees/feature".into(),
            branch: Some("kloop-worktree-feature".into()),
        }));
        assert_eq!(app.cwd, "/repo/.kloop/worktrees/feature");
        assert_eq!(app.branch.as_deref(), Some("kloop-worktree-feature"));
        assert_eq!(
            app.last_note.as_deref(),
            Some(
                "working directory → /repo/.kloop/worktrees/feature (branch kloop-worktree-feature)"
            )
        );

        app.apply(AgentEvent::Core(Event::CwdChanged {
            cwd: "/repo".into(),
            branch: None,
        }));
        assert_eq!(app.cwd, "/repo");
        assert_eq!(app.branch, None);
    }
}
