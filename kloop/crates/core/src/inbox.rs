//! The step-boundary injection queue (`Config.inbox`).
//!
//! Three producers push here; all are delivered as user messages at round
//! boundaries (never mid-request), and each carries its own framing so the
//! model can tell them apart:
//! - **user steering** (plan 22): text typed while a turn runs.
//! - **background sub-agent/program results** (plans 24/26): detached work's
//!   terminal value or summary.
//! - **background shell notifications** (plan 51): terminal status and output
//!   file pointer; command output stays out of context until read.
//!
//! The queue also signals waiters: [`Inbox::subscribe_activity`] lets the `wait`
//! tool observe activity newer than its own snapshot, and the TUI's idle
//! autowake checks [`Inbox::is_empty`] after every event to decide whether to
//! start a delivery turn. cc and codex independently converge on "enqueue at a
//! step boundary, never interleave with an in-flight request".

use std::sync::Mutex;

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
below — fold it into your work, and if you were waiting on it, continue from here:";

/// Framing for a background program's result reinjected into its parent
/// (plan 24: `run_program {"background": true}`). Like a sub-agent result but
/// from a program the parent launched, not a sub-agent.
const PROGRAM_PREFIX: &str = "A background program you launched has finished. Its return value is \
below — fold it into your work, and if you were waiting on it, continue from here:";

/// Framing for a background shell's terminal notification (plan 51). The
/// command output remains in its file; this message only tells the model that
/// the state changed and where to inspect it.
const SHELL_PREFIX: &str = "A background shell command you started has changed state. Inspect its \
output file if you need the command's result:";

/// One pending injection. Neutral text alone would force a single framing on
/// every producer (the drain used to hard-wrap everything as steering); a typed
/// item lets each producer frame its own message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InboxItem {
    /// User steering typed while the turn runs (plan 22).
    Steer(String),
    /// A background sub-agent's terminal summary, reinjected to its parent
    /// (plan 26). `label` is the "agent-N" id; `summary` is the framed body.
    SubAgentResult { label: String, summary: String },
    /// A background program's return value, reinjected to its parent
    /// (plan 24). `label` is the "program-N" id; `summary` is the return value.
    ProgramResult { label: String, summary: String },
    /// A background shell's terminal state (plan 51). Output is not copied into
    /// context; the output file remains the bounded/read-on-demand source.
    ShellResult {
        id: String,
        status: String,
        output_path: String,
        summary: String,
    },
}

impl InboxItem {
    /// The user-message text this item becomes when drained into history.
    pub fn into_message(self) -> String {
        match self {
            InboxItem::Steer(text) => format!("{STEERING_PREFIX}\n{text}"),
            InboxItem::SubAgentResult { label, summary } => {
                format!("{SUBAGENT_PREFIX}\n[{label}]\n{summary}")
            }
            InboxItem::ProgramResult { label, summary } => {
                format!("{PROGRAM_PREFIX}\n[{label}]\n{summary}")
            }
            InboxItem::ShellResult {
                id,
                status,
                output_path,
                summary,
            } => format!("{SHELL_PREFIX}\n[{id}] {status}\n{summary}\noutput file: {output_path}"),
        }
    }
}

/// A step-boundary injection queue that also signals waiters. Pushing advances
/// an activity generation observed by the `wait` tool; unlike a stored `Notify`
/// permit, an activity already consumed by the agent loop cannot wake a later
/// wait for a different task.
///
/// Each agent gets its OWN inbox: the `task` tool hands a fresh one to every
/// sub-agent (a running sub-agent must never drain the parent's steering), and
/// a background sub-agent reinjects into a *clone of the parent's* inbox held
/// separately from its own.
pub struct Inbox {
    items: Mutex<Vec<InboxItem>>,
    activity: watch::Sender<u64>,
}

impl Default for Inbox {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            items: Mutex::new(Vec::new()),
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
        self.items.lock().unwrap().is_empty()
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
            format!("{SUBAGENT_PREFIX}\n[agent-2]\nfound 3 matches")
        );
        assert_eq!(
            InboxItem::ProgramResult {
                label: "program-1".into(),
                summary: "42".into(),
            }
            .into_message(),
            format!("{PROGRAM_PREFIX}\n[program-1]\n42")
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
