//! HTTP contract tests for the Anthropic SSE adapter: scripted wire events
//! in, StreamEvent sequences out. What the live API validated once, these
//! keep validated forever.

use std::sync::Arc;

use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;
use kloop_provider::Provider;
use kloop_provider::ThinkingMode;
use serde_json::json;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

fn sse_body(events: &[serde_json::Value]) -> String {
    events
        .iter()
        .map(|e| format!("event: {}\ndata: {}\n\n", e["type"].as_str().unwrap(), e))
        .collect()
}

async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(server)
        .await;
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

fn anthropic(server: &MockServer) -> Provider {
    Provider::Anthropic {
        key: "test-key".into(),
        base: server.uri(),
        cache: true,
        thinking: ThinkingMode::Unset,
    }
}

#[tokio::test]
async fn streams_text_and_tool_use_with_usage() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 120}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hel"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "bash"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "{\"comm"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "input_json_delta", "partial_json": "and\":\"ls\"}"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_delta", "delta": {"stop_reason": "tool_use"}, "usage": {"output_tokens": 30}}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    let ok: Vec<StreamEvent> = events.into_iter().map(|e| e.unwrap()).collect();

    // Deltas stream through, blocks finalize accumulated, Done carries
    // stop_reason + usage.
    assert!(matches!(&ok[0], StreamEvent::TextDelta(t) if t == "hel"));
    assert!(matches!(&ok[1], StreamEvent::TextDelta(t) if t == "lo"));
    assert!(
        matches!(&ok[2], StreamEvent::BlockDone(ContentBlock::Text { text }) if text == "hello")
    );
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(ContentBlock::ToolUse { id, name, input })
            if id == "t1" && name == "bash" && input == &json!({"command": "ls"})
    ));
    assert!(matches!(
        &ok[4],
        StreamEvent::Done { stop_reason: Some(r), usage: Some(u) }
            if r == "tool_use" && *u == Usage { input_tokens: 120, output_tokens: 30, ..Default::default() }
    ));
    assert_eq!(ok.len(), 5);
}

/// The caching request contract, asserted whole-object: system becomes a
/// one-block array with the tools+system breakpoint, and exactly one message
/// breakpoint sits on the LAST content block of the LAST message.
#[tokio::test]
async fn request_body_carries_cache_breakpoints() {
    let server = MockServer::start().await;
    mount_sse(&server, sse_body(&[json!({"type": "message_stop"})])).await;
    let provider = Arc::new(anthropic(&server));
    let messages = vec![
        Message::user_text("hi"),
        Message::assistant(vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "bash".into(),
            input: json!({"command": "ls"}),
        }]),
        Message {
            role: kloop_protocol::Role::User,
            content: vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "ok".into(),
                    is_error: false,
                },
                ContentBlock::Text {
                    text: "continue".into(),
                },
            ],
        },
    ];
    let mut rx = provider.stream("test-model", "be brief", &messages, &[]);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "max_tokens": 8192,
            "system": [{
                "type": "text",
                "text": "be brief",
                "cache_control": {"type": "ephemeral"},
            }],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
                {"role": "assistant", "content": [{
                    "type": "tool_use", "id": "t1", "name": "bash",
                    "input": {"command": "ls"},
                }]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"},
                    {
                        "type": "text", "text": "continue",
                        "cache_control": {"type": "ephemeral"},
                    },
                ]},
            ],
            "tools": [],
            "stream": true,
        })
    );
}

/// With caching off the request is byte-identical to the pre-caching shape:
/// plain string system, no cache_control anywhere.
#[tokio::test]
async fn cache_off_sends_plain_request() {
    let server = MockServer::start().await;
    mount_sse(&server, sse_body(&[json!({"type": "message_stop"})])).await;
    let provider = Arc::new(Provider::Anthropic {
        key: "test-key".into(),
        base: server.uri(),
        cache: false,
        thinking: ThinkingMode::Unset,
    });
    let mut rx = provider.stream("test-model", "be brief", &[Message::user_text("hi")], &[]);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "max_tokens": 8192,
            "system": "be brief",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]},
            ],
            "tools": [],
            "stream": true,
        })
    );
}

/// Cache usage fields from message_start survive into Done: they are context
/// the window still holds, reported next to (not inside) input_tokens.
#[tokio::test]
async fn cache_usage_fields_are_parsed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "message_start", "message": {"usage": {
                "input_tokens": 10,
                "cache_read_input_tokens": 900,
                "cache_creation_input_tokens": 50,
            }}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(anthropic(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(
        &ok[0],
        StreamEvent::Done { usage: Some(u), .. }
            if *u == Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 900,
                cache_creation_input_tokens: 50,
            }
    ));
}

#[tokio::test]
async fn unknown_block_kinds_are_ignored() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "s1"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{}"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 1, "delta": {"type": "text_delta", "text": "ok"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(anthropic(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    // Only the text block survives: delta + done + Done.
    assert_eq!(ok.len(), 3);
    assert!(matches!(&ok[1], StreamEvent::BlockDone(ContentBlock::Text { text }) if text == "ok"));
}

/// The thinking SSE contract: thinking_delta streams as ThinkingDelta events,
/// signature_delta accumulates silently, and the finished block carries both
/// — followed by a complete-at-start redacted_thinking block.
#[tokio::test]
async fn thinking_blocks_stream_and_finalize_with_signature() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "let me"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": " see"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "blob"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "answer"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(anthropic(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::ThinkingDelta(t) if t == "let me"));
    assert!(matches!(&ok[1], StreamEvent::ThinkingDelta(t) if t == " see"));
    assert!(matches!(
        &ok[2],
        StreamEvent::BlockDone(ContentBlock::Thinking { thinking, signature })
            if thinking == "let me see" && signature == "sig-abc"
    ));
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(ContentBlock::RedactedThinking { data }) if data == "blob"
    ));
    assert!(matches!(&ok[4], StreamEvent::TextDelta(t) if t == "answer"));
    assert!(
        matches!(&ok[5], StreamEvent::BlockDone(ContentBlock::Text { text }) if text == "answer")
    );
    assert!(matches!(&ok[6], StreamEvent::Done { .. }));
    assert_eq!(ok.len(), 7);
}

/// History thinking blocks replay verbatim (signature included, empty text
/// included) and the moving cache breakpoint skips them; the thinking request
/// field follows the configured mode, raising max_tokens by a legacy budget.
#[tokio::test]
async fn thinking_replay_and_request_modes() {
    let server = MockServer::start().await;
    mount_sse(&server, sse_body(&[json!({"type": "message_stop"})])).await;
    let provider = Arc::new(Provider::Anthropic {
        key: "test-key".into(),
        base: server.uri(),
        cache: true,
        thinking: ThinkingMode::Budget(2048),
    });
    // Contrived: a trailing assistant message ending in thinking, to pin the
    // breakpoint-skips-thinking rule.
    let messages = vec![
        Message::user_text("hi"),
        Message::assistant(vec![
            ContentBlock::Thinking {
                thinking: String::new(),
                signature: "sig".into(),
            },
            ContentBlock::Text { text: "so".into() },
            ContentBlock::RedactedThinking { data: "d".into() },
        ]),
    ];
    let mut rx = provider.stream("test-model", "s", &messages, &[]);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "budget_tokens": 2048})
    );
    assert_eq!(body["max_tokens"], json!(8192 + 2048));
    assert_eq!(
        body["messages"][1]["content"],
        json!([
            {"type": "thinking", "thinking": "", "signature": "sig"},
            {
                "type": "text", "text": "so",
                "cache_control": {"type": "ephemeral"},
            },
            {"type": "redacted_thinking", "data": "d"},
        ]),
        "replay is verbatim; the breakpoint lands on the last cacheable block"
    );
}

/// Adaptive and off modes map to their wire shapes; Unset sends no field.
#[tokio::test]
async fn thinking_mode_field_shapes() {
    for (mode, expected) in [
        (ThinkingMode::Unset, None),
        (ThinkingMode::Off, Some(json!({"type": "disabled"}))),
        (ThinkingMode::Adaptive, Some(json!({"type": "adaptive"}))),
    ] {
        let server = MockServer::start().await;
        mount_sse(&server, sse_body(&[json!({"type": "message_stop"})])).await;
        let provider = Arc::new(Provider::Anthropic {
            key: "test-key".into(),
            base: server.uri(),
            cache: true,
            thinking: mode,
        });
        let mut rx = provider.stream("test-model", "s", &[Message::user_text("hi")], &[]);
        while rx.recv().await.is_some() {}
        let requests = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(body.get("thinking").cloned(), expected, "mode {mode:?}");
        assert_eq!(body["max_tokens"], json!(8192), "mode {mode:?}");
    }
}

#[tokio::test]
async fn malformed_tool_input_falls_back_to_empty_object() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "bash"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{not json"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(anthropic(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(
        &ok[0],
        StreamEvent::BlockDone(ContentBlock::ToolUse { input, .. }) if input == &json!({})
    ));
}

#[tokio::test]
async fn http_overflow_maps_to_overflow_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 210000 tokens > 200000 maximum"}}"#,
        ))
        .mount(&server)
        .await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1);
    let err = events.into_iter().next().unwrap().unwrap_err();
    assert!(
        err.downcast_ref::<OverflowError>().is_some(),
        "expected OverflowError, got: {err:#}"
    );
}

#[tokio::test]
async fn stream_error_event_surfaces_as_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "error", "error": {"type": "overloaded_error", "message": "try later"}}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1);
    let err = events.into_iter().next().unwrap().unwrap_err();
    assert!(err.downcast_ref::<OverflowError>().is_none());
    assert!(format!("{err:#}").contains("overloaded_error"));
}

/// A stream that dies without message_stop closes the channel with no Done —
/// the agent treats that as retryable.
#[tokio::test]
async fn stream_without_message_stop_closes_channel_cleanly() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    // TextDelta only; no BlockDone (block never stopped), no Done, no error.
    assert_eq!(events.len(), 1);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamEvent::TextDelta(t) if t == "partial"
    ));
}
