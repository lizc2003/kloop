use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::provider_route::FrozenProviderAttempt;
use crate::rollout::ResumedSession;
use crate::rollout::Rollout;
use crate::rollout::SessionRuntime;
use crate::rollout::TurnTerminal;
use crate::usage::{ProviderUsageRecord, UsageLedger};
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderAttemptKind;
use kloop_protocol::ProviderRouteReceipt;
use kloop_protocol::ProviderRouteSource;
use kloop_protocol::ReasoningContinuity;
use kloop_protocol::Role;
use kloop_protocol::ToolResultContent;

/// Offload ids are process-global so a sub-agent's spills never clobber the
/// parent's files in the shared offload directory.
static NEXT_OFFLOAD_ID: AtomicUsize = AtomicUsize::new(1);

const HEAD_CHARS: usize = 1500;
const TAIL_CHARS: usize = 500;

/// Char count above which a text tool result spills to disk instead of entering
/// the history. Public because whatever reads a spilled result back must stay
/// strictly under it: a reply that spills again would hand the model a
/// byte-identical preview under a fresh path, the escape failing at exactly the
/// size that needs it. `read_file`'s budget (`READ_CONTENT_CHARS`) is the one
/// that has to respect this today.
pub const OFFLOAD_CAP_CHARS: usize = 32_000;

/// Append-only conversation history. Oversized tool results are offloaded to
/// disk at record time; the history keeps a preview plus the file's path, which
/// the model queries in place (cc's shape) rather than reading back.
pub struct History {
    items: Vec<Message>,
    offload_dir: PathBuf,
    cap: usize,
    /// (items recorded at that point, total context tokens the provider
    /// reported for the request covering them). Anchors the estimate.
    usage_anchor: Option<(usize, u64)>,
    provider_usage: UsageLedger,
    provider_routes: Vec<ProviderRouteReceipt>,
    /// The smallest request size the provider has actually rejected as too
    /// large, in estimated tokens. A configured window is a claim; a rejection
    /// is ground truth, and it is the only signal that the claim was wrong.
    /// Recording it makes the predictive threshold self-correct instead of
    /// walking into the same rejection every round.
    observed_overflow_ceiling: Option<u64>,
    /// Session file written through on every record/replace_all; None for
    /// in-memory-only histories (sub-agents, tests).
    rollout: Option<Rollout>,
    next_memory_boundary: u64,
}

impl History {
    pub fn new(offload_dir: PathBuf) -> Self {
        Self {
            items: Vec::new(),
            offload_dir,
            cap: OFFLOAD_CAP_CHARS,
            usage_anchor: None,
            observed_overflow_ceiling: None,
            provider_usage: UsageLedger::default(),
            provider_routes: Vec::new(),
            rollout: None,
            next_memory_boundary: 1,
        }
    }

    pub fn attach_rollout(&mut self, rollout: Rollout) {
        self.provider_routes = rollout.route_timeline().to_vec();
        self.rollout = Some(rollout);
    }

    /// Resume a persisted session: the items were already offloaded when
    /// first recorded, so they are installed verbatim — no re-spill, and no
    /// re-append to the session file. The usage anchor starts empty and
    /// re-anchors on the first sampled response.
    pub fn resume(offload_dir: PathBuf, resumed: ResumedSession) -> Self {
        sync_offload_counter(&offload_dir);
        let provider_routes = resumed.snapshot.provider_routes.clone();
        Self {
            items: resumed.messages,
            offload_dir,
            cap: OFFLOAD_CAP_CHARS,
            usage_anchor: None,
            observed_overflow_ceiling: None,
            provider_usage: resumed.provider_usage,
            provider_routes,
            next_memory_boundary: resumed.rollout.next_boundary(),
            rollout: Some(resumed.rollout),
        }
    }

    /// Rewind the live conversation onto a forked branch: install the fork's
    /// items and redirect persistence to its rollout, keeping the same offload
    /// store (branches share it, like resume). The usage anchor resets so the
    /// next sampled response re-anchors the estimate. The old rollout is dropped
    /// unwritten — the branch it wrote already lives in its own file on disk.
    pub fn rebase(&mut self, resumed: ResumedSession) {
        self.items = resumed.messages;
        self.provider_usage = resumed.provider_usage;
        self.provider_routes = resumed.snapshot.provider_routes.clone();
        self.next_memory_boundary = resumed.rollout.next_boundary();
        self.rollout = Some(resumed.rollout);
        self.usage_anchor = None;
    }

    pub fn record(&mut self, mut msg: Message) {
        assert!(
            msg.provider_provenance.is_none(),
            "provider assistant messages must use History::record_provider_assistant"
        );
        for block in &mut msg.content {
            // Only text tool results spill to disk. Image blocks must reach the
            // model as-is (base64 inlines into the rollout) — cc likewise skips
            // offload for image content.
            if let ContentBlock::ToolResult {
                content: ToolResultContent::Text(text),
                ..
            } = block
            {
                let content = std::mem::take(text);
                *text = self.offload_text(content);
            }
        }
        let boundary = self
            .rollout
            .as_ref()
            .map(Rollout::next_boundary)
            .unwrap_or(self.next_memory_boundary);
        self.persist(|rollout| rollout.append_message(&msg));
        self.items.push(msg);
        self.next_memory_boundary = boundary.saturating_add(1);
    }

    pub fn record_provider_assistant(
        &mut self,
        blocks: Vec<ContentBlock>,
        attempt: &FrozenProviderAttempt,
    ) {
        let origin_boundary = self
            .rollout
            .as_ref()
            .map(Rollout::next_boundary)
            .unwrap_or(self.next_memory_boundary);
        let message = Message::assistant_from_provider(blocks, attempt.provenance(origin_boundary));
        self.persist(|rollout| rollout.append_message(&message));
        self.items.push(message);
        self.next_memory_boundary = origin_boundary.saturating_add(1);
    }

    pub fn ensure_initial_provider_route(
        &mut self,
        route: &crate::provider_route::FrozenProviderRoute,
    ) -> std::io::Result<()> {
        if let Some(existing) = self.provider_routes.last() {
            if existing.revision != route.revision()
                || existing.provider_id != route.provider_id()
                || existing.api_family != route.api_family()
                || existing.endpoint_fingerprint != route.endpoint_fingerprint()
                || existing.primary_model != route.primary_model()
                || existing.fallback_model.as_deref() != route.fallback_model()
            {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "history provider route does not match the frozen operation route",
                ));
            }
            return Ok(());
        }
        let receipt = if let Some(rollout) = self.rollout.as_mut() {
            rollout.append_initial_route(route)?
        } else {
            route.receipt(
                0,
                ProviderRouteSource::Initial,
                ReasoningContinuity::Preserved,
            )
        };
        self.provider_routes.push(receipt);
        Ok(())
    }

    pub fn append_provider_route_changed(
        &mut self,
        route: &crate::provider_route::FrozenProviderRoute,
        continuity: ReasoningContinuity,
    ) -> std::io::Result<ProviderRouteReceipt> {
        let previous = self.provider_routes.last().ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "history provider route timeline is missing",
            )
        })?;
        if route.revision() != previous.revision.saturating_add(1) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "provider route revision must advance by exactly one",
            ));
        }
        let receipt = if let Some(rollout) = self.rollout.as_mut() {
            rollout.append_provider_route_changed(route, continuity)?
        } else {
            let receipt = route.receipt(
                self.next_memory_boundary,
                ProviderRouteSource::ExplicitSwitch,
                continuity,
            );
            self.next_memory_boundary = self.next_memory_boundary.saturating_add(1);
            receipt
        };
        self.provider_routes.push(receipt.clone());
        Ok(receipt)
    }

    pub fn provider_routes(&self) -> &[ProviderRouteReceipt] {
        &self.provider_routes
    }

    pub fn provider_request_view(
        &self,
        attempt: &FrozenProviderAttempt,
    ) -> Result<Vec<Message>, kloop_provider::ProviderFailure> {
        provider_request_view(&self.items, &self.provider_routes, attempt)
    }

    pub(crate) fn provider_request_view_for(
        &self,
        messages: &[Message],
        attempt: &FrozenProviderAttempt,
    ) -> Result<Vec<Message>, kloop_provider::ProviderFailure> {
        provider_request_view(messages, &self.provider_routes, attempt)
    }

    pub fn switch_provider(
        &mut self,
        state: &crate::provider_route::SessionProviderState,
        expected_revision: u64,
        provider_id: &str,
        model: Option<&str>,
    ) -> Result<crate::provider_route::SwitchOutcome, ProviderSwitchError> {
        state
            .switch_with(expected_revision, provider_id, model, |_previous, next| {
                let boundary = self
                    .rollout
                    .as_ref()
                    .map(Rollout::next_boundary)
                    .unwrap_or(self.next_memory_boundary);
                let mut hypothetical_routes = self.provider_routes.clone();
                hypothetical_routes.push(next.receipt(
                    boundary,
                    ProviderRouteSource::ExplicitSwitch,
                    ReasoningContinuity::Preserved,
                ));
                let projected = provider_request_view(
                    &self.items,
                    &hypothetical_routes,
                    &next.primary_attempt(),
                )
                .map_err(ProviderSwitchCommitError::History)?;
                let continuity = if projected == self.items {
                    ReasoningContinuity::Preserved
                } else {
                    ReasoningContinuity::Filtered
                };
                self.append_provider_route_changed(next, continuity)
                    .map_err(ProviderSwitchCommitError::Persistence)?;
                Ok(continuity)
            })
            .map_err(|error| match error {
                crate::provider_route::SwitchCommitError::Switch(error) => {
                    ProviderSwitchError::Route(error)
                }
                crate::provider_route::SwitchCommitError::Commit(error) => match error {
                    ProviderSwitchCommitError::History(error) => {
                        ProviderSwitchError::History(error)
                    }
                    ProviderSwitchCommitError::Persistence(error) => {
                        ProviderSwitchError::Persistence(error)
                    }
                },
            })
    }

    /// Bound machine-produced text before injecting it as a non-tool user
    /// message (for example a detached Agent/Program result). Ordinary user text
    /// does not use this helper. Oversized content lands in the same offload store
    /// as tool results and returns a preview plus the file's path.
    pub(crate) fn offload_text(&mut self, content: String) -> String {
        if content.chars().count() > self.cap {
            self.spill(&content)
        } else {
            content
        }
    }

    /// Pin a server thread's effective runtime after its Config has resolved
    /// defaults. Unlike normal message persistence this is recovery-critical:
    /// failure is returned to the caller instead of silently dropping rollout.
    pub fn append_runtime(&mut self, runtime: SessionRuntime) -> std::io::Result<()> {
        self.rollout
            .as_mut()
            .ok_or_else(|| std::io::Error::other("history has no rollout"))?
            .append_runtime(&runtime)
    }

    /// Persist a model turn's display-only terminal state. It never enters
    /// `items`, so provider replay and token accounting remain unchanged.
    pub fn record_turn_terminal(&mut self, terminal: TurnTerminal) {
        self.persist(|rollout| rollout.append_turn_terminal(&terminal));
    }

    /// Persistence must never take down the live session: a failed write
    /// drops the rollout and the session continues in memory only.
    fn persist(&mut self, write: impl FnOnce(&mut Rollout) -> std::io::Result<()>) {
        let Some(rollout) = &mut self.rollout else {
            return;
        };
        if let Err(e) = write(rollout) {
            eprintln!("[session persistence failed ({e}); continuing without it]");
            self.rollout = None;
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.items
    }

    pub fn provider_usage(&self) -> &UsageLedger {
        &self.provider_usage
    }

    pub fn record_provider_usage(&mut self, record: ProviderUsageRecord) {
        self.persist(|rollout| rollout.append_provider_usage(&record));
        self.provider_usage.push(record);
    }

    /// Id of the last line persisted to the session file (`{stem}#{seq}`), or
    /// None for an in-memory-only history. run_agent reads this right after the
    /// assistant message carrying its tool_use is recorded, so a spawned
    /// sub-agent can point its `subagent_of` back at the exact parent turn.
    pub fn rollout_last_id(&self) -> Option<&str> {
        self.rollout.as_ref().and_then(Rollout::last_id)
    }

    /// Path to this session's file, or None for an in-memory-only history.
    /// The subagent_stop hook surfaces it as the sub-agent's transcript.
    pub fn rollout_path(&self) -> Option<&Path> {
        self.rollout.as_ref().map(Rollout::path)
    }

    /// Record the provider-reported total context size (uncached + cached
    /// input + output — cached tokens still occupy the window) for the
    /// request whose response is the most recently recorded item.
    pub fn note_usage(&mut self, total_tokens: u64) {
        self.usage_anchor = Some((self.items.len(), total_tokens));
    }

    /// Current context size: the last real usage anchor plus a ~4 chars/token
    /// estimate for everything recorded after it.
    /// The window to plan against: the configured value, lowered to anything the
    /// provider has actually rejected. Never raised — a rejection at N proves
    /// only that N is too big, never that anything is safe.
    pub fn effective_window(&self, configured: u64) -> u64 {
        match self.observed_overflow_ceiling {
            Some(observed) => configured.min(observed),
            None => configured,
        }
    }

    /// Record a size the provider refused. Keeps the smallest seen.
    pub fn note_overflow_at(&mut self, estimated_tokens: u64) {
        self.observed_overflow_ceiling = Some(match self.observed_overflow_ceiling {
            Some(previous) => previous.min(estimated_tokens),
            None => estimated_tokens,
        });
    }

    pub fn estimated_tokens(&self) -> u64 {
        let (anchored_len, anchored_tokens) = self.usage_anchor.unwrap_or((0, 0));
        let tail: u64 = self.items[anchored_len.min(self.items.len())..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        anchored_tokens + tail
    }

    /// Compaction is the one sanctioned rewrite of the otherwise append-only
    /// history. The usage anchor no longer describes the new items, so it is
    /// dropped and the estimate runs purely on the char heuristic until the
    /// next sampled response re-anchors it.
    pub fn replace_all(&mut self, items: Vec<Message>) {
        self.persist(|rollout| rollout.append_compacted(&items));
        self.items = items;
        self.usage_anchor = None;
    }

    fn spill(&mut self, content: &str) -> String {
        let id = format!("off-{:04}", NEXT_OFFLOAD_ID.fetch_add(1, Ordering::Relaxed));
        let head: String = content.chars().take(HEAD_CHARS).collect();
        let tail_rev: Vec<char> = content.chars().rev().take(TAIL_CHARS).collect();
        let tail: String = tail_rev.into_iter().rev().collect();
        let path = self.offload_dir.join(format!("{id}.txt"));
        let write =
            std::fs::create_dir_all(&self.offload_dir).and_then(|_| std::fs::write(&path, content));
        // cc's shape: hand over a path and let the general tools work on it, rather
        // than mint an opaque id for a reader that exists only to dereference it.
        // The advice is to extract *in place* — cc reads a 2 MB spec with
        // `python3 -c` printing three fields, never by pulling the file into the
        // conversation — and inside a program even that extraction costs no
        // context. Sandboxed bash can read this directory (the private state root
        // is denied for its credential, with the offload store carved back out)
        // but still cannot write it.
        let pointer = match write {
            Ok(()) => format!(
                "[full output saved to {path} ({chars} chars). Query it in place instead of \
                 reading it back: bash with `python3 -c '...'` over that path, printing only the \
                 fields you need, so only what you extract enters the context]",
                chars = content.chars().count(),
                path = path.display(),
            ),
            Err(e) => format!("[offload to disk failed ({e}); output truncated]"),
        };
        format!("{head}\n…[truncated]…\n{tail}\n{pointer}")
    }
}

#[derive(Debug)]
enum ProviderSwitchCommitError {
    History(kloop_provider::ProviderFailure),
    Persistence(std::io::Error),
}

#[derive(Debug)]
pub enum ProviderSwitchError {
    Route(crate::provider_route::SwitchError),
    History(kloop_provider::ProviderFailure),
    Persistence(std::io::Error),
}

impl std::fmt::Display for ProviderSwitchError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Route(error) => write!(formatter, "{error}"),
            Self::History(error) => write!(formatter, "{error}"),
            Self::Persistence(error) => {
                write!(formatter, "provider route persistence failed: {error}")
            }
        }
    }
}

impl std::error::Error for ProviderSwitchError {}

fn provider_request_view(
    messages: &[Message],
    routes: &[ProviderRouteReceipt],
    attempt: &FrozenProviderAttempt,
) -> Result<Vec<Message>, kloop_provider::ProviderFailure> {
    let active = routes.last().ok_or_else(|| {
        kloop_provider::ProviderFailure::protocol("provider route timeline is missing")
    })?;
    if active.revision != attempt.identity().route_revision
        || active.provider_id != attempt.identity().provider_id
        || active.api_family != attempt.identity().api_family
        || active.endpoint_fingerprint != attempt.identity().endpoint_fingerprint
    {
        return Err(kloop_provider::ProviderFailure::protocol(
            "frozen provider attempt does not match the active durable route",
        ));
    }

    let mut projected = Vec::with_capacity(messages.len());
    for message in messages {
        if !message.has_reasoning() {
            projected.push(message.clone());
            continue;
        }
        let source = message.provider_provenance.as_ref().ok_or_else(|| {
            kloop_provider::ProviderFailure::protocol(
                "reasoning history is missing provider route provenance",
            )
        })?;
        let source_index = routes
            .iter()
            .position(|route| route.revision == source.route_revision)
            .ok_or_else(|| {
                kloop_provider::ProviderFailure::protocol(
                    "reasoning history references an unknown provider route revision",
                )
            })?;
        let source_route = &routes[source_index];
        let interval_end = routes
            .get(source_index + 1)
            .map_or(u64::MAX, |next| next.boundary);
        if source.origin_boundary <= source_route.boundary || source.origin_boundary >= interval_end
        {
            return Err(kloop_provider::ProviderFailure::protocol(
                "reasoning history origin lies outside its provider route interval",
            ));
        }
        let source_model_valid = match source.attempt_kind {
            ProviderAttemptKind::Primary => source.model == source_route.primary_model,
            ProviderAttemptKind::Fallback => {
                source_route.fallback_model.as_deref() == Some(source.model.as_str())
            }
        };
        if source.provider_id != source_route.provider_id
            || source.api_family != source_route.api_family
            || source.endpoint_fingerprint != source_route.endpoint_fingerprint
            || !source_model_valid
        {
            return Err(kloop_provider::ProviderFailure::protocol(
                "reasoning history provenance does not match its producing route",
            ));
        }

        if source.api_family == ProviderApiFamily::OpenAiChatCompletions
            || (source.api_family == ProviderApiFamily::OpenAiResponses
                && message
                    .content
                    .iter()
                    .any(ContentBlock::has_redacted_reasoning))
        {
            return Err(kloop_provider::ProviderFailure::protocol(
                "reasoning history block shape does not match its producing API family",
            ));
        }

        let chat_target = attempt.identity().api_family == ProviderApiFamily::OpenAiChatCompletions;
        if !chat_target && source.exact_replay_compatible(attempt.identity()) {
            projected.push(message.clone());
            continue;
        }
        let sanctioned_switch = attempt.identity().attempt_kind == ProviderAttemptKind::Primary
            && source.route_revision < active.revision
            && routes[source_index + 1..]
                .iter()
                .any(|route| route.source == ProviderRouteSource::ExplicitSwitch);
        if !chat_target && !sanctioned_switch {
            return Err(kloop_provider::ProviderFailure::protocol(
                "reasoning mismatch is not authorized by a durable explicit provider switch",
            ));
        }
        let mut message = message.clone();
        message.content = message
            .content
            .into_iter()
            .filter_map(ContentBlock::into_without_reasoning)
            .collect();
        message.provider_provenance = None;
        if message.role != Role::Assistant || !message.content.is_empty() {
            projected.push(message);
        }
    }
    Ok(projected)
}

/// A resumed session shares the offload dir with the files its earlier run
/// spilled, but the process-global counter restarts at 1 — advance it past
/// every id already on disk so new spills never clobber old files.
pub fn sync_offload_counter(offload_dir: &Path) {
    let mut max_seen = 0;
    if let Ok(entries) = std::fs::read_dir(offload_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let id = name
                .to_str()
                .and_then(|n| n.strip_prefix("off-"))
                .and_then(|n| n.strip_suffix(".txt"))
                .and_then(|n| n.parse::<usize>().ok());
            if let Some(id) = id {
                max_seen = max_seen.max(id);
            }
        }
    }
    NEXT_OFFLOAD_ID.fetch_max(max_seen + 1, Ordering::Relaxed);
}

/// ~4 chars/token heuristic over the provider-visible message form, ceiling
/// division. Internal replay provenance is persisted with history but never
/// enters the provider prompt, so it must not inflate context estimates.
pub fn estimate_message_tokens(message: &Message) -> u64 {
    #[derive(serde::Serialize)]
    struct ProviderMessage<'a> {
        role: Role,
        content: &'a [ContentBlock],
    }

    // Count the serialized bytes without materializing the string — this runs
    // per message on every predictive-overflow check and every compaction
    // candidate, and only the length feeds the heuristic.
    struct ByteCounter(u64);
    impl std::io::Write for ByteCounter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let provider_message = ProviderMessage {
        role: message.role,
        content: &message.content,
    };
    let mut counter = ByteCounter(0);
    let bytes = serde_json::to_writer(&mut counter, &provider_message).map_or(0, |()| counter.0);
    bytes.div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::Role;

    fn temp_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kloop-test-{}-{tag}", std::process::id()))
    }

    fn tool_result(content: String) -> Message {
        Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: content.into(),
            is_error: false,
        }])
    }

    /// The spilled file's stem, read back out of the path the pointer names —
    /// the only place it appears now that the id is not a model-facing handle.
    fn pointer_id(pointer: &str) -> String {
        let start = pointer.find("off-").expect("pointer names the file");
        pointer[start..start + 8].to_string()
    }

    #[test]
    fn oversized_tool_result_is_offloaded_with_pointer() {
        let dir = temp_dir("spill");
        let mut h = History::new(dir.clone());
        let big = "x".repeat(OFFLOAD_CAP_CHARS + 1_000);
        h.record(tool_result(big.clone()));

        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
            panic!("expected tool result");
        };
        let content = content.as_text();
        assert!(content.chars().count() < OFFLOAD_CAP_CHARS);
        assert!(content.contains("…[truncated]…"));
        assert!(content.contains("saved to"));
        let id = pointer_id(&content);
        let on_disk = std::fs::read_to_string(dir.join(format!("{id}.txt"))).unwrap();
        assert_eq!(on_disk, big);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// cc's shape: the pointer names a path and the size, and points at querying
    /// the file in place with the shell. No id, no reader tool to dereference one,
    /// and no wrapper primitive — cc reads a 2 MB spec with `python3 -c`.
    #[test]
    fn the_offload_pointer_names_a_path_and_says_to_query_it_in_place() {
        let dir = temp_dir("spill-path");
        let mut h = History::new(dir.clone());
        let big = "x".repeat(OFFLOAD_CAP_CHARS + 1_000);
        h.record(tool_result(big.clone()));

        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
            panic!("expected tool result");
        };
        let content = content.as_text();
        let path = dir.join(format!("{}.txt", pointer_id(&content)));
        assert!(
            content.contains(&format!("({} chars)", big.chars().count())),
            "{content}"
        );
        assert!(content.contains(&path.display().to_string()), "{content}");
        assert!(content.contains("python3 -c"), "{content}");
        assert!(content.contains("Query it in place"), "{content}");
        // The retired vocabulary must not come back: no id, no reader tool.
        assert!(!content.contains("read_offloaded"), "{content}");
        assert!(!content.contains("id=off-"), "{content}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), big);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn machine_injected_text_reuses_the_offload_store() {
        let dir = temp_dir("inbox-spill");
        let mut history = History::new(dir.clone());
        assert_eq!(history.offload_text("small".into()), "small");

        let big = "result".repeat(OFFLOAD_CAP_CHARS / 6 + 200);
        let preview = history.offload_text(big.clone());
        assert!(preview.contains("…[truncated]…"), "{preview}");
        assert!(preview.contains("saved to"), "{preview}");
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("{}.txt", pointer_id(&preview)))).unwrap(),
            big
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn estimated_tokens_anchor_math() {
        let mut h = History::new(temp_dir("anchor"));
        h.record(Message::user_text("earlier message"));
        // Provider reports the real context size for everything so far.
        h.note_usage(1_000);
        assert_eq!(h.estimated_tokens(), 1_000);
        // Items after the anchor add their char-heuristic estimate on top.
        let tail = Message::user_text("x".repeat(400));
        let tail_estimate = estimate_message_tokens(&tail);
        h.record(tail.clone());
        assert_eq!(h.estimated_tokens(), 1_000 + tail_estimate);
        // Compaction (replace_all) invalidates the anchor: pure estimate again.
        h.replace_all(vec![tail.clone()]);
        assert_eq!(h.estimated_tokens(), tail_estimate);
    }

    #[test]
    fn replay_provenance_does_not_count_toward_context_estimates() {
        let content = vec![ContentBlock::Thinking {
            thinking: "summary".into(),
            signature: "opaque".into(),
        }];
        let plain = Message::assistant(content.clone());
        let bound = Message::assistant_from_provider(
            content,
            kloop_protocol::ProviderResponseProvenance {
                route_revision: 1,
                origin_boundary: 2,
                provider_id: "responses".into(),
                api_family: kloop_protocol::ProviderApiFamily::OpenAiResponses,
                endpoint_fingerprint: "endpoint-sha256".into(),
                model: "wire-model".into(),
                attempt_kind: kloop_protocol::ProviderAttemptKind::Primary,
            },
        );

        assert_eq!(
            estimate_message_tokens(&bound),
            estimate_message_tokens(&plain)
        );
    }

    #[test]
    fn offload_ids_unique_across_histories() {
        // Parent and sub-agent share the offload dir; the process-global
        // counter must keep their spill files from clobbering each other.
        let dir = temp_dir("shared");
        let mut a = History::new(dir.clone());
        let mut b = History::new(dir.clone());
        a.record(tool_result("a".repeat(OFFLOAD_CAP_CHARS + 1_000)));
        b.record(tool_result("b".repeat(OFFLOAD_CAP_CHARS + 1_000)));
        let id_of = |h: &History| {
            let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
                panic!("expected tool result");
            };
            let content = content.as_text();
            pointer_id(&content)
        };
        assert_ne!(id_of(&a), id_of(&b));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn only_oversized_blocks_in_a_message_are_offloaded() {
        let mut h = History::new(temp_dir("mixed"));
        h.record(Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "small".into(),
                content: "tiny".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "big".into(),
                content: "z".repeat(OFFLOAD_CAP_CHARS + 1_000).into(),
                is_error: false,
            },
        ]));
        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[0] else {
            panic!()
        };
        assert_eq!(content.as_text(), "tiny");
        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[1] else {
            panic!()
        };
        assert!(content.as_text().contains("saved to"));
    }

    #[test]
    fn record_and_replace_all_write_through_to_rollout() {
        let dir = temp_dir("writethrough");
        let session = dir.join("session.jsonl");
        let mut h = History::new(dir.clone());
        h.attach_rollout(Rollout::new(session.clone()));
        h.record(Message::user_text("first"));
        h.record(Message::assistant(vec![ContentBlock::Text {
            text: "reply".into(),
        }]));
        h.replace_all(vec![Message::user_text("[summary]")]);
        h.record(Message::user_text("after compaction"));

        // The file replays to exactly the in-memory history, compaction included.
        let loaded = crate::rollout::load_session(&session).unwrap();
        assert_eq!(loaded, h.messages());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn provider_usage_writes_through_and_survives_compaction() {
        let dir = temp_dir("usage-writethrough");
        let session = dir.join("session.jsonl");
        let mut history = History::new(dir.clone());
        history.attach_rollout(Rollout::new(session.clone()));
        let record = ProviderUsageRecord {
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            route_revision: 1,
            model: "mock".into(),
            attempt_kind: kloop_protocol::ProviderAttemptKind::Primary,
            operation: crate::usage::UsageOperation::Sampling,
            usage: kloop_protocol::Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_creation_input_tokens: 4,
            },
        };
        history.record_provider_usage(record.clone());
        history.replace_all(Vec::new());

        assert_eq!(
            history.provider_usage().records(),
            std::slice::from_ref(&record)
        );
        assert_eq!(
            crate::rollout::resume_session(&session)
                .unwrap()
                .provider_usage
                .records(),
            &[record]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn resumed_history_installs_items_without_reappending() {
        let dir = temp_dir("resume");
        let session = dir.join("session.jsonl");
        let mut rollout = Rollout::new(session.clone());
        rollout
            .append_message(&Message::user_text("earlier"))
            .unwrap();
        drop(rollout);

        let resumed = crate::rollout::resume_session(&session).unwrap();
        let mut h = History::resume(dir.clone(), resumed);
        assert_eq!(h.messages(), &[Message::user_text("earlier")]);
        // New records append after the resumed content, once each.
        h.record(Message::user_text("later"));
        assert_eq!(
            crate::rollout::load_session(&session).unwrap(),
            h.messages()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rebase_rewinds_onto_a_fork_and_redirects_writes() {
        use crate::rollout::fork_session;
        let dir = temp_dir("rebase");
        let session = dir.join("session.jsonl");
        let mut h = History::new(dir.clone());
        h.attach_rollout(Rollout::new(session.clone()));
        h.record(Message::user_text("one"));
        h.record(Message::assistant(vec![ContentBlock::Text {
            text: "done".into(),
        }]));
        h.record(Message::user_text("two"));
        h.record(Message::assistant(vec![ContentBlock::Text {
            text: "bye".into(),
        }]));

        // Fork before the "two" turn (route receipt + two message lines) and
        // rewind the live history onto that branch.
        let fork_path = fork_session(&session, Some(3), &dir).unwrap();
        let resumed = crate::rollout::resume_session(&fork_path).unwrap();
        h.rebase(resumed);
        assert_eq!(
            h.messages(),
            &[
                Message::user_text("one"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "done".into(),
                }]),
            ]
        );

        // New records land in the fork file; the original branch is untouched.
        h.record(Message::user_text("three"));
        assert_eq!(
            crate::rollout::load_session(&fork_path).unwrap(),
            h.messages()
        );
        assert_eq!(
            crate::rollout::load_session(&session).unwrap().len(),
            4,
            "the original branch keeps its full history"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn synced_offload_counter_never_clobbers_existing_files() {
        let dir = temp_dir("counter");
        std::fs::create_dir_all(&dir).unwrap();
        // A file left behind by the session being resumed.
        std::fs::write(dir.join("off-0007.txt"), "old spill").unwrap();
        sync_offload_counter(&dir);

        let mut h = History::new(dir.clone());
        h.record(tool_result("n".repeat(OFFLOAD_CAP_CHARS + 1_000)));
        assert_eq!(
            std::fs::read_to_string(dir.join("off-0007.txt")).unwrap(),
            "old spill",
            "the resumed session's spill file must survive new spills"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn explicit_switch_filters_request_view_and_switching_back_restores_reasoning() {
        use std::sync::Arc;

        use crate::provider_route::ProviderCatalog;
        use crate::provider_route::ProviderCatalogEntry;
        use crate::provider_route::SessionProviderState;
        use crate::provider_route::SwitchOutcome;
        use kloop_protocol::ProviderAvailabilityCode;
        use kloop_provider::Provider;

        let fingerprint = Provider::mock(Vec::new()).endpoint_fingerprint();
        let entry = |id: &str, fallback: Option<&str>| ProviderCatalogEntry {
            id: id.into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: fingerprint.clone(),
            default_model: format!("{id}-model"),
            models: fallback.map_or_else(
                || vec![format!("{id}-model")],
                |fallback| vec![format!("{id}-model"), fallback.to_string()],
            ),
            fallback_model: fallback.map(str::to_string),
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
        };
        let catalog = Arc::new(
            ProviderCatalog::new(vec![entry("a", None), entry("b", Some("b-fallback"))]).unwrap(),
        );
        let initial = catalog.initial_route("a", None).unwrap();
        let state = SessionProviderState::from_route(Arc::clone(&catalog), initial.clone());
        let mut history = History::new(temp_dir("provider-switch-view"));
        history.ensure_initial_provider_route(&initial).unwrap();
        history.record(Message::user_text("question"));
        history.record_provider_assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "readable reasoning".into(),
                    signature: "opaque".into(),
                },
                ContentBlock::ToolResult {
                    tool_use_id: "nested".into(),
                    content: ToolResultContent::Blocks(vec![
                        ContentBlock::Text {
                            text: "keep nested text".into(),
                        },
                        ContentBlock::Thinking {
                            thinking: "nested reasoning".into(),
                            signature: "nested opaque".into(),
                        },
                    ]),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            &initial.primary_attempt(),
        );
        let canonical = history.messages().to_vec();

        let changed = history.switch_provider(&state, 1, "b", None).unwrap();
        assert!(matches!(
            changed,
            SwitchOutcome::Changed {
                continuity: ReasoningContinuity::Filtered,
                ..
            }
        ));
        let route_b = state.freeze();
        let view_b = history
            .provider_request_view(&route_b.primary_attempt())
            .unwrap();
        assert_eq!(
            view_b[1].content,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: "nested".into(),
                    content: ToolResultContent::Blocks(vec![ContentBlock::Text {
                        text: "keep nested text".into(),
                    }]),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "answer".into()
                },
            ]
        );
        assert_eq!(history.messages(), canonical.as_slice());
        let fallback_error = history
            .provider_request_view(&route_b.fallback_attempt().unwrap())
            .unwrap_err();
        assert!(fallback_error.to_string().contains("not authorized"));

        history.switch_provider(&state, 2, "a", None).unwrap();
        let view_a = history
            .provider_request_view(&state.freeze().primary_attempt())
            .unwrap();
        assert_eq!(view_a, canonical);
    }

    #[test]
    fn chat_request_view_validates_source_then_removes_reasoning() {
        use std::sync::Arc;

        use crate::provider_route::ProviderCatalog;
        use crate::provider_route::ProviderCatalogEntry;
        use crate::provider_route::SessionProviderState;
        use kloop_protocol::ProviderAvailabilityCode;
        use kloop_provider::Provider;

        let source_provider = Provider::mock(Vec::new());
        let source_fingerprint = source_provider.endpoint_fingerprint();
        let chat_provider = || Provider::OpenAiCompat {
            key: "unused".into(),
            base: "https://chat.invalid".into(),
        };
        let chat_fingerprint = chat_provider().endpoint_fingerprint();
        let catalog = Arc::new(
            ProviderCatalog::new(vec![
                ProviderCatalogEntry {
                    id: "source".into(),
                    api_family: ProviderApiFamily::Mock,
                    endpoint_fingerprint: source_fingerprint,
                    default_model: "source-model".into(),
                    models: vec!["source-model".into()],
                    fallback_model: None,
                    availability: ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
                },
                ProviderCatalogEntry {
                    id: "chat".into(),
                    api_family: ProviderApiFamily::OpenAiChatCompletions,
                    endpoint_fingerprint: chat_fingerprint,
                    default_model: "chat-model".into(),
                    models: vec!["chat-model".into()],
                    fallback_model: None,
                    availability: ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    factory: Arc::new(move || Ok(chat_provider())),
                },
            ])
            .unwrap(),
        );
        let initial = catalog.initial_route("source", None).unwrap();
        let state = SessionProviderState::from_route(catalog, initial.clone());
        let mut history = History::new(temp_dir("chat-request-view"));
        history.ensure_initial_provider_route(&initial).unwrap();
        history.record(Message::user_text("question"));
        history.record_provider_assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "private".into(),
                    signature: "opaque".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            &initial.primary_attempt(),
        );

        history.switch_provider(&state, 1, "chat", None).unwrap();
        let view = history
            .provider_request_view(&state.freeze().primary_attempt())
            .unwrap();
        assert_eq!(
            view[1].content,
            vec![ContentBlock::Text {
                text: "answer".into(),
            }]
        );
        assert_eq!(view[1].provider_provenance, None);
    }

    #[test]
    fn route_append_failure_keeps_memory_state_and_remembered_model() {
        use std::sync::Arc;

        use crate::provider_route::ProviderCatalog;
        use crate::provider_route::ProviderCatalogEntry;
        use crate::provider_route::SessionProviderState;
        use kloop_protocol::ProviderAvailabilityCode;
        use kloop_provider::Provider;

        let fingerprint = Provider::mock(Vec::new()).endpoint_fingerprint();
        let entry = |id: &str| ProviderCatalogEntry {
            id: id.into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: fingerprint.clone(),
            default_model: format!("{id}-model"),
            models: vec![format!("{id}-model")],
            fallback_model: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
        };
        let catalog = Arc::new(ProviderCatalog::new(vec![entry("a"), entry("b")]).unwrap());
        let initial = catalog.initial_route("a", None).unwrap();
        let state = SessionProviderState::from_route(catalog, initial.clone());
        let root = temp_dir("route-persist-failure");
        let session = root.join("session.jsonl");
        let mut history = History::new(root.join("offload"));
        history.attach_rollout(Rollout::new_with_initial_route(session, &initial).unwrap());
        std::fs::remove_dir_all(&root).unwrap();
        std::fs::write(&root, b"blocks parent directory creation").unwrap();

        let error = history.switch_provider(&state, 1, "b", None).unwrap_err();
        assert!(matches!(error, ProviderSwitchError::Persistence(_)));
        assert_eq!(state.active_route().revision, 1);
        assert_eq!(state.active_route().provider_id, "a");
        assert_eq!(state.remembered_models().get("b"), None);
        assert_eq!(history.provider_routes().len(), 1);
        let _ = std::fs::remove_file(root);
    }

    #[test]
    fn small_tool_result_kept_verbatim() {
        let mut h = History::new(temp_dir("small"));
        h.record(tool_result("hello".into()));
        assert_eq!(
            h.messages()[0],
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "hello".into(),
                    is_error: false,
                }],
                provider_provenance: None,
            }
        );
    }
}
