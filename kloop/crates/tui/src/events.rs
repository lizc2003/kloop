//! The bridge from the agent's `Ui`/`Approver` seams to the TUI event loop.
//! [`ChannelUi`] implements both: the core [`Event`] stream is wrapped in
//! [`AgentEvent::Core`] and forwarded over an unbounded channel, and an approval
//! travels as [`AgentEvent::Confirm`] with a oneshot reply. The UI loop is the
//! sole consumer; the worker also constructs `Core` events for the turn bracket
//! (`TurnStarted`/`Usage`/`TurnEnded`) around its `run_turn` call.

use std::pin::Pin;

use kloop_core::agent::Ui;
use kloop_core::event::Event;
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
    /// `/clear` emptied History; the loop resets its transcript view to match.
    ClearTranscript,
    /// `/exit` ran on the worker; the UI loop quits (same clean teardown as a
    /// two-tap Ctrl+C).
    Quit,
    /// The rewind targets the worker read off the session file (plan 18): the
    /// UI loop opens the fork picker with them. Empty means nothing to rewind
    /// to, surfaced as a System note instead.
    ForkPoints(Vec<ForkPoint>),
    /// A rewind completed: History now holds this forked branch, so the loop
    /// rebuilds the transcript from `messages` and adopts the new `session_id`.
    Forked {
        session_id: String,
        messages: Vec<Message>,
    },
    /// A permission prompt. The decision travels back over `reply`; dropping
    /// the sender answers Deny (the agent side treats a closed channel as no).
    Confirm {
        req: ConfirmRequest,
        reply: oneshot::Sender<Decision>,
    },
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

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_core::event::Delta;

    /// Each `Ui::emit` becomes one `AgentEvent::Core`, in call order.
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
            remember_rules: None,
            preview: None,
        };

        let fut = ui.confirm(req.clone());
        let Some(AgentEvent::Confirm { req: got, reply }) = rx.recv().await else {
            panic!("expected a Confirm event");
        };
        assert_eq!(got, req);
        reply.send(Decision::AllowSession).unwrap();
        assert_eq!(fut.await, Decision::AllowSession);
    }

    /// A dropped reply sender (UI gone, turn cancelled) resolves to Deny.
    #[tokio::test]
    async fn dropped_reply_denies() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        let fut = ui.confirm(ConfirmRequest {
            description: "x".into(),
            remember_rules: None,
            preview: None,
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
            remember_rules: None,
            preview: None,
        });
        assert_eq!(fut.await, Decision::Deny);
    }
}
