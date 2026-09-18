//! HTTP contract tests for the OpenAI Responses API adapter: the Responses
//! SSE event family in, StreamEvents out, plus the stateless request shape.

use std::sync::Arc;

use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OutputLimitKind;
use kloop_protocol::ReasoningEffort;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;
use kloop_provider::Provider;
use kloop_provider::ProviderFailureKind;
use kloop_provider::StreamResult;
use serde_json::Value;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

fn sse_body(events: &[Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
        .collect()
}

async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            // The production client is process-wide; close fixture sockets so a
            // recycled wiremock port cannot inherit an idle connection.
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("connection", "close")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(server)
        .await;
}

async fn mount_bytes(server: &MockServer, body: Vec<u8>) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(
            // The production client is process-wide; close fixture sockets so a
            // recycled wiremock port cannot inherit an idle connection.
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .insert_header("connection", "close")
                .set_body_bytes(body),
        )
        .mount(server)
        .await;
}

fn responses(server: &MockServer) -> Provider {
    Provider::OpenAiResponses {
        key: "test-key".into(),
        base: server.uri(),
    }
}

async fn collect(provider: Provider) -> Vec<StreamResult> {
    let provider = Arc::new(provider);
    let mut rx = provider.stream("test-model", "system", &[Message::user_text("hi")], &[]);
    let mut events = Vec::new();
    while let Some(event) = rx.recv().await {
        events.push(event);
    }
    events
}

/// The stateless request contract, asserted whole-object: instructions,
/// translated input items, flat function tools, store:false and the
/// encrypted-reasoning include.
#[tokio::test]
async fn request_body_is_stateless_with_reasoning_include() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[json!({"type": "response.completed", "response": {}})]),
    )
    .await;
    let provider = Arc::new(responses(&server));
    let messages = vec![
        Message::user_text("hi"),
        Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "enc-blob".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_1".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
            provider.response_provenance("test-model"),
        ),
        Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "call_1".into(),
            content: "ok".into(),
            is_error: false,
        }]),
    ];
    let tools = [ToolDef {
        name: "bash".into(),
        description: "run a command".into(),
        schema: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
    }];
    let mut rx = provider.stream("test-model", "be brief", &messages, &tools);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "instructions": "be brief",
            "input": [
                {"type": "message", "role": "user",
                 "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "summary": [], "encrypted_content": "enc-blob"},
                {"type": "function_call", "call_id": "call_1", "name": "bash",
                 "arguments": "{\"command\":\"ls\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            ],
            "tools": [{
                "type": "function",
                "name": "bash",
                "description": "run a command",
                "parameters": {"type": "object", "properties": {"command": {"type": "string"}}},
            }],
            "max_output_tokens": 32768,
            "parallel_tool_calls": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": true,
        })
    );
}

/// The prompt-cache routing hint rides every request when the session is
/// bound, and is absent otherwise — an empty key would herd every unbound run
/// onto one bucket. It only steers routing, so the rest of the body is
/// unchanged either way.
#[tokio::test]
async fn prompt_cache_key_is_sent_when_bound_and_omitted_otherwise() {
    for (cache_key, expected) in [
        (Some("sess-42"), Some(json!("sess-42"))),
        (Some(""), None),
        (None, None),
    ] {
        let server = MockServer::start().await;
        mount_sse(
            &server,
            sse_body(&[json!({"type": "response.completed", "response": {}})]),
        )
        .await;
        let provider = Arc::new(responses(&server));
        let attempt = provider.attempt_identity("test", 1, "test-model");
        let mut rx = provider.stream_attempt(
            &attempt,
            None,
            cache_key,
            "s",
            &[Message::user_text("hi")],
            &[],
        );
        while rx.recv().await.is_some() {}

        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body.get("prompt_cache_key").cloned(),
            expected,
            "cache_key {cache_key:?}"
        );
    }
}

#[tokio::test]
async fn error_tool_output_replays_program_resume_contract_verbatim() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[json!({"type": "response.completed", "response": {}})]),
    )
    .await;
    let provider = Arc::new(responses(&server));
    let error = "PROGRAM_EXPECTED_FAILURE_68\nDurable Run ID: run-live-7\nresume_from_run_id: \"run-live-7\"";
    let messages = vec![
        Message::user_text("run the program"),
        Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: "use Program".into(),
                    signature: "enc-program".into(),
                },
                ContentBlock::ToolUse {
                    id: "call_program".into(),
                    name: "run_program".into(),
                    input: json!({"source": "throw new Error('expected')"}),
                },
            ],
            provider.response_provenance("test-model"),
        ),
        Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "call_program".into(),
            content: error.into(),
            is_error: true,
        }]),
    ];
    let mut rx = provider.stream("test-model", "continue", &messages, &[]);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "run the program"}]},
            {"type": "reasoning", "summary": [{"type": "summary_text", "text": "use Program"}],
             "encrypted_content": "enc-program"},
            {"type": "function_call", "call_id": "call_program", "name": "run_program",
             "arguments": "{\"source\":\"throw new Error('expected')\"}"},
            {"type": "function_call_output", "call_id": "call_program",
             "output": format!("[error] {error}")},
        ])
    );
}

/// The session effort maps to the reasoning request field (with summary=auto
/// for displayable text); absent effort sends no reasoning field at all.
#[tokio::test]
async fn effort_maps_to_reasoning_field() {
    for (effort, expected) in [
        (None, None),
        (
            Some(ReasoningEffort::High),
            Some(json!({"effort": "high", "summary": "auto"})),
        ),
        (
            Some(ReasoningEffort::None),
            Some(json!({"effort": "none", "summary": "auto"})),
        ),
    ] {
        let server = MockServer::start().await;
        mount_sse(
            &server,
            sse_body(&[json!({"type": "response.completed", "response": {}})]),
        )
        .await;
        let provider = Arc::new(Provider::OpenAiResponses {
            key: "test-key".into(),
            base: server.uri(),
        });
        let attempt = provider.attempt_identity("responses", 1, "test-model");
        let mut rx = provider.stream_attempt(
            &attempt,
            effort,
            None,
            "s",
            &[Message::user_text("hi")],
            &[],
        );
        while rx.recv().await.is_some() {}
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body.get("reasoning").cloned(),
            expected,
            "effort {effort:?}"
        );
    }
}

/// A relay re-sending the opening lifecycle frames is not a protocol violation.
/// gateway does it — an internal retry replays `response.created` /
/// `response.in_progress` — and treating it as one killed the whole turn over a
/// frame that carries nothing the first one did not.
#[tokio::test]
async fn repeated_lifecycle_frames_are_tolerated_when_the_identity_holds() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.in_progress", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.in_progress", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "m", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "m", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "r", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(
        events.iter().all(|event| event.is_ok()),
        "a repeated opening frame must not fail the stream: {events:?}"
    );
}

// `a_second_response_identity_still_fails_closed` used to sit here. It pinned
// "a second `created` with a different id fails the stream" — written before the
// relay was seen doing exactly that on a retry, with no output in between. The
// scenario is now split by *when* the new id lands, and the two tests above
// carry both halves: followed before output, refused after it. Deleted rather
// than relaxed — a test whose scenario has been re-decided should not keep its
// old name and its old claim.

/// `status` on the opening frames is descriptive and never read. A relay that
/// queues the request and says so has changed nothing about what happens next,
/// so it must not be a protocol violation.
#[tokio::test]
async fn a_queued_status_on_the_opening_frames_is_not_a_violation() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "queued"}}),
            json!({"type": "response.in_progress", "response": {"id": "r", "status": "queued"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "m", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "m", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "r", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(
        events.iter().all(|event| event.is_ok()),
        "a queued opening frame must not fail the stream: {events:?}"
    );
}

/// Some gateways omit `status` from the opening frames entirely (gw_cn's
/// deepseek route sends `response.created` with id/model/usage metadata and no
/// status key at all). Requiring the key was the same mistake as requiring a
/// particular value: the field decides nothing here, so its absence must not
/// kill the round.
#[tokio::test]
async fn opening_frames_without_a_status_key_are_accepted() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "object": "response"}}),
            json!({"type": "response.in_progress", "response": {"id": "r", "object": "response"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "m", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "m", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "r", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(
        events.iter().all(|event| event.is_ok()),
        "an opening frame without a status key must not fail the stream: {events:?}"
    );
}

/// The same gateway opens its parts with `{"type": "summary_text"}` and no
/// `text` key. An opening part holds nothing yet — every character arrives as a
/// delta — so an absent key and `""` say the same thing, on reasoning summaries
/// and message content alike.
#[tokio::test]
async fn opening_parts_without_a_text_key_are_empty() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress"
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "part": {"type": "summary_text"}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "delta": "thinking"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "text": "thinking"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": "thinking"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed",
                "summary": [{"type": "summary_text", "text": "thinking"}]
            }}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "message", "id": "m", "status": "in_progress", "role": "assistant"
            }}),
            json!({"type": "response.content_part.added", "output_index": 1,
                "item_id": "m", "content_index": 0, "part": {"type": "output_text"}}),
            json!({"type": "response.output_text.delta", "output_index": 1,
                "item_id": "m", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 1,
                "item_id": "m", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 1,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "r", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(
        events.iter().all(|event| event.is_ok()),
        "opening parts without a text key must not fail the stream: {events:?}"
    );
}

/// The closing twin is where the text is actually consumed — it has to match
/// what the deltas built — so a missing key there is a claim that cannot be
/// checked, and stays fatal.
#[tokio::test]
async fn a_closing_part_without_a_text_key_still_fails() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress"
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "part": {"type": "summary_text"}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "delta": "thinking"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "text": "thinking"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "part": {"type": "summary_text"}}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("a closing part without a text must fail the stream");
    assert!(!error.is_retryable());
    let rendered = error.to_string();
    assert!(
        rendered.contains("missing or invalid summary part text"),
        "{rendered}"
    );
}

/// The terminal events are the one place `status` decides something — whether
/// the turn ended or was cut short — so there it stays required.
#[tokio::test]
async fn a_terminal_frame_without_a_status_key_still_fails() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "m", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "m", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "r",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("a terminal frame without a status must fail the stream");
    assert!(!error.is_retryable());
    let rendered = error.to_string();
    assert!(
        rendered.contains("missing or invalid response status"),
        "{rendered}"
    );
}

/// A new identity before any output is an upstream restart — the relay retried
/// and is now forwarding the real response. Nothing has been attributed yet, so
/// following it is safe and keeps the turn alive.
#[tokio::test]
async fn a_new_identity_before_any_output_is_followed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "first", "status": "in_progress"}}),
            json!({"type": "response.created", "response": {"id": "retry", "status": "in_progress"}}),
            json!({"type": "response.in_progress", "response": {"id": "retry", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "m", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "m", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "retry", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(
        events.iter().all(|event| event.is_ok()),
        "a restart before output must be followed: {events:?}"
    );
}

/// After output has landed the same frame means the opposite: two responses are
/// sharing one stream, and everything from here would be attributed to the wrong
/// one. Both ids are named so the next reader does not have to guess.
#[tokio::test]
async fn a_new_identity_after_output_still_fails_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.in_progress", "response": {"id": "other", "status": "in_progress"}}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("a conflicting identity after output must fail the stream");
    assert!(!error.is_retryable());
    let rendered = error.to_string();
    assert!(rendered.contains("from r to other"), "{rendered}");
    assert!(rendered.contains("after output had landed"), "{rendered}");
}

/// A reasoning item that only has a summary omits `content` entirely. Absent and
/// empty say the same thing here, and demanding the key made a shape choice
/// upstream into a protocol violation — one that killed the turn after the model
/// had already done the work.
#[tokio::test]
async fn a_reasoning_item_without_a_content_key_is_accepted() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress",
                "summary": []
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "delta": "thought"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "text": "thought"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": "thought"}}),
            // No `content` key at all — the shape this test exists for.
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed",
                "summary": [{"type": "summary_text", "text": "thought"}]
            }}),
            json!({"type": "response.completed", "response": {
                "id": "r", "status": "completed",
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(
        events.iter().all(|event| event.is_ok()),
        "an omitted content key must not fail the stream: {events:?}"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        Ok(StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, .. })) if thinking == "thought"
    )));
}

/// Absent is tolerated; a present-but-wrong type is still bad data, and the
/// error now names what actually arrived.
#[tokio::test]
async fn a_non_array_parts_field_still_fails_and_names_its_type() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress", "summary": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed", "summary": "oops"
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    let error = events
        .into_iter()
        .find_map(|event| event.err())
        .expect("a non-array parts field must fail the stream");
    let rendered = error.to_string();
    assert!(
        rendered.contains("reasoning parts was not an array"),
        "{rendered}"
    );
    assert!(rendered.contains("got string"), "{rendered}");
}

/// Deltas stream for display; complete items arrive whole in
/// output_item.done; usage (with cached split out) rides response.completed.
#[tokio::test]
async fn streams_reasoning_text_and_function_call() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "status": "in_progress",
                "content": [], "summary": []
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0, "delta": "let me"}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0, "delta": " see"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0, "text": "let me see"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0,
                "part": {"type": "summary_text", "text": "let me see"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "status": "completed", "content": [],
                "summary": [{"type": "summary_text", "text": "let me see"}],
                "encrypted_content": "enc-blob"
            }}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "message", "id": "msg_1", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 1,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 1,
                "item_id": "msg_1", "content_index": 0, "delta": "hel"}),
            json!({"type": "response.output_text.delta", "output_index": 1,
                "item_id": "msg_1", "content_index": 0, "delta": "lo"}),
            json!({"type": "response.output_text.done", "output_index": 1,
                "item_id": "msg_1", "content_index": 0, "text": "hello"}),
            json!({"type": "response.content_part.done", "output_index": 1,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": "hello"}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}]
            }}),
            json!({"type": "response.output_item.added", "output_index": 2, "item": {
                "type": "function_call", "id": "fc_1", "status": "in_progress",
                "call_id": "call_9", "name": "bash", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 2,
                "item_id": "fc_1", "delta": "{\"command\":\"ls\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 2,
                "item_id": "fc_1", "arguments": "{\"command\":\"ls\"}"}),
            json!({"type": "response.output_item.done", "output_index": 2, "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_9",
                "name": "bash", "arguments": "{\"command\":\"ls\"}", "status": "completed"
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed", "usage": {
                "input_tokens": 1000, "output_tokens": 30,
                "input_tokens_details": {"cached_tokens": 900}
            }}}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::ThinkingDelta(t) if t == "let me"));
    assert!(matches!(&ok[1], StreamEvent::ThinkingDelta(t) if t == " see"));
    assert!(matches!(
        &ok[2],
        StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, signature })
            if thinking == "let me see" && signature == "enc-blob"
    ));
    assert!(matches!(&ok[3], StreamEvent::TextDelta(t) if t == "hel"));
    assert!(matches!(&ok[4], StreamEvent::TextDelta(t) if t == "lo"));
    assert!(matches!(
        &ok[5],
        StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "hello"
    ));
    assert!(matches!(
        &ok[6],
        StreamEvent::BlockDone(AssistantBlock::ToolUse { id, name, input })
            if id == "call_9" && name == "bash" && input == &json!({"command": "ls"})
    ));
    assert!(matches!(
        &ok[7],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::ToolUse,
            usage: Some(u),
        } if *u == Usage {
                input_tokens: 100,
                output_tokens: 30,
                cache_read_input_tokens: 900,
                cache_creation_input_tokens: 0,
            }
    ));
    assert_eq!(ok.len(), 8);
}

#[tokio::test]
async fn reasoning_output_item_status_may_be_omitted() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "summary": [], "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "summary": [], "content": [],
                "encrypted_content": "enc"
            }}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
        ]),
    )
    .await;

    let events: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(matches!(
        &events[0],
        StreamEvent::BlockDone(AssistantBlock::Thinking {
            thinking,
            signature
        }) if thinking.is_empty() && signature == "enc"
    ));
    assert!(matches!(
        &events[1],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        }
    ));
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn output_item_status_stays_strict_outside_omitted_reasoning() {
    let cases = vec![
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "role": "assistant", "content": []
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "call_id": "c", "name": "bash",
                "arguments": ""
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress", "role": "assistant",
                "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "role": "assistant", "content": []
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "status": "in_progress",
                "call_id": "c", "name": "bash", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc", "arguments": ""}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "call_id": "c", "name": "bash",
                "arguments": ""
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed", "summary": [],
                "content": []
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": null, "summary": [],
                "content": []
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress", "summary": [],
                "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress", "summary": [],
                "content": []
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress", "summary": [],
                "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": null, "summary": [],
                "content": []
            }}),
        ],
    ];

    let server = MockServer::start().await;
    for (index, wire) in cases.into_iter().enumerate() {
        mount_sse(&server, sse_body(&wire)).await;
        let events = collect(responses(&server)).await;
        assert_eq!(events.len(), 1, "case {index}");
        let error = events.into_iter().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol, "case {index}");
        assert!(!error.is_retryable(), "case {index}");
        assert!(!error.after_semantic_output(), "case {index}");
        server.reset().await;
    }
}

/// A response cut off by max_output_tokens maps to the "length" stop_reason
/// so the agent's truncation-continue nudge applies unchanged.
#[tokio::test]
async fn incomplete_maps_to_length_stop_reason() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_2", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "msg_2", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "msg_2", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "msg_2", "content_index": 0, "delta": "partial"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "msg_2", "content_index": 0, "text": "partial"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "msg_2", "content_index": 0,
                "part": {"type": "output_text", "text": "partial"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "msg_2", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "partial"}]
            }}),
            json!({"type": "response.incomplete", "response": {
                "id": "resp_2", "status": "incomplete",
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 10, "output_tokens": 8192}
            }}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(
        ok.last().unwrap(),
        StreamEvent::Terminal {
            outcome: AssistantOutcome::OutputLimit(OutputLimitKind::MaxOutputTokens),
            usage: Some(_),
        }
    ));
}

/// The three shapes a truncated response actually takes on gw_cn's deepseek
/// route (captured 2026-09-11 against ai-coding-sr-bj-direct). All three stamp
/// `status: "incomplete"` on the items they did emit and close with
/// `incomplete_details.reason: "length"`; every one of them used to die as a
/// non-retryable protocol error, throwing away a full minute of work.
///
/// Shape one: xhigh reasoning eats the whole output budget on its own — the
/// reasoning item closes incomplete and carries no encrypted_content, and no
/// message item is ever opened.
#[tokio::test]
async fn reasoning_truncated_by_the_output_budget_becomes_an_output_limit() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress"
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "delta": "counting primes"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "text": "counting primes"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": "counting primes"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "incomplete",
                "summary": [{"type": "summary_text", "text": "counting primes"}]
            }}),
            json!({"type": "response.incomplete", "response": {
                "id": "r", "status": "incomplete",
                "incomplete_details": {"reason": "length"},
                "usage": {"input_tokens": 133, "output_tokens": 8192,
                    "input_tokens_details": {"cached_tokens": 0}}
            }}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(
        ok,
        vec![
            StreamEvent::ThinkingDelta("counting primes".into()),
            StreamEvent::BlockDone(AssistantBlock::Thinking {
                thinking: "counting primes".into(),
                signature: String::new(),
            }),
            StreamEvent::Terminal {
                outcome: AssistantOutcome::OutputLimit(OutputLimitKind::MaxOutputTokens),
                usage: Some(Usage {
                    input_tokens: 133,
                    output_tokens: 8192,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                }),
            },
        ]
    );
}

/// Shape two: the answer itself is cut mid-sentence. Reasoning closed cleanly,
/// the message item closes incomplete — and the partial text it did produce is
/// exactly what the agent's truncation-continue recovery stitches together, so
/// it must survive.
#[tokio::test]
async fn a_message_truncated_mid_text_keeps_the_partial_answer() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress"
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed",
                "summary": [], "encrypted_content": "enc"
            }}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "message", "id": "msg", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 1,
                "item_id": "msg", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 1,
                "item_id": "msg", "content_index": 0, "delta": "The printing press"}),
            json!({"type": "response.output_text.done", "output_index": 1,
                "item_id": "msg", "content_index": 0, "text": "The printing press"}),
            json!({"type": "response.content_part.done", "output_index": 1,
                "item_id": "msg", "content_index": 0,
                "part": {"type": "output_text", "text": "The printing press"}}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "message", "id": "msg", "status": "incomplete", "role": "assistant",
                "content": [{"type": "output_text", "text": "The printing press"}]
            }}),
            json!({"type": "response.incomplete", "response": {
                "id": "r", "status": "incomplete",
                "incomplete_details": {"reason": "length"},
                "usage": {"input_tokens": 34, "output_tokens": 400,
                    "input_tokens_details": {"cached_tokens": 0}}
            }}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(
        ok,
        vec![
            StreamEvent::BlockDone(AssistantBlock::Thinking {
                thinking: String::new(),
                signature: "enc".into(),
            }),
            StreamEvent::TextDelta("The printing press".into()),
            StreamEvent::BlockDone(AssistantBlock::Text {
                text: "The printing press".into(),
            }),
            StreamEvent::Terminal {
                outcome: AssistantOutcome::OutputLimit(OutputLimitKind::MaxOutputTokens),
                usage: Some(Usage {
                    input_tokens: 34,
                    output_tokens: 400,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                }),
            },
        ]
    );
}

/// Shape three, and the reason item status cannot be read as "this item is
/// damaged": the budget ran out between items, so the two function calls that
/// did land are byte-complete — arguments done, valid JSON, final value equal
/// to the deltas — and both are stamped `incomplete` anyway. They are the
/// model's real next step, so the turn dispatches them instead of dying.
#[tokio::test]
async fn complete_function_calls_stamped_incomplete_are_still_dispatched() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_1", "status": "in_progress",
                "call_id": "call_1", "name": "read_file", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_1", "delta": "{\"path\":\"a1.txt\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc_1", "arguments": "{\"path\":\"a1.txt\"}"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_1", "status": "incomplete",
                "call_id": "call_1", "name": "read_file",
                "arguments": "{\"path\":\"a1.txt\"}"
            }}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "function_call", "id": "fc_2", "status": "in_progress",
                "call_id": "call_2", "name": "read_file", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 1,
                "item_id": "fc_2", "delta": "{\"path\":\"a2.txt\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 1,
                "item_id": "fc_2", "arguments": "{\"path\":\"a2.txt\"}"}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "function_call", "id": "fc_2", "status": "incomplete",
                "call_id": "call_2", "name": "read_file",
                "arguments": "{\"path\":\"a2.txt\"}"
            }}),
            json!({"type": "response.incomplete", "response": {
                "id": "r", "status": "incomplete",
                "incomplete_details": {"reason": "length"},
                "usage": {"input_tokens": 200, "output_tokens": 700,
                    "input_tokens_details": {"cached_tokens": 0}}
            }}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert_eq!(
        ok,
        vec![
            StreamEvent::BlockDone(AssistantBlock::ToolUse {
                id: "call_1".into(),
                name: "read_file".into(),
                input: json!({"path": "a1.txt"}),
            }),
            StreamEvent::BlockDone(AssistantBlock::ToolUse {
                id: "call_2".into(),
                name: "read_file".into(),
                input: json!({"path": "a2.txt"}),
            }),
            StreamEvent::Terminal {
                outcome: AssistantOutcome::ToolUse,
                usage: Some(Usage {
                    input_tokens: 200,
                    output_tokens: 700,
                    cache_read_input_tokens: 0,
                    cache_creation_input_tokens: 0,
                }),
            },
        ]
    );
}

/// The relaxation buys exactly one new statement — "the budget ran out" — and
/// it must agree with the terminal. An item that says truncated under a
/// response that says completed describes two different responses, and neither
/// half can be trusted to pick.
#[tokio::test]
async fn a_truncated_item_under_a_completed_response_fails_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress"
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "incomplete",
                "summary": [], "encrypted_content": "enc"
            }}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
        ]),
    )
    .await;

    let mut events = collect(responses(&server)).await;
    let error = events.pop().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(
        error
            .to_string()
            .contains("reported truncation but the response completed"),
        "{error}"
    );
}

/// Only `incomplete` was let in, and a rejected status now names itself: the
/// report that started this plan said only "was not completed", which cost a
/// round of guessing about what the wire had actually sent.
#[tokio::test]
async fn an_unknown_final_item_status_still_fails_and_names_itself() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "msg", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "msg", "status": "failed", "role": "assistant",
                "content": []
            }}),
        ]),
    )
    .await;

    let mut events = collect(responses(&server)).await;
    let error = events.pop().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(
        error
            .to_string()
            .contains(r#"message item status was "failed""#),
        "{error}"
    );
}

#[tokio::test]
async fn failed_with_context_error_maps_to_overflow() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[json!({"type": "response.failed", "response": {"error": {
            "code": "context_length_exceeded",
            "message": "input too long",
        }}})]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::ContextOverflow);
}

/// `response.failed` and the top-level `error` event share the unified
/// classification: transient upstream conditions are retryable, client-side /
/// permanent classes stay fatal.
#[tokio::test]
async fn failed_and_error_events_classify_transient_vs_fatal() {
    let server = MockServer::start().await;

    for (event, retryable, needle) in [
        (
            json!({"type": "response.failed", "response": {"error": {"code": "server_error"}}}),
            true,
            "server_error",
        ),
        (
            json!({"type": "response.failed", "response": {"error": {"code": "invalid_request_error"}}}),
            false,
            "invalid_request_error",
        ),
        (
            json!({"type": "error", "code": "upstream_error", "message": "temporarily unavailable"}),
            true,
            "upstream_error",
        ),
        (
            json!({"type": "error", "code": "insufficient_quota", "message": "no credit"}),
            false,
            "insufficient_quota",
        ),
    ] {
        mount_sse(&server, sse_body(&[event])).await;
        let events = collect(responses(&server)).await;
        assert_eq!(events.len(), 1, "{needle}");
        let error = events.into_iter().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol, "{needle}");
        assert_eq!(error.is_retryable(), retryable, "{needle}");
        assert!(error.to_string().contains(needle), "{needle}");
        server.reset().await;
    }
}

/// A stream that dies without a terminal event yields an error, not a
/// fabricated Done — the agent retries on it.
#[tokio::test]
async fn stream_dying_mid_flight_is_an_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_3", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "msg_3", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "msg_3", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "msg_3", "content_index": 0, "delta": "par"}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamEvent::TextDelta(t) if t == "par"
    ));
    let error = events.into_iter().nth(1).unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.is_retryable());
    assert!(error.to_string().contains("ended before completion"));
}

/// The arguments string is the model's, on this rail as on the others: an
/// unreadable one completes as a call the turn can answer instead of killing
/// the stream.
#[tokio::test]
async fn malformed_function_arguments_come_back_as_an_invalid_call() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_4", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_4", "status": "in_progress",
                "call_id": "call_1", "name": "bash", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_4", "delta": "{oops"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc_4", "arguments": "{oops"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_4", "call_id": "call_1", "name": "bash",
                "arguments": "{oops", "status": "completed"
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_4", "status": "completed"}}),
        ]),
    )
    .await;

    let events: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|event| event.unwrap())
        .collect();
    let [
        StreamEvent::BlockDone(block),
        StreamEvent::Terminal { outcome, .. },
    ] = &events[..]
    else {
        panic!("expected one invalid call and a terminal, got {events:?}");
    };
    assert_eq!(outcome, &AssistantOutcome::ToolUse);
    let AssistantBlock::InvalidToolUse {
        id,
        name,
        raw,
        error,
    } = block
    else {
        panic!("expected an invalid call, got {block:?}");
    };
    assert_eq!(
        (id.as_str(), name.as_str(), raw.as_str()),
        ("call_1", "bash", "{oops")
    );
    assert!(error.contains("column"), "{error}");
}

/// The proxy may stream compact argument deltas but echo a pretty-printed
/// object in `function_call_arguments.done` and `output_item.done`. Both
/// encodings parse to the same JSON value, so the round succeeds and the tool
/// input is the parsed object — the exact bytes observed on a real gpt-5.6 run.
#[tokio::test]
async fn function_arguments_agree_across_pretty_and_compact() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_1", "status": "in_progress",
                "call_id": "call_9", "name": "read", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_1", "delta": "{\"limit\":3,"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_1", "delta": "\"offset\":1,"}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_1", "delta": "\"path\":\"README.md\"}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc_1",
                "arguments": "{\"limit\": 3, \"offset\": 1, \"path\": \"README.md\"}"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_9", "name": "read",
                "arguments": "{\"limit\": 3, \"offset\": 1, \"path\": \"README.md\"}",
                "status": "completed"
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ]),
    )
    .await;

    let events: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(matches!(
        &events[0],
        StreamEvent::BlockDone(AssistantBlock::ToolUse { id, name, input })
            if id == "call_9" && name == "read"
                && input == &json!({"limit": 3, "offset": 1, "path": "README.md"})
    ));
    assert!(matches!(
        &events[1],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::ToolUse,
            ..
        }
    ));
    assert_eq!(events.len(), 2);
}

/// Only whitespace/format differences are tolerated: when the accumulated
/// delta and the `.done` arguments parse to genuinely different JSON values,
/// the round still fails closed at the first comparison site.
#[tokio::test]
async fn function_arguments_differing_values_still_fail_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_5", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_5", "status": "in_progress",
                "call_id": "call_1", "name": "bash", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.delta", "output_index": 0,
                "item_id": "fc_5", "delta": "{\"a\":1}"}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc_5", "arguments": "{\"a\":2}"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc_5", "call_id": "call_1", "name": "bash",
                "arguments": "{\"a\":2}", "status": "completed"
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_5", "status": "completed"}}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1, "value divergence must not yield a ToolUse");
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(
        error
            .to_string()
            .contains("arguments done did not match accumulated delta")
    );
}

#[tokio::test]
async fn refusal_fields_stream_and_finish_as_refused() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "refusal", "refusal": ""}}),
            json!({"type": "response.refusal.delta", "output_index": 0,
                "item_id": "m", "content_index": 0, "delta": "cannot"}),
            json!({"type": "response.refusal.done", "output_index": 0,
                "item_id": "m", "content_index": 0, "refusal": "cannot"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "m", "content_index": 0,
                "part": {"type": "refusal", "refusal": "cannot"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed", "role": "assistant",
                "content": [{"type": "refusal", "refusal": "cannot"}]
            }}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
        ]),
    )
    .await;

    let events: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(matches!(&events[0], StreamEvent::TextDelta(text) if text == "cannot"));
    assert!(matches!(
        &events[1],
        StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "cannot"
    ));
    assert!(matches!(
        &events[2],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::Refused,
            ..
        }
    ));
    assert_eq!(events.len(), 3);
}

#[tokio::test]
async fn multiple_reasoning_parts_preserve_the_streamed_text_exactly() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress",
                "summary": [], "content": []
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "delta": "a"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "text": "a"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": "a"}}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 1,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs", "summary_index": 1, "delta": "b"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 1, "text": "b"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 1,
                "part": {"type": "summary_text", "text": "b"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed", "content": [],
                "summary": [
                    {"type": "summary_text", "text": "a"},
                    {"type": "summary_text", "text": "b"}
                ]
            }}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
        ]),
    )
    .await;

    let events: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(matches!(&events[0], StreamEvent::ThinkingDelta(text) if text == "a"));
    assert!(matches!(&events[1], StreamEvent::ThinkingDelta(text) if text == "b"));
    assert!(matches!(
        &events[2],
        StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, .. }) if thinking == "ab"
    ));
    assert!(matches!(&events[3], StreamEvent::Terminal { .. }));
    assert_eq!(events.len(), 4);
}

#[tokio::test]
async fn final_function_item_must_repeat_required_identity() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "status": "in_progress",
                "call_id": "c1", "name": "bash", "arguments": ""
            }}),
            json!({"type": "response.function_call_arguments.done", "output_index": 0,
                "item_id": "fc", "arguments": ""}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "status": "completed", "arguments": ""
            }}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.after_semantic_output());
}

#[tokio::test]
async fn signed_empty_reasoning_is_the_only_no_part_semantic_block() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress",
                "summary": [], "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed",
                "summary": [], "content": [], "encrypted_content": "enc"
            }}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 1, "item": {
                "type": "message", "id": "m", "status": "completed",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
        ]),
    )
    .await;

    let events: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(Result::unwrap)
        .collect();
    assert!(matches!(
        &events[0],
        StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, signature })
            if thinking.is_empty() && signature == "enc"
    ));
    assert!(matches!(
        &events[1],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        }
    ));
    assert_eq!(events.len(), 2);
}

#[tokio::test]
async fn unclosed_or_forged_output_items_fail_before_blocks() {
    let cases = vec![
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "status": "in_progress",
                "call_id": "c1", "name": "bash", "arguments": ""
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "function_call", "id": "fc", "status": "completed",
                "call_id": "c1", "name": "bash", "arguments": "{}"
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "m", "status": "completed",
                "role": "assistant", "content": [{"type": "output_text", "text": "forged"}]
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress",
                "summary": [], "content": []
            }}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "completed",
                "summary": [{"type": "summary_text", "text": "forged"}], "content": []
            }}),
        ],
        vec![
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "a", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.output_item.added", "output_index": 1, "item": {
                "type": "message", "id": "b", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
        ],
    ];

    let server = MockServer::start().await;
    for (index, wire) in cases.into_iter().enumerate() {
        mount_sse(&server, sse_body(&wire)).await;
        let events = collect(responses(&server)).await;
        assert_eq!(events.len(), 1, "case {index}");
        let error = events.into_iter().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol, "case {index}");
        assert!(!error.after_semantic_output(), "case {index}");
        server.reset().await;
    }
}

#[tokio::test]
async fn empty_reasoning_parts_do_not_create_semantic_deltas() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs", "status": "in_progress",
                "summary": [], "content": []
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0, "text": ""}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs", "summary_index": 1,
                "part": {"type": "summary_text", "text": ""}}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert!(error.is_retryable());
    assert!(!error.after_semantic_output());
}

#[tokio::test]
async fn terminal_is_low_latency_but_later_complete_semantic_frames_fail_closed() {
    let terminal = [
        json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
        json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
    ];
    let server = MockServer::start().await;

    let mut partial_tail = sse_body(&terminal);
    partial_tail.push_str("data: [DO");
    mount_sse(&server, partial_tail).await;
    let events = collect(responses(&server)).await;
    assert!(matches!(
        events.as_slice(),
        [Ok(StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        })]
    ));

    server.reset().await;
    let duplicate = sse_body(&[
        terminal[0].clone(),
        terminal[1].clone(),
        terminal[1].clone(),
    ]);
    mount_sse(&server, duplicate).await;
    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());

    server.reset().await;
    mount_bytes(&server, b"data: \xff\n\n".to_vec()).await;
    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
}

/// A `codex.*` vendor out-of-band event (rate-limit telemetry) landing mid
/// stream is skipped: it produces no StreamEvent and does not disturb the
/// surrounding semantic sequence. Mirrors anthropic's `ping` handling.
#[tokio::test]
async fn out_of_band_codex_event_is_ignored_mid_stream() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "codex.rate_limits",
                "plan_type": "pro",
                "rate_limits": {
                    "primary": {"used_percent": 12.5, "window_minutes": 300, "reset_at": "2026-08-24T12:00:00Z"},
                    "secondary": {"used_percent": 3.0, "window_minutes": 10080, "reset_at": "2026-08-30T00:00:00Z"}
                },
                "credits": 42,
                "metered_limit_name": "gpt-5"}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "msg_1", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::TextDelta(t) if t == "hi"));
    assert!(matches!(
        &ok[1],
        StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "hi"
    ));
    assert!(matches!(
        &ok[2],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        }
    ));
    assert_eq!(ok.len(), 3);
}

/// A `codex.*` out-of-band event arriving after the semantic terminal is
/// exempt from the terminal-after guard (same as anthropic's `ping`): it is
/// skipped and the single Terminal still stands.
#[tokio::test]
async fn out_of_band_codex_event_after_terminal_is_ignored() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
            json!({"type": "codex.rate_limits", "plan_type": "pro", "credits": 7}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(matches!(
        events.as_slice(),
        [Ok(StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        })]
    ));
}

/// gateway's real reasoning shape: every summary part is opened and streamed,
/// but only the last one is closed with `text.done` + `part.done`. Two
/// independent captures, 24 reasoning items, all this shape — so the parts are
/// independent by index, not strictly nested. Correctness rides on the item
/// boundary, which still matches each accumulated part against the final array.
#[tokio::test]
async fn reasoning_summary_parts_may_stay_open_until_the_item_closes() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "status": "in_progress",
                "content": [], "summary": []
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0, "delta": "first"}),
            // No done for index 0 — index 1 opens on top of it.
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs_1", "summary_index": 1,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs_1", "summary_index": 1, "delta": "second"}),
            json!({"type": "response.reasoning_summary_text.done", "output_index": 0,
                "item_id": "rs_1", "summary_index": 1, "text": "second"}),
            json!({"type": "response.reasoning_summary_part.done", "output_index": 0,
                "item_id": "rs_1", "summary_index": 1,
                "part": {"type": "summary_text", "text": "second"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "status": "completed", "content": [],
                "summary": [
                    {"type": "summary_text", "text": "first"},
                    {"type": "summary_text", "text": "second"},
                ],
                "encrypted_content": "enc-blob"
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::ThinkingDelta(t) if t == "first"));
    assert!(matches!(&ok[1], StreamEvent::ThinkingDelta(t) if t == "second"));
    assert!(matches!(
        &ok[2],
        StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, signature })
            if thinking == "firstsecond" && signature == "enc-blob"
    ));
    assert!(matches!(
        &ok[3],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        }
    ));
    assert_eq!(ok.len(), 4);
}

/// Relaxing the per-part close did not relax what actually guarantees the
/// content: a never-closed part whose final text disagrees with the streamed
/// deltas still fails closed at the item boundary.
#[tokio::test]
async fn unclosed_reasoning_part_still_fails_when_final_text_diverges() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "status": "in_progress",
                "content": [], "summary": []
            }}),
            json!({"type": "response.reasoning_summary_part.added", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0,
                "part": {"type": "summary_text", "text": ""}}),
            json!({"type": "response.reasoning_summary_text.delta", "output_index": 0,
                "item_id": "rs_1", "summary_index": 0, "delta": "streamed"}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "reasoning", "id": "rs_1", "status": "completed", "content": [],
                "summary": [{"type": "summary_text", "text": "something else"}],
                "encrypted_content": "enc-blob"
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    let error = events.into_iter().find_map(|e| e.err()).unwrap();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert_eq!(
        error.to_string(),
        "provider protocol error: openai-responses final reasoning text did not match streamed text"
    );
}
/// gateway's `keepalive` heartbeat, on the real wire shape, is skipped like
/// `codex.*`. It arrives while the model is still thinking — the more thinking,
/// the more of them — so a high reasoning effort makes it the norm, not an edge
/// case, and treating it as fatal killed every xhigh turn.
#[tokio::test]
async fn keepalive_event_is_ignored_mid_stream() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"sequence_number": 2, "type": "keepalive"}),
            json!({"sequence_number": 3, "type": "keepalive"}),
            json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "msg_1", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::TextDelta(t) if t == "hi"));
    assert!(matches!(
        &ok[1],
        StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "hi"
    ));
    assert!(matches!(
        &ok[2],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        }
    ));
    assert_eq!(ok.len(), 3);
}

/// `responsesapi.websocket_timing` is transport telemetry from the same proxy.
/// It surfaced at the tail of an 856-second review and killed the whole turn:
/// like `keepalive`, it only appears in runs long enough that a short probe
/// never samples it.
#[tokio::test]
async fn vendor_namespaced_timing_event_is_ignored_mid_stream() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "responsesapi.websocket_timing", "sequence_number": 2, "ms": 17}),
            json!({"type": "response.in_progress", "response": {"id": "resp_1", "status": "in_progress"}}),
            json!({"type": "response.output_item.added", "output_index": 0, "item": {
                "type": "message", "id": "msg_1", "status": "in_progress",
                "role": "assistant", "content": []
            }}),
            json!({"type": "response.content_part.added", "output_index": 0,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": ""}}),
            json!({"type": "response.output_text.delta", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "delta": "hi"}),
            json!({"type": "response.output_text.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0, "text": "hi"}),
            json!({"type": "response.content_part.done", "output_index": 0,
                "item_id": "msg_1", "content_index": 0,
                "part": {"type": "output_text", "text": "hi"}}),
            json!({"type": "response.output_item.done", "output_index": 0, "item": {
                "type": "message", "id": "msg_1", "status": "completed", "role": "assistant",
                "content": [{"type": "output_text", "text": "hi"}]
            }}),
            json!({"type": "response.completed", "response": {"id": "resp_1", "status": "completed"}}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(responses(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::TextDelta(t) if t == "hi"));
    assert_eq!(ok.len(), 3);
}

/// A `keepalive` after the semantic terminal is exempt from the terminal-after
/// guard too — both rejection points treat the out-of-band list alike.
#[tokio::test]
async fn keepalive_after_terminal_is_ignored() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "response.completed", "response": {"id": "r", "status": "completed"}}),
            json!({"sequence_number": 9, "type": "keepalive"}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert!(matches!(
        events.as_slice(),
        [Ok(StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        })]
    ));
}

/// The out-of-band window is narrow: any event outside the recognized list is
/// still a fatal protocol error (fail-closed boundary preserved), and the
/// message names the offender — identifying `keepalive` otherwise cost an SSE
/// capture, because the error said only "an unknown semantic event".
#[tokio::test]
async fn unknown_non_codex_event_still_fails_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({"type": "some.unknown.event"}),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert_eq!(
        error.to_string(),
        "provider protocol error: openai-responses returned an unknown semantic event: \
         some.unknown.event"
    );
}

/// A hostile or runaway event name cannot turn the error into an unbounded
/// echo of provider bytes.
#[tokio::test]
async fn unknown_event_name_is_bounded_in_the_error() {
    let server = MockServer::start().await;
    let long = "x".repeat(500);
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created", "response": {"id": "r", "status": "in_progress"}}),
            json!({ "type": long }),
        ]),
    )
    .await;

    let events = collect(responses(&server)).await;
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "provider protocol error: openai-responses returned an unknown semantic event: {}…",
            "x".repeat(80)
        )
    );
}

#[tokio::test]
async fn malformed_sse_json_is_a_terminal_protocol_error() {
    let server = MockServer::start().await;
    mount_sse(&server, "data: {oops\n\n".into()).await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
}
