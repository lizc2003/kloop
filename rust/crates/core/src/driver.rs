//! The session driver: one place that owns a session's `Config`, `History` and
//! provider state, and turns one front-end input into one state transition.
//!
//! Front-ends keep what is theirs — the event loop, the cancellation token, the
//! shape of their turn bracket (the TUI's `TurnEnded`, the server's
//! `turn/completed`, the headless NDJSON stream) and the rendering. What they
//! do not keep is a second copy of the rules: what gets staged, what a slash
//! command or a skill expansion turns into, when an idle inbox owes a delivery
//! turn, and how a session is torn down. Those live here, so the four front-ends
//! cannot drift apart on the parts a test can compare.
//!
//! A turn is a staged input. Anything that ends before the model produced
//! anything — the user's Esc, a provider that never answered — leaves the
//! session exactly as it was, and the input comes back on [`TurnReport`] for
//! the front-end that has somewhere to put it.

use std::sync::Arc;

use kloop_protocol::ActiveProviderRoute;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::ReasoningContinuity;
use kloop_protocol::RoutePickerStage;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::agent::EndReason;
use crate::agent::TurnOutcome;
use crate::agent::Ui;
use crate::agent::run_turn;
use crate::agent::run_turn_with_input;
use crate::commands;
use crate::config::Config;
use crate::history::History;
use crate::history::ProviderSwitchError;
use crate::provider_route::EffortRequest;
use crate::provider_route::SessionProviderState;
use crate::provider_route::SwitchError;
use crate::provider_route::SwitchOutcome;
use crate::rewind::AbandonedBranch;
use crate::rewind::RewindOutcome;

/// One thing a front-end asks the session to do. The front-end's own message
/// types map onto this: a submitted line, a parked slash command, a delivery
/// turn the idle inbox owes.
#[derive(Debug)]
pub enum Input {
    /// The user's text (and images). Staged, never recorded up front: an
    /// interrupt before the model produces anything takes the whole turn back.
    Message {
        text: String,
        images: Vec<ContentBlock>,
    },
    /// A `/…` line. The command may produce output, ask for a new session,
    /// change the route, or expand a skill into a turn.
    Command { line: String },
    /// A delivery turn: the pending inbox items are the input, and there is no
    /// new text to stage. An empty inbox means the turn was already raced.
    Wake,
}

/// What one dispatch did, in the terms a front-end reports it.
#[derive(Debug)]
pub enum Report {
    /// A turn ran. `None` when a wake found the inbox already drained.
    Turn(Option<TurnReport>),
    Command(CommandReport),
}

#[derive(Debug)]
pub struct TurnReport {
    pub outcome: TurnOutcome,
    /// The input, when an interrupt before the model produced anything took the
    /// whole turn back. The front-end owns where it goes (a composer, a
    /// discarded client turn, or nowhere at all).
    pub returned: Option<Message>,
}

#[derive(Debug)]
pub struct CommandReport {
    pub output: String,
    /// `/exit`: the interactive front-ends quit. The server refuses instead.
    pub quit: bool,
    /// `/clear`: the front-end decides whether it may switch and then calls
    /// [`SessionDriver::start_fresh_session`]. The command itself has not
    /// touched the session.
    pub new_session: bool,
    /// The driver already rebuilt its `Config`; the front-end only has to
    /// refresh its display of the route.
    pub route_changed: bool,
    pub open_picker: Option<RoutePickerStage>,
    /// A skill expanded into a prompt and this turn ran it.
    pub turn: Option<TurnReport>,
}

/// The idle provider transaction's answer (`thread/provider/switch`).
#[derive(Debug)]
pub struct SwitchReport {
    pub route: ActiveProviderRoute,
    pub continuity: ReasoningContinuity,
    /// `false` when the switch was a no-op; nothing was shown or persisted.
    pub changed: bool,
}

/// A `/provider …` idle transaction's answer: the text the user sees, and the
/// route when the command changed it.
#[derive(Debug)]
pub struct ProviderCommandReport {
    pub output: String,
    pub route: Option<ActiveProviderRoute>,
}

/// The session `/clear` switched to, with the lines retiring the old one left
/// for the front-end to show.
pub struct FreshSessionReport {
    pub session_id: String,
    pub report: Vec<String>,
}

/// A rewind onto a fork cut: the branch's messages, and what leaving the old
/// branch behind has to say.
pub enum RewindReport {
    Rewound {
        session_id: String,
        report: Vec<String>,
        notes: Vec<String>,
        messages: Vec<Message>,
    },
    /// Cancelled while summarizing; the session is as it was.
    Cancelled,
}

impl Report {
    /// Whether this operation entered the agent loop, whose round boundaries
    /// drain the inbox. A command does not; a skill expansion does.
    pub fn drained_inbox(&self) -> bool {
        match self {
            Self::Turn(turn) => turn.is_some(),
            Self::Command(command) => command.turn.is_some(),
        }
    }

    /// The reason the operation ended, as the front-end's bracket reports it.
    /// A wake that found nothing still has to clear the busy state.
    pub fn reason(&self) -> EndReason {
        match self {
            Self::Turn(Some(turn)) => turn.outcome.reason.clone(),
            Self::Turn(None) => EndReason::Completed,
            Self::Command(command) => command
                .turn
                .as_ref()
                .map_or(EndReason::Completed, |turn| turn.outcome.reason.clone()),
        }
    }
}

/// The token every task in a session is bounded by, plus the shutdown report:
/// one shape for the four front-ends and `/clear`.
#[derive(Debug)]
pub struct ShutdownReport {
    /// Detached work still running when the shutdown began.
    pub running: usize,
    /// Work that missed the bounded reap deadline.
    pub missed: usize,
    /// The kept-worktree note, when the session left a tree behind.
    pub worktree: Option<String>,
}

pub struct SessionDriver {
    cfg: Arc<Config>,
    history: History,
    provider: Arc<SessionProviderState>,
    /// `--image` blocks, which ride the first user message. Taken once.
    launch_images: Vec<ContentBlock>,
    /// Inbox activity as this driver has seen it: lets it tell "a command left
    /// the queue alone" from "a model turn already drained this batch".
    activity: watch::Receiver<u64>,
}

impl SessionDriver {
    /// Take the session's state. The route timeline the history carries is the
    /// source of truth; a history with no rollout yet (a test, or an ephemeral
    /// session) starts from the Config's route.
    pub fn new(
        cfg: Arc<Config>,
        history: History,
        launch_images: Vec<ContentBlock>,
    ) -> Result<Self, SwitchError> {
        let provider = provider_state_for(&cfg, &history)?;
        let activity = cfg.inbox.subscribe_activity();
        Ok(Self {
            cfg,
            history,
            provider,
            launch_images,
            activity,
        })
    }

    pub fn cfg(&self) -> &Arc<Config> {
        &self.cfg
    }

    pub fn history(&self) -> &History {
        &self.history
    }

    pub fn provider_state(&self) -> &Arc<SessionProviderState> {
        &self.provider
    }

    /// Run one input to completion. The driver records its own activity
    /// baseline first, so [`Self::reannounce_if_owed`] after the call can tell
    /// what arrived while the operation ran.
    pub async fn dispatch(
        &mut self,
        input: Input,
        ui: &Arc<dyn Ui>,
        cancel: &CancellationToken,
    ) -> Report {
        // Anything already pending belongs to the batch this operation starts
        // for (or to a turn that already drained it), not to this operation.
        let _ = self.activity.borrow_and_update();
        match input {
            Input::Message { text, images } => {
                Report::Turn(Some(self.run_message(text, images, ui, cancel).await))
            }
            Input::Command { line } => Report::Command(self.run_command(&line, ui, cancel).await),
            Input::Wake => {
                if self.cfg.inbox.is_empty() {
                    return Report::Turn(None);
                }
                let outcome = run_turn(&self.cfg, &mut self.history, ui, cancel, 0).await;
                Report::Turn(Some(TurnReport {
                    outcome,
                    returned: None,
                }))
            }
        }
    }

    /// Whether an idle front-end should re-announce the inbox after an
    /// operation. A command never drains it, so anything left is work to pick
    /// up. A model turn drains at its round boundaries — re-announcing the very
    /// batch it started for would retry a turn that failed before its first
    /// request forever, so only what arrived during the turn counts as new.
    pub fn reannounce_if_owed(&self, drained: bool) {
        if self.cfg.inbox.is_empty() {
            return;
        }
        let arrived = self.activity.has_changed().unwrap_or(false);
        if drained && !arrived {
            return;
        }
        self.cfg.inbox.notify_activity();
    }

    /// Switch the session's route from an idle transaction. The caller shows
    /// the answer; the Config is already on the new route when this returns.
    pub fn switch_provider(
        &mut self,
        expected_revision: u64,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<SwitchReport, ProviderSwitchError> {
        let outcome = self.history.switch_provider(
            self.provider.as_ref(),
            expected_revision,
            provider_id,
            model,
            EffortRequest::Inherit,
        )?;
        let changed = matches!(outcome, SwitchOutcome::Changed { .. });
        let route = match outcome {
            SwitchOutcome::NoOp(route) => route.public_route(),
            SwitchOutcome::Changed { route, .. } => {
                self.cfg = Arc::new(self.cfg.clone_with_provider_route(route.clone()));
                route.public_route()
            }
        };
        let continuity = route.continuity;
        Ok(SwitchReport {
            route,
            continuity,
            changed,
        })
    }

    /// `/provider …` on the idle path: no turn, no projected turn, just the
    /// command's bounded answer. Mirrors the parsed forms the worker's
    /// classifier accepts.
    pub fn provider_command(&mut self, args: &[&str]) -> ProviderCommandReport {
        const USAGE: &str = "usage: /provider <provider> [model] [effort]";
        let Some(provider_id) = args.first().copied() else {
            let route = self.provider.active_route();
            return ProviderCommandReport {
                output: format!(
                    "active: {} {} (revision {})",
                    route.provider_id, route.model, route.revision
                ),
                route: None,
            };
        };
        if args.len() > 3 {
            return ProviderCommandReport {
                output: USAGE.into(),
                route: None,
            };
        }
        let model = args.get(1).copied();
        let effort = match args.get(2).copied().map(parse_effort_level).transpose() {
            Ok(level) => level.map_or(EffortRequest::Inherit, EffortRequest::Set),
            Err(error) => {
                return ProviderCommandReport {
                    output: format!("{error}\n{USAGE}"),
                    route: None,
                };
            }
        };
        let expected_revision = self.provider.active_route().revision;
        match self.history.switch_provider(
            self.provider.as_ref(),
            expected_revision,
            provider_id,
            model,
            effort,
        ) {
            Ok(SwitchOutcome::NoOp(route)) => ProviderCommandReport {
                output: format!(
                    "provider unchanged: {} {} (revision {})",
                    route.provider_id(),
                    route.model(),
                    route.revision()
                ),
                route: None,
            },
            Ok(SwitchOutcome::Changed { route, continuity }) => {
                let public = route.public_route();
                let provider_name = route.provider_id().to_string();
                let model_name = route.model().to_string();
                let revision = route.revision();
                let effort_label = kloop_protocol::ReasoningEffort::choice_str(route.effort());
                self.cfg = Arc::new(self.cfg.clone_with_provider_route(route));
                ProviderCommandReport {
                    output: format!(
                        "provider switched: {provider_name} {model_name} (revision {revision}, reasoning continuity: {continuity:?}, effort: {effort_label})",
                    ),
                    route: Some(public),
                }
            }
            Err(error) => ProviderCommandReport {
                output: format!("provider switch failed: {error}"),
                route: None,
            },
        }
    }

    /// `/clear`: build the new session from the one in hand, then swap it in.
    /// The caller decides whether it may switch at all; this method assumes it
    /// may.
    pub async fn start_fresh_session(
        &mut self,
        ui: &Arc<dyn Ui>,
    ) -> anyhow::Result<FreshSessionReport> {
        let fresh = commands::start_fresh_session(&self.cfg, ui).await?;
        self.history = fresh.history;
        self.cfg = Arc::new(fresh.cfg);
        self.provider = provider_state_for(&self.cfg, &self.history).map_err(anyhow::Error::new)?;
        self.activity = self.cfg.inbox.subscribe_activity();
        Ok(FreshSessionReport {
            session_id: self.cfg.session_id.clone(),
            report: fresh.report,
        })
    }

    /// Rewind onto the fork cut at `seq`: the branch is a new session, so the
    /// Config, provider state and inbox subscription all move with it.
    pub async fn rewind(
        &mut self,
        seq: u64,
        abandoned: AbandonedBranch,
        cancel: &CancellationToken,
        ui: &Arc<dyn Ui>,
    ) -> anyhow::Result<RewindReport> {
        let outcome =
            crate::rewind::rewind(&self.cfg, &mut self.history, seq, abandoned, ui, cancel).await?;
        let rewound = match outcome {
            RewindOutcome::Cancelled => return Ok(RewindReport::Cancelled),
            RewindOutcome::Rewound(rewound) => *rewound,
        };
        self.cfg = Arc::new(rewound.cfg);
        self.provider = provider_state_for(&self.cfg, &self.history).map_err(anyhow::Error::new)?;
        self.activity = self.cfg.inbox.subscribe_activity();
        Ok(RewindReport::Rewound {
            session_id: self.cfg.session_id.clone(),
            report: rewound.report,
            notes: rewound.notes,
            messages: self.history.messages().to_vec(),
        })
    }

    /// Stop everything this session still runs, then leave its worktree.
    pub async fn shutdown(self, ui: &Arc<dyn Ui>) -> ShutdownReport {
        shutdown_session(&self.cfg, ui).await
    }

    async fn run_message(
        &mut self,
        text: String,
        mut images: Vec<ContentBlock>,
        ui: &Arc<dyn Ui>,
        cancel: &CancellationToken,
    ) -> TurnReport {
        images.append(&mut self.launch_images);
        let message = if images.is_empty() {
            Message::user_text(text)
        } else {
            Message::user_with_blocks(text, images)
        };
        let (outcome, returned) =
            run_turn_with_input(&self.cfg, &mut self.history, ui, cancel, 0, message).await;
        TurnReport { outcome, returned }
    }

    async fn run_command(
        &mut self,
        line: &str,
        ui: &Arc<dyn Ui>,
        cancel: &CancellationToken,
    ) -> CommandReport {
        let result = commands::run_with_provider_state(
            line,
            &mut self.history,
            &self.cfg,
            self.provider.as_ref(),
            ui.as_ref(),
            cancel,
        )
        .await;
        if result.route_changed {
            self.cfg = Arc::new(self.cfg.clone_with_provider_route(self.provider.freeze()));
        }
        let turn = match result.run_turn {
            Some(prompt) => Some(self.run_message(prompt, Vec::new(), ui, cancel).await),
            None => None,
        };
        CommandReport {
            output: result.output,
            quit: result.quit,
            new_session: result.new_session,
            route_changed: result.route_changed,
            open_picker: result.open_picker,
            turn,
        }
    }
}

fn provider_state_for(
    cfg: &Config,
    history: &History,
) -> Result<Arc<SessionProviderState>, SwitchError> {
    let catalog = Arc::clone(&cfg.provider_catalog);
    if history.provider_routes().is_empty() {
        Ok(Arc::new(SessionProviderState::from_route(
            catalog,
            cfg.provider_route.clone(),
        )))
    } else {
        SessionProviderState::from_timeline(catalog, history.provider_routes()).map(Arc::new)
    }
}

/// `unset` is kloop's word for "send no effort field at all" and is not a wire
/// value, so it is matched before the level vocabulary.
fn parse_effort_level(
    raw: &str,
) -> Result<Option<kloop_protocol::ReasoningEffort>, kloop_protocol::UnknownReasoningEffort> {
    if raw.eq_ignore_ascii_case("unset") {
        return Ok(None);
    }
    raw.parse::<kloop_protocol::ReasoningEffort>().map(Some)
}

/// Stop everything `cfg` still runs, then leave its worktree. Background work
/// goes first: it may still write through the session, and the worktree must
/// only be retired once nothing can.
pub async fn shutdown_session(cfg: &Config, ui: &Arc<dyn Ui>) -> ShutdownReport {
    let running = cfg.background_executions.running_count() + cfg.background_shells.running_count();
    let missed = cfg.shutdown_background_work(ui).await;
    ShutdownReport {
        running,
        missed,
        worktree: crate::worktree::finish_active(cfg).await,
    }
}

#[cfg(test)]
mod tests;
