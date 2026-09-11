//! The step-boundary injection queue (`Config.inbox`).
//!
//! Producers push typed items here; the Local Agent directory exposes its own
//! bounded pending count through the same queue's activity signal. Everything is
//! delivered as user messages at round boundaries (never mid-request), and each
//! producer carries distinct framing so the model can tell steering, peer
//! messages, background results, shell pointers, and scheduler work apart.
//!
//! The queue also signals waiters: [`Inbox::subscribe_activity`] lets the `wait`
//! tool observe activity newer than its own snapshot, and the TUI's idle
//! autowake checks [`Inbox::is_empty`] after every event to decide whether to
//! start a delivery turn. cc and codex independently converge on "enqueue at a
//! step boundary, never interleave with an in-flight request".

use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use kloop_protocol::Injected;
use kloop_protocol::LocalAgentId;
use kloop_protocol::LocalAgentMessage;
use kloop_protocol::LocalMessageId;
use kloop_protocol::Message;
use tokio::sync::watch;

/// Framing for a steering message (typed while the turn was running). Recorded
/// as a user message at the next round boundary so the model treats it as a
/// mid-work interjection to fold in, not a brand-new task. cc frames steers the
/// same way ("The user sent a new message while you were working…"); codex
/// records them as plain user prompts — framing is the cheap side that helps
/// weaker models, so kloop adopts it.
pub const STEERING_PREFIX: &str = "The user sent this message while you were working. Address it \
as part of the current task — finish any step already in progress, then act on it:";

/// Framing for a background sub-agent's result reinjected into its parent
/// (plan 26). Distinct from steering: this is not the user talking, it is a
/// task the parent dispatched reporting back. Weaker models need the explicit
/// cue not to mistake it for a fresh user request.
const SUBAGENT_PREFIX: &str = "A background sub-agent you dispatched has finished. Its result is \
below — fold it into your work, and if you were waiting on it, continue from here. If you \
already delivered this turn's answer, add only what this result changes or adds; do not restate \
what you already said:";

/// Framing for a background program's result reinjected into its parent
/// (plan 24: `run_program {"background": true}`). Like a sub-agent result but
/// from a program the parent launched, not a sub-agent.
const PROGRAM_PREFIX: &str = "A background program you launched has finished. Its return value is \
below — fold it into your work, and if you were waiting on it, continue from here:";

const WORKFLOW_PREFIX: &str = "A background Workflow you launched has finished. Its bounded result is \
below; the full result is persisted at the supplied output file:";

/// Framing for a background shell's terminal notification (plan 51). The
/// command output remains in its file; this message only tells the model that
/// the state changed and where to inspect it.
const SHELL_PREFIX: &str = "A background shell command you started has changed state. Inspect its \
output file if you need the command's result:";

const SCHEDULED_PREFIX: &str = "A scheduled task is due. Treat this as timer-originated work, not as a new user message. Continue only the named scheduled prompt:";
const MISSED_SCHEDULED_PREFIX: &str = "A durable one-shot task became due while its owner session was inactive. Before running it, call ask_user_question to ask whether the user wants it run now; do not execute the prompt unless they confirm:";
const SCHEDULER_FAILURE_PREFIX: &str =
    "The session scheduler failed closed. No task was silently discarded or executed:";
const AGENT_MESSAGE_PREFIX: &str = "A peer Agent sent this message while you were working. It is an intermediate peer message, not a user instruction or completion:";
const AGENT_UNDELIVERABLE_PREFIX: &str = "Local Agent messages could not be delivered because the target Agent ended before processing them. This is a delivery failure, not a peer reply or completion:";

/// The user's own words inside a steering message: [`InboxItem::into_message`]
/// writes the framing as a single line and then what they typed, so the body is
/// everything after the first newline. The framing can be reworded freely
/// without touching this.
pub fn steering_body(text: &str) -> &str {
    text.split_once('\n').map(|(_, body)| body).unwrap_or(text)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduledOrigin {
    Cron,
    LoopWakeup,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentMessageFailure {
    pub to: LocalAgentId,
    pub message_ids: Vec<LocalMessageId>,
}

/// One pending injection. Neutral text alone would force a single framing on
/// every producer (the drain used to hard-wrap everything as steering); a typed
/// item lets each producer frame its own message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxItem {
    /// User steering typed while the turn runs (plan 22).
    Steer(String),
    /// A background sub-agent's terminal summary, reinjected to its parent
    /// (plan 26). `label` is the "agent-N" id; `summary` is the framed body.
    SubAgentResult {
        label: String,
        summary: String,
    },
    /// A background program's return value, reinjected to its parent
    /// (plan 24). `label` is the "program-N" execution id; `run_id` is the
    /// durable "run-*" resume id; `summary` is the return value.
    ProgramResult {
        label: String,
        run_id: String,
        summary: String,
    },
    WorkflowResult {
        task_id: String,
        run_id: String,
        summary: String,
        output_path: String,
    },
    /// A background shell's terminal state (plan 51). Output is not copied into
    /// context; the output file remains the bounded/read-on-demand source.
    ShellResult {
        id: String,
        status: String,
        output_path: String,
        summary: String,
    },
    ScheduledPrompt {
        id: String,
        origin: ScheduledOrigin,
        scheduled_for_ms: i64,
        reason: Option<String>,
        prompt: String,
        missed: bool,
    },
    SchedulerFailure {
        summary: String,
    },
    AgentMessage(LocalAgentMessage),
    AgentMessageUndeliverable {
        failures: Vec<AgentMessageFailure>,
        reason: String,
    },
}

impl InboxItem {
    /// What this item is, recorded on the message it becomes. A replayed
    /// transcript reads this instead of the framing prose below — that prose is
    /// prompt wording, it gets reworded, and matching on it would silently stop
    /// recognising every session written before the rewording.
    pub fn kind(&self) -> Injected {
        match self {
            InboxItem::Steer(_) => Injected::Steering,
            InboxItem::SubAgentResult { label, .. } => Injected::SubAgent {
                label: label.clone(),
            },
            InboxItem::ProgramResult { label, .. } => Injected::Program {
                label: label.clone(),
            },
            InboxItem::WorkflowResult { task_id, .. } => Injected::Workflow {
                task_id: task_id.clone(),
            },
            InboxItem::ShellResult { id, .. } => Injected::Shell { id: id.clone() },
            InboxItem::ScheduledPrompt { id, missed, .. } => {
                let id = id.clone();
                if *missed {
                    Injected::MissedScheduled { id }
                } else {
                    Injected::Scheduled { id }
                }
            }
            InboxItem::SchedulerFailure { .. } => Injected::SchedulerFailure,
            InboxItem::AgentMessage(message) => Injected::PeerMessage {
                from: message.from.to_string(),
            },
            InboxItem::AgentMessageUndeliverable { .. } => Injected::PeerUndeliverable,
        }
    }

    /// The history message this item becomes: the framed text the model reads,
    /// carrying the identity a reader needs.
    pub fn into_user_message(self) -> Message {
        Message::injected(self.kind(), self.into_message())
    }

    /// The user-message text this item becomes when drained into history. Every
    /// framing is ONE line, followed by the item's own detail — [`steering_body`]
    /// depends on that, and so does anything else that needs the payload without
    /// the frame.
    pub fn into_message(self) -> String {
        match self {
            InboxItem::Steer(text) => format!("{STEERING_PREFIX}\n{text}"),
            InboxItem::SubAgentResult { label, summary } => {
                format!("{SUBAGENT_PREFIX}\n[Agent {label}]\n{summary}")
            }
            InboxItem::ProgramResult {
                label,
                run_id,
                summary,
            } => {
                format!("{PROGRAM_PREFIX}\n[Program {label}] run {run_id}\n{summary}")
            }
            InboxItem::WorkflowResult {
                task_id,
                run_id,
                summary,
                output_path,
            } => format!(
                "{WORKFLOW_PREFIX}\n[Workflow {task_id}] run {run_id}\n{summary}\noutput file: {output_path}"
            ),
            InboxItem::ShellResult {
                id,
                status,
                output_path,
                summary,
            } => format!("{SHELL_PREFIX}\n[{id}] {status}\n{summary}\noutput file: {output_path}"),
            InboxItem::ScheduledPrompt {
                id,
                origin,
                scheduled_for_ms,
                reason,
                prompt,
                missed,
            } => {
                let origin = match origin {
                    ScheduledOrigin::Cron => "cron",
                    ScheduledOrigin::LoopWakeup => "loop wakeup",
                };
                let reason = reason
                    .as_deref()
                    .map(|value| format!("\nreason: {value}"))
                    .unwrap_or_default();
                let prefix = if missed {
                    MISSED_SCHEDULED_PREFIX
                } else {
                    SCHEDULED_PREFIX
                };
                format!(
                    "{prefix}\n[{id}] origin: {origin}; scheduled_for_ms: {scheduled_for_ms}{reason}\n{prompt}"
                )
            }
            InboxItem::SchedulerFailure { summary } => {
                format!("{SCHEDULER_FAILURE_PREFIX}\n{summary}")
            }
            InboxItem::AgentMessage(message) => format!(
                "{AGENT_MESSAGE_PREFIX}\n[{} from {}] {}\n{}",
                message.message_id,
                message.from,
                message.summary,
                message.text_body()
            ),
            InboxItem::AgentMessageUndeliverable { failures, reason } => {
                let failures = failures
                    .into_iter()
                    .map(|failure| {
                        let ids = failure
                            .message_ids
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("[target {}] {ids}", failure.to)
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                format!("{AGENT_UNDELIVERABLE_PREFIX}\n{failures}\n{reason}")
            }
        }
    }
}

/// A step-boundary injection queue that also signals waiters. Pushing advances
/// an activity generation observed by `wait_for_activity`; unlike a stored
/// `Notify` permit, an activity already consumed by the agent loop cannot wake a
/// later wait for different background work.
///
/// Each agent gets its OWN inbox: `run_agent` hands a fresh one to every
/// sub-agent (a running sub-agent must never drain the parent's steering), and
/// a background sub-agent reinjects into a *clone of the parent's* inbox held
/// separately from its own.
pub struct Inbox {
    items: Mutex<Vec<InboxItem>>,
    local_pending: AtomicUsize,
    activity: watch::Sender<u64>,
}

impl Default for Inbox {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            items: Mutex::new(Vec::new()),
            local_pending: AtomicUsize::new(0),
            activity,
        }
    }
}

impl Inbox {
    fn advance_activity(&self) {
        let next = (*self.activity.borrow()).wrapping_add(1);
        self.activity.send_replace(next);
    }

    /// Enqueue an item and advance the activity generation. A waiter subscribes
    /// before checking the queue, so a racing push is observed without retaining
    /// a stale permit after another consumer drains the item.
    pub fn push(&self, item: InboxItem) {
        self.items.lock().unwrap().push(item);
        self.advance_activity();
    }

    /// Wake a waiter without enqueuing anything. Used when a sub-agent reaches a
    /// terminal state that produces no reinjection (interrupted/aborted): `wait`
    /// should still re-evaluate rather than block out its full deadline.
    pub fn notify_activity(&self) {
        self.advance_activity();
    }

    /// Take everything pending, leaving the queue empty.
    pub fn drain(&self) -> Vec<InboxItem> {
        std::mem::take(&mut *self.items.lock().unwrap())
    }

    pub fn is_empty(&self) -> bool {
        self.items.lock().unwrap().is_empty() && self.local_pending.load(Ordering::Acquire) == 0
    }

    pub(crate) fn add_local_pending(&self, count: usize) {
        self.local_pending.fetch_add(count, Ordering::Release);
    }

    /// Saturating: an over-release must not wrap the counter. A wrapped value
    /// leaves `is_empty()` false forever, and the TUI's idle autowake then
    /// starts a delivery turn on every event — an agent that looks like it is
    /// talking to itself, with no way back short of restarting the session.
    ///
    /// This used to be `fetch_sub` guarded by a `debug_assert`, which is the
    /// one configuration the failure cannot occur in: the assert fires in tests
    /// and the wrap happens in release. The counter saturates instead, so an
    /// imbalance costs one undelivered wake rather than the session.
    pub(crate) fn remove_local_pending(&self, count: usize) {
        let _ = self
            .local_pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(pending.saturating_sub(count))
            });
    }

    #[cfg(test)]
    pub(crate) fn local_pending(&self) -> usize {
        self.local_pending.load(Ordering::Acquire)
    }

    /// Subscribe to activity from the current generation onward. Call this
    /// before checking the queue/registries, then await `changed()` only if the
    /// checks still say there is work to wait for.
    pub fn subscribe_activity(&self) -> watch::Receiver<u64> {
        self.activity.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An over-release used to wrap the counter, and a wrapped counter leaves
    /// `is_empty()` false forever — the TUI's idle autowake then starts a
    /// delivery turn on every event. The old `debug_assert` fired only in the
    /// build where the wrap cannot happen, so it never protected anything.
    #[test]
    fn releasing_more_local_pending_than_was_added_saturates_at_empty() {
        let inbox = Inbox::default();
        inbox.add_local_pending(1);
        assert!(!inbox.is_empty());

        inbox.remove_local_pending(3);
        assert_eq!(inbox.local_pending(), 0);
        assert!(
            inbox.is_empty(),
            "an over-release must not wrap the counter"
        );

        // Still usable afterwards: the queue is not stuck non-empty.
        inbox.add_local_pending(2);
        inbox.remove_local_pending(2);
        assert!(inbox.is_empty());
    }

    /// The producer records what it made, so replay never has to read the
    /// framing prose — which is precisely the part that gets reworded.
    #[test]
    fn every_item_records_its_kind_and_steering_keeps_the_user_words() {
        let steer = InboxItem::Steer("check logs".into());
        assert_eq!(steer.kind(), Injected::Steering);
        let message = steer.into_user_message();
        assert_eq!(message.injected, Some(Injected::Steering));
        let [kloop_protocol::ContentBlock::Text { text }] = message.content.as_slice() else {
            panic!("a framed steering message is one text block: {message:?}")
        };
        assert_eq!(steering_body(text), "check logs");

        assert_eq!(
            InboxItem::SubAgentResult {
                label: "agent-2".into(),
                summary: "found 3 matches".into(),
            }
            .kind(),
            Injected::SubAgent {
                label: "agent-2".into()
            }
        );
        assert_eq!(
            InboxItem::ShellResult {
                id: "bg-3".into(),
                status: "completed".into(),
                output_path: "/tmp/bg-3.out".into(),
                summary: "done".into(),
            }
            .kind(),
            Injected::Shell { id: "bg-3".into() }
        );
        // The same variant carries two identities; `missed` picks which.
        let scheduled = |missed| InboxItem::ScheduledPrompt {
            id: "task-1".into(),
            origin: ScheduledOrigin::Cron,
            scheduled_for_ms: 0,
            reason: None,
            prompt: "run it".into(),
            missed,
        };
        assert_eq!(
            scheduled(false).kind(),
            Injected::Scheduled {
                id: "task-1".into()
            }
        );
        assert_eq!(
            scheduled(true).kind(),
            Injected::MissedScheduled {
                id: "task-1".into()
            }
        );
    }

    /// Every framing is one line, which is what lets the payload be recovered
    /// without matching the prose. A reworded framing must not break that.
    #[test]
    fn every_framing_is_a_single_line() {
        let items = [
            InboxItem::Steer("s".into()),
            InboxItem::SubAgentResult {
                label: "agent-1".into(),
                summary: "s".into(),
            },
            InboxItem::ProgramResult {
                label: "program-1".into(),
                run_id: "r".into(),
                summary: "s".into(),
            },
            InboxItem::SchedulerFailure {
                summary: "s".into(),
            },
        ];
        for item in items {
            let text = item.into_message();
            let (framing, _) = text.split_once('\n').expect("framing then payload");
            assert!(!framing.is_empty() && framing.ends_with(':'), "{framing}");
        }
    }

    /// The clause exists because a background result can land after the parent
    /// already delivered its answer and ended its turn: without it the model's
    /// default is to rewrite the whole answer, and the user watches one review
    /// scroll past twice (measured: 2026-09-03 dogfood, 15 minutes apart).
    #[test]
    fn subagent_framing_asks_for_the_delta_once_delivered() {
        assert!(SUBAGENT_PREFIX.contains("already delivered this turn's answer"));
        assert!(SUBAGENT_PREFIX.contains("do not restate what you already said"));
    }

    #[test]
    fn item_kinds_have_distinct_framing() {
        assert_eq!(
            InboxItem::Steer("check logs".into()).into_message(),
            format!("{STEERING_PREFIX}\ncheck logs")
        );
        assert_eq!(
            InboxItem::SubAgentResult {
                label: "agent-2".into(),
                summary: "found 3 matches".into(),
            }
            .into_message(),
            format!("{SUBAGENT_PREFIX}\n[Agent agent-2]\nfound 3 matches")
        );
        assert_eq!(
            InboxItem::ProgramResult {
                label: "program-1".into(),
                run_id: "run-1".into(),
                summary: "42".into(),
            }
            .into_message(),
            format!("{PROGRAM_PREFIX}\n[Program program-1] run run-1\n42")
        );
        assert_eq!(
            InboxItem::WorkflowResult {
                task_id: "workflow-4".into(),
                run_id: "wf_123".into(),
                summary: "verified".into(),
                output_path: "/tmp/workflow.json".into(),
            }
            .into_message(),
            format!(
                "{WORKFLOW_PREFIX}\n[Workflow workflow-4] run wf_123\nverified\noutput file: /tmp/workflow.json"
            )
        );
        assert_eq!(
            InboxItem::ShellResult {
                id: "bg-3".into(),
                status: "completed".into(),
                output_path: "/tmp/bg-3.out".into(),
                summary: "Background command completed (exit code 0)".into(),
            }
            .into_message(),
            format!(
                "{SHELL_PREFIX}\n[bg-3] completed\nBackground command completed (exit code 0)\noutput file: /tmp/bg-3.out"
            )
        );
        let peer = LocalAgentMessage::text(
            LocalMessageId::new(7).unwrap(),
            kloop_protocol::LocalContextId::new("local-context-1").unwrap(),
            "agent-3".parse().unwrap(),
            LocalAgentId::Main,
            "review close".into(),
            "check the race".into(),
        );
        assert_eq!(
            InboxItem::AgentMessage(peer).into_message(),
            format!(
                "{AGENT_MESSAGE_PREFIX}\n[message-7 from agent-3] review close\ncheck the race"
            )
        );
        assert_eq!(
            InboxItem::AgentMessageUndeliverable {
                failures: vec![AgentMessageFailure {
                    to: "agent-4".parse().unwrap(),
                    message_ids: vec![
                        LocalMessageId::new(8).unwrap(),
                        LocalMessageId::new(9).unwrap(),
                    ],
                }],
                reason: "target stopped".into(),
            }
            .into_message(),
            format!(
                "{AGENT_UNDELIVERABLE_PREFIX}\n[target agent-4] message-8, message-9\ntarget stopped"
            )
        );
    }

    #[test]
    fn push_drain_roundtrips_and_empties() {
        let inbox = Inbox::default();
        assert!(inbox.is_empty());
        inbox.push(InboxItem::Steer("a".into()));
        inbox.push(InboxItem::SubAgentResult {
            label: "agent-1".into(),
            summary: "b".into(),
        });
        assert!(!inbox.is_empty());
        let drained = inbox.drain();
        assert_eq!(drained.len(), 2);
        assert!(inbox.is_empty());
    }

    #[tokio::test]
    async fn activity_wakes_on_push() {
        let inbox = Inbox::default();
        let mut activity = inbox.subscribe_activity();
        inbox.push(InboxItem::Steer("x".into()));
        // Resolves promptly; if signalling were broken this would hang the test.
        activity.changed().await.unwrap();
    }

    #[tokio::test]
    async fn subscriber_observes_only_newer_activity() {
        let inbox = Inbox::default();
        inbox.push(InboxItem::Steer("old".into()));
        inbox.drain();

        let mut activity = inbox.subscribe_activity();
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), activity.changed())
                .await
                .is_err(),
            "drained activity must not leave a permit for a later wait"
        );

        inbox.notify_activity();
        activity.changed().await.unwrap();
    }
}
