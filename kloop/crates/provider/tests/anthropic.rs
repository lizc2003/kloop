//! HTTP contract tests for the Anthropic SSE adapter: scripted wire events
//! in, StreamEvent sequences out. What the live API validated once, these
//! keep validated forever.

use std::sync::Arc;

use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::IncompleteReason;
use kloop_protocol::Message;
use kloop_protocol::OutputLimitKind;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;
use kloop_provider::Provider;
use kloop_provider::ProviderFailureKind;
use kloop_provider::StreamResult;
use kloop_provider::ThinkingMode;
use serde_json::json;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

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
        .and(path("/v1/messages"))
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

async fn collect(provider: Provider) -> Vec<StreamResult> {
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
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hel"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}}),
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
        matches!(&ok[2], StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "hello")
    );
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(AssistantBlock::ToolUse { id, name, input })
            if id == "t1" && name == "bash" && input == &json!({"command": "ls"})
    ));
    assert!(matches!(
        &ok[4],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::ToolUse,
            usage: Some(u),
        } if *u == Usage { input_tokens: 120, output_tokens: 30, ..Default::default() }
    ));
    assert_eq!(ok.len(), 5);
}

/// The caching request contract, asserted whole-object: one breakpoint on
/// the LAST tool (the tool set outlives the volatile system prompt), one on
/// the system block, and exactly one message breakpoint on the LAST content
/// block of the LAST message.
#[tokio::test]
async fn request_body_carries_cache_breakpoints() {
    let server = MockServer::start().await;
    mount_sse(&server, sse_body(&[json!({"type": "message_stop"})])).await;
    let provider = Arc::new(anthropic(&server));
    let tools = [
        kloop_protocol::ToolDef {
            name: "bash".into(),
            description: "run a command".into(),
            schema: json!({"type": "object"}),
        },
        kloop_protocol::ToolDef {
            name: "read_file".into(),
            description: "read a file".into(),
            schema: json!({"type": "object"}),
        },
    ];
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
            provider_provenance: None,
        },
    ];
    let mut rx = provider.stream("test-model", "be brief", &messages, &tools);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method.as_str(), "POST");
    assert_eq!(requests[0].url.path(), "/v1/messages");
    assert_eq!(requests[0].headers["x-api-key"], "test-key");
    assert_eq!(requests[0].headers["anthropic-version"], "2023-06-01");
    assert!(
        requests[0].headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
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
            "tools": [
                {"name": "bash", "description": "run a command",
                 "input_schema": {"type": "object"}},
                {"name": "read_file", "description": "read a file",
                 "input_schema": {"type": "object"},
                 "cache_control": {"type": "ephemeral"}},
            ],
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
        StreamEvent::Terminal { usage: Some(u), .. }
            if *u == Usage {
                input_tokens: 10,
                output_tokens: 5,
                cache_read_input_tokens: 900,
                cache_creation_input_tokens: 50,
            }
    ));
}

#[tokio::test]
async fn stop_reasons_map_to_typed_outcomes() {
    let cases = [
        ("end_turn", AssistantOutcome::EndTurn),
        ("stop_sequence", AssistantOutcome::EndTurn),
        (
            "max_tokens",
            AssistantOutcome::OutputLimit(OutputLimitKind::MaxOutputTokens),
        ),
        (
            "model_context_window_exceeded",
            AssistantOutcome::OutputLimit(OutputLimitKind::ModelContextWindow),
        ),
        ("refusal", AssistantOutcome::Refused),
        (
            "pause_turn",
            AssistantOutcome::Incomplete(IncompleteReason::PauseTurn),
        ),
    ];
    let server = MockServer::start().await;
    for (reason, expected) in cases {
        mount_sse(
            &server,
            sse_body(&[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 2}}}),
                json!({"type": "message_delta", "delta": {"stop_reason": reason}, "usage": {"output_tokens": 1}}),
                json!({"type": "message_stop"}),
            ]),
        )
        .await;
        let events = collect(anthropic(&server)).await;
        assert_eq!(events.len(), 1, "reason {reason}");
        assert!(matches!(
            events[0].as_ref().unwrap(),
            StreamEvent::Terminal { outcome, .. } if outcome == &expected
        ));
        server.reset().await;
    }
}

#[tokio::test]
async fn required_order_identity_and_stop_reason_fail_closed() {
    let cases = vec![
        vec![json!({"type": "content_block_start", "index": 0,
            "content_block": {"type": "text", "text": ""}})],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
        ],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "content_block": {"type": "text", "text": ""}}),
        ],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "t", "name": "", "input": {}}}),
        ],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0,
                "content_block": {"type": "tool_use", "id": "t", "name": "bash", "input": []}}),
        ],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_delta", "index": 7,
                "delta": {"type": "text_delta", "text": "orphan"}}),
        ],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "message_stop"}),
        ],
        vec![
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "future_reason"}, "usage": {"output_tokens": 1}}),
        ],
    ];
    let server = MockServer::start().await;
    for (index, events) in cases.into_iter().enumerate() {
        mount_sse(&server, sse_body(&events)).await;
        let result = collect(anthropic(&server)).await;
        assert_eq!(result.len(), 1, "case {index}");
        let error = result.into_iter().next().unwrap().unwrap_err();
        assert_eq!(
            error.kind(),
            &ProviderFailureKind::Protocol,
            "case {index}: {error}"
        );
        assert!(!error.is_retryable(), "case {index}");
        server.reset().await;
    }
}

#[tokio::test]
async fn unsupported_content_blocks_fail_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "server_tool_use", "id": "s1"}}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(error.to_string().contains("unsupported content block"));
}

#[tokio::test]
async fn concurrent_same_kind_display_blocks_fail_closed() {
    let server = MockServer::start().await;
    for block in [
        json!({"type": "text", "text": ""}),
        json!({"type": "thinking", "thinking": "", "signature": ""}),
    ] {
        mount_sse(
            &server,
            sse_body(&[
                json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
                json!({"type": "content_block_start", "index": 0, "content_block": block.clone()}),
                json!({"type": "content_block_start", "index": 1, "content_block": block}),
            ]),
        )
        .await;
        let events = collect(anthropic(&server)).await;
        assert_eq!(events.len(), 1);
        let error = events.into_iter().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
        assert!(!error.after_semantic_output());
        server.reset().await;
    }
}

#[tokio::test]
async fn terminal_is_low_latency_but_later_complete_semantic_frames_fail_closed() {
    let terminal = [
        json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"},
            "usage": {"output_tokens": 1}}),
        json!({"type": "message_stop"}),
    ];
    let server = MockServer::start().await;

    let mut partial_tail = sse_body(&terminal);
    partial_tail.push_str("event: ping\ndata: {");
    mount_sse(&server, partial_tail).await;
    let events = collect(anthropic(&server)).await;
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
        terminal[2].clone(),
        terminal[2].clone(),
    ]);
    mount_sse(&server, duplicate).await;
    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());

    server.reset().await;
    mount_bytes(&server, b"event: message_start\ndata: \xff\n\n".to_vec()).await;
    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
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
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": "let me"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "thinking_delta", "thinking": " see"}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "signature_delta", "signature": "sig-abc"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "content_block_start", "index": 1, "content_block": {"type": "redacted_thinking", "data": "blob"}}),
            json!({"type": "content_block_stop", "index": 1}),
            json!({"type": "content_block_start", "index": 2, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 2, "delta": {"type": "text_delta", "text": "answer"}}),
            json!({"type": "content_block_stop", "index": 2}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 8}}),
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
        StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, signature })
            if thinking == "let me see" && signature == "sig-abc"
    ));
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(AssistantBlock::RedactedThinking { data }) if data == "blob"
    ));
    assert!(matches!(&ok[4], StreamEvent::TextDelta(t) if t == "answer"));
    assert!(
        matches!(&ok[5], StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "answer")
    );
    assert!(matches!(&ok[6], StreamEvent::Terminal { .. }));
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
        Message::assistant_from_provider(
            vec![
                ContentBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                ContentBlock::Text { text: "so".into() },
                ContentBlock::RedactedThinking { data: "d".into() },
            ],
            provider.response_provenance("test-model"),
        ),
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
    let server = MockServer::start().await;
    for (mode, expected) in [
        (ThinkingMode::Unset, None),
        (ThinkingMode::Off, Some(json!({"type": "disabled"}))),
        (ThinkingMode::Adaptive, Some(json!({"type": "adaptive"}))),
    ] {
        server.reset().await;
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
async fn malformed_tool_input_fails_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{not json"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(
        events.len(),
        1,
        "no ToolUse or Done may follow invalid JSON"
    );
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.to_string().contains("invalid JSON input"));
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
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::ContextOverflow);
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
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.to_string().contains("overloaded_error"));
}

/// A stream that dies without message_stop emits one typed incomplete error;
/// an unfinished content block is never fabricated.
#[tokio::test]
async fn stream_without_message_stop_is_an_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "partial"}}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamEvent::TextDelta(t) if t == "partial"
    ));
    let error = events[1].as_ref().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.is_retryable());
    assert!(error.to_string().contains("before message_stop"));
}

#[tokio::test]
async fn malformed_sse_json_is_a_terminal_protocol_error() {
    let server = MockServer::start().await;
    mount_sse(&server, "event: message_start\ndata: {oops\n\n".into()).await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn message_stop_does_not_close_an_unfinished_block() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "t1", "name": "bash", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"command\":"}}),
            json!({"type": "message_stop"}),
        ]),
    )
    .await;

    let events = collect(anthropic(&server)).await;
    assert_eq!(events.len(), 1, "partial tool JSON must stay invisible");
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(error.to_string().contains("unfinished content blocks"));
}
