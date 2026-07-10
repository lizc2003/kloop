//! OpenAI-compat chat/completions adapter: translates the canonical history
//! to chat messages and the tool_calls delta stream back into StreamEvents.

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
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::Role;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;

/// Translate canonical (Anthropic-shaped) history into chat/completions messages.
pub(super) fn to_openai_messages(system: &str, messages: &[Message]) -> Vec<Value> {
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
struct CallAcc {
    id: String,
    name: String,
    args: String,
}

pub(super) async fn stream(
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
    let mut calls: Vec<CallAcc> = Vec::new();
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
            // empty choices. OpenAI reports cached prompt tokens INSIDE
            // prompt_tokens (unlike Anthropic), so they are subtracted out to
            // keep input_tokens = uncached remainder on both rails.
            if v["usage"].is_object() {
                let prompt = v["usage"]["prompt_tokens"].as_u64().unwrap_or(0);
                let cached = v["usage"]["prompt_tokens_details"]["cached_tokens"]
                    .as_u64()
                    .unwrap_or(0);
                usage = Some(Usage {
                    input_tokens: prompt.saturating_sub(cached),
                    output_tokens: v["usage"]["completion_tokens"].as_u64().unwrap_or(0),
                    cache_read_input_tokens: cached,
                    cache_creation_input_tokens: 0,
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
                        calls.push(CallAcc::default());
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Whole-object contract for the history translation: system first,
    /// assistant text + tool_calls with serialized arguments, tool results
    /// (error-prefixed when failed) BEFORE any trailing user text.
    #[test]
    fn translates_canonical_history_to_chat_messages() {
        let messages = vec![
            Message::user_text("do the thing"),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Text {
                        text: "on it".into(),
                    },
                    ContentBlock::ToolUse {
                        id: "t1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "t1".into(),
                        content: "file.txt".into(),
                        is_error: false,
                    },
                    ContentBlock::Text {
                        text: "and hurry".into(),
                    },
                ],
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "boom".into(),
                    is_error: true,
                }],
            },
        ];
        assert_eq!(
            to_openai_messages("be brief", &messages),
            vec![
                json!({"role": "system", "content": "be brief"}),
                json!({"role": "user", "content": "do the thing"}),
                json!({
                    "role": "assistant",
                    "content": "on it",
                    "tool_calls": [{
                        "id": "t1",
                        "type": "function",
                        "function": {"name": "bash", "arguments": "{\"command\":\"ls\"}"},
                    }],
                }),
                json!({"role": "tool", "tool_call_id": "t1", "content": "file.txt"}),
                json!({"role": "user", "content": "and hurry"}),
                json!({"role": "tool", "tool_call_id": "t2", "content": "[error] boom"}),
            ]
        );
    }

    /// A tool-calls-only assistant message must serialize content as null,
    /// not an empty string.
    #[test]
    fn assistant_without_text_has_null_content() {
        let messages = vec![Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({}),
        }])];
        let out = to_openai_messages("s", &messages);
        assert!(out[1]["content"].is_null());
    }
}
