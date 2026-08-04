//! OpenAI-compat chat/completions adapter: translates the canonical history
//! to chat messages and the tool_calls delta stream back into StreamEvents.

use serde_json::json;
use serde_json::Value;

use super::is_overflow_message;
use super::sse::SseParser;
use super::GuardedBody;
use super::ProviderFailure;
use super::StreamCompletion;
use super::StreamSink;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::Message;
use kloop_protocol::Role;
use kloop_protocol::ToolResultContent;
use kloop_protocol::Usage;

/// Left in the `tool` message when its image is relocated to a trailing user
/// message (the `tool` role cannot carry images). codex uses this exact text.
const IMAGE_RELOCATED_PLACEHOLDER: &str =
    "[tool output contains image data attached in the following message]";

/// One chat/completions `image_url` data-URL part from a canonical image source.
fn image_url_part(source: &ImageSource) -> Value {
    let ImageSource::Base64 { media_type, data } = source;
    json!({
        "type": "image_url",
        "image_url": {
            "url": format!("data:{media_type};base64,{data}"),
            "detail": "auto",
        },
    })
}

/// Split a tool_result's block array into (joined text, image_url parts): text
/// blocks are concatenated for the `tool` message; images become parts for the
/// relocated user message.
fn split_blocks_for_chat(blocks: &[ContentBlock]) -> (String, Vec<Value>) {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text { text: t } => {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
            ContentBlock::Image { source } => images.push(image_url_part(source)),
            _ => {}
        }
    }
    (text, images)
}

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
                        // Reasoning is stripped on the way out: chat/completions
                        // has no standard replay field, and providers that
                        // accept one (deepseek's reasoning_content) tolerate
                        // its absence.
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {}
                        // Assistant messages never carry images (top-level
                        // images ride on user messages).
                        ContentBlock::ToolResult { .. } | ContentBlock::Image { .. } => {}
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
                // message, so they go first; trailing text/images become a
                // user msg. Images embedded in a tool_result can't ride the
                // `tool` role (chat/completions forbids it), so they are
                // RELOCATED to their own trailing user message — pushed after
                // every tool message so the tool-message-adjacency rule holds.
                let mut text = String::new();
                let mut images = Vec::new();
                let mut relocated: Vec<Value> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } => {
                            let (mut tool_text, image_parts) = match content {
                                ToolResultContent::Text(s) => (s.clone(), Vec::new()),
                                ToolResultContent::Blocks(blocks) => split_blocks_for_chat(blocks),
                            };
                            // The tool message keeps the text and points at the
                            // relocated image so the model connects the two.
                            if !image_parts.is_empty() {
                                if !tool_text.is_empty() {
                                    tool_text.push('\n');
                                }
                                tool_text.push_str(IMAGE_RELOCATED_PLACEHOLDER);
                            }
                            let tool_text = if *is_error {
                                format!("[error] {tool_text}")
                            } else {
                                tool_text
                            };
                            out.push(json!({
                                "role": "tool",
                                "tool_call_id": tool_use_id,
                                "content": tool_text,
                            }));
                            if !image_parts.is_empty() {
                                let mut parts = vec![json!({
                                    "type": "text",
                                    "text": format!("Tool output for call_id {tool_use_id}:"),
                                })];
                                parts.extend(image_parts);
                                relocated.push(json!({"role": "user", "content": parts}));
                            }
                        }
                        ContentBlock::Text { text: t } => text.push_str(t),
                        // Chat/completions carries images as a data-URL part.
                        // detail=auto matches the OpenAI default (Anthropic has
                        // no detail; kloop does not expose it yet).
                        ContentBlock::Image { source } => images.push(image_url_part(source)),
                        ContentBlock::Thinking { .. }
                        | ContentBlock::RedactedThinking { .. }
                        | ContentBlock::ToolUse { .. } => {}
                    }
                }
                // With images the content is an array of parts (text first,
                // then images); without, a plain string keeps the common case
                // byte-identical to the pre-image wire form.
                if images.is_empty() {
                    if !text.is_empty() {
                        out.push(json!({"role": "user", "content": text}));
                    }
                } else {
                    let mut parts = Vec::new();
                    if !text.is_empty() {
                        parts.push(json!({"type": "text", "text": text}));
                    }
                    parts.extend(images);
                    out.push(json!({"role": "user", "content": parts}));
                }
                // Relocated tool-output images trail all tool messages (and the
                // main user message), never interleaving with them.
                out.extend(relocated);
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
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let req = crate::http_client().post(url).bearer_auth(key).json(body);
    let resp = crate::send_checked(req, "openai-compat", key).await?;

    let mut parser = SseParser::default();
    let mut byte_stream = GuardedBody::new(resp.bytes_stream());
    let mut text = String::new();
    let mut thinking = String::new();
    let mut calls: Vec<CallAcc> = Vec::new();
    let mut stop_reason: Option<String> = None;
    let mut usage: Option<Usage> = None;

    'outer: while let Some(chunk) = byte_stream.next().await? {
        for frame in parser.feed(&chunk)? {
            if frame.data.trim() == "[DONE]" {
                break 'outer;
            }
            let v = crate::parse_sse_json("openai-compat", &frame.data)?;
            if !v["error"].is_null() {
                if is_overflow_message(&v["error"].to_string()) {
                    return Err(ProviderFailure::context_overflow());
                }
                return Err(ProviderFailure::protocol(format!(
                    "openai-compat stream error: {}",
                    v["error"]
                )));
            }
            // With include_usage the final chunk carries usage and empty
            // choices. OpenAI reports cached prompt tokens INSIDE
            // prompt_tokens, so subtract them to keep the canonical split.
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
            // Reasoning models stream their thinking as reasoning_content
            // (deepseek-style) or reasoning; either becomes a Thinking block
            // with no signature (chat/completions has no replay blob).
            let reasoning = delta["reasoning_content"]
                .as_str()
                .or_else(|| delta["reasoning"].as_str());
            if let Some(piece) = reasoning {
                if !piece.is_empty() {
                    thinking.push_str(piece);
                    sink.thinking_delta(piece.into()).await?;
                }
            }
            if let Some(piece) = delta["content"].as_str() {
                if !piece.is_empty() {
                    text.push_str(piece);
                    sink.text_delta(piece.into()).await?;
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
            if let Some(reason) = v["choices"][0]["finish_reason"].as_str() {
                stop_reason = Some(reason.to_string());
            }
        }
        // finish_reason is the semantic terminal. Process every frame already
        // coalesced in this body chunk (which commonly includes usage/[DONE]),
        // but never wait on a separate optional transport-tail chunk.
        if stop_reason.is_some() {
            break;
        }
    }

    if stop_reason.is_none() {
        return Err(ProviderFailure::incomplete_protocol(
            "openai-compat stream ended before finish_reason",
        ));
    }

    let mut parsed_calls = Vec::with_capacity(calls.len());
    for acc in calls {
        let input = crate::parse_tool_input("openai-compat", &acc.name, &acc.args)?;
        parsed_calls.push(ContentBlock::ToolUse {
            id: acc.id,
            name: acc.name,
            input,
        });
    }

    // Thinking precedes the answer on the wire, so it finalizes first too.
    if !thinking.is_empty() {
        sink.block_done(ContentBlock::Thinking {
            thinking,
            signature: String::new(),
        })
        .await?;
    }
    if !text.is_empty() {
        sink.block_done(ContentBlock::Text { text }).await?;
    }
    for block in parsed_calls {
        sink.block_done(block).await?;
    }
    Ok(StreamCompletion::new(stop_reason, usage))
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

    /// Thinking never goes back out on the chat wire: no standard field
    /// exists, and stray reasoning text would corrupt the assistant content.
    #[test]
    fn thinking_blocks_are_stripped_from_outbound_history() {
        let messages = vec![
            Message::assistant(vec![
                ContentBlock::Thinking {
                    thinking: "pondering".into(),
                    signature: String::new(),
                },
                ContentBlock::Text { text: "hi".into() },
            ]),
            Message {
                role: Role::User,
                content: vec![ContentBlock::RedactedThinking { data: "d".into() }],
            },
        ];
        assert_eq!(
            to_openai_messages("s", &messages),
            vec![
                json!({"role": "system", "content": "s"}),
                json!({"role": "assistant", "content": "hi"}),
            ],
            "thinking stripped; a message left empty by stripping sends nothing"
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

    /// A user message with an image becomes a content-parts array: the text
    /// part first, then an image_url whose url is a base64 data URL with
    /// detail=auto (the OpenAI default; Anthropic has no detail field).
    #[test]
    fn user_image_becomes_image_url_data_url() {
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
            to_openai_messages("s", &messages),
            vec![
                json!({"role": "system", "content": "s"}),
                json!({
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "what is this"},
                        {"type": "image_url", "image_url": {
                            "url": "data:image/png;base64,aGk=",
                            "detail": "auto",
                        }},
                    ],
                }),
            ]
        );
    }

    /// A tool that returned an image (slice 2): the `tool` role cannot carry
    /// images, so the image is RELOCATED to a trailing user message. The tool
    /// message keeps the text plus a placeholder pointing at it; the relocated
    /// user message leads with a `Tool output for call_id …:` label then the
    /// image_url data URL. Critically, the relocated user message comes AFTER
    /// the tool message (OpenAI rejects a user message between tool_calls and
    /// their tool results).
    #[test]
    fn tool_result_image_relocates_to_trailing_user_message() {
        let messages = vec![
            Message::assistant(vec![ContentBlock::ToolUse {
                id: "c1".into(),
                name: "read_file".into(),
                input: json!({"path": "logo.png"}),
            }]),
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "c1".into(),
                content: ToolResultContent::Blocks(vec![ContentBlock::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "aGk=".into(),
                    },
                }]),
                is_error: false,
            }]),
        ];
        assert_eq!(
            to_openai_messages("s", &messages),
            vec![
                json!({"role": "system", "content": "s"}),
                json!({
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "c1",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"logo.png\"}"},
                    }],
                }),
                json!({
                    "role": "tool",
                    "tool_call_id": "c1",
                    "content": IMAGE_RELOCATED_PLACEHOLDER,
                }),
                json!({
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "Tool output for call_id c1:"},
                        {"type": "image_url", "image_url": {
                            "url": "data:image/png;base64,aGk=",
                            "detail": "auto",
                        }},
                    ],
                }),
            ]
        );
    }
}
