//! HTTP contract tests for the OpenAI Responses API adapter: the Responses
//! SSE event family in, StreamEvents out, plus the stateless request shape.

use std::sync::Arc;

use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OutputLimitKind;
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
        effort: None,
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
            "max_output_tokens": 8192,
            "parallel_tool_calls": true,
            "store": false,
            "include": ["reasoning.encrypted_content"],
            "stream": true,
        })
    );
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

/// KLOOP_EFFORT maps to the reasoning request field (with summary=auto for
/// displayable text); absent effort sends no reasoning field at all.
#[tokio::test]
async fn effort_maps_to_reasoning_field() {
    for (effort, expected) in [
        (None, None),
        (
            Some("high".to_string()),
            Some(json!({"effort": "high", "summary": "auto"})),
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
            effort: effort.clone(),
        });
        let mut rx = provider.stream("test-model", "s", &[Message::user_text("hi")], &[]);
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

#[tokio::test]
async fn malformed_function_arguments_fail_closed() {
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

    let events = collect(responses(&server)).await;
    assert_eq!(
        events.len(),
        1,
        "no ToolUse or Done may follow invalid JSON"
    );
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(error.to_string().contains("invalid JSON input"));
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

/// The out-of-band window is narrow: any non-`codex.` unknown event is still
/// a fatal protocol error (fail-closed boundary preserved).
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
