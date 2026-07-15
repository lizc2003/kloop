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
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;

/// The `tools` request field, with its own breakpoint on the last tool. Tools
/// render before system, and the tool set is far more stable than kloop's
/// system prompt (which embeds the date and a git snapshot): when a restart
/// changes the system, the tools prefix still reads from cache.
pub(super) fn tools_value(tools: &[ToolDef], cache: bool) -> Value {
    let mut items: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            })
        })
        .collect();
    if cache {
        if let Some(last) = items.last_mut() {
            last["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    Value::Array(items)
}

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
        // The API rejects cache_control on thinking blocks, so the marker
        // goes on the last cacheable block instead.
        if let Some(block) = value
            .as_array_mut()
            .and_then(|msgs| msgs.last_mut())
            .and_then(|msg| msg["content"].as_array_mut())
            .and_then(|content| {
                content.iter_mut().rev().find(|block| {
                    !matches!(
                        block["type"].as_str(),
                        Some("thinking" | "redacted_thinking")
                    )
                })
            })
        {
            block["cache_control"] = json!({"type": "ephemeral"});
        }
    }
    value
}

/// One in-flight content block, accumulated across its deltas.
enum BlockAcc {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        json: String,
    },
    Thinking {
        thinking: String,
        signature: String,
    },
    /// Arrives complete in content_block_start; no deltas follow.
    RedactedThinking {
        data: String,
    },
}

pub(super) async fn stream(
    url: &str,
    key: &str,
    body: &Value,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let req = crate::http_client()
        .post(url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .json(body);
    let resp = crate::send_checked(req, "anthropic").await?;

    let mut parser = SseParser::default();
    let mut byte_stream = resp.bytes_stream();
    // Open blocks by stream index.
    let mut open: HashMap<u64, BlockAcc> = HashMap::new();
    let mut stop_reason: Option<String> = None;
    let mut input_tokens: Option<u64> = None;
    let mut output_tokens: Option<u64> = None;
    let mut cache_read: u64 = 0;
    let mut cache_creation: u64 = 0;

    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        for frame in parser.feed(&chunk) {
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
                    let str_field = |name: &str| cb[name].as_str().unwrap_or_default().to_string();
                    let acc = match cb["type"].as_str().unwrap_or_default() {
                        "text" => BlockAcc::Text {
                            text: String::new(),
                        },
                        "tool_use" => BlockAcc::ToolUse {
                            id: str_field("id"),
                            name: str_field("name"),
                            json: String::new(),
                        },
                        "thinking" => BlockAcc::Thinking {
                            thinking: str_field("thinking"),
                            signature: str_field("signature"),
                        },
                        "redacted_thinking" => BlockAcc::RedactedThinking {
                            data: str_field("data"),
                        },
                        // Unknown block kinds are never inserted, so their
                        // deltas fall through harmlessly below.
                        _ => continue,
                    };
                    open.insert(index, acc);
                }
                "content_block_delta" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let Some(acc) = open.get_mut(&index) else {
                        continue;
                    };
                    let delta = &v["delta"];
                    match (delta["type"].as_str().unwrap_or_default(), acc) {
                        ("text_delta", BlockAcc::Text { text }) => {
                            let piece = delta["text"].as_str().unwrap_or_default();
                            text.push_str(piece);
                            let _ = tx.send(Ok(StreamEvent::TextDelta(piece.into()))).await;
                        }
                        ("input_json_delta", BlockAcc::ToolUse { json, .. }) => {
                            json.push_str(delta["partial_json"].as_str().unwrap_or_default());
                        }
                        ("thinking_delta", BlockAcc::Thinking { thinking, .. }) => {
                            let piece = delta["thinking"].as_str().unwrap_or_default();
                            thinking.push_str(piece);
                            let _ = tx.send(Ok(StreamEvent::ThinkingDelta(piece.into()))).await;
                        }
                        ("signature_delta", BlockAcc::Thinking { signature, .. }) => {
                            signature.push_str(delta["signature"].as_str().unwrap_or_default());
                        }
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    let index = v["index"].as_u64().unwrap_or(0);
                    let Some(acc) = open.remove(&index) else {
                        continue;
                    };
                    let block = match acc {
                        BlockAcc::Text { text } => ContentBlock::Text { text },
                        BlockAcc::ToolUse { id, name, json } => ContentBlock::ToolUse {
                            id,
                            name,
                            input: if json.trim().is_empty() {
                                json!({})
                            } else {
                                serde_json::from_str(&json).unwrap_or_else(|_| json!({}))
                            },
                        },
                        BlockAcc::Thinking {
                            thinking,
                            signature,
                        } => ContentBlock::Thinking {
                            thinking,
                            signature,
                        },
                        BlockAcc::RedactedThinking { data } => {
                            ContentBlock::RedactedThinking { data }
                        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::ImageSource;
    use kloop_protocol::Role;

    /// The anthropic adapter serializes the protocol raw, so an image block
    /// reaches the wire as `{type:"image", source:{type:"base64", …}}`. The
    /// moving cache breakpoint may land on it (it is not a thinking block).
    #[test]
    fn image_block_serializes_and_takes_cache_breakpoint() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    },
                },
            ],
        }];
        assert_eq!(
            messages_value(&messages, true),
            json!([{
                "role": "user",
                "content": [
                    {"type": "text", "text": "what is this"},
                    {
                        "type": "image",
                        "source": {"type": "base64", "media_type": "image/png", "data": "aGk="},
                        "cache_control": {"type": "ephemeral"},
                    },
                ],
            }])
        );
    }
}
