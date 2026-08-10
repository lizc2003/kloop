use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use kloop_protocol::LocalAgentId;
use kloop_protocol::LocalAgentMessage;
use kloop_protocol::LocalContextId;
use kloop_protocol::LocalMessageId;

use crate::agent::Ui;
use crate::event::AgentMessageStatus;
use crate::event::AgentMessageUpdate;
use crate::event::Event;
use crate::inbox::AgentMessageFailure;
use crate::inbox::Inbox;
use crate::inbox::InboxItem;

pub const MAX_MESSAGE_BYTES: usize = 8 * 1024;
pub const MAX_SUMMARY_CHARS: usize = 200;
pub const MAX_SUMMARY_BYTES: usize = 1024;
const MAX_MAILBOX_MESSAGES: usize = 32;
const MAX_MAILBOX_BODY_BYTES: usize = 128 * 1024;
const MAX_BOUNDARY_MESSAGES: usize = 8;
const MAX_BOUNDARY_BODY_BYTES: usize = 32 * 1024;
const MAX_AGENT_SENT_MESSAGES: usize = 64;
const MAX_SESSION_MESSAGES: usize = 256;
const MAX_SESSION_BODY_BYTES: usize = 512 * 1024;
const MAX_FAILURE_REASON_CHARS: usize = 240;

static CONTEXT_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone)]
pub struct LocalAgentContext {
    current: LocalAgentId,
    parent: Option<LocalAgentId>,
    directory: Arc<LiveAgentDirectory>,
}

impl LocalAgentContext {
    pub fn root(inbox: Arc<Inbox>) -> Self {
        let sequence = CONTEXT_SEQ.fetch_add(1, Ordering::Relaxed);
        let context_id = LocalContextId::new(format!("local-context-{sequence}"))
            .expect("generated local context id is valid");
        let directory = Arc::new(LiveAgentDirectory::new(context_id, inbox));
        Self {
            current: LocalAgentId::Main,
            parent: None,
            directory,
        }
    }

    pub fn child(&self, current: LocalAgentId) -> Self {
        debug_assert!(!current.is_main());
        Self {
            current,
            parent: Some(self.current.clone()),
            directory: Arc::clone(&self.directory),
        }
    }

    pub fn agent_id(&self) -> &LocalAgentId {
        &self.current
    }

    pub fn parent_agent_id(&self) -> Option<&LocalAgentId> {
        self.parent.as_ref()
    }

    pub fn agent_label(&self) -> &str {
        self.current.display_label()
    }

    pub fn register_child(
        &self,
        inbox: Arc<Inbox>,
        agent_type: Option<&str>,
        description: &str,
        ui: Arc<dyn Ui>,
    ) -> Result<LiveAgentLease, String> {
        self.directory.register_child(
            self.current.clone(),
            self.parent.clone(),
            inbox,
            agent_type,
            description,
            ui,
        )
    }

    pub fn send(
        &self,
        to: LocalAgentId,
        summary: String,
        body: String,
        ui: &Arc<dyn Ui>,
    ) -> Result<LocalMessageId, String> {
        self.directory.send(&self.current, to, summary, body, ui)
    }

    pub fn list_agents(&self) -> Result<Vec<LocalAgentRosterEntry>, String> {
        self.directory.list_agents(&self.current)
    }

    pub fn claim_boundary(&self) -> Option<MailboxBatch> {
        self.directory.claim_boundary(&self.current)
    }

    pub fn can_finish_naturally(&self) -> bool {
        self.directory.can_finish_naturally(&self.current)
    }

    pub fn shutdown(&self, ui: &Arc<dyn Ui>) {
        self.directory.shutdown(ui);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocalAgentRosterEntry {
    pub id: LocalAgentId,
    pub parent_id: Option<LocalAgentId>,
    pub agent_type: Option<String>,
    pub description: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EntryState {
    Open,
    Closing,
    Closed,
}

#[derive(Clone)]
enum MailboxItem {
    Peer(LocalAgentMessage),
    Undeliverable {
        failures: Vec<AgentMessageFailure>,
        reason: String,
    },
}

impl MailboxItem {
    fn as_inbox_item(&self) -> InboxItem {
        match self {
            Self::Peer(message) => InboxItem::AgentMessage(message.clone()),
            Self::Undeliverable { failures, reason } => InboxItem::AgentMessageUndeliverable {
                failures: failures.clone(),
                reason: reason.clone(),
            },
        }
    }

    fn body_bytes(&self) -> usize {
        match self {
            Self::Peer(message) => message.text_body().len(),
            Self::Undeliverable { .. } => 0,
        }
    }
}

struct LiveEntry {
    parent_id: Option<LocalAgentId>,
    inbox: Arc<Inbox>,
    agent_type: Option<String>,
    description: String,
    state: EntryState,
    sent_messages: usize,
    pending_messages: usize,
    pending_body_bytes: usize,
    queue: VecDeque<MailboxItem>,
    claims: HashMap<u64, Vec<MailboxItem>>,
    next_claim: u64,
}

impl LiveEntry {
    fn main(inbox: Arc<Inbox>) -> Self {
        Self {
            parent_id: None,
            inbox,
            agent_type: None,
            description: "main".into(),
            state: EntryState::Open,
            sent_messages: 0,
            pending_messages: 0,
            pending_body_bytes: 0,
            queue: VecDeque::new(),
            claims: HashMap::new(),
            next_claim: 1,
        }
    }

    fn has_pending(&self) -> bool {
        !self.queue.is_empty() || !self.claims.is_empty()
    }
}

fn queue_failure_notice(entry: &mut LiveEntry, failure: AgentMessageFailure, reason: &str) -> bool {
    if let Some(MailboxItem::Undeliverable { failures, .. }) = entry.queue.iter_mut().find(
        |item| matches!(item, MailboxItem::Undeliverable { reason: queued, .. } if queued == reason),
    ) {
        failures.push(failure);
        return false;
    }
    entry.queue.push_back(MailboxItem::Undeliverable {
        failures: vec![failure],
        reason: reason.to_string(),
    });
    true
}

struct DirectoryState {
    closing: bool,
    context_id: LocalContextId,
    next_message: u64,
    accepted_messages: usize,
    accepted_body_bytes: usize,
    entries: HashMap<LocalAgentId, LiveEntry>,
}

pub struct LiveAgentDirectory {
    state: Mutex<DirectoryState>,
}

impl LiveAgentDirectory {
    fn new(context_id: LocalContextId, main_inbox: Arc<Inbox>) -> Self {
        let mut entries = HashMap::new();
        entries.insert(LocalAgentId::Main, LiveEntry::main(main_inbox));
        Self {
            state: Mutex::new(DirectoryState {
                closing: false,
                context_id,
                next_message: 1,
                accepted_messages: 0,
                accepted_body_bytes: 0,
                entries,
            }),
        }
    }

    fn register_child(
        self: &Arc<Self>,
        id: LocalAgentId,
        parent_id: Option<LocalAgentId>,
        inbox: Arc<Inbox>,
        agent_type: Option<&str>,
        description: &str,
        ui: Arc<dyn Ui>,
    ) -> Result<LiveAgentLease, String> {
        if id.is_main() {
            return Err("main is registered with the session root".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.closing {
            return Err("the session is closing; no new local Agent may register".into());
        }
        if state.entries.contains_key(&id) {
            return Err(format!("local Agent {id} is already registered"));
        }
        let Some(parent_id) = parent_id else {
            return Err(format!("local Agent {id} has no parent identity"));
        };
        if !state
            .entries
            .get(&parent_id)
            .is_some_and(|parent| parent.state == EntryState::Open)
        {
            return Err(format!("parent Agent {parent_id} is not open"));
        }
        state.entries.insert(
            id.clone(),
            LiveEntry {
                parent_id: Some(parent_id),
                inbox,
                agent_type: agent_type.map(|value| bounded_roster_text(value, "Agent")),
                description: bounded_roster_text(description, "Agent"),
                state: EntryState::Open,
                sent_messages: 0,
                pending_messages: 0,
                pending_body_bytes: 0,
                queue: VecDeque::new(),
                claims: HashMap::new(),
                next_claim: 1,
            },
        );
        Ok(LiveAgentLease {
            directory: Arc::clone(self),
            id,
            ui,
            armed: true,
        })
    }

    fn send(
        &self,
        sender: &LocalAgentId,
        to: LocalAgentId,
        summary: String,
        body: String,
        ui: &Arc<dyn Ui>,
    ) -> Result<LocalMessageId, String> {
        validate_payload(&summary, &body)?;
        if sender == &to {
            return Err(format!(
                "cannot send a local Agent message to self ({sender})"
            ));
        }
        let (message_id, target_inbox) = {
            let mut state = self.state.lock().unwrap();
            if state.closing {
                return Err("the session is closing; local Agent messages are unavailable".into());
            }
            let Some(sender_entry) = state.entries.get(sender) else {
                return Err(format!("sender {sender} is not registered in this session"));
            };
            if sender_entry.state != EntryState::Open {
                return Err(format!("sender {sender} is not open"));
            }
            if sender_entry.sent_messages >= MAX_AGENT_SENT_MESSAGES {
                return Err(format!(
                    "sender {sender} reached the {MAX_AGENT_SENT_MESSAGES}-message session limit"
                ));
            }
            let Some(target_entry) = state.entries.get(&to) else {
                return Err(format!(
                    "target {to} is not a live local Agent in this session"
                ));
            };
            if target_entry.state != EntryState::Open {
                return Err(format!("target {to} is closing or closed"));
            }
            if target_entry.pending_messages >= MAX_MAILBOX_MESSAGES {
                return Err(format!(
                    "target {to} mailbox already holds {MAX_MAILBOX_MESSAGES} pending messages"
                ));
            }
            if target_entry.pending_body_bytes + body.len() > MAX_MAILBOX_BODY_BYTES {
                return Err(format!(
                    "target {to} mailbox would exceed the {MAX_MAILBOX_BODY_BYTES}-byte body limit"
                ));
            }
            if state.accepted_messages >= MAX_SESSION_MESSAGES {
                return Err(format!(
                    "the session reached the {MAX_SESSION_MESSAGES}-message local mailbox limit"
                ));
            }
            if state.accepted_body_bytes + body.len() > MAX_SESSION_BODY_BYTES {
                return Err(format!(
                    "the session would exceed the {MAX_SESSION_BODY_BYTES}-byte local mailbox body limit"
                ));
            }

            let message_id = LocalMessageId::new(state.next_message)
                .expect("local message sequence starts at one");
            state.next_message = state.next_message.saturating_add(1);
            let message = LocalAgentMessage::text(
                message_id.clone(),
                state.context_id.clone(),
                sender.clone(),
                to.clone(),
                summary.clone(),
                body,
            );
            state.accepted_messages += 1;
            state.accepted_body_bytes += message.text_body().len();
            state
                .entries
                .get_mut(sender)
                .expect("validated sender disappeared")
                .sent_messages += 1;
            let target = state
                .entries
                .get_mut(&to)
                .expect("validated target disappeared");
            target.pending_messages += 1;
            target.pending_body_bytes += message.text_body().len();
            target.queue.push_back(MailboxItem::Peer(message));
            target.inbox.add_local_pending(1);
            let target_inbox = Arc::clone(&target.inbox);

            // The activity signal is sent after unlock, but an already-running
            // recipient can reach a boundary without it. Publish Queued while
            // holding the routing lock so no claim can emit Delivered first.
            ui.emit(&Event::AgentMessageUpdated(AgentMessageUpdate {
                id: message_id.clone(),
                from: sender.clone(),
                to: to.clone(),
                summary,
                status: AgentMessageStatus::Queued,
            }));
            (message_id, target_inbox)
        };
        target_inbox.notify_activity();
        Ok(message_id)
    }

    fn list_agents(&self, sender: &LocalAgentId) -> Result<Vec<LocalAgentRosterEntry>, String> {
        let state = self.state.lock().unwrap();
        if state.closing {
            return Err("the session is closing; the local Agent roster is unavailable".into());
        }
        if !state
            .entries
            .get(sender)
            .is_some_and(|entry| entry.state == EntryState::Open)
        {
            return Err(format!("sender {sender} is not an open local Agent"));
        }
        let mut entries = state
            .entries
            .iter()
            .filter(|(id, entry)| *id != sender && entry.state == EntryState::Open)
            .map(|(id, entry)| LocalAgentRosterEntry {
                id: id.clone(),
                parent_id: entry.parent_id.clone(),
                agent_type: entry.agent_type.clone(),
                description: entry.description.clone(),
            })
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        Ok(entries)
    }

    fn claim_boundary(self: &Arc<Self>, recipient: &LocalAgentId) -> Option<MailboxBatch> {
        let mut state = self.state.lock().unwrap();
        let entry = state.entries.get_mut(recipient)?;
        if entry.state == EntryState::Closed || entry.queue.is_empty() {
            return None;
        }
        let mut body_bytes = 0usize;
        let mut claimed = Vec::new();
        while claimed.len() < MAX_BOUNDARY_MESSAGES {
            let Some(next) = entry.queue.front() else {
                break;
            };
            let next_bytes = next.body_bytes();
            if body_bytes + next_bytes > MAX_BOUNDARY_BODY_BYTES {
                break;
            }
            body_bytes += next_bytes;
            claimed.push(entry.queue.pop_front().expect("front item exists"));
        }
        if claimed.is_empty() {
            return None;
        }
        let claim_id = entry.next_claim;
        entry.next_claim = entry.next_claim.saturating_add(1);
        let items = claimed.iter().map(MailboxItem::as_inbox_item).collect();
        entry.claims.insert(claim_id, claimed);
        Some(MailboxBatch {
            directory: Arc::clone(self),
            recipient: recipient.clone(),
            claim_id,
            items,
            committed: false,
        })
    }

    fn commit_boundary(&self, recipient: &LocalAgentId, claim_id: u64, ui: &Arc<dyn Ui>) {
        let (updates, inbox, count) = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.get_mut(recipient) else {
                return;
            };
            let Some(items) = entry.claims.remove(&claim_id) else {
                return;
            };
            let mut updates = Vec::new();
            for item in &items {
                if let MailboxItem::Peer(message) = item {
                    entry.pending_messages -= 1;
                    entry.pending_body_bytes -= message.text_body().len();
                    updates.push(AgentMessageUpdate {
                        id: message.message_id.clone(),
                        from: message.from.clone(),
                        to: message.to.clone(),
                        summary: message.summary.clone(),
                        status: AgentMessageStatus::Delivered,
                    });
                }
            }
            if entry.state == EntryState::Closing && !entry.has_pending() {
                entry.state = EntryState::Closed;
            }
            (updates, Arc::clone(&entry.inbox), items.len())
        };
        inbox.remove_local_pending(count);
        for update in updates {
            ui.emit(&Event::AgentMessageUpdated(update));
        }
    }

    fn abandon_claim(&self, recipient: &LocalAgentId, claim_id: u64) {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(recipient) else {
            return;
        };
        let Some(items) = entry.claims.remove(&claim_id) else {
            return;
        };
        for item in items.into_iter().rev() {
            entry.queue.push_front(item);
        }
    }

    fn can_finish_naturally(&self, id: &LocalAgentId) -> bool {
        let mut state = self.state.lock().unwrap();
        let Some(entry) = state.entries.get_mut(id) else {
            return true;
        };
        if id.is_main() {
            return !entry.has_pending();
        }
        match entry.state {
            EntryState::Open if entry.has_pending() => false,
            EntryState::Open => {
                entry.state = EntryState::Closing;
                true
            }
            EntryState::Closing | EntryState::Closed => true,
        }
    }

    fn finish_lease(&self, id: &LocalAgentId, ui: &Arc<dyn Ui>) {
        let effects = {
            let mut state = self.state.lock().unwrap();
            let Some(entry) = state.entries.get(id) else {
                return;
            };
            let force = match entry.state {
                EntryState::Closing if !entry.has_pending() => false,
                EntryState::Closed => false,
                EntryState::Open | EntryState::Closing => true,
            };
            let effects = force.then(|| {
                force_close_locked(
                    &mut state,
                    id,
                    "the target Agent ended before processing the message",
                    true,
                )
            });
            state.entries.remove(id);
            effects
        };
        if let Some((updates, wakes)) = effects {
            publish_close_effects(updates, wakes, ui);
        }
    }

    fn shutdown(&self, ui: &Arc<dyn Ui>) {
        let (updates, wakes) = {
            let mut state = self.state.lock().unwrap();
            if state.closing {
                return;
            }
            state.closing = true;
            let ids = state.entries.keys().cloned().collect::<Vec<_>>();
            let mut updates = Vec::new();
            let mut failures: HashMap<(LocalAgentId, LocalAgentId), Vec<LocalMessageId>> =
                HashMap::new();
            for id in ids {
                let (dropped, dropped_failures) = close_queued_for_shutdown_locked(&mut state, &id);
                updates.extend(dropped);
                for (sender, target, message_id) in dropped_failures {
                    failures
                        .entry((sender, target))
                        .or_default()
                        .push(message_id);
                }
            }

            let reason: String = "the session shut down before the message was processed"
                .chars()
                .take(MAX_FAILURE_REASON_CHARS)
                .collect();
            let mut wakes = Vec::new();
            for ((sender, target), message_ids) in failures {
                let Some(sender_entry) = state.entries.get_mut(&sender) else {
                    continue;
                };
                if sender_entry.state == EntryState::Closed {
                    sender_entry.state = EntryState::Closing;
                }
                let queued = queue_failure_notice(
                    sender_entry,
                    AgentMessageFailure {
                        to: target,
                        message_ids,
                    },
                    &reason,
                );
                if queued {
                    sender_entry.inbox.add_local_pending(1);
                }
                wakes.push(Arc::clone(&sender_entry.inbox));
            }
            (updates, wakes)
        };
        publish_close_effects(updates, wakes, ui);
    }
}

fn is_unicode_line_separator(ch: char) -> bool {
    matches!(ch, '\u{2028}' | '\u{2029}')
}

fn bounded_roster_text(value: &str, fallback: &str) -> String {
    let mut output = String::new();
    for ch in value.chars() {
        let ch = if ch.is_control() || is_unicode_line_separator(ch) {
            ' '
        } else {
            ch
        };
        if output.chars().count() >= MAX_SUMMARY_CHARS
            || output.len() + ch.len_utf8() > MAX_SUMMARY_BYTES
        {
            break;
        }
        output.push(ch);
    }
    let output = output.trim().to_string();
    if output.is_empty() {
        fallback.to_string()
    } else {
        output
    }
}

fn validate_payload(summary: &str, body: &str) -> Result<(), String> {
    if body.trim().is_empty() {
        return Err("local Agent message must not be empty".into());
    }
    if body.len() > MAX_MESSAGE_BYTES {
        return Err(format!(
            "local Agent message exceeds the {MAX_MESSAGE_BYTES}-byte limit"
        ));
    }
    if body
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("local Agent message contains an unsupported control character".into());
    }
    if summary.trim().is_empty() {
        return Err("local Agent message summary must not be empty".into());
    }
    if summary
        .chars()
        .any(|ch| ch.is_control() || is_unicode_line_separator(ch))
    {
        return Err(
            "local Agent message summary must be a single line without control characters".into(),
        );
    }
    if summary.chars().count() > MAX_SUMMARY_CHARS || summary.len() > MAX_SUMMARY_BYTES {
        return Err(format!(
            "local Agent message summary exceeds {MAX_SUMMARY_CHARS} characters or {MAX_SUMMARY_BYTES} bytes"
        ));
    }
    Ok(())
}

fn close_queued_for_shutdown_locked(
    state: &mut DirectoryState,
    id: &LocalAgentId,
) -> (
    Vec<AgentMessageUpdate>,
    Vec<(LocalAgentId, LocalAgentId, LocalMessageId)>,
) {
    let Some(entry) = state.entries.get_mut(id) else {
        return (Vec::new(), Vec::new());
    };
    if entry.state == EntryState::Closed {
        return (Vec::new(), Vec::new());
    }
    entry.state = EntryState::Closing;
    let items = entry.queue.drain(..).collect::<Vec<_>>();
    if !items.is_empty() {
        entry.inbox.remove_local_pending(items.len());
    }
    let mut updates = Vec::new();
    let mut failures = Vec::new();
    for item in items {
        if let MailboxItem::Peer(message) = item {
            entry.pending_messages -= 1;
            entry.pending_body_bytes -= message.text_body().len();
            failures.push((
                message.from.clone(),
                message.to.clone(),
                message.message_id.clone(),
            ));
            updates.push(AgentMessageUpdate {
                id: message.message_id,
                from: message.from,
                to: message.to,
                summary: message.summary,
                status: AgentMessageStatus::Undeliverable,
            });
        }
    }
    if entry.claims.is_empty() {
        entry.state = EntryState::Closed;
    }
    (updates, failures)
}

fn force_close_locked(
    state: &mut DirectoryState,
    id: &LocalAgentId,
    reason: &str,
    notify_senders: bool,
) -> (Vec<AgentMessageUpdate>, Vec<Arc<Inbox>>) {
    let Some(entry) = state.entries.get_mut(id) else {
        return (Vec::new(), Vec::new());
    };
    if entry.state == EntryState::Closed {
        return (Vec::new(), Vec::new());
    }
    entry.state = EntryState::Closing;
    let mut items = entry.queue.drain(..).collect::<Vec<_>>();
    for (_, claim) in entry.claims.drain() {
        items.extend(claim);
    }
    let dropped_count = items.len();
    let target_inbox = Arc::clone(&entry.inbox);
    entry.pending_messages = 0;
    entry.pending_body_bytes = 0;
    entry.state = EntryState::Closed;
    if dropped_count > 0 {
        target_inbox.remove_local_pending(dropped_count);
    }

    let reason: String = reason.chars().take(MAX_FAILURE_REASON_CHARS).collect();
    let mut updates = Vec::new();
    let mut by_sender: HashMap<LocalAgentId, Vec<LocalMessageId>> = HashMap::new();
    for item in items {
        if let MailboxItem::Peer(message) = item {
            updates.push(AgentMessageUpdate {
                id: message.message_id.clone(),
                from: message.from.clone(),
                to: message.to.clone(),
                summary: message.summary,
                status: AgentMessageStatus::Undeliverable,
            });
            by_sender
                .entry(message.from)
                .or_default()
                .push(message.message_id);
        }
    }

    let mut wakes = Vec::new();
    if notify_senders {
        for (sender, message_ids) in by_sender {
            let Some(sender_entry) = state.entries.get_mut(&sender) else {
                continue;
            };
            if sender_entry.state != EntryState::Open {
                continue;
            }
            let queued = queue_failure_notice(
                sender_entry,
                AgentMessageFailure {
                    to: id.clone(),
                    message_ids,
                },
                &reason,
            );
            if queued {
                sender_entry.inbox.add_local_pending(1);
            }
            wakes.push(Arc::clone(&sender_entry.inbox));
        }
    }
    (updates, wakes)
}

fn publish_close_effects(
    updates: Vec<AgentMessageUpdate>,
    wakes: Vec<Arc<Inbox>>,
    ui: &Arc<dyn Ui>,
) {
    for update in updates {
        ui.emit(&Event::AgentMessageUpdated(update));
    }
    for inbox in wakes {
        inbox.notify_activity();
    }
}

pub struct MailboxBatch {
    directory: Arc<LiveAgentDirectory>,
    recipient: LocalAgentId,
    claim_id: u64,
    items: Vec<InboxItem>,
    committed: bool,
}

impl MailboxBatch {
    pub fn items(&self) -> &[InboxItem] {
        &self.items
    }

    pub fn commit(mut self, ui: &Arc<dyn Ui>) {
        self.directory
            .commit_boundary(&self.recipient, self.claim_id, ui);
        self.committed = true;
    }
}

impl Drop for MailboxBatch {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        self.directory.abandon_claim(&self.recipient, self.claim_id);
    }
}

pub struct LiveAgentLease {
    directory: Arc<LiveAgentDirectory>,
    id: LocalAgentId,
    ui: Arc<dyn Ui>,
    armed: bool,
}

impl Drop for LiveAgentLease {
    fn drop(&mut self) {
        if self.armed {
            self.directory.finish_lease(&self.id, &self.ui);
            self.armed = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    struct RecordingUi(Mutex<Vec<AgentMessageUpdate>>);

    impl RecordingUi {
        fn new() -> Arc<Self> {
            Arc::new(Self(Mutex::new(Vec::new())))
        }
    }

    impl Ui for RecordingUi {
        fn emit(&self, event: &Event) {
            if let Event::AgentMessageUpdated(update) = event {
                self.0.lock().unwrap().push(update.clone());
            }
        }
    }

    fn child(
        root: &LocalAgentContext,
        number: u64,
        ui: &Arc<RecordingUi>,
    ) -> (LocalAgentContext, Arc<Inbox>, LiveAgentLease) {
        let id: LocalAgentId = format!("agent-{number}").parse().unwrap();
        let context = root.child(id);
        let inbox = Arc::new(Inbox::default());
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let lease = context
            .register_child(Arc::clone(&inbox), None, "test child", ui_trait)
            .unwrap();
        (context, inbox, lease)
    }

    #[test]
    fn routes_fifo_and_reports_queued_then_delivered() {
        let root_inbox = Arc::new(Inbox::default());
        let root = LocalAgentContext::root(root_inbox);
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (agent, inbox, _lease) = child(&root, 1, &ui);

        let first = root
            .send(
                agent.agent_id().clone(),
                "one".into(),
                "first".into(),
                &ui_trait,
            )
            .unwrap();
        let second = root
            .send(
                agent.agent_id().clone(),
                "two".into(),
                "second".into(),
                &ui_trait,
            )
            .unwrap();
        assert_eq!(first.as_str(), "message-1");
        assert_eq!(second.as_str(), "message-2");
        assert_eq!(inbox.local_pending(), 2);

        let batch = agent.claim_boundary().unwrap();
        let ids = batch
            .items()
            .iter()
            .map(|item| match item {
                InboxItem::AgentMessage(message) => message.message_id.as_str(),
                _ => panic!("unexpected mailbox item"),
            })
            .collect::<Vec<_>>();
        assert_eq!(ids, ["message-1", "message-2"]);
        batch.commit(&ui_trait);
        assert_eq!(inbox.local_pending(), 0);
        assert_eq!(
            ui.0.lock()
                .unwrap()
                .iter()
                .map(|u| u.status)
                .collect::<Vec<_>>(),
            [
                AgentMessageStatus::Queued,
                AgentMessageStatus::Queued,
                AgentMessageStatus::Delivered,
                AgentMessageStatus::Delivered,
            ]
        );
    }

    #[test]
    fn identical_agent_ids_in_another_directory_are_not_routes() {
        let first = LocalAgentContext::root(Arc::new(Inbox::default()));
        let second = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (child, _, _lease) = child(&first, 40, &ui);
        let error = second
            .send(
                child.agent_id().clone(),
                "cross session".into(),
                "must fail".into(),
                &ui_trait,
            )
            .unwrap_err();
        assert!(error.contains("not a live local Agent in this session"));
    }

    #[test]
    fn roster_is_scoped_open_and_excludes_self() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let (left, _, left_lease) = child(&root, 2, &ui);
        let (right, _, _right_lease) = child(&root, 3, &ui);
        assert_eq!(root.list_agents().unwrap().len(), 2);
        assert_eq!(left.list_agents().unwrap().len(), 2);
        drop(left_lease);
        assert!(!root
            .directory
            .state
            .lock()
            .unwrap()
            .entries
            .contains_key(left.agent_id()));
        assert_eq!(right.list_agents().unwrap().len(), 1);
        assert_eq!(right.list_agents().unwrap()[0].id, LocalAgentId::Main);
        let ui_trait: Arc<dyn Ui> = ui.clone();
        assert!(root
            .send(
                left.agent_id().clone(),
                "closed".into(),
                "x".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("not a live local Agent"));
        assert!(root
            .send(LocalAgentId::Main, "self".into(), "x".into(), &ui_trait,)
            .unwrap_err()
            .contains("self"));
    }

    #[test]
    fn close_and_send_linearize_without_losing_a_message() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (agent, _, lease) = child(&root, 4, &ui);
        let directory = Arc::clone(&root.directory);
        let target = agent.agent_id().clone();
        let barrier = Arc::new(Barrier::new(2));
        let send_root = root.clone();
        let send_ui = Arc::clone(&ui_trait);
        let send_barrier = Arc::clone(&barrier);
        let target_for_send = target.clone();
        let sender = std::thread::spawn(move || {
            send_barrier.wait();
            send_root.send(target_for_send, "race".into(), "body".into(), &send_ui)
        });
        barrier.wait();
        let can_finish = directory.can_finish_naturally(&target);
        let sent = sender.join().unwrap();
        match (can_finish, sent) {
            (true, Err(_)) => {}
            (false, Ok(_)) => {
                assert!(agent.claim_boundary().is_some());
            }
            other => panic!("non-linearized close/send result: {other:?}"),
        }
        drop(lease);
    }

    #[test]
    fn forced_close_reports_pending_message_and_notifies_sender() {
        let root_inbox = Arc::new(Inbox::default());
        let root = LocalAgentContext::root(Arc::clone(&root_inbox));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (agent, _, lease) = child(&root, 5, &ui);
        root.send(
            agent.agent_id().clone(),
            "pending".into(),
            "body".into(),
            &ui_trait,
        )
        .unwrap();
        drop(lease);
        let batch = root.claim_boundary().unwrap();
        assert!(matches!(
            &batch.items()[0],
            InboxItem::AgentMessageUndeliverable { failures, .. }
                if failures[0].message_ids[0].as_str() == "message-1"
        ));
        batch.commit(&ui_trait);
        assert!(ui.0.lock().unwrap().iter().any(|update| {
            update.id.as_str() == "message-1" && update.status == AgentMessageStatus::Undeliverable
        }));
    }

    #[test]
    fn delivery_failures_coalesce_beyond_the_peer_mailbox_limit() {
        let root_inbox = Arc::new(Inbox::default());
        let root = LocalAgentContext::root(Arc::clone(&root_inbox));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let mut targets = Vec::new();
        for number in 100..133 {
            targets.push(child(&root, number, &ui));
        }
        for (index, (target, _, _)) in targets.iter().enumerate() {
            root.send(
                target.agent_id().clone(),
                format!("pending {index}"),
                "body".into(),
                &ui_trait,
            )
            .unwrap();
        }
        for (_, _, lease) in targets {
            drop(lease);
        }

        assert_eq!(root_inbox.local_pending(), 1);
        let batch = root.claim_boundary().unwrap();
        assert!(matches!(
            &batch.items()[0],
            InboxItem::AgentMessageUndeliverable { failures, .. }
                if failures.len() == 33
                    && failures.iter().map(|failure| failure.message_ids.len()).sum::<usize>() == 33
        ));
        batch.commit(&ui_trait);
        assert!(root_inbox.is_empty());
    }

    #[test]
    fn shutdown_does_not_reclassify_a_claimed_delivery() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (agent, _, _lease) = child(&root, 7, &ui);
        root.send(
            agent.agent_id().clone(),
            "claimed".into(),
            "body".into(),
            &ui_trait,
        )
        .unwrap();
        let batch = agent.claim_boundary().unwrap();
        root.shutdown(&ui_trait);
        batch.commit(&ui_trait);
        assert_eq!(
            ui.0.lock()
                .unwrap()
                .iter()
                .map(|update| update.status)
                .collect::<Vec<_>>(),
            [AgentMessageStatus::Queued, AgentMessageStatus::Delivered]
        );
    }

    #[test]
    fn shutdown_marks_queued_message_and_notifies_sender() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (agent, _, _lease) = child(&root, 9, &ui);
        root.send(
            agent.agent_id().clone(),
            "shutdown queued".into(),
            "body".into(),
            &ui_trait,
        )
        .unwrap();
        root.shutdown(&ui_trait);

        let batch = root.claim_boundary().unwrap();
        assert!(matches!(
            &batch.items()[0],
            InboxItem::AgentMessageUndeliverable { failures, reason }
                if failures[0].to == *agent.agent_id()
                    && failures[0].message_ids[0].as_str() == "message-1"
                    && reason.contains("session shut down")
        ));
        batch.commit(&ui_trait);
        assert_eq!(
            ui.0.lock()
                .unwrap()
                .iter()
                .map(|update| update.status)
                .collect::<Vec<_>>(),
            [
                AgentMessageStatus::Queued,
                AgentMessageStatus::Undeliverable
            ]
        );
    }

    #[test]
    fn directory_revalidates_payloads_and_bounds_roster_metadata() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let id: LocalAgentId = "agent-8".parse().unwrap();
        let context = root.child(id);
        let inbox = Arc::new(Inbox::default());
        let _lease = context
            .register_child(
                inbox,
                Some(&format!(
                    "type\u{2028}{}",
                    "x".repeat(MAX_SUMMARY_BYTES * 2)
                )),
                &format!("description\n{}", "界".repeat(MAX_SUMMARY_CHARS * 2)),
                ui_trait.clone(),
            )
            .unwrap();
        let roster = root.list_agents().unwrap();
        assert_eq!(roster.len(), 1);
        let agent_type = roster[0].agent_type.as_deref().unwrap();
        assert!(!agent_type.contains('\u{2028}'));
        assert!(agent_type.len() <= MAX_SUMMARY_BYTES);
        assert!(!roster[0].description.contains('\n'));
        assert!(roster[0].description.chars().count() <= MAX_SUMMARY_CHARS);
        assert!(root
            .send(
                context.agent_id().clone(),
                "bad\u{2028}summary".into(),
                "body".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("single line"));
        assert!(root
            .send(
                context.agent_id().clone(),
                "summary".into(),
                "body\u{0000}".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("control"));
        assert!(ui.0.lock().unwrap().is_empty());
    }

    #[test]
    fn byte_budgets_limit_boundary_mailbox_and_session() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (target, _, _target_lease) = child(&root, 10, &ui);
        let full = "x".repeat(MAX_MESSAGE_BYTES);
        for index in 0..5 {
            root.send(
                target.agent_id().clone(),
                format!("full {index}"),
                full.clone(),
                &ui_trait,
            )
            .unwrap();
        }
        let first = target.claim_boundary().unwrap();
        assert_eq!(first.items().len(), 4, "32 KiB boundary cap");
        first.commit(&ui_trait);
        let second = target.claim_boundary().unwrap();
        assert_eq!(second.items().len(), 1);
        second.commit(&ui_trait);
        assert!(root
            .send(
                target.agent_id().clone(),
                "too large".into(),
                "x".repeat(MAX_MESSAGE_BYTES + 1),
                &ui_trait,
            )
            .unwrap_err()
            .contains("8192-byte"));

        let mailbox_root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let (mailbox, _, _mailbox_lease) = child(&mailbox_root, 11, &ui);
        for index in 0..16 {
            mailbox_root
                .send(
                    mailbox.agent_id().clone(),
                    format!("mailbox {index}"),
                    full.clone(),
                    &ui_trait,
                )
                .unwrap();
        }
        assert!(mailbox_root
            .send(
                mailbox.agent_id().clone(),
                "mailbox bytes".into(),
                "x".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("131072-byte"));

        let session_root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let (sender, _, _sender_lease) = child(&session_root, 12, &ui);
        let (receiver, _, _receiver_lease) = child(&session_root, 13, &ui);
        for index in 0..MAX_AGENT_SENT_MESSAGES {
            sender
                .send(
                    receiver.agent_id().clone(),
                    format!("session body {index}"),
                    full.clone(),
                    &ui_trait,
                )
                .unwrap();
            let batch = receiver.claim_boundary().unwrap();
            batch.commit(&ui_trait);
        }
        assert!(session_root
            .send(
                receiver.agent_id().clone(),
                "session body overflow".into(),
                "x".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("524288-byte"));
    }

    #[test]
    fn sender_and_session_message_counts_do_not_reset_after_delivery() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let mut senders = Vec::new();
        let mut leases = Vec::new();
        for number in 20..=24 {
            let (sender, _, lease) = child(&root, number, &ui);
            senders.push(sender);
            leases.push(lease);
        }
        for sender in &senders[..4] {
            for _ in 0..MAX_AGENT_SENT_MESSAGES {
                sender
                    .send(LocalAgentId::Main, "count".into(), "x".into(), &ui_trait)
                    .unwrap();
                let batch = root.claim_boundary().unwrap();
                batch.commit(&ui_trait);
            }
            assert!(sender
                .send(
                    LocalAgentId::Main,
                    "sender overflow".into(),
                    "x".into(),
                    &ui_trait,
                )
                .unwrap_err()
                .contains("64-message"));
        }
        assert!(senders[4]
            .send(
                LocalAgentId::Main,
                "session overflow".into(),
                "x".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("256-message"));
        drop(leases);
    }

    #[test]
    fn quotas_are_cumulative_and_boundary_is_bounded() {
        let root = LocalAgentContext::root(Arc::new(Inbox::default()));
        let ui = RecordingUi::new();
        let ui_trait: Arc<dyn Ui> = ui.clone();
        let (agent, _, _lease) = child(&root, 6, &ui);
        for index in 0..MAX_MAILBOX_MESSAGES {
            root.send(
                agent.agent_id().clone(),
                format!("message {index}"),
                "x".into(),
                &ui_trait,
            )
            .unwrap();
        }
        assert!(root
            .send(
                agent.agent_id().clone(),
                "overflow".into(),
                "x".into(),
                &ui_trait,
            )
            .unwrap_err()
            .contains("32 pending"));
        let batch = agent.claim_boundary().unwrap();
        assert_eq!(batch.items().len(), MAX_BOUNDARY_MESSAGES);
        batch.commit(&ui_trait);
        assert_eq!(
            agent.claim_boundary().unwrap().items().len(),
            MAX_BOUNDARY_MESSAGES
        );
    }
}
