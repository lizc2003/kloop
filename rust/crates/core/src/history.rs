use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use crate::provider_route::FrozenProviderAttempt;
use crate::provider_route::FrozenProviderRoute;
use crate::provider_route::ProvenanceMismatch;
use crate::provider_route::RouteReopened;
use crate::provider_route::SwitchError;
use crate::rollout::ResumedSession;
use crate::rollout::Rollout;
use crate::rollout::SessionRuntime;
use crate::rollout::TurnTerminal;
use crate::usage::{ProviderUsageRecord, UsageLedger};
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::ProviderApiFamily;
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
///
/// **That guarantee is per result, not per round.** A read-back that shares a
/// round with four other large results can still be spilled by
/// [`ROUND_OFFLOAD_CAP_CHARS`] below, handing the model a second pointer where
/// it asked for the file. Known and accepted: the recourse is a narrower
/// `offset`, and the shape of a fix if it is ever seen for real is cc's
/// `skipToolNames` — a round budget that skips tools which bound themselves.
pub const OFFLOAD_CAP_CHARS: usize = 32_000;

/// Char budget for one round's tool results taken *together*.
/// [`OFFLOAD_CAP_CHARS`] only ever judges one result, so ten results of 31 999
/// chars each pass it untouched and put ~320 000 chars (~80 000 tokens) into
/// the context in a single round — the hole cc's per-message budget exists to
/// close (`MAX_TOOL_RESULTS_PER_MESSAGE_CHARS`, 200 000 chars over a 50 000
/// per-result cap; the comment there names the same "10 × 40K in one turn"
/// shape). Same 4:1 ratio here. The two caps answer two different questions:
/// lowering this one would spill results that are individually fine, and
/// lowering that one would spill results no round ever had a problem with.
pub const ROUND_OFFLOAD_CAP_CHARS: usize = 4 * OFFLOAD_CAP_CHARS;

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
        self.enforce_round_budget(&mut msg);
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

    /// Whether this session's route timeline has been opened yet. A revision can
    /// only follow an existing one, so a caller that wants to append before the
    /// first turn has to ask.
    pub fn has_provider_route(&self) -> bool {
        !self.provider_routes.is_empty()
    }

    pub fn ensure_initial_provider_route(
        &mut self,
        route: &FrozenProviderRoute,
    ) -> std::io::Result<()> {
        if let Some(existing) = self.provider_routes.last() {
            if existing.revision != route.revision()
                || existing.provider_id != route.provider_id()
                || existing.api_family != route.api_family()
                || existing.endpoint_fingerprint != route.endpoint_fingerprint()
                || existing.primary_model != route.primary_model()
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
        route: &FrozenProviderRoute,
        source: ProviderRouteSource,
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
            rollout.append_provider_route_changed(route, source, continuity)?
        } else {
            let receipt = route.receipt(self.next_memory_boundary, source, continuity);
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

    /// What a proposed route change would cost the request view: `Preserved`
    /// when every message replays onto it verbatim, `Filtered` when reasoning
    /// has to be stripped. Projected against a timeline that already carries
    /// the proposed receipt, because the projection's own authorization rule
    /// reads it — and computed before anything is written, so a refusal leaves
    /// the session exactly as it was.
    fn projected_continuity(
        &self,
        next: &FrozenProviderRoute,
        source: ProviderRouteSource,
    ) -> Result<ReasoningContinuity, kloop_provider::ProviderFailure> {
        let boundary = self
            .rollout
            .as_ref()
            .map(Rollout::next_boundary)
            .unwrap_or(self.next_memory_boundary);
        let mut hypothetical_routes = self.provider_routes.clone();
        hypothetical_routes.push(next.receipt(boundary, source, ReasoningContinuity::Preserved));
        let projected =
            provider_request_view(&self.items, &hypothetical_routes, &next.primary_attempt())?;
        Ok(if projected == self.items {
            ReasoningContinuity::Preserved
        } else {
            ReasoningContinuity::Filtered
        })
    }

    /// Start this session on `opening` — the route the front-end is opening it
    /// with: today's configured default when a session is reopened from disk,
    /// or the live session route when a running session rewinds onto a branch.
    /// Returns the route plus, when it is not the one the session was last
    /// written on, a receipt-backed [`RouteReopened`] for the front-end to say
    /// out loud.
    ///
    /// A `/provider` switch is a decision about a running conversation, not a
    /// standing preference: it lives as long as that session does. Reopening
    /// starts where a brand-new session would start, so changing
    /// `model_provider` in `~/.kloop/config.toml` moves every session that is
    /// opened afterwards — including ones written on a provider that has since
    /// left the file, which is what stops a config edit from making old
    /// transcripts unopenable. The hop is a durable route revision like any
    /// other, so the transcript says where each stretch of the session ran.
    pub fn adopt_provider_route(
        &mut self,
        opening: &FrozenProviderRoute,
    ) -> Result<(FrozenProviderRoute, Option<RouteReopened>), ProviderSwitchError> {
        let last = self.provider_routes.last().ok_or_else(|| {
            ProviderSwitchError::Persistence(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "history provider route timeline is missing",
            ))
        })?;
        // What the last receipt would look like had `opening` written it. The
        // comparison is whole-object rather than a hand-listed set of identity
        // fields, so a field added to the receipt later cannot quietly fall out
        // of this decision; the ones taken from `last` are exactly the ones that
        // are not route identity — boundary and source describe the recording,
        // continuity is a consequence of a hop, and effort is session-local and
        // re-seeds from configuration on every open.
        let candidate = ProviderRouteReceipt {
            revision: last.revision,
            effort: last.effort,
            ..opening.receipt(last.boundary, last.source, last.continuity)
        };
        if candidate == *last {
            // Reopening on what the session already recorded: nothing to write,
            // and the receipt resolves by construction because `opening` came
            // from the running catalog.
            let route = opening
                .at_revision_with_continuity(last.revision, last.continuity)
                .map_err(ProviderSwitchError::Route)?;
            return Ok((route, None));
        }
        let from_provider = last.provider_id.clone();
        let from_model = last.primary_model.clone();
        let revision = last
            .revision
            .checked_add(1)
            .ok_or(ProviderSwitchError::Route(SwitchError::RevisionExhausted))?;
        let tentative = opening
            .at_revision(revision)
            .map_err(ProviderSwitchError::Route)?;
        let continuity = self
            .projected_continuity(&tentative, ProviderRouteSource::Reopened)
            .map_err(ProviderSwitchError::History)?;
        let next = opening
            .at_revision_with_continuity(revision, continuity)
            .map_err(ProviderSwitchError::Route)?;
        self.append_provider_route_changed(&next, ProviderRouteSource::Reopened, continuity)
            .map_err(ProviderSwitchError::Persistence)?;
        let reopened = RouteReopened {
            from_provider,
            from_model,
            to_provider: next.provider_id().to_string(),
            to_model: next.primary_model().to_string(),
            continuity,
        };
        Ok((next, Some(reopened)))
    }

    pub fn switch_provider(
        &mut self,
        state: &crate::provider_route::SessionProviderState,
        expected_revision: u64,
        provider_id: &str,
        model: Option<&str>,
        effort: crate::provider_route::EffortRequest,
    ) -> Result<crate::provider_route::SwitchOutcome, ProviderSwitchError> {
        state
            .switch_with(
                expected_revision,
                provider_id,
                model,
                effort,
                |previous, next| {
                    // A revision that moves only the effort stays on the same
                    // route, so nothing about reasoning replay changes and the
                    // continuity this route already carries rides forward.
                    // Projecting it would be asking whether a request view
                    // nobody is rewriting still matches itself.
                    let continuity = if previous.same_route(next) {
                        previous.continuity()
                    } else {
                        self.projected_continuity(next, ProviderRouteSource::ExplicitSwitch)
                            .map_err(ProviderSwitchCommitError::History)?
                    };
                    self.append_provider_route_changed(
                        next,
                        ProviderRouteSource::ExplicitSwitch,
                        continuity,
                    )
                    .map_err(ProviderSwitchCommitError::Persistence)?;
                    Ok(continuity)
                },
            )
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

    /// Current context size: the last real usage anchor plus a ~4 chars/token
    /// estimate for everything recorded after it.
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

    /// Bound one round's tool results *together*. Each result has already
    /// passed the per-result cap above; this spills the largest of what is
    /// left — largest first, stopping the moment the round is back under
    /// budget, which is cc's `selectFreshToReplace` — because a round of ten
    /// just-under-cap results is exactly the shape a per-result cap cannot see.
    ///
    /// **A spill that cannot reach disk leaves its result inline, in full.**
    /// The result was under the per-result cap, so the model can still use it;
    /// truncating it would destroy content to defend a budget that is about
    /// context size, not correctness, and the failure is the disk's, not the
    /// result's. The budget therefore loses whenever the offload store does —
    /// deliberately, and the same way cc's per-message budget loses when its
    /// own persist fails.
    fn enforce_round_budget(&self, msg: &mut Message) {
        let mut results: Vec<(usize, usize)> = msg
            .content
            .iter()
            .enumerate()
            .filter_map(|(index, block)| match block {
                ContentBlock::ToolResult {
                    content: ToolResultContent::Text(text),
                    ..
                } => Some((index, text.chars().count())),
                ContentBlock::ToolResult { .. }
                | ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolUse { .. } => None,
            })
            .collect();
        let mut total: usize = results.iter().map(|(_, chars)| chars).sum();
        if total <= ROUND_OFFLOAD_CAP_CHARS {
            return;
        }
        results.sort_by_key(|&(_, chars)| std::cmp::Reverse(chars));
        for (index, chars) in results {
            if total <= ROUND_OFFLOAD_CAP_CHARS {
                return;
            }
            // Every index came from exactly this pattern a moment ago.
            let ContentBlock::ToolResult {
                content: ToolResultContent::Text(text),
                ..
            } = &mut msg.content[index]
            else {
                continue;
            };
            let content = std::mem::take(text);
            match self.spill_to_disk(&content) {
                Ok((pointer, _)) if pointer.chars().count() < chars => {
                    total = total - chars + pointer.chars().count();
                    *text = pointer;
                }
                // Spilling this one would make the round *bigger*: a preview
                // plus a path costs more than the result it replaces. Results
                // come largest first, so nothing left can do better either —
                // undo the write and stop. A round that is over budget purely
                // by having many small results has no spill that helps, and
                // spending the context on pointers instead of content would be
                // the worst of both.
                Ok((_, path)) => {
                    let _ = std::fs::remove_file(path);
                    *text = content;
                    return;
                }
                Err(_) => *text = content,
            }
        }
    }

    fn spill(&mut self, content: &str) -> String {
        match self.spill_to_disk(content) {
            Ok((pointer, _)) => pointer,
            // Unlike a round-budget spill this one has no inline fallback: the
            // result is over the per-result cap, which is the size no single
            // result may enter the history at. Truncation is what is left.
            Err(e) => format!(
                "{preview}\n[offload to disk failed ({e}); output truncated]",
                preview = preview(content)
            ),
        }
    }

    /// Write the whole result to the offload store and return what the model
    /// sees instead — the head/tail preview plus the file's path — together
    /// with that path, so a caller that decides against the trade can take the
    /// file back out. `Err` means nothing was written and the caller still
    /// holds the only copy.
    fn spill_to_disk(&self, content: &str) -> std::io::Result<(String, PathBuf)> {
        let id = format!("off-{:04}", NEXT_OFFLOAD_ID.fetch_add(1, Ordering::Relaxed));
        let path = self.offload_dir.join(format!("{id}.txt"));
        std::fs::create_dir_all(&self.offload_dir).and_then(|()| std::fs::write(&path, content))?;
        // cc's shape: hand over a path and let the general tools work on it, rather
        // than mint an opaque id for a reader that exists only to dereference it.
        // The advice is to extract *in place* — cc reads a 2 MB spec with
        // `python3 -c` printing three fields, never by pulling the file into the
        // conversation — and inside a program even that extraction costs no
        // context. Sandboxed bash can read this directory (the private state root
        // is denied for its credential, with the offload store carved back out)
        // but still cannot write it.
        let pointer = format!(
            "{preview}\n[full output saved to {path} ({chars} chars). Query it in place instead \
             of reading it back: bash with `python3 -c '...'` over that path, printing only the \
             fields you need, so only what you extract enters the context]",
            preview = preview(content),
            chars = content.chars().count(),
            path = path.display(),
        );
        Ok((pointer, path))
    }
}

/// The head and tail a spilled result keeps in the history, with the middle
/// marked as dropped. Shared by both spill outcomes so a failed offload reads
/// the same as a successful one up to the pointer.
fn preview(content: &str) -> String {
    let head: String = content.chars().take(HEAD_CHARS).collect();
    let tail_rev: Vec<char> = content.chars().rev().take(TAIL_CHARS).collect();
    let tail: String = tail_rev.into_iter().rev().collect();
    format!("{head}\n…[truncated]…\n{tail}")
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
        // Same five rules the rollout validator applies to the same provenance;
        // only the wording is ours. A line boundary is not one of them here — a
        // request view has no lines.
        let source_index = crate::provider_route::validate_provenance(
            source,
            routes,
            crate::provider_route::ReasoningShape::of(message),
            /*origin_line_boundary*/ None,
        )
        .map_err(|mismatch| {
            kloop_provider::ProviderFailure::protocol(match mismatch {
                ProvenanceMismatch::UnknownRevision => {
                    "reasoning history references an unknown provider route revision"
                }
                ProvenanceMismatch::OriginOutsideInterval => {
                    "reasoning history origin lies outside its provider route interval"
                }
                // Not reachable while this caller passes no line boundary;
                // spelled out rather than panicking, so a later caller that does
                // pass one gets an error instead of an abort.
                ProvenanceMismatch::OriginNotOnItsLine => {
                    "reasoning history origin does not match its recorded line boundary"
                }
                ProvenanceMismatch::IdentityMismatch => {
                    "reasoning history provenance does not match its producing route"
                }
                ProvenanceMismatch::BlockShapeMismatch => {
                    "reasoning history block shape does not match its producing API family"
                }
            })
        })?;

        let chat_target = attempt.identity().api_family == ProviderApiFamily::OpenAiChatCompletions;
        if !chat_target && source.exact_replay_compatible(attempt.identity()) {
            projected.push(message.clone());
            continue;
        }
        // Either durable change kind authorizes the strip: a `/provider` the user
        // typed, or the `Reopened` hop a session takes when it comes back on a
        // different route. Both are recorded, both are visible in the
        // transcript, and refusing the second would leave a reopened session
        // able to open but unable to take a turn.
        let sanctioned_switch = source.route_revision < active.revision
            && routes[source_index + 1..].iter().any(|route| {
                matches!(
                    route.source,
                    ProviderRouteSource::ExplicitSwitch | ProviderRouteSource::Reopened
                )
            });
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

    /// One round's tool results, in request order.
    fn tool_results(results: &[(&str, String)]) -> Message {
        Message::tool_results(
            results
                .iter()
                .map(|(id, content)| ContentBlock::ToolResult {
                    tool_use_id: (*id).into(),
                    content: content.clone().into(),
                    is_error: false,
                })
                .collect(),
        )
    }

    fn recorded_text(h: &History, index: usize) -> String {
        let ContentBlock::ToolResult { content, .. } = &h.messages()[0].content[index] else {
            panic!("expected tool result");
        };
        content.as_text().into_owned()
    }

    /// Ten results that each clear the per-result cap put 175 000 chars into
    /// one round. The two largest go to disk — largest first, not first-come —
    /// and the round stops spilling the moment it is back under budget.
    #[test]
    fn a_wide_round_spills_its_largest_results_until_it_is_under_budget() {
        let dir = temp_dir("round-budget");
        let mut h = History::new(dir.clone());
        let round: Vec<(&str, String)> = vec![
            ("c", "c".repeat(30_000)),
            ("s0", "0".repeat(5_000)),
            ("a", "a".repeat(31_000)),
            ("s1", "1".repeat(5_000)),
            ("d", "d".repeat(29_500)),
            ("s2", "2".repeat(5_000)),
            ("b", "b".repeat(30_500)),
            ("s3", "3".repeat(5_000)),
            ("e", "e".repeat(29_000)),
            ("s4", "4".repeat(5_000)),
        ];
        assert!(
            round
                .iter()
                .all(|(_, text)| text.chars().count() < OFFLOAD_CAP_CHARS)
        );
        h.record(tool_results(&round));

        // Every result is still there, in request order.
        let ids: Vec<&str> = h.messages()[0]
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
                other => panic!("expected tool result, got {other:?}"),
            })
            .collect();
        assert_eq!(
            ids,
            vec!["c", "s0", "a", "s1", "d", "s2", "b", "s3", "e", "s4"]
        );

        // Only the two largest were spilled, and each is a pointer to its own
        // file holding the whole result.
        for (index, id) in [(2, "a"), (6, "b")] {
            let text = recorded_text(&h, index);
            assert!(text.contains("Query it in place"), "{id}: {text}");
            let path = dir.join(format!("{}.txt", pointer_id(&text)));
            let original = &round.iter().find(|(name, _)| *name == id).unwrap().1;
            assert_eq!(&std::fs::read_to_string(&path).unwrap(), original);
        }
        for (index, (id, original)) in round.iter().enumerate() {
            if index == 2 || index == 6 {
                continue;
            }
            assert_eq!(
                &recorded_text(&h, index),
                original,
                "{id} was not left alone"
            );
        }

        let total: usize = (0..round.len())
            .map(|index| recorded_text(&h, index).chars().count())
            .sum();
        assert!(total <= ROUND_OFFLOAD_CAP_CHARS, "{total}");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Over budget purely by count: 80 results of 1 800 chars each. Every one
    /// of them is smaller than the preview-plus-path that would replace it, so
    /// spilling would cost context rather than save it. The round stays inline
    /// and the store keeps nothing.
    #[test]
    fn a_round_of_results_too_small_to_shrink_is_left_alone() {
        let dir = temp_dir("round-budget-small");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = History::new(dir.clone());
        let round: Vec<(String, String)> = (0..80)
            .map(|n| (format!("t{n}"), "s".repeat(1_800)))
            .collect();
        let round: Vec<(&str, String)> = round
            .iter()
            .map(|(id, text)| (id.as_str(), text.clone()))
            .collect();
        let total: usize = round.iter().map(|(_, text)| text.chars().count()).sum();
        assert!(total > ROUND_OFFLOAD_CAP_CHARS, "{total}");

        let msg = tool_results(&round);
        h.record(msg.clone());
        assert_eq!(h.messages(), [msg].as_slice());
        assert_eq!(
            std::fs::read_dir(&dir).map(Iterator::count).unwrap_or(0),
            0,
            "a spill that could not help was left on disk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A round under budget is recorded byte for byte, and nothing reaches the
    /// offload store — the directory is not even created.
    #[test]
    fn a_round_under_budget_is_recorded_verbatim() {
        let dir = temp_dir("round-budget-under");
        let _ = std::fs::remove_dir_all(&dir);
        let mut h = History::new(dir.clone());
        let round = vec![
            ("t1", "a".repeat(31_000)),
            ("t2", "b".repeat(31_000)),
            ("t3", "c".repeat(31_000)),
        ];
        let msg = tool_results(&round);
        h.record(msg.clone());

        assert_eq!(h.messages(), [msg].as_slice());
        assert!(
            !dir.exists(),
            "an under-budget round touched the offload store"
        );
    }

    /// Two budgets, two answers when the disk says no. A round-budget spill
    /// keeps its result inline in full — it was under the per-result cap, so
    /// losing content to defend a context budget would be the worse trade. A
    /// per-result spill has no such option and still truncates.
    #[test]
    fn a_spill_that_cannot_reach_disk_stays_inline_for_the_round_budget_only() {
        let dir = temp_dir("round-budget-no-disk");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.parent().expect("temp dir has a parent")).unwrap();
        // A file where the offload directory should be: create_dir_all fails.
        std::fs::write(&dir, "not a directory").unwrap();

        let mut h = History::new(dir.clone());
        let round = vec![
            ("t1", "a".repeat(31_000)),
            ("t2", "b".repeat(31_000)),
            ("t3", "c".repeat(31_000)),
            ("t4", "d".repeat(31_000)),
            ("t5", "e".repeat(31_000)),
        ];
        let msg = tool_results(&round);
        h.record(msg.clone());
        assert_eq!(h.messages(), [msg].as_slice());

        h.record(tool_result("z".repeat(OFFLOAD_CAP_CHARS + 1_000)));
        let ContentBlock::ToolResult { content, .. } = &h.messages()[1].content[0] else {
            panic!("expected tool result");
        };
        let truncated = content.as_text();
        assert!(truncated.contains("offload to disk failed"), "{truncated}");
        assert!(truncated.contains("output truncated"), "{truncated}");
        assert!(truncated.chars().count() < OFFLOAD_CAP_CHARS, "{truncated}");
        let _ = std::fs::remove_file(dir);
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

        use crate::provider_route::EffortRequest;
        use crate::provider_route::ProviderCatalog;
        use crate::provider_route::ProviderCatalogEntry;
        use crate::provider_route::SessionProviderState;
        use crate::provider_route::SwitchOutcome;
        use kloop_protocol::ProviderAvailabilityCode;
        use kloop_provider::Provider;

        let fingerprint = Provider::mock(Vec::new()).endpoint_fingerprint();
        let entry = |id: &str, second_model: Option<&str>| ProviderCatalogEntry {
            id: id.into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: fingerprint.clone(),
            default_model: format!("{id}-model"),
            models: second_model.map_or_else(
                || vec![format!("{id}-model")],
                |second| vec![format!("{id}-model"), second.to_string()],
            ),
            context_window: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            sends_thinking: true,
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

        let changed = history
            .switch_provider(&state, 1, "b", None, EffortRequest::Inherit)
            .unwrap();
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
        history
            .switch_provider(&state, 2, "a", None, EffortRequest::Inherit)
            .unwrap();
        let view_a = history
            .provider_request_view(&state.freeze().primary_attempt())
            .unwrap();
        assert_eq!(view_a, canonical);
    }

    #[test]
    fn chat_request_view_validates_source_then_removes_reasoning() {
        use std::sync::Arc;

        use crate::provider_route::EffortRequest;
        use crate::provider_route::ProviderCatalog;
        use crate::provider_route::ProviderCatalogEntry;
        use crate::provider_route::SessionProviderState;
        use kloop_protocol::ProviderAvailabilityCode;
        use kloop_provider::Provider;

        let source_provider = Provider::mock(Vec::new());
        let source_fingerprint = source_provider.endpoint_fingerprint();
        let chat_provider = || Provider::OpenAiCompat {
            cred: kloop_provider::Credential::bearer("unused"),
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
                    context_window: None,
                    availability: ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    sends_thinking: true,
                    factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
                },
                ProviderCatalogEntry {
                    id: "chat".into(),
                    api_family: ProviderApiFamily::OpenAiChatCompletions,
                    endpoint_fingerprint: chat_fingerprint,
                    default_model: "chat-model".into(),
                    models: vec!["chat-model".into()],
                    context_window: None,
                    availability: ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    sends_thinking: true,
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

        history
            .switch_provider(&state, 1, "chat", None, EffortRequest::Inherit)
            .unwrap();
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

    /// The chat rail produces reasoning of its own — DeepSeek- and GLM-shaped
    /// models stream `reasoning_content`, which the adapter keeps as
    /// signature-less thinking. The next turn on that same route must strip it,
    /// exactly as a switched-away block is stripped, instead of rejecting the
    /// history as a shape the chat family could not have produced.
    #[test]
    fn chat_produced_reasoning_is_stripped_not_rejected() {
        use crate::provider_route::ProviderCatalog;
        use kloop_provider::Provider;

        let (_catalog, route) = ProviderCatalog::from_provider(
            "chat",
            Provider::OpenAiCompat {
                cred: kloop_provider::Credential::bearer("unused"),
                base: "https://chat.invalid".into(),
            },
            "chat-model",
            vec!["chat-model".into()],
        )
        .unwrap();
        let mut history = History::new(temp_dir("chat-self-reasoning"));
        history.ensure_initial_provider_route(&route).unwrap();
        history.record(Message::user_text("question"));
        history.record_provider_assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "streamed as reasoning_content".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            &route.primary_attempt(),
        );

        let view = history
            .provider_request_view(&route.primary_attempt())
            .unwrap();
        assert_eq!(view.len(), 2);
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

        use crate::provider_route::EffortRequest;
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
            context_window: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            sends_thinking: true,
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

        let error = history
            .switch_provider(&state, 1, "b", None, EffortRequest::Inherit)
            .unwrap_err();
        assert!(matches!(error, ProviderSwitchError::Persistence(_)));
        assert_eq!(state.active_route().revision, 1);
        assert_eq!(state.active_route().provider_id, "a");
        assert_eq!(state.remembered_models().get("b"), None);
        assert_eq!(history.provider_routes().len(), 1);
        let _ = std::fs::remove_file(root);
    }

    /// The dogfood failure of plan 132, and the rule it settled on: a session
    /// reopens on the route a brand-new session would open on. The provider it
    /// was written on may be gone from configuration (the failure that started
    /// this) or merely no longer the default (the rule) — either way the hop is
    /// recorded, said out loud, and not repeated on the next open.
    #[test]
    fn a_reopened_session_starts_on_the_route_it_is_opened_with() {
        use std::sync::Arc;

        use crate::provider_route::ProviderCatalog;
        use crate::provider_route::ProviderCatalogEntry;
        use crate::provider_route::SessionProviderState;
        use kloop_protocol::ProviderAvailabilityCode;
        use kloop_provider::Provider;

        let dir = temp_dir("route-reopen");
        let _ = std::fs::remove_dir_all(&dir);
        let session = dir.join("session.jsonl");
        let fingerprint = Provider::mock(Vec::new()).endpoint_fingerprint();
        let entry = |id: &str| ProviderCatalogEntry {
            id: id.into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: fingerprint.clone(),
            default_model: format!("{id}-model"),
            models: vec![format!("{id}-model")],
            context_window: None,
            availability: ProviderAvailabilityCode::Ready,
            default_effort: None,
            sends_thinking: true,
            factory: Arc::new(|| Ok(Provider::mock(Vec::new()))),
        };

        // The session as it was written: one turn on a provider named `gone`,
        // the assistant reply carrying reasoning bound to that route.
        let old_catalog = Arc::new(ProviderCatalog::new(vec![entry("gone")]).unwrap());
        let old_route = old_catalog.initial_route("gone", None).unwrap();
        let mut history = History::new(dir.clone());
        history
            .attach_rollout(Rollout::new_with_initial_route(session.clone(), &old_route).unwrap());
        history.record(Message::user_text("question"));
        history.record_provider_assistant(
            vec![
                ContentBlock::Thinking {
                    thinking: "reasoning from the old rail".into(),
                    signature: "opaque".into(),
                },
                ContentBlock::Text {
                    text: "answer".into(),
                },
            ],
            &old_route.primary_attempt(),
        );
        let canonical = history.messages().to_vec();

        // Configuration has since dropped `gone` and defaults to `kept`.
        let new_catalog = Arc::new(ProviderCatalog::new(vec![entry("kept")]).unwrap());
        let default_route = new_catalog.initial_route("kept", None).unwrap();
        let (adopted, reopened) = history.adopt_provider_route(&default_route).unwrap();
        assert_eq!(
            reopened,
            Some(crate::provider_route::RouteReopened {
                from_provider: "gone".into(),
                from_model: "gone-model".into(),
                to_provider: "kept".into(),
                to_model: "kept-model".into(),
                continuity: ReasoningContinuity::Filtered,
            })
        );
        assert_eq!(adopted.revision(), 2);
        assert_eq!(adopted.provider_id(), "kept");

        // The canonical transcript is untouched; only the request view drops the
        // reasoning the new rail cannot replay — which the `reopened` receipt,
        // like an explicit switch, is what authorizes.
        assert_eq!(history.messages(), canonical.as_slice());
        let view = history
            .provider_request_view(&adopted.primary_attempt())
            .unwrap();
        assert_eq!(
            view[1].content,
            vec![ContentBlock::Text {
                text: "answer".into()
            }]
        );

        // Re-reading the file accepts what the hop wrote, a timeline whose first
        // receipt names a provider no longer in the catalog restores anyway, and
        // opening again on the same default writes nothing: reopening twice must
        // not stack revisions.
        let reread = crate::rollout::resume_session(&session).unwrap();
        let timeline = reread.snapshot.provider_routes.clone();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[1].source, ProviderRouteSource::Reopened);
        assert_eq!(timeline[1].provider_id, "kept");
        assert_eq!(timeline[1].continuity, ReasoningContinuity::Filtered);
        assert_eq!(
            SessionProviderState::from_timeline(Arc::clone(&new_catalog), &timeline)
                .unwrap()
                .active_route()
                .provider_id,
            "kept"
        );
        let mut resumed = History::resume(dir.clone(), reread);
        let (again, reopened) = resumed.adopt_provider_route(&default_route).unwrap();
        assert_eq!(reopened, None);
        assert_eq!(again.revision(), 2);
        assert_eq!(again.continuity(), ReasoningContinuity::Filtered);
        assert_eq!(resumed.provider_routes(), timeline.as_slice());

        // And the rule the user settled on: the provider it was written on is
        // still configured, but is no longer the default, so reopening moves —
        // an in-session `/provider` does not outlive the session that ran it.
        let both = Arc::new(ProviderCatalog::new(vec![entry("kept"), entry("other")]).unwrap());
        let other_default = both.initial_route("other", None).unwrap();
        let (moved, reopened) = resumed.adopt_provider_route(&other_default).unwrap();
        assert_eq!(
            reopened.map(|reopened| reopened.to_provider),
            Some("other".to_string())
        );
        assert_eq!(moved.revision(), 3);
        let _ = std::fs::remove_dir_all(dir);
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
                injected: None,
            }
        );
    }
}
