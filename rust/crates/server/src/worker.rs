//! 每线程的会话所有者、操作串行化与 turn/命令执行。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::Ui;
use kloop_core::commands;
use kloop_core::driver::Input;
use kloop_core::driver::Report;
use kloop_core::driver::SessionDriver;
use kloop_core::event::Event;
use kloop_protocol::ActiveProviderRoute;

use crate::ThreadWorkerMsg;
use crate::ThreadWorkerState;
use crate::params::switch_error_kind;
use crate::thread_ui::ThreadUi;
use crate::wire;
use crate::wire::Outgoing;

/// Owns this thread's session for its whole life. Client turns, commands and
/// idle scheduler deliveries share one single-flight bracket and one monotonic
/// id allocator; a scheduled prompt never borrows the id of the turn that
/// created it.
pub(super) async fn thread_worker(
    mut driver: SessionDriver,
    ui: Arc<ThreadUi>,
    mut turns: mpsc::UnboundedReceiver<ThreadWorkerMsg>,
    state: ThreadWorkerState,
) {
    let ThreadWorkerState {
        running,
        current_cancel,
        turn_seq,
        inbox,
        active_route,
    } = state;
    let mut inbox_activity = inbox.subscribe_activity();
    loop {
        let message = tokio::select! {
            turn = turns.recv() => {
                let Some(turn) = turn else { break };
                turn
            }
            activity = inbox_activity.changed() => {
                if activity.is_err() {
                    break;
                }
                if inbox.is_empty()
                    || running
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_err()
                {
                    continue;
                }
                let id = turn_seq.fetch_add(1, Ordering::SeqCst);
                *ui.turn.lock().unwrap() = Some(id);
                inbox.open_steer_window(id);
                let cancel = CancellationToken::new();
                *current_cancel.lock().unwrap() = Some(cancel.clone());
                ThreadWorkerMsg::Wake { id, cancel }
            }
        };
        // From here on, only activity that arrives *during* this operation is
        // new. Anything already pending was either swallowed while `running`
        // was set by the caller, or is the delivery this turn was started for;
        // both are the same batch and must not be re-announced on failure.
        let _ = inbox_activity.borrow_and_update();
        let (turn_id, cancel, input) = match message {
            ThreadWorkerMsg::Turn(turn) => {
                // A command is recognized the same way every front-end
                // recognizes one: a `/…` line with no images. Everything else
                // is a model turn.
                let input = if turn.images.is_empty()
                    && commands::is_command_or_skill(&turn.text, &driver.cfg().skills)
                {
                    Input::Command { line: turn.text }
                } else {
                    Input::Message {
                        text: turn.text,
                        images: turn.images,
                    }
                };
                (turn.id, turn.cancel, input)
            }
            // The queue is the input: nothing is recorded as a new user
            // message, and the turn drains what is already there.
            ThreadWorkerMsg::Wake { id, cancel } => (id, cancel, Input::Wake),
            ThreadWorkerMsg::ProviderCommand { text } => {
                let args = provider_command_args(&text).unwrap_or_default();
                let report = driver.provider_command(&args);
                // The client may read config/events the moment it sees the
                // success line, so the route it can read has to be the new one
                // first — exactly as the queued switch transaction answers.
                if let Some(route) = report.route {
                    *active_route.lock().unwrap() = route.clone();
                    ui.projection.update_provider_route(route.clone());
                    ui.notify(
                        "thread/provider/changed",
                        json!({"route": route, "continuity": route.continuity}),
                    );
                }
                ui.notify("system", json!({"text": report.output}));
                inbox.close_steer_window();
                *current_cancel.lock().unwrap() = None;
                running.store(false, Ordering::SeqCst);
                driver.reannounce_if_owed(false);
                continue;
            }
            ThreadWorkerMsg::SwitchProvider {
                provider_id,
                model,
                expected_revision,
                request_id,
            } => {
                let result =
                    driver.switch_provider(expected_revision, &provider_id, model.as_deref());
                let outgoing = match result {
                    Ok(report) => {
                        if report.changed {
                            *active_route.lock().unwrap() = report.route.clone();
                            ui.projection.update_provider_route(report.route.clone());
                            ui.notify(
                                "thread/provider/changed",
                                json!({
                                    "route": report.route,
                                    "continuity": report.continuity,
                                }),
                            );
                        }
                        Outgoing::Response {
                            id: request_id,
                            result: json!({
                                "route": report.route,
                                "continuity": report.continuity,
                            }),
                        }
                    }
                    Err(error) => Outgoing::Error {
                        id: Some(request_id),
                        code: wire::SERVER_ERROR,
                        message: error.to_string(),
                        data: Some(json!({"kind": switch_error_kind(&error)})),
                    },
                };
                running.store(false, Ordering::SeqCst);
                let _ = ui.out.send(outgoing.into_bytes());
                driver.reannounce_if_owed(false);
                continue;
            }
        };
        ui.notify("turn/started", wire::turn_started_params(turn_id));
        let dyn_ui: Arc<dyn Ui> = ui.clone();
        let report = driver.dispatch(input, &dyn_ui, &cancel).await;
        finish_operation(
            &driver,
            &ui,
            turn_id,
            &report,
            &running,
            &current_cancel,
            &active_route,
        );
    }
    let shutdown_ui: Arc<dyn Ui> = ui.clone();
    let shutdown = driver.shutdown(&shutdown_ui).await;
    let missed = shutdown.missed;
    if missed > 0 {
        ui.emit(&Event::Note(format!(
            "{missed} background task(s) missed the shutdown deadline"
        )));
    }
    if let Some(note) = shutdown.worktree {
        ui.emit(&Event::Note(note.trim().to_string()));
    }
}

/// Project one dispatched operation onto this thread's wire bracket. Command
/// notices come first, where they were emitted from inside the run before.
#[allow(clippy::too_many_arguments)]
fn finish_operation(
    driver: &SessionDriver,
    ui: &Arc<ThreadUi>,
    turn_id: u64,
    report: &Report,
    running: &AtomicBool,
    current_cancel: &Mutex<Option<CancellationToken>>,
    active_route: &Mutex<ActiveProviderRoute>,
) {
    if let Report::Command(command) = report {
        if command.route_changed {
            let route = driver.cfg().provider_route.public_route();
            *active_route.lock().unwrap() = route.clone();
            ui.projection.update_provider_route(route);
        }
        if command.new_session {
            ui.notify(
                "system",
                json!({"text": "/clear is not available on the server — start a new thread with thread/start"}),
            );
        }
        if command.quit {
            ui.notify(
                "system",
                json!({"text": "/exit is for interactive sessions; the server keeps running — close the connection to end this one"}),
            );
        }
        if !command.output.is_empty() {
            ui.notify("system", json!({"text": command.output}));
        }
    }
    let returned = match report {
        Report::Turn(turn) => turn.as_ref().is_some_and(|turn| turn.returned.is_some()),
        Report::Command(command) => command
            .turn
            .as_ref()
            .is_some_and(|turn| turn.returned.is_some()),
    };
    if returned {
        // Interrupted before the model produced anything: the turn reached
        // neither history nor the session file, so the thread snapshot must not
        // keep its input either. The client still learns it aborted, from
        // `turn/completed` like any other ending, and owns what it does with
        // the text it sent.
        ui.projection.discard_turn(turn_id);
    }
    ui.emit(&Event::Usage(kloop_core::agent::context_estimate(
        driver.cfg(),
        driver.history(),
    )));
    ui.notify(
        "turn/completed",
        wire::turn_completed_params(turn_id, &report.reason()),
    );
    // Usually already closed by the final drain; this covers the endings that
    // never reach it (error, max rounds, interrupt).
    driver.cfg().inbox.close_steer_window();
    *current_cancel.lock().unwrap() = None;
    running.store(false, Ordering::SeqCst);
    *ui.turn.lock().unwrap() = None;
    driver.reannounce_if_owed(report.drained_inbox());
}

pub(super) fn provider_command_args(text: &str) -> Option<Vec<&str>> {
    let rest = text.strip_prefix("/provider")?;
    if rest
        .chars()
        .next()
        .is_some_and(|character| !character.is_whitespace())
    {
        return None;
    }
    Some(rest.split_whitespace().collect())
}
