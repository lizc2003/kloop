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
use kloop_protocol::Message;
use kloop_protocol::Role;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;

/// Cap on how much of the output limit the growth estimate reserves.
const OUTPUT_GROWTH_CAP: u64 = 20_000;
/// Allowance for tool results recorded within one round.
const TOOL_RESULT_GROWTH_ESTIMATE: u64 = 15_000;
/// Budget (in estimated tokens) of recent messages kept verbatim through a
/// compaction; everything older is replaced by the summary.
const KEEP_RECENT_TOKENS: u64 = 2_000;

pub const SUMMARY_PREFIX: &str =
    "[Context summary of the earlier part of this session — earlier messages were compacted]\n";

const COMPACT_SYSTEM: &str = "You summarize an in-progress coding-agent session so it can \
continue seamlessly in a fresh context window. Be precise and concrete; prefer exact file \
paths, commands, code identifiers, and error messages over prose.";

const COMPACT_INSTRUCTION: &str = "Summarize the conversation above for a context handoff. \
Cover, in order: 1. the user's request and current objective; 2. all user messages in brief \
(intent changes matter); 3. work completed so far (files touched, commands run, results); \
4. key decisions and why; 5. errors hit and how they were fixed; 6. work in progress and the \
exact next step. Reply with the summary only.";

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
    pub model: String,
    pub trigger: CompactionTrigger,
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
    matches!(
        message,
        Message {
            role: Role::User,
            content,
            ..
        } if content.len() == 1
            && matches!(&content[0], ContentBlock::Text { text } if text.starts_with(SUMMARY_PREFIX))
    )
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

fn canonicalize_summary(raw: &str) -> Result<String> {
    let mut summary = raw.trim();
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

fn build_replacement(plan: &CompactionPlan, summary: &str) -> Vec<Message> {
    let mut items = Vec::with_capacity(plan.tail.len() + 1);
    items.push(Message::user_text(format!("{SUMMARY_PREFIX}{summary}")));
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

    let projected_request = history
        .provider_request_view_for(&request_plan.request, provider_attempt)
        .map_err(anyhow::Error::new)?;
    let (summary, usage) = sample_summary(provider_attempt, &projected_request, cancel).await?;
    let summary = canonicalize_summary(&summary)?;
    let items = build_replacement(&request_plan, &summary);
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

/// One summarization request: no tools, text collected from BlockDone.
async fn sample_summary(
    provider_attempt: &FrozenProviderAttempt,
    request: &[Message],
    cancel: &CancellationToken,
) -> Result<(String, Option<Usage>)> {
    let mut rx = provider_attempt.provider().stream_attempt(
        provider_attempt.identity(),
        provider_attempt.effort(),
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
            text: "old work ".repeat(1_500), // ~3.4k tokens: exceeds keep budget
        }]));
        h.record(Message::user_text("current request"));
        h
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
            Message::user_text(format!("{SUMMARY_PREFIX}what happened so far"))
        );
        assert_eq!(msgs.last().unwrap(), &Message::user_text("current request"));
        assert!(msgs.len() < 4);
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
                Message { role: kloop_protocol::Role::User, content, .. },
                Message { role: kloop_protocol::Role::Assistant, content: tool_use, .. },
                Message { role: kloop_protocol::Role::User, content: tool_result, .. },
                Message { role: kloop_protocol::Role::Assistant, content: tail, .. },
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
        let summary = Message::user_text(format!("{SUMMARY_PREFIX}old summary"));
        let current = Message::user_text("current request");
        assert_eq!(
            plan_compaction(&[summary.clone(), current.clone()]),
            Err(NoOpReason::NoFoldableMessages)
        );

        let old_work = Message::assistant(vec![ContentBlock::Text {
            text: "old work ".repeat(1_500),
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
        history.record(Message::user_text(format!("{SUMMARY_PREFIX}old summary")));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "old work ".repeat(1_500),
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
                model: "fallback-model".into(),
                trigger: CompactionTrigger::Predictive,
            })
        );
        assert_eq!(
            history.messages()[0],
            Message::user_text(format!("{SUMMARY_PREFIX}new summary"))
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
