use std::sync::Arc;

use anyhow::bail;
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use crate::agent::Ui;
use crate::config::Config;
use crate::history::estimate_message_tokens;
use crate::history::History;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::StreamEvent;

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

/// Replace everything before the keep-boundary with a model-written summary.
/// Fails without touching the history if the summary request fails.
pub async fn run_compaction(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
) -> Result<()> {
    let messages = history.messages();
    if messages.len() < 2 {
        bail!("history too short to compact");
    }
    let keep_from = keep_from_index(messages);
    if keep_from == 0 {
        bail!("nothing to compact without splitting the kept tail");
    }

    let mut request = messages[..keep_from].to_vec();
    request.push(Message::user_text(COMPACT_INSTRUCTION));
    let summary = sample_summary(cfg, &request, cancel).await?;
    if summary.trim().is_empty() {
        bail!("compaction model returned an empty summary");
    }

    let mut items = vec![Message::user_text(format!("{SUMMARY_PREFIX}{summary}"))];
    items.extend_from_slice(&messages[keep_from..]);
    let summarized = keep_from;
    let kept = items.len() - 1;
    history.replace_all(items);
    ui.note(&format!(
        "history compacted: {summarized} message(s) summarized, {kept} kept verbatim"
    ));
    Ok(())
}

/// One summarization request: no tools, text collected from BlockDone.
async fn sample_summary(
    cfg: &Arc<Config>,
    request: &[Message],
    cancel: &CancellationToken,
) -> Result<String> {
    let mut rx = cfg
        .provider
        .stream(&cfg.model, COMPACT_SYSTEM, request, &[]);
    let mut summary = String::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => bail!("compaction interrupted"),
            event = rx.recv() => match event {
                None => bail!("compaction stream closed early"),
                Some(Err(e)) => return Err(e.context("compaction request failed")),
                Some(Ok(StreamEvent::TextDelta(_))) => {}
                // The summary is the text; reasoning about it is discarded.
                Some(Ok(StreamEvent::ThinkingDelta(_))) => {}
                Some(Ok(StreamEvent::BlockDone(ContentBlock::Text { text }))) => {
                    summary.push_str(&text);
                }
                Some(Ok(StreamEvent::BlockDone(_))) => {}
                Some(Ok(StreamEvent::Done { .. })) => return Ok(summary),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct NullUi;
    impl Ui for NullUi {
        fn text_delta(&self, _: &str) {}
        fn note(&self, _: &str) {}
    }

    fn compact_test_cfg(provider: kloop_provider::Provider, tag: &str) -> Arc<Config> {
        Arc::new(Config {
            provider: Arc::new(provider),
            model: "mock".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: 5,
            offload_dir: std::env::temp_dir().join(format!("kloop-compact-{tag}")),
            context_window: Some(200_000),
            fallback_model: None,
            permissions: Arc::new(crate::permissions::Permissions::allow_all()),
            tool_sources: Vec::new(),
            session_id: String::new(),
            agent_label: String::new(),
            hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
            background_shells: crate::tools::BackgroundShells::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
        })
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
        let provider = kloop_provider::Provider::mock(vec![vec![ContentBlock::Text {
            text: "what happened so far".into(),
        }]]);
        let cfg = compact_test_cfg(provider, "rebuild");
        let mut history = seeded_history(cfg.offload_dir.clone());
        let ui: Arc<dyn Ui> = Arc::new(NullUi);

        run_compaction(&cfg, &mut history, &ui, &CancellationToken::new())
            .await
            .expect("compaction should succeed");

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
    async fn failed_compaction_leaves_history_untouched() {
        let provider =
            kloop_provider::Provider::mock_scripted(vec![kloop_provider::MockTurn::Error(
                "summarizer unavailable".into(),
            )]);
        let cfg = compact_test_cfg(provider, "fail");
        let mut history = seeded_history(cfg.offload_dir.clone());
        let before = history.messages().to_vec();
        let ui: Arc<dyn Ui> = Arc::new(NullUi);

        let result = run_compaction(&cfg, &mut history, &ui, &CancellationToken::new()).await;

        assert!(result.is_err());
        assert_eq!(history.messages(), &before[..], "history must be untouched");
    }

    #[tokio::test]
    async fn empty_summary_is_rejected() {
        let provider =
            kloop_provider::Provider::mock(vec![vec![ContentBlock::Text { text: "   ".into() }]]);
        let cfg = compact_test_cfg(provider, "empty");
        let mut history = seeded_history(cfg.offload_dir.clone());
        let before = history.messages().to_vec();
        let ui: Arc<dyn Ui> = Arc::new(NullUi);

        let result = run_compaction(&cfg, &mut history, &ui, &CancellationToken::new()).await;

        assert!(result.is_err());
        assert_eq!(history.messages(), &before[..]);
    }

    #[tokio::test]
    async fn too_short_history_is_not_compacted() {
        let provider = kloop_provider::Provider::mock(vec![]);
        let cfg = compact_test_cfg(provider, "short");
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("only message"));
        let ui: Arc<dyn Ui> = Arc::new(NullUi);

        let result = run_compaction(&cfg, &mut history, &ui, &CancellationToken::new()).await;

        assert!(result.is_err());
        assert_eq!(history.messages().len(), 1);
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
