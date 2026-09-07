//! Anthropic Messages native SSE adapter.

use std::collections::HashMap;
use std::collections::HashSet;

use serde_json::Value;
use serde_json::json;

use super::GuardedBody;
use super::ProviderFailure;
use super::StreamCompletion;
use super::StreamSink;
use super::is_overflow_message;
use super::sse::SseFrame;
use super::sse::SseParser;
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::IncompleteReason;
use kloop_protocol::Message;
use kloop_protocol::OutputLimitKind;
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
    if cache && let Some(last) = items.last_mut() {
        last["cache_control"] = json!({"type": "ephemeral"});
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
    let mut value = Value::Array(
        messages
            .iter()
            .map(|message| {
                json!({
                    "role": message.role,
                    "content": message.content,
                })
            })
            .collect(),
    );
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
        initial_input: Value,
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

fn protocol(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure::protocol(format!("anthropic {}", message.into()))
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderFailure> {
    value
        .as_str()
        .ok_or_else(|| protocol(format!("missing or invalid {field}")))
}

fn required_u64(value: &Value, field: &str) -> Result<u64, ProviderFailure> {
    value
        .as_u64()
        .ok_or_else(|| protocol(format!("missing or invalid {field}")))
}

fn event_type<'a>(frame: &SseFrame, value: &'a Value) -> Result<&'a str, ProviderFailure> {
    let kind = required_str(&value["type"], "event type")?;
    if let Some(wire) = frame.event.as_deref()
        && wire != kind
    {
        return Err(protocol("SSE event name did not match payload type"));
    }
    Ok(kind)
}

fn map_stop_reason(reason: &str) -> Result<AssistantOutcome, ProviderFailure> {
    match reason {
        "end_turn" | "stop_sequence" => Ok(AssistantOutcome::EndTurn),
        "tool_use" => Ok(AssistantOutcome::ToolUse),
        "max_tokens" => Ok(AssistantOutcome::OutputLimit(
            OutputLimitKind::MaxOutputTokens,
        )),
        "model_context_window_exceeded" => Ok(AssistantOutcome::OutputLimit(
            OutputLimitKind::ModelContextWindow,
        )),
        "refusal" => Ok(AssistantOutcome::Refused),
        "pause_turn" => Ok(AssistantOutcome::Incomplete(IncompleteReason::PauseTurn)),
        _ => Err(protocol("returned an unknown stop reason")),
    }
}

fn start_block(content: &Value) -> Result<BlockAcc, ProviderFailure> {
    let kind = required_str(&content["type"], "content block type")?;
    match kind {
        "text" => Ok(BlockAcc::Text {
            text: required_str(&content["text"], "text block text")?.to_string(),
        }),
        "tool_use" => {
            let id = required_str(&content["id"], "tool id")?;
            let name = required_str(&content["name"], "tool name")?;
            if id.is_empty() || name.is_empty() {
                return Err(protocol("tool id and name must be non-empty"));
            }
            let initial_input = content
                .get("input")
                .filter(|input| input.is_object())
                .cloned()
                .ok_or_else(|| protocol("tool start input must be an object"))?;
            Ok(BlockAcc::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                initial_input,
                json: String::new(),
            })
        }
        "thinking" => Ok(BlockAcc::Thinking {
            thinking: required_str(&content["thinking"], "thinking text")?.to_string(),
            signature: required_str(&content["signature"], "thinking signature")?.to_string(),
        }),
        "redacted_thinking" => Ok(BlockAcc::RedactedThinking {
            data: required_str(&content["data"], "redacted thinking data")?.to_string(),
        }),
        _ => Err(protocol("returned an unsupported content block type")),
    }
}

fn finish_block(acc: BlockAcc) -> Result<AssistantBlock, ProviderFailure> {
    match acc {
        BlockAcc::Text { text } => Ok(AssistantBlock::Text { text }),
        BlockAcc::ToolUse {
            id,
            name,
            initial_input,
            json,
        } => {
            let input = if json.trim().is_empty() {
                initial_input
            } else {
                if initial_input
                    .as_object()
                    .is_some_and(|object| !object.is_empty())
                {
                    return Err(protocol(
                        "tool input appeared in both start and delta events",
                    ));
                }
                crate::parse_tool_input("anthropic", &name, &json)?
            };
            Ok(AssistantBlock::ToolUse { id, name, input })
        }
        BlockAcc::Thinking {
            thinking,
            signature,
        } => Ok(AssistantBlock::Thinking {
            thinking,
            signature,
        }),
        BlockAcc::RedactedThinking { data } => Ok(AssistantBlock::RedactedThinking { data }),
    }
}

/// The gateway-facing session header. Anthropic's own endpoint has no
/// prompt-cache routing knob — its cache is prefix-keyed and workspace-scoped,
/// so affinity is the platform's problem, not ours. A gateway in the middle is
/// a different story: it needs a stable per-conversation id to route on, and
/// Claude Code's gateway protocol names this header for exactly that ("use it
/// to aggregate all requests from one session without parsing the body"), so
/// existing gateways already key off it. kloop follows that convention rather
/// than inventing a name nothing reads.
const SESSION_HEADER: &str = "x-claude-code-session-id";

pub(super) async fn stream(
    url: &str,
    key: &str,
    session: Option<&str>,
    body: &Value,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let mut req = crate::http_client()
        .post(url)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01");
    // A session id only has to be a safe filename, so it can hold bytes that do
    // not belong in a header: `HeaderValue` would accept a non-ASCII id as
    // obs-text rather than reject it, and obs-text is deprecated and handled
    // unevenly by proxies — precisely the hop this header exists for. Empty is
    // not a session either, and HTTP would accept that too. Both are dropped:
    // the header is a routing hint, so losing it costs cache affinity, while
    // sending one a gateway chokes on costs the request.
    if let Some(value) = session
        .filter(|id| !id.is_empty() && id.is_ascii())
        .and_then(|id| reqwest::header::HeaderValue::from_str(id).ok())
    {
        req = req.header(SESSION_HEADER, value);
    }
    let req = req.json(body);
    let resp = crate::send_checked(req, "anthropic", key).await?;

    let mut parser = SseParser::default();
    let mut byte_stream = GuardedBody::new(resp.bytes_stream());
    let mut started = false;
    let mut open: HashMap<u64, BlockAcc> = HashMap::new();
    let mut seen_indices = HashSet::new();
    let mut completed = Vec::new();
    let mut outcome: Option<AssistantOutcome> = None;
    let mut completion = None;
    let mut input_tokens = None;
    let mut output_tokens = None;
    let mut cache_read = 0;
    let mut cache_creation = 0;

    loop {
        let next = byte_stream.next().await?;
        let Some(chunk) = next else {
            break;
        };
        for frame in parser.feed(&chunk)? {
            let value = crate::parse_sse_json("anthropic", &frame.data)?;
            let event = event_type(&frame, &value)?;
            if completion.is_some() && event != "ping" {
                return Err(protocol("semantic event arrived after message_stop"));
            }
            match event {
                "message_start" => {
                    if started {
                        return Err(protocol("received duplicate message_start"));
                    }
                    if outcome.is_some() || !open.is_empty() || !seen_indices.is_empty() {
                        return Err(protocol("received message_start after message content"));
                    }
                    let usage = value["message"]["usage"]
                        .as_object()
                        .ok_or_else(|| protocol("message_start missing usage"))?;
                    input_tokens = Some(required_u64(
                        &value["message"]["usage"]["input_tokens"],
                        "input_tokens",
                    )?);
                    cache_read = usage
                        .get("cache_read_input_tokens")
                        .map(|value| required_u64(value, "cache_read_input_tokens"))
                        .transpose()?
                        .unwrap_or(0);
                    cache_creation = usage
                        .get("cache_creation_input_tokens")
                        .map(|value| required_u64(value, "cache_creation_input_tokens"))
                        .transpose()?
                        .unwrap_or(0);
                    started = true;
                }
                "content_block_start" => {
                    if !started {
                        return Err(protocol("content block arrived before message_start"));
                    }
                    if outcome.is_some() {
                        return Err(protocol("content block arrived after message_delta"));
                    }
                    let index = required_u64(&value["index"], "content block index")?;
                    if !seen_indices.insert(index) {
                        return Err(protocol("content block index was started more than once"));
                    }
                    let content = value["content_block"]
                        .as_object()
                        .ok_or_else(|| protocol("content_block_start missing content_block"))?;
                    let acc = start_block(&Value::Object(content.clone()))?;
                    let display_kind_open = open.values().any(|current| {
                        matches!(
                            (&acc, current),
                            (BlockAcc::Text { .. }, BlockAcc::Text { .. })
                                | (BlockAcc::Thinking { .. }, BlockAcc::Thinking { .. })
                        )
                    });
                    if display_kind_open {
                        return Err(protocol(
                            "concurrent display blocks had no canonical identity",
                        ));
                    }
                    open.insert(index, acc);
                }
                "content_block_delta" => {
                    if !started {
                        return Err(protocol("content delta arrived before message_start"));
                    }
                    if outcome.is_some() {
                        return Err(protocol("content delta arrived after message_delta"));
                    }
                    let index = required_u64(&value["index"], "content block index")?;
                    let acc = open
                        .get_mut(&index)
                        .ok_or_else(|| protocol("content delta referenced a non-open block"))?;
                    let delta = value["delta"]
                        .as_object()
                        .ok_or_else(|| protocol("content delta missing delta object"))?;
                    let delta = Value::Object(delta.clone());
                    let kind = required_str(&delta["type"], "content delta type")?;
                    match (kind, acc) {
                        ("text_delta", BlockAcc::Text { text }) => {
                            let piece = required_str(&delta["text"], "text delta text")?;
                            text.push_str(piece);
                            if !piece.is_empty() {
                                sink.text_delta(piece.to_string()).await?;
                            }
                        }
                        ("input_json_delta", BlockAcc::ToolUse { json, .. }) => {
                            json.push_str(required_str(
                                &delta["partial_json"],
                                "tool input JSON delta",
                            )?);
                        }
                        ("thinking_delta", BlockAcc::Thinking { thinking, .. }) => {
                            let piece = required_str(&delta["thinking"], "thinking delta text")?;
                            thinking.push_str(piece);
                            if !piece.is_empty() {
                                sink.thinking_delta(piece.to_string()).await?;
                            }
                        }
                        ("signature_delta", BlockAcc::Thinking { signature, .. }) => {
                            signature.push_str(required_str(
                                &delta["signature"],
                                "thinking signature delta",
                            )?);
                        }
                        _ => return Err(protocol("content delta type did not match its block")),
                    }
                }
                "content_block_stop" => {
                    if !started {
                        return Err(protocol("content block stop arrived before message_start"));
                    }
                    if outcome.is_some() {
                        return Err(protocol("content block stop arrived after message_delta"));
                    }
                    let index = required_u64(&value["index"], "content block index")?;
                    let acc = open.remove(&index).ok_or_else(|| {
                        protocol("content block stop referenced a non-open block")
                    })?;
                    let block = finish_block(acc)?;
                    if block.has_semantic_payload() {
                        completed.push(block.clone());
                        sink.block_done(block).await?;
                    }
                }
                "message_delta" => {
                    if !started {
                        return Err(protocol("message_delta arrived before message_start"));
                    }
                    if outcome.is_some() {
                        return Err(protocol("received duplicate message_delta stop reason"));
                    }
                    if !open.is_empty() {
                        return Err(protocol(
                            "message_delta arrived with unfinished content blocks",
                        ));
                    }
                    let reason =
                        required_str(&value["delta"]["stop_reason"], "message stop reason")?;
                    outcome = Some(map_stop_reason(reason)?);
                    output_tokens = Some(required_u64(
                        &value["usage"]["output_tokens"],
                        "output_tokens",
                    )?);
                }
                "message_stop" => {
                    if !started {
                        return Err(protocol("message_stop arrived before message_start"));
                    }
                    if !open.is_empty() {
                        return Err(protocol(
                            "message_stop arrived with unfinished content blocks",
                        ));
                    }
                    let outcome = outcome
                        .take()
                        .ok_or_else(|| protocol("message_stop arrived before message_delta"))?;
                    crate::validate_assistant_output("anthropic", &outcome, &completed)?;
                    let usage = Some(Usage {
                        input_tokens: input_tokens
                            .ok_or_else(|| protocol("missing final input token usage"))?,
                        output_tokens: output_tokens
                            .ok_or_else(|| protocol("missing final output token usage"))?,
                        cache_read_input_tokens: cache_read,
                        cache_creation_input_tokens: cache_creation,
                    });
                    completion = Some(StreamCompletion::new(outcome, usage));
                }
                "ping" => {}
                "error" => {
                    if is_overflow_message(&value["error"].to_string()) {
                        return Err(ProviderFailure::context_overflow());
                    }
                    return Err(crate::stream_error(
                        "anthropic",
                        crate::error_label(&value["error"]),
                        crate::error_detail(&value["error"], key),
                    ));
                }
                _ => return Err(protocol("returned an unknown semantic event")),
            }
        }
        if let Some(completion) = completion.take() {
            return Ok(completion);
        }
    }
    parser.finish()?;
    Err(ProviderFailure::incomplete_protocol(
        "anthropic stream ended before message_stop",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::ContentBlock;
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
            provider_provenance: None,
            injected: None,
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

    /// A tool that read an image (slice 2) lands its image natively inside
    /// `tool_result.content` as an array — Anthropic's own shape, so the raw
    /// serialization is already correct and no adapter work is needed.
    #[test]
    fn tool_result_image_serializes_natively() {
        let messages = vec![Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: kloop_protocol::ToolResultContent::Blocks(vec![ContentBlock::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
            }]),
            is_error: false,
        }])];
        assert_eq!(
            messages_value(&messages, false),
            json!([{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGk="}},
                    ],
                }],
            }])
        );
    }
}
