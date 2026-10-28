//! 每线程的 History 所有者、操作串行化与 turn/命令执行。

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::Ordering;

use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use kloop_core::Config;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent::run_turn;
use kloop_core::agent::run_turn_with_input;
use kloop_core::commands;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::Message;

use crate::ThreadWorkerMsg;
use crate::ThreadWorkerState;
use crate::Turn;
use crate::params::switch_error_kind;
use crate::thread_ui::ThreadUi;
use crate::wire;
use crate::wire::Outgoing;

/// Owns this thread's History for its whole life. Client turns and idle
/// scheduler deliveries share one single-flight bracket and one monotonic id
/// allocator; a scheduled prompt never borrows the id of the turn that created it.
pub(super) async fn thread_worker(
    mut cfg: Arc<Config>,
    mut history: History,
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
        provider_state,
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
                ThreadWorkerMsg::Turn(Turn {
                    text: String::new(),
                    images: Vec::new(),
                    id,
                    cancel,
                    delivery_only: true,
                })
            }
        };
        let turn = match message {
            ThreadWorkerMsg::Turn(turn) => turn,
            ThreadWorkerMsg::SwitchProvider {
                provider_id,
                model,
                expected_revision,
                request_id,
            } => {
                let result = history
                    .switch_provider(
                        &provider_state,
                        expected_revision,
                        &provider_id,
                        model.as_deref(),
                        kloop_core::provider_route::EffortRequest::Inherit,
                    )
                    .map(|outcome| {
                        let (route, continuity) = match outcome {
                            kloop_core::provider_route::SwitchOutcome::NoOp(route) => {
                                let continuity = route.public_route().continuity;
                                (route, continuity)
                            }
                            kloop_core::provider_route::SwitchOutcome::Changed {
                                route,
                                continuity,
                            } => {
                                ui.projection.update_provider_route(route.public_route());
                                ui.notify(
                                    "thread/provider/changed",
                                    json!({
                                        "route": route.public_route(),
                                        "continuity": continuity,
                                    }),
                                );
                                (route, continuity)
                            }
                        };
                        cfg = Arc::new(cfg.clone_with_provider_route(route.clone()));
                        let public_route = route.public_route();
                        *active_route.lock().unwrap() = public_route.clone();
                        (public_route, continuity)
                    });
                let outgoing = match result {
                    Ok((route, continuity)) => Outgoing::Response {
                        id: request_id,
                        result: json!({"route": route, "continuity": continuity}),
                    },
                    Err(error) => Outgoing::Error {
                        id: Some(request_id),
                        code: wire::SERVER_ERROR,
                        message: error.to_string(),
                        data: Some(json!({"kind": switch_error_kind(&error)})),
                    },
                };
                running.store(false, Ordering::SeqCst);
                let _ = ui.out.send(outgoing.to_json());
                continue;
            }
        };
        if turn.images.is_empty()
            && let Some(args) = provider_command_args(&turn.text)
        {
            let output = run_provider_command(
                &args,
                &mut cfg,
                &provider_state,
                &active_route,
                &mut history,
                &ui,
            );
            ui.notify("system", json!({"text": output}));
            inbox.close_steer_window();
            *current_cancel.lock().unwrap() = None;
            running.store(false, Ordering::SeqCst);
            *ui.turn.lock().unwrap() = None;
            continue;
        }
        ui.notify("turn/started", wire::turn_started_params(turn.id));
        let reason = run_turn_or_command(
            &mut cfg,
            &provider_state,
            &active_route,
            &mut history,
            &ui,
            &turn,
        )
        .await;
        ui.emit(&Event::Usage(kloop_core::agent::context_estimate(
            &cfg, &history,
        )));
        ui.notify(
            "turn/completed",
            wire::turn_completed_params(turn.id, &reason),
        );
        // Usually already closed by the final drain; this covers the endings
        // that never reach it (error, max rounds, interrupt).
        inbox.close_steer_window();
        *current_cancel.lock().unwrap() = None;
        running.store(false, Ordering::SeqCst);
        *ui.turn.lock().unwrap() = None;
    }
    let shutdown_ui: Arc<dyn Ui> = ui.clone();
    let remaining = cfg.shutdown_background_work(&shutdown_ui).await;
    if remaining > 0 {
        ui.emit(&Event::Note(format!(
            "{remaining} background task(s) missed the shutdown deadline"
        )));
    }
    let active_worktree = kloop_core::worktree::finish_active(&cfg).await;
    if let Some(note) = active_worktree {
        ui.emit(&Event::Note(note.trim().to_string()));
    }
}

fn provider_command_args(text: &str) -> Option<Vec<&str>> {
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

const PROVIDER_USAGE: &str = "usage: /provider <provider> [model] [effort]";

/// `unset` is kloop's word for "send no effort field at all" and is not a wire
/// value, so it is matched before the level vocabulary (see the command's own
/// `parse_level`, which this mirrors for the server's idle-transaction path).
fn parse_effort_level(
    raw: &str,
) -> Result<Option<kloop_protocol::ReasoningEffort>, kloop_protocol::UnknownReasoningEffort> {
    if raw.eq_ignore_ascii_case("unset") {
        return Ok(None);
    }
    raw.parse::<kloop_protocol::ReasoningEffort>().map(Some)
}

fn run_provider_command(
    args: &[&str],
    cfg: &mut Arc<Config>,
    provider_state: &kloop_core::provider_route::SessionProviderState,
    active_route: &Arc<Mutex<ActiveProviderRoute>>,
    history: &mut History,
    ui: &Arc<ThreadUi>,
) -> String {
    let Some(provider_id) = args.first().copied() else {
        let route = provider_state.active_route();
        return format!(
            "active: {} {} (revision {})",
            route.provider_id, route.model, route.revision
        );
    };
    if args.len() > 3 {
        return PROVIDER_USAGE.into();
    }
    let model = args.get(1).copied();
    let effort = match args.get(2).copied().map(parse_effort_level).transpose() {
        Ok(level) => level.map_or(
            kloop_core::provider_route::EffortRequest::Inherit,
            kloop_core::provider_route::EffortRequest::Set,
        ),
        Err(error) => return format!("{error}\n{PROVIDER_USAGE}"),
    };
    let expected_revision = provider_state.active_route().revision;
    match history.switch_provider(
        provider_state,
        expected_revision,
        provider_id,
        model,
        effort,
    ) {
        Ok(kloop_core::provider_route::SwitchOutcome::NoOp(route)) => format!(
            "provider unchanged: {} {} (revision {})",
            route.provider_id(),
            route.model(),
            route.revision()
        ),
        Ok(kloop_core::provider_route::SwitchOutcome::Changed { route, continuity }) => {
            let public_route = route.public_route();
            let provider_name = route.provider_id().to_string();
            let model_name = route.model().to_string();
            let revision = route.revision();
            let effort_label = kloop_protocol::ReasoningEffort::choice_str(route.effort());
            *cfg = Arc::new(cfg.clone_with_provider_route(route));
            *active_route.lock().unwrap() = public_route.clone();
            ui.projection.update_provider_route(public_route.clone());
            ui.notify(
                "thread/provider/changed",
                json!({"route": public_route, "continuity": continuity}),
            );
            format!(
                "provider switched: {provider_name} {model_name} (revision {revision}, reasoning continuity: {continuity:?}, effort: {effort_label})",
            )
        }
        Err(error) => format!("provider switch failed: {error}"),
    }
}

/// Run one turn's work and return its end reason, without touching the turn
/// bracket (the caller owns that). A slash command line reads/rewrites History
/// like a turn but is not a model turn: no user message is recorded and nothing
/// is sampled — its output comes back as a `system` notification. `/clear` is
/// refused: a thread is one session, and a new one is `thread/start`. Images (or
/// any non-command text) take the model path.
async fn run_turn_or_command(
    cfg: &mut Arc<Config>,
    provider_state: &kloop_core::provider_route::SessionProviderState,
    active_route: &Arc<Mutex<ActiveProviderRoute>>,
    history: &mut History,
    ui: &Arc<ThreadUi>,
    turn: &Turn,
) -> EndReason {
    if turn.delivery_only {
        let dyn_ui: Arc<dyn Ui> = ui.clone();
        return run_turn(cfg, history, &dyn_ui, &turn.cancel, 0)
            .await
            .reason;
    }
    if turn.images.is_empty() && commands::is_command_or_skill(&turn.text, &cfg.skills) {
        let result = commands::run_with_provider_state(
            &turn.text,
            history,
            cfg,
            provider_state,
            ui.as_ref(),
            &turn.cancel,
        )
        .await;
        if result.route_changed {
            let route = provider_state.active_route();
            *cfg = Arc::new(cfg.clone_with_provider_route(provider_state.freeze()));
            *active_route.lock().unwrap() = route.clone();
            ui.projection.update_provider_route(route);
        }
        // `/exit` is a client-side concept: quitting one thread must never stop
        // a multi-session server. Relay a note instead of acting on it.
        if result.quit {
            ui.notify(
                "system",
                json!({"text": "/exit is for interactive sessions; the server keeps running — close the connection to end this one"}),
            );
            return EndReason::Completed;
        }
        // `/clear` starts a new session, and here a session is a thread the
        // client addressed by id: switching it underneath the client is what
        // `thread/start` exists to avoid.
        if result.new_session {
            ui.notify(
                "system",
                json!({"text": "/clear is not available on the server — start a new thread with thread/start"}),
            );
            return EndReason::Completed;
        }
        if !result.output.is_empty() {
            ui.notify("system", json!({"text": result.output}));
        }
        // A skill invoked as `/name` expands to a prompt: record it and run a
        // turn (streaming via the same notifications as a normal turn).
        if let Some(prompt) = result.run_turn {
            history.record(Message::user_text(prompt));
            let dyn_ui: Arc<dyn Ui> = ui.clone();
            return run_turn(cfg, history, &dyn_ui, &turn.cancel, 0)
                .await
                .reason;
        }
        return EndReason::Completed;
    }
    let msg = if turn.images.is_empty() {
        Message::user_text(turn.text.clone())
    } else {
        Message::user_with_blocks(turn.text.clone(), turn.images.clone())
    };
    let dyn_ui: Arc<dyn Ui> = ui.clone();
    let (outcome, returned) =
        run_turn_with_input(cfg, history, &dyn_ui, &turn.cancel, 0, msg).await;
    if returned.is_some() {
        // Interrupted before the model produced anything: the turn reached
        // neither history nor the session file, so the thread snapshot must not
        // keep its input either. The client still learns it aborted, from
        // `turn/completed` like any other ending, and owns what it does with
        // the text it sent.
        ui.projection.discard_turn(turn.id);
    }
    outcome.reason
}
