use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use futures::StreamExt;
use serde_json::json;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::sse::SseParser;
use crate::types::ContentBlock;
use crate::types::Message;
use crate::types::OverflowError;
use crate::types::Role;
use crate::types::StreamEvent;
use crate::types::ToolDef;
use crate::types::Usage;
use crate::types::MAX_OUTPUT_TOKENS;

/// One scripted Mock response: either content blocks or a provider error.
pub enum MockTurn {
    Blocks(Vec<ContentBlock>),
    /// The request is rejected for exceeding the context window.
    Overflow,
}

pub enum Provider {
    Anthropic {
        key: String,
        base: String,
    },
    OpenAiCompat {
        key: String,
        base: String,
    },
    /// Scripted turns for keyless end-to-end runs; each `stream()` call pops one turn.
    Mock {
        turns: Mutex<VecDeque<MockTurn>>,
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
        }
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
            Provider::Mock { turns } => {
                let turn = turns.lock().unwrap().pop_front().unwrap_or_else(|| {
                    MockTurn::Blocks(vec![ContentBlock::Text {
                        text: "mock exhausted".into(),
                    }])
                });
                tokio::spawn(async move {
                    let blocks = match turn {
                        MockTurn::Blocks(blocks) => blocks,
                        MockTurn::Overflow => {
                            let _ = tx.send(Err(anyhow::Error::new(OverflowError))).await;
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
                            stop_reason: None,
                            usage: None,
                        }))
                        .await;
                });
            }
            Provider::Anthropic { key, base } => {
                let url = format!("{base}/v1/messages");
                let key = key.clone();
                let body = json!({
                    "model": model,
                    "max_tokens": MAX_OUTPUT_TOKENS,
                    "system": system,
                    "messages": messages,
                    "tools": tools.iter().map(|t| json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.schema,
                    })).collect::<Vec<_>>(),
                    "stream": true,
                });
                tokio::spawn(async move {
                    if let Err(e) = anthropic_stream(&url, &key, &body, &tx).await {
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
                    "messages": to_openai_messages(system, messages),
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
                    if let Err(e) = openai_stream(&url, &key, &body, &tx).await {
                        let _ = tx.send(Err(e)).await;
                    }
                });
            }
        }
        rx
    }
}

// ---------------------------------------------------------------- Anthropic

#[derive(Default)]
struct AnthropicBlockAcc {
    is_tool_use: bool,
    id: String,
    name: String,
    text: String,
    json: String,
}

async fn anthropic_stream(
    url: &str,
    key: &str,
    body: &Value,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let resp = reqwest::Client::new()
        .post(url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if is_overflow_message(&text) {
            return Err(anyhow::Error::new(OverflowError));
        }
        bail!("anthropic http {status}: {text}");
    }

    let mut parser = SseParser::default();
    let mut byte_stream = resp.bytes_stream();
    // Open blocks by stream index; unknown block kinds (thinking, ...) are never
    // inserted, so their deltas fall through harmlessly.
    let mut open: HashMap<u64, AnthropicBlockAcc> = HashMap::new();
    let mut stop_reason: Option<String> = None;
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        for frame in parser.feed(&String::from_utf8_lossy(&chunk)) {
            let v: Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["type"].as_str().unwrap_or_default() {
                "message_start" => {
                    if let Some(n) = v["message"]["usage"]["input_tokens"].as_u64() {
                        input_tokens = Some(n);
                    }
                }
                "content_block_start" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let cb = &v["content_block"];
                    match cb["type"].as_str().unwrap_or_default() {
                        "text" => {
                            open.insert(index, AnthropicBlockAcc::default());
                        }
                        "tool_use" => {
                            open.insert(
                                index,
                                AnthropicBlockAcc {
                                    is_tool_use: true,
                                    id: cb["id"].as_str().unwrap_or_default().to_string(),
                                    name: cb["name"].as_str().unwrap_or_default().to_string(),
                                    ..Default::default()
                                },
                            );
                        }
                        _ => {}
                    }
                }
                "content_block_delta" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let Some(acc) = open.get_mut(&index) else {
                        continue;
                    };
                    match v["delta"]["type"].as_str().unwrap_or_default() {
                        "text_delta" => {
                            let piece = v["delta"]["text"].as_str().unwrap_or_default();
                            acc.text.push_str(piece);
                            let _ = tx.send(Ok(StreamEvent::TextDelta(piece.into()))).await;
                        }
                        "input_json_delta" => {
                            acc.json
                                .push_str(v["delta"]["partial_json"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let Some(acc) = open.remove(&index) else {
                        continue;
                    };
                    let block = if acc.is_tool_use {
                        ContentBlock::ToolUse {
                            id: acc.id,
                            name: acc.name,
                            input: if acc.json.trim().is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&acc.json).unwrap_or_else(|_| json!({}))
                            },
                        }
                    } else {
                        ContentBlock::Text { text: acc.text }
                    };
                    let _ = tx.send(Ok(StreamEvent::BlockDone(block))).await;
                }
                "message_delta" => {
                    if let Some(r) = v["delta"]["stop_reason"].as_str() {
                        stop_reason = Some(r.to_string());
                    }
                    // Cumulative output tokens ride on message_delta events.
                    if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                        output_tokens = Some(n);
                    }
                }
                "message_stop" => {
                    let usage = input_tokens.map(|input| Usage {
                        input_tokens: input,
                        output_tokens: output_tokens.unwrap_or(0),
                    });
                    let _ = tx
                        .send(Ok(StreamEvent::Done {
                            stop_reason: stop_reason.take(),
                            usage,
                        }))
                        .await;
                    return Ok(());
                }
                "error" => {
                    if is_overflow_message(&v["error"].to_string()) {
                        return Err(anyhow::Error::new(OverflowError));
                    }
                    bail!("anthropic stream error: {}", v["error"]);
                }
                _ => {}
            }
        }
    }
    // Stream ended without message_stop; the caller treats a closed channel
    // without Done as retryable.
    Ok(())
}

// ------------------------------------------------------------ OpenAI-compat

/// Translate canonical (Anthropic-shaped) history into chat/completions messages.
fn to_openai_messages(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = vec![json!({"role": "system", "content": system})];
    for msg in messages {
        match msg.role {
            Role::Assistant => {
                let mut text = String::new();
                let mut tool_calls = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolUse { id, name, input } => tool_calls.push(json!({
                            "id": id,
                            "type": "function",
                            "function": {
                                "name": name,
                                "arguments": serde_json::to_string(input).unwrap_or_default(),
                            },
                        })),
                        ContentBlock::ToolResult { .. } => {}
                    }
                }
                let mut m = json!({"role": "assistant"});
                m["content"] = if text.is_empty() {
                    Value::Null
                } else {
                    Value::String(text)
                };
                if !tool_calls.is_empty() {
                    m["tool_calls"] = Value::Array(tool_calls);
                }
                out.push(m);
            }
            Role::User => {
                // Tool results must directly follow the assistant tool_calls
                // message, so they go first; trailing text becomes a user msg.
                let mut text = String::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let content = if *is_error {
                                format!("[error] {content}")
                            } else {
                                content.clone()
                            };
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": content,
                            }));
                        }
                        ContentBlock::Text { text: t } => text.push_str(t),
                        ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !text.is_empty() {
                    out.push(json!({"role": "user", "content": text}));
                }
            }
        }
    }
    out
}

#[derive(Default)]
struct OpenAiCallAcc {
    id: String,
    name: String,
    args: String,
}

async fn openai_stream(
    url: &str,
    key: &str,
    body: &Value,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let resp = reqwest::Client::new()
        .post(url)
        .bearer_auth(key)
        .json(body)
        .send()
        .await?;
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if is_overflow_message(&text) {
            return Err(anyhow::Error::new(OverflowError));
        }
        bail!("openai-compat http {status}: {text}");
    }

    let mut parser = SseParser::default();
    let mut byte_stream = resp.bytes_stream();
    let mut text = String::new();
    let mut calls: Vec<OpenAiCallAcc> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut finished = false;

    'outer: while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        for frame in parser.feed(&String::from_utf8_lossy(&chunk)) {
            if frame.data.trim() == "[DONE]" {
                finished = true;
                break 'outer;
            }
            let v: Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if !v["error"].is_null() {
                if is_overflow_message(&v["error"].to_string()) {
                    return Err(anyhow::Error::new(OverflowError));
                }
                bail!("openai-compat stream error: {}", v["error"]);
            }
            // With include_usage the final pre-[DONE] chunk carries usage and
            // empty choices.
            if v["usage"].is_object() {
                usage = Some(Usage {
                    input_tokens: v["usage"]["prompt_tokens"].as_u64().unwrap_or(0),
                    output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                });
            }
            let delta = &v["choices"][0]["delta"];
            if let Some(piece) = delta["content"].as_str() {
                if !piece.is_empty() {
                    text.push_str(piece);
                    let _ = tx.send(Ok(StreamEvent::TextDelta(piece.into()))).await;
                }
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                for tc in tcs {
                    let index = tc["index"].as_u64().unwrap_or(0) as usize;
                    while calls.len() <= index {
                        calls.push(OpenAiCallAcc::default());
                    }
                    let acc = &mut calls[index];
                    if let Some(id) = tc["id"].as_str() {
                        acc.id.push_str(id);
                    }
                    if let Some(name) = tc["function"]["name"].as_str() {
                        acc.name.push_str(name);
                    }
                    if let Some(args) = tc["function"]["arguments"].as_str() {
                        acc.args.push_str(args);
                    }
                }
            }
            if let Some(r) = v["choices"][0]["finish_reason"].as_str() {
                stop_reason = Some(r.to_string());
                // Keep reading: with include_usage the usage chunk arrives
                // after finish_reason, before [DONE].
                finished = true;
            }
        }
    }

    if !finished {
        // Connection died mid-stream; closing without Done signals retryable.
        return Err(anyhow!("openai-compat stream ended before finish"));
    }
    if !text.is_empty() {
        let _ = tx
            .send(Ok(StreamEvent::BlockDone(ContentBlock::Text { text })))
            .await;
    }
    for acc in calls {
        let input = if acc.args.trim().is_empty() {
            json!({})
        } else {
            serde_json::from_str(&acc.args).unwrap_or(Value::String(acc.args))
        };
        let _ = tx
            .send(Ok(StreamEvent::BlockDone(ContentBlock::ToolUse {
                id: acc.id,
                name: acc.name,
                input,
            })))
            .await;
    }
    let _ = tx.send(Ok(StreamEvent::Done { stop_reason, usage })).await;
    Ok(())
}
