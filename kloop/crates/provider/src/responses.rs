//! OpenAI Responses API adapter: translates the canonical history to input
//! items and the Responses SSE event family back into StreamEvents.
//!
//! The exchange is stateless (`store: false`): every request resends the
//! whole history, and reasoning survives across requests only through the
//! `encrypted_content` blob the server hands back when asked via
//! `include: ["reasoning.encrypted_content"]`. That blob rides in
//! `Thinking.signature`, and reasoning items MUST be replayed next to the
//! function calls they preceded — gpt-5-era models reject a function_call
//! whose reasoning item is missing.

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use futures::StreamExt;
use serde_json::json;
use serde_json::Value;
use tokio::sync::mpsc;

use super::is_overflow_message;
use super::sse::SseParser;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::Role;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolResultContent;
use kloop_protocol::Usage;

/// A canonical image source as a Responses `input_image` content item: a
/// data-URL image_url with detail=auto. Used for both top-level user images and
/// images embedded in a tool_result's function_call_output.
fn input_image_item(source: &ImageSource) -> Value {
    let ImageSource::Base64 { media_type, data } = source;
    json!({
        "type": "input_image",
        "image_url": format!("data:{media_type};base64,{data}"),
        "detail": "auto",
    })
}

/// A tool_result block array as function_call_output `content_items`: text
/// blocks become `input_text`, images become `input_image`. An error result
/// prefixes the leading text so the model sees the failure marker.
fn blocks_to_output_items(blocks: &[ContentBlock], is_error: bool) -> Vec<Value> {
    let mut text = String::new();
    let mut items = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::Image { source } => items.push(input_image_item(source)),
            _ => {}
        }
    }
    let text = if is_error {
        format!("[error] {text}")
    } else {
        text
    };
    if !text.is_empty() {
        items.insert(0, json!({"type": "input_text", "text": text}));
    }
    items
}

/// Translate canonical (Anthropic-shaped) history into Responses input items.
pub(super) fn to_input_items(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for msg in messages {
        match msg.role {
            Role::Assistant => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => out.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        })),
                        ContentBlock::Thinking {
                            thinking,
                            signature,
                        } => {
                            // Without the encrypted blob the item cannot be
                            // verified server-side (e.g. blocks recorded on
                            // another rail), so it is dropped rather than
                            // rejected.
                            if signature.is_empty() {
                                continue;
                            }
                            let summary: Vec<Value> = if thinking.is_empty() {
                                Vec::new()
                            } else {
                                vec![json!({"type": "summary_text", "text": thinking})]
                            };
                            out.push(json!({
                                "type": "reasoning",
                                "summary": summary,
                                "encrypted_content": signature,
                            }));
                        }
                        // Anthropic-only shape; nothing to replay here.
                        ContentBlock::RedactedThinking { .. } => {}
                        ContentBlock::ToolUse { id, name, input } => out.push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_default(),
                        })),
                        // Assistant messages never carry images.
                        ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
                    }
                }
            }
            Role::User => {
                // Tool outputs must directly follow their calls; trailing
                // text/images become a user message item.
                let mut text = String::new();
                let mut images = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            // The Responses API carries images natively in the
                            // function_call_output: `output` is `string |
                            // array`, so a tool that read an image needs no
                            // relocation (unlike chat/completions). Text stays a
                            // bare string to keep the common case unchanged.
                            let output = match content {
                                ToolResultContent::Text(s) => {
                                    if *is_error {
                                        Value::String(format!("[error] {s}"))
                                    } else {
                                        Value::String(s.clone())
                                    }
                                }
                                ToolResultContent::Blocks(blocks) => {
                                    Value::Array(blocks_to_output_items(blocks, *is_error))
                                }
                            };
                            out.push(json!({
                                "type": "function_call_output",
                                "call_id": tool_use_id,
                                "output": output,
                            }));
                        }
                        ContentBlock::Text { text: t } => text.push_str(t),
                        // Responses carries images as an input_image data URL.
                        // detail=auto matches the OpenAI default.
                        ContentBlock::Image { source } => images.push(input_image_item(source)),
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolUse { .. } => {}
                    }
                }
                if !text.is_empty() || !images.is_empty() {
                    let mut content = Vec::new();
                    if !text.is_empty() {
                        content.push(json!({"type": "input_text", "text": text}));
                    }
                    content.extend(images);
                    out.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": content,
                    }));
                }
            }
        }
    }
    out
}

/// One completed output item (from response.output_item.done) to a canonical
/// block. Unknown item kinds map to None and are skipped.
fn item_to_block(item: &Value) -> Option<ContentBlock> {
    match item["type"].as_str().unwrap_or_default() {
        "message" => {
            let text: String = item["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|part| part["type"] == "output_text")
                .map(|part| part["text"].as_str().unwrap_or(""))
                .collect();
            Some(ContentBlock::Text { text })
        }
        "function_call" => Some(ContentBlock::ToolUse {
            id: item["call_id"].as_str().unwrap_or_default().to_string(),
            name: item["name"].as_str().unwrap_or_default().to_string(),
            input: match item["arguments"].as_str().unwrap_or_default() {
                "" => json!({}),
                raw => serde_json::from_str(raw).unwrap_or(Value::String(raw.to_string())),
            },
        }),
        "reasoning" => Some(ContentBlock::Thinking {
            thinking: item["summary"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n"),
            signature: item["encrypted_content"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
        }),
        _ => None,
    }
}

fn usage_from(response: &Value) -> Option<Usage> {
    let usage = &response["usage"];
    if !usage.is_object() {
        return None;
    }
    // Like chat/completions, input_tokens includes the cached portion;
    // subtract it out so input_tokens is the uncached remainder on all rails.
    let input = usage["input_tokens"].as_u64().unwrap_or(0);
    let cached = usage["input_tokens_details"]["cached_tokens"]
        .as_u64()
        .unwrap_or(0);
    Some(Usage {
        input_tokens: input.saturating_sub(cached),
        output_tokens: usage["output_tokens"].as_u64().unwrap_or(0),
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    })
}

pub(super) async fn stream(
    url: &str,
    key: &str,
    body: &Value,
    tx: &mpsc::Sender<Result<StreamEvent>>,
) -> Result<()> {
    let req = crate::http_client().post(url).bearer_auth(key).json(body);
    let resp = crate::send_checked(req, "openai-responses", key).await?;

    let mut parser = SseParser::default();
    let mut byte_stream = resp.bytes_stream();
    while let Some(chunk) = byte_stream.next().await {
        let chunk = chunk?;
        for frame in parser.feed(&chunk) {
            let v: Value = match serde_json::from_str(&frame.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match v["type"].as_str().unwrap_or_default() {
                "response.output_text.delta" => {
                    let piece = v["delta"].as_str().unwrap_or_default();
                    if !piece.is_empty() {
                        let _ = tx.send(Ok(StreamEvent::TextDelta(piece.into()))).await;
                    }
                }
                // Summarized and raw reasoning text respectively; backends
                // send one or the other.
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let piece = v["delta"].as_str().unwrap_or_default();
                    if !piece.is_empty() {
                        let _ = tx.send(Ok(StreamEvent::ThinkingDelta(piece.into()))).await;
                    }
                }
                // Complete items arrive whole here; the deltas above are for
                // display only.
                "response.output_item.done" => {
                    if let Some(block) = item_to_block(&v["item"]) {
                        let _ = tx.send(Ok(StreamEvent::BlockDone(block))).await;
                    }
                }
                "response.completed" => {
                    let _ = tx
                        .send(Ok(StreamEvent::Done {
                            stop_reason: None,
                            usage: usage_from(&v["response"]),
                        }))
                        .await;
                    return Ok(());
                }
                // The output cap cut the response short: surface it like the
                // other rails' truncation stop_reasons so the agent's
                // continue-nudge applies.
                "response.incomplete" => {
                    let reason = v["response"]["incomplete_details"]["reason"]
                        .as_str()
                        .unwrap_or("incomplete");
                    let stop_reason = if reason == "max_output_tokens" {
                        Some("length".to_string())
                    } else {
                        Some(reason.to_string())
                    };
                    let _ = tx
                        .send(Ok(StreamEvent::Done {
                            stop_reason,
                            usage: usage_from(&v["response"]),
                        }))
                        .await;
                    return Ok(());
                }
                "response.failed" => {
                    let error = v["response"]["error"].to_string();
                    if is_overflow_message(&error) {
                        return Err(anyhow::Error::new(OverflowError));
                    }
                    bail!("openai-responses failed: {error}");
                }
                "error" => {
                    if is_overflow_message(&v.to_string()) {
                        return Err(anyhow::Error::new(OverflowError));
                    }
                    bail!("openai-responses stream error: {v}");
                }
                _ => {}
            }
        }
    }
    // Stream ended without a terminal event; the caller treats a closed
    // channel without Done as retryable.
    Err(anyhow!("openai-responses stream ended before completion"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Whole-object contract for the history translation: reasoning items
    /// replayed with their encrypted blob next to the calls they preceded,
    /// signature-less thinking dropped, tool outputs before trailing text.
    #[test]
    fn translates_canonical_history_to_input_items() {
        let messages = vec![
            Message::user_text("do the thing"),
            Message::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "planning".into(),
                    signature: "enc-blob".into(),
                },
                ContentBlock::Thinking {
                    thinking: "from another rail".into(),
                    signature: String::new(),
                },
                ContentBlock::Text {
                    text: "on it".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ]),
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "call_1".into(),
                        content: "boom".into(),
                        is_error: true,
                    },
                    ContentBlock::Text {
                        text: "and hurry".into(),
                    },
                ],
            },
        ];
        assert_eq!(
            to_input_items(&messages),
            vec![
                json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "do the thing"}],
                }),
                json!({
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "planning"}],
                    "encrypted_content": "enc-blob",
                }),
                json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "on it"}],
                }),
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "bash",
                    "arguments": "{\"command\":\"ls\"}",
                }),
                json!({
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": "[error] boom",
                }),
                json!({
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "and hurry"}],
                }),
            ]
        );
    }

    /// A user message with an image becomes a message item whose content
    /// holds an input_text part then an input_image data URL with detail=auto.
    #[test]
    fn user_image_becomes_input_image_data_url() {
        let messages = vec![Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "what is this".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/webp".into(),
                        data: "d2VicA==".into(),
                    },
                },
            ],
        }];
        assert_eq!(
            to_input_items(&messages),
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "what is this"},
                    {
                        "type": "input_image",
                        "image_url": "data:image/webp;base64,d2VicA==",
                        "detail": "auto",
                    },
                ],
            })]
        );
    }

    /// A tool that returned an image (slice 2): the Responses API carries it
    /// natively in the function_call_output — `output` becomes a content-items
    /// array with an `input_image` — so nothing is relocated (unlike
    /// chat/completions). A leading text block, if any, becomes an `input_text`.
    #[test]
    fn tool_result_image_rides_function_call_output_content_items() {
        let messages = vec![Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "c1".into(),
            content: ToolResultContent::Blocks(vec![
                ContentBlock::Text {
                    text: "here it is".into(),
                },
                ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    },
                },
            ]),
            is_error: false,
        }])];
        assert_eq!(
            to_input_items(&messages),
            vec![json!({
                "type": "function_call_output",
                "call_id": "c1",
                "output": [
                    {"type": "input_text", "text": "here it is"},
                    {
                        "type": "input_image",
                        "image_url": "data:image/png;base64,aGk=",
                        "detail": "auto",
                    },
                ],
            })]
        );
    }

    /// An empty-summary reasoning item (nothing displayable, blob only) still
    /// replays — the blob is the load-bearing part.
    #[test]
    fn blob_only_thinking_replays_with_empty_summary() {
        let messages = vec![Message::assistant(vec![ContentBlock::Thinking {
            thinking: String::new(),
            signature: "enc".into(),
        }])];
        assert_eq!(
            to_input_items(&messages),
            vec![json!({"type": "reasoning", "summary": [], "encrypted_content": "enc"})]
        );
    }
}
