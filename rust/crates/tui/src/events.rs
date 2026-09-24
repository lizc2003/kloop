//! The bridge from the agent's `Ui`/`Approver` seams to the TUI event loop.
//! [`ChannelUi`] implements both: the core [`Event`] stream is wrapped in
//! [`AgentEvent::Core`] and forwarded over an unbounded channel, and an approval
//! travels as [`AgentEvent::Confirm`] with a oneshot reply. The UI loop is the
//! sole consumer; the worker also constructs `Core` events for the turn bracket
//! (`TurnStarted`/`Usage`/`TurnEnded`) around its `run_turn` call.

use std::pin::Pin;

use kloop_core::agent::Ui;
use kloop_core::event::Event;
use kloop_core::interaction::QuestionOutcome;
use kloop_core::interaction::QuestionRequest;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::rollout::ForkPoint;
use kloop_protocol::Message;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Everything the agent side can tell the UI loop. Rendering state is derived
/// exclusively from this stream (plus key events), which is what makes the
/// transcript logic unit-testable without a terminal. Agent output rides
/// [`AgentEvent::Core`]; the rest are UI-control events that never leave the TUI.
#[derive(Debug)]
pub enum AgentEvent {
    /// A core [`Event`] — from the `Ui::emit` seam during a turn, or constructed
    /// by the worker for the turn bracket (`TurnStarted`/`Usage`/`TurnEnded`).
    Core(Event),
    /// Output of a slash command (`/help`, `/cost`, …). Rendered as a wrapped
    /// system block, not a one-line note — emitted by the worker directly.
    System(String),
    ProviderChanged(kloop_protocol::ActiveProviderRoute),
    /// The immutable route snapshot used by the current turn/operation.
    /// ProviderChanged may arrive while this operation is still settling; it
    /// updates the idle selection but must not rewrite this snapshot.
    RouteFrozen(kloop_protocol::ActiveProviderRoute),
    /// Open the route picker on this payload. The stage inside it is the entry
    /// point the command named (`/provider`, `/model`, `/effort`), and Esc there
    /// closes the panel instead of descending a level that command never offered.
    ProviderPicker(kloop_core::provider_route::RoutePicker),
    /// `/clear` ended the session and the worker switched to a new, empty one.
    /// The transcript starts over from `version`'s banner, and the UI loop
    /// wipes the terminal and its scrollback before the next draw.
    Cleared {
        session: SessionSwitch,
        /// The build stamp for the new session's banner.
        version: String,
    },
    /// `/exit` ran on the worker; the UI loop quits (same clean teardown as a
    /// two-tap Ctrl+C).
    Quit,
    /// The rewind targets the worker read off the session file (plan 18): the
    /// UI loop opens the fork picker with them. Empty means nothing to rewind
    /// to, surfaced as a System note instead.
    ForkPoints(Vec<ForkPoint>),
    /// The turn was interrupted before the model produced anything, so it never
    /// entered history or the session file. The transcript drops the cells it
    /// echoed and the composer gets the input back to be edited and resent.
    InputReturned {
        text: String,
        images: Vec<(String, kloop_protocol::ContentBlock)>,
    },
    /// A rewind completed: the worker is on a new session that holds this
    /// forked branch, so the loop rebuilds the transcript from `messages`.
    Forked {
        session: SessionSwitch,
        messages: Vec<Message>,
    },
    /// A permission prompt. The decision travels back over `reply`; dropping
    /// the sender answers Deny (the agent side treats a closed channel as no).
    Confirm {
        req: ConfirmRequest,
        reply: oneshot::Sender<Decision>,
    },
    /// A general product question, separate from permission approval.
    Question {
        req: QuestionRequest,
        reply: oneshot::Sender<QuestionOutcome>,
    },
}

/// What the UI mirrors of a session the worker has just switched to (`/clear`,
/// rewind). The new session's inbox and permission gate are not in here: the
/// UI loop picks those up from the worker's current Config.
#[derive(Debug)]
pub struct SessionSwitch {
    pub session_id: String,
    pub route: kloop_protocol::ActiveProviderRoute,
    pub mode: kloop_core::permissions::Mode,
    /// Display-ready working directory, and its branch if in a repository.
    pub cwd: String,
    pub branch: Option<String>,
    pub todos: kloop_core::tools::TodoSnapshot,
    /// What retiring the old session did, one line each.
    pub report: Vec<String>,
}

/// `Ui` + `Approver` implementation that lives on the agent task and speaks
/// to the UI loop only through the event channel. Send failures are ignored:
/// they mean the UI loop is gone and the process is exiting anyway.
pub struct ChannelUi {
    tx: mpsc::UnboundedSender<AgentEvent>,
}

impl ChannelUi {
    pub fn new(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self { tx }
    }

    pub fn send(&self, event: AgentEvent) {
        let _ = self.tx.send(event);
    }
}

impl Ui for ChannelUi {
    fn emit(&self, ev: &Event) {
        // The channel carries owned events; the render state machine (App) is
        // the single place that projects them (e.g. dropping a sub-agent's todo).
        self.send(AgentEvent::Core(ev.clone()));
    }
}

impl Approver for ChannelUi {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(AgentEvent::Confirm { req, reply }).is_ok();
        Box::pin(async move {
            if !sent {
                return Decision::Deny;
            }
            rx.await.unwrap_or(Decision::Deny)
        })
    }
}

impl Questioner for ChannelUi {
    fn ask(
        &self,
        req: QuestionRequest,
    ) -> Pin<Box<dyn std::future::Future<Output = QuestionOutcome> + Send + '_>> {
        let (reply, rx) = oneshot::channel();
        let sent = self.tx.send(AgentEvent::Question { req, reply }).is_ok();
        Box::pin(async move {
            if !sent {
                return QuestionOutcome::Unavailable("TUI event channel is closed".into());
            }
            rx.await.unwrap_or_else(|_| {
                QuestionOutcome::Unavailable("TUI question reply channel was dropped".into())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_core::event::Delta;

    fn question_request() -> QuestionRequest {
        QuestionRequest {
            questions: vec![kloop_core::interaction::Question {
                question: "Which?".into(),
                header: "Choice".into(),
                options: vec![
                    kloop_core::interaction::QuestionOption {
                        label: "A".into(),
                        description: "first".into(),
                        preview: None,
                    },
                    kloop_core::interaction::QuestionOption {
                        label: "B".into(),
                        description: "second".into(),
                        preview: None,
                    },
                ],
                multi_select: false,
            }],
            metadata: None,
        }
    }

    #[tokio::test]
    async fn question_round_trips_and_channel_loss_is_unavailable() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        let req = question_request();
        let fut = ui.ask(req.clone());
        let Some(AgentEvent::Question { req: got, reply }) = rx.recv().await else {
            panic!("expected a Question event");
        };
        assert_eq!(got, req);
        let answer = kloop_core::interaction::QuestionAnswer {
            question_index: 0,
            selected: vec![1],
            other: None,
            notes: None,
        };
        reply
            .send(QuestionOutcome::Answered(vec![answer.clone()]))
            .unwrap();
        assert_eq!(fut.await, QuestionOutcome::Answered(vec![answer]));

        let fut = ui.ask(question_request());
        let Some(AgentEvent::Question { reply, .. }) = rx.recv().await else {
            panic!("expected a Question event");
        };
        drop(reply);
        assert!(matches!(fut.await, QuestionOutcome::Unavailable(_)));

        drop(rx);
        assert!(matches!(
            ui.ask(question_request()).await,
            QuestionOutcome::Unavailable(_)
        ));
    }
    #[tokio::test]
    async fn emit_wraps_core_events_in_order() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);

        ui.emit(&Event::ItemDelta {
            id: "m0".into(),
            delta: Delta::Text("hi".into()),
        });
        ui.emit(&Event::Note("retrying".into()));

        let got: Vec<AgentEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(got.len(), 2);
        assert!(matches!(
            &got[0],
            AgentEvent::Core(Event::ItemDelta { delta: Delta::Text(t), .. }) if t == "hi"
        ));
        assert!(matches!(
            &got[1],
            AgentEvent::Core(Event::Note(n)) if n == "retrying"
        ));
    }

    #[tokio::test]
    async fn confirm_round_trips_the_decision() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        let req = ConfirmRequest {
            description: "bash: rm -rf /tmp/x".into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
            ..Default::default()
        };

        let fut = ui.confirm(req.clone());
        let Some(AgentEvent::Confirm { req: got, reply }) = rx.recv().await else {
            panic!("expected a Confirm event");
        };
        assert_eq!(got, req);
        reply
            .send(Decision::Allow(
                kloop_core::permissions::ApprovalScope::WorkspaceSession,
            ))
            .unwrap();
        assert_eq!(
            fut.await,
            Decision::Allow(kloop_core::permissions::ApprovalScope::WorkspaceSession)
        );
    }

    /// A dropped reply sender (UI gone, turn cancelled) resolves to Deny.
    #[tokio::test]
    async fn dropped_reply_denies() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        let fut = ui.confirm(ConfirmRequest {
            description: "x".into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
            ..Default::default()
        });
        let Some(AgentEvent::Confirm { reply, .. }) = rx.recv().await else {
            panic!("expected a Confirm event");
        };
        drop(reply);
        assert_eq!(fut.await, Decision::Deny);

        // And a closed event channel denies without hanging.
        drop(rx);
        let fut = ui.confirm(ConfirmRequest {
            description: "y".into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
            ..Default::default()
        });
        assert_eq!(fut.await, Decision::Deny);
    }
}
