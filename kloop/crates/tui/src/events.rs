//! The bridge from the agent's `Ui`/`Approver` seams to the TUI event loop.
//! Both traits are implemented by [`ChannelUi`], which forwards everything as
//! [`AgentEvent`]s over an unbounded channel; the UI loop is the sole consumer.

use std::pin::Pin;

use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::permissions::Mode;
use kloop_core::rollout::ForkPoint;
use kloop_core::tools::TodoItem;
use kloop_protocol::Message;
use tokio::sync::mpsc;
use tokio::sync::oneshot;

/// Everything the agent side can tell the UI loop. Rendering state is derived
/// exclusively from this stream (plus key events), which is what makes the
/// transcript logic unit-testable without a terminal.
#[derive(Debug)]
pub enum AgentEvent {
    TextDelta(String),
    ThinkingDelta(String),
    Note(String),
    /// Output of a slash command (`/help`, `/cost`, …). Rendered as a wrapped
    /// system block, not a one-line note — emitted by the worker directly, not
    /// through the `Ui` seam.
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
    /// `agent` is "" for the main agent's own calls, "agent-N" for calls a
    /// sub-agent makes — parallel sub-agents interleave on this stream.
    ToolStart {
        agent: String,
        id: String,
        name: String,
        summary: String,
    },
    ToolEnd {
        agent: String,
        id: String,
        ok: bool,
    },
    /// A task call spawned a sub-agent; ends exactly once per start.
    AgentStart {
        agent: String,
        task: String,
    },
    AgentEnd {
        agent: String,
        ok: bool,
    },
    /// The model rewrote its task list (todo_write, full replacement). Only
    /// the main agent's updates reach the UI loop; a sub-agent's planning
    /// stays internal, like its text.
    TodoUpdate {
        todos: Vec<TodoItem>,
    },
    /// A permission prompt. The decision travels back over `reply`; dropping
    /// the sender answers Deny (the agent side treats a closed channel as no).
    Confirm {
        req: ConfirmRequest,
        reply: oneshot::Sender<Decision>,
    },
    /// The permission mode changed on the agent side (exit_plan_mode was
    /// approved): refresh the status-bar badge so it never lies.
    ModeChanged(Mode),
    TurnEnded(EndReason),
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
    fn text_delta(&self, s: &str) {
        self.send(AgentEvent::TextDelta(s.to_string()));
    }

    fn thinking_delta(&self, s: &str) {
        self.send(AgentEvent::ThinkingDelta(s.to_string()));
    }

    fn note(&self, s: &str) {
        self.send(AgentEvent::Note(s.to_string()));
    }

    fn tool_start(&self, agent: &str, id: &str, name: &str, summary: &str) {
        self.send(AgentEvent::ToolStart {
            agent: agent.to_string(),
            id: id.to_string(),
            name: name.to_string(),
            summary: summary.to_string(),
        });
    }

    fn tool_end(&self, agent: &str, id: &str, ok: bool) {
        self.send(AgentEvent::ToolEnd {
            agent: agent.to_string(),
            id: id.to_string(),
            ok,
        });
    }

    fn agent_start(&self, agent: &str, task: &str) {
        self.send(AgentEvent::AgentStart {
            agent: agent.to_string(),
            task: task.to_string(),
        });
    }

    fn agent_end(&self, agent: &str, ok: bool) {
        self.send(AgentEvent::AgentEnd {
            agent: agent.to_string(),
            ok,
        });
    }

    fn todo_update(&self, agent: &str, todos: &[TodoItem]) {
        // A sub-agent's planning stays internal (lesson 3): only the main
        // agent's list surfaces as a transcript block.
        if agent.is_empty() {
            self.send(AgentEvent::TodoUpdate {
                todos: todos.to_vec(),
            });
        }
    }

    fn mode_changed(&self, mode: Mode) {
        self.send(AgentEvent::ModeChanged(mode));
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

    /// The Ui trait methods map 1:1 onto channel events, in call order.
    #[tokio::test]
    async fn ui_calls_become_ordered_events() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);

        ui.text_delta("hel");
        ui.text_delta("lo");
        ui.tool_start("", "t1", "bash", "{\"command\":\"ls\"}");
        ui.note("retrying");
        ui.tool_end("", "t1", true);
        ui.agent_start("agent-1", "look things up");
        ui.tool_start("agent-1", "t2", "grep", "{\"pattern\":\"x\"}");
        ui.tool_end("agent-1", "t2", true);
        ui.agent_end("agent-1", true);

        let mut got = Vec::new();
        while let Ok(e) = rx.try_recv() {
            got.push(format!("{e:?}"));
        }
        assert_eq!(
            got,
            vec![
                r#"TextDelta("hel")"#,
                r#"TextDelta("lo")"#,
                r#"ToolStart { agent: "", id: "t1", name: "bash", summary: "{\"command\":\"ls\"}" }"#,
                r#"Note("retrying")"#,
                r#"ToolEnd { agent: "", id: "t1", ok: true }"#,
                r#"AgentStart { agent: "agent-1", task: "look things up" }"#,
                r#"ToolStart { agent: "agent-1", id: "t2", name: "grep", summary: "{\"pattern\":\"x\"}" }"#,
                r#"ToolEnd { agent: "agent-1", id: "t2", ok: true }"#,
                r#"AgentEnd { agent: "agent-1", ok: true }"#,
            ]
        );
    }

    /// The main agent's todo_update becomes a TodoUpdate event; a sub-agent's
    /// is dropped (its planning stays internal, like its text).
    #[tokio::test]
    async fn todo_update_forwards_main_agent_only() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ui = ChannelUi::new(tx);
        let items = vec![TodoItem {
            content: "Do it".into(),
            active_form: "Doing it".into(),
            status: kloop_core::tools::TodoStatus::InProgress,
        }];

        ui.todo_update("agent-1", &items); // sub-agent: dropped
        ui.todo_update("", &items); // main agent: forwarded

        let got: Vec<AgentEvent> = std::iter::from_fn(|| rx.try_recv().ok()).collect();
        assert_eq!(got.len(), 1, "only the main agent's update is forwarded");
        assert!(matches!(&got[0], AgentEvent::TodoUpdate { todos } if todos == &items));
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
