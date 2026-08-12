//! The provider seam: everything inside the crate speaks the canonical
//! Anthropic Messages shape; adapters translate at this boundary only.

mod anthropic;
mod failure;
mod openai;
mod responses;
pub mod sse;
mod stream;

use std::collections::HashSet;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::Value;
use serde_json::json;

pub use failure::ProviderFailure;
pub use failure::ProviderFailureKind;
pub use failure::TimeoutStage;
pub use stream::ProviderStream;
pub use stream::StreamResult;

pub(crate) use stream::GuardedBody;
pub(crate) use stream::StreamCompletion;
pub(crate) use stream::StreamSink;
pub(crate) use stream::send_checked;
use stream::spawn_stream;

use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::MAX_OUTPUT_TOKENS;
use kloop_protocol::Message;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;

/// One process-wide HTTP client shared by every adapter. reqwest pools
/// connections and reuses TLS sessions, but only within a single `Client`, so
/// a fresh `Client::new()` per request would discard that on every turn.
pub(crate) fn http_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new).clone()
}

/// What the Mock provider saw in one `stream()` call; lets core tests assert
/// the request shape (e.g. injected context messages) without a wire.
#[derive(Clone, Debug)]
pub struct MockRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

/// One scripted Mock response: content blocks, a gate-delayed response, a
/// truncated response, or a typed provider failure.
pub enum MockTurn {
    Blocks(Vec<AssistantBlock>),
    /// Completed blocks without display deltas, for lifecycle regression tests.
    BlocksWithoutDeltas(Vec<AssistantBlock>),
    /// Return an explicit semantic terminal after the supplied blocks.
    Outcome {
        blocks: Vec<AssistantBlock>,
        outcome: AssistantOutcome,
    },
    /// Return an explicit semantic terminal and canonical usage after the supplied blocks.
    Response {
        blocks: Vec<AssistantBlock>,
        outcome: AssistantOutcome,
        usage: Usage,
    },
    /// Report that sampling started, then wait for an explicit release before
    /// emitting blocks. Tests use this to coordinate concurrent and cancelled
    /// requests without wall-clock timing assumptions.
    Gate {
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
        blocks: Vec<AssistantBlock>,
    },
    /// Blocks delivered, but the stream reports the output limit was hit.
    Truncated(Vec<AssistantBlock>),
    /// Content deltas arrive, then the stream fails before any block completes.
    PartialError(Vec<AssistantBlock>, String),
    /// Complete blocks arrive, then the attempt fails before its terminal.
    BlocksThenError(Vec<AssistantBlock>, ProviderFailure),
    /// The request is rejected for exceeding the context window.
    Overflow,
    /// A retryable transport failure.
    Error(String),
    /// An explicitly classified failure for retry/fallback policy tests.
    Failure(ProviderFailure),
}

/// The `thinking` request parameter on the Anthropic wire. Whatever the mode,
/// thinking blocks the model sends are always accumulated and replayed — on
/// current models thinking is on by default even with no field sent.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThinkingMode {
    /// Send no thinking field (current models then run adaptive).
    #[default]
    Unset,
    /// `{"type": "disabled"}`.
    Off,
    /// `{"type": "adaptive"}` — explicit, for models where omitting means off.
    Adaptive,
    /// `{"type": "enabled", "budget_tokens": n}` for pre-adaptive models
    /// (rejected by current ones). Thinking spends from max_tokens, so the
    /// request raises max_tokens by the budget instead of clamping the budget.
    Budget(u64),
}

pub enum Provider {
    Anthropic {
        key: String,
        base: String,
        /// Prompt caching: mark cache_control breakpoints on the last tool,
        /// the system block, and the last message block.
        cache: bool,
        thinking: ThinkingMode,
    },
    OpenAiCompat {
        key: String,
        base: String,
    },
    /// OpenAI Responses API (`/responses`), stateless: `store: false`, with
    /// reasoning carried across requests via encrypted_content blobs riding
    /// in `Thinking.signature`.
    OpenAiResponses {
        key: String,
        base: String,
        /// `reasoning: {effort, summary: "auto"}` request field; None sends
        /// no reasoning field.
        effort: Option<String>,
    },
    /// Scripted turns for keyless end-to-end runs; each `stream()` call pops one turn.
    Mock {
        turns: Mutex<VecDeque<MockTurn>>,
        /// Requests as seen, shared out by `mock_recording`.
        seen: Arc<Mutex<Vec<MockRequest>>>,
    },
}

/// Provider-agnostic detection of "request too large for the context window"
/// error payloads.
pub(crate) fn is_overflow_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
}

pub(crate) fn parse_sse_json(rail: &str, data: &str) -> Result<Value, ProviderFailure> {
    serde_json::from_str(data)
        .map_err(|error| ProviderFailure::protocol(format!("{rail} malformed SSE JSON: {error}")))
}

fn validate_assistant_blocks(
    rail: &str,
    blocks: &[AssistantBlock],
) -> Result<bool, ProviderFailure> {
    let mut tool_ids = HashSet::new();
    let mut has_tool = false;
    for block in blocks {
        if let AssistantBlock::ToolUse { id, name, input } = block {
            has_tool = true;
            if id.is_empty() || name.is_empty() {
                return Err(ProviderFailure::protocol(format!(
                    "{rail} completed a tool call with an empty identity"
                )));
            }
            if !input.is_object() {
                return Err(ProviderFailure::protocol(format!(
                    "{rail} completed tool {name} with non-object input"
                )));
            }
            if !tool_ids.insert(id) {
                return Err(ProviderFailure::protocol(format!(
                    "{rail} completed duplicate tool call id"
                )));
            }
        }
    }
    Ok(has_tool)
}

pub(crate) fn validate_assistant_output(
    rail: &str,
    outcome: &AssistantOutcome,
    blocks: &[AssistantBlock],
) -> Result<(), ProviderFailure> {
    let has_tool = validate_assistant_blocks(rail, blocks)?;
    match (matches!(outcome, AssistantOutcome::ToolUse), has_tool) {
        (true, false) => Err(ProviderFailure::protocol(format!(
            "{rail} reported tool use without a completed tool call"
        ))),
        (false, true) => Err(ProviderFailure::protocol(format!(
            "{rail} completed a tool call for a non-tool outcome"
        ))),
        _ => Ok(()),
    }
}

pub(crate) fn parse_tool_input(
    rail: &str,
    tool_name: &str,
    raw: &str,
) -> Result<Value, ProviderFailure> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let input: Value = serde_json::from_str(raw).map_err(|error| {
        ProviderFailure::protocol(format!(
            "{rail} tool {tool_name} returned invalid JSON input: {error}"
        ))
    })?;
    if !input.is_object() {
        return Err(ProviderFailure::protocol(format!(
            "{rail} tool {tool_name} returned non-object JSON input"
        )));
    }
    Ok(input)
}

impl Provider {
    pub fn mock(turns: Vec<Vec<AssistantBlock>>) -> Self {
        Self::mock_scripted(turns.into_iter().map(MockTurn::Blocks).collect())
    }

    pub fn mock_scripted(turns: Vec<MockTurn>) -> Self {
        Provider::Mock {
            turns: Mutex::new(turns.into()),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// Like `mock_scripted`, but also hands back the request log.
    pub fn mock_recording(turns: Vec<MockTurn>) -> (Self, Arc<Mutex<Vec<MockRequest>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let provider = Provider::Mock {
            turns: Mutex::new(turns.into()),
            seen: seen.clone(),
        };
        (provider, seen)
    }

    /// Start one streaming sampling request. The returned receiver owns the
    /// producer task; dropping it aborts an in-flight open/body read.
    pub fn stream(
        self: &Arc<Self>,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> ProviderStream {
        match self.as_ref() {
            Provider::Mock { turns, seen } => {
                seen.lock().unwrap().push(MockRequest {
                    model: model.to_string(),
                    system: system.to_string(),
                    messages: messages.to_vec(),
                    tools: tools.to_vec(),
                });
                let turn = turns.lock().unwrap().pop_front().unwrap_or_else(|| {
                    MockTurn::Blocks(vec![AssistantBlock::Text {
                        text: "mock exhausted".into(),
                    }])
                });
                spawn_stream(move |sink| async move { run_mock_turn(turn, &sink).await })
            }
            Provider::Anthropic {
                key,
                base,
                cache,
                thinking,
            } => {
                let url = format!("{base}/v1/messages");
                let key = key.clone();
                let mut body = json!({
                    "model": model,
                    "max_tokens": MAX_OUTPUT_TOKENS,
                    "system": anthropic::system_value(system, *cache),
                    "messages": anthropic::messages_value(messages, *cache),
                    "tools": anthropic::tools_value(tools, *cache),
                    "stream": true,
                });
                match thinking {
                    ThinkingMode::Unset => {}
                    ThinkingMode::Off => body["thinking"] = json!({"type": "disabled"}),
                    ThinkingMode::Adaptive => body["thinking"] = json!({"type": "adaptive"}),
                    ThinkingMode::Budget(n) => {
                        body["thinking"] = json!({"type": "enabled", "budget_tokens": n});
                        body["max_tokens"] = json!(MAX_OUTPUT_TOKENS + n);
                    }
                }
                spawn_stream(move |sink| async move {
                    anthropic::stream(&url, &key, &body, &sink).await
                })
            }
            Provider::OpenAiResponses { key, base, effort } => {
                let url = format!("{base}/responses");
                let key = key.clone();
                let mut body = json!({
                    "model": model,
                    "instructions": system,
                    "input": responses::to_input_items(messages),
                    "tools": tools.iter().map(|t| json!({
                        "type": "function",
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.schema,
                    })).collect::<Vec<_>>(),
                    "max_output_tokens": MAX_OUTPUT_TOKENS,
                    "parallel_tool_calls": true,
                    "store": false,
                    "include": ["reasoning.encrypted_content"],
                    "stream": true,
                });
                if let Some(effort) = effort {
                    body["reasoning"] = json!({"effort": effort, "summary": "auto"});
                }
                spawn_stream(move |sink| async move {
                    responses::stream(&url, &key, &body, &sink).await
                })
            }
            Provider::OpenAiCompat { key, base } => {
                let url = format!("{base}/chat/completions");
                let key = key.clone();
                let body = json!({
                    "model": model,
                    "max_tokens": MAX_OUTPUT_TOKENS,
                    "stream_options": {"include_usage": true},
                    "messages": openai::to_openai_messages(system, messages),
                    "tools": tools.iter().map(|t| json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.schema,
                        },
                    })).collect::<Vec<_>>(),
                    "stream": true,
                });
                spawn_stream(
                    move |sink| async move { openai::stream(&url, &key, &body, &sink).await },
                )
            }
        }
    }
}

fn mock_outcome(blocks: &[AssistantBlock]) -> AssistantOutcome {
    if blocks
        .iter()
        .any(|block| matches!(block, AssistantBlock::ToolUse { .. }))
    {
        AssistantOutcome::ToolUse
    } else {
        AssistantOutcome::EndTurn
    }
}

async fn run_mock_turn(
    turn: MockTurn,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let (blocks, outcome, usage, with_deltas) = match turn {
        MockTurn::Blocks(blocks) => {
            let outcome = mock_outcome(&blocks);
            (blocks, outcome, None, true)
        }
        MockTurn::BlocksWithoutDeltas(blocks) => {
            let outcome = mock_outcome(&blocks);
            (blocks, outcome, None, false)
        }
        MockTurn::Outcome { blocks, outcome } => (blocks, outcome, None, true),
        MockTurn::Response {
            blocks,
            outcome,
            usage,
        } => (blocks, outcome, Some(usage), true),
        MockTurn::Gate {
            started,
            release,
            blocks,
        } => {
            let _ = started.send(());
            let _ = release.await;
            let outcome = mock_outcome(&blocks);
            (blocks, outcome, None, true)
        }
        MockTurn::Truncated(blocks) => (
            blocks,
            AssistantOutcome::OutputLimit(kloop_protocol::OutputLimitKind::MaxOutputTokens),
            None,
            true,
        ),
        MockTurn::PartialError(blocks, message) => {
            emit_deltas(&blocks, sink).await?;
            return Err(ProviderFailure::transport(message));
        }
        MockTurn::BlocksThenError(blocks, failure) => {
            validate_assistant_blocks("mock", &blocks)?;
            emit_blocks(blocks, sink).await?;
            return Err(failure);
        }
        MockTurn::Overflow => return Err(ProviderFailure::context_overflow()),
        MockTurn::Error(message) => return Err(ProviderFailure::transport(message)),
        MockTurn::Failure(failure) => return Err(failure),
    };
    validate_assistant_output("mock", &outcome, &blocks)?;
    if with_deltas {
        emit_blocks(blocks, sink).await?;
    } else {
        for block in blocks {
            sink.block_done(block).await?;
        }
    }
    Ok(StreamCompletion::new(outcome, usage))
}

async fn emit_deltas(blocks: &[AssistantBlock], sink: &StreamSink) -> Result<(), ProviderFailure> {
    for block in blocks {
        match block {
            AssistantBlock::Text { text } => sink.text_delta(text.clone()).await?,
            AssistantBlock::Thinking { thinking, .. } => {
                sink.thinking_delta(thinking.clone()).await?
            }
            AssistantBlock::RedactedThinking { .. } | AssistantBlock::ToolUse { .. } => {}
        }
    }
    Ok(())
}

async fn emit_blocks(
    blocks: Vec<AssistantBlock>,
    sink: &StreamSink,
) -> Result<(), ProviderFailure> {
    emit_deltas(&blocks, sink).await?;
    for block in blocks {
        sink.block_done(block).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_input_requires_complete_json_object() {
        assert_eq!(parse_tool_input("test", "bash", "").unwrap(), json!({}));
        assert_eq!(parse_tool_input("test", "bash", "  \n").unwrap(), json!({}));
        assert_eq!(
            parse_tool_input("test", "bash", r#"{"command":"pwd"}"#).unwrap(),
            json!({"command": "pwd"})
        );

        for raw in ["null", "[]", "true", "1", r#""text""#] {
            let error = parse_tool_input("test", "bash", raw).unwrap_err();
            assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
            assert!(!error.is_retryable());
            assert!(error.to_string().contains("non-object JSON input"));
        }
        let error = parse_tool_input("test", "bash", "{oops").unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
        assert!(!error.is_retryable());
        assert!(error.to_string().contains("invalid JSON input"));
    }

    #[tokio::test]
    async fn mock_rejects_invalid_blocks_and_outcome_mismatches_before_emitting() {
        let tool = |id: &str, input: Value| AssistantBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input,
        };
        let cases = vec![
            MockTurn::Outcome {
                blocks: vec![tool("", json!({}))],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![tool("t1", json!([]))],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![tool("t1", json!({})), tool("t1", json!({}))],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![AssistantBlock::Text { text: "x".into() }],
                outcome: AssistantOutcome::ToolUse,
            },
            MockTurn::Outcome {
                blocks: vec![tool("t1", json!({}))],
                outcome: AssistantOutcome::EndTurn,
            },
        ];

        for turn in cases {
            let provider = Arc::new(Provider::mock_scripted(vec![turn]));
            let mut stream = provider.stream("mock", "system", &[], &[]);
            let error = stream.recv().await.unwrap().unwrap_err();
            assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
            assert!(!error.after_semantic_output());
            assert!(stream.recv().await.is_none());
        }
    }

    #[tokio::test]
    async fn dropping_provider_stream_aborts_its_producer() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let provider = Arc::new(Provider::mock_scripted(vec![MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: Vec::new(),
        }]));
        let stream = provider.stream("mock", "system", &[], &[]);
        started_rx.await.unwrap();

        drop(stream);
        tokio::task::yield_now().await;

        assert!(
            release_tx.send(()).is_err(),
            "aborting the producer must drop the gate receiver"
        );
    }
}
