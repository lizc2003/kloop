//! ratatui terminal UI for kloop: a scrolling transcript, a one-line input,
//! per-tool-call status rows, and a centered permission popup.
//!
//! Split of responsibilities: the agent runs on its own tokio task and only
//! talks through channels ([`events::ChannelUi`] implements both `Ui` and
//! `Approver`); [`app::App`] folds agent + key events into pure state; and
//! [`render`] turns that state into lines. Only this module touches the
//! terminal.

mod app;
mod events;
mod render;

use std::io::Write as _;
use std::sync::Arc;

use anyhow::Result;
use crossterm::event::Event;
use crossterm::event::EventStream;
use crossterm::event::KeyEventKind;
use futures::StreamExt as _;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::Ui;
use kloop_core::history::History;
use kloop_core::permissions::Approver;
use kloop_core::Config;
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

/// Run the TUI until the user quits. `make_config` is called once with the
/// TUI's approver (the y/a/p/n popup) and note sink, so the caller can wire
/// them into `Permissions` without this crate knowing about rule loading.
pub async fn run(
    make_config: impl FnOnce(Arc<dyn Approver>, NoteFn) -> Result<Config>,
    history: History,
    session_id: String,
) -> Result<()> {
    let (event_tx, event_rx) = mpsc::unbounded_channel();
    let channel_ui = Arc::new(ChannelUi::new(event_tx.clone()));
    let note_ui = channel_ui.clone();
    let cfg = Arc::new(make_config(
        channel_ui.clone(),
        Arc::new(move |s: &str| note_ui.note(s)),
    )?);
    // Shared with the worker's Config: the UI loop enqueues steering here while
    // a turn runs, the agent loop drains it at round boundaries (plan 22).
    let inbox = cfg.inbox.clone();

    // Snapshot before the worker takes History: a resumed session replays
    // into the transcript instead of starting on a blank screen.
    let resumed_cells = app::cells_from_history(history.messages());

    let (turn_tx, turn_rx) = mpsc::unbounded_channel();
    let worker = tokio::spawn(agent_worker(
        cfg,
        history,
        channel_ui as Arc<dyn Ui>,
        turn_rx,
        event_tx,
    ));

    let mut terminal = setup_terminal()?;
    let result = ui_loop(
        &mut terminal,
        event_rx,
        turn_tx,
        inbox,
        session_id,
        resumed_cells,
    )
    .await;
    restore_terminal();
    // The worker holds the session rollout; aborting mid-write is equivalent
    // to a killed session, which resume already repairs.
    worker.abort();
    result
}

/// Owns History for its whole lifetime and runs turns strictly one at a time;
/// the UI loop enforces single-flight by ignoring Enter while running.
async fn agent_worker(
    cfg: Arc<Config>,
    mut history: History,
    ui: Arc<dyn Ui>,
    mut turns: mpsc::UnboundedReceiver<Turn>,
    events: mpsc::UnboundedSender<AgentEvent>,
) {
    while let Some(turn) = turns.recv().await {
        history.record(Message::user_text(turn.text));
        let outcome = run_turn(&cfg, &mut history, &ui, &turn.cancel, 0).await;
        if events.send(AgentEvent::TurnEnded(outcome.reason)).is_err() {
            return;
        }
    }
}

type Terminal = ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>;

fn setup_terminal() -> Result<Terminal> {
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
    // A panic elsewhere (agent task, draw code) must not leave the terminal
    // in raw mode with no visible output.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        hook(info);
    }));
    Ok(ratatui::Terminal::new(
        ratatui::backend::CrosstermBackend::new(std::io::stdout()),
    )?)
}

fn restore_terminal() {
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
    let _ = std::io::stdout().flush();
}

async fn ui_loop(
    terminal: &mut Terminal,
    mut events: mpsc::UnboundedReceiver<AgentEvent>,
    turns: mpsc::UnboundedSender<Turn>,
    inbox: Arc<std::sync::Mutex<Vec<String>>>,
    session_id: String,
    resumed_cells: Vec<app::Cell>,
) -> Result<()> {
    let mut app = App::new(session_id);
    app.cells = resumed_cells;
    let mut keys = EventStream::new();
    let mut current_cancel: Option<CancellationToken> = None;
    loop {
        terminal.draw(|f| render::draw(f, &mut app))?;
        tokio::select! {
            key = keys.next() => match key {
                Some(Ok(Event::Key(k))) if k.kind != KeyEventKind::Release => {
                    match app.on_key(k) {
                        Command::Submit(text) => {
                            let cancel = CancellationToken::new();
                            current_cancel = Some(cancel.clone());
                            let _ = turns.send(Turn { text, cancel });
                        }
                        Command::Steer(text) => {
                            // Enqueue for the running turn; the agent loop
                            // drains it at the next round boundary. The user's
                            // raw text already showed as a User cell.
                            inbox.lock().unwrap().push(text);
                        }
                        Command::Interrupt => {
                            if let Some(cancel) = &current_cancel {
                                cancel.cancel();
                            }
                        }
                        Command::Quit => return Ok(()),
                        Command::None => {}
                    }
                }
                // Resize (and any other terminal event) just needs a redraw,
                // which the top of the loop always does.
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            event = events.recv() => {
                let Some(event) = event else { return Ok(()) };
                app.apply(event);
                // Drain whatever else already arrived (streaming deltas come
                // in bursts) so we redraw once per batch, not per token.
                while let Ok(event) = events.try_recv() {
                    app.apply(event);
                }
            }
        }
    }
}
