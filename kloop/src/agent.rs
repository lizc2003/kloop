use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::history::History;
use crate::tools::dispatch_tools;
use crate::tools::tool_defs;
use crate::tools::ToolCtx;
use crate::types::ContentBlock;
use crate::types::Message;
use crate::types::StreamEvent;
use crate::types::ToolDef;
use crate::Config;

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
    for round in 0..cfg.max_rounds {
        let blocks = match sample_with_retry(cfg, history.messages(), &tools, ui, cancel, stream_text).await
        {
            Sampled::Blocks(blocks) => blocks,
            Sampled::Cancelled => {
                return TurnOutcome {
                    reason: EndReason::Aborted,
                    final_text: String::new(),
                    rounds: round,
                }
            }
            Sampled::Failed(e) => {
                return TurnOutcome {
                    reason: EndReason::Error(e),
                    final_text: String::new(),
                    rounds: round,
                }
            }
        };
        history.record(Message::assistant(blocks.clone()));

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

enum Sampled {
    Blocks(Vec<ContentBlock>),
    Cancelled,
    Failed(String),
}

enum SampleError {
    Cancelled,
    Retryable(String),
}

const MAX_ATTEMPTS: u32 = 3;

async fn sample_with_retry(
    cfg: &Arc<Config>,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Sampled {
    for attempt in 0..MAX_ATTEMPTS {
        match sample_once(cfg, messages, tools, ui, cancel, stream_text).await {
            Ok(blocks) => return Sampled::Blocks(blocks),
            Err(SampleError::Cancelled) => return Sampled::Cancelled,
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

async fn sample_once(
    cfg: &Arc<Config>,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Result<Vec<ContentBlock>, SampleError> {
    let mut rx = cfg.provider.stream(&cfg.model, &cfg.system, messages, tools);
    let mut blocks = Vec::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(SampleError::Cancelled),
            event = rx.recv() => match event {
                None => return Err(SampleError::Retryable("stream closed early".into())),
                Some(Err(e)) => return Err(SampleError::Retryable(format!("{e:#}"))),
                Some(Ok(StreamEvent::TextDelta(t))) => {
                    if stream_text {
                        ui.text_delta(&t);
                    }
                }
                Some(Ok(StreamEvent::BlockDone(b))) => blocks.push(b),
                Some(Ok(StreamEvent::Done { .. })) => return Ok(blocks),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Provider;
    use crate::types::Role;
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
}
