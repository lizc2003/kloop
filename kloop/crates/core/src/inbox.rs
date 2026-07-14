//! The step-boundary injection queue (`Config.inbox`).
//!
//! Two producers push here; both are delivered as user messages at round
//! boundaries (never mid-request), and each carries its own framing so the
//! model can tell them apart:
//! - **user steering** (plan 22): text typed while a turn runs.
//! - **background sub-agent results** (plan 26): a fire-and-forget `task`'s
//!   terminal summary, pushed by the detached sub-agent when it finishes.
//!
//! The queue also signals waiters: [`Inbox::notified`] wakes the `wait` tool
//! when anything lands (a completion or new steering), and the TUI's idle
//! autowake checks [`Inbox::is_empty`] after every event to decide whether to
//! start a delivery turn. cc and codex independently converge on "enqueue at a
//! step boundary, never interleave with an in-flight request".

use std::sync::Mutex;

use tokio::sync::Notify;

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
        }
    }
}

/// A step-boundary injection queue that also signals waiters. Pushing wakes any
/// task blocked in [`Inbox::notified`] (the `wait` tool); the drain is done at
/// round boundaries by the agent loop.
///
/// Each agent gets its OWN inbox: the `task` tool hands a fresh one to every
/// sub-agent (a running sub-agent must never drain the parent's steering), and
/// a background sub-agent reinjects into a *clone of the parent's* inbox held
/// separately from its own.
#[derive(Default)]
pub struct Inbox {
    items: Mutex<Vec<InboxItem>>,
    notify: Notify,
}

impl Inbox {
    /// Enqueue an item and wake a waiter. `notify_one` stores a permit if no
    /// one is currently waiting, so a completion that races ahead of `wait` is
    /// not lost (the next `notified()` consumes the permit immediately).
    pub fn push(&self, item: InboxItem) {
        self.items.lock().unwrap().push(item);
        self.notify.notify_one();
    }

    /// Wake a waiter without enqueuing anything. Used when a sub-agent reaches a
    /// terminal state that produces no reinjection (interrupted/aborted): `wait`
    /// should still re-evaluate rather than block out its full deadline.
    pub fn notify_activity(&self) {
        self.notify.notify_one();
    }

    /// Take everything pending, leaving the queue empty.
    pub fn drain(&self) -> Vec<InboxItem> {
        std::mem::take(&mut *self.items.lock().unwrap())
    }

    pub fn is_empty(&self) -> bool {
        self.items.lock().unwrap().is_empty()
    }

    /// A future that resolves the next time [`Inbox::push`] or
    /// [`Inbox::notify_activity`] is called (or immediately if a permit is
    /// already stored). The `wait` tool selects on this against a deadline.
    pub fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.notified()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steer_and_subagent_frame_distinctly() {
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
    async fn notified_wakes_on_push() {
        let inbox = Inbox::default();
        let notified = inbox.notified();
        inbox.push(InboxItem::Steer("x".into()));
        // Resolves promptly; if signalling were broken this would hang the test.
        notified.await;
    }

    #[tokio::test]
    async fn notify_permit_survives_a_race() {
        let inbox = Inbox::default();
        // Push BEFORE anyone waits: notify_one stores a permit.
        inbox.push(InboxItem::Steer("early".into()));
        // A fresh waiter still resolves immediately by consuming the permit.
        inbox.notified().await;
    }
}
