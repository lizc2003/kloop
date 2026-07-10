//! Anthropic Messages native SSE adapter.

use std::collections::HashMap;

use anyhow::bail;
use anyhow::Result;
use futures::StreamExt;
use serde_json::json;
use serde_json::Value;
use tokio::sync::mpsc;

use super::is_overflow_message;
use super::sse::SseParser;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;

/// The `system` request field: a plain string without caching, or a one-block
/// array whose cache_control breakpoint caches tools + system together (the
/// request renders tools -> system -> messages, and a breakpoint covers
/// everything before it).
pub(super) fn system_value(system: &str, cache: bool) -> Value {
    if !cache {
        return Value::String(system.to_string());
    }
    json!([{
        "type": "text",
        "text": system,
        "cache_control": {"type": "ephemeral"},
    }])
}

/// Serialize the history, marking the last content block of the last message
/// as the moving cache breakpoint. Earlier requests' breakpoints remain valid
/// read points server-side, so each round reuses the whole prior prefix.
/// cache_control stays out of the protocol types: it is a transport detail
/// injected here, never persisted.
pub(super) fn messages_value(messages: &[Message], cache: bool) -> Value {
    let mut value = serde_json::to_value(messages).unwrap_or_default();
    if cache {
        if let Some(block) = value
            .as_array_mut()
            .and_then(|msgs| msgs.last_mut())
            .and_then(|msg| msg["content"].as_array_mut())
            .and_then(|content| content.last_mut())
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    value
}

#[derive(Default)]
struct BlockAcc {
    is_tool_use: bool,
    id: String,
    name: String,
    text: String,
    json: String,
}

pub(super) async fn stream(
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
    let mut open: HashMap<u64, BlockAcc> = HashMap::new();
    let mut stop_reason: Option<String> = None;
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;
    let mut cache_read: u64 = 0;
    let mut cache_creation: u64 = 0;

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        for frame in parser.feed(&String::from_utf8_lossy(&chunk)) {
            let v: Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["type"].as_str().unwrap_or_default() {
                "message_start" => {
                    let usage = &v["message"]["usage"];
                    if let Some(n) = usage["input_tokens"].as_u64() {
                        input_tokens = Some(n);
                    }
                    // Cached prompt tokens are reported next to (not inside)
                    // input_tokens; both kinds still occupy the window.
                    cache_read = usage["cache_read_input_tokens"].as_u64().unwrap_or(0);
                    cache_creation = usage["cache_creation_input_tokens"].as_u64().unwrap_or(0);
                }
                "content_block_start" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let cb = &v["content_block"];
                    match cb["type"].as_str().unwrap_or_default() {
                        "text" => {
                            open.insert(index, BlockAcc::default());
                        }
                        "tool_use" => {
                            open.insert(
                                index,
                                BlockAcc {
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
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_creation,
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
