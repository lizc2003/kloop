//! OpenAI-compat chat/completions adapter: translates the canonical history
//! to chat messages and the tool_calls delta stream back into StreamEvents.

use std::collections::BTreeMap;

use serde_json::Value;
use serde_json::json;

use super::ProviderFailure;
use super::SseFrames;
use super::StreamCompletion;
use super::StreamSink;
use super::is_overflow_message;
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::Message;
use kloop_protocol::OutputLimitKind;
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
fn split_blocks_for_chat(blocks: &[ContentBlock]) -> Result<(String, Vec<Value>), ProviderFailure> {
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
            ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                return Err(protocol(
                    "received reasoning that was not removed by the authorized request view",
                ));
            }
            ContentBlock::ToolUse { .. } | ContentBlock::ToolResult { .. } => {}
        }
    }
    Ok((text, images))
}

/// Translate canonical (Anthropic-shaped) history into chat/completions messages.
pub(super) fn to_openai_messages(
    system: &str,
    messages: &[Message],
) -> Result<Vec<Value>, ProviderFailure> {
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
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                            return Err(protocol(
                                "received reasoning that was not removed by the authorized request view",
                            ));
                        }
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
                                ToolResultContent::Blocks(blocks) => split_blocks_for_chat(blocks)?,
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
                        ContentBlock::Thinking { .. } | ContentBlock::RedactedThinking { .. } => {
                            return Err(protocol(
                                "received reasoning that was not removed by the authorized request view",
                            ));
                        }
                        ContentBlock::ToolUse { .. } => {}
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
    Ok(out)
}

#[derive(Default)]
struct CallAcc {
    id: Option<String>,
    name: Option<String>,
    args: String,
}

fn protocol(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure::protocol(format!("openai-compat {}", message.into()))
}

fn lock_identity(
    slot: &mut Option<String>,
    value: &Value,
    field: &str,
) -> Result<(), ProviderFailure> {
    if value.is_null() {
        return Ok(());
    }
    let value = value
        .as_str()
        .ok_or_else(|| protocol(format!("tool {field} was not a string")))?;
    if value.is_empty() {
        return Ok(());
    }
    match slot {
        Some(current) if current != value => {
            Err(protocol(format!("tool {field} changed during streaming")))
        }
        Some(_) => Ok(()),
        None => {
            *slot = Some(value.to_string());
            Ok(())
        }
    }
}

fn map_finish_reason(reason: &str) -> Result<AssistantOutcome, ProviderFailure> {
    match reason {
        "stop" | "end_turn" => Ok(AssistantOutcome::EndTurn),
        "tool_calls" | "function_call" | "tool_use" => Ok(AssistantOutcome::ToolUse),
        "length" | "max_tokens" => Ok(AssistantOutcome::OutputLimit(
            OutputLimitKind::MaxOutputTokens,
        )),
        "content_filter" => Ok(AssistantOutcome::Filtered),
        "refusal" => Ok(AssistantOutcome::Refused),
        _ => Err(protocol("returned an unknown finish reason")),
    }
}

fn parse_usage(value: &Value) -> Result<Usage, ProviderFailure> {
    let prompt = value["prompt_tokens"]
        .as_u64()
        .ok_or_else(|| protocol("usage missing prompt_tokens"))?;
    let output = value["completion_tokens"]
        .as_u64()
        .ok_or_else(|| protocol("usage missing completion_tokens"))?;
    let cached = match value["prompt_tokens_details"]["cached_tokens"].as_u64() {
        Some(value) => value,
        None if value["prompt_tokens_details"].is_null() => 0,
        None => return Err(protocol("usage cached_tokens was not an integer")),
    };
    if cached > prompt {
        return Err(protocol("usage cached_tokens exceeded prompt_tokens"));
    }
    Ok(Usage {
        input_tokens: prompt - cached,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    })
}

fn semantic_string<'a>(
    object: &'a serde_json::Map<String, Value>,
    field: &str,
) -> Result<Option<&'a str>, ProviderFailure> {
    match object.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value)),
        Some(_) => Err(protocol(format!("delta {field} was not a string"))),
    }
}

#[allow(clippy::too_many_arguments)]
async fn apply_choice_payload(
    payload: &serde_json::Map<String, Value>,
    text: &mut String,
    thinking: &mut String,
    display_text: &mut String,
    refusal_seen: &mut bool,
    calls: &mut BTreeMap<u64, CallAcc>,
    sink: &StreamSink,
) -> Result<bool, ProviderFailure> {
    const ALLOWED_FIELDS: &[&str] = &[
        "role",
        "content",
        "reasoning_content",
        "reasoning",
        "refusal",
        "tool_calls",
    ];
    if payload
        .keys()
        .any(|field| !ALLOWED_FIELDS.contains(&field.as_str()))
    {
        return Err(protocol("returned an unknown semantic delta field"));
    }
    if let Some(role) = semantic_string(payload, "role")?
        && role != "assistant"
    {
        return Err(protocol("delta role was not assistant"));
    }

    let mut semantic = false;
    let reasoning_content = semantic_string(payload, "reasoning_content")?;
    let reasoning = semantic_string(payload, "reasoning")?;
    let reasoning_piece = match (reasoning_content, reasoning) {
        (Some(left), Some(right)) if left != right => {
            return Err(protocol("reasoning aliases conflicted"));
        }
        (Some(value), _) | (_, Some(value)) => Some(value),
        (None, None) => None,
    };
    if let Some(piece) = reasoning_piece
        && !piece.is_empty()
    {
        semantic = true;
        thinking.push_str(piece);
        sink.thinking_delta(piece.to_string()).await?;
    }
    if let Some(piece) = semantic_string(payload, "content")?
        && !piece.is_empty()
    {
        semantic = true;
        text.push_str(piece);
        display_text.push_str(piece);
        sink.text_delta(piece.to_string()).await?;
    }
    if let Some(piece) = semantic_string(payload, "refusal")?
        && !piece.is_empty()
    {
        semantic = true;
        *refusal_seen = true;
        display_text.push_str(piece);
        sink.text_delta(piece.to_string()).await?;
    }

    if let Some(tool_calls) = payload.get("tool_calls") {
        let Some(tool_calls) = tool_calls.as_array() else {
            if tool_calls.is_null() {
                return Ok(semantic);
            }
            return Err(protocol("delta tool_calls was not an array"));
        };
        for call in tool_calls {
            semantic = true;
            let call_object = call
                .as_object()
                .ok_or_else(|| protocol("tool call was not an object"))?;
            let index = call["index"]
                .as_u64()
                .ok_or_else(|| protocol("tool call was missing its wire index"))?;
            if let Some(kind) = call.get("type")
                && kind.as_str() != Some("function")
            {
                return Err(protocol("tool call type was not function"));
            }
            let acc = calls.entry(index).or_default();
            lock_identity(&mut acc.id, &call["id"], "id")?;
            let function = call.get("function");
            if let Some(function) = function {
                let function = function
                    .as_object()
                    .ok_or_else(|| protocol("tool call function was not an object"))?;
                lock_identity(
                    &mut acc.name,
                    function.get("name").unwrap_or(&Value::Null),
                    "name",
                )?;
                if let Some(arguments) = function.get("arguments") {
                    let arguments = arguments
                        .as_str()
                        .ok_or_else(|| protocol("tool arguments fragment was not a string"))?;
                    acc.args.push_str(arguments);
                }
                if function
                    .keys()
                    .any(|field| !matches!(field.as_str(), "name" | "arguments"))
                {
                    return Err(protocol("tool function contained an unknown field"));
                }
            }
            if call_object
                .keys()
                .any(|field| !matches!(field.as_str(), "index" | "id" | "type" | "function"))
            {
                return Err(protocol("tool call contained an unknown field"));
            }
        }
    }
    Ok(semantic)
}

pub(super) async fn stream(
    url: &str,
    cred: &crate::Credential,
    body: &Value,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let req = cred.apply(crate::http_client().post(url)).json(body);
    let resp = crate::send_checked(req, "openai-compat", url, cred.secret()).await?;

    let mut frames = SseFrames::new(resp.bytes_stream());
    let mut text = String::new();
    let mut display_text = String::new();
    let mut thinking = String::new();
    let mut refusal_seen = false;
    let mut calls = BTreeMap::new();
    let mut choice_index = None;
    let mut outcome = None;
    let mut usage = None;
    let mut transport_done = false;

    while let Some(frame) = frames.next().await? {
        if transport_done {
            return Err(protocol("SSE frame arrived after [DONE]"));
        }
        if !matches!(
            frame.event.as_deref(),
            None | Some("message") | Some("error")
        ) {
            return Err(protocol("returned an unknown SSE event name"));
        }
        if frame.data.trim() == "[DONE]" {
            transport_done = true;
            frames.stop();
            continue;
        }
        let value = crate::parse_sse_json("openai-compat", &frame.data)?;
        if !value["error"].is_null() {
            if is_overflow_message(&value["error"].to_string()) {
                return Err(ProviderFailure::context_overflow());
            }
            return Err(crate::stream_error(
                "openai-compat",
                crate::error_label(&value["error"]),
                crate::error_detail(&value["error"], cred.secret()),
            ));
        }

        // `include_usage` puts the authoritative report in the trailing
        // empty-choices frame, but a gateway may also attach a copy to the
        // `finish_reason` frame (observed on a GLM route: both frames carried
        // byte-identical usage). A later report therefore overwrites rather
        // than failing the stream.
        if let Some(raw_usage) = value.get("usage")
            && !raw_usage.is_null()
        {
            usage = Some(parse_usage(raw_usage)?);
        }

        let choices = value
            .get("choices")
            .and_then(Value::as_array)
            .ok_or_else(|| protocol("frame was missing choices"))?;
        if choices.is_empty() {
            if value.get("usage").is_none_or(Value::is_null) {
                return Err(protocol("empty choices frame did not contain usage"));
            }
            continue;
        }
        if choices.len() != 1 {
            return Err(protocol("returned more than one logical choice"));
        }
        let choice = &choices[0];
        let index = choice["index"]
            .as_u64()
            .ok_or_else(|| protocol("choice was missing its index"))?;
        match choice_index {
            Some(current) if current != index => {
                return Err(protocol("choice index changed during streaming"));
            }
            None => choice_index = Some(index),
            Some(_) => {}
        }

        let payload = match (choice.get("delta"), choice.get("message")) {
            (Some(Value::Object(delta)), None) => delta,
            (None, Some(Value::Object(message))) => message,
            (Some(Value::Object(_)), Some(Value::Object(_))) => {
                return Err(protocol("choice contained both delta and final message"));
            }
            _ => return Err(protocol("choice was missing a delta object")),
        };
        let semantic = apply_choice_payload(
            payload,
            &mut text,
            &mut thinking,
            &mut display_text,
            &mut refusal_seen,
            &mut calls,
            sink,
        )
        .await?;
        if outcome.is_some() && semantic {
            return Err(protocol("semantic delta arrived after finish reason"));
        }

        if let Some(reason) = choice.get("finish_reason")
            && !reason.is_null()
        {
            let reason = reason
                .as_str()
                .ok_or_else(|| protocol("finish_reason was not a string"))?;
            if outcome.is_some() {
                return Err(protocol("received duplicate finish_reason"));
            }
            let mapped = map_finish_reason(reason)?;
            outcome = Some(if refusal_seen && mapped == AssistantOutcome::EndTurn {
                AssistantOutcome::Refused
            } else {
                mapped
            });
        }

        if outcome.is_some() {
            frames.stop();
        }
    }

    let Some(outcome) = outcome else {
        return Err(ProviderFailure::incomplete_protocol(
            "openai-compat stream ended before finish_reason",
        ));
    };

    let mut blocks = Vec::new();
    if !thinking.is_empty() {
        blocks.push(AssistantBlock::Thinking {
            thinking,
            signature: String::new(),
        });
    }
    if !display_text.is_empty() {
        blocks.push(AssistantBlock::Text { text: display_text });
    }
    for (_, acc) in calls {
        let id = acc
            .id
            .ok_or_else(|| protocol("tool call completed without an id"))?;
        let name = acc
            .name
            .ok_or_else(|| protocol("tool call completed without a name"))?;
        blocks.push(crate::tool_use_block(id, name, &acc.args));
    }
    if refusal_seen
        && blocks.iter().any(|block| {
            matches!(
                block,
                AssistantBlock::ToolUse { .. } | AssistantBlock::InvalidToolUse { .. }
            )
        })
    {
        return Err(protocol("response combined refusal with tool calls"));
    }
    crate::validate_assistant_output("openai-compat", &outcome, &blocks)?;
    for block in blocks {
        sink.block_done(block).await?;
    }
    Ok(StreamCompletion::new(outcome, usage))
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
                provider_provenance: None,
                injected: None,
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
                provider_provenance: None,
                injected: None,
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "boom".into(),
                    is_error: true,
                }],
                provider_provenance: None,
                injected: None,
            },
        ];
        assert_eq!(
            to_openai_messages("be brief", &messages).unwrap(),
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

    /// Chat translation is not a second reasoning projection owner. Any
    /// reasoning left after the core request view fails closed.
    #[test]
    fn thinking_blocks_are_rejected_by_outbound_translation() {
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
                provider_provenance: None,
                injected: None,
            },
        ];
        let error = to_openai_messages("s", &messages).unwrap_err();
        assert_eq!(error.kind(), &super::super::ProviderFailureKind::Protocol);
        assert!(error.to_string().contains("authorized request view"));
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
        let out = to_openai_messages("s", &messages).unwrap();
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
            provider_provenance: None,
            injected: None,
        }];
        assert_eq!(
            to_openai_messages("s", &messages).unwrap(),
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
            to_openai_messages("s", &messages).unwrap(),
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
