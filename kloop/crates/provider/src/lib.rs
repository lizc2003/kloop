//! The provider seam: everything inside the crate speaks the canonical
//! Anthropic Messages shape; adapters translate at this boundary only.

mod anthropic;
mod openai;
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
    pub system: String,
    pub messages: Vec<Message>,
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

pub enum Provider {
    Anthropic {
        key: String,
        base: String,
        /// Prompt caching: mark cache_control breakpoints on the system block
        /// and the last message block. On by default (pure cost saving); the
        /// escape hatch exists for diagnosing cache behavior.
        cache: bool,
    },
    OpenAiCompat {
        key: String,
        base: String,
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
                    system: system.to_string(),
                    messages: messages.to_vec(),
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
                        if let ContentBlock::Text { text } = block {
                            let _ = tx.send(Ok(StreamEvent::TextDelta(text.clone()))).await;
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
            Provider::Anthropic { key, base, cache } => {
                let url = format!("{base}/v1/messages");
                let key = key.clone();
                let body = json!({
                    "model": model,
                    "max_tokens": MAX_OUTPUT_TOKENS,
                    "system": anthropic::system_value(system, *cache),
                    "messages": anthropic::messages_value(messages, *cache),
                    "tools": tools.iter().map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.schema,
                    })).collect::<Vec<_>>(),
                    "stream": true,
                });
                tokio::spawn(async move {
                    if let Err(e) = anthropic::stream(&url, &key, &body, &tx).await {
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
