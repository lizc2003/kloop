//! ratatui terminal UI for kloop: an inline viewport (plan 38 slice 0) whose
//! finalized cells scroll into the terminal's native scrollback, a one-line
//! input, per-tool-call status rows, and a centered permission popup.
//!
//! Split of responsibilities: the agent runs on its own tokio task and only
//! talks through channels ([`events::ChannelUi`] implements both `Ui` and
//! `Approver`); [`app::App`] folds agent + key events into pure state; and
//! [`render`] turns that state into lines. Only this module touches the
//! terminal — it owns the inline viewport, freezes overflowing cells into
//! scrollback with `insert_before`, and reads keys on a dedicated poll thread.

mod app;
mod events;
mod render;

use std::io::Write as _;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use crossterm::event::Event;
use crossterm::event::KeyEventKind;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::widgets::Widget as _;
use ratatui::TerminalOptions;
use ratatui::Viewport;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::history::History;
use kloop_core::inbox::Inbox;
use kloop_core::inbox::InboxItem;
use kloop_core::permissions::Approver;
use kloop_core::rollout::fork_points;
use kloop_core::rollout::fork_session;
use kloop_core::rollout::resume_session;
use kloop_core::rollout::session_id_of;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

use crate::app::App;
use crate::app::Command;
use crate::events::AgentEvent;
use crate::events::ChannelUi;

/// Out-of-band notification sink (e.g. "saved rule to config.toml"); the
/// CLI's plain mode prints these to stderr, the TUI routes them into the
/// transcript as notes.
pub type NoteFn = Arc<dyn Fn(&str) + Send + Sync>;

struct Turn {
    text: String,
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
    make_config: impl FnOnce(Arc<dyn Approver>, NoteFn) -> Result<Config>,
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
        Arc::new(move |s: &str| note_ui.note(s)),
    )?);
    // `--worktree`: enter an isolated tree for the whole session before any
    // turn runs (plan 35 slice 2). Fail-closed — the user asked for isolation,
    // so a creation error aborts rather than silently working in the main tree.
    // The note buffers in the event channel and shows once the UI loop starts.
    if let Some(name) = &worktree {
        match kloop_core::worktree::enter(&cfg, name).await {
            Ok(msg) => channel_ui.note(&msg),
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
    let permissions = cfg.effective_permissions();

    // Snapshot before the worker takes History: a resumed session replays
    // into the transcript instead of starting on a blank screen.
    let resumed_cells = app::cells_from_history(history.messages());

    let (msg_tx, msg_rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(agent_worker(
        cfg,
        history,
        channel_ui as Arc<dyn Ui>,
        msg_rx,
        event_tx,
        pending_images,
    ));

    let mut terminal = setup_terminal()?;
    let result = ui_loop(
        &mut terminal,
        event_rx,
        msg_tx,
        inbox,
        permissions,
        session_id,
        resumed_cells,
    )
    .await;
    restore_terminal();
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

/// Owns History for its whole lifetime and runs turns strictly one at a time;
/// the UI loop enforces single-flight by ignoring Enter while running.
async fn agent_worker(
    cfg: Arc<Config>,
    mut history: History,
    ui: Arc<dyn Ui>,
    mut msgs: mpsc::UnboundedReceiver<WorkerMsg>,
    events: mpsc::UnboundedSender<AgentEvent>,
    // `--image` blocks ride the first user turn; taken once, then empty.
    mut pending_images: Vec<ContentBlock>,
) {
    while let Some(msg) = msgs.recv().await {
        match msg {
            WorkerMsg::Turn(turn) => {
                let msg = if pending_images.is_empty() {
                    Message::user_text(turn.text)
                } else {
                    Message::user_with_blocks(turn.text, std::mem::take(&mut pending_images))
                };
                history.record(msg);
                let outcome = run_turn(&cfg, &mut history, &ui, &turn.cancel, 0).await;
                if events.send(AgentEvent::TurnEnded(outcome.reason)).is_err() {
                    return;
                }
            }
            WorkerMsg::Wake { cancel } => {
                // Raced: a still-running turn already drained the reinjection,
                // or stop_agent left nothing. Nothing to sample — just clear the
                // busy state the UI loop set when it dispatched the wake.
                if cfg.inbox.is_empty() {
                    if events
                        .send(AgentEvent::TurnEnded(EndReason::Completed))
                        .is_err()
                    {
                        return;
                    }
                    continue;
                }
                let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
                if events.send(AgentEvent::TurnEnded(outcome.reason)).is_err() {
                    return;
                }
            }
            WorkerMsg::Command { line, cancel } => {
                let result = kloop_core::commands::run(&line, &mut history, &cfg, &cancel).await;
                // Clear first (drops the old cells), then show the result on
                // the now-blank transcript.
                if result.cleared && events.send(AgentEvent::ClearTranscript).is_err() {
                    return;
                }
                if !result.output.is_empty()
                    && events.send(AgentEvent::System(result.output)).is_err()
                {
                    return;
                }
                // A skill invoked as `/name` expands to a prompt: record it and
                // run a turn, streaming through `ui` exactly like WorkerMsg::Turn.
                if let Some(prompt) = result.run_turn {
                    history.record(Message::user_text(prompt));
                    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
                    if events.send(AgentEvent::TurnEnded(outcome.reason)).is_err() {
                        return;
                    }
                    continue;
                }
                // TurnEnded clears the busy state the key handler set on submit.
                if events
                    .send(AgentEvent::TurnEnded(EndReason::Completed))
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
                    Ok((session_id, messages, rollout)) => {
                        history.rebase(messages.clone(), rollout);
                        AgentEvent::Forked {
                            session_id,
                            messages,
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
) -> std::io::Result<(String, Vec<Message>, kloop_core::rollout::Rollout)> {
    let src = history.rollout_path().ok_or_else(|| {
        std::io::Error::other("this session is not being saved, so it cannot be rewound")
    })?;
    let sessions_dir = src
        .parent()
        .ok_or_else(|| std::io::Error::other("session file has no parent directory"))?;
    let fork_path = fork_session(src, Some(seq), sessions_dir)?;
    let (messages, rollout) = resume_session(&fork_path)?;
    Ok((session_id_of(&fork_path), messages, rollout))
}

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Terminal> {
    crossterm::terminal::enable_raw_mode()?;
    // A panic elsewhere (agent task, draw code) must not leave the terminal in
    // raw mode with no visible output.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));
    // Inline viewport, no alternate screen (plan 38 slice 0): the transcript
    // scrolls into native scrollback, so the mouse wheel / selection / Cmd+F
    // reach history directly. The viewport is the full terminal height, so
    // `insert_before` pushes finalized cells above it into scrollback. This CPR
    // (cursor-position query) runs before the input thread starts, so nothing
    // races it for stdin.
    let height = crossterm::terminal::size().map(|(_, h)| h).unwrap_or(24);
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stdout());
    let terminal = ratatui::Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(height.max(1)),
        },
    )?;
    Ok(terminal)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    // No alternate screen to leave; show the cursor and drop below the viewport
    // so the shell prompt returns on a clean line.
    let mut out = std::io::stdout();
    let _ = crossterm::execute!(out, crossterm::cursor::Show, crossterm::style::Print("\n"));
    let _ = out.flush();
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

/// Freeze the finalized cells that overflow the live region into native
/// scrollback via `insert_before`, then drop them from the app's tail. Called
/// before each draw (unless a popup owns the screen), so the viewport only ever
/// holds the recent, still-mutable tail.
fn commit_overflow(terminal: &mut Terminal, app: &mut App) -> Result<()> {
    let (w, h) = crossterm::terminal::size().unwrap_or((80, 24));
    let width = (w as usize).max(1);
    // The live region is the viewport minus the bottom chrome: two rules that
    // fence the composer, the composer, and the status line (see render::draw).
    let active_h = (h as usize).saturating_sub(4).max(1);
    let n = render::commit_count(&app.cells, width, active_h);
    if n == 0 {
        return Ok(());
    }
    // Render each cell to fixed-height lines up front so the borrow of
    // `app.cells` ends before `drain_committed` takes it mutably.
    let blocks: Vec<Vec<Line<'static>>> = app.cells[..n]
        .iter()
        .map(|c| render::cell_lines(c, width))
        .collect();
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
    app.drain_committed(n);
    Ok(())
}

/// Autowake (plan 26) fires only when the agent is idle AND a reinjection is
/// waiting: a running turn drains the inbox at its own round boundary (firing
/// then would double-deliver), and an empty inbox means there is nothing to wake
/// for. Kept as a pure function so the delivery invariant is unit-tested rather
/// than only exercised through the live terminal loop.
fn autowake_ready(running: bool, inbox: &Inbox) -> bool {
    !running && !inbox.is_empty()
}

async fn ui_loop(
    terminal: &mut Terminal,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    msgs: mpsc::UnboundedSender<WorkerMsg>,
    inbox: Arc<Inbox>,
    permissions: Arc<kloop_core::permissions::Permissions>,
    session_id: String,
    resumed_cells: Vec<app::Cell>,
) -> Result<()> {
    let mut app = App::new(session_id);
    // A resumed session's cells start as the tail and scroll into scrollback as
    // they overflow, exactly like live output.
    app.cells = resumed_cells;
    // Seed the status-bar badge from the real starting mode (e.g. plan).
    app.mode = permissions.mode();

    // Keys arrive from a dedicated poll thread, not crossterm's EventStream, so
    // its reader never parks holding the lock a resize's CPR needs (see
    // `spawn_input_thread`). Started after `setup_terminal`, so nothing raced
    // the viewport's construction CPR.
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<Event>();
    let stop = Arc::new(AtomicBool::new(false));
    let input_thread = spawn_input_thread(input_tx, stop.clone());

    let mut current_cancel: Option<CancellationToken> = None;
    let outcome = loop {
        // Match the inline viewport to the terminal (repositions on resize),
        // then freeze finalized overflow into scrollback before drawing the
        // tail. Skip committing while a popup owns the screen — scrolling
        // content out from under an overlay would corrupt it.
        if let Err(e) = terminal.autoresize() {
            break Err(e.into());
        }
        if app.confirms.is_empty() && app.fork_picker.is_none() {
            if let Err(e) = commit_overflow(terminal, &mut app) {
                break Err(e);
            }
        }
        if let Err(e) = terminal.draw(|f| render::draw(f, &mut app)) {
            break Err(e.into());
        }
        tokio::select! {
            input = input_rx.recv() => match input {
                Some(Event::Key(k)) if k.kind != KeyEventKind::Release => {
                    match app.on_key(k) {
                        Command::Submit(text) => {
                            let cancel = CancellationToken::new();
                            current_cancel = Some(cancel.clone());
                            let _ = msgs.send(WorkerMsg::Turn(Turn { text, cancel }));
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
                        Command::Quit => break Ok(()),
                        Command::None => {}
                    }
                }
                // Resize repositions the viewport (handled by the autoresize at
                // the top of the loop); any other event just needs a redraw.
                Some(_) => {}
                None => break Ok(()),
            },
            event = events.recv() => {
                let Some(event) = event else { break Ok(()) };
                app.apply(event);
                // Drain whatever else already arrived (streaming deltas come
                // in bursts) so we redraw once per batch, not per token.
                while let Ok(event) = events.try_recv() {
                    app.apply(event);
                }
                // Autowake (plan 26): a background sub-agent finished (its
                // agent_end woke this select) and left a result in the inbox
                // while the agent sits idle. Start a turn to deliver it without
                // waiting for the user. The guard also catches the race where a
                // reinjection lands just after a turn ends.
                if autowake_ready(app.running, &inbox) {
                    let cancel = CancellationToken::new();
                    current_cancel = Some(cancel.clone());
                    app.running = true;
                    let _ = msgs.send(WorkerMsg::Wake { cancel });
                }
            }
        }
    };
    // Stop the input thread (it wakes within one poll interval) before the
    // caller restores the terminal, so no stray read lands after teardown.
    stop.store(true, Ordering::Relaxed);
    let _ = input_thread.join();
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_core::inbox::InboxItem;
    use kloop_core::rollout::Rollout;
    use kloop_protocol::ContentBlock;

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

        // Cut at #2 keeps the first turn only; the branch gets a fresh id.
        let (id, messages, _rollout) = fork_here(&history, 2).unwrap();
        assert_ne!(id, "session");
        assert_eq!(
            messages,
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
    fn autowake_only_when_idle_with_pending() {
        let inbox = Inbox::default();
        // Idle but nothing pending: don't wake.
        assert!(!autowake_ready(false, &inbox));
        inbox.push(InboxItem::SubAgentResult {
            label: "agent-1".into(),
            summary: "done".into(),
        });
        // Idle + a reinjection waiting: wake to deliver it.
        assert!(autowake_ready(false, &inbox));
        // A turn is running: don't wake — it drains at its own round boundary.
        assert!(!autowake_ready(true, &inbox));
    }
}
