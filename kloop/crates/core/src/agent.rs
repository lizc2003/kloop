use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::config::Config;
use crate::history::History;
use crate::tools::dispatch_tools;
use crate::tools::tool_defs;
use crate::tools::ToolCtx;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;
use kloop_protocol::MAX_OUTPUT_TOKENS;

pub trait Ui: Send + Sync {
    fn text_delta(&self, s: &str);
    fn note(&self, s: &str);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    MaxRounds,
    Aborted,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub reason: EndReason,
    pub final_text: String,
    pub rounds: usize,
}

/// The agent loop. One "round" = one sampling request plus the tool calls it
/// asked for. Continuation is decided ONLY by the presence of tool_use blocks
/// in the sampled response — never by stop_reason.
pub async fn run_turn(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
) -> TurnOutcome {
    let tools = tool_defs(depth);
    // A sub-agent's text is its deliverable and returns via the tool result;
    // streaming it to the main UI would interleave with the parent's output.
    let stream_text = depth == 0;
    let growth = compact::max_turn_growth(MAX_OUTPUT_TOKENS);
    // Overflow is recovered at most once per turn: compact, then retry. A
    // second overflow after a successful compaction surfaces as an error.
    let mut overflow_compact_attempted = false;
    // The model can be swapped once per turn: after retries are exhausted on
    // the primary, the rest of the turn runs on the fallback.
    let mut active_model = cfg.model.clone();
    let mut truncation_recoveries = 0u32;
    for round in 0..cfg.max_rounds {
        // Predictive: compact BEFORE sampling when this round's estimated
        // growth would overflow the window — don't wait to be rejected.
        if let Some(window) = cfg.context_window {
            if history.messages().len() >= 2
                && compact::predicted_overflow(history.estimated_tokens(), growth, window)
            {
                ui.note("predicted context overflow; compacting history");
                if let Err(e) = compact::run_compaction(cfg, history, ui, cancel).await {
                    if cancel.is_cancelled() {
                        return TurnOutcome {
                            reason: EndReason::Aborted,
                            final_text: String::new(),
                            rounds: round,
                        };
                    }
                    // Predictive failure is not fatal: fall through and let
                    // the request itself succeed or overflow reactively.
                    ui.note(&format!("predictive compaction failed: {e:#}"));
                }
            }
        }

        let SampleOk {
            blocks,
            usage,
            stop_reason,
        } = match sample_with_retry(
            cfg,
            &active_model,
            history.messages(),
            &tools,
            ui,
            cancel,
            stream_text,
        )
        .await
        {
            Sampled::Ok(ok) => ok,
            Sampled::Overflow => {
                if cfg.context_window.is_none() || overflow_compact_attempted {
                    return TurnOutcome {
                        reason: EndReason::Error(
                            "context window exceeded (compaction unavailable or already tried)"
                                .into(),
                        ),
                        final_text: String::new(),
                        rounds: round,
                    };
                }
                overflow_compact_attempted = true;
                ui.note("context window exceeded; compacting and retrying");
                match compact::run_compaction(cfg, history, ui, cancel).await {
                    Ok(()) => continue,
                    Err(e) => {
                        return TurnOutcome {
                            reason: if cancel.is_cancelled() {
                                EndReason::Aborted
                            } else {
                                EndReason::Error(format!("reactive compaction failed: {e:#}"))
                            },
                            final_text: String::new(),
                            rounds: round,
                        }
                    }
                }
            }
            Sampled::Cancelled => {
                return TurnOutcome {
                    reason: EndReason::Aborted,
                    final_text: String::new(),
                    rounds: round,
                }
            }
            Sampled::Failed(e) => {
                // Retries exhausted on the primary model: switch to the
                // fallback (once) instead of surfacing the error.
                if let Some(fallback) = &cfg.fallback_model {
                    if *fallback != active_model {
                        ui.note(&format!(
                            "sampling failed on {active_model}; switching to fallback model {fallback}: {e}"
                        ));
                        active_model = fallback.clone();
                        continue;
                    }
                }
                return TurnOutcome {
                    reason: EndReason::Error(e),
                    final_text: String::new(),
                    rounds: round,
                };
            }
        };
        history.record(Message::assistant(blocks.clone()));
        if let Some(usage) = usage {
            // input + output = full context size at this request; anchors the
            // char-heuristic estimate for items recorded after this point.
            history.note_usage(usage.total());
        }

        let tool_uses: Vec<(String, String, Value)> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();
        if tool_uses.is_empty() {
            // The turn would end here — but if the response was cut off by
            // the output limit, ending would strand it mid-thought. Nudge the
            // model to continue, a bounded number of times per turn. (A
            // truncated response WITH tool calls needs no special handling:
            // the loop continues naturally and the model resumes itself.)
            if is_truncated(stop_reason.as_deref())
                && truncation_recoveries < TRUNCATION_RECOVERY_LIMIT
            {
                truncation_recoveries += 1;
                ui.note(&format!(
                    "response truncated by output limit; asking the model to continue ({truncation_recoveries}/{TRUNCATION_RECOVERY_LIMIT})"
                ));
                history.record(Message::user_text(TRUNCATION_CONTINUE_MSG));
                continue;
            }
            let final_text = blocks
                .iter()
                .rev()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
            return TurnOutcome {
                reason: EndReason::Completed,
                final_text,
                rounds: round + 1,
            };
        }

        let ctx = ToolCtx {
            cfg: cfg.clone(),
            ui: ui.clone(),
            cancel: cancel.clone(),
            depth,
        };
        let results = dispatch_tools(tool_uses, &ctx).await;
        // Record results BEFORE checking cancellation so every tool_use has a
        // paired tool_result and history stays legal for the next request.
        history.record(Message::tool_results(results));
        if cancel.is_cancelled() {
            return TurnOutcome {
                reason: EndReason::Aborted,
                final_text: String::new(),
                rounds: round + 1,
            };
        }
    }
    TurnOutcome {
        reason: EndReason::MaxRounds,
        final_text: String::new(),
        rounds: cfg.max_rounds,
    }
}

struct SampleOk {
    blocks: Vec<ContentBlock>,
    usage: Option<Usage>,
    stop_reason: Option<String>,
}

enum Sampled {
    Ok(SampleOk),
    Overflow,
    Cancelled,
    Failed(String),
}

enum SampleError {
    Cancelled,
    Overflow,
    Retryable(String),
}

/// The response was cut off by the output token limit ("max_tokens" on the
/// Anthropic wire, "length" on OpenAI-compat). This is the one legitimate use
/// of stop_reason: not to decide continuation, but to detect an ungraceful
/// ending worth recovering from.
fn is_truncated(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, Some("max_tokens") | Some("length"))
}

const TRUNCATION_RECOVERY_LIMIT: u32 = 3;
const TRUNCATION_CONTINUE_MSG: &str = "Your previous response was cut off by the output token \
limit. Continue exactly where you left off; break the remaining work into smaller pieces.";

const MAX_ATTEMPTS: u32 = 3;

async fn sample_with_retry(
    cfg: &Arc<Config>,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Sampled {
    for attempt in 0..MAX_ATTEMPTS {
        match sample_once(cfg, model, messages, tools, ui, cancel, stream_text).await {
            Ok(ok) => return Sampled::Ok(ok),
            Err(SampleError::Cancelled) => return Sampled::Cancelled,
            // Retrying an oversized request verbatim can never succeed; hand
            // it straight to the reactive compaction path.
            Err(SampleError::Overflow) => return Sampled::Overflow,
            Err(SampleError::Retryable(e)) => {
                if attempt + 1 == MAX_ATTEMPTS {
                    return Sampled::Failed(e);
                }
                // Exponential backoff with sub-ms jitter from the clock's
                // nanoseconds — good enough without pulling in `rand`.
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()) % 250)
                    .unwrap_or(0);
                let delay = Duration::from_millis((250 << attempt) + jitter);
                ui.note(&format!(
                    "sampling failed (attempt {}/{MAX_ATTEMPTS}), retrying in {delay:?}: {e}",
                    attempt + 1
                ));
                tokio::select! {
                    _ = cancel.cancelled() => return Sampled::Cancelled,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    unreachable!("retry loop always returns")
}

#[allow(clippy::too_many_arguments)]
async fn sample_once(
    cfg: &Arc<Config>,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Result<SampleOk, SampleError> {
    let mut rx = cfg.provider.stream(model, &cfg.system, messages, tools);
    let mut blocks = Vec::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(SampleError::Cancelled),
            event = rx.recv() => match event {
                None => return Err(SampleError::Retryable("stream closed early".into())),
                Some(Err(e)) => {
                    if e.downcast_ref::<OverflowError>().is_some() {
                        return Err(SampleError::Overflow);
                    }
                    return Err(SampleError::Retryable(format!("{e:#}")));
                }
                Some(Ok(StreamEvent::TextDelta(t))) => {
                    if stream_text {
                        ui.text_delta(&t);
                    }
                }
                Some(Ok(StreamEvent::BlockDone(b))) => blocks.push(b),
                Some(Ok(StreamEvent::Done { usage, stop_reason })) => {
                    return Ok(SampleOk { blocks, usage, stop_reason })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::Role;
    use kloop_provider::Provider;
    use serde_json::json;

    struct NullUi;
    impl Ui for NullUi {
        fn text_delta(&self, _: &str) {}
        fn note(&self, _: &str) {}
    }

    fn tool_use(id: &str, cmd: &str) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: json!({"command": cmd}),
        }
    }

    /// End-to-end over the Mock provider: round 1 issues two concurrency-safe
    /// bash calls (one concurrent batch), round 2 one unsafe call (sequential),
    /// round 3 plain text ends the turn. Asserts the full history shape.
    #[tokio::test]
    async fn mock_end_to_end_three_rounds() {
        let provider = Provider::mock(vec![
            vec![tool_use("t1", "echo one"), tool_use("t2", "echo two")],
            vec![tool_use("t3", "true")],
            vec![ContentBlock::Text {
                text: "all done".into(),
            }],
        ]);
        let cfg = Arc::new(Config {
            provider: Arc::new(provider),
            model: "mock".into(),
            system: "test".into(),
            max_rounds: 10,
            offload_dir: std::env::temp_dir().join("kloop-test-e2e"),
            context_window: None,
            fallback_model: None,
        });
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("run the demo"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "all done");
        assert_eq!(outcome.rounds, 3);

        let msgs = history.messages();
        let shape: Vec<(Role, Vec<&str>)> = msgs
            .iter()
            .map(|m| {
                let kinds = m
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { .. } => "text",
                        ContentBlock::ToolUse { .. } => "tool_use",
                        ContentBlock::ToolResult { .. } => "tool_result",
                    })
                    .collect();
                (m.role, kinds)
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Role::User, vec!["text"]),
                (Role::Assistant, vec!["tool_use", "tool_use"]),
                (Role::User, vec!["tool_result", "tool_result"]),
                (Role::Assistant, vec!["tool_use"]),
                (Role::User, vec!["tool_result"]),
                (Role::Assistant, vec!["text"]),
            ]
        );

        // The concurrent batch preserved request order and captured output.
        assert_eq!(
            msgs[2].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "one\n".into(),
                is_error: false,
            }
        );
        assert_eq!(
            msgs[2].content[1],
            ContentBlock::ToolResult {
                tool_use_id: "t2".into(),
                content: "two\n".into(),
                is_error: false,
            }
        );
        assert_eq!(
            msgs[4].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "t3".into(),
                content: "(no output)".into(),
                is_error: false,
            }
        );
    }

    fn compaction_cfg(provider: Provider, window: u64, tag: &str) -> Arc<Config> {
        Arc::new(Config {
            provider: Arc::new(provider),
            model: "mock".into(),
            system: "test".into(),
            max_rounds: 10,
            offload_dir: std::env::temp_dir().join(format!("kloop-test-{tag}")),
            context_window: Some(window),
            fallback_model: None,
        })
    }

    /// Predictive: a fat history under a small (but > growth reserve) window
    /// triggers compaction BEFORE the sampling request. Mock turn 1 serves
    /// the summary, turn 2 the actual reply.
    #[tokio::test]
    async fn predictive_compaction_fires_before_sampling() {
        let provider = Provider::mock(vec![
            vec![ContentBlock::Text {
                text: "summary of everything so far".into(),
            }],
            vec![ContentBlock::Text {
                text: "final answer".into(),
            }],
        ]);
        // growth = 8192 + 15_000 = 23_192; window 30_000 → threshold ≈ 6_808
        // tokens ≈ 27k chars. Two fat user messages blow past it.
        let cfg = compaction_cfg(provider, 30_000, "predictive");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("x".repeat(30_000)));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "y".repeat(30_000),
        }]));
        history.record(Message::user_text("now answer briefly"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "final answer");
        let msgs = history.messages();
        // [summary, ...kept tail..., assistant reply]; the fat prefix is gone.
        let ContentBlock::Text { text } = &msgs[0].content[0] else {
            panic!("expected text summary at history start");
        };
        assert!(
            text.starts_with(crate::compact::SUMMARY_PREFIX),
            "history should start with the compaction summary"
        );
        assert!(
            history.estimated_tokens() < 5_000,
            "compaction should have shrunk the history, got {} tokens",
            history.estimated_tokens()
        );
    }

    /// Reactive: the first sampling request is rejected as too large; the
    /// loop compacts once (mock turn 2 = summary) and retries successfully
    /// (turn 3), with no user-visible error.
    #[tokio::test]
    async fn overflow_compacts_and_retries() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Overflow,
            MockTurn::Blocks(vec![ContentBlock::Text {
                text: "summary of everything so far".into(),
            }]),
            MockTurn::Blocks(vec![ContentBlock::Text {
                text: "recovered answer".into(),
            }]),
        ]);
        // Large window: predictive stays silent, only the reactive path runs.
        let cfg = compaction_cfg(provider, 200_000, "reactive");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("earlier context"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "earlier reply".into(),
        }]));
        history.record(Message::user_text("the request that overflows"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "recovered answer");
        let ContentBlock::Text { text } = &history.messages()[0].content[0] else {
            panic!("expected text summary at history start");
        };
        assert!(text.starts_with(crate::compact::SUMMARY_PREFIX));
    }

    /// A second overflow after a successful compaction must surface as an
    /// error instead of looping.
    #[tokio::test]
    async fn repeated_overflow_surfaces_error() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Overflow,
            MockTurn::Blocks(vec![ContentBlock::Text {
                text: "summary".into(),
            }]),
            MockTurn::Overflow,
        ]);
        let cfg = compaction_cfg(provider, 200_000, "reactive-repeat");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("earlier context"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "earlier reply".into(),
        }]));
        history.record(Message::user_text("still too big"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert!(
            matches!(outcome.reason, EndReason::Error(_)),
            "second overflow must not loop, got {:?}",
            outcome.reason
        );
    }

    fn text(t: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: t.into() }]
    }

    /// A truncated final response gets a "continue" nudge instead of ending
    /// the turn mid-thought.
    #[tokio::test]
    async fn truncated_response_recovers_with_continuation() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Truncated(text("part one, cut off mid-")),
            MockTurn::Blocks(text("part two, complete.")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "truncation");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("write something long"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "part two, complete.");
        assert_eq!(outcome.rounds, 2);
        // [user, assistant(truncated), user(continue nudge), assistant(rest)]
        let msgs = history.messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[2],
            Message::user_text(super::TRUNCATION_CONTINUE_MSG),
            "the continuation nudge must be recorded so history stays legal"
        );
    }

    /// Truncation nudges are bounded: after the limit the turn completes with
    /// whatever text arrived instead of looping.
    #[tokio::test]
    async fn truncation_recovery_is_bounded() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Truncated(text("cut 1")),
            MockTurn::Truncated(text("cut 2")),
            MockTurn::Truncated(text("cut 3")),
            MockTurn::Truncated(text("cut 4")),
            MockTurn::Truncated(text("cut 5")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "truncation-limit");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("write something very long"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        // 3 nudges (the limit), so the 4th truncated response ends the turn.
        assert_eq!(outcome.final_text, "cut 4");
        assert_eq!(outcome.rounds, 4);
        let nudges = history
            .messages()
            .iter()
            .filter(|m| *m == &Message::user_text(super::TRUNCATION_CONTINUE_MSG))
            .count();
        assert_eq!(nudges, 3);
    }

    /// After the primary model exhausts its retries, the turn continues on
    /// the fallback model instead of surfacing an error.
    #[tokio::test]
    async fn fallback_model_takes_over_after_retries() {
        use kloop_provider::MockTurn;
        struct NoteUi(std::sync::Mutex<Vec<String>>);
        impl Ui for NoteUi {
            fn text_delta(&self, _: &str) {}
            fn note(&self, s: &str) {
                self.0.lock().unwrap().push(s.to_string());
            }
        }

        let provider = Provider::mock_scripted(vec![
            MockTurn::Error("boom 1".into()),
            MockTurn::Error("boom 2".into()),
            MockTurn::Error("boom 3".into()),
            MockTurn::Blocks(text("answer from fallback")),
        ]);
        let mut cfg = (*compaction_cfg(provider, 200_000, "fallback")).clone();
        cfg.fallback_model = Some("mock-fallback".into());
        let cfg = Arc::new(cfg);
        let note_ui = Arc::new(NoteUi(std::sync::Mutex::new(Vec::new())));
        let ui: Arc<dyn Ui> = note_ui.clone();
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("hello"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "answer from fallback");
        let notes = note_ui.0.lock().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("switching to fallback model mock-fallback")),
            "expected a fallback-switch note, got {notes:?}"
        );
    }
}
