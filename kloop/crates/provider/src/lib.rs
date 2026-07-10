//! The provider seam: everything inside the crate speaks the canonical
//! Anthropic Messages shape; adapters translate at this boundary only.

mod anthropic;
mod openai;
mod responses;
pub mod sse;

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use serde_json::json;
use tokio::sync::mpsc;

use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::MAX_OUTPUT_TOKENS;

/// What the Mock provider saw in one `stream()` call; lets core tests assert
/// the request shape (e.g. injected context messages) without a wire.
#[derive(Clone, Debug)]
pub struct MockRequest {
    pub model: String,
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

/// One scripted Mock response: content blocks, a truncated response, or a
/// provider error.
pub enum MockTurn {
    Blocks(Vec<ContentBlock>),
    /// Blocks delivered, but the stream reports the output limit was hit.
    Truncated(Vec<ContentBlock>),
    /// The request is rejected for exceeding the context window.
    Overflow,
    /// A transient provider failure (retryable).
    Error(String),
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
    /// request raises max_tokens by the budget instead of clamping the budget
    /// (a clamp degenerates at small limits — lesson 4).
    Budget(u64),
}

pub enum Provider {
    Anthropic {
        key: String,
        base: String,
        /// Prompt caching: mark cache_control breakpoints on the last tool,
        /// the system block, and the last message block. On by default (pure
        /// cost saving); the escape hatch exists for diagnosing cache
        /// behavior.
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
        /// no reasoning field. Backends may emit no reasoning items at all
        /// without it (live-observed), so this is also the switch that turns
        /// reasoning capture on.
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
fn is_overflow_message(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("prompt is too long")
        || lower.contains("context_length_exceeded")
        || lower.contains("maximum context length")
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

    /// Start one streaming sampling request. The request body is built before
    /// spawning so no borrowed data crosses into the task.
    pub fn stream(
        self: &Arc<Self>,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolDef],
    ) -> mpsc::Receiver<Result<StreamEvent>> {
        let (tx, rx) = mpsc::channel::<Result<StreamEvent>>(64);
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
                tokio::spawn(async move {
                    let (blocks, stop_reason) = match turn {
                        MockTurn::Blocks(blocks) => (blocks, None),
                        MockTurn::Truncated(blocks) => (blocks, Some("max_tokens".to_string())),
                        MockTurn::Overflow => {
                            let _ = tx.send(Err(anyhow::Error::new(OverflowError))).await;
                            return;
                        }
                        MockTurn::Error(message) => {
                            let _ = tx.send(Err(anyhow::anyhow!(message))).await;
                            return;
                        }
                    };
                    for block in &blocks {
                        match block {
                            ContentBlock::Text { text } => {
                                let _ = tx.send(Ok(StreamEvent::TextDelta(text.clone()))).await;
                            }
                            ContentBlock::Thinking { thinking, .. } => {
                                let _ = tx
                                    .send(Ok(StreamEvent::ThinkingDelta(thinking.clone())))
                                    .await;
                            }
                            _ => {}
                        }
                    }
                    for block in blocks {
                        let _ = tx.send(Ok(StreamEvent::BlockDone(block))).await;
                    }
                    let _ = tx
                        .send(Ok(StreamEvent::Done {
                            stop_reason,
                            usage: None,
                        }))
                        .await;
                });
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
                tokio::spawn(async move {
                    if let Err(e) = anthropic::stream(&url, &key, &body, &tx).await {
                        let _ = tx.send(Err(e)).await;
                    }
                });
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
                    // Stateless: the server keeps nothing, so reasoning must
                    // come back as encrypted blobs for the next request.
                    "store": false,
                    "include": ["reasoning.encrypted_content"],
                    "stream": true,
                });
                if let Some(effort) = effort {
                    // summary=auto asks for displayable reasoning summaries
                    // alongside the encrypted blob.
                    body["reasoning"] = json!({"effort": effort, "summary": "auto"});
                }
                tokio::spawn(async move {
                    if let Err(e) = responses::stream(&url, &key, &body, &tx).await {
                        let _ = tx.send(Err(e)).await;
                    }
                });
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
                tokio::spawn(async move {
                    if let Err(e) = openai::stream(&url, &key, &body, &tx).await {
                        let _ = tx.send(Err(e)).await;
                    }
                });
            }
        }
        rx
    }
}
