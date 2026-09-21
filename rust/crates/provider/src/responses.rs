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

use std::collections::BTreeMap;
use std::collections::HashSet;

use serde_json::Value;
use serde_json::json;

use super::ProviderFailure;
use super::SseFrames;
use super::StreamCompletion;
use super::StreamSink;
use super::is_overflow_message;
use super::sse::SseFrame;
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::ImageSource;
use kloop_protocol::IncompleteReason;
use kloop_protocol::Message;
use kloop_protocol::OutputLimitKind;
use kloop_protocol::Role;
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MessagePartKind {
    OutputText,
    Refusal,
}

fn message_part_field(kind: MessagePartKind) -> &'static str {
    match kind {
        MessagePartKind::OutputText => "text",
        MessagePartKind::Refusal => "refusal",
    }
}

fn message_part_value<'a>(
    value: &'a Value,
    kind: MessagePartKind,
    field: &str,
) -> Result<&'a str, ProviderFailure> {
    required_str(&value[message_part_field(kind)], field)
}

/// The text a part opens with. `*_part.added` carries `""` on the reference
/// wire, and a gateway that drops the key entirely is saying the same thing:
/// an opening part holds nothing yet, and every character it ends up with
/// arrives as a delta. The `.done` twin keeps `required_str` — there the value
/// is compared against what the deltas built, and a missing key is a claim that
/// cannot be checked.
fn opening_part_text<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderFailure> {
    if value.is_null() {
        return Ok("");
    }
    required_str(value, field)
}

struct TextPart {
    kind: MessagePartKind,
    text: String,
    field_done: bool,
    part_closed: bool,
}

#[derive(Default)]
struct ReasoningPart {
    text: String,
    field_done: bool,
    part_closed: bool,
}

enum ItemKind {
    Message {
        parts: BTreeMap<u64, TextPart>,
    },
    Reasoning {
        summary: BTreeMap<u64, ReasoningPart>,
        content: BTreeMap<u64, ReasoningPart>,
    },
    FunctionCall {
        call_id: Option<String>,
        name: Option<String>,
        arguments: String,
        arguments_started: bool,
        arguments_done: bool,
    },
}

struct ItemState {
    kind: ItemKind,
}

type ItemKey = (u64, String);

fn protocol(message: impl Into<String>) -> ProviderFailure {
    ProviderFailure::protocol(format!("openai-responses {}", message.into()))
}

fn required_str<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderFailure> {
    value
        .as_str()
        .ok_or_else(|| protocol(format!("missing or invalid {field}")))
}

fn required_non_empty<'a>(value: &'a Value, field: &str) -> Result<&'a str, ProviderFailure> {
    let value = required_str(value, field)?;
    if value.is_empty() {
        return Err(protocol(format!("{field} was empty")));
    }
    Ok(value)
}

/// Two Responses argument encodings agree when they parse to the same JSON
/// value. The proxy may stream compact deltas but send a pretty-printed
/// `.done`/`output_item.done` for the same object, so byte-equality is too
/// strict. If either side is not valid JSON (e.g. an empty "" for a no-arg
/// call), fall back to byte-equality so genuine divergence still fails closed.
fn arguments_agree(a: &str, b: &str) -> bool {
    match (
        serde_json::from_str::<Value>(a),
        serde_json::from_str::<Value>(b),
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
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

/// Codex 式代理往 Responses 流里塞的厂商带外事件只携带遥测或心跳、
/// 不属官方语义族——识别后跳过,其余未知事件仍 fail-closed(见 match 兜底)。
/// 对齐 anthropic 的 `ping` 处理。名单两类:
/// - `codex.*`:厂商命名空间前缀,目前只见 `codex.rate_limits` 的限流遥测。
/// - `keepalive`:裸名心跳,代理在模型思考期间填。填得多少只取决于思考
///   多久——实测 effort=low 一个没有,xhigh 一轮 2~14 个——所以它对高 effort
///   是常态而非异常。
/// - `responsesapi.*`:另一个厂商命名空间,目前只见 `responsesapi.websocket_timing`
///   的传输计时遥测。它在一次 856 秒的真实审查末尾出现并打死整轮——同 keepalive
///   一样,只在长跑里才撞得到,所以短探针取样不到(见教训 89a)。
///
/// 官方语义族全部以 `response.` 开头(外加裸名 `error`),厂商噪声一律走自己的
/// 命名空间;这条名单每长一次,都在提示"非 `response.` 前缀即带外"可能才是正确
/// 的规则,但那要把 fail-closed 的边界整体挪一次,不在本片范围内。
fn is_out_of_band(event: &str) -> bool {
    event.starts_with("codex.") || event.starts_with("responsesapi.") || event == "keepalive"
}

/// The one field every `response.*` envelope must carry. `status` is read only
/// where it decides something (the terminal events), so it is checked there and
/// not here: a gateway that omits it from the opening frames is still saying
/// everything this stream needs to hear.
fn response_id_of(response: &Value) -> Result<&str, ProviderFailure> {
    required_non_empty(&response["id"], "response id")
}

fn event_item_key(value: &Value) -> Result<ItemKey, ProviderFailure> {
    Ok((
        required_u64(&value["output_index"], "output_index")?,
        required_non_empty(&value["item_id"], "item_id")?.to_string(),
    ))
}

fn item_key(value: &Value) -> Result<ItemKey, ProviderFailure> {
    Ok((
        required_u64(&value["output_index"], "output_index")?,
        required_non_empty(&value["item"]["id"], "output item id")?.to_string(),
    ))
}

fn lock_identity(
    slot: &mut Option<String>,
    value: &Value,
    field: &str,
) -> Result<(), ProviderFailure> {
    if value.is_null() {
        return Ok(());
    }
    let value = required_non_empty(value, field)?;
    match slot {
        Some(current) if current != value => Err(protocol(format!("{field} changed"))),
        Some(_) => Ok(()),
        None => {
            *slot = Some(value.to_string());
            Ok(())
        }
    }
}

fn usage_from(response: &Value) -> Result<Option<Usage>, ProviderFailure> {
    let usage = &response["usage"];
    if usage.is_null() {
        return Ok(None);
    }
    if !usage.is_object() {
        return Err(protocol("usage was not an object"));
    }
    let input = required_u64(&usage["input_tokens"], "input_tokens")?;
    let output = required_u64(&usage["output_tokens"], "output_tokens")?;
    let cached = match usage["input_tokens_details"]["cached_tokens"].as_u64() {
        Some(value) => value,
        None if usage["input_tokens_details"].is_null() => 0,
        None => return Err(protocol("cached_tokens was not an integer")),
    };
    if cached > input {
        return Err(protocol("cached_tokens exceeded input_tokens"));
    }
    Ok(Some(Usage {
        input_tokens: input - cached,
        output_tokens: output,
        cache_read_input_tokens: cached,
        cache_creation_input_tokens: 0,
    }))
}

#[derive(Clone, Copy)]
enum ItemStatusPolicy {
    Required,
    Optional,
}

/// What an item's own status said at `output_item.done`. `Truncated` does not
/// mean the item is damaged: a response that runs out of output budget stamps
/// `incomplete` on *every* item it managed to emit — gw_cn's deepseek route
/// stamps it on eleven function calls whose arguments are each complete JSON,
/// when the twelfth never started at all. So the flag describes the response,
/// the terminal frame carries the reason, and the item's own content is still
/// validated exactly as strictly as before.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemCompletion {
    Completed,
    Truncated,
}

/// The item's `status`, or `None` when it carried none and the policy allows
/// that. Handing back the value instead of comparing it lets each caller name
/// the statuses it accepts — and say which one it actually got.
fn item_status<'a>(
    item: &'a Value,
    field: &str,
    policy: ItemStatusPolicy,
) -> Result<Option<&'a str>, ProviderFailure> {
    match item.get("status") {
        None => match policy {
            ItemStatusPolicy::Required => required_str(&item["status"], field).map(Some),
            ItemStatusPolicy::Optional => Ok(None),
        },
        Some(status) => required_str(status, field).map(Some),
    }
}

fn start_item(item: &Value) -> Result<ItemState, ProviderFailure> {
    let item_type = required_str(&item["type"], "output item type")?;
    let status_policy = if item_type == "reasoning" {
        ItemStatusPolicy::Optional
    } else {
        ItemStatusPolicy::Required
    };
    match item_status(item, "output item status", status_policy)? {
        None | Some("in_progress") => {}
        Some(other) => {
            return Err(protocol(format!(
                "added output {item_type} item status was {other:?}, not in_progress"
            )));
        }
    }
    let kind = match item_type {
        "message" => {
            if required_str(&item["role"], "message role")? != "assistant" {
                return Err(protocol("output message role was not assistant"));
            }
            ItemKind::Message {
                parts: BTreeMap::new(),
            }
        }
        "reasoning" => ItemKind::Reasoning {
            summary: BTreeMap::new(),
            content: BTreeMap::new(),
        },
        "function_call" => {
            let mut call_id = None;
            let mut name = None;
            lock_identity(&mut call_id, &item["call_id"], "call_id")?;
            lock_identity(&mut name, &item["name"], "function name")?;
            let arguments = match item.get("arguments") {
                None | Some(Value::Null) => String::new(),
                Some(Value::String(value)) => value.clone(),
                Some(_) => return Err(protocol("function arguments were not a string")),
            };
            ItemKind::FunctionCall {
                call_id,
                name,
                arguments,
                arguments_started: false,
                arguments_done: false,
            }
        }
        _ => return Err(protocol("returned an unsupported output item type")),
    };
    Ok(ItemState { kind })
}

fn add_content_part(
    state: &mut ItemState,
    index: u64,
    part: &Value,
) -> Result<bool, ProviderFailure> {
    match &mut state.kind {
        ItemKind::Message { parts } => {
            if parts.values().any(|part| !part.part_closed) {
                return Err(protocol("message content parts overlapped"));
            }
            let kind = match required_str(&part["type"], "content part type")? {
                "output_text" => MessagePartKind::OutputText,
                "refusal" => MessagePartKind::Refusal,
                _ => return Err(protocol("message contained an unsupported content part")),
            };
            let text = opening_part_text(&part[message_part_field(kind)], "content part value")?
                .to_string();
            if parts
                .insert(
                    index,
                    TextPart {
                        kind,
                        text,
                        field_done: false,
                        part_closed: false,
                    },
                )
                .is_some()
            {
                return Err(protocol("content_index was added more than once"));
            }
            Ok(kind == MessagePartKind::Refusal)
        }
        ItemKind::Reasoning { content, .. } => {
            // No overlap guard here, unlike Message above: the proxy opens every
            // reasoning part and streams its delta but closes only the last one,
            // so a new part legitimately opens while earlier ones are still
            // open. Reasoning parts are independent by index, and correctness
            // rides on the item boundary instead — `verify_reasoning_parts`
            // matches each accumulated part against the final array.
            if required_str(&part["type"], "reasoning content part type")? != "reasoning_text" {
                return Err(protocol("reasoning contained an unsupported content part"));
            }
            let text = opening_part_text(&part["text"], "reasoning part text")?.to_string();
            if content
                .insert(
                    index,
                    ReasoningPart {
                        text,
                        ..Default::default()
                    },
                )
                .is_some()
            {
                return Err(protocol("reasoning content_index was added more than once"));
            }
            Ok(false)
        }
        ItemKind::FunctionCall { .. } => Err(protocol("function call received a content part")),
    }
}

fn text_part_mut(
    state: &mut ItemState,
    index: u64,
    expected: MessagePartKind,
) -> Result<&mut TextPart, ProviderFailure> {
    let ItemKind::Message { parts } = &mut state.kind else {
        return Err(protocol("text event referenced a non-message item"));
    };
    let part = parts
        .get_mut(&index)
        .ok_or_else(|| protocol("text event referenced an unknown content part"))?;
    if part.kind != expected || part.part_closed || part.field_done {
        return Err(protocol(
            "text event referenced the wrong or closed content part",
        ));
    }
    Ok(part)
}

fn reasoning_part_mut(
    state: &mut ItemState,
    index: u64,
    summary: bool,
) -> Result<&mut ReasoningPart, ProviderFailure> {
    let ItemKind::Reasoning {
        summary: summaries,
        content,
    } = &mut state.kind
    else {
        return Err(protocol("reasoning event referenced a non-reasoning item"));
    };
    let part = if summary {
        summaries.get_mut(&index)
    } else {
        content.get_mut(&index)
    }
    .ok_or_else(|| protocol("reasoning event referenced an unknown part"))?;
    if part.part_closed || part.field_done {
        return Err(protocol("reasoning event referenced a closed part"));
    }
    Ok(part)
}

fn check_final_item_status(
    item: &Value,
    expected_type: &str,
    status_policy: ItemStatusPolicy,
) -> Result<ItemCompletion, ProviderFailure> {
    let item_type = required_str(&item["type"], "final output item type")?;
    if item_type != expected_type {
        return Err(protocol(format!(
            "final output item type changed from {expected_type} to {item_type}"
        )));
    }
    match item_status(item, "final output item status", status_policy)? {
        None | Some("completed") => Ok(ItemCompletion::Completed),
        Some("incomplete") => Ok(ItemCompletion::Truncated),
        Some(other) => Err(protocol(format!(
            "final output {item_type} item status was {other:?}, neither completed nor incomplete"
        ))),
    }
}

fn finish_message(
    state: ItemState,
    item: &Value,
    refusal_seen: &mut bool,
) -> Result<(Vec<AssistantBlock>, ItemCompletion), ProviderFailure> {
    let completion = check_final_item_status(item, "message", ItemStatusPolicy::Required)?;
    if required_str(&item["role"], "final message role")? != "assistant" {
        return Err(protocol("final output message role was not assistant"));
    }
    let content = parts_array(&item["content"], "message content")?;
    let ItemKind::Message { parts } = state.kind else {
        return Err(protocol("final message referenced the wrong item type"));
    };
    let mut text = String::new();
    if parts.is_empty() {
        if !content.is_empty() {
            return Err(protocol(
                "final message introduced content without a part lifecycle",
            ));
        }
    } else {
        if content.len() != parts.len() {
            return Err(protocol(
                "final message content count did not match streamed parts",
            ));
        }
        for (position, (index, part)) in parts.into_iter().enumerate() {
            if index != position as u64 || !part.field_done || !part.part_closed {
                return Err(protocol("message content parts were not fully closed"));
            }
            let final_part = &content[position];
            let expected = match part.kind {
                MessagePartKind::OutputText => "output_text",
                MessagePartKind::Refusal => {
                    *refusal_seen = true;
                    "refusal"
                }
            };
            if required_str(&final_part["type"], "final content part type")? != expected
                || message_part_value(final_part, part.kind, "final content part value")?
                    != part.text
            {
                return Err(protocol(
                    "final message content did not match streamed text",
                ));
            }
            text.push_str(&part.text);
        }
    }
    Ok((
        (!text.is_empty())
            .then_some(AssistantBlock::Text { text })
            .into_iter()
            .collect(),
        completion,
    ))
}

/// An absent parts array and an empty one say the same thing: this item carried
/// none. The relay omits `content` on a reasoning item that only has a summary,
/// and demanding the key turned a shape choice upstream into a protocol
/// violation down here. A present-but-wrong type is still bad data — and now
/// says what it actually was, so the next report does not need a guess.
fn parts_array<'a>(value: &'a Value, what: &str) -> Result<&'a [Value], ProviderFailure> {
    match value {
        Value::Null => Ok(&[]),
        other => other.as_array().map(Vec::as_slice).ok_or_else(|| {
            protocol(format!(
                "final {what} was not an array (got {})",
                json_type(other)
            ))
        }),
    }
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn verify_reasoning_parts(
    final_parts: &Value,
    parts: BTreeMap<u64, ReasoningPart>,
    final_type: &str,
) -> Result<Vec<String>, ProviderFailure> {
    let final_parts = parts_array(final_parts, "reasoning parts")?;
    if parts.is_empty() {
        if final_parts.is_empty() {
            return Ok(Vec::new());
        }
        return Err(protocol(
            "final reasoning introduced content without a part lifecycle",
        ));
    }
    if final_parts.len() != parts.len() {
        return Err(protocol(
            "final reasoning part count did not match streaming",
        ));
    }
    let mut texts = Vec::with_capacity(parts.len());
    for (position, (index, part)) in parts.into_iter().enumerate() {
        // Indices must still be dense and ordered, but an unclosed part is not
        // an error: the proxy closes only the last one. What actually verifies
        // the stream is the text comparison below against the final array —
        // per-part `.done` was only ever a redundant, earlier check.
        if index != position as u64 {
            return Err(protocol("reasoning part indices were not dense"));
        }
        let final_part = &final_parts[position];
        if required_str(&final_part["type"], "final reasoning part type")? != final_type
            || required_str(&final_part["text"], "final reasoning text")? != part.text
        {
            return Err(protocol("final reasoning text did not match streamed text"));
        }
        texts.push(part.text);
    }
    Ok(texts)
}

fn finish_reasoning(
    state: ItemState,
    item: &Value,
) -> Result<(Vec<AssistantBlock>, ItemCompletion), ProviderFailure> {
    let completion = check_final_item_status(item, "reasoning", ItemStatusPolicy::Optional)?;
    let ItemKind::Reasoning { summary, content } = state.kind else {
        return Err(protocol("final reasoning referenced the wrong item type"));
    };
    let mut texts = verify_reasoning_parts(&item["summary"], summary, "summary_text")?;
    let mut raw = verify_reasoning_parts(&item["content"], content, "reasoning_text")?;
    texts.append(&mut raw);
    let signature = match item.get("encrypted_content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(value)) => value.clone(),
        Some(_) => return Err(protocol("encrypted_content was not a string")),
    };
    let block = AssistantBlock::Thinking {
        thinking: texts.concat(),
        signature,
    };
    Ok((
        block
            .has_semantic_payload()
            .then_some(block)
            .into_iter()
            .collect(),
        completion,
    ))
}

fn finish_function_call(
    state: ItemState,
    item: &Value,
) -> Result<(Vec<AssistantBlock>, ItemCompletion), ProviderFailure> {
    let completion = check_final_item_status(item, "function_call", ItemStatusPolicy::Required)?;
    let ItemKind::FunctionCall {
        call_id,
        name,
        arguments,
        arguments_started,
        arguments_done,
    } = state.kind
    else {
        return Err(protocol(
            "final function call referenced the wrong item type",
        ));
    };
    let final_call_id = required_non_empty(&item["call_id"], "final call_id")?;
    let final_name = required_non_empty(&item["name"], "final function name")?;
    if call_id
        .as_deref()
        .is_some_and(|value| value != final_call_id)
    {
        return Err(protocol("call_id changed"));
    }
    if name.as_deref().is_some_and(|value| value != final_name) {
        return Err(protocol("function name changed"));
    }
    let final_arguments = required_str(&item["arguments"], "final function arguments")?;
    if !arguments_started || !arguments_done {
        return Err(protocol("function arguments were not fully closed"));
    }
    if !arguments_agree(&arguments, final_arguments) {
        return Err(protocol(
            "final function arguments did not match streamed arguments",
        ));
    }
    let call_id = final_call_id.to_string();
    let name = final_name.to_string();
    Ok((
        vec![crate::tool_use_block(call_id, name, final_arguments)],
        completion,
    ))
}

fn finish_item(
    state: ItemState,
    item: &Value,
    refusal_seen: &mut bool,
) -> Result<(Vec<AssistantBlock>, ItemCompletion), ProviderFailure> {
    if matches!(&state.kind, ItemKind::Message { .. }) {
        finish_message(state, item, refusal_seen)
    } else if matches!(&state.kind, ItemKind::Reasoning { .. }) {
        finish_reasoning(state, item)
    } else {
        finish_function_call(state, item)
    }
}

fn bounded_reason(reason: &str) -> String {
    let mut chars = reason.chars();
    let mut bounded: String = chars.by_ref().take(80).collect();
    if chars.next().is_some() {
        bounded.push('…');
    }
    bounded
}

/// What the item stream said about this response, read once at the terminal.
/// Three booleans that only ever travel together, so they travel as one thing
/// rather than as a positional argument list nobody can read at the call site.
#[derive(Default)]
struct OutputSummary {
    has_tool: bool,
    has_refusal: bool,
    /// An `output_item.done` carried `status: "incomplete"`.
    truncated: bool,
}

fn terminal_outcome(
    event: &str,
    response: &Value,
    output: &OutputSummary,
) -> Result<AssistantOutcome, ProviderFailure> {
    let status = required_str(&response["status"], "response status")?;
    match event {
        "response.completed" => {
            if status != "completed" {
                return Err(protocol("completed event carried a non-completed status"));
            }
            // An item said the budget ran out and the response says it ran to
            // completion: the two describe different responses. Nothing below
            // could pick the right one, so fail closed.
            if output.truncated {
                return Err(protocol(
                    "an output item reported truncation but the response completed",
                ));
            }
            match (output.has_tool, output.has_refusal) {
                (true, true) => Err(protocol("response combined refusal with function calls")),
                (true, false) => Ok(AssistantOutcome::ToolUse),
                (false, true) => Ok(AssistantOutcome::Refused),
                (false, false) => Ok(AssistantOutcome::EndTurn),
            }
        }
        "response.incomplete" => {
            if status != "incomplete" {
                return Err(protocol("incomplete event carried a non-incomplete status"));
            }
            if output.has_tool && output.has_refusal {
                return Err(protocol("response combined refusal with function calls"));
            }
            let reason = required_non_empty(
                &response["incomplete_details"]["reason"],
                "incomplete reason",
            )?;
            // Filtering outranks everything: the response was stopped on
            // content grounds, and whatever it had already emitted is not a
            // decision to act on.
            if reason == "content_filter" {
                return Ok(AssistantOutcome::Filtered);
            }
            if output.has_refusal {
                return Ok(AssistantOutcome::Refused);
            }
            // Truncation lands on item boundaries: every function call that
            // reached `output_item.done` passed the full argument contract
            // (arguments done, valid JSON object, final value equal to the
            // accumulated deltas), and the one that did not fit sent no events
            // at all. Those calls are the model's actual next step — dispatch
            // them and the tool results bring it back to finish the rest.
            // Killing the round instead throws away work that is already paid
            // for, which is what `gw_cn`'s xhigh runs kept doing.
            if output.has_tool {
                return Ok(AssistantOutcome::ToolUse);
            }
            match reason {
                // Same event, two spellings: OpenAI says `max_output_tokens`,
                // ark (gw_cn's deepseek route) says `length`. Only this arm
                // reaches the agent's bounded truncation-continue recovery, so
                // a missing spelling here costs the whole round.
                "max_output_tokens" | "length" => Ok(AssistantOutcome::OutputLimit(
                    OutputLimitKind::MaxOutputTokens,
                )),
                _ => Ok(AssistantOutcome::Incomplete(IncompleteReason::Provider(
                    bounded_reason(reason),
                ))),
            }
        }
        _ => unreachable!("terminal_outcome only receives semantic terminal events"),
    }
}

/// `response.failed` / `error`: the two shapes a Responses stream reports a
/// failure in. Either may carry a context-overflow message, which the agent
/// recovers from by compacting instead of failing the turn.
fn stream_failure(event: &str, value: &Value, key: &str) -> ProviderFailure {
    if event == "response.failed" {
        let error = &value["response"]["error"];
        if is_overflow_message(&error.to_string()) {
            return ProviderFailure::context_overflow();
        }
        return crate::stream_error(
            "openai-responses",
            crate::error_label(error),
            crate::error_detail(error, key),
        );
    }
    if is_overflow_message(&value.to_string()) {
        return ProviderFailure::context_overflow();
    }
    // The `error` event's own `type` is the envelope ("error"), so only its
    // `code` names the failure here.
    let label = value["code"].as_str().unwrap_or("unknown");
    crate::stream_error("openai-responses", label, crate::error_detail(value, key))
}

/// Everything one Responses stream accumulates: the response identity it
/// adopted, the output items still open, the blocks already handed to the sink,
/// and the terminal once one lands. One method per wire event family, so the
/// grouping the event names already declare (`response.output_item.*`,
/// `response.function_call_arguments.*`, …) is the grouping of the code too.
#[derive(Default)]
struct ResponseStream {
    response_id: Option<String>,
    items: BTreeMap<ItemKey, ItemState>,
    seen_items: HashSet<ItemKey>,
    completed_blocks: Vec<AssistantBlock>,
    output: OutputSummary,
    completion: Option<StreamCompletion>,
}

impl ResponseStream {
    /// The open item this event names. `what` names the event for the failure
    /// message, which is all that distinguishes the ten call sites.
    fn open_item(&mut self, value: &Value, what: &str) -> Result<&mut ItemState, ProviderFailure> {
        let key = event_item_key(value)?;
        self.items
            .get_mut(&key)
            .ok_or_else(|| protocol(format!("{what} referenced an unknown item")))
    }

    /// `response.created` and `response.in_progress` are pure lifecycle
    /// metadata: they open the response and say nothing a later frame does not
    /// repeat. A relay that re-sends one — after an internal retry, or when
    /// merging an upstream stream — has told us nothing new, and killing the
    /// turn over it costs the whole round.
    ///
    /// Their `status` is descriptive and never read: nothing below branches on
    /// it, and a relay that queues the request and says so has changed nothing
    /// about what we do. Checking it made a word choice upstream into a
    /// protocol violation down here — and requiring the *key* did the same to a
    /// gateway that simply does not send it on the opening frames (gw_cn's
    /// deepseek route), which is why only the id is read here.
    ///
    /// A *different* identity is the one thing that matters, and it means two
    /// things depending on when it lands. Before any output item, it is an
    /// upstream restart — the relay retried and is now forwarding the real
    /// response, and nothing has been attributed yet, so follow it. After
    /// output, it is two responses sharing one stream, and everything from here
    /// would land on the wrong one: fail closed, naming both ids so the next
    /// reader does not have to guess which half moved.
    fn on_lifecycle(&mut self, event: &str, value: &Value) -> Result<(), ProviderFailure> {
        let id = response_id_of(&value["response"])?;
        let adopt = match self.response_id.as_deref() {
            None if event == "response.in_progress" => {
                return Err(protocol("response.in_progress arrived before created"));
            }
            None => true,
            Some(seen) if seen == id => false,
            Some(seen) => {
                if !self.seen_items.is_empty() || !self.completed_blocks.is_empty() {
                    return Err(protocol(format!(
                        "response identity changed from {seen} to {id} after output \
                             had landed"
                    )));
                }
                true
            }
        };
        if adopt {
            self.response_id = Some(id.to_string());
        }
        Ok(())
    }

    /// `response.output_item.added`: an item opens.
    fn on_item_added(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        if self.response_id.is_none() {
            return Err(protocol("output item arrived before response.created"));
        }
        let key = item_key(value)?;
        if !self.seen_items.insert(key.clone()) {
            return Err(protocol("output item identity was added more than once"));
        }
        let state = start_item(&value["item"])?;
        let display_kind_open = self.items.values().any(|current| {
            matches!(
                (&state.kind, &current.kind),
                (ItemKind::Message { .. }, ItemKind::Message { .. })
                    | (ItemKind::Reasoning { .. }, ItemKind::Reasoning { .. })
            )
        });
        if display_kind_open {
            return Err(protocol(
                "concurrent display items had no canonical identity",
            ));
        }
        self.items.insert(key, state);
        Ok(())
    }

    /// `response.output_item.done`: the item seals into assistant blocks.
    async fn on_item_done(
        &mut self,
        value: &Value,
        sink: &StreamSink,
    ) -> Result<(), ProviderFailure> {
        let key = item_key(value)?;
        let state = self
            .items
            .remove(&key)
            .ok_or_else(|| protocol("output item done referenced a non-open item"))?;
        let (mut blocks, completion) =
            finish_item(state, &value["item"], &mut self.output.has_refusal)?;
        self.output.truncated |= completion == ItemCompletion::Truncated;
        for block in &blocks {
            // A call whose arguments could not be read is still a call: the
            // response's outcome is what the model did, not whether we could
            // parse what it wrote.
            self.output.has_tool |= matches!(
                block,
                AssistantBlock::ToolUse { .. } | AssistantBlock::InvalidToolUse { .. }
            );
        }
        for block in blocks.drain(..) {
            if block.has_semantic_payload() {
                self.completed_blocks.push(block.clone());
                sink.block_done(block).await?;
            }
        }
        Ok(())
    }

    /// `response.content_part.added`: a message part opens.
    fn on_content_part_added(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        let state = self.open_item(value, "content part")?;
        let index = required_u64(&value["content_index"], "content_index")?;
        let refusal = add_content_part(state, index, &value["part"])?;
        self.output.has_refusal |= refusal;
        Ok(())
    }

    /// `response.content_part.done`: a message or reasoning-content part
    /// closes, and its final value has to be the one the deltas built.
    fn on_content_part_done(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        let state = self.open_item(value, "content part done")?;
        let index = required_u64(&value["content_index"], "content_index")?;
        match &mut state.kind {
            ItemKind::Message { parts } => {
                let part = parts
                    .get_mut(&index)
                    .ok_or_else(|| protocol("content part done referenced an unknown part"))?;
                if !part.field_done || part.part_closed {
                    return Err(protocol("content part closed out of order"));
                }
                let expected = match part.kind {
                    MessagePartKind::OutputText => "output_text",
                    MessagePartKind::Refusal => "refusal",
                };
                if required_str(&value["part"]["type"], "content part type")? != expected
                    || message_part_value(&value["part"], part.kind, "content part value")?
                        != part.text
                {
                    return Err(protocol("content part final value changed"));
                }
                part.part_closed = true;
            }
            ItemKind::Reasoning { content, .. } => {
                let part = content
                    .get_mut(&index)
                    .ok_or_else(|| protocol("reasoning content done referenced an unknown part"))?;
                if !part.field_done || part.part_closed {
                    return Err(protocol("reasoning content part closed out of order"));
                }
                if required_str(&value["part"]["type"], "reasoning content part type")?
                    != "reasoning_text"
                    || required_str(&value["part"]["text"], "reasoning content text")? != part.text
                {
                    return Err(protocol("reasoning content final value changed"));
                }
                part.part_closed = true;
            }
            ItemKind::FunctionCall { .. } => {
                return Err(protocol("function call received content_part.done"));
            }
        }
        Ok(())
    }

    /// `response.output_text.delta` / `response.refusal.delta`: streamed
    /// message text. The refusal spelling is the same channel with a different
    /// part kind, and it also decides the turn's outcome.
    async fn on_message_text_delta(
        &mut self,
        event: &str,
        value: &Value,
        sink: &StreamSink,
    ) -> Result<(), ProviderFailure> {
        let expected = self.message_part_kind(event, "response.output_text.delta");
        let state = self.open_item(value, "text delta")?;
        let index = required_u64(&value["content_index"], "content_index")?;
        let part = text_part_mut(state, index, expected)?;
        let delta = required_str(&value["delta"], "text delta")?;
        part.text.push_str(delta);
        if !delta.is_empty() {
            sink.text_delta(delta.to_string()).await?;
        }
        Ok(())
    }

    /// `response.output_text.done` / `response.refusal.done`.
    fn on_message_text_done(&mut self, event: &str, value: &Value) -> Result<(), ProviderFailure> {
        let expected = self.message_part_kind(event, "response.output_text.done");
        let state = self.open_item(value, "text done")?;
        let index = required_u64(&value["content_index"], "content_index")?;
        let part = text_part_mut(state, index, expected)?;
        if message_part_value(value, expected, "final text value")? != part.text {
            return Err(protocol("text done did not match accumulated delta"));
        }
        part.field_done = true;
        Ok(())
    }

    /// Which message part an `output_text`/`refusal` pair is addressing. A
    /// refusal on either channel decides the turn's outcome, so it is recorded
    /// here rather than at each of the two call sites.
    fn message_part_kind(&mut self, event: &str, text_event: &str) -> MessagePartKind {
        if event == text_event {
            MessagePartKind::OutputText
        } else {
            self.output.has_refusal = true;
            MessagePartKind::Refusal
        }
    }

    /// `response.reasoning_summary_part.added`.
    fn on_reasoning_part_added(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        let state = self.open_item(value, "reasoning part")?;
        let index = required_u64(&value["summary_index"], "summary_index")?;
        let ItemKind::Reasoning { summary, .. } = &mut state.kind else {
            return Err(protocol("reasoning part referenced the wrong item type"));
        };
        // Opening a part while earlier ones are still open is the
        // norm on this wire, not a violation — see `add_content_part`.
        if required_str(&value["part"]["type"], "summary part type")? != "summary_text" {
            return Err(protocol("reasoning summary part type was unsupported"));
        }
        let text = opening_part_text(&value["part"]["text"], "summary part text")?;
        if summary.contains_key(&index) {
            return Err(protocol("summary_index was added more than once"));
        }
        summary.insert(
            index,
            ReasoningPart {
                text: text.to_string(),
                ..Default::default()
            },
        );
        Ok(())
    }

    /// `response.reasoning_summary_part.done`.
    fn on_reasoning_part_done(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        let state = self.open_item(value, "reasoning part done")?;
        let index = required_u64(&value["summary_index"], "summary_index")?;
        let ItemKind::Reasoning { summary, .. } = &mut state.kind else {
            return Err(protocol(
                "reasoning part done referenced the wrong item type",
            ));
        };
        let part = summary
            .get_mut(&index)
            .ok_or_else(|| protocol("reasoning part done referenced an unknown part"))?;
        if !part.field_done || part.part_closed {
            return Err(protocol("reasoning summary part closed out of order"));
        }
        if required_str(&value["part"]["type"], "summary part type")? != "summary_text"
            || required_str(&value["part"]["text"], "summary part text")? != part.text
        {
            return Err(protocol("reasoning summary part final value changed"));
        }
        part.part_closed = true;
        Ok(())
    }

    /// `response.reasoning_summary_text.delta` / `response.reasoning_text.delta`:
    /// the summary and the raw-content channels, indexed by different fields.
    async fn on_reasoning_text_delta(
        &mut self,
        event: &str,
        value: &Value,
        sink: &StreamSink,
    ) -> Result<(), ProviderFailure> {
        let summary = event == "response.reasoning_summary_text.delta";
        let state = self.open_item(value, "reasoning delta")?;
        let index = reasoning_index(value, summary)?;
        let part = reasoning_part_mut(state, index, summary)?;
        let delta = required_str(&value["delta"], "reasoning delta")?;
        part.text.push_str(delta);
        if !delta.is_empty() {
            sink.thinking_delta(delta.to_string()).await?;
        }
        Ok(())
    }

    /// `response.reasoning_summary_text.done` / `response.reasoning_text.done`.
    fn on_reasoning_text_done(
        &mut self,
        event: &str,
        value: &Value,
    ) -> Result<(), ProviderFailure> {
        let summary = event == "response.reasoning_summary_text.done";
        let state = self.open_item(value, "reasoning done")?;
        let index = reasoning_index(value, summary)?;
        let part = reasoning_part_mut(state, index, summary)?;
        if required_str(&value["text"], "final reasoning text")? != part.text {
            return Err(protocol("reasoning done did not match accumulated delta"));
        }
        part.field_done = true;
        Ok(())
    }

    /// `response.function_call_arguments.delta`.
    fn on_arguments_delta(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        let state = self.open_item(value, "arguments delta")?;
        let ItemKind::FunctionCall {
            arguments,
            arguments_started,
            arguments_done,
            ..
        } = &mut state.kind
        else {
            return Err(protocol("arguments delta referenced a non-function item"));
        };
        if *arguments_done {
            return Err(protocol("arguments delta arrived after arguments done"));
        }
        *arguments_started = true;
        arguments.push_str(required_str(&value["delta"], "arguments delta")?);
        Ok(())
    }

    /// `response.function_call_arguments.done`.
    fn on_arguments_done(&mut self, value: &Value) -> Result<(), ProviderFailure> {
        let state = self.open_item(value, "arguments done")?;
        let ItemKind::FunctionCall {
            arguments,
            arguments_started,
            arguments_done,
            ..
        } = &mut state.kind
        else {
            return Err(protocol("arguments done referenced a non-function item"));
        };
        if *arguments_done {
            return Err(protocol("received duplicate arguments done"));
        }
        *arguments_started = true;
        if !arguments_agree(
            required_str(&value["arguments"], "final arguments")?,
            arguments.as_str(),
        ) {
            return Err(protocol("arguments done did not match accumulated delta"));
        }
        *arguments_done = true;
        Ok(())
    }

    /// `response.completed` / `response.incomplete`: the semantic terminal.
    fn on_terminal(&mut self, event: &str, value: &Value) -> Result<(), ProviderFailure> {
        let expected = self
            .response_id
            .as_deref()
            .ok_or_else(|| protocol("terminal arrived before response.created"))?;
        if !self.items.is_empty() {
            return Err(protocol("terminal arrived with open output items"));
        }
        let id = response_id_of(&value["response"])?;
        if id != expected {
            return Err(protocol("terminal response identity changed"));
        }
        let outcome = terminal_outcome(event, &value["response"], &self.output)?;
        crate::validate_assistant_output("openai-responses", &outcome, &self.completed_blocks)?;
        self.completion = Some(StreamCompletion::new(
            outcome,
            usage_from(&value["response"])?,
        ));
        Ok(())
    }
}

/// The summary channel indexes by `summary_index`, the raw-content channel by
/// `content_index`.
fn reasoning_index(value: &Value, summary: bool) -> Result<u64, ProviderFailure> {
    if summary {
        required_u64(&value["summary_index"], "summary_index")
    } else {
        required_u64(&value["content_index"], "content_index")
    }
}

pub(super) async fn stream(
    url: &str,
    cred: &crate::Credential,
    body: &Value,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let req = cred.apply(crate::http_client().post(url)).json(body);
    let resp = crate::send_checked(req, "openai-responses", url, cred.secret()).await?;

    let mut frames = SseFrames::new(resp.bytes_stream());
    let mut state = ResponseStream::default();

    while let Some(frame) = frames.next().await? {
        if frame.data.trim() == "[DONE]" {
            if state.completion.is_some() {
                continue;
            }
            return Err(protocol("[DONE] arrived before a semantic terminal"));
        }
        let value = crate::parse_sse_json("openai-responses", &frame.data)?;
        let event = event_type(&frame, &value)?;
        if state.completion.is_some() && !is_out_of_band(event) {
            return Err(protocol("semantic event arrived after response terminal"));
        }
        match event {
            "response.created" | "response.in_progress" => state.on_lifecycle(event, &value)?,
            "response.output_item.added" => state.on_item_added(&value)?,
            "response.content_part.added" => state.on_content_part_added(&value)?,
            "response.output_text.delta" | "response.refusal.delta" => {
                state.on_message_text_delta(event, &value, sink).await?
            }
            "response.output_text.done" | "response.refusal.done" => {
                state.on_message_text_done(event, &value)?
            }
            "response.reasoning_summary_part.added" => state.on_reasoning_part_added(&value)?,
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                state.on_reasoning_text_delta(event, &value, sink).await?
            }
            "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                state.on_reasoning_text_done(event, &value)?
            }
            "response.reasoning_summary_part.done" => state.on_reasoning_part_done(&value)?,
            "response.content_part.done" => state.on_content_part_done(&value)?,
            "response.function_call_arguments.delta" => state.on_arguments_delta(&value)?,
            "response.function_call_arguments.done" => state.on_arguments_done(&value)?,
            "response.output_item.done" => state.on_item_done(&value, sink).await?,
            "response.completed" | "response.incomplete" => state.on_terminal(event, &value)?,
            "response.failed" | "error" => {
                return Err(stream_failure(event, &value, cred.secret()));
            }
            _ if is_out_of_band(event) => {}
            // Name the offender: without it a new vendor event costs an SSE
            // capture to identify (how `keepalive` was found).
            _ => {
                return Err(protocol(format!(
                    "returned an unknown semantic event: {}",
                    bounded_reason(event)
                )));
            }
        }
        if state.completion.is_some() {
            frames.stop();
        }
    }

    if let Some(completion) = state.completion {
        return Ok(completion);
    }
    Err(ProviderFailure::incomplete_protocol(
        "openai-responses stream ended before completion",
    ))
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
                provider_provenance: None,
                injected: None,
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
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "from another rail"}],
                    "encrypted_content": "",
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
            provider_provenance: None,
            injected: None,
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
