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
use crate::types::ContentBlock;
use crate::types::OverflowError;
use crate::types::StreamEvent;
use crate::types::Usage;

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
