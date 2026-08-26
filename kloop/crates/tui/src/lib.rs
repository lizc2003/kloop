//! ratatui terminal UI for kloop: an inline viewport (plan 38 slice 0) whose
//! finalized cells scroll into the terminal's native scrollback, a multi-line
//! composer, per-tool-call status rows, and a centered permission popup.
//!
//! Split of responsibilities: the agent runs on its own tokio task and only
//! talks through channels ([`events::ChannelUi`] implements both `Ui` and
//! `Approver`); [`app::App`] folds agent + key events into pure state; and
//! [`render`] turns that state into lines. Only this module touches the
//! terminal — it owns the inline viewport, freezes overflowing cells into
//! scrollback with `insert_before`, and reads keys on a dedicated poll thread.

mod anim;
mod app;
mod clipboard;
mod composer;
mod events;
mod markdown;
mod menu;
mod render;
mod text_layout;
mod toolrow;

use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use anyhow::Result;
use crossterm::event::Event;
use crossterm::event::KeyEventKind;
#[cfg(unix)]
use crossterm::event::KeyboardEnhancementFlags;
#[cfg(unix)]
use crossterm::event::PopKeyboardEnhancementFlags;
#[cfg(unix)]
use crossterm::event::PushKeyboardEnhancementFlags;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent::run_turn;
// Aliased: `Event` alone is crossterm's terminal event in this module.
use kloop_core::Config;
use kloop_core::event::Event as CoreEvent;
use kloop_core::history::History;
use kloop_core::inbox::Inbox;
use kloop_core::inbox::InboxItem;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::Approver;
use kloop_core::rollout::fork_points;
use kloop_core::rollout::fork_session;
use kloop_core::rollout::inspect_session;
use kloop_core::rollout::session_id_of;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

use crate::app::App;
use crate::app::Cell;
use crate::app::Command;
use crate::events::AgentEvent;
use crate::events::ChannelUi;

/// Out-of-band notification sink (e.g. "saved rule to config.toml"); the
/// CLI's plain mode prints these to stderr, the TUI routes them into the
/// transcript as notes.
pub type NoteFn = Arc<dyn Fn(&str) + Send + Sync>;

struct Turn {
    text: String,
    /// Images the composer attached to this turn (plan 38 slice 3); merged with
    /// any `--image` blocks that still ride the first turn.
    images: Vec<ContentBlock>,
    cancel: CancellationToken,
}

/// What the UI loop hands the worker (which owns History). A slash command is
/// routed here rather than run in the loop precisely because it reads or
/// rewrites History, exactly like a turn does.
enum WorkerMsg {
    Turn(Turn),
    Command {
        line: String,
        cancel: CancellationToken,
    },
    /// Autowake (plan 26): a background sub-agent finished while the agent was
    /// idle. Run a turn with NO new user text — `run_turn` drains the reinjected
    /// result at its round-0 boundary and responds — so the result reaches the
    /// model without the user having to type. The worker no-ops if the inbox
    /// was already drained by a race.
    Wake {
        cancel: CancellationToken,
    },
    /// Read the session's rewind targets off disk (plan 18) and reply with a
    /// `ForkPoints` event. Runs on the worker because it owns the rollout path;
    /// idle-only, so no turn is in flight racing the read.
    ListForkPoints,
    /// Rewind History onto the fork cut at `seq`: fork the session file, swap
    /// History to the branch, and reply with a `Forked` event carrying its
    /// messages and id.
    Fork {
        seq: u64,
    },
}

/// Run the TUI until the user quits. `make_config` is called once with the
/// TUI's approver (the y/a/p/n popup) and note sink, so the caller can wire
/// them into `Permissions` without this crate knowing about rule loading.
pub async fn run(
    make_config: impl FnOnce(Arc<dyn Approver>, Arc<dyn Questioner>, NoteFn) -> Result<Config>,
    history: History,
    session_id: String,
    pending_images: Vec<ContentBlock>,
    worktree: Option<String>,
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let channel_ui = Arc::new(ChannelUi::new(event_tx.clone()));
    let note_ui = channel_ui.clone();
    let cfg = Arc::new(make_config(
        channel_ui.clone(),
        channel_ui.clone(),
        Arc::new(move |s: &str| note_ui.emit(&CoreEvent::Note(s.to_string()))),
    )?);
    // `--worktree`: enter an isolated tree for the whole session before any
    // turn runs (plan 35 slice 2). Fail-closed — the user asked for isolation,
    // so a creation error aborts rather than silently working in the main tree.
    // The note buffers in the event channel and shows once the UI loop starts.
    if let Some(name) = &worktree {
        match kloop_core::worktree::enter(&cfg, name).await {
            Ok(msg) => channel_ui.emit(&CoreEvent::Note(msg)),
            Err(e) => return Err(e),
        }
    }
    // A shared handle to tear the worktree down after the session (the worker
    // moves `cfg`, but both point at the same active-worktree slot).
    let cfg_shutdown = cfg.clone();
    // Shared with the worker's Config: the UI loop enqueues steering here while
    // a turn runs, the agent loop drains it at round boundaries (plan 22).
    let inbox = cfg.inbox.clone();
    // The permission gate is shared (Arc) with the worker's Config, so the loop
    // can apply shift+Tab mode changes to it and seed the status-bar badge from
    // the real starting mode (--permission-mode). effective_permissions covers
    // a session started in a worktree, whose gate shares the mode cell anyway.
    let workspace = cfg.effective_workspace();
    let permissions = Arc::clone(&workspace.permissions);

    // Build the UI state before the worker takes History: a resumed session
    // replays into the transcript, and the `/` menu is seeded with the command
    // catalog (built-ins then loaded skills/commands — the same set
    // `commands::run` dispatches, plan 38 slice 4). The `@` menu searches from
    // the project cwd.
    let effective_cwd = workspace.cwd.clone();
    let effective_branch = git_branch(&effective_cwd);
    let mut app = App::new(session_id)
        .with_commands(slash_catalog(&cfg))
        .with_working_directory(display_cwd(&effective_cwd), effective_branch.clone())
        .with_context(
            cfg.provider_route.primary_model().to_string(),
            cfg.context_window,
            history.estimated_tokens(),
        )
        .with_route(cfg.provider_route.public_route());
    app.cells = app::cells_from_history(history.messages());
    // The opening session banner (plan 38 slice 6): the first cell, so it leads
    // the transcript and scrolls into scrollback. Built here with the git/env
    // reads done, keeping the renderer pure. On resume it still opens the replay.
    app.cells.insert(
        0,
        Cell::SessionHeader {
            model: cfg.provider_route.primary_model().to_string(),
            cwd: display_cwd(&effective_cwd),
            branch: effective_branch,
            mode: permissions.mode().label().to_string(),
        },
    );
    // Seed through the same event seam used by live mutations. Keeping revision 0
    // as a real first snapshot lets App distinguish an empty graph from a seed
    // that has not arrived yet.
    channel_ui.emit(&CoreEvent::TaskGraphUpdated(cfg.tasks.snapshot()));
    let cwd = effective_cwd;

    let shutdown_ui: Arc<dyn Ui> = channel_ui.clone();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();

    // Setup can fail AFTER raw mode is enabled (e.g. the inline viewport's CPR
    // probe times out on a PTY that never answers, or an intermediate write
    // fails). A bare `?` here would return without restoring the terminal or
    // tearing down a `--worktree` tree — leaving the shell in raw mode and
    // leaking the worktree on disk (there is no Drop-based cleanup). So on
    // failure run the same teardown the normal exit does, then propagate.
    let mut terminal = match setup_terminal() {
        Ok(t) => t,
        Err(e) => {
            let remaining = cfg_shutdown.shutdown_background_work(&shutdown_ui).await;
            if remaining > 0 {
                eprintln!("warning: {remaining} background task(s) missed the shutdown deadline");
            }
            if let Some(note) = kloop_core::worktree::finish_active(&cfg_shutdown).await {
                eprintln!("{}", note.trim());
            }
            return Err(e);
        }
    };

    // A panic in the worker task is otherwise absorbed by JoinHandle and leaves
    // the UI reading/drawing against a terminal whose owner is still alive. Ask
    // the single UI owner to stop; TerminalSession then performs the only full
    // restore, rather than having a panic hook race normal drawing.
    let panic_events = event_tx.clone();
    let panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = panic_events.send(AgentEvent::Quit);
        panic_hook(info);
    }));
    let worker = tokio::spawn(agent_worker(
        cfg,
        history,
        channel_ui as Arc<dyn Ui>,
        msg_rx,
        event_tx,
        pending_images,
    ));

    let result = ui_loop(
        &mut terminal.terminal,
        event_rx,
        msg_tx,
        inbox,
        permissions,
        app,
        cwd,
    )
    .await;
    terminal.restore();
    let remaining = cfg_shutdown.shutdown_background_work(&shutdown_ui).await;
    if remaining > 0 {
        eprintln!("warning: {remaining} background task(s) missed the shutdown deadline");
    }
    // The worker holds the session rollout; aborting mid-write is equivalent
    // to a killed session, which resume already repairs.
    worker.abort();
    // Tear down the session worktree (dirty kept on its branch, clean removed);
    // the terminal is restored, so the kept-tree note prints to stderr.
    if let Some(note) = kloop_core::worktree::finish_active(&cfg_shutdown).await {
        eprintln!("{}", note.trim());
    }
    result
}

/// The cwd for the session banner, with `$HOME` contracted to `~` (the common
/// case) so the header stays short.
fn display_cwd(cwd: &std::path::Path) -> String {
    if let Some(home) = std::env::var_os("HOME") {
        let home = std::path::Path::new(&home);
        if let Ok(rest) = cwd.strip_prefix(home) {
            return if rest.as_os_str().is_empty() {
                "~".to_string()
            } else {
                format!("~/{}", rest.display())
            };
        }
    }
    cwd.display().to_string()
}

/// The current git branch for the session banner, or `None` when the cwd is not
/// a repository (or git is unavailable). Best-effort and one-shot at startup —
/// a display nicety, not a correctness path, so failure just omits the row.
fn git_branch(cwd: &std::path::Path) -> Option<String> {
    let mut command = std::process::Command::new("git");
    command
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd);
    let out = kloop_process_spawn::output_std(&mut command).ok()?;
    if !out.status.success() {
        return None;
    }
    let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// The slash-menu catalog: the built-in commands, then the loaded skills and
/// user commands (both `/name`-invocable through `commands::run`). Order matches
/// what the `/help` and unknown-command listings show.
fn slash_catalog(cfg: &Config) -> Vec<menu::CommandInfo> {
    let mut out: Vec<menu::CommandInfo> = kloop_core::commands::BUILTINS
        .iter()
        .map(|b| menu::CommandInfo {
            name: b.name.to_string(),
            description: b.summary.to_string(),
        })
        .collect();
    out.extend(cfg.skills.iter().map(|s| menu::CommandInfo {
        name: s.name.clone(),
        description: s.description.clone(),
    }));
    out
}

fn send_command_result_events(
    events: &mpsc::UnboundedSender<AgentEvent>,
    result: &kloop_core::commands::SlashResult,
) -> bool {
    if result.cleared && events.send(AgentEvent::ClearTranscript).is_err() {
        return false;
    }
    if let Some(snapshot) = &result.task_graph
        && events
            .send(AgentEvent::Core(CoreEvent::TaskGraphUpdated(
                snapshot.clone(),
            )))
            .is_err()
    {
        return false;
    }
    result.output.is_empty()
        || events
            .send(AgentEvent::System(result.output.clone()))
            .is_ok()
}

/// Owns History for its whole lifetime and runs turns strictly one at a time;
/// the UI loop enforces single-flight by ignoring Enter while running.
async fn agent_worker(
    mut cfg: Arc<Config>,
    mut history: History,
    ui: Arc<dyn Ui>,
    mut msgs: mpsc::UnboundedReceiver<WorkerMsg>,
    events: mpsc::UnboundedSender<AgentEvent>,
    // `--image` blocks ride the first user turn; taken once, then empty.
    mut pending_images: Vec<ContentBlock>,
) {
    let mut provider_state = kloop_core::provider_route::SessionProviderState::from_timeline(
        Arc::clone(&cfg.provider_catalog),
        history.provider_routes(),
    )
    .expect("TUI history route timeline was validated before worker start");
    loop {
        let next = msgs.recv().await;
        let Some(msg) = next else {
            break;
        };
        match msg {
            WorkerMsg::Turn(turn) => {
                let _ = events.send(AgentEvent::RouteFrozen(cfg.provider_route.public_route()));
                // `--image` blocks ride the first turn; the composer's attached
                // images ride the turn they were sent with. Merge both.
                let mut images = std::mem::take(&mut pending_images);
                images.extend(turn.images);
                let msg = if images.is_empty() {
                    Message::user_text(turn.text)
                } else {
                    Message::user_with_blocks(turn.text, images)
                };
                history.record(msg);
                let outcome = run_turn(&cfg, &mut history, &ui, &turn.cancel, 0).await;
                let _ = events.send(AgentEvent::Core(CoreEvent::Usage(
                    history.estimated_tokens(),
                )));
                if events
                    .send(AgentEvent::Core(CoreEvent::TurnEnded(outcome.reason)))
                    .is_err()
                {
                    return;
                }
            }
            WorkerMsg::Wake { cancel } => {
                let _ = events.send(AgentEvent::RouteFrozen(cfg.provider_route.public_route()));
                // Raced: a still-running turn already drained the reinjection,
                // or stop_agent left nothing. Nothing to sample — just clear the
                // busy state the UI loop set when it dispatched the wake.
                if cfg.inbox.is_empty() {
                    if events
                        .send(AgentEvent::Core(CoreEvent::TurnEnded(EndReason::Completed)))
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
                let _ = events.send(AgentEvent::Core(CoreEvent::Usage(
                    history.estimated_tokens(),
                )));
                if events
                    .send(AgentEvent::Core(CoreEvent::TurnEnded(outcome.reason)))
                    .is_err()
                {
                    return;
                }
            }
            WorkerMsg::Command { line, cancel } => {
                let _ = events.send(AgentEvent::RouteFrozen(cfg.provider_route.public_route()));
                let result = kloop_core::commands::run_with_provider_state(
                    &line,
                    &mut history,
                    &cfg,
                    &provider_state,
                    &cancel,
                )
                .await;
                if result.provider_changed {
                    let route = provider_state.active_route();
                    cfg = Arc::new(cfg.clone_with_provider_route(provider_state.freeze()));
                    let _ = events.send(AgentEvent::ProviderChanged(route));
                }
                // Clear first (drops the old cells), then apply the exact empty
                // graph fence, then show the result on the now-blank transcript.
                if !send_command_result_events(&events, &result) {
                    return;
                }
                if result.open_provider_picker
                    && events
                        .send(AgentEvent::ProviderPicker(
                            cfg.provider_catalog.descriptors(),
                        ))
                        .is_err()
                {
                    return;
                }
                // `/exit`: tell the loop to quit (it tears the terminal down
                // exactly like a two-tap Ctrl+C). No TurnEnded is needed.
                if result.quit {
                    let _ = events.send(AgentEvent::Quit);
                    return;
                }
                // A skill invoked as `/name` expands to a prompt: record it and
                // run a turn, streaming through `ui` exactly like WorkerMsg::Turn.
                if let Some(prompt) = result.run_turn {
                    history.record(Message::user_text(prompt));
                    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
                    let _ = events.send(AgentEvent::Core(CoreEvent::Usage(
                        history.estimated_tokens(),
                    )));
                    if events
                        .send(AgentEvent::Core(CoreEvent::TurnEnded(outcome.reason)))
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                // TurnEnded clears the busy state the key handler set on submit.
                if events
                    .send(AgentEvent::Core(CoreEvent::TurnEnded(EndReason::Completed)))
                    .is_err()
                {
                    return;
                }
            }
            WorkerMsg::ListForkPoints => {
                // An in-memory-only history (no rollout) can't be rewound.
                let points = match history.rollout_path() {
                    Some(path) => fork_points(path).unwrap_or_default(),
                    None => Vec::new(),
                };
                if events.send(AgentEvent::ForkPoints(points)).is_err() {
                    return;
                }
            }
            WorkerMsg::Fork { seq } => {
                let event = match fork_here(&history, seq) {
                    Ok((session_id, resumed)) => {
                        let messages = resumed.messages.clone();
                        let route = resumed
                            .snapshot
                            .provider_routes
                            .last()
                            .ok_or_else(|| {
                                std::io::Error::new(
                                    std::io::ErrorKind::InvalidData,
                                    "rewound session provider route timeline is missing",
                                )
                            })
                            .and_then(|receipt| {
                                cfg.provider_catalog
                                    .restore_route(receipt)
                                    .map_err(std::io::Error::other)
                            });
                        match route {
                            Ok(route) => {
                                history.rebase(resumed);
                                cfg.reset_deferred_tool_capabilities();
                                cfg = Arc::new(cfg.clone_with_provider_route(route.clone()));
                                provider_state =
                                    kloop_core::provider_route::SessionProviderState::from_timeline(
                                        Arc::clone(&cfg.provider_catalog),
                                        history.provider_routes(),
                                    )
                                    .expect("rewound provider timeline was validated on recovery");
                                AgentEvent::Forked {
                                    session_id,
                                    messages,
                                    route: route.public_route(),
                                }
                            }
                            Err(error) => AgentEvent::System(format!(
                                "rewind failed: cannot restore provider route: {error}"
                            )),
                        }
                    }
                    // A failed rewind leaves History untouched; report and carry
                    // on the original branch.
                    Err(e) => AgentEvent::System(format!("rewind failed: {e}")),
                };
                if events.send(event).is_err() {
                    return;
                }
            }
        }
    }
}

/// Fork the live session at `seq` and load the branch: the new id, its messages,
/// and a rollout writer pointed at the fork file. The session's own directory is
/// the sessions dir (branches are siblings). Errors if the history isn't backed
/// by a file or the fork/reload fails.
fn fork_here(
    history: &History,
    seq: u64,
) -> std::io::Result<(String, kloop_core::rollout::ResumedSession)> {
    let src = history.rollout_path().ok_or_else(|| {
        std::io::Error::other("this session is not being saved, so it cannot be rewound")
    })?;
    let sessions_dir = src
        .parent()
        .ok_or_else(|| std::io::Error::other("session file has no parent directory"))?;
    let fork_path = fork_session(src, Some(seq), sessions_dir)?;
    let resumed = inspect_session(&fork_path)?.recover()?;
    Ok((session_id_of(&fork_path), resumed))
}

struct PinnedBackend<B> {
    inner: B,
    pinned_size: Option<ratatui::layout::Size>,
}

impl<B> PinnedBackend<B> {
    fn new(inner: B) -> Self {
        Self {
            inner,
            pinned_size: None,
        }
    }
}

impl<B: ratatui::backend::Backend> PinnedBackend<B> {
    fn pin_current_size(&mut self) -> std::result::Result<(), B::Error> {
        self.pinned_size = Some(self.inner.size()?);
        Ok(())
    }

    fn unpin_size(&mut self) {
        self.pinned_size = None;
    }
}

impl<B: ratatui::backend::Backend> ratatui::backend::Backend for PinnedBackend<B> {
    type Error = B::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        self.inner.draw(content)
    }

    fn append_lines(&mut self, lines: u16) -> std::result::Result<(), Self::Error> {
        self.inner.append_lines(lines)
    }

    fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
        self.inner.hide_cursor()
    }

    fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
        self.inner.show_cursor()
    }

    fn get_cursor_position(
        &mut self,
    ) -> std::result::Result<ratatui::layout::Position, Self::Error> {
        self.inner.get_cursor_position()
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.set_cursor_position(position)
    }

    fn clear(&mut self) -> std::result::Result<(), Self::Error> {
        self.inner.clear()
    }

    fn clear_region(
        &mut self,
        clear_type: ratatui::backend::ClearType,
    ) -> std::result::Result<(), Self::Error> {
        self.inner.clear_region(clear_type)
    }

    fn size(&self) -> std::result::Result<ratatui::layout::Size, Self::Error> {
        self.pinned_size.map_or_else(|| self.inner.size(), Ok)
    }

    fn window_size(&mut self) -> std::result::Result<ratatui::backend::WindowSize, Self::Error> {
        let mut size = self.inner.window_size()?;
        if let Some(pinned) = self.pinned_size {
            size.columns_rows = pinned;
        }
        Ok(size)
    }

    fn flush(&mut self) -> std::result::Result<(), Self::Error> {
        self.inner.flush()
    }
}

type Terminal =
    ratatui::Terminal<PinnedBackend<ratatui::backend::CrosstermBackend<std::io::Stdout>>>;

#[cfg(unix)]
struct KeyboardEnhancementGuard {
    pushed: bool,
}

#[cfg(unix)]
impl KeyboardEnhancementGuard {
    fn push() -> Self {
        let pushed = crossterm::execute!(
            std::io::stdout(),
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )
        .is_ok();
        Self { pushed }
    }

    fn pop(&mut self, out: &mut std::io::Stdout) {
        if self.pushed {
            let _ = crossterm::execute!(out, PopKeyboardEnhancementFlags);
            self.pushed = false;
        }
    }
}

#[cfg(not(unix))]
struct KeyboardEnhancementGuard;

#[cfg(not(unix))]
impl KeyboardEnhancementGuard {
    fn push() -> Self {
        Self
    }

    fn pop(&mut self, _out: &mut std::io::Stdout) {}
}

struct TerminalModes {
    keyboard_enhancement: KeyboardEnhancementGuard,
    restored: bool,
}

impl TerminalModes {
    fn new(keyboard_enhancement: KeyboardEnhancementGuard) -> Self {
        Self {
            keyboard_enhancement,
            restored: false,
        }
    }

    fn restore(&mut self) {
        if self.restored {
            return;
        }
        restore_terminal(&mut self.keyboard_enhancement);
        self.restored = true;
    }
}

impl Drop for TerminalModes {
    fn drop(&mut self) {
        self.restore();
    }
}

struct TerminalSession {
    terminal: Terminal,
    modes: TerminalModes,
}

impl TerminalSession {
    fn restore(&mut self) {
        self.modes.restore();
    }
}

fn setup_terminal() -> Result<TerminalSession> {
    crossterm::terminal::enable_raw_mode()?;
    // Bracketed paste (plan 38 slice 3): the terminal wraps pasted text so a
    // large paste arrives as one `Event::Paste` (collapsed to a placeholder)
    // instead of a burst of keystrokes, and a dragged image-file path can be
    // recognized. This is a plain control sequence — no CPR, so it does not race
    // stdin like the viewport probe does.
    if let Err(error) =
        crossterm::execute!(std::io::stdout(), crossterm::event::EnableBracketedPaste)
    {
        let _ = crossterm::terminal::disable_raw_mode();
        return Err(error.into());
    }
    // Ask compatible Unix terminals to report modified keys through CSI-u, so
    // Shift+Enter reaches App as Enter+SHIFT instead of the same CR as Enter.
    // This is best-effort: legacy terminals ignore the push. Do not use
    // supports_keyboard_enhancement() here — its stdin query may block for two
    // seconds and would add another reader beside the inline viewport's CPR.
    let keyboard_enhancement = KeyboardEnhancementGuard::push();
    // Inline viewport, no alternate screen (plan 38 slice 0): the transcript
    // scrolls into native scrollback, so the mouse wheel / selection / Cmd+F
    // reach history directly. The viewport is the full terminal height, so
    // `insert_before` pushes finalized cells above it into scrollback. This CPR
    // (cursor-position query) runs before the input thread starts, so nothing
    // races it for stdin.
    let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
    let backend = PinnedBackend::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()));
    match ratatui::Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height.max(1)),
        },
    ) {
        Ok(terminal) => Ok(TerminalSession {
            terminal,
            modes: TerminalModes::new(keyboard_enhancement),
        }),
        Err(error) => {
            let mut keyboard_enhancement = keyboard_enhancement;
            restore_terminal(&mut keyboard_enhancement);
            Err(error.into())
        }
    }
}

fn restore_terminal(keyboard_enhancement: &mut KeyboardEnhancementGuard) {
    // No alternate screen to leave. The last draw left the cursor mid-viewport
    // (at the composer); drop it to the bottom row and emit an explicit CR+LF so
    // the shell prompt returns on a fresh line at column 0 — otherwise zsh marks
    // the partial line with a "%". The last frame stays in scrollback.
    let mut out = std::io::stdout();
    keyboard_enhancement.pop(&mut out);
    let bottom = crossterm::terminal::size()
        .map(|(_, h)| h.saturating_sub(1))
        .unwrap_or(0);
    let _ = crossterm::execute!(
        out,
        crossterm::event::DisableBracketedPaste,
        crossterm::cursor::Show,
        crossterm::cursor::MoveTo(0, bottom),
        crossterm::style::Print("\r\n"),
    );
    let _ = out.flush();
    let _ = crossterm::terminal::disable_raw_mode();
}

/// Read terminal events on a dedicated OS thread and forward them to the async
/// loop. Uses `poll(timeout)` + `read()` rather than crossterm's `EventStream`
/// so the internal reader lock is released between polls: that lets a resize's
/// `get_cursor_position` (CPR) acquire the lock within one poll interval instead
/// of deadlocking against a reader parked in a lock-holding blocking read (plan
/// 38 slice 0, the inline integration trap). The 200ms poll bounds that wait
/// well under crossterm's 2s CPR timeout and stays idle-cheap.
fn spawn_input_thread(
    tx: mpsc::UnboundedSender<Event>,
    stop: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        while !stop.load(Ordering::Relaxed) {
            match crossterm::event::poll(Duration::from_millis(200)) {
                Ok(true) => match crossterm::event::read() {
                    Ok(ev) => {
                        if tx.send(ev).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                },
                // Timed out with no event: loop to re-check the stop flag.
                Ok(false) => {}
                Err(_) => break,
            }
        }
    })
}

/// Insert one overflow batch into native scrollback, then invalidate the inline
/// viewport. With `scrolling-regions` off, Ratatui's full-height `insert_before`
/// scrolls the batch off the bottom with `append_lines` (a real LF-at-bottom
/// scroll every terminal handles), instead of the DECSTBM one-row scroll that
/// iTerm2 smears (plan 99); `clear` resets its diff buffer so the next draw
/// restores the transcript tail and bottom chrome.
fn insert_scrollback_blocks<B>(
    terminal: &mut ratatui::Terminal<B>,
    blocks: Vec<Vec<Line<'static>>>,
) -> std::result::Result<(), B::Error>
where
    B: ratatui::backend::Backend,
{
    for lines in blocks {
        let height = lines.len() as u16;
        if height == 0 {
            continue;
        }
        terminal.insert_before(height, |buf| {
            let area = buf.area;
            Paragraph::new(lines).render(area, buf);
        })?;
    }
    // Inline clear starts at the viewport, so committed native scrollback stays
    // intact while the next draw is forced to repaint every visible cell.
    terminal.clear()?;
    Ok(())
}

fn overflow_commit_count(app: &App, viewport: Rect) -> usize {
    let width = usize::from(viewport.width).max(1);
    let height = usize::from(viewport.height);
    // Draw and commit share one mutable-chrome calculation: activity plus the
    // revisioned Task panel must both stay out of native scrollback.
    let chrome = render::live_chrome_layout(app, viewport);
    let reserve = 2 + render::composer_height(app, width) + 1 + chrome.reserved_rows();
    let active_h = height.saturating_sub(reserve).max(1);
    render::commit_count(&app.cells, width, active_h, |index| {
        app.display_cell_live(index)
    })
}

/// Freeze finalized cells that overflow the last successfully drawn live region
/// into native scrollback, then drop them from the app's tail. The caller must
/// pass the viewport from that draw; committing from a pre-draw size probe would
/// race Ratatui's own autoresize and could irreversibly freeze the wrong prefix.
fn commit_overflow<B>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut App,
    viewport: Rect,
) -> std::result::Result<bool, B::Error>
where
    B: ratatui::backend::Backend,
{
    let width = usize::from(viewport.width).max(1);
    let n = overflow_commit_count(app, viewport);
    if n == 0 {
        return Ok(false);
    }
    // Render each cell to fixed-height lines up front so the borrow of
    // `app.cells` ends before `drain_committed` takes it mutably.
    let blocks: Vec<Vec<Line<'static>>> = app.cells[..n]
        .iter()
        .map(|c| render::cell_lines(c, width))
        .collect();
    insert_scrollback_blocks(terminal, blocks)?;
    app.drain_committed(n);
    Ok(true)
}

/// Draw once so Ratatui's internal autoresize establishes the authoritative
/// viewport. Before an irreversible commit, the backend captures and pins one
/// physical size: autoresize confirmation, insert/drain, and the required repaint
/// therefore share one geometry even if a resize arrives mid-transaction. The
/// returned geometry belongs to the final frame and drives the next key event.
fn draw_frame<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
    app: &mut App,
    hud: &render::Hud,
) -> std::result::Result<Rect, B::Error>
where
    B: ratatui::backend::Backend,
{
    for retry in 0..=1 {
        terminal.draw(|frame| render::draw(frame, app, hud))?;
        let drawn_viewport = terminal.get_frame().area();
        let overlay_open =
            !app.interactions.is_empty() || app.fork_picker.is_some() || app.popup.is_some();
        if overlay_open {
            return Ok(drawn_viewport);
        }

        terminal.backend_mut().pin_current_size()?;
        let attempt: std::result::Result<Option<Rect>, B::Error> = (|| {
            terminal.autoresize()?;
            let confirmed_viewport = terminal.get_frame().area();
            if confirmed_viewport != drawn_viewport {
                return Ok(None);
            }
            if commit_overflow(terminal, app, confirmed_viewport)? {
                terminal.draw(|frame| render::draw(frame, app, hud))?;
            }
            Ok(Some(terminal.get_frame().area()))
        })();
        terminal.backend_mut().unpin_size();

        match attempt? {
            Some(viewport) => return Ok(viewport),
            None if retry == 0 => continue,
            None => {
                terminal.draw(|frame| render::draw(frame, app, hud))?;
                return Ok(terminal.get_frame().area());
            }
        }
    }
    unreachable!("bounded geometry retry loop always returns")
}

/// Recognize a pasted/dragged image-file path and load it into an Image block
/// (plan 38 slice 3). Terminals paste a dropped file as its (often quoted) path;
/// a single-line path with an image extension that reads and validates becomes
/// an attachment, otherwise the paste is plain text. Returns the display label
/// (file name) and the block.
fn load_image_paste(s: &str) -> Option<(String, ContentBlock)> {
    let path = s.trim().trim_matches(['\'', '"']).trim();
    if path.is_empty() || path.contains('\n') {
        return None;
    }
    let p = std::path::Path::new(path);
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    if !matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp") {
        return None;
    }
    let bytes = std::fs::read(p).ok()?;
    let block = kloop_core::image::image_block_from_bytes(&bytes).ok()?;
    let label = p
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string();
    Some((label, block))
}

async fn search_files(cwd: &std::path::Path, target: &menu::CompletionTarget) -> Vec<String> {
    let root = cwd.to_path_buf();
    let query = target.query.clone();
    tokio::task::spawn_blocking(move || {
        kloop_core::fs_complete::complete_files(&root, &query, menu::FILE_MENU_MAX)
    })
    .await
    .unwrap_or_default()
}

/// Autowake (plan 26) fires only when the agent is idle AND a reinjection is
/// waiting: a running turn drains the inbox at its own round boundary (firing
/// then would double-deliver), and an empty inbox means there is nothing to wake
/// for. Kept as a pure function so the delivery invariant is unit-tested rather
/// than only exercised through the live terminal loop.
fn autowake_ready(running: bool, inbox: &Inbox) -> bool {
    !running && !inbox.is_empty()
}

fn dispatch_autowake(
    app: &mut App,
    inbox: &Inbox,
    msgs: &mpsc::UnboundedSender<WorkerMsg>,
) -> Option<CancellationToken> {
    if !autowake_ready(app.running, inbox) {
        return None;
    }
    let cancel = CancellationToken::new();
    msgs.send(WorkerMsg::Wake {
        cancel: cancel.clone(),
    })
    .ok()?;
    app.running = true;
    app.freeze_selected_route();
    Some(cancel)
}

fn event_cwd(event: &AgentEvent) -> Option<std::path::PathBuf> {
    match event {
        AgentEvent::Core(CoreEvent::CwdChanged { cwd, .. }) => Some(std::path::PathBuf::from(cwd)),
        _ => None,
    }
}

async fn ui_loop(
    terminal: &mut Terminal,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    msgs: mpsc::UnboundedSender<WorkerMsg>,
    inbox: Arc<Inbox>,
    permissions: Arc<kloop_core::permissions::Permissions>,
    mut app: App,
    mut cwd: std::path::PathBuf,
) -> Result<()> {
    // Seed the status-bar badge from the real starting mode (e.g. plan). The
    // App is built in `run` (transcript replay + `/` menu catalog); a resumed
    // session's cells start as the tail and scroll into scrollback on overflow.
    app.mode = permissions.mode();

    // Keys arrive from a dedicated poll thread, not crossterm's EventStream, so
    // its reader never parks holding the lock a resize's CPR needs (see
    // `spawn_input_thread`). Started after `setup_terminal`, so nothing raced
    // the viewport's construction CPR.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let input_thread = spawn_input_thread(input_tx, stop.clone());

    let mut current_cancel: Option<CancellationToken> = None;
    let mut inbox_activity = inbox.subscribe_activity();
    // Wall-clock timing for the animated HUD (plan 38 slice 5). The pure `App`
    // has no clock, so the loop owns it: the turn clock runs while `app.running`,
    // the thinking clock while a thinking block streams.
    let reduced_motion = anim::reduced_motion();
    let mut turn_started: Option<Instant> = None;
    let mut thinking_started: Option<Instant> = None;
    let outcome = loop {
        // Reconcile the turn clock with the app's running state (starts on the
        // first frame of a turn, clears when it ends).
        if app.running {
            turn_started.get_or_insert_with(Instant::now);
        } else {
            turn_started = None;
            thinking_started = None;
        }
        let elapsed = turn_started.map(|t| t.elapsed());
        let hud = render::Hud {
            elapsed,
            thinking: thinking_started.map(|t| t.elapsed()),
            phase: elapsed
                .map(|d| (d.as_millis() as u64 / anim::STEP_MS) as usize)
                .unwrap_or(0),
            reduced_motion,
        };

        // Ratatui's draw owns autoresize. Commit only from the viewport of that
        // completed frame; if a commit clears it, `draw_frame` immediately
        // repaints and returns the final geometry used by key navigation.
        let viewport = match draw_frame(terminal, &mut app, &hud) {
            Ok(viewport) => viewport,
            Err(error) => break Err(error),
        };
        // Animation self-drives: while a turn runs (and no overlay owns the
        // screen), a frame tick wakes the loop to advance the spinner/elapsed;
        // idle, the tick is disabled so `select` blocks with zero CPU (the
        // FrameRequester role, played by tokio, plan 38 slice 5).
        let animating = app.running
            && app.interactions.is_empty()
            && app.fork_picker.is_none()
            && app.popup.is_none();
        let tick_ms = if reduced_motion { 1000 } else { anim::STEP_MS };
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    match app.on_key(usize::from(viewport.width), k) {
                        Command::Submit(text) => {
                            let cancel = CancellationToken::new();
                            current_cancel = Some(cancel.clone());
                            // Attachments live on the App (ContentBlock isn't Eq,
                            // so they can't ride the Command); take them here.
                            let images = app.take_submit_images();
                            let _ = msgs.send(WorkerMsg::Turn(Turn {
                                text,
                                images,
                                cancel,
                            }));
                        }
                        Command::Slash(line) => {
                            // Runs on the worker (owns History); its cancel lets
                            // Ctrl+C interrupt a slow /compact like a turn.
                            let cancel = CancellationToken::new();
                            current_cancel = Some(cancel.clone());
                            let _ = msgs.send(WorkerMsg::Command { line, cancel });
                        }
                        Command::Steer(text) => {
                            // Enqueue for the running turn; the agent loop
                            // drains it at the next round boundary. The user's
                            // raw text already showed as a User cell.
                            inbox.push(InboxItem::Steer(text));
                        }
                        Command::SetMode(mode) => {
                            // shift+Tab: apply the new mode to the shared gate;
                            // subsequent tool calls read it live. The badge is
                            // already updated in the App.
                            permissions.set_mode(mode);
                        }
                        Command::RequestForkPoints => {
                            // The worker owns the rollout path; it reads the
                            // fork targets and replies with a ForkPoints event.
                            let _ = msgs.send(WorkerMsg::ListForkPoints);
                        }
                        Command::Fork(seq) => {
                            let _ = msgs.send(WorkerMsg::Fork { seq });
                        }
                        Command::Interrupt => {
                            if let Some(cancel) = &current_cancel {
                                cancel.cancel();
                            }
                        }
                        Command::PasteClipboardImage => {
                            // Read the OS clipboard here (a side effect); attach
                            // an image, or note why there was none.
                            match clipboard::clipboard_image() {
                                Ok((label, block)) => app.attach_image(label, block),
                                Err(e) => app.apply(AgentEvent::Core(CoreEvent::Note(e))),
                            }
                        }
                        Command::SearchFiles(target) => {
                            let paths = search_files(&cwd, &target).await;
                            app.set_file_results(target, paths);
                        }
                        Command::Quit => break Ok(()),
                        Command::None => {}
                    }
                }
                // Bracketed paste (plan 38 slice 3): a dragged/pasted image-file
                // path attaches as an image, anything else goes to the composer
                // (a large paste collapses to a placeholder there).
                Some(Event::Paste(s)) if app.question_editor_active() => {
                    let _ = app.paste_text(&s);
                }
                Some(Event::Paste(s)) => match load_image_paste(&s) {
                    Some((label, block)) => app.attach_image(label, block),
                    None => {
                        if let Command::SearchFiles(target) = app.paste_text(&s) {
                            let paths = search_files(&cwd, &target).await;
                            app.set_file_results(target, paths);
                        }
                    }
                },
                // Resize repositions the viewport (handled by the autoresize at
                // the top of the loop); any other event just needs a redraw.
                Some(_) => {}
                None => break Ok(()),
            },
            activity = inbox_activity.changed() => {
                if activity.is_err() {
                    break Ok(());
                }
                let autowake = dispatch_autowake(&mut app, &inbox, &msgs);
                if let Some(cancel) = autowake {
                    current_cancel = Some(cancel);
                }
            }
            event = events.recv() => {
                let Some(event) = event else { break Ok(()) };
                // Snapshot before applying: a thinking block that was streaming
                // and is no longer gets its elapsed stamped into the sealed cell.
                let was_thinking = app.streaming_thinking();
                // `/exit` (AgentEvent::Quit) ends the loop; the caller restores
                // the terminal. Filter it out of the batch so app.apply never
                // sees it.
                let mut quit = matches!(event, AgentEvent::Quit);
                if !quit {
                    if let Some(next_cwd) = event_cwd(&event) {
                        cwd = next_cwd;
                    }
                    app.apply(event);
                }
                // Drain whatever else already arrived (streaming deltas come
                // in bursts) so we redraw once per batch, not per token.
                loop {
                    let next = events.try_recv();
                    let Ok(event) = next else {
                        break;
                    };
                    if matches!(event, AgentEvent::Quit) {
                        quit = true;
                    } else {
                        if let Some(next_cwd) = event_cwd(&event) {
                            cwd = next_cwd;
                        }
                        app.apply(event);
                    }
                }
                if quit {
                    break Ok(());
                }
                // Time the thinking block: start the clock when it opens, seal the
                // cell with its final elapsed when it closes.
                if app.streaming_thinking() {
                    thinking_started.get_or_insert_with(Instant::now);
                } else if was_thinking
                    && let Some(t) = thinking_started.take() {
                        app.seal_thinking(t.elapsed().as_secs());
                    }
                // Autowake (plan 26): a background sub-agent finished (its
                // agent_end woke this select) and left a result in the inbox
                // while the agent sits idle. Start a turn to deliver it without
                // waiting for the user. The guard also catches the race where a
                // reinjection lands just after a turn ends.
                let autowake = dispatch_autowake(&mut app, &inbox, &msgs);
                if let Some(cancel) = autowake {
                    current_cancel = Some(cancel);
                }
            }
            // Frame tick: only armed while animating, so an idle loop never wakes
            // here. Firing just redraws (elapsed/spinner advance at the top).
            _ = tokio::time::sleep(Duration::from_millis(tick_ms)), if animating => {}
        }
    };
    // Stop the input thread (it wakes within one poll interval) before the
    // caller restores the terminal, so no stray read lands after teardown.
    stop.store(true, Ordering::Relaxed);
    let _ = input_thread.join();
    outcome?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell as StateCell;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use ratatui::backend::Backend;
    use ratatui::backend::ClearType;
    use ratatui::backend::TestBackend;
    use ratatui::backend::WindowSize;
    use ratatui::buffer::Cell as BufferCell;
    use ratatui::layout::Position;
    use ratatui::layout::Size;

    use super::*;
    use kloop_core::rollout::Rollout;
    use kloop_protocol::ContentBlock;

    struct StagedSizeBackend {
        inner: TestBackend,
        sizes: RefCell<VecDeque<Size>>,
        last_size: StateCell<Size>,
        events: RefCell<Vec<&'static str>>,
    }

    impl StagedSizeBackend {
        fn new(width: u16, height: u16) -> Self {
            Self {
                inner: TestBackend::new(width, height),
                sizes: RefCell::new(VecDeque::new()),
                last_size: StateCell::new(Size::new(width, height)),
                events: RefCell::new(Vec::new()),
            }
        }

        fn stage_sizes(&self, sizes: impl IntoIterator<Item = Size>) {
            self.sizes.borrow_mut().extend(sizes);
            self.events.borrow_mut().clear();
        }

        fn record(&self, event: &'static str) {
            self.events.borrow_mut().push(event);
        }
    }

    impl Backend for StagedSizeBackend {
        type Error = <TestBackend as Backend>::Error;

        fn draw<'a, I>(&mut self, content: I) -> std::result::Result<(), Self::Error>
        where
            I: Iterator<Item = (u16, u16, &'a BufferCell)>,
        {
            self.record("draw");
            self.inner.draw(content)
        }

        fn append_lines(&mut self, lines: u16) -> std::result::Result<(), Self::Error> {
            self.inner.append_lines(lines)
        }

        fn hide_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.hide_cursor()
        }

        fn show_cursor(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.show_cursor()
        }

        fn get_cursor_position(&mut self) -> std::result::Result<Position, Self::Error> {
            self.inner.get_cursor_position()
        }

        fn set_cursor_position<P: Into<Position>>(
            &mut self,
            position: P,
        ) -> std::result::Result<(), Self::Error> {
            self.inner.set_cursor_position(position)
        }

        fn clear(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.clear()
        }

        fn clear_region(&mut self, clear_type: ClearType) -> std::result::Result<(), Self::Error> {
            self.inner.clear_region(clear_type)
        }

        fn size(&self) -> std::result::Result<Size, Self::Error> {
            self.record("size");
            if let Some(size) = self.sizes.borrow_mut().pop_front() {
                self.last_size.set(size);
            }
            Ok(self.last_size.get())
        }

        fn window_size(&mut self) -> std::result::Result<WindowSize, Self::Error> {
            self.inner.window_size()
        }

        fn flush(&mut self) -> std::result::Result<(), Self::Error> {
            self.inner.flush()
        }
    }

    /// The worker's rewind primitive: fork the live session's file at a cut,
    /// derive the sessions dir from the rollout path, and hand back the branch's
    /// id and truncated messages (which `rebase` then installs).
    #[test]
    fn fork_here_branches_the_live_session_at_a_cut() {
        let dir = std::env::temp_dir().join(format!("kloop-tui-forkhere-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("session.jsonl");
        let mut history = History::new(dir.clone());
        history.attach_rollout(Rollout::new(session.clone()));
        history.record(Message::user_text("one"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "done".into(),
        }]));
        history.record(Message::user_text("two"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "bye".into(),
        }]));

        // Route receipt + two messages keeps the first turn only.
        let (id, resumed) = fork_here(&history, 3).unwrap();
        assert_ne!(id, "session");
        assert_eq!(
            resumed.messages,
            vec![
                Message::user_text("one"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "done".into(),
                }]),
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn scrollback_insert_clears_viewport_without_clearing_history() {
        const WIDTH: u16 = 24;
        const HEIGHT: u16 = 6;
        let backend = ratatui::backend::TestBackend::new(WIDTH, HEIGHT);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(HEIGHT),
            },
        )
        .unwrap();
        let initial = vec![
            Line::from("live tail"),
            Line::from(""),
            Line::from("────────────────────────"),
            Line::from("› draft"),
            Line::from("────────────────────────"),
            Line::from("[manual]"),
        ];
        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new(initial.clone()), area);
            })
            .unwrap();

        insert_scrollback_blocks(
            &mut terminal,
            vec![
                vec![Line::from("committed one"), Line::from("committed two")],
                vec![Line::from("committed three")],
            ],
        )
        .unwrap();

        terminal.backend().assert_scrollback_lines([
            "committed one           ",
            "committed two           ",
            "committed three         ",
        ]);
        // TestBackend's AfterCursor keeps the cursor cell itself; every other
        // visible cell proves that the inline viewport was cleared. The real
        // Crossterm ED sequence clears from the cursor inclusively.
        assert!(
            terminal
                .backend()
                .buffer()
                .content
                .iter()
                .skip(1)
                .all(|cell| cell.symbol() == " ")
        );

        terminal
            .draw(|frame| {
                let area = frame.area();
                frame.render_widget(Paragraph::new(initial), area);
            })
            .unwrap();
        terminal.backend().assert_buffer_lines([
            "live tail               ",
            "                        ",
            "────────────────────────",
            "› draft                 ",
            "────────────────────────",
            "[manual]                ",
        ]);
    }

    fn snapshot(revision: u64, tasks: usize) -> kloop_core::tools::TaskGraphSnapshot {
        kloop_core::tools::TaskGraphSnapshot {
            revision,
            tasks: (1..=tasks)
                .map(|id| kloop_core::tools::TaskGraphTask {
                    id: id.to_string(),
                    subject: format!("Task {id}"),
                    status: kloop_core::tools::TaskStatus::Pending,
                    blocked_by: Vec::new(),
                    blocks: Vec::new(),
                })
                .collect(),
        }
    }

    #[test]
    fn startup_seed_uses_the_channel_ui_event_seam() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        ui.emit(&CoreEvent::TaskGraphUpdated(snapshot(0, 0)));
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::Core(CoreEvent::TaskGraphUpdated(graph))
                if graph.revision == 0 && graph.tasks.is_empty()
        ));
    }

    #[test]
    fn clear_command_events_order_transcript_then_graph_fence_then_system() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = kloop_core::commands::SlashResult {
            output: "cleared".into(),
            cleared: true,
            run_turn: None,
            task_graph: Some(snapshot(7, 0)),
            quit: false,
            provider_changed: false,
            open_provider_picker: false,
        };
        assert!(send_command_result_events(&tx, &result));
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::ClearTranscript
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::Core(CoreEvent::TaskGraphUpdated(graph))
                if graph.revision == 7 && graph.tasks.is_empty()
        ));
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::System(output) if output == "cleared"
        ));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn resize_before_commit_repaints_without_stale_overflow() {
        let backend = PinnedBackend::new(StagedSizeBackend::new(80, 24));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .unwrap();
        terminal
            .backend()
            .inner
            .stage_sizes([Size::new(80, 6), Size::new(80, 24)]);

        let mut app = App::new("resize-race".into());
        app.cells = (1..=12)
            .map(|index| Cell::Assistant(format!("history {index}")))
            .collect();
        let viewport = draw_frame(&mut terminal, &mut app, &render::Hud::default()).unwrap();

        let events = terminal.backend().inner.events.borrow();
        assert!(events.contains(&"draw"));
        assert_eq!((viewport.width, viewport.height), (80, 24));
        // A stale small size must not freeze overflow: the confirmed geometry is
        // the full height, so all twelve cells stay live (none committed).
        assert_eq!(app.cells.len(), 12);
    }

    #[test]
    fn size_pin_keeps_commit_and_repaint_on_confirmed_geometry() {
        let backend = PinnedBackend::new(StagedSizeBackend::new(80, 24));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .unwrap();
        terminal.backend().inner.stage_sizes([
            Size::new(80, 6),
            Size::new(80, 6),
            Size::new(80, 24),
        ]);

        let mut app = App::new("stable-small".into());
        app.cells = (1..=12)
            .map(|index| Cell::Assistant(format!("history {index}")))
            .collect();
        let viewport = draw_frame(&mut terminal, &mut app, &render::Hud::default()).unwrap();

        let events = terminal.backend().inner.events.borrow();
        assert!(events.contains(&"draw"), "frame was drawn: {events:?}");
        assert_eq!((viewport.width, viewport.height), (80, 6));
        // Confirmed small geometry freezes the overflowing prefix into
        // scrollback, so the live tail drops below the original twelve cells.
        assert!(app.cells.len() < 12);
        drop(events);

        let grown = draw_frame(&mut terminal, &mut app, &render::Hud::default()).unwrap();
        assert_eq!((grown.width, grown.height), (80, 24));
    }

    #[test]
    fn overflow_budget_reserves_live_tasks_without_committing_them() {
        let mut app = App::new("task-overflow".into());
        app.cells = (1..=8)
            .map(|index| Cell::Assistant(format!("history {index}")))
            .collect();
        let cells = app.cells.clone();
        let without_tasks = overflow_commit_count(&app, Rect::new(0, 0, 40, 10));

        app.task_graph = Some(snapshot(1, 3));
        let with_tasks = overflow_commit_count(&app, Rect::new(0, 0, 40, 10));
        assert!(with_tasks > without_tasks);
        assert_eq!(app.cells, cells);
        assert_eq!(app.task_graph.as_ref().unwrap().tasks.len(), 3);
    }

    #[test]
    fn inline_growth_uses_effective_viewport_for_overflow_budget() {
        let backend = ratatui::backend::TestBackend::new(40, 6);
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(6),
            },
        )
        .unwrap();
        terminal.backend_mut().resize(40, 12);
        terminal.autoresize().unwrap();
        let viewport = terminal.get_frame().area();
        assert_eq!((viewport.width, viewport.height), (40, 6));

        let mut app = App::new("inline-growth".into());
        app.cells = (1..=12)
            .map(|index| Cell::Assistant(format!("history {index}")))
            .collect();
        let effective = overflow_commit_count(&app, viewport);
        let physical = overflow_commit_count(&app, Rect::new(0, 0, 40, 12));
        assert!(effective > physical);
    }

    #[tokio::test]
    async fn scheduled_due_dispatches_wake_when_idle() {
        let inbox = Arc::new(Inbox::default());
        let clock = kloop_core::scheduler::ManualClock::new(0);
        let scheduler = kloop_core::scheduler::Scheduler::with_clock(
            Arc::clone(&inbox),
            None,
            clock.clone(),
            kloop_core::scheduler::SchedulerTimeZone::named("UTC").unwrap(),
        );
        scheduler.bind_owner("tui-owner").unwrap();
        let wakeup = scheduler
            .schedule_wakeup(60.0, "test delivery", "timer work")
            .unwrap();
        let mut activity = inbox.subscribe_activity();
        clock.set(wakeup.scheduled_for_ms);
        tokio::time::timeout(Duration::from_secs(1), activity.changed())
            .await
            .unwrap()
            .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new("scheduler-test".into());
        let cancel = dispatch_autowake(&mut app, &inbox, &tx).expect("idle app must wake");
        assert!(app.running);
        assert!(!cancel.is_cancelled());
        match rx.recv().await.unwrap() {
            WorkerMsg::Wake { cancel } => assert!(!cancel.is_cancelled()),
            _ => panic!("expected scheduler wake"),
        }
        assert!(dispatch_autowake(&mut app, &inbox, &tx).is_none());
        assert!(rx.try_recv().is_err());

        scheduler.shutdown().await;
    }
}
