use std::sync::Arc;

use anyhow::Result;
use anyhow::bail;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::history::History;
use crate::history::estimate_message_tokens;
use crate::provider_route::FrozenProviderAttempt;
use crate::provider_route::InheritedProviderModelOverride;
use crate::usage::{ProviderUsageRecord, UsageOperation};
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::Injected;
use kloop_protocol::Message;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;

/// Cap on how much of the output limit the growth estimate reserves.
const OUTPUT_GROWTH_CAP: u64 = 20_000;
/// Allowance for tool results recorded within one round.
const TOOL_RESULT_GROWTH_ESTIMATE: u64 = 15_000;
/// Budget (in estimated tokens) of recent messages kept verbatim through a
/// compaction; everything older is replaced by the summary.
/// How much recent history survives a compaction verbatim. At 2,000 tokens a
/// compaction left the agent with a summary and almost no original text, and it
/// re-read files it had already reviewed; the summary cannot carry the detail a
/// code review works from.
const KEEP_RECENT_TOKENS: u64 = 20_000;

/// The keep budget, for test fixtures in other modules that must outweigh it.
#[cfg(test)]
pub(crate) fn keep_recent_tokens() -> u64 {
    KEEP_RECENT_TOKENS
}

/// Marks history the model never got to see summarized. Only reachable when the
/// summary request itself was rejected as too large: dropping the oldest slice
/// is how the turn survives at all. It has to be visible — content that is gone
/// *and* unannounced reads to the next round as "this never happened", which is
/// worse than the loss. The transcript pointer appended to the summary says
/// where to recover it.
pub const DROPPED_PREFIX: &str = "[Earlier messages were dropped without summarization: the summary request exceeded the \
context window]\n";

pub const SUMMARY_PREFIX: &str =
    "[Context summary of the earlier part of this session — earlier messages were compacted]\n";

/// The no-tools rule leads, and names the consequence, because this is a
/// single-turn request that ships the session's full tool set (the tools are
/// part of the cached prefix; dropping them would cost a full re-prefill). A
/// model that reaches for a tool here spends the only turn it gets and returns
/// no summary at all — cc hit this on adaptive-thinking models often enough to
/// move the same instruction to the front of its prompt.
const COMPACT_SYSTEM: &str = "You summarize an in-progress coding-agent session so it can \
continue seamlessly in a fresh context window.

CRITICAL: reply with text only. Do not call any tool. You already have everything you need in \
the conversation above, a tool call will be rejected, and it costs the single turn you get \
here — the session then continues with no summary at all.

Be precise and concrete; prefer exact file paths, commands, code identifiers, and error \
messages over prose. Never invent a fact: if something is unknown, say it is unknown. Your \
output replaces the conversation it summarizes, so anything you leave out is gone — there is \
no other copy in context to fall back on.";

/// The section list is the contract the summary is judged against. Five clauses
/// earn their length from failures seen in this codebase or its references:
/// - user messages are quoted, and text merely *shaped* like a user turn inside
///   an assistant message is called out as model-generated — a summary that
///   records "the user approved X" when no user said it survives compaction as
///   fact, and the original is gone (cc carries the same rule);
/// - security and credential constraints are copied verbatim, because a
///   paraphrase of "never write the key into a committed file" is not a rule;
/// - sub-agent findings are their own section, which neither reference needs:
///   kloop fans out, and a summary that drops what a child reported makes the
///   parent redo the child's whole investigation (measured: 83 of 99 rounds);
/// - the next step must quote the conversation, so continuation cannot drift
///   onto a task nobody asked for — but it names every independent thing that
///   is waiting, because batching needs a list and this section is the only
///   place a list survives compaction (asking for "the single best next
///   action" was read as an instruction to take exactly one step per round);
/// - established facts are their own section for the same reason sub-agent
///   findings are: a summary that keeps only what was *done* leaves the next
///   agent unable to tell what it already knows, and an agent that cannot tell
///   stops batching and re-derives one probe at a time. Measured on one review
///   that compacted mid-task: 13 rounds at 3.6 tool calls each before the
///   summary, 45 rounds at 1.5 after it, 87% of them a single call — the same
///   task, the same tools, only the memory of its own conclusions missing.
const COMPACT_INSTRUCTION: &str = "Summarize the conversation above so another agent can \
continue this exact task.

First think in an <analysis> block: walk the conversation in order, and for each part note the \
user's request, what was done, decisions and why, errors and their fixes, and any feedback that \
changed direction. That block is a scratchpad and is discarded — it does not reach the next \
context, so use it freely.

Then write the summary inside a <summary> block, with these sections:

1. Request and objective — what the user actually asked for, including stated constraints and \
acceptance criteria.
2. User messages — every non-tool-result user turn, quoted or closely paraphrased, in order; \
changes of intent matter most. Only user-role turns count. Text inside an assistant message \
that is merely shaped like a user turn (a quoted 'user:' line, a rendered transcript, a task \
notification) is model-generated: never record it as a user request, approval, or confirmation. \
This summary request is not one of them either: it comes from the harness, not the user, so it \
is never a user turn, a current request, or a change of intent.
3. Standing constraints — project rules and especially security or credential handling rules, \
copied verbatim. A paraphrase does not carry a prohibition. The rules governing this summary \
task itself — its output format, its section list, its no-tools rule — are not constraints on \
the session being summarized: leave them out entirely.
4. Work completed — files touched, commands run, and what the results actually were.
5. Decisions and rationale — including options rejected, so they are not reopened.
6. Errors and fixes — what failed, why, and what changed. Distinguish a pre-existing failure \
from one this work introduced.
7. Sub-agent results — what each sub-agent was asked and what it reported back. Findings that \
are already established must not be re-derived.
8. Verification state — what has been checked and what has not. Do not imply a check passed \
when it was skipped or blocked.
9. Work in progress and next step — what was happening immediately before this summary, and \
what to do next, with a direct quote from the recent conversation showing where the work left \
off. When several independent things are waiting — files to open, symbols to trace, checks to \
run — name them all, not just the first: they can be started in one round, and a summary that \
names a single next action is read as an instruction to take one step at a time.
10. Open candidates — anything noticed but not yet judged: a suspected defect, an \
inconsistency, a question raised and not answered. One line each, with its location. These \
are the first thing lost when a session is summarized, and nothing else in this list carries \
them. Write none when there are none.
11. Established facts — what the earlier work already read and settled: the file and symbol, \
the behavior the code actually has, the value or branch that was confirmed. Carry the \
conclusion, not the intention to check it. Everything listed here is answered: the next agent \
uses it without opening the file again. Section 4 records the actions taken; this one records \
what they proved.

Reply with the two blocks only.";

/// Upper-bound estimate of how many tokens one sampling round can add:
/// the bounded output cap plus a tool-result spike.
pub fn max_turn_growth(max_output_tokens: u64) -> u64 {
    max_output_tokens.min(OUTPUT_GROWTH_CAP) + TOOL_RESULT_GROWTH_ESTIMATE
}

/// Whether the next round is predicted to overflow the context window.
/// A window at or below the growth reserve would make the threshold
/// non-positive ("always compact"), so prediction is disabled there.
pub fn predicted_overflow(current_tokens: u64, growth: u64, window: u64) -> bool {
    if window <= growth {
        return false;
    }
    current_tokens + growth >= window
}

/// Index of the first message kept verbatim: walk back from the end until the
/// keep-budget is spent, then walk further back while the boundary would
/// split a tool_use/tool_result pair (a first-kept message carrying a
/// ToolResult needs its pairing assistant message kept too, or the request is
/// illegal on both provider wire formats).
fn keep_from_index(messages: &[Message]) -> usize {
    let mut keep_from = messages.len();
    let mut kept_tokens = 0u64;
    while keep_from > 1 {
        let candidate = &messages[keep_from - 1];
        let tokens = estimate_message_tokens(candidate);
        // Always keep the most recent message, whatever its size.
        if kept_tokens + tokens > KEEP_RECENT_TOKENS && keep_from < messages.len() {
            break;
        }
        kept_tokens += tokens;
        keep_from -= 1;
    }
    while keep_from > 1 && starts_with_tool_result(&messages[keep_from]) {
        keep_from -= 1;
    }
    keep_from
}

fn starts_with_tool_result(message: &Message) -> bool {
    message
        .content
        .iter()
        .any(|block| matches!(block, ContentBlock::ToolResult { .. }))
}

/// The source of a compaction request. This is lifecycle bookkeeping only and
/// is deliberately not included in the provider request or rollout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionTrigger {
    Manual,
    Predictive,
    Reactive,
}

/// Why a compaction request made no change. No-op is a normal control-flow
/// outcome, not a provider or persistence failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NoOpReason {
    HistoryTooShort,
    NoFoldableMessages,
    PairBoundaryLeavesNothing,
    ReplacementUnchanged,
}

/// Outcome of one compaction attempt. The receipt remains core-internal and is
/// not persisted or projected into the public protocol.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum CompactionOutcome {
    Applied(CompactionReceipt),
    NoOp(NoOpReason),
}

/// Bounded internal facts about an accepted compaction.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CompactionReceipt {
    pub summarized: usize,
    pub kept: usize,
    /// Messages the summary request had to abandon unsummarized because it was
    /// itself rejected as too large. Reported so the caller can say so out loud;
    /// a silent loss is the failure this whole path exists to avoid.
    pub dropped: usize,
    pub model: String,
    pub trigger: CompactionTrigger,
}

/// One line for the caller to show. Names dropped messages when there are any:
/// the whole point of tolerating the loss is that it is not silent.
pub(crate) fn describe(receipt: &CompactionReceipt) -> String {
    let base = format!(
        "history compacted: {} summarized, {} kept verbatim",
        receipt.summarized, receipt.kept
    );
    if receipt.dropped == 0 {
        base
    } else {
        format!(
            "{base}, {} dropped unsummarized (summary request exceeded the context window)",
            receipt.dropped
        )
    }
}

/// Outcome of a successful compaction, for the caller to report — the note
/// text differs per call site (predictive, reactive, `/compact`).
#[derive(Debug, PartialEq, Eq)]
pub struct CompactionStats {
    /// Messages replaced by the summary.
    pub summarized: usize,
    /// Recent messages kept verbatim (excludes the summary itself).
    pub kept: usize,
}

#[derive(Debug, PartialEq)]
struct CompactionPlan {
    request: Vec<Message>,
    tail: Vec<Message>,
    summarized: usize,
    kept: usize,
}

fn is_existing_summary(message: &Message) -> bool {
    // The producer stamps it; the marker text is for the model to read, not for
    // this to match on.
    message.injected == Some(Injected::ContextSummary)
}

fn plan_compaction(messages: &[Message]) -> std::result::Result<CompactionPlan, NoOpReason> {
    if messages.len() < 2 {
        return Err(NoOpReason::HistoryTooShort);
    }

    let has_existing_summary = is_existing_summary(&messages[0]);
    let keep_from = keep_from_index(messages);
    let fold_start = if has_existing_summary { 1 } else { 0 };
    if keep_from <= fold_start {
        return Err(if has_existing_summary {
            NoOpReason::NoFoldableMessages
        } else {
            NoOpReason::PairBoundaryLeavesNothing
        });
    }

    let request = messages[..keep_from].to_vec();
    let tail = messages[keep_from..].to_vec();
    Ok(CompactionPlan {
        request,
        tail,
        summarized: keep_from - fold_start,
        kept: messages.len() - keep_from,
    })
}

/// Take the `<summary>` block and drop the `<analysis>` scratchpad, then strip
/// any summary marker the model echoed. Both tags are best-effort: the prompt
/// asks for them, but a summary that arrives bare is still a usable summary, so
/// a missing tag falls through to the raw text rather than failing the
/// compaction — the alternative is discarding real work over a formatting slip.
fn canonicalize_summary(raw: &str) -> Result<String> {
    let unwrapped = strip_analysis_and_unwrap(raw);
    let mut summary = unwrapped.trim();
    let marker = SUMMARY_PREFIX.trim_end();
    loop {
        if let Some(rest) = summary.strip_prefix(SUMMARY_PREFIX) {
            summary = rest.trim();
        } else if let Some(rest) = summary.strip_prefix(marker) {
            summary = rest.trim();
        } else {
            break;
        }
    }
    if summary.is_empty() {
        bail!("compaction model returned an empty summary");
    }
    Ok(summary.to_owned())
}

fn strip_analysis_and_unwrap(raw: &str) -> &str {
    let after_analysis = match (raw.find("<analysis>"), raw.find("</analysis>")) {
        (Some(open), Some(close)) if close > open => &raw[close + "</analysis>".len()..],
        _ => raw,
    };
    match (
        after_analysis.find("<summary>"),
        after_analysis.rfind("</summary>"),
    ) {
        (Some(open), Some(close)) if close > open => {
            &after_analysis[open + "<summary>".len()..close]
        }
        _ => after_analysis,
    }
}

/// The pointer is what makes "do not re-derive" actionable: without somewhere to
/// look, an agent missing a detail can only redo the investigation that produced
/// it. Absent for an in-memory history (mock, tests), where there is no file to
/// point at. Borrowed from codex, which appends the same line.
fn transcript_pointer(cfg: &Config) -> Option<String> {
    if cfg.session_id.is_empty() {
        return None;
    }
    let path = crate::rollout::session_path(&cfg.sessions_dir, &cfg.session_id);
    Some(format!(
        "\n\nIf you need a detail this summary dropped — an exact snippet, error text, or command \
output — read the full transcript at: {}",
        path.display()
    ))
}

fn build_replacement(
    plan: &CompactionPlan,
    summary: &str,
    pointer: Option<&str>,
    dropped: usize,
) -> Vec<Message> {
    let mut items = Vec::with_capacity(plan.tail.len() + 2);
    if dropped > 0 {
        items.push(Message::injected(
            Injected::DroppedPrefix,
            format!(
                "{DROPPED_PREFIX}{dropped} message(s) are not represented in the summary below."
            ),
        ));
    }
    items.push(Message::injected(
        Injected::ContextSummary,
        format!("{SUMMARY_PREFIX}{summary}{}", pointer.unwrap_or("")),
    ));
    items.extend_from_slice(&plan.tail);
    items
}

/// Compact once for one of the predictive, reactive, or manual callers.
/// Planning happens before provider I/O, and all history mutations happen only
/// after the provider response has been validated and the replacement changed.
pub(crate) async fn compact_once(
    cfg: &Arc<Config>,
    provider_attempt: &FrozenProviderAttempt,
    trigger: CompactionTrigger,
    history: &mut History,
    cancel: &CancellationToken,
) -> Result<CompactionOutcome> {
    history.ensure_initial_provider_route(&cfg.provider_route)?;
    let messages = history.messages().to_vec();
    let mut request_plan = match plan_compaction(&messages) {
        Ok(plan) => plan,
        Err(reason) => return Ok(CompactionOutcome::NoOp(reason)),
    };
    request_plan
        .request
        .push(Message::user_text(COMPACT_INSTRUCTION));

    // The summary request can itself be too large — that is how a turn used to
    // die outright: sampling overflows, compaction is asked to rescue it, and
    // compaction sends nearly the same history. On a size rejection, drop the
    // oldest slice of what we were going to summarize and try again; the tail is
    // already held verbatim, so the summary should abut it. Losing the oldest
    // context beats losing the turn — provided the loss is announced.
    let mut dropped = 0usize;
    let (summary, usage) = loop {
        let projected_request = history
            .provider_request_view_for(&request_plan.request, provider_attempt)
            .map_err(anyhow::Error::new)?;
        match sample_summary(
            provider_attempt,
            cfg.cache_key(),
            &projected_request,
            cancel,
        )
        .await
        {
            Ok(ok) => break ok,
            Err(error) if is_overflow(&error) && request_plan.request.len() > 2 => {
                // A rejection is the only true reading we get of the real limit;
                // record it so the predictive threshold stops walking into it.
                let refused: u64 = request_plan
                    .request
                    .iter()
                    .map(crate::history::estimate_message_tokens)
                    .sum();
                history.note_overflow_at(refused);
                let just_dropped = shrink_to_newest(&mut request_plan.request, refused / 2);
                if just_dropped == 0 {
                    return Err(error.context("summary request too large to shrink further"));
                }
                dropped += just_dropped;
                request_plan.summarized = request_plan.summarized.saturating_sub(just_dropped);
            }
            Err(error) => return Err(error),
        }
    };
    let summary = canonicalize_summary(&summary)?;
    let pointer = transcript_pointer(cfg);
    let items = build_replacement(&request_plan, &summary, pointer.as_deref(), dropped);
    if items == messages {
        return Ok(CompactionOutcome::NoOp(NoOpReason::ReplacementUnchanged));
    }

    if let Some(usage) = usage {
        history.record_provider_usage(ProviderUsageRecord::from_attempt(
            provider_attempt.identity(),
            UsageOperation::Compaction,
            usage,
        ));
    }
    history.replace_all(items);
    Ok(CompactionOutcome::Applied(CompactionReceipt {
        summarized: request_plan.summarized,
        kept: request_plan.kept,
        dropped,
        model: provider_attempt.model().to_string(),
        trigger,
    }))
}

/// Compatibility wrapper for the original public stats-only API. Production
/// callers use `compact_once` so they can distinguish a normal NoOp.
pub async fn run_compaction(
    cfg: &Arc<Config>,
    model: &str,
    history: &mut History,
    cancel: &CancellationToken,
) -> Result<CompactionStats> {
    history.ensure_initial_provider_route(&cfg.provider_route)?;
    let model = InheritedProviderModelOverride::parse(model).map_err(anyhow::Error::msg)?;
    let provider_route = cfg
        .provider_route
        .child_route(Some(&model))
        .map_err(anyhow::Error::msg)?;
    let provider_attempt = provider_route.primary_attempt();
    match compact_once(
        cfg,
        &provider_attempt,
        CompactionTrigger::Manual,
        history,
        cancel,
    )
    .await?
    {
        CompactionOutcome::Applied(receipt) => Ok(CompactionStats {
            summarized: receipt.summarized,
            kept: receipt.kept,
        }),
        CompactionOutcome::NoOp(NoOpReason::HistoryTooShort) => {
            bail!("history too short to compact")
        }
        CompactionOutcome::NoOp(NoOpReason::PairBoundaryLeavesNothing) => {
            bail!("nothing to compact without splitting the kept tail")
        }
        CompactionOutcome::NoOp(NoOpReason::NoFoldableMessages) => {
            bail!("nothing new to compact")
        }
        CompactionOutcome::NoOp(NoOpReason::ReplacementUnchanged) => {
            bail!("compaction replacement unchanged")
        }
    }
}

/// Whether a failed summary request was rejected for size. The typed failure
/// survives inside the `anyhow` chain; without this the caller cannot tell "too
/// large" from "connection dropped", and shrinking the fold window is the wrong
/// answer to the second.
fn is_overflow(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<kloop_provider::ProviderFailure>()
            .is_some_and(kloop_provider::ProviderFailure::is_context_overflow)
    })
}

/// Trim `request` to roughly `target` estimated tokens by dropping the OLDEST
/// messages: the kept tail already holds the newest turns verbatim, so what is
/// summarized should abut it, and recent context is what the current task runs
/// on. Returns how many were dropped. Never empties the slice — a request of
/// zero messages has nothing to summarize.
fn shrink_to_newest(request: &mut Vec<Message>, target: u64) -> usize {
    let mut keep_from = request.len();
    let mut kept = 0u64;
    while keep_from > 1 {
        let candidate = &request[keep_from - 1];
        let tokens = crate::history::estimate_message_tokens(candidate);
        if kept + tokens > target && keep_from < request.len() {
            break;
        }
        kept += tokens;
        keep_from -= 1;
    }
    // A tool_result must keep the assistant message that issued its tool_use, or
    // the request is illegal on both wire formats — the same rule the tail
    // boundary follows.
    while keep_from > 0 && starts_with_tool_result(&request[keep_from]) {
        keep_from -= 1;
    }
    if keep_from == 0 {
        return 0;
    }
    request.drain(..keep_from);
    keep_from
}

/// One summarization request: no tools, text collected from BlockDone.
async fn sample_summary(
    provider_attempt: &FrozenProviderAttempt,
    cache_key: Option<&str>,
    request: &[Message],
    cancel: &CancellationToken,
) -> Result<(String, Option<Usage>)> {
    let mut rx = provider_attempt.provider().stream_attempt(
        provider_attempt.identity(),
        provider_attempt.effort(),
        cache_key,
        COMPACT_SYSTEM,
        request,
        &[],
    );
    let mut summary = String::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => bail!("compaction interrupted"),
            event = rx.recv() => match event {
                None => bail!("compaction stream closed early"),
                Some(Err(error)) => {
                    return Err(anyhow::Error::new(error).context("compaction request failed"));
                }
                Some(Ok(StreamEvent::TextDelta(_))) => {}
                // The summary is the text; reasoning about it is discarded.
                Some(Ok(StreamEvent::ThinkingDelta(_))) => {}
                Some(Ok(StreamEvent::BlockDone(AssistantBlock::Text { text }))) => {
                    summary.push_str(&text);
                }
                Some(Ok(StreamEvent::BlockDone(_))) => {}
                Some(Ok(StreamEvent::Terminal {
                    outcome: AssistantOutcome::EndTurn,
                    usage,
                })) => return Ok((summary, usage)),
                Some(Ok(StreamEvent::Terminal { outcome, .. })) => {
                    bail!("compaction ended with non-success outcome {outcome:?}")
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kloop_protocol::Role;

    use super::*;
    use serde_json::json;

    fn compact_test_cfg(provider: kloop_provider::Provider, tag: &str) -> Arc<Config> {
        let (provider_catalog, provider_route) =
            crate::provider_route::ProviderCatalog::from_provider(
                "test",
                provider,
                "mock",
                vec![
                    "mock".into(),
                    "actual-model".into(),
                    "fallback-model".into(),
                ],
                None,
            )
            .unwrap();
        let inbox = Arc::new(crate::inbox::Inbox::default());
        Arc::new(Config {
            provider_catalog,
            provider_route,
            system: "test".into(),
            project_instructions: None,
            max_rounds: Some(5),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            offload_dir: std::env::temp_dir().join(format!("kloop-compact-{tag}")),
            sessions_dir: std::env::temp_dir().join(format!("kloop-compact-{tag}-sessions")),
            context_window: Some(200_000),
            permissions: Arc::new(crate::permissions::Permissions::allow_all()),
            questioner: None,
            file_state: Default::default(),
            tool_sources: Vec::new(),
            session_id: String::new(),
            local_agent: crate::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
            hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
            background_shells: crate::tools::BackgroundShells::new(),
            shell_programs: std::sync::Arc::new(
                crate::shell_programs::ShellPrograms::test_fixture(),
            ),
            powershell_execution_gate: Default::default(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            tasks: Default::default(),
            inbox: Arc::clone(&inbox),
            scheduler: crate::scheduler::Scheduler::in_memory(inbox),
            background_executions: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(crate::worktree::ActiveWorktreeState::default()),
            surface: Default::default(),
        })
    }

    fn usage(input_tokens: u64) -> Usage {
        Usage {
            input_tokens,
            output_tokens: 2,
            cache_read_input_tokens: 3,
            cache_creation_input_tokens: 4,
        }
    }

    fn seeded_history(offload_dir: std::path::PathBuf) -> History {
        let mut h = History::new(offload_dir);
        h.record(Message::user_text("old request"));
        h.record(Message::assistant(vec![ContentBlock::Text {
            // Sized off the keep budget so the fixture keeps exceeding it when the
            // constant moves — a literal here silently stops testing compaction.
            text: "old work ".repeat(KEEP_RECENT_TOKENS as usize),
        }]));
        h.record(Message::user_text("current request"));
        h
    }

    /// The scratchpad must not reach the next context — it is drafting, and it is
    /// the largest part of the reply. Both tags are best-effort: a bare summary
    /// still compacts, because discarding real work over a formatting slip is the
    /// worse failure.
    #[test]
    fn analysis_scratchpad_is_dropped_and_summary_unwrapped() {
        let full =
            "<analysis>\nlong drafting notes\n</analysis>\n<summary>\nthe real summary\n</summary>";
        assert_eq!(canonicalize_summary(full).unwrap(), "the real summary");

        // Bare text, no tags at all.
        assert_eq!(
            canonicalize_summary("just a summary").unwrap(),
            "just a summary"
        );
        // Analysis only, unclosed summary tag: keep what is there rather than fail.
        assert_eq!(
            canonicalize_summary("<analysis>notes</analysis>\ntail text").unwrap(),
            "tail text"
        );
        // An echoed marker is still stripped after unwrapping.
        let echoed = format!("<summary>{SUMMARY_PREFIX}already prefixed</summary>");
        assert_eq!(canonicalize_summary(&echoed).unwrap(), "already prefixed");
        // Nothing but a scratchpad is an empty summary, not a silent success.
        assert!(canonicalize_summary("<analysis>only notes</analysis>").is_err());
    }

    /// "Do not re-derive" is only actionable with somewhere to look; an in-memory
    /// history has no file, and must not get a pointer to one.
    #[test]
    fn transcript_pointer_is_present_only_for_a_persisted_session() {
        let provider = kloop_provider::Provider::mock(Vec::new());
        let mut cfg = compact_test_cfg(provider, "pointer").test_clone();
        cfg.session_id = "20260901-120000".into();
        let pointer = transcript_pointer(&cfg).expect("a persisted session has a transcript");
        assert!(pointer.contains("20260901-120000.jsonl"), "{pointer}");
        assert!(pointer.contains("read the full transcript at"), "{pointer}");

        cfg.session_id = String::new();
        assert!(transcript_pointer(&cfg).is_none());

        let plan = CompactionPlan {
            request: vec![Message::user_text("old")],
            tail: vec![Message::user_text("recent")],
            summarized: 1,
            kept: 1,
        };
        let with = build_replacement(&plan, "S", Some("\n\nPOINTER"), /*dropped*/ 0);
        let ContentBlock::Text { text } = &with[0].content[0] else {
            panic!("summary is text")
        };
        assert!(text.ends_with("POINTER"), "{text}");
        let without = build_replacement(&plan, "S", None, /*dropped*/ 0);
        let ContentBlock::Text { text } = &without[0].content[0] else {
            panic!("summary is text")
        };
        assert_eq!(text, &format!("{SUMMARY_PREFIX}S"));
    }

    /// The prompt's load-bearing clauses, asserted by intent rather than wording
    /// so a rewrite stays free but a deletion is caught. Each exists because its
    /// absence produced a real failure; see the constant's doc comment.
    #[test]
    fn compaction_prompt_keeps_its_load_bearing_clauses() {
        assert!(COMPACT_SYSTEM.contains("Do not call any tool"));
        assert!(COMPACT_SYSTEM.contains("Never invent a fact"));
        for clause in [
            "Only user-role turns count",
            "never record it as a user request, approval, or confirmation",
            "This summary request is not one of them either",
            "are not constraints on the session being summarized",
            "copied verbatim",
            "Sub-agent results",
            "must not be re-derived",
            "direct quote",
            // Plan 128: "the single best next action" was doing exactly what it
            // said — one action per round, for the 45 rounds after a compaction.
            "name them all, not just the first",
            "<analysis>",
            "<summary>",
            // Plan 119: a suspicion that is noticed but not yet judged fits in
            // none of the other sections, so compaction used to drop it.
            "Open candidates",
            // Plan 128: keeping only the actions taken left the next agent
            // unable to tell what it already knew, so it stopped batching and
            // re-probed one call at a time (3.6 tool calls per round before a
            // mid-task compaction, 1.5 after).
            "Established facts",
            "without opening the file again",
        ] {
            assert!(
                COMPACT_INSTRUCTION.contains(clause),
                "compaction instruction lost: {clause}"
            );
        }
    }

    /// The dead end this path exists for: sampling overflows, compaction is asked
    /// to rescue it, and the summary request is itself too large. Before, that
    /// ended the turn. Now the oldest slice is abandoned, the loss is announced
    /// in history, and the session continues.
    #[tokio::test]
    async fn oversized_summary_request_drops_oldest_and_still_compacts() {
        let provider = kloop_provider::Provider::mock_scripted(vec![
            kloop_provider::MockTurn::Overflow,
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "summary of what fit".into(),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "shrink");
        let mut history = History::new(cfg.offload_dir.clone());
        for i in 0..8 {
            history.record(Message::user_text(format!("turn {i} ").repeat(2_000)));
        }
        history.record(Message::user_text("current request"));

        let outcome = compact_once(
            &cfg,
            &cfg.provider_route.primary_attempt(),
            CompactionTrigger::Reactive,
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .expect("shrinking must rescue the turn, not end it");

        let CompactionOutcome::Applied(receipt) = outcome else {
            panic!("expected an applied compaction")
        };
        assert!(
            receipt.dropped > 0,
            "the first attempt was refused for size"
        );
        assert!(
            describe(&receipt).contains("dropped unsummarized"),
            "the loss must be announced"
        );

        // The marker leads the rebuilt history, so the next round can see that
        // something is missing rather than reading absence as "never happened".
        let ContentBlock::Text { text } = &history.messages()[0].content[0] else {
            panic!("first message is text")
        };
        assert!(text.starts_with(DROPPED_PREFIX), "{text}");

        // The refusal is remembered: planning now uses the observed ceiling.
        assert!(history.effective_window(u64::MAX) < u64::MAX);
    }

    /// A non-overflow failure must not be answered by shrinking — the fold window
    /// has nothing to do with a dropped connection.
    #[tokio::test]
    async fn a_transport_failure_is_not_treated_as_too_large() {
        let provider = kloop_provider::Provider::mock_scripted(vec![
            kloop_provider::MockTurn::Failure(kloop_provider::ProviderFailure::transport(
                "connection reset",
            )),
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "must not be reached".into(),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "not-overflow");
        let mut history = seeded_history(cfg.offload_dir.clone());

        let error = compact_once(
            &cfg,
            &cfg.provider_route.primary_attempt(),
            CompactionTrigger::Reactive,
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .expect_err("a transport failure must surface, not shrink");
        assert!(
            format!("{error:#}").contains("connection reset"),
            "{error:#}"
        );
        assert_eq!(
            history.effective_window(1_000),
            1_000,
            "no ceiling recorded"
        );
    }

    /// A rejection is evidence about the real limit and nothing else: it lowers
    /// the planning window and never raises it.
    #[test]
    fn observed_ceiling_only_lowers_the_window() {
        let mut history = History::new(std::env::temp_dir().join("kloop-ceiling"));
        assert_eq!(history.effective_window(200_000), 200_000);
        history.note_overflow_at(150_000);
        assert_eq!(history.effective_window(200_000), 150_000);
        // A larger later refusal tells us nothing new; keep the tighter bound.
        history.note_overflow_at(180_000);
        assert_eq!(history.effective_window(200_000), 150_000);
        // And a configured window below the observation still wins.
        assert_eq!(history.effective_window(100_000), 100_000);
    }

    #[tokio::test]
    async fn run_compaction_rebuilds_history_around_summary() {
        let provider = kloop_provider::Provider::mock(vec![vec![AssistantBlock::Text {
            text: "what happened so far".into(),
        }]]);
        let cfg = compact_test_cfg(provider, "rebuild");
        let mut history = seeded_history(cfg.offload_dir.clone());

        let stats = run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .expect("compaction should succeed");
        // [user, assistant(fat), user] → summarize the first two, keep the last.
        assert_eq!(
            stats,
            CompactionStats {
                summarized: 2,
                kept: 1
            }
        );

        let msgs = history.messages();
        // [summary, ...kept tail] — the fat prefix is summarized away and the
        // kept tail survives verbatim.
        assert_eq!(
            msgs[0],
            Message::injected(
                Injected::ContextSummary,
                format!("{SUMMARY_PREFIX}what happened so far"),
            )
        );
        assert_eq!(msgs.last().unwrap(), &Message::user_text("current request"));
        assert!(msgs.len() < 4);
    }

    /// Compaction reuses the session's cache key rather than minting its own:
    /// it is one more request against the same conversation, and a separate key
    /// would send it to a backend holding none of the prefix.
    #[tokio::test]
    async fn compaction_reuses_the_session_cache_key() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "summary".into(),
            }]),
        ]);
        let mut cfg = compact_test_cfg(provider, "compact-cache-key").test_clone();
        cfg.session_id = "sess-xyz".into();
        let cfg = Arc::new(cfg);
        let mut history = seeded_history(cfg.offload_dir.clone());

        run_compaction(&cfg, "mock", &mut history, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            seen.lock().unwrap()[0].cache_key.as_deref(),
            Some("sess-xyz")
        );
    }

    #[tokio::test]
    async fn compaction_keeps_reasoning_provenance_on_the_verbatim_tail() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "summary".into(),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "reasoning-tail");
        let mut history = seeded_history(cfg.offload_dir.clone());
        let reasoning_content = vec![ContentBlock::Thinking {
            thinking: "display summary".into(),
            signature: "opaque".into(),
        }];
        history.record_provider_assistant(reasoning_content, &cfg.provider_route.primary_attempt());
        let reasoning = history.messages().last().unwrap().clone();

        run_compaction(&cfg, "mock", &mut history, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(history.messages().last(), Some(&reasoning));
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0].messages.last(),
            Some(&Message::user_text(COMPACT_INSTRUCTION))
        );
        assert!(
            seen[0].messages.iter().all(|message| message != &reasoning),
            "the verbatim tail must not be folded into the summary request"
        );
    }

    #[tokio::test]
    async fn accepted_summary_records_usage_before_compacted_marker() {
        let dir = std::env::temp_dir().join(format!("kloop-compact-usage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let session = dir.join("session.jsonl");
        let provider =
            kloop_provider::Provider::mock_scripted(vec![kloop_provider::MockTurn::Response {
                blocks: vec![AssistantBlock::Text {
                    text: "summary".into(),
                }],
                outcome: AssistantOutcome::EndTurn,
                usage: usage(10),
            }]);
        let cfg = compact_test_cfg(provider, "usage");
        let mut history = seeded_history(cfg.offload_dir.clone());
        history.attach_rollout(crate::rollout::Rollout::new(session.clone()));

        run_compaction(&cfg, "mock", &mut history, &CancellationToken::new())
            .await
            .unwrap();

        assert_eq!(
            history.provider_usage().records(),
            &[ProviderUsageRecord::from_attempt(
                cfg.provider_route.primary_attempt().identity(),
                UsageOperation::Compaction,
                usage(10),
            )]
        );
        let lines: Vec<serde_json::Value> = std::fs::read_to_string(&session)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            lines.iter().map(|line| &line["type"]).collect::<Vec<_>>(),
            vec!["provider_route_initial", "provider_usage", "compacted"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn compaction_without_usage_still_succeeds_without_a_record() {
        let provider = kloop_provider::Provider::mock(vec![vec![AssistantBlock::Text {
            text: "summary".into(),
        }]]);
        let cfg = compact_test_cfg(provider, "no-usage");
        let mut history = seeded_history(cfg.offload_dir.clone());

        run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert!(history.provider_usage().records().is_empty());
        assert_eq!(
            history.messages()[0].content[0],
            ContentBlock::Text {
                text: format!("{SUMMARY_PREFIX}summary"),
            }
        );
    }

    #[tokio::test]
    async fn rejected_summary_usage_is_not_recorded() {
        let cases = [
            kloop_provider::MockTurn::Response {
                blocks: vec![AssistantBlock::Text { text: "   ".into() }],
                outcome: AssistantOutcome::EndTurn,
                usage: usage(10),
            },
            kloop_provider::MockTurn::Response {
                blocks: vec![AssistantBlock::Text {
                    text: "not accepted".into(),
                }],
                outcome: AssistantOutcome::Refused,
                usage: usage(20),
            },
        ];
        for (index, turn) in cases.into_iter().enumerate() {
            let provider = kloop_provider::Provider::mock_scripted(vec![turn]);
            let cfg = compact_test_cfg(provider, &format!("rejected-usage-{index}"));
            let mut history = seeded_history(cfg.offload_dir.clone());
            let before = history.messages().to_vec();

            assert!(
                run_compaction(
                    &cfg,
                    cfg.provider_route.primary_model(),
                    &mut history,
                    &CancellationToken::new()
                )
                .await
                .is_err()
            );
            assert_eq!(history.messages(), before);
            assert!(history.provider_usage().records().is_empty());
        }
    }

    /// Compaction samples on the model it is handed, not `cfg.model` — so a
    /// turn that already fell back off a broken primary compacts on the
    /// fallback instead of failing on the dead primary.
    #[tokio::test]
    async fn compaction_samples_on_the_given_model() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "summary".into(),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "model-arg");
        assert_eq!(cfg.provider_route.primary_model(), "mock");
        let mut history = seeded_history(cfg.offload_dir.clone());

        run_compaction(
            &cfg,
            "fallback-model",
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        let seen = seen.lock().unwrap();
        assert_eq!(
            seen[0].model, "fallback-model",
            "compaction must use the passed model, not cfg.model"
        );
    }

    /// The session effort reaches the wire through the frozen route: compaction
    /// (like every child sampler) mints its attempt from `cfg.provider_route`,
    /// so `/effort` governs it without being threaded through separately.
    #[tokio::test]
    async fn compaction_samples_at_the_session_effort() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "summary".into(),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "effort");
        let state = crate::provider_route::SessionProviderState::from_route(
            Arc::clone(&cfg.provider_catalog),
            cfg.provider_route.clone(),
        );
        state.set_effort(Some(kloop_protocol::ReasoningEffort::XHigh));
        let cfg = Arc::new(cfg.clone_with_provider_route(state.freeze()));
        let mut history = seeded_history(cfg.offload_dir.clone());

        run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            seen.lock().unwrap()[0].effort,
            Some(kloop_protocol::ReasoningEffort::XHigh)
        );
    }

    #[tokio::test]
    async fn failed_compaction_leaves_history_untouched() {
        let provider =
            kloop_provider::Provider::mock_scripted(vec![kloop_provider::MockTurn::Error(
                "summarizer unavailable".into(),
            )]);
        let cfg = compact_test_cfg(provider, "fail");
        let mut history = seeded_history(cfg.offload_dir.clone());
        let before = history.messages().to_vec();

        let result = run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(history.messages(), &before[..], "history must be untouched");
    }

    #[tokio::test]
    async fn empty_summary_is_rejected() {
        let provider =
            kloop_provider::Provider::mock(vec![vec![AssistantBlock::Text { text: "   ".into() }]]);
        let cfg = compact_test_cfg(provider, "empty");
        let mut history = seeded_history(cfg.offload_dir.clone());
        let before = history.messages().to_vec();

        let result = run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(history.messages(), &before[..]);
    }

    #[test]
    fn keep_boundary_walks_back_before_oversized_tool_pair() {
        let tool_use = Message::assistant(vec![
            ContentBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: json!({"command": "x".repeat(10_000)}),
            },
            ContentBlock::ToolUse {
                id: "t2".into(),
                name: "read_file".into(),
                input: json!({"path": "small.txt"}),
            },
        ]);
        let tool_result = Message::tool_results(vec![
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "first result".into(),
                is_error: false,
            },
            ContentBlock::ToolResult {
                tool_use_id: "t2".into(),
                content: "second result".into(),
                is_error: false,
            },
        ]);
        let messages = vec![
            Message::user_text("old request"),
            tool_use,
            tool_result,
            Message::assistant(vec![ContentBlock::Text {
                text: "recent tail".into(),
            }]),
        ];

        // The result and recent tail fit the keep budget, but the preceding
        // assistant tool-use does not. The boundary must therefore move before
        // the whole assistant/result pair rather than retain an orphan result.
        assert_eq!(keep_from_index(&messages), 1);
    }

    #[tokio::test]
    async fn compaction_retains_complete_tool_pair_at_boundary() {
        let provider = kloop_provider::Provider::mock(vec![vec![AssistantBlock::Text {
            text: "summary".into(),
        }]]);
        let cfg = compact_test_cfg(provider, "tool-boundary");
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("old request"));
        history.record(Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({"command": "x".repeat(10_000)}),
        }]));
        history.record(Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
        }]));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "recent tail".into(),
        }]));

        let stats = run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .expect("compaction should preserve the tool pair");
        assert_eq!(stats.summarized, 1);
        assert_eq!(stats.kept, 3);
        assert!(matches!(
            history.messages(),
            [
                Message { role: Role::User, content, .. },
                Message { role: Role::Assistant, content: tool_use, .. },
                Message { role: Role::User, content: tool_result, .. },
                Message { role: Role::Assistant, content: tail, .. },
            ] if content.iter().any(|block| matches!(block, ContentBlock::Text { text } if text.starts_with(SUMMARY_PREFIX)))
                && tool_use.iter().any(|block| matches!(block, ContentBlock::ToolUse { id, .. } if id == "t1"))
                && tool_result.iter().any(|block| matches!(block, ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "t1"))
                && tail.iter().any(|block| matches!(block, ContentBlock::Text { text } if text == "recent tail"))
        ));
    }
    #[tokio::test]
    async fn too_short_history_is_not_compacted() {
        let provider = kloop_provider::Provider::mock(vec![]);
        let cfg = compact_test_cfg(provider, "short");
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("only message"));

        let result = run_compaction(
            &cfg,
            cfg.provider_route.primary_model(),
            &mut history,
            &CancellationToken::new(),
        )
        .await;

        assert!(result.is_err());
        assert_eq!(history.messages().len(), 1);
    }

    #[tokio::test]
    async fn compact_once_returns_noop_for_short_history_before_provider_io() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(Vec::new());
        let cfg = compact_test_cfg(provider, "short-noop");
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("only message"));
        let before = history.messages().to_vec();

        let result = compact_once(
            &cfg,
            &cfg.provider_route.primary_attempt(),
            CompactionTrigger::Manual,
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(result, CompactionOutcome::NoOp(NoOpReason::HistoryTooShort));
        assert_eq!(history.messages(), before.as_slice());
        assert!(history.provider_usage().records().is_empty());
        assert!(seen.lock().unwrap().is_empty());
    }

    #[test]
    fn canonicalize_summary_trims_and_keeps_one_prefix() {
        assert_eq!(
            canonicalize_summary("  summary text  ").unwrap(),
            "summary text"
        );
        assert_eq!(
            canonicalize_summary(&format!("  {SUMMARY_PREFIX}{SUMMARY_PREFIX}summary text  "))
                .unwrap(),
            "summary text"
        );
        assert!(canonicalize_summary(&format!(" {SUMMARY_PREFIX} ")).is_err());
    }

    #[test]
    fn plan_skips_existing_summary_but_allows_new_foldable_messages() {
        let summary = Message::injected(
            Injected::ContextSummary,
            format!("{SUMMARY_PREFIX}old summary"),
        );
        let current = Message::user_text("current request");
        assert_eq!(
            plan_compaction(&[summary.clone(), current.clone()]),
            Err(NoOpReason::NoFoldableMessages)
        );

        let old_work = Message::assistant(vec![ContentBlock::Text {
            // Must outweigh the keep budget or there is nothing to fold.
            text: "old work ".repeat(KEEP_RECENT_TOKENS as usize),
        }]);
        let plan = plan_compaction(&[summary.clone(), old_work.clone(), current.clone()])
            .expect("new work after a summary should be foldable");
        assert_eq!(plan.summarized, 1);
        assert_eq!(plan.kept, 1);
        assert_eq!(plan.request, vec![summary, old_work]);
        assert_eq!(plan.tail, vec![current]);
    }

    #[tokio::test]
    async fn existing_summary_is_replaced_without_prefix_stacking() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: format!("  {SUMMARY_PREFIX}{SUMMARY_PREFIX}new summary  "),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "existing-summary");
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::injected(
            Injected::ContextSummary,
            format!("{SUMMARY_PREFIX}old summary"),
        ));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "old work ".repeat(KEEP_RECENT_TOKENS as usize),
        }]));
        history.record(Message::user_text("current request"));

        let fallback_model = InheritedProviderModelOverride::parse("fallback-model").unwrap();
        let fallback_route = cfg
            .provider_route
            .child_route(Some(&fallback_model))
            .unwrap();
        let result = compact_once(
            &cfg,
            &fallback_route.primary_attempt(),
            CompactionTrigger::Predictive,
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            CompactionOutcome::Applied(CompactionReceipt {
                summarized: 1,
                kept: 1,
                dropped: 0,
                model: "fallback-model".into(),
                trigger: CompactionTrigger::Predictive,
            })
        );
        // The replacement carries the identity, so the next compaction finds it
        // without reading the marker text.
        assert_eq!(
            history.messages()[0],
            Message::injected(
                Injected::ContextSummary,
                format!("{SUMMARY_PREFIX}new summary")
            )
        );
        assert_eq!(
            history.messages().last(),
            Some(&Message::user_text("current request"))
        );

        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].model, "fallback-model");
        assert_eq!(seen[0].system, COMPACT_SYSTEM);
        assert!(seen[0].tools.is_empty());
        assert_eq!(
            seen[0].messages.last().unwrap(),
            &Message::user_text(COMPACT_INSTRUCTION)
        );
    }

    #[tokio::test]
    async fn repeated_compaction_is_a_zero_mutation_noop() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "canonical summary".into(),
            }]),
        ]);
        let cfg = compact_test_cfg(provider, "duplicate");
        let mut history = seeded_history(cfg.offload_dir.clone());

        let first = compact_once(
            &cfg,
            &cfg.provider_route.primary_attempt(),
            CompactionTrigger::Manual,
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(matches!(first, CompactionOutcome::Applied(_)));
        let after_first = history.messages().to_vec();
        let usage_after_first = history.provider_usage().records().to_vec();

        let second = compact_once(
            &cfg,
            &cfg.provider_route.primary_attempt(),
            CompactionTrigger::Manual,
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(
            second,
            CompactionOutcome::NoOp(NoOpReason::NoFoldableMessages)
        );
        assert_eq!(history.messages(), after_first.as_slice());
        assert_eq!(
            history.provider_usage().records(),
            usage_after_first.as_slice()
        );
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn growth_is_bounded_output_plus_tool_spike() {
        assert_eq!(max_turn_growth(8_192), 8_192 + TOOL_RESULT_GROWTH_ESTIMATE);
        assert_eq!(
            max_turn_growth(128_000),
            OUTPUT_GROWTH_CAP + TOOL_RESULT_GROWTH_ESTIMATE
        );
    }

    #[test]
    fn predicted_overflow_boundary_and_small_window_guard() {
        let growth = max_turn_growth(8_192); // 23_192
        // At and above the line.
        assert!(predicted_overflow(100_000 - growth, growth, 100_000));
        // One under the line.
        assert!(!predicted_overflow(100_000 - growth - 1, growth, 100_000));
        // A window at or below the growth reserve never predicts overflow —
        // the lesson from the codex dry run: a negative threshold means
        // "always compact", which is wrong.
        assert!(!predicted_overflow(u64::MAX / 2, growth, growth));
        assert!(!predicted_overflow(1_900, growth, 2_000));
    }

    #[test]
    fn keep_boundary_never_splits_a_tool_pair() {
        let tool_use = Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        }]);
        let tool_result = Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "ok".into(),
            is_error: false,
        }]);
        // [user, assistant(tool_use), user(tool_result), assistant(text)]
        let messages = vec![
            Message::user_text("start"),
            tool_use,
            tool_result,
            Message::assistant(vec![ContentBlock::Text {
                text: "done".into(),
            }]),
        ];
        let keep_from = keep_from_index(&messages);
        // Small messages all fit the keep budget except we must summarize at
        // least one; whatever the budget decides, the boundary must not land
        // on the tool_result (index 2) with its tool_use (index 1) dropped.
        assert_ne!(keep_from, 2, "boundary would orphan the tool_result");
        assert!(keep_from >= 1);
    }
}
