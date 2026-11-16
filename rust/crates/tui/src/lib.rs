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
mod choice;
mod clipboard;
mod composer;
mod events;
mod markdown;
mod menu;
mod render;
mod session_picker;
mod terminal;
mod text_layout;
mod toolrow;

use std::future::Future;
use std::io::Write as _;
use std::ops::ControlFlow;
use std::pin::Pin;
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
use tokio::sync::mpsc;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent::run_turn;
use kloop_core::agent::run_turn_with_input;
// Aliased: `Event` alone is crossterm's terminal event in this module.
use kloop_core::Config;
use kloop_core::event::Event as CoreEvent;
use kloop_core::git::branch as git_branch;
use kloop_core::history::History;
use kloop_core::inbox::Inbox;
use kloop_core::inbox::InboxItem;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::Approver;
use kloop_core::rewind::AbandonedBranch;
use kloop_core::rewind::RewindOutcome;
use kloop_core::rollout::fork_points;

use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::Message;
pub use session_picker::SessionEntry;
pub use session_picker::pick_session;

use crate::app::App;
use crate::app::Cell;
use crate::app::Command;
use crate::events::AgentEvent;
use crate::events::ChannelUi;
use crate::terminal::FrameWriter;
use crate::terminal::PinnedBackend;
use crate::terminal::insert_scrollback_blocks;

/// Out-of-band notification sink (e.g. "saved rule to config.toml"); the
/// CLI's plain mode prints these to stderr, the TUI routes them into the
/// transcript as notes.
pub type NoteFn = Arc<dyn Fn(&str) + Send + Sync>;

/// What a startup task's body returns: one owned future, run on its own task.
pub type StartupFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Work the front-end must finish before it accepts input. It runs only after
/// the terminal is set up, so the banner and [`Self::label`] are on screen while
/// it works: a slow handshake reads as progress instead of a blank start.
pub struct StartupTask {
    /// The footer line shown from the first frame until the task reports or
    /// finishes.
    pub label: String,
    /// The body. It reports through [`StartupReporter`] and must call
    /// `line(None)` when it is done; dropping the reporter also releases the
    /// keyboard, so an early return cannot leave the UI gated.
    pub run: Box<dyn FnOnce(StartupReporter) -> StartupFuture + Send>,
}

/// The startup task's two outputs: notes join the transcript, a line replaces
/// the footer status. `line(None)` clears it and hands the keyboard to the user.
#[derive(Clone)]
pub struct StartupReporter {
    note: NoteFn,
    line: Arc<dyn Fn(Option<String>) + Send + Sync>,
}

impl StartupReporter {
    pub fn note(&self, message: &str) {
        (self.note)(message);
    }

    pub fn line(&self, message: Option<String>) {
        (self.line)(message);
    }
}

struct Turn {
    text: String,
    /// The composer's images in placeholder order (plan 242), so the k-th is
    /// the k-th `[Image #N]` in `text`; any `--image` blocks still riding the
    /// first turn are appended after them.
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
    /// messages and id. `cancel` stops a summarizing rewind before anything
    /// is swapped.
    Fork {
        seq: u64,
        abandoned: AbandonedBranch,
        cancel: CancellationToken,
    },
}

/// Tear the session down after the terminal is restored (or was never set up):
/// background work first, then the worker — it holds the session rollout, and
/// aborting mid-write is equivalent to a killed session, which resume already
/// repairs — and last the session worktree (dirty kept on its branch, clean
/// removed; the terminal is restored by now, so a kept-tree note reaches
/// stderr). The setup-failure exit and the normal one both go through here;
/// only the worker differs, and it does not exist yet when setup failed.
async fn teardown(cfg: &Config, ui: &Arc<dyn Ui>, worker: Option<&tokio::task::JoinHandle<()>>) {
    let remaining = cfg.shutdown_background_work(ui).await;
    if remaining > 0 {
        eprintln!("warning: {remaining} background task(s) missed the shutdown deadline");
    }
    if let Some(worker) = worker {
        worker.abort();
    }
    if let Some(note) = kloop_core::worktree::finish_active(cfg).await {
        eprintln!("{}", note.trim());
    }
}

/// Run the TUI until the user quits. `make_config` is called once with the
/// TUI's approver (the y/a/p/n popup) and note sink, so the caller can wire
/// them into `Permissions` without this crate knowing about rule loading.
/// `startup` is work that must settle before the first turn (the CLI uses it
/// for MCP handshakes); it runs against the already-painted UI and gates input.
/// `version` is the caller's build stamp for the banner (plan 161) — the binary
/// knows which commit it came from, this crate does not.
pub async fn run(
    make_config: impl FnOnce(Arc<dyn Approver>, Arc<dyn Questioner>, NoteFn) -> Result<Config>,
    startup: Option<StartupTask>,
    history: History,
    session_id: String,
    pending_images: Vec<ContentBlock>,
    worktree: Option<String>,
    version: &str,
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let channel_ui = Arc::new(ChannelUi::new(event_tx.clone()));
    let notes: NoteFn = {
        let note_ui = channel_ui.clone();
        Arc::new(move |s: &str| note_ui.emit(&CoreEvent::Note(s.to_string())))
    };
    let cfg = Arc::new(make_config(
        channel_ui.clone(),
        channel_ui.clone(),
        notes.clone(),
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
    // The worker's current Config. `/clear` and rewind replace it with a new
    // session's, so everything outside the worker that acts on the session —
    // the UI loop's steering inbox and mode switch, the teardown below — reads
    // it from here instead of holding the first session's handles.
    let current = Arc::new(std::sync::Mutex::new(Arc::clone(&cfg)));
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
            cfg.provider_route.model().to_string(),
            cfg.context_window,
            kloop_core::agent::context_estimate(&cfg, &history),
        )
        .with_route(cfg.provider_route.public_route());
    app.cells = app::cells_from_history(history.messages());
    // The opening session banner (plan 38 slice 6): the first cell, so it leads
    // the transcript and scrolls into scrollback. Built here with the git/env
    // reads done, keeping the renderer pure. On resume it still opens the replay.
    app.cells.insert(
        0,
        Cell::SessionHeader {
            version: version.to_string(),
            model: cfg.provider_route.model().to_string(),
            cwd: display_cwd(&effective_cwd),
            branch: effective_branch,
            mode: permissions.mode().label().to_string(),
        },
    );
    // Seed through the same event seam used by live mutations. Keeping revision 0
    // as a real first snapshot lets App distinguish an empty list from a seed
    // that has not arrived yet.
    channel_ui.emit(&CoreEvent::TodoUpdated(cfg.todos.snapshot()));
    let cwd = effective_cwd;
    // The first frame already carries this line, so a slow startup task is
    // visible from the moment the terminal takes over.
    let startup_label = startup.as_ref().map(|task| task.label.clone());
    app.startup = startup_label.clone();

    let shutdown_ui: Arc<dyn Ui> = channel_ui.clone();
    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let current_cfg = || Arc::clone(&current.lock().unwrap());

    // Setup can fail AFTER raw mode is enabled (e.g. an intermediate write
    // fails; the inline viewport's construction probe falls back to the origin
    // instead of failing). A bare `?` here would return without restoring the
    // terminal or tearing down a `--worktree` tree — leaving the shell in raw
    // mode and leaking the worktree on disk (there is no Drop-based cleanup).
    // So on failure run the same teardown the normal exit does, then propagate.
    let mut terminal = match setup_terminal() {
        Ok(t) => t,
        Err(e) => {
            teardown(&current_cfg(), &shutdown_ui, None).await;
            return Err(e);
        }
    };

    // A panic in the worker task is otherwise absorbed by JoinHandle and leaves
    // the UI reading/drawing against a terminal whose owner is still alive. Ask
    // the single UI owner to stop; TerminalSession then performs the only full
    // restore, rather than having a panic hook race normal drawing.
    let panic_hook = PanicHookGuard::install(event_tx.clone());
    let worker = tokio::spawn(agent_worker(
        Worker {
            cfg,
            current: Arc::clone(&current),
            ui: channel_ui as Arc<dyn Ui>,
            events: event_tx,
            version: version.to_string(),
        },
        history,
        msg_rx,
        pending_images,
    ));

    // Startup gets a watch channel rather than an event: the loop only needs
    // the latest line, and dropping the sender (normal return, panic, or a
    // future that forgot `line(None)`) is itself the "done" signal.
    let (startup_line_tx, startup_line_rx) = watch::channel::<Option<String>>(startup_label);
    let startup_task = startup.map(|task| {
        let line = Arc::new(move |line: Option<String>| {
            let _ = startup_line_tx.send(line);
        });
        tokio::spawn((task.run)(StartupReporter {
            note: notes.clone(),
            line,
        }))
    });

    let result = ui_loop(
        &mut terminal.terminal,
        event_rx,
        msg_tx,
        Arc::clone(&current),
        permissions,
        UiBoot {
            app,
            startup_line: startup_line_rx,
        },
        cwd,
    )
    .await;
    if let Some(task) = startup_task {
        task.abort();
    }
    terminal.restore();
    teardown(&current_cfg(), &shutdown_ui, Some(&worker)).await;
    // Nothing below this line panics into the TUI's own hook, and a host that
    // runs `run` twice (or installs its own hook afterwards) should not find
    // ours still in front, holding an `event_tx` clone.
    drop(panic_hook);
    result
}

/// The session's panic hook, put back on drop.
///
/// It reaches the previous hook lazily through a shared slot rather than owning
/// it, so a panic that already fired does not consume the hook the restore
/// needs.
struct PanicHookGuard {
    previous: PanicHookSlot,
}

type PanicHook = Box<dyn Fn(&std::panic::PanicHookInfo<'_>) + Send + Sync + 'static>;
/// Shared between the installed closure and the guard: the closure reaches the
/// previous hook through it, the guard takes it back on drop.
type PanicHookSlot = std::sync::Arc<std::sync::Mutex<Option<PanicHook>>>;

impl PanicHookGuard {
    fn install(events: mpsc::UnboundedSender<AgentEvent>) -> Self {
        let previous: PanicHookSlot =
            std::sync::Arc::new(std::sync::Mutex::new(Some(std::panic::take_hook())));
        let slot = std::sync::Arc::clone(&previous);
        std::panic::set_hook(Box::new(move |info| {
            let _ = events.send(AgentEvent::Quit);
            if let Ok(previous) = slot.lock()
                && let Some(hook) = previous.as_ref()
            {
                hook(info);
            }
        }));
        Self { previous }
    }
}

impl Drop for PanicHookGuard {
    fn drop(&mut self) {
        // `set_hook` itself panics with "cannot modify the panic hook from a
        // panicking thread", and a panic in a destructor during cleanup aborts
        // the process — which would take the terminal guard's restore down with
        // it. Unwinding therefore keeps our hook installed; a normal exit is
        // the only time putting the previous one back is both possible and
        // useful.
        if std::thread::panicking() {
            return;
        }
        let previous = self.previous.lock().ok().and_then(|mut slot| slot.take());
        if let Some(hook) = previous {
            std::panic::set_hook(hook);
        }
    }
}

/// The cwd for the session banner, with the process home contracted to `~`
/// (the common case) so the header stays short.
fn display_cwd(cwd: &std::path::Path) -> String {
    kloop_core::paths::display_with_tilde(cwd)
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

/// Close out a turn for the UI loop: the context gauge first (those events are
/// only sent around turns, so `/compact` and the end of a turn must refresh
/// it), then the reason that clears the busy state. Returns `false` when the
/// loop is gone.
fn finish_turn(
    events: &mpsc::UnboundedSender<AgentEvent>,
    cfg: &Arc<Config>,
    history: &History,
    reason: EndReason,
) -> bool {
    let _ = events.send(AgentEvent::Core(CoreEvent::Usage(
        kloop_core::agent::context_estimate(cfg, history),
    )));
    events
        .send(AgentEvent::Core(CoreEvent::TurnEnded(reason)))
        .is_ok()
}

fn send_command_result_events(
    events: &mpsc::UnboundedSender<AgentEvent>,
    result: &kloop_core::commands::SlashResult,
    context_used: u64,
) -> bool {
    if !result.output.is_empty()
        && events
            .send(AgentEvent::System(result.output.clone()))
            .is_err()
    {
        return false;
    }
    // The footer's context gauge only moves on a Usage event, and those are
    // sent around turns. `/compact` rewrites History without one,
    // so the gauge kept quoting the size of a conversation that no longer
    // existed until the next turn ended.
    events
        .send(AgentEvent::Core(CoreEvent::Usage(context_used)))
        .is_ok()
}

/// The worker's session handles and the channels it talks through.
struct Worker {
    cfg: Arc<Config>,
    /// Where the worker publishes each new session's Config (see `run`).
    current: Arc<std::sync::Mutex<Arc<Config>>>,
    ui: Arc<dyn Ui>,
    events: mpsc::UnboundedSender<AgentEvent>,
    /// The build stamp, for the banner a `/clear` opens the new session with.
    version: String,
}

/// What the UI mirrors of the session `cfg` now runs: its gate's mode and its
/// working directory are the base ones, since a new session starts outside any
/// worktree.
fn session_switch(cfg: &Config, report: Vec<String>) -> events::SessionSwitch {
    let workspace = cfg.effective_workspace();
    events::SessionSwitch {
        session_id: cfg.session_id.clone(),
        route: cfg.provider_route.public_route(),
        context_window: cfg.context_window,
        mode: workspace.permissions.mode(),
        cwd: display_cwd(&workspace.cwd),
        branch: git_branch(&workspace.cwd),
        todos: cfg.todos.snapshot(),
        report,
    }
}

/// Owns History for its whole lifetime and runs turns strictly one at a time;
/// the UI loop enforces single-flight by ignoring Enter while running.
async fn agent_worker(
    worker: Worker,
    mut history: History,
    mut msgs: mpsc::UnboundedReceiver<WorkerMsg>,
    // `--image` blocks ride the first user turn; taken once, then empty.
    mut pending_images: Vec<ContentBlock>,
) {
    let Worker {
        mut cfg,
        current,
        ui,
        events,
        version,
    } = worker;
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
                let _ = events.send(AgentEvent::RouteFrozen {
                    route: cfg.provider_route.public_route(),
                    context_window: cfg.context_window,
                });
                // The composer's images ride the turn they were sent with and
                // lead, in the order their `[Image #N]`s appear in the text;
                // `--image` blocks ride the first turn after them.
                let mut images = turn.images;
                images.append(&mut pending_images);
                let msg = if images.is_empty() {
                    Message::user_text(turn.text)
                } else {
                    Message::user_with_blocks(turn.text, images)
                };
                let (outcome, returned) =
                    run_turn_with_input(&cfg, &mut history, &ui, &turn.cancel, 0, msg).await;
                // Interrupted before the model said anything: the turn is not in
                // history and not in the session file, so the transcript must
                // drop it too and the text goes back to the composer to be
                // fixed. `--image` blocks that rode this turn ride the next one.
                if let Some(input) = returned {
                    let (text, images) = split_user_input(input);
                    let _ = events.send(AgentEvent::InputReturned { text, images });
                }
                if !finish_turn(&events, &cfg, &history, outcome.reason) {
                    return;
                }
            }
            WorkerMsg::Wake { cancel } => {
                let _ = events.send(AgentEvent::RouteFrozen {
                    route: cfg.provider_route.public_route(),
                    context_window: cfg.context_window,
                });
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
                if !finish_turn(&events, &cfg, &history, outcome.reason) {
                    return;
                }
            }
            WorkerMsg::Command { line, cancel } => {
                let _ = events.send(AgentEvent::RouteFrozen {
                    route: cfg.provider_route.public_route(),
                    context_window: cfg.context_window,
                });
                let result = kloop_core::commands::run_with_provider_state(
                    &line,
                    &mut history,
                    &cfg,
                    &provider_state,
                    ui.as_ref(),
                    &cancel,
                )
                .await;
                if result.route_changed {
                    let route = provider_state.active_route();
                    cfg = Arc::new(cfg.clone_with_provider_route(provider_state.freeze()));
                    *current.lock().unwrap() = Arc::clone(&cfg);
                    let _ = events.send(AgentEvent::ProviderChanged {
                        route,
                        context_window: cfg.context_window,
                    });
                }
                if result.new_session {
                    let event = match kloop_core::commands::start_fresh_session(&cfg, &ui).await {
                        Ok(fresh) => {
                            // The old History goes here; a session that never
                            // got past its opening line removes its own file.
                            history = fresh.history;
                            cfg = Arc::new(fresh.cfg);
                            provider_state =
                                kloop_core::provider_route::SessionProviderState::from_timeline(
                                    Arc::clone(&cfg.provider_catalog),
                                    history.provider_routes(),
                                )
                                .expect("a new session's route timeline is its current route");
                            *current.lock().unwrap() = Arc::clone(&cfg);
                            AgentEvent::Cleared {
                                session: session_switch(&cfg, fresh.report),
                                version: version.clone(),
                            }
                        }
                        Err(error) => AgentEvent::System(format!("clear failed: {error:#}")),
                    };
                    if events.send(event).is_err() {
                        return;
                    }
                }
                if !send_command_result_events(
                    &events,
                    &result,
                    kloop_core::agent::context_estimate(&cfg, &history),
                ) {
                    return;
                }
                if let Some(stage) = result.open_picker
                    && events
                        .send(AgentEvent::ProviderPicker(
                            provider_state.route_picker(stage),
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
                    if !finish_turn(&events, &cfg, &history, outcome.reason) {
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
            WorkerMsg::Fork {
                seq,
                abandoned,
                cancel,
            } => {
                if !rewind(
                    seq,
                    abandoned,
                    &cancel,
                    &mut cfg,
                    &mut history,
                    &mut provider_state,
                    &current,
                    &ui,
                    &events,
                )
                .await
                {
                    return;
                }
                // Only a summarizing rewind made the UI busy.
                let busy = match abandoned {
                    AbandonedBranch::Drop => false,
                    AbandonedBranch::Summarize => true,
                };
                if busy
                    && events
                        .send(AgentEvent::Core(CoreEvent::TurnEnded(EndReason::Completed)))
                        .is_err()
                {
                    return;
                }
            }
        }
    }
}

/// Rewind onto the fork cut at `seq`. The branch is a new session file, so it
/// runs as a new session — new id, new session state — exactly like `/clear`,
/// except that History keeps the branch's messages. `false` means the UI loop
/// is gone.
#[allow(clippy::too_many_arguments)]
async fn rewind(
    seq: u64,
    abandoned: AbandonedBranch,
    cancel: &CancellationToken,
    cfg: &mut Arc<Config>,
    history: &mut History,
    provider_state: &mut kloop_core::provider_route::SessionProviderState,
    current: &std::sync::Mutex<Arc<Config>>,
    ui: &Arc<dyn Ui>,
    events: &mpsc::UnboundedSender<AgentEvent>,
) -> bool {
    // A failed or cancelled rewind leaves History and the session untouched;
    // report and carry on the original branch.
    let rewound = match kloop_core::rewind::rewind(cfg, history, seq, abandoned, ui, cancel).await {
        Ok(RewindOutcome::Rewound(rewound)) => rewound,
        Ok(RewindOutcome::Cancelled) => {
            return events
                .send(AgentEvent::System(
                    "rewind cancelled — still on the same session".into(),
                ))
                .is_ok();
        }
        Err(e) => {
            return events
                .send(AgentEvent::System(format!("rewind failed: {e:#}")))
                .is_ok();
        }
    };
    *cfg = Arc::new(rewound.cfg);
    *provider_state = kloop_core::provider_route::SessionProviderState::from_timeline(
        Arc::clone(&cfg.provider_catalog),
        history.provider_routes(),
    )
    .expect("rewound provider timeline was validated on recovery");
    *current.lock().unwrap() = Arc::clone(cfg);
    let forked = AgentEvent::Forked {
        session: session_switch(cfg, rewound.report),
        messages: history.messages().to_vec(),
    };
    if events.send(forked).is_err() {
        return false;
    }
    rewound
        .notes
        .into_iter()
        .all(|note| events.send(AgentEvent::System(note)).is_ok())
}

type Terminal = ratatui::Terminal<
    PinnedBackend<ratatui::backend::CrosstermBackend<FrameWriter<std::io::Stdout>>>,
>;

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
        // Hand over anything the last frame left buffered before the direct
        // stdout writes in `restore_terminal` reorder themselves ahead of it.
        let _ = self.terminal.backend_mut().commit_frame();
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
    // (cursor-position query) is the only one the TUI sends: it runs before the
    // input thread starts, so nothing races it for stdin, and a timeout falls
    // back to the origin. Later queries answer from the last position sent.
    let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
    // Every byte of a frame goes through one `FrameWriter` (plan 103): ratatui-
    // crossterm flushes on each `execute!`, so without it a frame reaches the
    // terminal in pieces — and a commit's viewport clear as a piece of its own,
    // leaving the screen blank until the repaint lands.
    let frames = FrameWriter::new(std::io::stdout());
    let backend = PinnedBackend::with_frames(
        ratatui::backend::CrosstermBackend::new(frames.clone()),
        frames,
    );
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
/// so the internal reader lock is released between polls and the thread observes
/// `stop` every 200ms. (The lock used to matter for a resize's cursor-position
/// query; after the viewport's construction probe, `PinnedBackend` answers from
/// the last position it sent, so a resize no longer queries at all.) Idle
/// polling stays cheap.
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

/// Freeze whatever of the live tail overflows the last successfully drawn live
/// region into native scrollback, then drop the cells that left whole from the
/// app's tail. `plan` is the frame that draw produced and `viewport` its
/// geometry; committing from a pre-draw size probe would race Ratatui's own
/// autoresize and could irreversibly freeze the wrong prefix.
///
/// Cells leave whole where they may; the new head then freezes its settled
/// lines down to exactly the overflow ([`render::freeze_target`]). The lines
/// written are the ones the viewport shows ([`render::FramePlan`]), so the
/// scrollback continues seamlessly into the screen.
fn commit_overflow<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
    app: &mut App,
    plan: &render::FramePlan,
    viewport: Rect,
) -> std::result::Result<bool, B::Error>
where
    B: ratatui::backend::Backend,
{
    let width = plan.width;
    let frozen = app.head_skip(width);
    let (n, head) = overflow_freeze_target(app, plan, viewport);
    // Everything to write is taken from the plan before `drain_committed`,
    // which forgets the head's frozen prefix.
    let mut blocks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut already = frozen;
    for cell in plan.shown.iter().take(n) {
        blocks.push(cell.lines.iter().skip(already).cloned().collect());
        already = 0;
    }
    if let Some(cell) = plan.shown.get(n)
        && head > already
    {
        blocks.push(cell.lines[already..head].to_vec());
    }
    if blocks.is_empty() {
        return Ok(false);
    }
    app.drain_committed(n);
    if head > 0 {
        app.freeze_head_lines(width, head);
    }
    insert_scrollback_blocks(terminal, blocks)?;
    Ok(true)
}

fn overflow_freeze_target(app: &App, plan: &render::FramePlan, viewport: Rect) -> (usize, usize) {
    let cells: Vec<render::Freezable> = plan
        .shown
        .iter()
        .map(render::ShownCell::freezable)
        .collect();
    render::freeze_target(&cells, app.head_skip(plan.width), plan.active_h(viewport))
}

/// Draw one frame and hand it to the terminal as a single synchronized write.
/// Bytes a draw leaves in the [`FrameWriter`] are not on screen until the
/// handover, and a handover in the middle of a repaint is exactly the flicker
/// plan 103 removed — so the one draw that is not handed over at once is the
/// first in [`draw_frame`], which joins the commit's frame.
fn draw_and_hand_over<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
    app: &mut App,
    hud: &render::Hud,
    cache: &mut render::ShownCache,
) -> std::result::Result<Rect, B::Error>
where
    B: ratatui::backend::Backend,
{
    let (area, _plan) = draw_with_plan(terminal, app, hud, cache)?;
    terminal.backend_mut().commit_frame()?;
    Ok(area)
}

/// Draw one frame through a freshly computed [`render::FramePlan`]. The plan is
/// built from the frame's own area, so it can never wrap for a geometry the
/// terminal did not give this draw.
fn draw_with_plan<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
    app: &mut App,
    hud: &render::Hud,
    cache: &mut render::ShownCache,
) -> std::result::Result<(Rect, render::FramePlan), B::Error>
where
    B: ratatui::backend::Backend,
{
    let mut plan = None;
    terminal.draw(|frame| {
        let planned = render::FramePlan::new(app, frame.area(), hud, cache);
        render::draw(frame, app, hud, &planned);
        plan = Some(planned);
    })?;
    let area = terminal.get_frame().area();
    Ok((
        area,
        plan.expect("the draw closure plans the frame it draws"),
    ))
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
    cache: &mut render::ShownCache,
) -> std::result::Result<Rect, B::Error>
where
    B: ratatui::backend::Backend,
{
    for retry in 0..=1 {
        // Held rather than handed over: a frame that overflows is clipped at
        // the top until the commit below moves the overflow to scrollback, so
        // it reaches the terminal together with that commit and its repaint,
        // as one frame — handed over alone, it would show the tail for a moment
        // with its top on no screen and in no scrollback (plan 215).
        let (drawn_viewport, plan) = draw_with_plan(terminal, app, hud, cache)?;
        // Only the completion menu holds the commit: it floats over the
        // transcript, outside the height budget. The panels — approvals,
        // questions, pickers — are in that budget (plan 104), so what they
        // push off the top belongs in scrollback like anything else; held back,
        // it would be neither on screen nor there (plan 215).
        if app.popup.is_some() {
            terminal.backend_mut().commit_frame()?;
            return Ok(drawn_viewport);
        }

        terminal.backend_mut().pin_current_size()?;
        let attempt: std::result::Result<Option<Rect>, B::Error> = (|| {
            terminal.autoresize()?;
            let confirmed_viewport = terminal.get_frame().area();
            if confirmed_viewport != drawn_viewport {
                return Ok(None);
            }
            // The commit tears the viewport down and the repaint puts it back
            // inside one handover, so the screen never shows the gap (plan 103).
            if commit_overflow(terminal, app, &plan, confirmed_viewport)? {
                draw_and_hand_over(terminal, app, hud, cache)?;
            } else {
                terminal.backend_mut().commit_frame()?;
            }
            Ok(Some(terminal.get_frame().area()))
        })();
        terminal.backend_mut().unpin_size();

        match attempt? {
            Some(viewport) => return Ok(viewport),
            None if retry == 0 => continue,
            None => return draw_and_hand_over(terminal, app, hud, cache),
        }
    }
    unreachable!("bounded geometry retry loop always returns")
}

/// Take a returned user message apart: the text as it was sent (its
/// `[Image #N]` placeholders included), and the images in message order — the
/// composer's in placeholder order, then any `--image` blocks.
fn split_user_input(input: Message) -> (String, Vec<ImageSource>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in input.content {
        match block {
            ContentBlock::Text { text: part } => text.push_str(&part),
            ContentBlock::Image { source } => images.push(source),
            _ => {}
        }
    }
    (text, images)
}

/// The image file a bracketed paste names, when it names one. Terminals paste a
/// dropped file as its (often quoted) path. The check is cheap — no I/O — so the
/// loop can decide before scheduling the read.
fn image_paste_path(s: &str) -> Option<std::path::PathBuf> {
    let path = s.trim().trim_matches(['\'', '"']).trim();
    if path.is_empty() || path.contains('\n') {
        return None;
    }
    let p = std::path::Path::new(path);
    let ext = p.extension()?.to_str()?.to_ascii_lowercase();
    matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp").then(|| p.to_path_buf())
}

/// Read and validate an image file for a pasted path (blocking; the loop runs
/// it on the blocking pool).
fn load_image_paste(path: &std::path::Path) -> Option<ImageSource> {
    let bytes = std::fs::read(path).ok()?;
    clipboard::image_source_from_bytes(&bytes).ok()
}

/// Schedule one `@` completion walk for `target` and send its result to
/// `tasks`. The debounce coalesces a burst of keystrokes into one walk, and a
/// query superseded before the walk starts skips it entirely; either way the
/// event loop never awaits the walk (a full tree read for every key used to
/// stall the spinner and whatever was streaming in).
fn spawn_file_search(
    cwd: std::path::PathBuf,
    target: menu::CompletionTarget,
    seq: u64,
    popup_generation: u64,
    latest: Arc<std::sync::atomic::AtomicU64>,
    tasks: mpsc::UnboundedSender<UiTask>,
) {
    let query = target.query.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(FILE_SEARCH_DEBOUNCE_MS)).await;
        if latest.load(Ordering::Relaxed) != seq {
            return;
        }
        let paths = tokio::task::spawn_blocking(move || {
            kloop_core::fs_complete::complete_files(&cwd, &query, menu::FILE_MENU_MAX)
        })
        .await
        .unwrap_or_default();
        let _ = tasks.send(UiTask::FileSearch {
            seq,
            popup_generation,
            target,
            paths,
        });
    });
}

/// Whether a landed `@` search still belongs to the state that asked for it:
/// a later query has not superseded it, and the user has not dismissed the menu
/// it was meant to fill. The token itself may be unchanged across Esc, so the
/// target check in `set_file_results` cannot decide this alone.
fn search_lands(seq: u64, latest: u64, generation: u64, current_generation: u64) -> bool {
    seq == latest && generation == current_generation
}

/// Whether this key is the submit that has to wait for an image still loading.
/// Enter on a popup or a panel answers that surface and is never held; a
/// newline chord is not a submit either.
fn enter_waits_for_paste(
    pending_pastes: usize,
    app: &App,
    key: &crossterm::event::KeyEvent,
) -> bool {
    use crossterm::event::KeyModifiers;
    pending_pastes > 0
        && key.code == crossterm::event::KeyCode::Enter
        && !key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
        && app.popup.is_none()
        && !app.interaction_active()
        && app.fork_picker.is_none()
        && app.provider_picker.is_none()
}

/// Autowake (plan 26) fires only when the agent is idle AND a reinjection is
/// waiting AND no startup task still owns the keyboard: a running turn drains
/// the inbox at its own round boundary (firing then would double-deliver), an
/// empty inbox means there is nothing to wake for, and a pending startup would
/// run the first turn against a half-known tool catalog. Kept as a pure function
/// so the delivery invariant is unit-tested rather than only exercised through
/// the live terminal loop.
fn autowake_ready(running: bool, startup_pending: bool, inbox: &Inbox) -> bool {
    !running && !startup_pending && !inbox.is_empty()
}

fn dispatch_autowake(
    app: &mut App,
    inbox: &Inbox,
    msgs: &mpsc::UnboundedSender<WorkerMsg>,
) -> Option<CancellationToken> {
    if !autowake_ready(app.running, app.startup.is_some(), inbox) {
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

/// The worktree tool reports `branch: None` when the session leaves it, but the
/// footer shows the session's branch and must keep showing the original
/// checkout's. Re-read it at the new cwd rather than changing the core event
/// shape, which the worktree parity report pins. A `git` process is blocking, so
/// this runs on the blocking pool and its result lands as [`UiTask::Branch`].
fn resolve_cwd_branch(cwd: &std::path::Path) -> Option<String> {
    git_branch(cwd)
}

/// Work the UI loop used to do inline. A full tree walk (`@` completion), an OS
/// clipboard read with PNG encoding, a dropped image file's read, and a `git`
/// branch lookup are all blocking calls; run on the loop they stalled the
/// spinner and anything streaming in. Each result comes back here and is applied
/// only if it still matches the state that asked for it.
enum UiTask {
    /// `@` completion results; `seq` is the newest search the loop asked for,
    /// `popup_generation` the menu state it was asked in.
    FileSearch {
        seq: u64,
        popup_generation: u64,
        target: menu::CompletionTarget,
        paths: Vec<String>,
    },
    /// Ctrl+V: an image, or why there was none.
    Clipboard {
        snapshot: PasteSnapshot,
        image: std::result::Result<ImageSource, String>,
    },
    /// A pasted path: the attachment, or None when it did not read — then the
    /// paste goes into the composer as text.
    ImagePaste {
        snapshot: PasteSnapshot,
        text: String,
        source: Option<ImageSource>,
    },
    /// A cwd change that arrived without its branch.
    Branch { cwd: String, branch: Option<String> },
}

/// The draft a paste was aimed at. Both reads run off the event loop, so the
/// result has to find the place the user pasted into — or learn that the draft
/// is gone rather than land in the next message.
#[derive(Clone, Debug)]
struct PasteSnapshot {
    /// [`App::draft_generation`] when the paste was asked for.
    generation: u64,
    /// The composer's text then; a different text only means the draft was
    /// edited, but the recorded seam may no longer exist.
    text: String,
    /// The cursor then, in expanded-text bytes.
    at: crate::text_layout::ByteOffset,
}

/// How long `@` typing settles before a tree walk starts: one walk per burst of
/// keystrokes instead of one per key.
const FILE_SEARCH_DEBOUNCE_MS: u64 = 60;

/// What the loop starts with: the seeded [`App`] and the startup task's line
/// channel (whose sender dropping means the task finished).
struct UiBoot {
    app: App,
    startup_line: watch::Receiver<Option<String>>,
}

/// What the event loop owns next to the pure [`App`]: the handles a key command
/// acts on, the cancel token of whatever is in flight, and the wall clocks the
/// animated HUD reads (the `App` has no clock of its own).
struct UiState {
    app: App,
    cwd: std::path::PathBuf,
    msgs: mpsc::UnboundedSender<WorkerMsg>,
    /// The worker's current Config; `inbox` and `permissions` are re-read from
    /// it whenever the worker switches sessions.
    session: Arc<std::sync::Mutex<Arc<Config>>>,
    inbox: Arc<Inbox>,
    permissions: Arc<kloop_core::permissions::Permissions>,
    /// The worker switched sessions since the loop last looked.
    session_switched: bool,
    /// `/clear` asked for a blank terminal before the next draw.
    clear_terminal: bool,
    current_cancel: Option<CancellationToken>,
    turn_started: Option<Instant>,
    thinking_started: Option<Instant>,
    /// Results of work started off the loop (see [`UiTask`]).
    tasks: mpsc::UnboundedSender<UiTask>,
    /// The newest `@` query the loop asked for; an older walk that lands is
    /// dropped, and one still sleeping when a key moves the query on skips its
    /// walk entirely.
    file_search_seq: Arc<std::sync::atomic::AtomicU64>,
    /// Pastes whose image read is still in flight. Enter waits for them so the
    /// image rides the message the user sent.
    pending_pastes: usize,
    /// The Enter that arrived while a paste was in flight; replayed once the
    /// composer has the attachment.
    deferred_enter: Option<(crossterm::event::KeyEvent, usize)>,
}

impl UiState {
    /// Reconcile the turn clock with the app's running state (it starts on the
    /// first frame of a turn and clears when the turn ends), then read both
    /// clocks into this frame's HUD.
    fn hud(&mut self, reduced_motion: bool) -> render::Hud {
        if self.app.running {
            self.turn_started.get_or_insert_with(Instant::now);
        } else {
            self.turn_started = None;
            self.thinking_started = None;
        }
        let elapsed = self.turn_started.map(|t| t.elapsed());
        render::Hud {
            elapsed,
            thinking: self.thinking_started.map(|t| t.elapsed()),
            phase: elapsed
                .map(|d| (d.as_millis() as u64 / anim::STEP_MS) as usize)
                .unwrap_or(0),
            reduced_motion,
        }
    }

    /// Autowake (plan 26): a background sub-agent left a result in the inbox
    /// while the agent sits idle. Start a turn to deliver it without waiting for
    /// the user. The readiness guard also catches the race where a reinjection
    /// lands just after a turn ends.
    fn autowake(&mut self) {
        if let Some(cancel) = dispatch_autowake(&mut self.app, &self.inbox, &self.msgs) {
            self.current_cancel = Some(cancel);
        }
    }

    /// Start an `@` completion walk off the loop. Only the newest query walks: a
    /// burst of keystrokes gets one walk after the debounce, and a walk already
    /// running is never awaited by the loop — its result arrives as a
    /// [`UiTask`] and is dropped if the query moved on.
    fn search_files(&mut self, target: menu::CompletionTarget) {
        let seq = self.file_search_seq.fetch_add(1, Ordering::Relaxed) + 1;
        spawn_file_search(
            self.cwd.clone(),
            target,
            seq,
            self.app.popup_generation(),
            Arc::clone(&self.file_search_seq),
            self.tasks.clone(),
        );
    }

    /// What a paste read off the loop has to remember: which draft it was asked
    /// for, and where in it the attachment belongs.
    fn paste_snapshot(&self) -> PasteSnapshot {
        PasteSnapshot {
            generation: self.app.draft_generation(),
            text: self.app.composer.text().to_string(),
            at: self.app.composer.cursor(),
        }
    }

    /// Ctrl+V: reading the OS clipboard (arboard) and PNG-encoding a screenshot
    /// are both blocking.
    fn read_clipboard(&mut self) {
        let snapshot = self.paste_snapshot();
        self.pending_pastes += 1;
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            // The wrapper keeps a panicking read from stranding the count, and
            // with it the Enter waiting on it.
            let image = tokio::task::spawn_blocking(clipboard::clipboard_image)
                .await
                .unwrap_or_else(|error| Err(format!("clipboard read failed: {error}")));
            let _ = tasks.send(UiTask::Clipboard { snapshot, image });
        });
    }

    /// A pasted path may name an image: read it off the loop, and if it does not
    /// read, paste the text as ordinary input.
    fn read_image_paste(&mut self, text: String, path: std::path::PathBuf) {
        let snapshot = self.paste_snapshot();
        self.pending_pastes += 1;
        let tasks = self.tasks.clone();
        tokio::spawn(async move {
            let source = tokio::task::spawn_blocking(move || load_image_paste(&path))
                .await
                .ok()
                .flatten();
            let _ = tasks.send(UiTask::ImagePaste {
                snapshot,
                text,
                source,
            });
        });
    }

    /// Put a pasted attachment where it was asked for: at the recorded seam when
    /// the draft still has one, at the cursor when the draft was edited under
    /// it, and nowhere when that draft was submitted or cleared meanwhile — the
    /// image must not turn up in the next message. A dropped paste says so.
    fn attach_pasted_image(&mut self, snapshot: PasteSnapshot, source: ImageSource) {
        if self.app.draft_generation() != snapshot.generation {
            self.note(
                "pasted image arrived after that draft was sent or cleared — not attached".into(),
            );
            return;
        }
        let source = match self.app.attach_image_at(snapshot.at, source) {
            Ok(()) => return,
            Err(source) => source,
        };
        // The seam is gone (an edit crossed it, or an atom sits there now): the
        // cursor is the one place the user can still see the attachment land.
        if self.app.composer.text() != snapshot.text {
            self.note(
                "pasted image attached at the cursor — the draft changed while it loaded".into(),
            );
        }
        self.app.attach_image(source);
    }

    /// One dim transcript note, the same channel every other loop-side notice
    /// uses.
    fn note(&mut self, text: String) {
        self.app.apply(AgentEvent::Core(CoreEvent::Note(text)));
    }

    /// A paste result landed: count it in, write it where it was asked for, and
    /// replay the Enter that waited for it once nothing else is in flight.
    fn settle_paste(&mut self) -> ControlFlow<()> {
        self.pending_pastes = self.pending_pastes.saturating_sub(1);
        if self.pending_pastes > 0 {
            return ControlFlow::Continue(());
        }
        if let Some((key, width)) = self.deferred_enter.take() {
            let command = self.app.on_key(width, key);
            return self.on_command(command);
        }
        ControlFlow::Continue(())
    }

    /// A cwd change arrived without its branch (the worktree tool leaving a
    /// tree): ask `git` off the loop.
    fn resolve_branch(&self, cwd: String) {
        let tasks = self.tasks.clone();
        tokio::task::spawn_blocking(move || {
            let branch = resolve_cwd_branch(std::path::Path::new(&cwd));
            let _ = tasks.send(UiTask::Branch { cwd, branch });
        });
    }

    /// Paste text into the composer — or, when its tail opens an `@` token,
    /// start the completion search.
    fn paste(&mut self, text: &str) {
        if let Command::SearchFiles(target) = self.app.paste_text(text) {
            self.search_files(target);
        }
    }

    /// Deliver one off-loop result. A stale one — a moved-on `@` query, a menu
    /// the user dismissed, a draft that was sent, a branch for a directory the
    /// session already left — is dropped.
    fn on_task(&mut self, task: UiTask) -> ControlFlow<()> {
        match task {
            UiTask::FileSearch {
                seq,
                popup_generation,
                target,
                paths,
            } => {
                if search_lands(
                    seq,
                    self.file_search_seq.load(Ordering::Relaxed),
                    popup_generation,
                    self.app.popup_generation(),
                ) {
                    self.app.set_file_results(target, paths);
                }
            }
            UiTask::Clipboard { snapshot, image } => {
                match image {
                    Ok(source) => self.attach_pasted_image(snapshot, source),
                    Err(error) => self.note(error),
                }
                return self.settle_paste();
            }
            UiTask::ImagePaste {
                snapshot,
                text,
                source,
            } => {
                match source {
                    Some(source) => self.attach_pasted_image(snapshot, source),
                    // Not an image after all: the paste is plain text, and it
                    // belongs to the draft it was pasted into just the same.
                    None if self.app.draft_generation() == snapshot.generation => {
                        self.paste(&text);
                    }
                    None => self.note(
                        "paste arrived after that draft was sent or cleared — not inserted".into(),
                    ),
                }
                return self.settle_paste();
            }
            UiTask::Branch { cwd, branch } => self.app.fill_branch(&cwd, branch),
        }
        ControlFlow::Continue(())
    }

    /// One terminal input event. `Break` ends the loop (the input channel died).
    fn on_input(&mut self, input: Option<Event>, viewport_width: usize) -> ControlFlow<()> {
        // A startup task (MCP handshakes) owns the keyboard until it settles:
        // no turn and no slash command may run against a half-known tool
        // catalog. The quit chords stay live so a hung server is escapable.
        if self.app.startup.is_some() {
            return match input {
                None => ControlFlow::Break(()),
                Some(Event::Key(k))
                    if k.kind != KeyEventKind::Release && startup_allows_key(&k) =>
                {
                    let command = self.app.on_key(viewport_width, k);
                    self.on_command(command)
                }
                _ => ControlFlow::Continue(()),
            };
        }
        match input {
            Some(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                // A paste is still loading its image: Enter waits for it, so the
                // image rides the message the user just sent instead of landing
                // in the next draft.
                if enter_waits_for_paste(self.pending_pastes, &self.app, &k) {
                    self.deferred_enter = Some((k, viewport_width));
                    return ControlFlow::Continue(());
                }
                let command = self.app.on_key(viewport_width, k);
                self.on_command(command)
            }
            // Bracketed paste (plan 38 slice 3): a dragged/pasted image-file
            // path attaches as an image, anything else goes to the composer
            // (a large paste collapses to a placeholder there).
            Some(Event::Paste(s)) if self.app.question_editor_active() => {
                let _ = self.app.paste_text(&s);
                ControlFlow::Continue(())
            }
            Some(Event::Paste(s)) => {
                match image_paste_path(&s) {
                    Some(path) => self.read_image_paste(s, path),
                    None => self.paste(&s),
                }
                ControlFlow::Continue(())
            }
            // Resize repositions the viewport (handled by the autoresize at
            // the top of the loop); any other event just needs a redraw.
            Some(_) => ControlFlow::Continue(()),
            None => ControlFlow::Break(()),
        }
    }

    /// One command the key handler produced. `Break` ends the loop.
    fn on_command(&mut self, command: Command) -> ControlFlow<()> {
        match command {
            Command::Submit(text) => {
                let cancel = CancellationToken::new();
                self.current_cancel = Some(cancel.clone());
                // The images live on the App, not on the Command; take them here.
                let images = self
                    .app
                    .take_submit_images()
                    .into_iter()
                    .map(|source| ContentBlock::Image { source })
                    .collect();
                let _ = self.msgs.send(WorkerMsg::Turn(Turn {
                    text,
                    images,
                    cancel,
                }));
            }
            Command::Slash(line) => {
                // Runs on the worker (owns History); its cancel lets
                // Ctrl+C interrupt a slow /compact like a turn.
                let cancel = CancellationToken::new();
                self.current_cancel = Some(cancel.clone());
                let _ = self.msgs.send(WorkerMsg::Command { line, cancel });
            }
            Command::Steer(text) => {
                // Enqueue for the running turn; the agent loop drains it at the
                // next round boundary. The user's raw text already showed as a
                // User cell.
                let images = self.app.take_submit_images();
                self.inbox.push(InboxItem::Steer { text, images });
            }
            Command::SetMode(mode) => {
                // shift+Tab: apply the new mode to the shared gate; subsequent
                // tool calls read it live. The badge is already updated in the
                // App.
                self.permissions.set_mode(mode);
            }
            Command::RequestForkPoints => {
                // The worker owns the rollout path; it reads the fork targets
                // and replies with a ForkPoints event.
                let _ = self.msgs.send(WorkerMsg::ListForkPoints);
            }
            Command::Fork { seq, abandoned } => {
                let cancel = CancellationToken::new();
                self.current_cancel = Some(cancel.clone());
                let _ = self.msgs.send(WorkerMsg::Fork {
                    seq,
                    abandoned,
                    cancel,
                });
            }
            Command::Interrupt => {
                if let Some(cancel) = &self.current_cancel {
                    cancel.cancel();
                }
            }
            Command::PasteClipboardImage => {
                // Read the OS clipboard off the loop (a side effect); the
                // result attaches an image or notes why there was none.
                self.read_clipboard();
            }
            Command::SearchFiles(target) => {
                self.search_files(target);
            }
            Command::Quit => return ControlFlow::Break(()),
            Command::None => {}
        }
        ControlFlow::Continue(())
    }

    /// One agent event plus whatever else already arrived (streaming deltas come
    /// in bursts), so the loop redraws once per batch and not per token.
    fn on_agent_events(
        &mut self,
        first: AgentEvent,
        events: &mut mpsc::UnboundedReceiver<AgentEvent>,
    ) -> ControlFlow<()> {
        // Snapshot before applying: a thinking block that was streaming and is
        // no longer gets its elapsed stamped into the sealed cell.
        let was_thinking = self.app.streaming_thinking();
        let mut quit = self.absorb(first);
        while let Ok(event) = events.try_recv() {
            quit |= self.absorb(event);
        }
        if quit {
            return ControlFlow::Break(());
        }
        // Time the thinking block: start the clock when it opens, seal the cell
        // with its final elapsed when it closes.
        if self.app.streaming_thinking() {
            self.thinking_started.get_or_insert_with(Instant::now);
        } else if was_thinking && let Some(t) = self.thinking_started.take() {
            self.app.seal_thinking(t.elapsed().as_secs());
        }
        // Same for the turn itself: stamp its elapsed onto the transcript the
        // moment it stops running — the top of the loop drops the clock on the
        // next iteration, and by then the time is gone.
        if !self.app.running
            && let Some(t) = self.turn_started.take()
        {
            self.app.seal_turn(t.elapsed().as_secs());
        }
        self.autowake();
        ControlFlow::Continue(())
    }

    /// Project one agent event onto the app. `true` means it was `/exit`'s quit
    /// marker, which ends the loop and is never applied — the caller restores
    /// the terminal.
    fn absorb(&mut self, event: AgentEvent) -> bool {
        // A cwd change is the one event whose branch may arrive missing (the
        // worktree tool means "left the worktree"). Filling it needs a `git`
        // process, which would stall the loop here, so the footer picks it up
        // when the off-loop read answers (`App::fill_branch` drops it if the
        // session has moved on).
        if let AgentEvent::Core(CoreEvent::CwdChanged { cwd, branch: None }) = &event {
            self.resolve_branch(cwd.clone());
        }
        match &event {
            AgentEvent::Quit => return true,
            AgentEvent::Cleared { .. } => {
                self.session_switched = true;
                self.clear_terminal = true;
            }
            AgentEvent::Forked { .. } => self.session_switched = true,
            _ => {}
        }
        if let Some(next_cwd) = event_cwd(&event) {
            self.cwd = next_cwd;
        }
        self.app.apply(event);
        false
    }

    /// Point the loop's handles at the session the worker now runs. `true`
    /// when there was a switch, so the caller re-subscribes to the new inbox.
    fn adopt_session(&mut self) -> bool {
        if !std::mem::take(&mut self.session_switched) {
            return false;
        }
        let cfg = Arc::clone(&self.session.lock().unwrap());
        let workspace = cfg.effective_workspace();
        self.inbox = Arc::clone(&cfg.inbox);
        self.permissions = workspace.permissions;
        self.cwd = workspace.cwd;
        true
    }
}

/// Start the terminal over for a new session: erase the screen and the
/// scrollback above it, and make the next draw repaint the whole viewport.
fn clear_terminal<B>(
    terminal: &mut ratatui::Terminal<PinnedBackend<B>>,
) -> std::result::Result<(), B::Error>
where
    B: ratatui::backend::Backend,
{
    use ratatui::backend::Backend as _;
    let backend = terminal.backend_mut();
    backend.clear_region(ratatui::backend::ClearType::All)?;
    backend.purge_scrollback();
    // `Terminal::clear` would otherwise query the cursor, which can block on
    // the input thread's reader lock (see `PinnedBackend::begin_commit`).
    backend.begin_commit();
    let cleared = terminal.clear();
    terminal.backend_mut().end_commit();
    cleared
}

/// The only key a pending startup passes through: the quit chord. Ctrl+C is the
/// two-tap exit (armed and fired by `App::on_key`); a key dropped here never
/// reaches it. Ctrl+D is not bound anywhere, so it is not passed through.
fn startup_allows_key(key: &crossterm::event::KeyEvent) -> bool {
    key.modifiers
        .contains(crossterm::event::KeyModifiers::CONTROL)
        && matches!(key.code, crossterm::event::KeyCode::Char('c'))
}

async fn ui_loop(
    terminal: &mut Terminal,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    msgs: mpsc::UnboundedSender<WorkerMsg>,
    session: Arc<std::sync::Mutex<Arc<Config>>>,
    permissions: Arc<kloop_core::permissions::Permissions>,
    boot: UiBoot,
    cwd: std::path::PathBuf,
) -> Result<()> {
    let UiBoot {
        mut app,
        mut startup_line,
    } = boot;
    let inbox = Arc::clone(&session.lock().unwrap().inbox);
    // Seed the status-bar badge from the real starting mode (e.g. plan). The
    // App is built in `run` (transcript replay + `/` menu catalog); a resumed
    // session's cells start as the tail and scroll into scrollback on overflow.
    app.mode = permissions.mode();

    // Keys arrive from a dedicated poll thread (see `spawn_input_thread`).
    // Started after `setup_terminal`, so nothing raced the viewport's
    // construction CPR — the only cursor query that reaches the terminal.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let input_thread = spawn_input_thread(input_tx, stop.clone());

    let mut inbox_activity = inbox.subscribe_activity();
    // Rendered cells carried across frames, so a sealed answer above the
    // streaming one is not parsed again for every frame (plan 263).
    let mut render_cache = render::ShownCache::default();
    // Work that used to run inline (see `UiTask`) reports back here.
    let (task_tx, mut task_rx) = mpsc::unbounded_channel::<UiTask>();
    // Wall-clock timing for the animated HUD (plan 38 slice 5).
    let reduced_motion = anim::reduced_motion();
    let mut state = UiState {
        app,
        cwd,
        msgs,
        session,
        inbox,
        permissions,
        session_switched: false,
        clear_terminal: false,
        current_cancel: None,
        turn_started: None,
        thinking_started: None,
        tasks: task_tx,
        file_search_seq: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        pending_pastes: 0,
        deferred_enter: None,
    };
    let outcome = loop {
        if std::mem::take(&mut state.clear_terminal)
            && let Err(error) = clear_terminal(terminal)
        {
            break Err(error);
        }
        let hud = state.hud(reduced_motion);
        // Ratatui's draw owns autoresize. Commit only from the viewport of that
        // completed frame; if a commit clears it, `draw_frame` immediately
        // repaints and returns the final geometry used by key navigation.
        let viewport = match draw_frame(terminal, &mut state.app, &hud, &mut render_cache) {
            Ok(viewport) => viewport,
            Err(error) => break Err(error),
        };
        // Animation self-drives: while a turn runs (and no overlay owns the
        // screen), a frame tick wakes the loop to advance the spinner/elapsed;
        // idle, the tick is disabled so `select` blocks with zero CPU (the
        // FrameRequester role, played by tokio, plan 38 slice 5).
        let animating = state.app.running
            && state.app.interactions.is_empty()
            && state.app.fork_picker.is_none()
            && state.app.popup.is_none();
        let tick_ms = if reduced_motion { 1000 } else { anim::STEP_MS };
        let flow = tokio::select! {
            input = input_rx.recv() => state.on_input(input, usize::from(viewport.width)),
            task = task_rx.recv() => match task {
                Some(task) => state.on_task(task),
                // The sender lives on `state`, so this never closes.
                None => ControlFlow::Continue(()),
            },
            activity = inbox_activity.changed() => {
                if activity.is_err() {
                    ControlFlow::Break(())
                } else {
                    state.autowake();
                    ControlFlow::Continue(())
                }
            }
            event = events.recv() => match event {
                Some(event) => state.on_agent_events(event, &mut events),
                None => ControlFlow::Break(()),
            },
            // The startup task publishing its latest line, or releasing the
            // keyboard by dropping the sender when it finishes.
            line = startup_line.changed(), if state.app.startup.is_some() => {
                state.app.startup = match line {
                    Ok(()) => startup_line.borrow_and_update().clone(),
                    // The task finished or was aborted without a final line.
                    Err(_) => None,
                };
                if state.app.startup.is_none() {
                    // A scheduler fire or a background sub-agent may have filled
                    // the inbox while the gate held the keyboard; deliver it now
                    // instead of waiting for the next unrelated activity.
                    state.autowake();
                }
                ControlFlow::Continue(())
            },
            // Frame tick: only armed while animating, so an idle loop never wakes
            // here. Firing just redraws (elapsed/spinner advance at the top).
            _ = tokio::time::sleep(Duration::from_millis(tick_ms)), if animating => {
                ControlFlow::Continue(())
            }
        };
        if flow.is_break() {
            break Ok(());
        }
        if state.adopt_session() {
            inbox_activity = state.inbox.subscribe_activity();
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

    /// Draw one frame the way the event loop does — with a plan of its own and a
    /// cold render cache — for tests that only care where the frame landed.
    fn draw_frame_once<B>(terminal: &mut ratatui::Terminal<PinnedBackend<B>>, app: &mut App) -> Rect
    where
        B: Backend,
    {
        draw_frame(
            terminal,
            app,
            &render::Hud::default(),
            &mut render::ShownCache::default(),
        )
        .unwrap()
    }

    /// A drawn frame plans once: the draw and the overflow commit read the same
    /// cells, so a streaming answer is parsed a bounded number of times per
    /// frame instead of once per consumer (draw, commit, repaint) — and never
    /// proportional to its length (plan 263).
    #[test]
    fn a_drawn_frame_parses_the_streaming_answer_once() {
        use kloop_core::event::Delta;

        let backend = PinnedBackend::new(StagedSizeBackend::new(80, 24));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .unwrap();
        let mut app = App::new("parse-count".into());
        let answer = (0..1000)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        app.apply(AgentEvent::Core(CoreEvent::ItemDelta {
            id: "m".into(),
            delta: Delta::Text(answer),
        }));
        let hud = render::Hud::default();
        // The answer overflows, so each frame draws, commits and repaints: two
        // plans, one parse each. The old path parsed it per consumer.
        let mut cache = render::ShownCache::default();
        crate::markdown::take_parse_count();
        draw_frame(&mut terminal, &mut app, &hud, &mut cache).unwrap();
        let first = crate::markdown::take_parse_count();
        draw_frame(&mut terminal, &mut app, &hud, &mut cache).unwrap();
        let second = crate::markdown::take_parse_count();
        assert!(first <= 2, "first frame parsed {first} times");
        assert!(second <= 2, "later frame parsed {second} times");
    }

    /// The rows a draw at `viewport` leaves for the live tail.
    fn tail_rows(app: &App, viewport: Rect) -> usize {
        render::FramePlan::new(
            app,
            viewport,
            &render::Hud::default(),
            &mut render::ShownCache::default(),
        )
        .active_h(viewport)
    }

    #[test]
    fn banner_contracts_the_process_home() {
        let Some(home) = std::env::home_dir() else {
            return;
        };
        assert_eq!(display_cwd(&home), "~");
        assert_eq!(display_cwd(&home.join("repo")), "~/repo");
    }

    #[test]
    fn a_pending_startup_only_passes_the_quit_chords() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        for (code, modifiers, allowed) in [
            (KeyCode::Char('c'), KeyModifiers::CONTROL, true),
            (KeyCode::Char('d'), KeyModifiers::CONTROL, false),
            (KeyCode::Char('c'), KeyModifiers::NONE, false),
            (KeyCode::Enter, KeyModifiers::NONE, false),
            (KeyCode::Esc, KeyModifiers::NONE, false),
            (KeyCode::Char('x'), KeyModifiers::CONTROL, false),
        ] {
            assert_eq!(
                startup_allows_key(&KeyEvent::new(code, modifiers)),
                allowed,
                "{code:?} with {modifiers:?}"
            );
        }
    }

    /// The panic hook is process-global, so the two tests that install one take
    /// this lock: they must not see each other's hook (or restore over it).
    static PANIC_HOOK_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn dropping_the_guard_puts_the_previous_hook_back() {
        let _serial = PANIC_HOOK_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (events, mut quit) = mpsc::unbounded_channel();
        drop(PanicHookGuard::install(events));
        // A parallel panic while the hook was installed is not this test's; the
        // point is that nothing new arrives once the guard has dropped.
        while quit.try_recv().is_ok() {}
        let _ = std::panic::catch_unwind(|| panic!("hook probe"));
        assert!(
            quit.try_recv().is_err(),
            "the session hook was still installed after the guard dropped"
        );
    }

    /// Dropping the guard while unwinding must not call `set_hook`: std panics
    /// ("cannot modify the panic hook from a panicking thread") and a panic in a
    /// destructor during cleanup aborts, taking the terminal restore with it. A
    /// regression aborts this test binary, which is exactly the report.
    #[test]
    fn a_guard_dropped_while_unwinding_leaves_the_hook_alone() {
        let _serial = PANIC_HOOK_TESTS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (events, mut quit) = mpsc::unbounded_channel();
        let guard = PanicHookGuard::install(events);
        let slot = std::sync::Arc::clone(&guard.previous);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _guard = guard;
            panic!("hook probe");
        }));
        assert!(result.is_err());
        // The session hook ran for the probe (and chained the previous hook).
        assert!(quit.try_recv().is_ok(), "the session hook did not run");
        // Restore by hand what the guard skips during unwinding, so the rest of
        // the suite keeps its own hook.
        let previous = slot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .expect("the previous hook survived the probe");
        std::panic::set_hook(previous);
    }

    /// Leaving a worktree emits `branch: None`, but the footer's always-on
    /// branch must come back as the original checkout's, not disappear until
    /// the next session switch.
    #[test]
    fn leaving_a_worktree_refills_the_original_branch() {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "kloop-tui-branch-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        // The fixture owns every input Git can read: the host's global and
        // system config, its HOME/XDG lookup, its template directory and its
        // hooks. `hermetic_command` covers config sources; clearing the rest
        // here keeps a host with `commit.gpgSign = true`, a hook-bearing
        // `GIT_TEMPLATE_DIR`, or a poisoned `GIT_DIR` out of the seed commit.
        let home = root.join(".git-home");
        let xdg = home.join(".config");
        let hooks = home.join("hooks");
        std::fs::create_dir_all(&xdg).unwrap();
        std::fs::create_dir_all(&hooks).unwrap();
        let null_config = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let git = |args: &[&str]| {
            let mut command = kloop_core::git::hermetic_command(&root);
            command.args(args).env_clear();
            if let Some(path) = std::env::var_os("PATH") {
                command.env("PATH", path);
            }
            let output = command
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &xdg)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", null_config)
                .env("GIT_CONFIG_SYSTEM", null_config)
                .env("LC_ALL", "C")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "commit.gpgSign", "false"]);
        git(&["config", "core.hooksPath", hooks.to_str().unwrap()]);
        git(&["commit", "--allow-empty", "-qm", "base"]);
        let expected = git_branch(&root).expect("temp repo has a branch");
        assert_eq!(
            resolve_cwd_branch(&root).as_deref(),
            Some(expected.as_str()),
            "the off-loop lookup reads the branch the footer needs"
        );
        let mut app = App::new("branch-refill".into());
        app.apply(AgentEvent::Core(CoreEvent::CwdChanged {
            cwd: root.display().to_string(),
            branch: None,
        }));
        assert_eq!(app.branch, None, "the event itself carried no branch");
        // The lookup lands later: the footer gets the original branch back, and
        // the note the cwd change wrote is completed rather than duplicated.
        app.fill_branch(&root.display().to_string(), resolve_cwd_branch(&root));
        assert_eq!(app.branch.as_deref(), Some(expected.as_str()));
        assert_eq!(
            app.cells.last(),
            Some(&Cell::Note(format!(
                "working directory → {} (branch {expected})",
                root.display()
            )))
        );

        // A result for a directory the session has already left is dropped.
        app.fill_branch("/repo/.kloop/worktrees/x", Some("kloop-worktree-x".into()));
        assert_eq!(app.branch.as_deref(), Some(expected.as_str()));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Only a single-line path with an image extension is worth a read; the
    /// check itself must not touch the disk (it runs on the event loop).
    #[test]
    fn image_paste_paths_are_recognized_without_reading() {
        for (paste, expected) in [
            ("/tmp/shot.png", Some("/tmp/shot.png")),
            ("  '/tmp/shot.PNG'  ", Some("/tmp/shot.PNG")),
            ("/tmp/shot.gif", Some("/tmp/shot.gif")),
            ("/tmp/notes.txt", None),
            ("/tmp/shot.png\nsecond line", None),
            ("", None),
        ] {
            assert_eq!(
                image_paste_path(paste),
                expected.map(std::path::PathBuf::from),
                "{paste:?}"
            );
        }
    }

    /// `@` typing used to await a full tree walk per key. Now the walk is
    /// debounced and superseded queries never start one; the loop is free to
    /// process keys, ticks and agent events while it runs.
    #[tokio::test(start_paused = true)]
    async fn file_search_is_debounced_and_superseded_queries_are_dropped() {
        use std::sync::atomic::AtomicU64;

        let root = std::env::temp_dir().join(format!("kloop-tui-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for name in ["alpha.txt", "beta.txt", "unrelated.md"] {
            std::fs::write(root.join(name), "").unwrap();
        }
        let target = |query: &str| menu::CompletionTarget {
            kind: menu::PopupKind::File,
            query: query.to_string(),
            range: crate::text_layout::TextRange::empty(crate::text_layout::ByteOffset::ZERO),
            cursor: crate::text_layout::ByteOffset::ZERO,
        };

        let latest = Arc::new(AtomicU64::new(1));
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Query 1 is superseded before its debounce elapses, so it never walks.
        spawn_file_search(
            root.clone(),
            target("alpha"),
            1,
            0,
            Arc::clone(&latest),
            tx.clone(),
        );
        latest.store(2, Ordering::Relaxed);
        spawn_file_search(
            root.clone(),
            target("beta"),
            2,
            0,
            Arc::clone(&latest),
            tx.clone(),
        );

        // The paused clock lets the debounce elapse without real sleeping; the
        // loop would meanwhile run its select arms (keys, ticks, agent events).
        let task = rx.recv().await.expect("the newest query answers");
        match task {
            UiTask::FileSearch { seq, paths, .. } => {
                assert_eq!(seq, 2);
                assert_eq!(paths, vec!["beta.txt".to_string()]);
            }
            other => panic!("unexpected task: {:?}", task_name(&other)),
        }
        tokio::task::yield_now().await;
        assert!(
            rx.try_recv().is_err(),
            "the superseded query must not deliver a result"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Enter is held only for a paste that is actually loading an image, and
    /// only where Enter would submit: a popup, a panel or a newline chord keeps
    /// its own meaning.
    #[test]
    fn enter_waits_for_a_paste_only_while_one_is_loading() {
        use crossterm::event::KeyCode;
        use crossterm::event::KeyEvent;
        use crossterm::event::KeyModifiers;

        let mut app = App::new("paste-enter".into());
        let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(!enter_waits_for_paste(0, &app, &enter));
        assert!(enter_waits_for_paste(1, &app, &enter));
        assert!(
            !enter_waits_for_paste(1, &app, &KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT)),
            "shift+Enter is a newline, not a submit"
        );

        // A popup consumes Enter itself (accept), so it is not held.
        app.composer.paste("@mai");
        let target = app.detect_trigger().expect("@ token");
        app.set_file_results(target, vec!["main.rs".into()]);
        assert!(app.popup.is_some());
        assert!(!enter_waits_for_paste(1, &app, &enter));
    }

    /// A search lands only while it is the newest one and the menu state that
    /// asked for it is still current; Esc closes the menu without touching the
    /// token, so the sequence alone would reopen it.
    #[test]
    fn a_dismissed_search_does_not_land() {
        assert!(search_lands(3, 3, 1, 1));
        assert!(!search_lands(2, 3, 1, 1), "superseded by a later query");
        assert!(!search_lands(3, 3, 1, 2), "menu dismissed since");
    }

    fn task_name(task: &UiTask) -> &'static str {
        match task {
            UiTask::FileSearch { .. } => "file search",
            UiTask::Clipboard { .. } => "clipboard",
            UiTask::ImagePaste { .. } => "image paste",
            UiTask::Branch { .. } => "branch",
        }
    }

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

    fn snapshot(revision: u64, todos: usize) -> kloop_core::tools::TodoSnapshot {
        kloop_core::tools::TodoSnapshot {
            revision,
            todos: (1..=todos)
                .map(|id| kloop_core::tools::TodoItem {
                    subject: format!("Todo {id}"),
                    status: kloop_core::tools::TodoStatus::Pending,
                })
                .collect(),
        }
    }

    #[test]
    fn startup_seed_uses_the_channel_ui_event_seam() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        ui.emit(&CoreEvent::TodoUpdated(snapshot(0, 0)));
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::Core(CoreEvent::TodoUpdated(snapshot))
                if snapshot.revision == 0 && snapshot.todos.is_empty()
        ));
    }

    #[test]
    fn command_events_show_the_output_then_move_the_gauge() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let result = kloop_core::commands::SlashResult {
            output: "compacted".into(),
            new_session: false,
            run_turn: None,
            quit: false,
            route_changed: false,
            open_picker: None,
        };
        assert!(send_command_result_events(&tx, &result, 40_000));
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::System(output) if output == "compacted"
        ));
        // Last, so the footer gauge stops quoting the history the command just
        // rewrote — no turn runs here to send a Usage of its own.
        assert!(matches!(
            rx.try_recv().unwrap(),
            AgentEvent::Core(CoreEvent::Usage(used)) if used == 40_000
        ));
        assert!(rx.try_recv().is_err());
    }

    /// `/clear` wipes the screen, and the next frame is a full repaint: a draw
    /// identical to the one before the clear must still put the content back,
    /// which a diff against the stale back buffer would skip.
    #[test]
    fn clear_terminal_blanks_the_screen_and_forces_a_full_repaint() {
        let mut terminal = ratatui::Terminal::with_options(
            PinnedBackend::new(TestBackend::new(12, 3)),
            TerminalOptions {
                viewport: Viewport::Inline(3),
            },
        )
        .unwrap();
        let draw = |terminal: &mut ratatui::Terminal<PinnedBackend<TestBackend>>| {
            terminal
                .draw(|frame| {
                    frame.render_widget(ratatui::widgets::Paragraph::new("banner"), frame.area())
                })
                .unwrap();
        };
        draw(&mut terminal);
        terminal
            .backend()
            .inner
            .assert_buffer_lines(["banner      ", "", ""]);

        clear_terminal(&mut terminal).unwrap();
        terminal
            .backend()
            .inner
            .assert_buffer_lines(["            ", "", ""]);

        draw(&mut terminal);
        terminal
            .backend()
            .inner
            .assert_buffer_lines(["banner      ", "", ""]);
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
        let viewport = draw_frame_once(&mut terminal, &mut app);

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
        let viewport = draw_frame_once(&mut terminal, &mut app);

        let events = terminal.backend().inner.events.borrow();
        assert!(events.contains(&"draw"), "frame was drawn: {events:?}");
        assert_eq!((viewport.width, viewport.height), (80, 6));
        // Confirmed small geometry freezes the overflowing prefix into
        // scrollback, so the live tail drops below the original twelve cells.
        assert!(app.cells.len() < 12);
        drop(events);

        let grown = draw_frame_once(&mut terminal, &mut app);
        assert_eq!((grown.width, grown.height), (80, 24));
    }

    /// End of a resumed session: one answer taller than the whole screen, then
    /// the resume note. Committing the answer whole would strand the note
    /// behind a blank pad, and `draw` clips its top, so the commit has
    /// to freeze the overflowing lines themselves — otherwise the opening of the
    /// conclusion is on no screen and in no scrollback, and `kloop -r` can never
    /// show it.
    #[test]
    fn a_tall_resumed_answer_is_frozen_instead_of_clipped_away() {
        let backend = PinnedBackend::new(TestBackend::new(40, 12));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(12),
            },
        )
        .unwrap();

        let mut app = App::new("resume-tall".into());
        // A System cell renders one row per source line: 40 rows of answer.
        app.cells = vec![
            Cell::System(
                (0..40)
                    .map(|i| format!("row {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Cell::Note("resumed session — 32 message(s)".into()),
        ];
        let viewport = draw_frame_once(&mut terminal, &mut app);

        let width = usize::from(viewport.width);
        let active_h = tail_rows(&app, viewport);
        let frozen = app.head_skip(width);
        let visible: Vec<String> = render::visible_transcript(&app, &render::Hud::default(), width)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(app.cells.len(), 2, "neither whole cell is committable");
        assert_eq!(visible.len(), active_h, "the live tail fills the viewport");
        assert_eq!(
            frozen + visible.len(),
            41,
            "every answer line plus the note is either frozen or on screen"
        );
        assert_eq!(
            visible.first().map(String::as_str),
            Some(format!("row {frozen}").as_str()),
            "the viewport opens on the line after the scrollback seam"
        );
    }

    /// A plan taller than the screen, still waiting on its answer. Its top has to
    /// be in scrollback while the panel is up — the user reads the whole plan
    /// before answering, not after (plan 214) — and answering only adds the
    /// outcome below the seam.
    #[test]
    fn a_waiting_plan_reaches_scrollback_before_it_is_answered() {
        use crate::app::PlanStatus;
        use crate::app::ToolStatus;
        use kloop_core::permissions::ApprovalScope;
        use kloop_core::permissions::ConfirmPreview;
        use kloop_core::permissions::ConfirmRequest;

        let backend = PinnedBackend::new(TestBackend::new(40, 24));
        let mut terminal = ratatui::Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(24),
            },
        )
        .unwrap();
        let mut app = App::new("plan-tall".into());
        app.running = true;
        app.cells = vec![
            Cell::User("plan the fix".into()),
            Cell::Tool {
                name: "exit_plan_mode".into(),
                input: "{}".into(),
                status: ToolStatus::Running,
                output: None,
            },
        ];
        let plan = (0..40)
            .map(|i| format!("- step {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (reply, _answer) = tokio::sync::oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "Exit plan mode and start on this plan?".into(),
                approval_scopes: vec![ApprovalScope::Once],
                preview: Some(ConfirmPreview::Plan(plan.clone())),
                ..Default::default()
            },
            reply,
        });
        let viewport = draw_frame_once(&mut terminal, &mut app);

        let width = usize::from(viewport.width);
        let text = |line: &Line<'_>| -> String {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        };
        let plan_lines: Vec<String> = render::cell_lines(&app.cells[0], width)
            .iter()
            .map(text)
            .collect();
        let visible = |app: &App| -> Vec<String> {
            render::visible_transcript(app, &render::Hud::default(), width)
                .iter()
                .map(text)
                .collect()
        };
        let frozen = app.head_skip(width);
        let shown = visible(&app);
        assert!(
            matches!(
                app.interactions.front(),
                Some(crate::app::PendingInteraction::Confirm { req, .. })
                    if matches!(req.preview, Some(ConfirmPreview::Plan(_)))
            ),
            "the panel is still up"
        );
        assert_eq!(
            app.cells,
            vec![Cell::Plan {
                text: plan.clone(),
                status: PlanStatus::Pending,
            }],
            "the plan took its row's place and everything above it is in scrollback"
        );
        assert_eq!(shown.len(), tail_rows(&app, viewport));
        assert_eq!(
            [&plan_lines[..frozen], &shown[..]].concat(),
            plan_lines,
            "scrollback ends where the viewport begins, and nothing is dropped"
        );
        let scrollback = terminal.backend().inner.scrollback();
        let rows: Vec<String> = (0..scrollback.area.height)
            .map(|y| {
                (0..scrollback.area.width)
                    .map(|x| scrollback[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect();
        let top: Vec<String> = plan_lines[..frozen]
            .iter()
            .map(|line| line.trim_end().to_string())
            .collect();
        assert_eq!(
            rows,
            [
                &["".to_string(), "> plan the fix".to_string()][..],
                &top[..]
            ]
            .concat(),
            "the plan's top is in native scrollback, right under what came before"
        );

        app.on_key(
            width,
            crossterm::event::KeyEvent::from(crossterm::event::KeyCode::Enter),
        );
        let viewport = draw_frame_once(&mut terminal, &mut app);
        let width = usize::from(viewport.width);
        let shown = visible(&app);
        assert_eq!(app.head_skip(width), frozen, "the seam did not move");
        assert_eq!(
            shown,
            [&plan_lines[frozen..], &["  ✓ approved".to_string()]].concat()
        );
    }

    /// A terminal whose inline viewport is the whole screen, so every line a
    /// commit writes lands in `TestBackend`'s scrollback.
    fn full_screen(width: u16, height: u16) -> ratatui::Terminal<PinnedBackend<TestBackend>> {
        ratatui::Terminal::with_options(
            PinnedBackend::new(TestBackend::new(width, height)),
            TerminalOptions {
                viewport: Viewport::Inline(height),
            },
        )
        .unwrap()
    }

    fn rows(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn scrollback_rows(terminal: &ratatui::Terminal<PinnedBackend<TestBackend>>) -> Vec<String> {
        let scrollback = terminal.backend().inner.scrollback();
        (0..scrollback.area.height)
            .map(|y| {
                (0..scrollback.area.width)
                    .map(|x| scrollback[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    /// Draw a frame and check what plan 215 is about: scrollback followed by
    /// the live tail is exactly `whole` — nothing repeated, nothing dropped —
    /// and the live tail fits its rows, so none of it is clipped off the top.
    fn draw_and_check(
        terminal: &mut ratatui::Terminal<PinnedBackend<TestBackend>>,
        app: &mut App,
        whole: &[String],
        context: &str,
    ) {
        let viewport = draw_frame_once(terminal, app);
        let width = usize::from(viewport.width);
        let visible = rows(&render::visible_transcript(
            app,
            &render::Hud::default(),
            width,
        ));
        let active_h = tail_rows(app, viewport);
        assert!(
            visible.len() <= active_h,
            "{context}: {} live rows for {active_h}, the top is clipped",
            visible.len()
        );
        assert_eq!(
            [scrollback_rows(terminal), visible].concat(),
            whole,
            "{context}"
        );
    }

    fn answer_delta(text: &str) -> AgentEvent {
        AgentEvent::Core(CoreEvent::ItemDelta {
            id: "m".into(),
            delta: kloop_core::event::Delta::Text(text.into()),
        })
    }

    fn answer_done(text: &str) -> AgentEvent {
        AgentEvent::Core(CoreEvent::ItemCompleted {
            id: "m".into(),
            item: kloop_core::event::Item::AssistantMessage {
                text: text.into(),
                status: kloop_core::event::ItemStatus::Completed,
            },
        })
    }

    /// Stream `answer[from..to]` in `chunk`-byte deltas, checking every frame:
    /// the lines above the answer (`above`), then the answer as it shows right
    /// now, are split between scrollback and screen with nothing lost. Returns
    /// how many rows scrollback holds at the end.
    fn stream_and_check(
        terminal: &mut ratatui::Terminal<PinnedBackend<TestBackend>>,
        app: &mut App,
        above: &[String],
        answer: &str,
        (from, to): (usize, usize),
        chunk: usize,
    ) -> usize {
        let width = usize::from(terminal.get_frame().area().width);
        let mut sent = from;
        while sent < to {
            let next = (sent + chunk).min(to);
            app.apply(answer_delta(&answer[sent..next]));
            sent = next;
            let (showing, _) = crate::markdown::assistant_stream(&answer[..sent], width);
            let whole = [above, &rows(&showing)].concat();
            draw_and_check(terminal, app, &whole, &format!("after {sent} bytes"));
        }
        scrollback_rows(terminal).len()
    }

    /// An answer three screens tall, streamed in small pieces. At every frame
    /// its settled top is in scrollback and the rest on screen; once it is
    /// complete and the turn has closed, the two together are the finished
    /// render, line for line.
    #[test]
    fn a_streaming_answer_reaches_scrollback_as_it_settles() {
        let mut terminal = full_screen(40, 14);
        let width = 40;
        let mut app = App::new("stream-tall".into());
        app.running = true;
        let answer: String = (0..5)
            .map(|i| {
                format!(
                    "Step {i} explains one more part of the change, in enough words to wrap.\n\n- point {i}a\n- point {i}b\n\n- point {i}c after a gap\n\n"
                )
            })
            .collect();
        let finished = rows(&crate::markdown::markdown_lines(&answer, width));
        assert!(finished.len() > 3 * 8, "three screens of answer");

        let frozen = stream_and_check(&mut terminal, &mut app, &[], &answer, (0, answer.len()), 5);
        assert!(frozen > 8, "the top went to scrollback while it streamed");

        app.apply(answer_done(&answer));
        draw_and_check(&mut terminal, &mut app, &finished, "completed");
        app.apply(AgentEvent::Core(CoreEvent::TurnEnded(EndReason::Completed)));
        app.seal_turn(5);
        let rule = rows(&render::cell_lines(&Cell::TurnEnd(5), width));
        draw_and_check(
            &mut terminal,
            &mut app,
            &[finished, rule].concat(),
            "turn closed",
        );
    }

    /// A tight list has no blank line in it until it ends. Each item settles
    /// when the next one starts, so a list taller than the screen reaches
    /// scrollback while it is written instead of losing its top.
    #[test]
    fn a_tall_tight_list_reaches_scrollback_item_by_item() {
        let mut terminal = full_screen(40, 14);
        let width = 40;
        let mut app = App::new("stream-list".into());
        app.running = true;
        let items: String = (1..=40)
            .map(|i| format!("- item {i:02} is a short line\n"))
            .collect();
        let answer = format!("Files:\n{items}\nDone.\n");

        let frozen = stream_and_check(
            &mut terminal,
            &mut app,
            &[],
            &answer,
            (0, answer.len() - "\nDone.\n".len()),
            4,
        );
        assert!(
            frozen > 20,
            "the list went to scrollback while it was written"
        );
        app.apply(answer_done(&answer));
        let finished = rows(&crate::markdown::markdown_lines(&answer, width));
        draw_and_check(&mut terminal, &mut app, &finished, "completed");
    }

    /// A code block taller than the screen reaches scrollback line by line
    /// while it is still being written, not only once its fence closes.
    #[test]
    fn a_tall_code_block_reaches_scrollback_while_it_is_written() {
        let mut terminal = full_screen(40, 14);
        let width = 40;
        let mut app = App::new("stream-code".into());
        app.running = true;
        let body: String = (0..30)
            .map(|i| format!("    total += step({i})  # line {i}\n"))
            .collect();
        let head = format!("The script:\n\n```python\ndef run():\n    total = 0\n{body}");
        let answer = format!("{head}    return total\n```\n\nThat is all.\n");

        stream_and_check(&mut terminal, &mut app, &[], &answer, (0, head.len()), 7);
        assert!(
            scrollback_rows(&terminal)
                .iter()
                .any(|row| row.ends_with("# line 0")),
            "code went to scrollback before its fence closed"
        );
        stream_and_check(
            &mut terminal,
            &mut app,
            &[],
            &answer,
            (head.len(), answer.len()),
            7,
        );
        app.apply(answer_done(&answer));
        let finished = rows(&crate::markdown::markdown_lines(&answer, width));
        draw_and_check(&mut terminal, &mut app, &finished, "completed");
    }

    /// An approval is up under an answer taller than the screen. A panel used
    /// to hold the commit, and the answer's top was then neither on screen nor
    /// in scrollback while the user decided (plan 215).
    #[test]
    fn an_open_approval_does_not_hold_back_the_answer_above_it() {
        use crate::app::ToolStatus;
        use kloop_core::permissions::ApprovalScope;
        use kloop_core::permissions::ConfirmRequest;

        let mut terminal = full_screen(40, 24);
        let width = 40;
        let mut app = App::new("approval-tall".into());
        app.running = true;
        app.cells = vec![
            Cell::Assistant(
                (0..30)
                    .map(|i| format!("- finding {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            Cell::Tool {
                name: "Bash".into(),
                input: r#"{"command":"cargo test"}"#.into(),
                status: ToolStatus::Running,
                output: None,
            },
        ];
        let (reply, _answer) = tokio::sync::oneshot::channel();
        app.apply(AgentEvent::Confirm {
            req: ConfirmRequest {
                description: "Run cargo test?".into(),
                approval_scopes: vec![ApprovalScope::Once],
                ..Default::default()
            },
            reply,
        });
        let whole: Vec<String> = app
            .cells
            .iter()
            .flat_map(|cell| rows(&render::cell_lines(cell, width)))
            .collect();
        draw_and_check(&mut terminal, &mut app, &whole, "panel up");
        assert!(app.interaction_active(), "the panel is still up");
        assert_eq!(scrollback_rows(&terminal)[0], "• finding 0");
    }

    /// A background task's row stood in place while it ran and held everything
    /// behind it until four screens had piled up. Now it leaves when the tail
    /// needs the room, and its finish is written as a row of its own.
    #[test]
    fn a_running_background_row_does_not_hold_back_a_long_answer() {
        use kloop_core::event::BackgroundTask;
        use kloop_core::event::BackgroundTaskKind;
        use kloop_core::event::BackgroundTaskStatus;

        let mut terminal = full_screen(40, 14);
        let width = 40;
        let mut app = App::new("background-tall".into());
        app.running = true;
        let task = |status| BackgroundTask {
            id: "agent-1".into(),
            run_id: None,
            kind: BackgroundTaskKind::Agent,
            description: "long audit".into(),
            command: None,
            status,
            output_path: None,
            detail: None,
        };
        app.apply(AgentEvent::Core(CoreEvent::BackgroundTaskUpdated(task(
            BackgroundTaskStatus::Running,
        ))));
        let running = rows(&render::cell_lines(&app.cells[0], width));
        let answer: String = (0..6)
            .map(|i| format!("Finding {i} is written out as a paragraph long enough to wrap.\n\n"))
            .collect();

        stream_and_check(
            &mut terminal,
            &mut app,
            &running,
            &answer,
            (0, answer.len()),
            6,
        );
        app.apply(answer_done(&answer));
        app.apply(AgentEvent::Core(CoreEvent::BackgroundTaskUpdated(task(
            BackgroundTaskStatus::Completed,
        ))));
        let finished = rows(&crate::markdown::markdown_lines(&answer, width));
        let done = rows(&render::cell_lines(app.cells.last().unwrap(), width));
        assert_eq!(
            done[0], "✓ Agent(agent-1) long audit",
            "the finish is a row of its own"
        );
        draw_and_check(
            &mut terminal,
            &mut app,
            &[running, finished, done].concat(),
            "task finished",
        );
    }

    /// How many whole cells a commit at `viewport` would move to scrollback.
    fn cells_leaving(app: &App, viewport: Rect) -> usize {
        let plan = render::FramePlan::new(
            app,
            viewport,
            &render::Hud::default(),
            &mut render::ShownCache::default(),
        );
        overflow_freeze_target(app, &plan, viewport).0
    }

    #[test]
    fn overflow_budget_reserves_live_todos_without_committing_them() {
        let mut app = App::new("todo-overflow".into());
        app.cells = (1..=8)
            .map(|index| Cell::Assistant(format!("history {index}")))
            .collect();
        let cells = app.cells.clone();
        let without_todos = cells_leaving(&app, Rect::new(0, 0, 40, 10));

        app.todos = Some(snapshot(1, 3));
        let with_todos = cells_leaving(&app, Rect::new(0, 0, 40, 10));
        assert!(with_todos > without_todos);
        assert_eq!(app.cells, cells);
        assert_eq!(app.todos.as_ref().unwrap().todos.len(), 3);
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
        let effective = cells_leaving(&app, viewport);
        let physical = cells_leaving(&app, Rect::new(0, 0, 40, 12));
        assert!(effective > physical);
    }

    /// A scheduled fire that landed during a slow MCP startup must not start a
    /// turn before the catalogs settle; releasing the gate must deliver it
    /// without waiting for further activity.
    #[tokio::test]
    async fn scheduled_wake_waits_for_a_pending_startup() {
        let inbox = Arc::new(Inbox::default());
        inbox.push(InboxItem::ScheduledPrompt {
            id: "job-1".into(),
            origin: kloop_core::inbox::ScheduledOrigin::Cron,
            scheduled_for_ms: 0,
            reason: None,
            prompt: "timer work".into(),
            missed: false,
        });
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut app = App::new("startup-gated".into());
        app.startup = Some("MCP connecting 1 server(s)…".into());

        assert!(dispatch_autowake(&mut app, &inbox, &tx).is_none());
        assert!(rx.try_recv().is_err(), "startup must not dispatch a wake");

        app.startup = None;
        assert!(dispatch_autowake(&mut app, &inbox, &tx).is_some());
        assert!(matches!(rx.recv().await, Some(WorkerMsg::Wake { .. })));
        assert!(app.running);
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
