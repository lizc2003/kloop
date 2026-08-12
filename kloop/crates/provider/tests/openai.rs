//! HTTP contract tests for the OpenAI-compat adapter: chat/completions
//! delta streams in, StreamEvent sequences out.

use std::sync::Arc;

use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::Message;
use kloop_protocol::StreamEvent;
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

fn sse_body(chunks: &[Value], done: bool) -> String {
    let mut body: String = chunks.iter().map(|c| format!("data: {c}\n\n")).collect();
    if done {
        body.push_str("data: [DONE]\n\n");
    }
    body
}

async fn mount_sse(server: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
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
        .and(path("/chat/completions"))
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

fn openai(server: &MockServer) -> Provider {
    Provider::OpenAiCompat {
        key: "test-key".into(),
        base: server.uri(),
    }
}

/// Text deltas, tool_calls accumulated across chunks by index, usage arriving
/// AFTER finish_reason (the include_usage contract), then [DONE].
#[tokio::test]
async fn accumulates_tool_calls_and_usage_across_chunks() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"content": "thin"}}]}),
                json!({"choices": [{"index": 0, "delta": {"content": "king"}}]}),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "", "arguments": "{\"comm"}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "function": {"name": "bash", "arguments": "and\":\"ls\"}"}},
                    {"index": 1, "id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"x\"}"}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "", "function": {"name": "", "arguments": ""}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 88, "completion_tokens": 17}}),
            ],
            true,
        ),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(openai(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();

    assert!(matches!(&ok[0], StreamEvent::TextDelta(t) if t == "thin"));
    assert!(matches!(&ok[1], StreamEvent::TextDelta(t) if t == "king"));
    assert!(matches!(
        &ok[2],
        StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "thinking"
    ));
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(AssistantBlock::ToolUse { id, name, input })
            if id == "c1" && name == "bash" && input == &json!({"command": "ls"})
    ));
    assert!(matches!(
        &ok[4],
        StreamEvent::BlockDone(AssistantBlock::ToolUse { id, name, input })
            if id == "c2" && name == "read_file" && input == &json!({"path": "x"})
    ));
    assert!(matches!(
        &ok[5],
        StreamEvent::Terminal {
            outcome: AssistantOutcome::ToolUse,
            usage: Some(u),
        } if *u == Usage { input_tokens: 88, output_tokens: 17, ..Default::default() }
    ));
    assert_eq!(ok.len(), 6);
}

/// reasoning_content deltas (deepseek-style; plain `reasoning` also accepted)
/// stream as ThinkingDelta and finalize into a signature-less Thinking block
/// ahead of the text block.
#[tokio::test]
async fn reasoning_content_becomes_thinking_block() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"reasoning_content": "hmm, "}}]}),
                json!({"choices": [{"index": 0, "delta": {"reasoning": "two"}}]}),
                json!({"choices": [{"index": 0, "delta": {"content": "4"}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ],
            true,
        ),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(openai(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    assert!(matches!(&ok[0], StreamEvent::ThinkingDelta(t) if t == "hmm, "));
    assert!(matches!(&ok[1], StreamEvent::ThinkingDelta(t) if t == "two"));
    assert!(matches!(&ok[2], StreamEvent::TextDelta(t) if t == "4"));
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(AssistantBlock::Thinking { thinking, signature })
            if thinking == "hmm, two" && signature.is_empty()
    ));
    assert!(matches!(&ok[4], StreamEvent::BlockDone(AssistantBlock::Text { text }) if text == "4"));
    assert!(matches!(&ok[5], StreamEvent::Terminal { .. }));
    assert_eq!(ok.len(), 6);
}

/// Cached prompt tokens ride INSIDE prompt_tokens on the OpenAI wire; the
/// adapter subtracts them out so input_tokens is the uncached remainder on
/// both rails and total() never double-counts.
#[tokio::test]
async fn cached_prompt_tokens_are_split_out_of_input() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"content": "hi"}, "finish_reason": "stop"}]}),
                json!({"choices": [], "usage": {
                    "prompt_tokens": 1000,
                    "completion_tokens": 20,
                    "prompt_tokens_details": {"cached_tokens": 900},
                }}),
            ],
            true,
        ),
    )
    .await;

    let ok: Vec<StreamEvent> = collect(openai(&server))
        .await
        .into_iter()
        .map(|e| e.unwrap())
        .collect();
    let StreamEvent::Terminal {
        usage: Some(usage), ..
    } = ok.last().unwrap()
    else {
        panic!("expected Done with usage, got {:?}", ok.last());
    };
    assert_eq!(
        *usage,
        Usage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_input_tokens: 900,
            cache_creation_input_tokens: 0,
        }
    );
}

/// Non-empty malformed tool arguments fail closed before a ToolUse or Done is emitted.
#[tokio::test]
async fn empty_tool_identity_must_eventually_fill_and_nonempty_values_cannot_change() {
    let cases = vec![
        vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "", "function": {"name": "", "arguments": "{}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
        ],
        vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c2"}
            ]}}]}),
        ],
        vec![
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{}"}}
            ]}}]}),
            json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                {"index": 0, "function": {"name": "sh"}}
            ]}}]}),
        ],
    ];
    let server = MockServer::start().await;
    for (index, wire) in cases.into_iter().enumerate() {
        mount_sse(&server, sse_body(&wire, true)).await;
        let events = collect(openai(&server)).await;
        assert_eq!(events.len(), 1, "case {index}");
        let error = events.into_iter().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol, "case {index}");
        assert!(!error.after_semantic_output(), "case {index}");
        server.reset().await;
    }
}

#[tokio::test]
async fn malformed_arguments_fail_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{oops"}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ],
            true,
        ),
    )
    .await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.to_string().contains("invalid JSON input"));
}

#[tokio::test]
async fn http_overflow_maps_to_overflow_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string(
            r#"{"error":{"message":"This model's maximum context length is 128000 tokens","code":"context_length_exceeded"}}"#,
        ))
        .mount(&server)
        .await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::ContextOverflow);
}

/// A stream that dies before finish_reason/[DONE] yields an error event, not
/// a fabricated Done — the agent retries on it.
#[tokio::test]
async fn stream_dying_mid_flight_is_an_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[json!({"choices": [{"index": 0, "delta": {"content": "par"}}]})],
            false,
        ),
    )
    .await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 2);
    assert!(matches!(
        events[0].as_ref().unwrap(),
        StreamEvent::TextDelta(t) if t == "par"
    ));
    let error = events.into_iter().nth(1).unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.is_retryable());
    assert!(error.to_string().contains("ended before finish_reason"));
}

#[tokio::test]
async fn finish_reason_succeeds_without_done_sentinel() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"content": "ok"}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ],
            false,
        ),
    )
    .await;

    let events = collect(openai(&server)).await;
    assert!(matches!(
        events.last().unwrap().as_ref().unwrap(),
        StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        }
    ));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Ok(StreamEvent::Terminal { .. }) | Err(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn done_without_finish_reason_is_not_completion() {
    let server = MockServer::start().await;
    mount_sse(&server, sse_body(&[], true)).await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.is_retryable());
}

#[tokio::test]
async fn semantic_frame_after_done_fails_closed() {
    let server = MockServer::start().await;
    let finish = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
    mount_sse(&server, format!("data: [DONE]\n\ndata: {finish}\n\n")).await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(!error.after_semantic_output());
}

#[tokio::test]
async fn finish_reason_is_low_latency_but_later_complete_semantic_frames_fail_closed() {
    let finish = json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]});
    let server = MockServer::start().await;

    let mut partial_tail = sse_body(std::slice::from_ref(&finish), false);
    partial_tail.push_str("data: [DO");
    mount_sse(&server, partial_tail).await;
    let events = collect(openai(&server)).await;
    assert!(matches!(
        events.as_slice(),
        [Ok(StreamEvent::Terminal {
            outcome: AssistantOutcome::EndTurn,
            ..
        })]
    ));

    server.reset().await;
    mount_sse(&server, sse_body(&[finish.clone(), finish], false)).await;
    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());

    server.reset().await;
    mount_bytes(&server, b"data: \xff\n\n".to_vec()).await;
    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn malformed_sse_json_is_a_terminal_protocol_error() {
    let server = MockServer::start().await;
    mount_sse(&server, "data: {oops\n\n".into()).await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
}

#[tokio::test]
async fn http_status_and_retry_after_remain_typed() {
    let limited = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("Retry-After", "120")
                .set_body_string("slow down"),
        )
        .mount(&limited)
        .await;
    let events = collect(openai(&limited)).await;
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Http { status: 429 });
    assert!(error.is_retryable());
    assert_eq!(
        error.retry_after(),
        Some(std::time::Duration::from_secs(60))
    );

    let bad_request = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(400).set_body_string("bad request"))
        .mount(&bad_request)
        .await;
    let events = collect(openai(&bad_request)).await;
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Http { status: 400 });
    assert!(!error.is_retryable());
    assert_eq!(error.retry_after(), None);
}
