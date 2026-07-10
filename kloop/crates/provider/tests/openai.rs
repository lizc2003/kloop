//! HTTP contract tests for the OpenAI-compat adapter: chat/completions
//! delta streams in, StreamEvent sequences out.

use std::sync::Arc;

use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;
use kloop_provider::Provider;
use serde_json::json;
use serde_json::Value;
use wiremock::matchers::method;
use wiremock::matchers::path;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

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
                json!({"choices": [{"delta": {"content": "thin"}}]}),
                json!({"choices": [{"delta": {"content": "king"}}]}),
                json!({"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{\"comm"}}
                ]}}]}),
                json!({"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "function": {"arguments": "and\":\"ls\"}"}},
                    {"index": 1, "id": "c2", "function": {"name": "read_file", "arguments": "{\"path\":\"x\"}"}}
                ]}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
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
        StreamEvent::BlockDone(ContentBlock::Text { text }) if text == "thinking"
    ));
    assert!(matches!(
        &ok[3],
        StreamEvent::BlockDone(ContentBlock::ToolUse { id, name, input })
            if id == "c1" && name == "bash" && input == &json!({"command": "ls"})
    ));
    assert!(matches!(
        &ok[4],
        StreamEvent::BlockDone(ContentBlock::ToolUse { id, name, input })
            if id == "c2" && name == "read_file" && input == &json!({"path": "x"})
    ));
    assert!(matches!(
        &ok[5],
        StreamEvent::Done { stop_reason: Some(r), usage: Some(u) }
            if r == "tool_calls" && *u == Usage { input_tokens: 88, output_tokens: 17, ..Default::default() }
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
                json!({"choices": [{"delta": {"reasoning_content": "hmm, "}}]}),
                json!({"choices": [{"delta": {"reasoning": "two"}}]}),
                json!({"choices": [{"delta": {"content": "4"}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "stop"}]}),
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
        StreamEvent::BlockDone(ContentBlock::Thinking { thinking, signature })
            if thinking == "hmm, two" && signature.is_empty()
    ));
    assert!(matches!(&ok[4], StreamEvent::BlockDone(ContentBlock::Text { text }) if text == "4"));
    assert!(matches!(&ok[5], StreamEvent::Done { .. }));
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
                json!({"choices": [{"delta": {"content": "hi"}, "finish_reason": "stop"}]}),
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
    let StreamEvent::Done {
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

/// Unparseable tool arguments degrade to a raw string, never a panic or a
/// silent empty object (the model may want to see its own malformed output).
#[tokio::test]
async fn malformed_arguments_degrade_to_raw_string() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{oops"}}
                ]}}]}),
                json!({"choices": [{"delta": {}, "finish_reason": "tool_calls"}]}),
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
    assert!(matches!(
        &ok[0],
        StreamEvent::BlockDone(ContentBlock::ToolUse { input, .. })
            if input == &Value::String("{oops".into())
    ));
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
    let err = events.into_iter().next().unwrap().unwrap_err();
    assert!(
        err.downcast_ref::<OverflowError>().is_some(),
        "expected OverflowError, got: {err:#}"
    );
}

/// A stream that dies before finish_reason/[DONE] yields an error event, not
/// a fabricated Done — the agent retries on it.
#[tokio::test]
async fn stream_dying_mid_flight_is_an_error() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[json!({"choices": [{"delta": {"content": "par"}}]})],
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
    let err = events.into_iter().nth(1).unwrap().unwrap_err();
    assert!(format!("{err:#}").contains("ended before finish"));
}
