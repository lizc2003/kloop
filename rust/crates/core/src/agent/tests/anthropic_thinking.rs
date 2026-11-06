use super::*;
use crate::event::ItemStatus;
use kloop_provider::Credential;
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;

async fn fixture(
    events: Vec<serde_json::Value>,
    release: Option<tokio::sync::oneshot::Receiver<()>>,
) -> (Provider, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body: String = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect();
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        let header_end = loop {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
            if let Some(index) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") {
                break index + 4;
            }
        };
        let length = std::str::from_utf8(&request[..header_end])
            .unwrap()
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .unwrap()
            .1
            .trim()
            .parse::<usize>()
            .unwrap();
        while request.len() < header_end + length {
            let count = socket.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0);
            request.extend_from_slice(&buffer[..count]);
        }
        // 取消回归留一个未交付字节，使流保持打开，避免 EOF 错误与取消同时就绪。
        let length = body.len() + usize::from(release.is_some());
        socket
            .write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n{body}").as_bytes())
            .await
            .unwrap();
        if let Some(release) = release {
            let _ = release.await;
        }
    });
    (
        Provider::Anthropic {
            cred: Credential::api_key("test-key"),
            base: format!("http://{address}"),
            prompt_cache: true,
            thinking_signature: kloop_provider::ThinkingSignaturePolicy::Optional,
        },
        task,
    )
}

fn thinking(index: u64, text: &str, signature: Option<&str>) -> Vec<serde_json::Value> {
    let mut events = vec![
        json!({"type": "content_block_start", "index": index, "content_block": {"type": "thinking", "thinking": ""}}),
        json!({"type": "content_block_delta", "index": index, "delta": {"type": "thinking_delta", "thinking": text}}),
    ];
    if let Some(signature) = signature {
        events.push(json!({"type": "content_block_delta", "index": index, "delta": {"type": "signature_delta", "signature": signature}}));
    }
    events.push(json!({"type": "content_block_stop", "index": index}));
    events
}

#[derive(Default)]
struct EventUi(std::sync::Mutex<Vec<Event>>);

impl Ui for EventUi {
    fn emit(&self, event: &Event) {
        self.0.lock().unwrap().push(event.clone());
    }
}

#[tokio::test]
async fn optional_unsigned_thinking_preserves_display_and_history() {
    for depth in [0, 1] {
        for signed in [false, true] {
            let mut wire =
                vec![json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}})];
            wire.extend(thinking(0, "first thought", None));
            if signed {
                wire.extend(thinking(1, "keep", Some("sig")));
            }
            wire.extend(thinking(2, "last thought", None));
            wire.extend([
                json!({"type": "content_block_start", "index": 3, "content_block": {"type": "text", "text": ""}}),
                json!({"type": "content_block_delta", "index": 3, "delta": {"type": "text_delta", "text": "answer"}}),
                json!({"type": "content_block_stop", "index": 3}),
                json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 8}}),
                json!({"type": "message_stop"}),
            ]);
            let (provider, server) = fixture(wire, None).await;
            let cfg = compaction_cfg(provider, 200_000, "optional-thinking");
            let event_ui = Arc::new(EventUi::default());
            let ui: Arc<dyn Ui> = event_ui.clone();
            let mut history = History::new(cfg.offload_dir.clone());
            history.record(Message::user_text("think"));

            let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), depth).await;

            server.await.unwrap();
            assert_eq!(outcome.reason, EndReason::Completed);
            assert_eq!(outcome.final_text, "answer");
            let mut content = vec![ContentBlock::Thinking {
                thinking: "first thought".into(),
                signature: String::new(),
            }];
            if signed {
                content.push(ContentBlock::Thinking {
                    thinking: "keep".into(),
                    signature: "sig".into(),
                });
            }
            content.push(ContentBlock::Thinking {
                thinking: "last thought".into(),
                signature: String::new(),
            });
            content.push(ContentBlock::Text {
                text: "answer".into(),
            });
            assert_eq!(
                history.messages(),
                &[
                    Message::user_text("think"),
                    Message::assistant_from_provider(
                        content,
                        cfg.provider_route.primary_attempt().provenance(2),
                    ),
                ]
            );
            let events: Vec<_> = event_ui
                .0
                .lock()
                .unwrap()
                .iter()
                .filter(|event| !matches!(event, Event::Usage(_)))
                .cloned()
                .collect();
            let mut expected = Vec::new();
            if depth == 0 {
                let mut displayed = vec!["first thought"];
                if signed {
                    displayed.push("keep");
                }
                displayed.push("last thought");
                for (index, text) in displayed.iter().enumerate() {
                    let id = format!("reasoning-{index}");
                    expected.extend([
                        Event::ItemStarted {
                            id: id.clone(),
                            item: Item::Reasoning {
                                text: String::new(),
                                status: ItemStatus::InProgress,
                            },
                        },
                        Event::ItemDelta {
                            id: id.clone(),
                            delta: Delta::Reasoning((*text).into()),
                        },
                        Event::ItemCompleted {
                            id,
                            item: Item::Reasoning {
                                text: (*text).into(),
                                status: ItemStatus::Completed,
                            },
                        },
                    ]);
                }
                let id = format!("msg-{}", displayed.len());
                expected.extend([
                    Event::ItemStarted {
                        id: id.clone(),
                        item: Item::AssistantMessage {
                            text: String::new(),
                            status: ItemStatus::InProgress,
                        },
                    },
                    Event::ItemDelta {
                        id: id.clone(),
                        delta: Delta::Text("answer".into()),
                    },
                    Event::ItemCompleted {
                        id,
                        item: Item::AssistantMessage {
                            text: "answer".into(),
                            status: ItemStatus::Completed,
                        },
                    },
                ]);
            }
            assert_eq!(events, expected);
        }
    }
}

#[tokio::test]
async fn cancellation_preserves_completed_optional_unsigned_thinking() {
    struct CancelOnCompletion(CancellationToken);
    impl Ui for CancelOnCompletion {
        fn emit(&self, event: &Event) {
            if matches!(
                event,
                Event::ItemCompleted {
                    item: Item::Reasoning { .. },
                    ..
                }
            ) {
                self.0.cancel();
            }
        }
    }
    let mut wire =
        vec![json!({"type": "message_start", "message": {"usage": {"input_tokens": 10}}})];
    wire.extend(thinking(0, "thought", None));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (provider, server) = fixture(wire, Some(release_rx)).await;
    let cfg = compaction_cfg(provider, 200_000, "cancel-optional-thinking");
    let cancel = CancellationToken::new();
    let ui: Arc<dyn Ui> = Arc::new(CancelOnCompletion(cancel.clone()));
    let mut history = History::new(cfg.offload_dir.clone());

    let (outcome, returned) = run_turn_with_input(
        &cfg,
        &mut history,
        &ui,
        &cancel,
        0,
        Message::user_text("think"),
    )
    .await;

    let _ = release_tx.send(());
    server.await.unwrap();
    assert_eq!(outcome.reason, EndReason::Aborted);
    assert_eq!(returned, None);
    assert_eq!(
        history.messages(),
        &[
            Message::user_text("think"),
            Message::assistant_from_provider(
                vec![ContentBlock::Thinking {
                    thinking: "thought".into(),
                    signature: String::new()
                }],
                cfg.provider_route.primary_attempt().provenance(2)
            ),
        ]
    );
}
