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

use super::GuardedBody;
use super::ProviderFailure;
use super::StreamCompletion;
use super::StreamSink;
use super::is_overflow_message;
use super::sse::SseFrame;
use super::sse::SseParser;
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

fn message_part_value<'a>(
    value: &'a Value,
    kind: MessagePartKind,
    field: &str,
) -> Result<&'a str, ProviderFailure> {
    match kind {
        MessagePartKind::OutputText => required_str(&value["text"], field),
        MessagePartKind::Refusal => required_str(&value["refusal"], field),
    }
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

/// gateway/Codex 代理往 Responses 流里塞的厂商带外事件只携带遥测或心跳、
/// 不属官方语义族——识别后跳过,其余未知事件仍 fail-closed(见 match 兜底)。
/// 对齐 anthropic 的 `ping` 处理。名单两类:
/// - `codex.*`:厂商命名空间前缀,目前只见 `codex.rate_limits` 的限流遥测。
/// - `keepalive`:裸名心跳,gateway 在模型思考期间填。填得多少只取决于思考
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

fn response_identity(response: &Value) -> Result<(&str, &str), ProviderFailure> {
    Ok((
        required_non_empty(&response["id"], "response id")?,
        required_str(&response["status"], "response status")?,
    ))
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

fn check_item_status(
    item: &Value,
    field: &str,
    expected: &str,
    policy: ItemStatusPolicy,
    mismatch: &str,
) -> Result<(), ProviderFailure> {
    let Some(status) = item.get("status") else {
        return match policy {
            ItemStatusPolicy::Required => required_str(&item["status"], field).map(|_| ()),
            ItemStatusPolicy::Optional => Ok(()),
        };
    };
    if required_str(status, field)? != expected {
        return Err(protocol(mismatch));
    }
    Ok(())
}

fn start_item(item: &Value) -> Result<ItemState, ProviderFailure> {
    let item_type = required_str(&item["type"], "output item type")?;
    let status_policy = if item_type == "reasoning" {
        ItemStatusPolicy::Optional
    } else {
        ItemStatusPolicy::Required
    };
    check_item_status(
        item,
        "output item status",
        "in_progress",
        status_policy,
        "added output item was not in_progress",
    )?;
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
            let text = message_part_value(part, kind, "content part value")?.to_string();
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
            // No overlap guard here, unlike Message above: gateway opens every
            // reasoning part and streams its delta but closes only the last one,
            // so a new part legitimately opens while earlier ones are still
            // open. Reasoning parts are independent by index, and correctness
            // rides on the item boundary instead — `verify_reasoning_parts`
            // matches each accumulated part against the final array.
            if required_str(&part["type"], "reasoning content part type")? != "reasoning_text" {
                return Err(protocol("reasoning contained an unsupported content part"));
            }
            let text = required_str(&part["text"], "reasoning part text")?.to_string();
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
) -> Result<(), ProviderFailure> {
    if required_str(&item["type"], "final output item type")? != expected_type {
        return Err(protocol("final output item type changed"));
    }
    check_item_status(
        item,
        "final output item status",
        "completed",
        status_policy,
        "final output item was not completed",
    )
}

fn finish_message(
    state: ItemState,
    item: &Value,
    refusal_seen: &mut bool,
) -> Result<Vec<AssistantBlock>, ProviderFailure> {
    check_final_item_status(item, "message", ItemStatusPolicy::Required)?;
    if required_str(&item["role"], "final message role")? != "assistant" {
        return Err(protocol("final output message role was not assistant"));
    }
    let content = item["content"]
        .as_array()
        .ok_or_else(|| protocol("final message content was not an array"))?;
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
    Ok((!text.is_empty())
        .then_some(AssistantBlock::Text { text })
        .into_iter()
        .collect())
}

fn verify_reasoning_parts(
    final_parts: &Value,
    parts: BTreeMap<u64, ReasoningPart>,
    final_type: &str,
) -> Result<Vec<String>, ProviderFailure> {
    let final_parts = final_parts
        .as_array()
        .ok_or_else(|| protocol("final reasoning parts were not an array"))?;
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
        // an error: gateway closes only the last one. What actually verifies
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
) -> Result<Vec<AssistantBlock>, ProviderFailure> {
    check_final_item_status(item, "reasoning", ItemStatusPolicy::Optional)?;
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
    Ok(block
        .has_semantic_payload()
        .then_some(block)
        .into_iter()
        .collect())
}

fn finish_function_call(
    state: ItemState,
    item: &Value,
) -> Result<Vec<AssistantBlock>, ProviderFailure> {
    check_final_item_status(item, "function_call", ItemStatusPolicy::Required)?;
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
    let input = crate::parse_tool_input("openai-responses", &name, final_arguments)?;
    Ok(vec![AssistantBlock::ToolUse {
        id: call_id,
        name,
        input,
    }])
}

fn finish_item(
    state: ItemState,
    item: &Value,
    refusal_seen: &mut bool,
) -> Result<Vec<AssistantBlock>, ProviderFailure> {
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

fn terminal_outcome(
    event: &str,
    response: &Value,
    has_tool: bool,
    has_refusal: bool,
) -> Result<AssistantOutcome, ProviderFailure> {
    let (_, status) = response_identity(response)?;
    match event {
        "response.completed" => {
            if status != "completed" {
                return Err(protocol("completed event carried a non-completed status"));
            }
            match (has_tool, has_refusal) {
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
            if has_tool || has_refusal {
                return Err(protocol("incomplete response contained conflicting output"));
            }
            let reason = required_non_empty(
                &response["incomplete_details"]["reason"],
                "incomplete reason",
            )?;
            match reason {
                "max_output_tokens" => Ok(AssistantOutcome::OutputLimit(
                    OutputLimitKind::MaxOutputTokens,
                )),
                "content_filter" => Ok(AssistantOutcome::Filtered),
                _ => Ok(AssistantOutcome::Incomplete(IncompleteReason::Provider(
                    bounded_reason(reason),
                ))),
            }
        }
        _ => unreachable!("terminal_outcome only receives semantic terminal events"),
    }
}

pub(super) async fn stream(
    url: &str,
    key: &str,
    body: &Value,
    sink: &StreamSink,
) -> Result<StreamCompletion, ProviderFailure> {
    let req = crate::http_client().post(url).bearer_auth(key).json(body);
    let resp = crate::send_checked(req, "openai-responses", key).await?;

    let mut parser = SseParser::default();
    let mut byte_stream = GuardedBody::new(resp.bytes_stream());
    let mut response_id: Option<String> = None;
    let mut items: BTreeMap<ItemKey, ItemState> = BTreeMap::new();
    let mut seen_items = HashSet::new();
    let mut completed_blocks = Vec::new();
    let mut has_tool = false;
    let mut has_refusal = false;
    let mut completion = None;

    loop {
        let next = byte_stream.next().await?;
        let Some(chunk) = next else {
            break;
        };
        for frame in parser.feed(&chunk)? {
            if frame.data.trim() == "[DONE]" {
                if completion.is_some() {
                    continue;
                }
                return Err(protocol("[DONE] arrived before a semantic terminal"));
            }
            let value = crate::parse_sse_json("openai-responses", &frame.data)?;
            let event = event_type(&frame, &value)?;
            if completion.is_some() && !is_out_of_band(event) {
                return Err(protocol("semantic event arrived after response terminal"));
            }
            match event {
                // `response.created` and `response.in_progress` are pure
                // lifecycle metadata: they open the response and say nothing a
                // later frame does not repeat. A relay that re-sends one — after
                // an internal retry, or when merging an upstream stream — has
                // told us nothing new, and killing the turn over it costs the
                // whole round. What must still fail closed is a *different*
                // identity: that is two responses multiplexed onto one stream,
                // and everything after it would be attributed to the wrong one.
                "response.created" => {
                    let (id, status) = response_identity(&value["response"])?;
                    if status != "in_progress" {
                        return Err(protocol("created response was not in_progress"));
                    }
                    match response_id.as_deref() {
                        Some(seen) if seen != id => {
                            return Err(protocol("response.created identity changed"));
                        }
                        Some(_) => {}
                        None => response_id = Some(id.to_string()),
                    }
                }
                "response.in_progress" => {
                    let expected = response_id
                        .as_deref()
                        .ok_or_else(|| protocol("response.in_progress arrived before created"))?;
                    let (id, status) = response_identity(&value["response"])?;
                    if id != expected || status != "in_progress" {
                        return Err(protocol("response.in_progress identity or status changed"));
                    }
                }
                "response.output_item.added" => {
                    if response_id.is_none() {
                        return Err(protocol("output item arrived before response.created"));
                    }
                    let key = item_key(&value)?;
                    if !seen_items.insert(key.clone()) {
                        return Err(protocol("output item identity was added more than once"));
                    }
                    let state = start_item(&value["item"])?;
                    let display_kind_open = items.values().any(|current| {
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
                    items.insert(key, state);
                }
                "response.content_part.added" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("content part referenced an unknown item"))?;
                    let index = required_u64(&value["content_index"], "content_index")?;
                    has_refusal |= add_content_part(state, index, &value["part"])?;
                }
                "response.output_text.delta" | "response.refusal.delta" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("text delta referenced an unknown item"))?;
                    let index = required_u64(&value["content_index"], "content_index")?;
                    let expected = if event == "response.output_text.delta" {
                        MessagePartKind::OutputText
                    } else {
                        has_refusal = true;
                        MessagePartKind::Refusal
                    };
                    let part = text_part_mut(state, index, expected)?;
                    let delta = required_str(&value["delta"], "text delta")?;
                    part.text.push_str(delta);
                    if !delta.is_empty() {
                        sink.text_delta(delta.to_string()).await?;
                    }
                }
                "response.output_text.done" | "response.refusal.done" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("text done referenced an unknown item"))?;
                    let index = required_u64(&value["content_index"], "content_index")?;
                    let expected = if event == "response.output_text.done" {
                        MessagePartKind::OutputText
                    } else {
                        has_refusal = true;
                        MessagePartKind::Refusal
                    };
                    let part = text_part_mut(state, index, expected)?;
                    if message_part_value(&value, expected, "final text value")? != part.text {
                        return Err(protocol("text done did not match accumulated delta"));
                    }
                    part.field_done = true;
                }
                "response.reasoning_summary_part.added" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("reasoning part referenced an unknown item"))?;
                    let index = required_u64(&value["summary_index"], "summary_index")?;
                    let ItemKind::Reasoning { summary, .. } = &mut state.kind else {
                        return Err(protocol("reasoning part referenced the wrong item type"));
                    };
                    // Opening a part while earlier ones are still open is the
                    // norm on this wire, not a violation — see `add_content_part`.
                    if required_str(&value["part"]["type"], "summary part type")? != "summary_text"
                    {
                        return Err(protocol("reasoning summary part type was unsupported"));
                    }
                    let text = required_str(&value["part"]["text"], "summary part text")?;
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
                }
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("reasoning delta referenced an unknown item"))?;
                    let summary = event == "response.reasoning_summary_text.delta";
                    let index = if summary {
                        required_u64(&value["summary_index"], "summary_index")?
                    } else {
                        required_u64(&value["content_index"], "content_index")?
                    };
                    let part = reasoning_part_mut(state, index, summary)?;
                    let delta = required_str(&value["delta"], "reasoning delta")?;
                    part.text.push_str(delta);
                    if !delta.is_empty() {
                        sink.thinking_delta(delta.to_string()).await?;
                    }
                }
                "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("reasoning done referenced an unknown item"))?;
                    let summary = event == "response.reasoning_summary_text.done";
                    let index = if summary {
                        required_u64(&value["summary_index"], "summary_index")?
                    } else {
                        required_u64(&value["content_index"], "content_index")?
                    };
                    let part = reasoning_part_mut(state, index, summary)?;
                    if required_str(&value["text"], "final reasoning text")? != part.text {
                        return Err(protocol("reasoning done did not match accumulated delta"));
                    }
                    part.field_done = true;
                }
                "response.reasoning_summary_part.done" => {
                    let key = event_item_key(&value)?;
                    let state = items.get_mut(&key).ok_or_else(|| {
                        protocol("reasoning part done referenced an unknown item")
                    })?;
                    let index = required_u64(&value["summary_index"], "summary_index")?;
                    let ItemKind::Reasoning { summary, .. } = &mut state.kind else {
                        return Err(protocol(
                            "reasoning part done referenced the wrong item type",
                        ));
                    };
                    let part = summary.get_mut(&index).ok_or_else(|| {
                        protocol("reasoning part done referenced an unknown part")
                    })?;
                    if !part.field_done || part.part_closed {
                        return Err(protocol("reasoning summary part closed out of order"));
                    }
                    if required_str(&value["part"]["type"], "summary part type")? != "summary_text"
                        || required_str(&value["part"]["text"], "summary part text")? != part.text
                    {
                        return Err(protocol("reasoning summary part final value changed"));
                    }
                    part.part_closed = true;
                }
                "response.content_part.done" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("content part done referenced an unknown item"))?;
                    let index = required_u64(&value["content_index"], "content_index")?;
                    match &mut state.kind {
                        ItemKind::Message { parts } => {
                            let part = parts.get_mut(&index).ok_or_else(|| {
                                protocol("content part done referenced an unknown part")
                            })?;
                            if !part.field_done || part.part_closed {
                                return Err(protocol("content part closed out of order"));
                            }
                            let expected = match part.kind {
                                MessagePartKind::OutputText => "output_text",
                                MessagePartKind::Refusal => "refusal",
                            };
                            if required_str(&value["part"]["type"], "content part type")?
                                != expected
                                || message_part_value(
                                    &value["part"],
                                    part.kind,
                                    "content part value",
                                )? != part.text
                            {
                                return Err(protocol("content part final value changed"));
                            }
                            part.part_closed = true;
                        }
                        ItemKind::Reasoning { content, .. } => {
                            let part = content.get_mut(&index).ok_or_else(|| {
                                protocol("reasoning content done referenced an unknown part")
                            })?;
                            if !part.field_done || part.part_closed {
                                return Err(protocol("reasoning content part closed out of order"));
                            }
                            if required_str(&value["part"]["type"], "reasoning content part type")?
                                != "reasoning_text"
                                || required_str(&value["part"]["text"], "reasoning content text")?
                                    != part.text
                            {
                                return Err(protocol("reasoning content final value changed"));
                            }
                            part.part_closed = true;
                        }
                        ItemKind::FunctionCall { .. } => {
                            return Err(protocol("function call received content_part.done"));
                        }
                    }
                }
                "response.function_call_arguments.delta" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("arguments delta referenced an unknown item"))?;
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
                }
                "response.function_call_arguments.done" => {
                    let key = event_item_key(&value)?;
                    let state = items
                        .get_mut(&key)
                        .ok_or_else(|| protocol("arguments done referenced an unknown item"))?;
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
                }
                "response.output_item.done" => {
                    let key = item_key(&value)?;
                    let state = items
                        .remove(&key)
                        .ok_or_else(|| protocol("output item done referenced a non-open item"))?;
                    let mut blocks = finish_item(state, &value["item"], &mut has_refusal)?;
                    for block in &blocks {
                        has_tool |= matches!(block, AssistantBlock::ToolUse { .. });
                    }
                    for block in blocks.drain(..) {
                        if block.has_semantic_payload() {
                            completed_blocks.push(block.clone());
                            sink.block_done(block).await?;
                        }
                    }
                }
                "response.completed" | "response.incomplete" => {
                    let expected = response_id
                        .as_deref()
                        .ok_or_else(|| protocol("terminal arrived before response.created"))?;
                    if !items.is_empty() {
                        return Err(protocol("terminal arrived with open output items"));
                    }
                    let (id, _) = response_identity(&value["response"])?;
                    if id != expected {
                        return Err(protocol("terminal response identity changed"));
                    }
                    let outcome =
                        terminal_outcome(event, &value["response"], has_tool, has_refusal)?;
                    crate::validate_assistant_output(
                        "openai-responses",
                        &outcome,
                        &completed_blocks,
                    )?;
                    completion = Some(StreamCompletion::new(
                        outcome,
                        usage_from(&value["response"])?,
                    ));
                }
                "response.failed" => {
                    let error = &value["response"]["error"];
                    if is_overflow_message(&error.to_string()) {
                        return Err(ProviderFailure::context_overflow());
                    }
                    return Err(crate::stream_error(
                        "openai-responses",
                        crate::error_label(error),
                        crate::error_detail(error, key),
                    ));
                }
                "error" => {
                    if is_overflow_message(&value.to_string()) {
                        return Err(ProviderFailure::context_overflow());
                    }
                    // The `error` event's own `type` is the envelope ("error"),
                    // so only its `code` names the failure here.
                    let label = value["code"].as_str().unwrap_or("unknown");
                    return Err(crate::stream_error(
                        "openai-responses",
                        label,
                        crate::error_detail(&value, key),
                    ));
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
        }
        if let Some(completion) = completion.take() {
            return Ok(completion);
        }
    }
    parser.finish()?;
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
