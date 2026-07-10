//! HTTP contract tests for the OpenAI Responses API adapter: the Responses
//! SSE event family in, StreamEvents out, plus the stateless request shape.

use std::sync::Arc;

use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;
use kloop_provider::Provider;
use serde_json::json;
use serde_json::Value;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

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
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
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

async fn collect(provider: Provider) -> Vec<anyhow::Result<StreamEvent>> {
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
        Message::assistant(vec![
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: "enc-blob".into(),
            },
            ContentBlock::ToolUse {
                id: "call_1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            },
        ]),
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
            "max_output_tokens": 8192,
            "parallel_tool_calls": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": true,
        })
    );
}

/// Deltas stream for display; complete items arrive whole in
/// output_item.done; usage (with cached split out) rides response.completed.
#[tokio::test]
async fn streams_reasoning_text_and_function_call() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.created"}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": "let me"}),
            json!({"type": "response.reasoning_summary_text.delta", "delta": " see"}),
            // Field set mirrors the real wire (2026-07-10, live capture):
            // reasoning carries content/id alongside summary/encrypted_content.
            json!({"type": "response.output_item.done", "item": {
                "type": "reasoning", "id": "rs_1", "content": [],
                "summary": [{"type": "summary_text", "text": "let me"},
                            {"type": "summary_text", "text": " see"}],
                "encrypted_content": "enc-blob",
            }}),
            json!({"type": "response.output_text.delta", "delta": "hel"}),
            json!({"type": "response.output_text.delta", "delta": "lo"}),
            json!({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "hello"}],
            }}),
            json!({"type": "response.output_item.done", "item": {
                "type": "function_call", "id": "fc_1", "call_id": "call_9",
                "name": "bash", "arguments": "{\"command\":\"ls\"}",
                "status": "completed",
            }}),
            json!({"type": "response.completed", "response": {"usage": {
                "input_tokens": 1000, "output_tokens": 30,
                "input_tokens_details": {"cached_tokens": 900},
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
        StreamEvent::BlockDone(ContentBlock::Thinking { thinking, signature })
            if thinking == "let me\n see" && signature == "enc-blob"
    ));
    assert!(matches!(&ok[3], StreamEvent::TextDelta(t) if t == "hel"));
    assert!(matches!(&ok[4], StreamEvent::TextDelta(t) if t == "lo"));
    assert!(matches!(
        &ok[5],
        StreamEvent::BlockDone(ContentBlock::Text { text }) if text == "hello"
    ));
    assert!(matches!(
        &ok[6],
        StreamEvent::BlockDone(ContentBlock::ToolUse { id, name, input })
            if id == "call_9" && name == "bash" && input == &json!({"command": "ls"})
    ));
    assert!(matches!(
        &ok[7],
        StreamEvent::Done { stop_reason: None, usage: Some(u) }
            if *u == Usage {
                input_tokens: 100,
                output_tokens: 30,
                cache_read_input_tokens: 900,
                cache_creation_input_tokens: 0,
            }
    ));
    assert_eq!(ok.len(), 8);
}

/// A response cut off by max_output_tokens maps to the "length" stop_reason
/// so the agent's truncation-continue nudge applies unchanged.
#[tokio::test]
async fn incomplete_maps_to_length_stop_reason() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "response.output_text.delta", "delta": "partial"}),
            json!({"type": "response.output_item.done", "item": {
                "type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "partial"}],
            }}),
            json!({"type": "response.incomplete", "response": {
                "incomplete_details": {"reason": "max_output_tokens"},
                "usage": {"input_tokens": 10, "output_tokens": 8192},
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
        StreamEvent::Done { stop_reason: Some(r), usage: Some(_) } if r == "length"
    ));
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
    let err = events.into_iter().next().unwrap().unwrap_err();
    assert!(
        err.downcast_ref::<OverflowError>().is_some(),
        "expected OverflowError, got: {err:#}"
    );
}

/// A stream that dies without a terminal event yields an error, not a
/// fabricated Done — the agent retries on it.
#[tokio::test]
async fn stream_dying_mid_flight_is_an_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[json!({"type": "response.output_text.delta", "delta": "par"})]),
    )
    .await;

    let events = collect(responses(&server)).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamEvent::TextDelta(t) if t == "par"
    ));
    let err = events.into_iter().nth(1).unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("ended before completion"));
}
