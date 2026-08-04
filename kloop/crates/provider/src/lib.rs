//! The provider seam: everything inside the crate speaks the canonical
//! Anthropic Messages shape; adapters translate at this boundary only.

mod anthropic;
mod failure;
mod openai;
mod responses;
pub mod sse;
mod stream;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::OnceLock;

use serde_json::json;
use serde_json::Value;

pub use failure::ProviderFailure;
pub use failure::ProviderFailureKind;
pub use failure::TimeoutStage;
pub use stream::ProviderStream;
pub use stream::StreamResult;

pub(crate) use stream::send_checked;
use stream::spawn_stream;
pub(crate) use stream::GuardedBody;
pub(crate) use stream::StreamCompletion;
pub(crate) use stream::StreamSink;

use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::ToolDef;
use kloop_protocol::MAX_OUTPUT_TOKENS;

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
    Blocks(Vec<ContentBlock>),
    /// Report that sampling started, then wait for an explicit release before
    /// emitting blocks. Tests use this to coordinate concurrent and cancelled
    /// requests without wall-clock timing assumptions.
    Gate {
        started: tokio::sync::oneshot::Sender<()>,
        release: tokio::sync::oneshot::Receiver<()>,
        blocks: Vec<ContentBlock>,
    },
    /// Blocks delivered, but the stream reports the output limit was hit.
    Truncated(Vec<ContentBlock>),
    /// Content deltas arrive, then the stream fails before any block completes.
    PartialError(Vec<ContentBlock>, String),
    /// Complete blocks arrive, then the attempt fails before its terminal.
    BlocksThenError(Vec<ContentBlock>, ProviderFailure),
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

pub(crate) fn parse_tool_input(
    rail: &str,
    tool_name: &str,
    raw: &str,
) -> Result<Value, ProviderFailure> {
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(raw).map_err(|error| {
        ProviderFailure::protocol(format!(
            "{rail} tool {tool_name} returned invalid JSON input: {error}"
        ))
    })
}

impl Provider {
    pub fn mock(turns: Vec<Vec<ContentBlock>>) -> Self {
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
                    MockTurn::Blocks(vec![ContentBlock::Text {
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

async fn run_mock_turn(
    turn: MockTurn,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let (blocks, stop_reason) = match turn {
        MockTurn::Blocks(blocks) => (blocks, None),
        MockTurn::Gate {
            started,
            release,
            blocks,
        } => {
            let _ = started.send(());
            let _ = release.await;
            (blocks, None)
        }
        MockTurn::Truncated(blocks) => (blocks, Some("max_tokens".to_string())),
        MockTurn::PartialError(blocks, message) => {
            emit_deltas(&blocks, sink).await?;
            return Err(ProviderFailure::transport(message));
        }
        MockTurn::BlocksThenError(blocks, failure) => {
            emit_blocks(blocks, sink).await?;
            return Err(failure);
        }
        MockTurn::Overflow => return Err(ProviderFailure::context_overflow()),
        MockTurn::Error(message) => return Err(ProviderFailure::transport(message)),
        MockTurn::Failure(failure) => return Err(failure),
    };
    emit_blocks(blocks, sink).await?;
    Ok(StreamCompletion::new(stop_reason, None))
}

async fn emit_deltas(blocks: &[ContentBlock], sink: &StreamSink) -> Result<(), ProviderFailure> {
    for block in blocks {
        match block {
            ContentBlock::Text { text } => sink.text_delta(text.clone()).await?,
            ContentBlock::Thinking { thinking, .. } => {
                sink.thinking_delta(thinking.clone()).await?
            }
            _ => {}
        }
    }
    Ok(())
}

async fn emit_blocks(blocks: Vec<ContentBlock>, sink: &StreamSink) -> Result<(), ProviderFailure> {
    emit_deltas(&blocks, sink).await?;
    for block in blocks {
        sink.block_done(block).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

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
