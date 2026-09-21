//! HTTP contract tests for the OpenAI-compat adapter: chat/completions
//! delta streams in, StreamEvent sequences out.

use std::sync::Arc;

use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::Message;
use kloop_protocol::ReasoningEffort;
use kloop_protocol::StreamEvent;
use kloop_protocol::Usage;
use kloop_provider::Credential;
use kloop_provider::Provider;
use kloop_provider::ProviderFailureKind;
use kloop_provider::Reasoning;
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
        cred: Credential::bearer("test-key"),
        base: server.uri(),
    }
}

/// The chat rail asks for the same output cap as Responses, and for the same
/// reason: reasoning tokens come out of this budget too, and there is no
/// "retry with a bigger cap" step behind either rail. The Anthropic rail keeps
/// its smaller cap because a thinking budget is added on top of it there.
#[tokio::test]
async fn chat_requests_carry_the_openai_output_cap() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
                json!({"choices": [], "usage": {"prompt_tokens": 5, "completion_tokens": 1}}),
            ],
            true,
        ),
    )
    .await;

    let provider = Arc::new(openai(&server));
    let mut rx = provider.stream("test-model", "be brief", &[Message::user_text("hi")], &[]);
    while rx.recv().await.is_some() {}

    let requests = server.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["max_tokens"], json!(32_768));
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

/// A gateway may attach usage to BOTH the `finish_reason` frame and the
/// trailing empty-choices frame (a GLM route sends two byte-identical copies).
/// The stream survives that and reports the last one, which is the frame the
/// `include_usage` contract calls final.
#[tokio::test]
async fn repeated_usage_keeps_the_last_report() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"content": "hi"}}]}),
                json!({
                    "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                    "usage": {
                        "prompt_tokens": 17,
                        "completion_tokens": 120,
                        "prompt_tokens_details": {"cached_tokens": 5},
                        "cost": 4.617e-5,
                        "total_tokens": 137
                    }
                }),
                json!({
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 17,
                        "completion_tokens": 126,
                        "prompt_tokens_details": {"cached_tokens": 5},
                        "cost": 4.617e-5,
                        "total_tokens": 143
                    }
                }),
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

    assert_eq!(
        ok,
        vec![
            StreamEvent::TextDelta("hi".into()),
            StreamEvent::BlockDone(AssistantBlock::Text { text: "hi".into() }),
            StreamEvent::Terminal {
                outcome: AssistantOutcome::EndTurn,
                usage: Some(Usage {
                    input_tokens: 12,
                    output_tokens: 126,
                    cache_read_input_tokens: 5,
                    cache_creation_input_tokens: 0,
                }),
            },
        ]
    );
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

#[tokio::test]
async fn null_tool_calls_are_ignored_but_non_arrays_fail_closed() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"content": "ok", "tool_calls": null}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
            ],
            true,
        ),
    )
    .await;
    let events = collect(openai(&server)).await;
    assert!(matches!(
        events.as_slice(),
        [
            Ok(StreamEvent::TextDelta(text)),
            Ok(StreamEvent::BlockDone(AssistantBlock::Text { text: block_text })),
            Ok(StreamEvent::Terminal {
                outcome: AssistantOutcome::EndTurn,
                ..
            })
        ] if text == "ok" && block_text == "ok"
    ));

    for invalid in [json!({"not": "an array"}), json!("not an array"), json!(42)] {
        server.reset().await;
        mount_sse(
            &server,
            sse_body(
                &[json!({
                    "choices": [{"index": 0, "delta": {"tool_calls": invalid}}]
                })],
                false,
            ),
        )
        .await;
        let events = collect(openai(&server)).await;
        assert_eq!(events.len(), 1);
        let error = events.into_iter().next().unwrap().unwrap_err();
        assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
        assert!(error.to_string().contains("tool_calls was not an array"));
    }
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

/// Arguments are a string the model wrote; an unreadable one is its mistake to
/// fix, so the stream completes with a call the turn can answer instead of
/// dying on it. Repair handles the shapes it can read unambiguously — this one
/// it cannot.
#[tokio::test]
async fn malformed_arguments_come_back_as_an_invalid_call() {
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

    let events: Vec<StreamEvent> = collect(openai(&server))
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
        ("c1", "bash", "{oops")
    );
    assert!(error.contains("column"), "{error}");
}

/// The mistake models actually make, and the one that cost a whole turn before:
/// a value left unquoted. It is repaired into the call the model meant.
#[tokio::test]
async fn repairable_arguments_still_reach_the_tool() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[
                json!({"choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "id": "c1", "function": {"name": "bash", "arguments": "{\"command\": \"ls\", \"description\": 看一眼}"}}
                ]}}]}),
                json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}),
            ],
            true,
        ),
    )
    .await;

    let events: Vec<StreamEvent> = collect(openai(&server))
        .await
        .into_iter()
        .map(|event| event.unwrap())
        .collect();
    assert!(matches!(
        &events[0],
        StreamEvent::BlockDone(AssistantBlock::ToolUse { id, name, input })
            if id == "c1" && name == "bash"
                && input == &json!({"command": "ls", "description": "看一眼"})
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

/// The real gateway frame: a proxy relays a transient upstream error as a
/// named `event: error` frame. It must surface the true `type` and be retryable,
/// not degrade into the misleading "unknown SSE event name".
#[tokio::test]
async fn named_error_event_surfaces_upstream_error_and_is_retryable() {
    let server = MockServer::start().await;
    let delta = json!({"choices": [{"index": 0, "delta": {"content": "par"}}]});
    let error = json!({"error": {
        "message": "Upstream service temporarily unavailable",
        "type": "upstream_error",
    }});
    mount_sse(
        &server,
        format!("data: {delta}\n\nevent: error\ndata: {error}\n\n"),
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
    assert!(error.to_string().contains("upstream_error"));
    assert!(!error.to_string().contains("unknown SSE event name"));
}

/// The relay's own sentence reaches the reader. Without it every transient
/// upstream failure renders as a bare `stream error (server_error)`, which says
/// the class but not the cause — the note the user is left staring at.
#[tokio::test]
async fn named_error_event_carries_the_relayed_message() {
    let server = MockServer::start().await;
    let error = json!({"error": {
        "message": "Upstream service temporarily unavailable",
        "type": "server_error",
    }});
    mount_sse(&server, format!("event: error\ndata: {error}\n\n")).await;

    let events = collect(openai(&server)).await;
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert!(error.is_retryable());
    assert_eq!(
        error.to_string(),
        "provider stream interrupted: openai-compat stream error (server_error): \
         Upstream service temporarily unavailable"
    );
}

/// A relay that echoes the request's credential into its error message must not
/// have it read back out in a note. Same rule as the HTTP error body — the SSE
/// frame is no more trustworthy than the body is.
#[tokio::test]
async fn a_relayed_message_never_carries_the_key_back() {
    let server = MockServer::start().await;
    let error = json!({"error": {
        // The key this fixture actually sends (`openai()` above).
        "message": "rejected Authorization: Bearer test-key",
        "type": "server_error",
    }});
    mount_sse(&server, format!("event: error\ndata: {error}\n\n")).await;

    let events = collect(openai(&server)).await;
    let error = events.into_iter().next().unwrap().unwrap_err();
    let rendered = error.to_string();
    assert!(!rendered.contains("test-key"), "{rendered}");
    assert!(rendered.contains("[redacted]"), "{rendered}");
}

/// A frame with no usable message still names the class rather than trailing an
/// empty colon.
#[tokio::test]
async fn named_error_event_without_a_message_stays_bare() {
    let server = MockServer::start().await;
    let error = json!({"error": {"message": "   ", "type": "server_error"}});
    mount_sse(&server, format!("event: error\ndata: {error}\n\n")).await;

    let events = collect(openai(&server)).await;
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(
        error.to_string(),
        "provider stream interrupted: openai-compat stream error (server_error)"
    );
}

/// A named error frame whose type is a client-side/permanent class stays fatal.
#[tokio::test]
async fn named_error_event_with_fatal_type_stays_fatal() {
    let server = MockServer::start().await;
    let error = json!({"error": {"message": "bad", "type": "invalid_request_error"}});
    mount_sse(&server, format!("event: error\ndata: {error}\n\n")).await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(error.to_string().contains("invalid_request_error"));
}

/// Overflow still wins first, even when wrapped in a named error frame.
#[tokio::test]
async fn named_error_event_with_overflow_message_maps_to_overflow() {
    let server = MockServer::start().await;
    let error = json!({"error": {
        "message": "This model's maximum context length is 128000 tokens",
        "type": "invalid_request_error",
    }});
    mount_sse(&server, format!("event: error\ndata: {error}\n\n")).await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::ContextOverflow);
}

/// The unnamed `data:{"error":...}` body classifies identically to the named
/// frame — one shared surfacing/retry path.
#[tokio::test]
async fn unnamed_error_body_uses_same_classification() {
    let server = MockServer::start().await;
    mount_sse(
        &server,
        sse_body(
            &[json!({"error": {"type": "upstream_error", "message": "temporarily unavailable"}})],
            false,
        ),
    )
    .await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(error.is_retryable());
    assert!(error.to_string().contains("upstream_error"));
}

/// Any named event other than `error` is still rejected — the narrow gate holds.
#[tokio::test]
async fn unknown_non_error_named_event_still_fails_closed() {
    let server = MockServer::start().await;
    mount_sse(&server, "event: something\ndata: {}\n\n".into()).await;

    let events = collect(openai(&server)).await;
    assert_eq!(events.len(), 1);
    let error = events.into_iter().next().unwrap().unwrap_err();
    assert_eq!(error.kind(), &ProviderFailureKind::Protocol);
    assert!(!error.is_retryable());
    assert!(error.to_string().contains("unknown SSE event name"));
}

/// The chat rail carries the session id in the same body field as Responses.
/// Both halves are measured: the field is accepted (against a control row that
/// sent nothing and also succeeded), and caching works on this rail — an
/// identical 3,076-token prompt reported `cached_tokens: 2816` on its third
/// send. The test asserts only the wire shape; hit rates belong to the
/// endpoint, not to us.
#[tokio::test]
async fn chat_carries_the_session_id_as_prompt_cache_key() {
    for (cache_key, expected) in [
        (Some("sess-7"), Some(json!("sess-7"))),
        (Some(""), None),
        (None, None),
    ] {
        let server = MockServer::start().await;
        mount_sse(&server, sse_body(&[], /*done*/ true)).await;
        let provider = Arc::new(openai(&server));
        let attempt = provider.attempt_identity("chat", 1, "test-model");
        let mut rx = provider.stream_attempt(
            &attempt,
            Reasoning::new(None, kloop_provider::ThinkingMode::Unset),
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

/// The chat rail spells the knob `reasoning_effort` (a bare string, not the
/// nested object the other two rails use). It is absent unless the session set
/// an effort, because a non-reasoning model rejects the field.
#[tokio::test]
async fn effort_maps_to_reasoning_effort_field() {
    for (effort, expected) in [
        (None, None),
        (Some(ReasoningEffort::None), Some(json!("none"))),
    ] {
        let server = MockServer::start().await;
        mount_sse(&server, sse_body(&[], /*done*/ true)).await;
        let provider = Arc::new(openai(&server));
        let attempt = provider.attempt_identity("chat", 1, "test-model");
        let mut rx = provider.stream_attempt(
            &attempt,
            Reasoning::new(effort, kloop_provider::ThinkingMode::Unset),
            None,
            "s",
            &[Message::user_text("hi")],
            &[],
        );
        while rx.recv().await.is_some() {}
        let requests = server.received_requests().await.unwrap();
        let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
        assert_eq!(
            body.get("reasoning_effort").cloned(),
            expected,
            "{effort:?}"
        );
    }
}
