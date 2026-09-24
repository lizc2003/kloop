use super::*;
use crate::event::Delta;
use crate::event::Item;
use crate::inbox::InboxItem;
use crate::inbox::STEERING_PREFIX;
use crate::tools::SourceOutput;
use crate::tools::ToolSource;
use kloop_protocol::AssistantBlock;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::IncompleteReason;
use kloop_protocol::ProviderApiFamily;
use kloop_protocol::ProviderResponseProvenance;
use kloop_protocol::Role;
use kloop_protocol::ToolDef;
use kloop_protocol::ToolResultContent;
use kloop_protocol::Usage;
use kloop_provider::MockTurn;

/// Local alias so the fixtures below read as sizes, not as a module path.
fn kloop_core_offload_cap() -> usize {
    crate::history::OFFLOAD_CAP_CHARS
}

fn kloop_core_round_cap() -> usize {
    crate::history::ROUND_OFFLOAD_CAP_CHARS
}
use kloop_provider::Provider;
use kloop_provider::ProviderFailure;
use serde_json::json;

struct NullUi;
impl Ui for NullUi {
    fn emit(&self, _: &Event) {}
}

fn usage(input_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        output_tokens: input_tokens + 1,
        cache_read_input_tokens: input_tokens + 2,
        cache_creation_input_tokens: input_tokens + 3,
    }
}

fn mock_assistant(content: Vec<ContentBlock>, model: &str) -> Message {
    Message::assistant_from_provider(
        content,
        ProviderResponseProvenance {
            route_revision: 1,
            route_boundary: 2,
            provider_id: "test".into(),
            api_family: ProviderApiFamily::Mock,
            endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
            model: model.into(),
        },
    )
}

#[test]
fn run_agent_schema_only_advertises_configured_agent_types() {
    let mut tools =
        crate::tools::tool_defs(0, &crate::shell_programs::ShellPrograms::native_posix());
    specialize_run_agent_def(&mut tools, &[]);
    let run_agent = tools.iter().find(|tool| tool.name == "run_agent").unwrap();
    assert!(run_agent.schema["properties"].get("agent_type").is_none());

    let mut tools =
        crate::tools::tool_defs(0, &crate::shell_programs::ShellPrograms::native_posix());
    let agent_types = vec![
        crate::agent_type::AgentType {
            name: "researcher".into(),
            description: "Searches broadly".into(),
            system: None,
            model: None,
            tools: None,
        },
        crate::agent_type::AgentType {
            name: "reviewer".into(),
            description: "Reviews changes".into(),
            system: None,
            model: None,
            tools: None,
        },
    ];
    specialize_run_agent_def(&mut tools, &agent_types);
    let run_agent = tools.iter().find(|tool| tool.name == "run_agent").unwrap();
    assert_eq!(
        run_agent.schema["properties"]["agent_type"]["enum"],
        json!([null, "researcher", "reviewer"])
    );
    assert!(run_agent.description.contains("researcher"));
    assert!(run_agent.description.contains("reviewer"));
}

/// Plan 190: where the reminder lands is the whole design. It is appended at
/// the end of history, after the cache prefix — never into `injected_context`,
/// which is the synthetic first user message at the head of it — and a
/// sub-agent, which has no `todo_write` and no list, is never reminded.
#[tokio::test]
async fn a_stale_todo_list_is_reminded_at_the_end_of_history_and_only_at_depth_zero() {
    let cfg = crate::tools::testutil::TestConfig::new("agent-todo-reminder").build();
    let ctx = crate::tools::testutil::test_ctx_with_cfg(0, Arc::clone(&cfg));
    let (output, is_error) = crate::tools::testutil::run_tool(
        "todo_write",
        json!({"todos": [{"subject": "Ship the reminder", "status": "in_progress"}]}),
        &ctx,
    )
    .await;
    assert!(!is_error, "{output}");

    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("ship it"));

    // A sub-agent's boundaries neither remind nor advance the clock — if they
    // advanced it, the depth-0 stretch below would fire early and fail.
    let mut child_history = History::new(cfg.offload_dir.clone());
    for _ in 0..4 * crate::tools::REMINDER_STALE_ROUNDS {
        assert!(!remind_todos(&cfg, &mut child_history, 1));
    }
    assert!(child_history.messages().is_empty());

    for round in 0..crate::tools::REMINDER_STALE_ROUNDS {
        assert!(!remind_todos(&cfg, &mut history, 0), "round {round}");
    }
    assert_eq!(history.messages().len(), 1);
    assert!(remind_todos(&cfg, &mut history, 0));

    {
        let messages = history.messages();
        assert_eq!(messages.len(), 2);
        let last = messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        let ContentBlock::Text { text } = &last.content[0] else {
            panic!("expected text, got {:?}", last.content[0]);
        };
        assert!(text.starts_with("<system-reminder>"), "{text}");
        assert!(text.contains("- [in_progress] Ship the reminder"), "{text}");
    }

    // Said once: the next boundary appends nothing.
    assert!(!remind_todos(&cfg, &mut history, 0));
    assert_eq!(history.messages().len(), 2);
}

/// The same reminder seen from the loop rather than from its own function: a
/// real `run_turn` over a mock provider that writes a list and then works
/// without touching it. The reminder has to arrive as a user message of its
/// own, at a round boundary — after the round's `tool_result` message, before
/// the next assistant message — and never inside the tool_result block list.
#[tokio::test]
async fn the_reminder_arrives_between_rounds_of_a_real_turn() {
    let rounds = crate::tools::REMINDER_STALE_ROUNDS as usize + 2;
    let mut script = vec![vec![tool_use_named(
        "t0",
        "todo_write",
        json!({"todos": [
            {"subject": "Read the reference projects", "status": "in_progress"},
            {"subject": "Wire the round-boundary reminder", "status": "pending"},
        ]}),
    )]];
    for round in 0..rounds {
        script.push(vec![tool_use_named(
            &format!("t{round}"),
            "bash",
            json!({"command": "true"}),
        )]);
    }
    script.push(vec![AssistantBlock::Text {
        text: "done".into(),
    }]);

    let cfg = crate::tools::testutil::TestConfig::new("agent-todo-reminder-loop")
        .provider(Provider::mock(script))
        .max_rounds(Some(rounds + 4))
        .build();
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("do the multi-step thing"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let reminders: Vec<usize> = history
        .messages()
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::User
                && matches!(&message.content[0], ContentBlock::Text { text } if text
                    .starts_with("<system-reminder>"))
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(reminders.len(), 1, "one reminder, once per revision");

    let messages = history.messages();
    let reminder = &messages[reminders[0]];
    assert_eq!(reminder.content.len(), 1, "a message of its own");
    let ContentBlock::Text { text } = &reminder.content[0] else {
        panic!("expected text");
    };
    assert!(
        text.contains("- [in_progress] Read the reference projects"),
        "{text}"
    );
    assert!(
        text.contains("- [pending] Wire the round-boundary reminder"),
        "{text}"
    );
    // Round boundary: the round before it closed with its tool_result, and the
    // next message is the assistant's answer to both.
    assert!(
        messages[reminders[0] - 1]
            .content
            .iter()
            .all(|block| matches!(block, ContentBlock::ToolResult { .. })),
        "{:?}",
        messages[reminders[0] - 1]
    );
    assert_eq!(messages[reminders[0] + 1].role, Role::Assistant);
}

/// Plan 197 from the loop: a read, then a `bash` round that rewrites the file
/// read, then an answer. The changed-reads reminder has to land as a user
/// message of its own between the `bash` round's `tool_result` and the answer,
/// and — being a history entry rather than part of a tool result — it has to be
/// in the rollout at that same place, for a replay and for a resume alike.
#[tokio::test]
async fn a_changed_read_is_named_between_rounds_and_replays_in_place() {
    let dir = std::env::temp_dir().join(format!("kloop-plan197-loop-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(dir).unwrap();
    let file = dir.join("notes.txt");
    std::fs::write(&file, "first draft\n").unwrap();

    let script = vec![
        vec![tool_use_named(
            "r0",
            "read_file",
            json!({"path": file.to_str().unwrap()}),
        )],
        vec![tool_use("b1", "printf 'second draft\\n' > notes.txt")],
        vec![AssistantBlock::Text {
            text: "done".into(),
        }],
    ];
    let mut cfg = crate::tools::testutil::TestConfig::new("agent-changed-reads")
        .provider(Provider::mock(script))
        .max_rounds(Some(6))
        .build()
        .test_clone();
    cfg.cwd = dir.clone();
    let cfg = Arc::new(cfg);
    let session = dir.join("session.jsonl");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("revise the notes"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let messages = history.messages();
    let reminders: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, message)| {
            message.role == Role::User
                && matches!(&message.content[0], ContentBlock::Text { text } if text
                    .starts_with("<system-reminder>"))
        })
        .map(|(index, _)| index)
        .collect();
    assert_eq!(reminders.len(), 1, "{messages:?}");
    let at = reminders[0];
    assert_eq!(
        messages[at].content,
        vec![ContentBlock::Text {
            text: "<system-reminder>\nFiles you read have changed on disk since, whether by a \
                   command you ran or by someone else. What you saw of them is out of date; read \
                   again before relying on it:\n- notes.txt\n</system-reminder>"
                .into(),
        }]
    );
    assert!(
        matches!(
            messages[at - 1].content.as_slice(),
            [ContentBlock::ToolResult { tool_use_id, .. }] if tool_use_id == "b1"
        ),
        "{:?}",
        messages[at - 1]
    );
    assert_eq!(messages[at + 1].role, Role::Assistant);

    assert_eq!(
        crate::rollout::load_session_snapshot(&session)
            .unwrap()
            .messages,
        messages,
        "replay"
    );
    assert_eq!(
        crate::rollout::resume_session(&session).unwrap().messages,
        messages,
        "resume"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn drain_inbox_offloads_only_large_machine_results() {
    let dir = std::env::temp_dir().join(format!("kloop-inbox-offload-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let inbox = Inbox::default();
    // Sized off the offload threshold so the fixture stays "large" when the
    // constant moves; a literal here silently stops testing the spill.
    let over = kloop_core_offload_cap() / 8;
    let large_agent = "agent-result".repeat(over);
    let large_program = "program-result".repeat(over);
    let large_user = "user-text".repeat(over);
    inbox.push(InboxItem::SubAgentResult {
        label: "agent-1".into(),
        summary: large_agent.clone(),
    });
    inbox.push(InboxItem::ProgramResult {
        label: "program-1".into(),
        run_id: "run-1".into(),
        summary: large_program.clone(),
    });
    inbox.push(InboxItem::Steer(large_user.clone()));
    let mut history = History::new(dir.clone());
    let ui: Arc<dyn Ui> = Arc::new(NullUi);

    assert!(drain_inbox(&inbox, &mut history, &ui));
    assert_eq!(history.messages().len(), 3);
    let text = |index: usize| match &history.messages()[index].content[0] {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("expected text, got {other:?}"),
    };
    assert!(text(0).contains("[Agent agent-1]"), "{}", text(0));
    assert!(text(0).contains("saved to"), "{}", text(0));
    assert!(!text(0).contains(&large_agent), "agent body stayed inline");
    assert!(
        text(1).contains("[Program program-1] run run-1"),
        "{}",
        text(1)
    );
    assert!(text(1).contains("saved to"), "{}", text(1));
    assert!(
        !text(1).contains(&large_program),
        "program body stayed inline"
    );
    assert!(text(2).contains(&large_user), "user steering was truncated");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

fn tool_use(id: &str, cmd: &str) -> AssistantBlock {
    tool_use_named(id, "bash", json!({"command": cmd}))
}

fn tool_use_named(id: &str, name: &str, input: Value) -> AssistantBlock {
    AssistantBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn structured_config(
    turns: Vec<MockTurn>,
) -> (
    Arc<Config>,
    Arc<std::sync::Mutex<Vec<kloop_provider::MockRequest>>>,
) {
    let (provider, seen) = Provider::mock_recording(turns);
    let ctx = crate::tools::testutil::test_ctx(1, "structured-agent");
    let mut cfg = ctx.cfg.test_clone();
    cfg.set_test_provider(provider);
    cfg.max_rounds = Some(10);
    cfg.local_agent = cfg.local_agent.child("agent-99".parse().unwrap());
    (Arc::new(cfg), seen)
}

/// Read-only source whose every call returns a result just under the
/// per-result cap — the shape the round budget exists for, and the one the
/// per-result cap alone never sees.
struct BulkSource;

impl ToolSource for BulkSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        Arc::from(vec![ToolDef {
            name: "bulk__result".into(),
            description: "Returns a result just under the per-result cap".into(),
            schema: json!({"type": "object"}),
        }])
    }

    fn is_readonly(&self, _tool: &str) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        _tool: &'a str,
        _input: &'a Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<SourceOutput>> + Send + 'a>,
    > {
        Box::pin(async move { Ok(SourceOutput::text("x".repeat(kloop_core_offload_cap() - 1))) })
    }
}

/// Run one round of six just-under-cap tool calls and hand back the tool
/// results as they were recorded. `schema` picks the dispatch path: `None` is
/// the ordinary one, `Some` the structured one, which routes its non-synthetic
/// calls through a second entry point.
async fn wide_round_results(tag: &str, schema: Option<Value>) -> Vec<ContentBlock> {
    let calls: Vec<AssistantBlock> = (0..6)
        .map(|n| tool_use_named(&format!("b{n}"), "bulk__result", json!({})))
        .collect();
    let finish = match &schema {
        Some(_) => tool_use_named("s1", "structured_output", json!({"ok": true})),
        None => AssistantBlock::Text {
            text: "done".into(),
        },
    };
    let provider = Provider::mock(vec![calls, vec![finish]]);
    let sources: Vec<Arc<dyn ToolSource>> = vec![Arc::new(BulkSource)];
    let cfg = crate::tools::testutil::TestConfig::new(tag)
        .provider(provider)
        .tool_sources(sources)
        .build();
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("read six things"));
    let cancel = CancellationToken::new();
    let outcome = match schema {
        Some(schema) => run_structured_turn(&cfg, &mut history, &ui, &cancel, 0, schema).await,
        None => run_turn(&cfg, &mut history, &ui, &cancel, 0).await,
    };
    assert_eq!(outcome.reason, EndReason::Completed, "{tag}");
    let results = history
        .messages()
        .iter()
        .find(|message| {
            matches!(
                message.content.first(),
                Some(ContentBlock::ToolResult { tool_use_id, .. }) if tool_use_id == "b0"
            )
        })
        .unwrap_or_else(|| panic!("{tag}: the round's tool results were never recorded"))
        .content
        .clone();
    let _ = std::fs::remove_dir_all(&cfg.offload_dir);
    results
}

/// The round budget lives in `History::record`, which is the single place a
/// round's tool results enter the history — the ordinary dispatch path and the
/// structured one both end there. This pins that: neither path can put six
/// just-under-cap results (192 000 chars) into one round.
#[tokio::test]
async fn a_wide_round_lands_under_budget_on_both_dispatch_paths() {
    for (tag, schema) in [
        ("round-budget-ordinary", None),
        (
            "round-budget-structured",
            Some(json!({
                "type": "object",
                "properties": {"ok": {"type": "boolean"}},
                "required": ["ok"],
                "additionalProperties": false
            })),
        ),
    ] {
        let results = wide_round_results(tag, schema).await;
        let ids: Vec<&str> = results
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
                other => panic!("{tag}: expected tool result, got {other:?}"),
            })
            .collect();
        assert_eq!(ids, vec!["b0", "b1", "b2", "b3", "b4", "b5"], "{tag}");
        let texts: Vec<String> = results
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { content, .. } => content.as_text().into_owned(),
                other => panic!("{tag}: expected tool result, got {other:?}"),
            })
            .collect();
        let total: usize = texts.iter().map(|text| text.chars().count()).sum();
        assert!(total <= kloop_core_round_cap(), "{tag}: {total} chars");
        // Some of the round went to disk and some stayed inline: the budget
        // spilled what it had to and stopped, rather than spilling the round.
        assert!(
            texts.iter().any(|text| text.contains("Query it in place")),
            "{tag}: nothing was offloaded"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.chars().count() == kloop_core_offload_cap() - 1),
            "{tag}: every result was offloaded"
        );
    }
}

struct BarrierSource {
    defs: Vec<ToolDef>,
    barrier: Arc<tokio::sync::Barrier>,
}

impl BarrierSource {
    fn new(barrier: Arc<tokio::sync::Barrier>) -> Arc<Self> {
        Arc::new(Self {
            defs: vec![ToolDef {
                name: "test__blocking_read".into(),
                description: "Wait until both read calls have started".into(),
                schema: json!({
                    "type": "object",
                    "properties": {"value": {"type": "string"}},
                    "required": ["value"],
                    "additionalProperties": false
                }),
            }],
            barrier,
        })
    }
}

impl ToolSource for BarrierSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        Arc::from(self.defs.clone())
    }

    fn is_readonly(&self, tool: &str) -> bool {
        tool == "test__blocking_read"
    }

    fn call<'a>(
        &'a self,
        _tool: &'a str,
        input: &'a Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<SourceOutput>> + Send + 'a>,
    > {
        let barrier = self.barrier.clone();
        let value = input["value"].as_str().unwrap_or_default().to_string();
        Box::pin(async move {
            barrier.wait().await;
            Ok(SourceOutput::text(value))
        })
    }
}

#[tokio::test]
async fn structured_turn_exposes_synthetic_tool_and_retries_invalid_value() {
    let schema = json!({
        "type": "object",
        "properties": {"count": {"type": "integer"}},
        "required": ["count"],
        "additionalProperties": false
    });
    let (cfg, seen) = structured_config(vec![
        MockTurn::Blocks(vec![tool_use_named(
            "s1",
            "structured_output",
            json!({"count": "bad"}),
        )]),
        MockTurn::Blocks(vec![tool_use_named(
            "s2",
            "structured_output",
            json!({"count": 2}),
        )]),
    ]);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("return a count"));
    let outcome = run_structured_turn(
        &cfg,
        &mut history,
        &ui,
        &CancellationToken::new(),
        1,
        schema.clone(),
    )
    .await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.structured_output, Some(json!({"count": 2})));
    assert_eq!(outcome.final_text, "");
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        let synthetic = request
            .tools
            .iter()
            .find(|tool| tool.name == "structured_output")
            .unwrap();
        assert_eq!(synthetic.schema, schema);
    }
    assert!(matches!(
        &history.messages()[2].content[0],
        ContentBlock::ToolResult { is_error: true, .. }
    ));
    assert!(matches!(
        &history.messages().last().unwrap().content[0],
        ContentBlock::ToolResult {
            is_error: false,
            ..
        }
    ));
}

#[tokio::test]
async fn structured_turn_nudges_missing_calls_and_exhausts() {
    let schema = json!({"type": "string"});
    let (cfg, seen) = structured_config(vec![
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "plain".into(),
        }]),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "still plain".into(),
        }]),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "never called".into(),
        }]),
    ]);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("return structured"));
    let outcome = run_structured_turn(
        &cfg,
        &mut history,
        &ui,
        &CancellationToken::new(),
        1,
        schema,
    )
    .await;
    assert!(matches!(outcome.reason, EndReason::Error(ref error) if error.contains("3 attempts")));
    assert_eq!(seen.lock().unwrap().len(), 3);
    assert!(history.messages().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.contains("Call structured_output now"))
        })
    }));
}

#[tokio::test]
async fn structured_turn_batches_ordinary_tools_and_preserves_response_order() {
    let schema = json!({"type": "array", "items": {"type": "integer"}});
    let (base, _) = structured_config(vec![MockTurn::Blocks(vec![
        tool_use_named("o1", "test__blocking_read", json!({"value": "first"})),
        tool_use_named("s1", "structured_output", json!({"value": [1, 2]})),
        tool_use_named("o2", "test__blocking_read", json!({"value": "second"})),
    ])]);
    let mut cfg = base.test_clone();
    cfg.tool_sources = vec![BarrierSource::new(Arc::new(tokio::sync::Barrier::new(2)))];
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("work then return"));
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        run_structured_turn(
            &cfg,
            &mut history,
            &ui,
            &CancellationToken::new(),
            1,
            schema,
        ),
    )
    .await
    .expect("ordinary read-only tools should run as one concurrent batch");
    assert_eq!(outcome.structured_output, Some(json!([1, 2])));
    let results = &history.messages().last().unwrap().content;
    assert_eq!(results.len(), 3);
    let ids: Vec<&str> = results
        .iter()
        .map(|result| match result {
            ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
            _ => panic!("expected tool result"),
        })
        .collect();
    assert_eq!(ids, vec!["o1", "s1", "o2"]);
    assert!(matches!(
        &results[1],
        ContentBlock::ToolResult {
            is_error: false,
            ..
        }
    ));
}
#[tokio::test]
async fn ordinary_turn_never_exposes_structured_output() {
    let (cfg, seen) = structured_config(vec![MockTurn::Blocks(vec![AssistantBlock::Text {
        text: "plain result".into(),
    }])]);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("ordinary child"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 1).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "plain result");
    assert!(
        seen.lock().unwrap()[0]
            .tools
            .iter()
            .all(|tool| tool.name != "structured_output")
    );
}

#[tokio::test]
async fn structured_output_waits_for_a_late_peer_message() {
    use kloop_provider::MockTurn;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct SendOnToolUi {
        sender: crate::agent_mailbox::LocalAgentContext,
        target: kloop_protocol::LocalAgentId,
        publish_ui: Arc<dyn Ui>,
        fired: AtomicBool,
    }
    impl Ui for SendOnToolUi {
        fn emit(&self, event: &Event) {
            if matches!(event, Event::ItemStarted { item: Item::ToolCall { name, .. }, .. } if name == "bash")
                && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.sender
                    .send(
                        self.target.clone(),
                        "late structured review".into(),
                        "fold this into the structured answer".into(),
                        &self.publish_ui,
                    )
                    .unwrap();
            }
        }
    }

    let schema = json!({
        "type": "object",
        "properties": {"count": {"type": "integer"}},
        "required": ["count"],
        "additionalProperties": false,
    });
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![
            AssistantBlock::ToolUse {
                id: "structured-1".into(),
                name: crate::structured_output::TOOL_NAME.into(),
                input: json!({"count": 1}),
            },
            AssistantBlock::ToolUse {
                id: "bash-1".into(),
                name: "bash".into(),
                input: json!({"command":"echo hi"}),
            },
        ]),
        MockTurn::Blocks(vec![AssistantBlock::ToolUse {
            id: "structured-2".into(),
            name: crate::structured_output::TOOL_NAME.into(),
            input: json!({"count": 2}),
        }]),
    ]);
    let root_ctx = crate::tools::testutil::with_provider(
        crate::tools::testutil::test_ctx(0, "structured-peer"),
        provider,
    );
    let target: kloop_protocol::LocalAgentId = "agent-79".parse().unwrap();
    let mut cfg = root_ctx.cfg.test_clone();
    cfg.local_agent = cfg.local_agent.child(target.clone());
    cfg.max_rounds = Some(5);
    let cfg = Arc::new(cfg);
    let publish_ui: Arc<dyn Ui> = Arc::new(NullUi);
    let _lease = cfg
        .local_agent
        .register_child(
            Arc::clone(&cfg.inbox),
            None,
            "structured peer child",
            publish_ui.clone(),
        )
        .unwrap();
    let ui: Arc<dyn Ui> = Arc::new(SendOnToolUi {
        sender: root_ctx.cfg.local_agent.clone(),
        target,
        publish_ui,
        fired: AtomicBool::new(false),
    });
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("produce structured output"));

    let outcome = run_structured_turn(
        &cfg,
        &mut history,
        &ui,
        &CancellationToken::new(),
        1,
        schema,
    )
    .await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.rounds, 2);
    assert_eq!(outcome.structured_output, Some(json!({"count": 2})));
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(history.messages().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.contains("late structured review"))
        })
    }));
}

#[tokio::test]
async fn structured_turn_cancellation_never_accepts_a_late_value() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (cfg, _) = structured_config(vec![MockTurn::Gate {
        started: started_tx,
        release: release_rx,
        blocks: vec![tool_use_named(
            "late",
            "structured_output",
            json!("late value"),
        )],
    }]);
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("return a string"));

        run_structured_turn(
            &cfg,
            &mut history,
            &ui,
            &child_cancel,
            1,
            json!({"type": "string"}),
        )
        .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("sampling did not start")
        .expect("sampling gate dropped");
    cancel.cancel();
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(2), handle)
        .await
        .expect("structured child ignored cancellation")
        .expect("structured child panicked");
    tokio::task::yield_now().await;
    assert!(
        release_tx.send(()).is_err(),
        "cancelled sampling must abort and drop the provider producer"
    );
    assert_eq!(outcome.reason, EndReason::Aborted);
    assert_eq!(outcome.structured_output, None);
    assert_eq!(outcome.final_text, "");
}

#[tokio::test]
async fn mock_end_to_end_three_rounds() {
    let provider = Provider::mock(vec![
        vec![tool_use("t1", "echo one"), tool_use("t2", "echo two")],
        vec![tool_use("t3", "true")],
        vec![AssistantBlock::Text {
            text: "all done".into(),
        }],
    ]);
    let cfg = crate::tools::testutil::TestConfig::new("test-e2e")
        .provider(provider)
        .max_rounds(Some(10))
        .build();
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("run the demo"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "all done");
    assert_eq!(outcome.rounds, 3);

    let msgs = history.messages();
    let shape: Vec<(Role, Vec<&str>)> = msgs
        .iter()
        .map(|m| {
            let kinds = m
                .content
                .iter()
                .map(|b| match b {
                    ContentBlock::Text { .. } => "text",
                    ContentBlock::Thinking { .. } => "thinking",
                    ContentBlock::RedactedThinking { .. } => "redacted_thinking",
                    ContentBlock::ToolUse { .. } => "tool_use",
                    ContentBlock::ToolResult { .. } => "tool_result",
                    ContentBlock::Image { .. } => "image",
                })
                .collect();
            (m.role, kinds)
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            (Role::User, vec!["text"]),
            (Role::Assistant, vec!["tool_use", "tool_use"]),
            (Role::User, vec!["tool_result", "tool_result"]),
            (Role::Assistant, vec!["tool_use"]),
            (Role::User, vec!["tool_result"]),
            (Role::Assistant, vec!["text"]),
        ]
    );

    // The concurrent batch preserved request order and captured output.
    assert_eq!(
        msgs[2].content[0],
        ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "one\n".into(),
            is_error: false,
        }
    );
    assert_eq!(
        msgs[2].content[1],
        ContentBlock::ToolResult {
            tool_use_id: "t2".into(),
            content: "two\n".into(),
            is_error: false,
        }
    );
    assert_eq!(
        msgs[4].content[0],
        ContentBlock::ToolResult {
            tool_use_id: "t3".into(),
            content: "(no output)".into(),
            is_error: false,
        }
    );
}

/// Slice 5 routing: a sub-agent's turn fires subagent_start/subagent_stop,
/// NOT pre_turn/post_turn (the split both references converge on), and
/// subagent_stop carries the agent label, the sub-agent's own transcript
/// path, and its final message.
#[tokio::test]
async fn subagent_turn_routes_to_subagent_hooks() {
    use crate::hooks::{DEFAULT_TIMEOUT_MS, HookDef, HookEvent, Hooks};
    let dir = std::env::temp_dir().join(format!("kloop-subhook-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let hook = |event, script: String| HookDef {
        event,
        command: crate::hooks::test_shell_command(&script),
        matcher: None,
        timeout_ms: DEFAULT_TIMEOUT_MS,
    };
    let d = dir.display();
    let hooks = Hooks {
        defs: vec![
            hook(HookEvent::PreTurn, format!("touch {d}/pre_turn")),
            hook(HookEvent::PostTurn, format!("touch {d}/post_turn")),
            hook(
                HookEvent::SubagentStart,
                format!("touch {d}/subagent_start"),
            ),
            hook(HookEvent::SubagentStop, format!("cat > {d}/subagent_stop")),
        ],
    };

    let provider = Provider::mock(vec![vec![AssistantBlock::Text {
        text: "sub answer".into(),
    }]]);
    let mut cfg = compaction_cfg(provider, 200_000, "subhook").test_clone();
    cfg.local_agent = cfg.local_agent.child("agent-7".parse().unwrap());
    cfg.session_id = "parent-sess".into();
    cfg.hooks = Arc::new(hooks);
    let cfg = Arc::new(cfg);

    // Give the sub-agent a session file so the stop payload has a transcript.
    let session = dir.join("agent-7.jsonl");
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("do sub work"));

    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 1).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    assert!(dir.join("subagent_start").exists(), "subagent_start fired");
    assert!(dir.join("subagent_stop").exists(), "subagent_stop fired");
    assert!(
        !dir.join("pre_turn").exists(),
        "pre_turn must NOT fire for a sub-agent"
    );
    assert!(
        !dir.join("post_turn").exists(),
        "post_turn must NOT fire for a sub-agent"
    );

    let payload: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(dir.join("subagent_stop"))
            .unwrap()
            .trim(),
    )
    .unwrap();
    assert_eq!(
        payload,
        json!({
            "event": "subagent_stop",
            "session_id": "parent-sess",
            "agent": "agent-7",
            // This Config is built by hand and never registered in the live
            // directory, so the type is the name an untyped sub-agent reports.
            "agent_type": crate::hooks::DEFAULT_AGENT_TYPE,
            "agent_transcript_path": session.to_string_lossy(),
            "last_assistant_message": "sub answer",
        })
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Plan 153: the agent TYPE reaches the sub-agent hook points, and a matcher
/// filters on it. The type lives only in the live Agent directory — the label
/// ("agent-N") is a spawn counter — so this covers the whole path from
/// `register_child` to the hook's stdin.
#[tokio::test]
async fn subagent_hooks_carry_the_registered_agent_type() {
    use crate::hooks::{DEFAULT_TIMEOUT_MS, HookDef, HookEvent, Hooks};
    let dir = std::env::temp_dir().join(format!("kloop-subtype-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let d = dir.display();
    let hook = |event, matcher: &str, script: String| HookDef {
        event,
        command: crate::hooks::test_shell_command(&script),
        matcher: Some(matcher.into()),
        timeout_ms: DEFAULT_TIMEOUT_MS,
    };
    let hooks = Hooks {
        defs: vec![
            hook(
                HookEvent::SubagentStop,
                "reviewer",
                format!("cat > {d}/matched"),
            ),
            hook(
                HookEvent::SubagentStop,
                "explorer",
                format!("touch {d}/other-type"),
            ),
        ],
    };

    let provider = Provider::mock(vec![vec![AssistantBlock::Text {
        text: "reviewed".into(),
    }]]);
    let mut cfg = compaction_cfg(provider, 200_000, "subtype").test_clone();
    cfg.local_agent = cfg.local_agent.child("agent-8".parse().unwrap());
    cfg.session_id = "parent-sess".into();
    cfg.hooks = Arc::new(hooks);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let _lease = cfg
        .local_agent
        .register_child(
            Arc::clone(&cfg.inbox),
            Some("reviewer"),
            "typed child",
            ui.clone(),
        )
        .unwrap();

    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("review this"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 1).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let payload: serde_json::Value = serde_json::from_str(
        std::fs::read_to_string(dir.join("matched"))
            .expect("the matching hook ran")
            .trim(),
    )
    .unwrap();
    assert_eq!(
        payload,
        json!({
            "event": "subagent_stop",
            "session_id": "parent-sess",
            "agent": "agent-8",
            "agent_type": "reviewer",
            "last_assistant_message": "reviewed",
        })
    );
    assert!(
        !dir.join("other-type").exists(),
        "a hook matching another agent type must not fire"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

fn compaction_cfg(provider: Provider, window: u64, tag: &str) -> Arc<Config> {
    crate::tools::testutil::TestConfig::new(&format!("test-{tag}"))
        .provider(provider)
        .max_rounds(Some(10))
        .context_window(Some(window))
        .build()
}

/// Predictive: a fat history under a small (but > growth reserve) window
/// triggers compaction BEFORE the sampling request. Mock turn 1 serves
/// the summary, turn 2 the actual reply.
#[tokio::test]
async fn predictive_compaction_fires_before_sampling() {
    let provider = Provider::mock(vec![
        vec![AssistantBlock::Text {
            text: "summary of everything so far".into(),
        }],
        vec![AssistantBlock::Text {
            text: "final answer".into(),
        }],
    ]);
    // growth = 8192 + 15_000 = 23_192; window 30_000 → threshold ≈ 6_808 tokens.
    // Each fat message is sized off the keep budget so one of them still has to
    // fold when that constant moves — at a literal size a larger keep budget
    // swallows the whole fixture and the test silently stops exercising
    // compaction.
    let fat = crate::compact::keep_recent_tokens() as usize * 4;
    let cfg = compaction_cfg(provider, 30_000, "predictive");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("x".repeat(fat)));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "y".repeat(fat),
    }]));
    history.record(Message::user_text("now answer briefly"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "final answer");
    let msgs = history.messages();
    // [anchors, summary, ...kept tail..., assistant reply]; the fat prefix is gone.
    assert_eq!(msgs[0].injected, Some(Injected::UserAnchors));
    let ContentBlock::Text { text } = &msgs[1].content[0] else {
        panic!("expected text summary at history start");
    };
    assert!(
        text.starts_with(crate::compact::SUMMARY_PREFIX),
        "history should start with the compaction summary"
    );
    assert!(
        history.estimated_tokens() < 5_000,
        "compaction should have shrunk the history, got {} tokens",
        history.estimated_tokens()
    );
}

/// Notes a turn emitted, for asserting whether predictive compaction fired.
#[derive(Default)]
struct NotesUi(std::sync::Mutex<Vec<String>>);

impl Ui for NotesUi {
    fn emit(&self, ev: &Event) {
        if let Event::Note(note) = ev {
            self.0.lock().unwrap().push(note.clone());
        }
    }
}

const PREDICTED_NOTE: &str = "predicted context overflow; compacting history";

fn growth(cfg: &Config) -> u64 {
    crate::compact::max_turn_growth(cfg.provider_route.api_family().max_output_tokens())
}

fn short_history(cfg: &Config) -> History {
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("earlier"));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "earlier reply".into(),
    }]));
    history.record(Message::user_text("now answer briefly"));
    history
}

#[test]
fn the_request_overhead_is_added_only_without_an_anchor() {
    let cfg = compaction_cfg(Provider::mock(vec![]), 200_000, "overhead-anchor");
    let mut history = short_history(&cfg);
    let bare = history.estimated_tokens();
    assert_eq!(estimate_request(&history, || 100), bare + 100);
    history.note_usage(5_000);
    assert_eq!(
        estimate_request(&history, || panic!("an anchor already holds the overhead")),
        5_000
    );
}

/// Before any response, the system prompt and the tool array are part of the
/// request and nobody has measured them yet: history alone fits the window
/// here, history plus the tools does not, and the turn must compact.
#[tokio::test]
async fn predictive_compaction_counts_the_tools_before_the_first_response() {
    let provider = Provider::mock(vec![
        vec![AssistantBlock::Text {
            text: "summary".into(),
        }],
        vec![AssistantBlock::Text {
            text: "final answer".into(),
        }],
    ]);
    let probe = compaction_cfg(Provider::mock(vec![]), 200_000, "overhead-probe");
    let history = short_history(&probe);
    let overhead = crate::agent::context_estimate(&probe, &history) - history.estimated_tokens();
    assert!(
        overhead > 1_000,
        "the tool array alone is thousands: {overhead}"
    );
    let window = growth(&probe) + history.estimated_tokens() + overhead / 2;

    let cfg = compaction_cfg(provider, window, "overhead-pre-anchor");
    let mut history = short_history(&cfg);
    let notes = Arc::new(NotesUi::default());
    let ui: Arc<dyn Ui> = notes.clone();
    run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        notes.0.lock().unwrap().iter().any(|n| n == PREDICTED_NOTE),
        "{:?}",
        notes.0.lock().unwrap()
    );
}

/// After a response, the anchor is the provider's count of the whole request,
/// injected instructions included. Counting them again on top used to push
/// this turn over a window it fits in.
#[tokio::test]
async fn predictive_compaction_does_not_count_the_injected_context_twice() {
    let provider = Provider::mock(vec![vec![AssistantBlock::Text {
        text: "final answer".into(),
    }]]);
    let base = compaction_cfg(provider, 200_000, "overhead-anchored");
    let mut cfg = (*base).clone();
    // ~10k tokens of instructions, all of them inside the anchor below.
    cfg.project_instructions = Some("p".repeat(40_000));
    let anchored = 1_000;
    cfg.context_window = Some(growth(&cfg) + anchored + 5_000);
    let cfg = Arc::new(cfg);
    let mut history = short_history(&cfg);
    history.note_usage(anchored);
    let notes = Arc::new(NotesUi::default());
    let ui: Arc<dyn Ui> = notes.clone();

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.final_text, "final answer");
    assert!(
        !notes.0.lock().unwrap().iter().any(|n| n == PREDICTED_NOTE),
        "{:?}",
        notes.0.lock().unwrap()
    );
}

/// Reactive: the first sampling request is rejected as too large; the
/// loop compacts once (mock turn 2 = summary) and retries successfully
/// (turn 3), with no user-visible error.
#[tokio::test]
async fn overflow_compacts_and_retries() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Overflow,
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "summary of everything so far".into(),
        }]),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "recovered answer".into(),
        }]),
    ]);
    // Large window: predictive stays silent, only the reactive path runs.
    let cfg = compaction_cfg(provider, 200_000, "reactive");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("earlier context"));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "earlier reply".into(),
    }]));
    history.record(Message::user_text("the request that overflows"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "recovered answer");
    let ContentBlock::Text { text } = &history.messages()[1].content[0] else {
        panic!("expected text summary at history start");
    };
    assert!(text.starts_with(crate::compact::SUMMARY_PREFIX));
}

/// A second overflow after a successful compaction must surface as an
/// error instead of looping.
#[tokio::test]
async fn repeated_overflow_surfaces_error() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Overflow,
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "summary".into(),
        }]),
        MockTurn::Overflow,
    ]);
    let cfg = compaction_cfg(provider, 200_000, "reactive-repeat");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("earlier context"));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "earlier reply".into(),
    }]));
    history.record(Message::user_text("still too big"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert!(
        matches!(outcome.reason, EndReason::Error(_)),
        "second overflow must not loop, got {:?}",
        outcome.reason
    );
}

/// Predictive NoOp leaves the original sampling request on the normal path.
#[tokio::test]
async fn predictive_noop_continues_sampling_without_provider_compaction() {
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Blocks(text("answer"))]);
    let cfg = compaction_cfg(provider, 30_000, "predictive-noop");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(kloop_protocol::Message::injected(
        kloop_protocol::Injected::ContextSummary,
        format!("{}{}", crate::compact::SUMMARY_PREFIX, "x".repeat(30_000)),
    ));
    history.record(Message::user_text("current request"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "answer");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "NoOp must not consume a compaction turn");
    assert_eq!(seen[0].system, "test");
}

/// The main sampling path sends the session reasoning effort: it is minted onto
/// the attempt from the frozen route, never threaded separately, so `/effort`
/// governs the turn without touching the sampler.
#[tokio::test]
async fn turn_samples_at_the_session_effort() {
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Blocks(text("answer"))]);
    let cfg = compaction_cfg(provider, 200_000, "turn-effort");
    let state = crate::provider_route::SessionProviderState::from_route(
        Arc::clone(&cfg.provider_catalog),
        cfg.provider_route.clone(),
    );
    state.set_effort(Some(kloop_protocol::ReasoningEffort::Low));
    let cfg = Arc::new(cfg.clone_with_provider_route(state.freeze()));
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hi"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        seen.lock().unwrap()[0].effort,
        Some(kloop_protocol::ReasoningEffort::Low)
    );
}

/// The session id rides every sampling request as the prompt-cache routing
/// hint. It is a routing hint only — a missing one costs cache hits, never
/// correctness — but on one real gateway that cost is real: without it an identical
/// prefix hit the cache on only one of three consecutive requests.
#[tokio::test]
async fn turn_samples_with_the_session_id_as_the_cache_key() {
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Blocks(text("answer"))]);
    let mut cfg = compaction_cfg(provider, 200_000, "turn-cache-key").test_clone();
    cfg.session_id = "sess-abc".into();
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hi"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        seen.lock().unwrap()[0].cache_key.as_deref(),
        Some("sess-abc")
    );
}

/// An unbound session (`--mock`, tests) sends no cache key at all rather than
/// an empty one, which would herd every such run onto a single bucket.
#[tokio::test]
async fn unbound_session_sends_no_cache_key() {
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Blocks(text("answer"))]);
    let cfg = compaction_cfg(provider, 200_000, "turn-no-cache-key");
    assert_eq!(cfg.session_id, "", "fixture must start unbound");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hi"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(seen.lock().unwrap()[0].cache_key, None);
}

/// Reactive NoOp ends the turn instead of retrying an unchanged request.
#[tokio::test]
async fn reactive_noop_does_not_retry_after_overflow() {
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Overflow]);
    let cfg = compaction_cfg(provider, 200_000, "reactive-noop");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(kloop_protocol::Message::injected(
        kloop_protocol::Injected::ContextSummary,
        format!(
            "{prefix}already compacted",
            prefix = crate::compact::SUMMARY_PREFIX
        ),
    ));
    history.record(Message::user_text("current request"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(outcome.reason, EndReason::Error(ref error) if error.contains("made no changes")),
        "reactive NoOp should terminate the turn: {:?}",
        outcome.reason
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
}

/// Reactive compaction runs on the attempt that is active after retries.
#[tokio::test]
async fn reactive_compaction_uses_the_active_attempt() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Error("primary 1".into()),
        MockTurn::Error("primary 2".into()),
        MockTurn::Overflow,
        MockTurn::Blocks(text("compacted summary")),
        MockTurn::Blocks(text("answer after compaction")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "reactive-compaction").test_clone();
    cfg.set_test_route_models(&["mock"]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("earlier context"));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "earlier reply".into(),
    }]));
    history.record(Message::user_text("current request"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "answer after compaction");
    let seen = seen.lock().unwrap();
    // The overflow, the compaction it triggers and the retry all run on the
    // one attempt that is active after the retries.
    assert_eq!(seen[2].model, "mock");
    assert_eq!(seen[3].model, "mock");
    assert_eq!(seen[4].model, "mock");
    assert!(
        seen[3]
            .system
            .starts_with("You summarize an in-progress coding-agent session")
    );
}

/// A history over a 30k window's predictive threshold, with something to fold.
fn fat_history(cfg: &Config) -> History {
    let fat = crate::compact::keep_recent_tokens() as usize * 4;
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("x".repeat(fat)));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "y".repeat(fat),
    }]));
    history.record(Message::user_text("request 1"));
    history
}

fn is_summary_request(request: &kloop_provider::MockRequest) -> bool {
    request
        .system
        .starts_with("You summarize an in-progress coding-agent session")
}

/// A summary request that keeps failing is paid for twice, then not at all
/// for three turns — the turns still run, on the uncompacted history — and
/// is tried again on the sixth.
#[tokio::test]
async fn failing_predictive_compaction_pauses_for_three_turns() {
    let refused = || MockTurn::Failure(ProviderFailure::protocol("summary refused"));
    let (provider, seen) = Provider::mock_recording(vec![
        refused(),
        MockTurn::Blocks(text("answer 1")),
        refused(),
        MockTurn::Blocks(text("answer 2")),
        MockTurn::Blocks(text("answer 3")),
        MockTurn::Blocks(text("answer 4")),
        MockTurn::Blocks(text("answer 5")),
        refused(),
        MockTurn::Blocks(text("answer 6")),
    ]);
    let cfg = compaction_cfg(provider, 30_000, "breaker-pause");
    let mut history = fat_history(&cfg);
    let mut summaries = Vec::new();
    let mut notes = Vec::new();
    for turn in 1..=6 {
        if turn > 1 {
            history.record(Message::user_text(format!("request {turn}")));
        }
        let turn_notes = Arc::new(NotesUi::default());
        let ui: Arc<dyn Ui> = turn_notes.clone();
        let sent_before = seen.lock().unwrap().len();

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.final_text, format!("answer {turn}"));
        let sent = &seen.lock().unwrap()[sent_before..];
        summaries.push(sent.iter().filter(|r| is_summary_request(r)).count());
        notes.push(turn_notes.0.lock().unwrap().clone());
    }

    assert_eq!(summaries, [1, 1, 0, 0, 0, 1]);
    let failed = "predictive compaction failed: compaction request failed: provider protocol error: summary refused";
    assert_eq!(notes[0], [PREDICTED_NOTE, failed]);
    assert_eq!(
        notes[1],
        [
            PREDICTED_NOTE,
            failed,
            "automatic compaction paused for 3 turns after 2 consecutive failures; \
/compact still works"
        ]
    );
    // The pause is announced once, not every turn it skips.
    assert_eq!(notes[2..5], vec![Vec::<String>::new(); 3]);
    assert_eq!(notes[5], [PREDICTED_NOTE, failed]);
}

/// The pause gates only the predictive path. An overflow the provider
/// actually rejects is still compacted reactively, and that success closes
/// the breaker.
#[tokio::test]
async fn reactive_compaction_runs_during_a_pause_and_closes_it() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Overflow,
        MockTurn::Blocks(text("summary")),
        MockTurn::Blocks(text("answer")),
    ]);
    let cfg = compaction_cfg(provider, 30_000, "breaker-reactive");
    let mut history = fat_history(&cfg);
    history.compaction_breaker().begin_turn();
    history.compaction_breaker().record_failure();
    history.compaction_breaker().record_failure();
    let ui: Arc<dyn Ui> = Arc::new(NullUi);

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "answer");
    let summaries: Vec<bool> = seen
        .lock()
        .unwrap()
        .iter()
        .map(is_summary_request)
        .collect();
    assert_eq!(summaries, [false, true, false]);
    assert_eq!(
        *history.compaction_breaker(),
        compact::CompactionBreaker::at(2, 0, None)
    );
}

/// A summary the user interrupted did not fail: it is not counted.
#[tokio::test]
async fn cancelled_predictive_compaction_is_not_a_failure() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (provider, _) = Provider::mock_recording(vec![MockTurn::Gate {
        started: started_tx,
        release: release_rx,
        blocks: text("never delivered"),
    }]);
    let cfg = compaction_cfg(provider, 30_000, "breaker-cancel");
    let cancel = CancellationToken::new();
    let child_cancel = cancel.clone();
    let handle = tokio::spawn(async move {
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = fat_history(&cfg);
        let outcome = run_turn(&cfg, &mut history, &ui, &child_cancel, 0).await;
        (outcome, history)
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("summary request did not start")
        .expect("summary gate dropped");
    cancel.cancel();
    let (outcome, mut history) = handle.await.unwrap();

    assert_eq!(outcome.reason, EndReason::Aborted);
    assert_eq!(
        *history.compaction_breaker(),
        compact::CompactionBreaker::at(1, 0, None)
    );
}

fn text(t: &str) -> Vec<AssistantBlock> {
    vec![AssistantBlock::Text { text: t.into() }]
}

/// A truncated final response gets a "continue" nudge instead of ending
/// the turn mid-thought.
#[tokio::test]
async fn truncated_response_recovers_with_continuation() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Truncated(text("part one, cut off mid-")),
        MockTurn::Blocks(text("part two, complete.")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "truncation");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("write something long"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    // The cut-off segment is preserved and prepended to the continuation,
    // so the deliverable is the whole answer — not just its tail.
    assert_eq!(
        outcome.final_text,
        "part one, cut off mid-part two, complete."
    );
    assert_eq!(outcome.rounds, 2);
    // [user, assistant(truncated), user(continue nudge), assistant(rest)]
    let msgs = history.messages();
    assert_eq!(msgs.len(), 4);
    assert_eq!(
        msgs[2],
        Message::injected(Injected::Harness, super::TRUNCATION_CONTINUE_MSG),
        "the continuation nudge must be recorded so history stays legal"
    );
}

/// Truncation nudges are bounded: after the limit the turn returns an error
/// while preserving every partial segment instead of misreporting completion.
#[tokio::test]
async fn truncation_recovery_is_bounded() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Truncated(text("cut 1")),
        MockTurn::Truncated(text("cut 2")),
        MockTurn::Truncated(text("cut 3")),
        MockTurn::Truncated(text("cut 4")),
        MockTurn::Truncated(text("cut 5")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "truncation-limit");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("write something very long"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(
        outcome.reason,
        EndReason::Error(TurnError::ProviderOutcome(AssistantOutcome::OutputLimit(
            kloop_protocol::OutputLimitKind::MaxOutputTokens,
        )))
    );
    // 3 nudges (the limit), so the 4th truncated response returns the error.
    // Every cut-off segment is accumulated into the final deliverable.
    assert_eq!(outcome.final_text, "cut 1cut 2cut 3cut 4");
    assert_eq!(outcome.rounds, 4);
    let nudges = history
        .messages()
        .iter()
        .filter(|m| *m == &Message::injected(Injected::Harness, super::TRUNCATION_CONTINUE_MSG))
        .count();
    assert_eq!(nudges, 3);
}

#[tokio::test]
async fn model_context_output_limit_keeps_its_typed_kind() {
    let truncated = || MockTurn::Outcome {
        blocks: text("cut"),
        outcome: AssistantOutcome::OutputLimit(kloop_protocol::OutputLimitKind::ModelContextWindow),
    };
    let provider =
        Provider::mock_scripted(vec![truncated(), truncated(), truncated(), truncated()]);
    let cfg = compaction_cfg(provider, 200_000, "context-limit-kind");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("long answer"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(
        outcome.reason,
        EndReason::Error(TurnError::ProviderOutcome(AssistantOutcome::OutputLimit(
            kloop_protocol::OutputLimitKind::ModelContextWindow,
        )))
    );
    assert_eq!(outcome.final_text, "cutcutcutcut");
}

#[tokio::test]
async fn validated_terminal_usage_is_recorded_before_assistant_message() {
    let dir = std::env::temp_dir().join(format!("kloop-usage-order-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let session = dir.join("session.jsonl");
    let provider = Provider::mock_scripted(vec![MockTurn::Response {
        blocks: text("answer"),
        outcome: AssistantOutcome::EndTurn,
        usage: usage(10),
    }]);
    let cfg = compaction_cfg(provider, 200_000, "usage-order");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("question"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        history.provider_usage().records(),
        &[ProviderUsageRecord::from_attempt(
            cfg.provider_route.primary_attempt().identity(),
            UsageOperation::Sampling,
            usage(10),
        )]
    );
    let lines: Vec<serde_json::Value> = std::fs::read_to_string(&session)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(
        lines.iter().map(|line| &line["type"]).collect::<Vec<_>>(),
        vec![
            "provider_route_initial",
            "message",
            "provider_usage",
            "message",
            // The turn's terminal closes the file; it is written after every
            // message the turn produced, so it never lands between the usage
            // record and the assistant message this test pins.
            "turn_terminal",
        ]
    );
    assert_eq!(lines[2]["parent"], lines[1]["id"]);
    assert_eq!(lines[3]["parent"], lines[2]["id"]);
    assert_eq!(lines[4]["status"], "completed");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn every_valid_terminal_outcome_keeps_reported_usage() {
    let outcomes = [
        AssistantOutcome::EndTurn,
        AssistantOutcome::ToolUse,
        AssistantOutcome::OutputLimit(kloop_protocol::OutputLimitKind::MaxOutputTokens),
        AssistantOutcome::Refused,
        AssistantOutcome::Filtered,
        AssistantOutcome::Incomplete(IncompleteReason::Provider("paused".into())),
    ];
    for (index, outcome) in outcomes.into_iter().enumerate() {
        let blocks = if matches!(outcome, AssistantOutcome::ToolUse) {
            vec![tool_use("t1", "echo hi")]
        } else {
            text("answer")
        };
        let provider = Provider::mock_scripted(vec![MockTurn::Response {
            blocks,
            outcome,
            usage: usage(index as u64 + 1),
        }]);
        let mut cfg =
            compaction_cfg(provider, 200_000, &format!("usage-outcome-{index}")).test_clone();
        cfg.max_rounds = Some(1);
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("request"));

        let _ = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(
            history.provider_usage().records(),
            &[ProviderUsageRecord::from_attempt(
                cfg.provider_route.primary_attempt().identity(),
                UsageOperation::Sampling,
                usage(index as u64 + 1),
            )],
            "terminal case {index}"
        );
    }
}

#[tokio::test]
async fn failures_and_missing_usage_do_not_create_records() {
    let cases = vec![
        vec![MockTurn::Blocks(text("no usage"))],
        vec![MockTurn::Failure(ProviderFailure::protocol("bad frame"))],
        vec![MockTurn::Overflow],
        vec![MockTurn::PartialError(text("partial"), "dropped".into())],
    ];
    for (index, turns) in cases.into_iter().enumerate() {
        let provider = Provider::mock_scripted(turns);
        let cfg = compaction_cfg(provider, 200_000, &format!("usage-none-{index}"));
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("request"));

        let _ = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert!(
            history.provider_usage().records().is_empty(),
            "failure case {index}"
        );
    }
}

#[tokio::test]
async fn retries_record_only_the_terminal_response() {
    let provider = Provider::mock_scripted(vec![
        MockTurn::Error("primary one".into()),
        MockTurn::Error("primary two".into()),
        MockTurn::Response {
            blocks: text("answer after retries"),
            outcome: AssistantOutcome::EndTurn,
            usage: usage(7),
        },
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "usage-fallback").test_clone();
    cfg.set_test_route_models(&["primary"]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("request"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        history.messages()[1].provider_provenance,
        Some(cfg.provider_route.primary_attempt().provenance(2),)
    );
    assert_eq!(
        history.provider_usage().records(),
        &[ProviderUsageRecord::from_attempt(
            cfg.provider_route.primary_attempt().identity(),
            UsageOperation::Sampling,
            usage(7),
        )]
    );
}

#[tokio::test]
async fn final_text_includes_every_text_block_in_provider_order() {
    let provider = Provider::mock(vec![vec![
        AssistantBlock::Text {
            text: "first ".into(),
        },
        AssistantBlock::Thinking {
            thinking: "middle".into(),
            signature: String::new(),
        },
        AssistantBlock::Text {
            text: "second".into(),
        },
    ]]);
    let cfg = compaction_cfg(provider, 200_000, "multiple-text-blocks");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("answer in pieces"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "first second");
}

#[tokio::test]
async fn empty_end_turn_completes_without_recording_empty_assistant() {
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Outcome {
        blocks: Vec::new(),
        outcome: AssistantOutcome::EndTurn,
    }]);
    let mut cfg = compaction_cfg(provider, 200_000, "empty-end-turn").test_clone();
    cfg.set_test_route_models(&["mock"]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("say nothing"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "");
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(history.messages(), &[Message::user_text("say nothing")]);
}

#[tokio::test]
async fn semantic_error_outcomes_record_content_without_retry() {
    let cases = [
        (
            AssistantOutcome::Refused,
            "model refused the request".to_string(),
        ),
        (
            AssistantOutcome::Filtered,
            "provider filtered the response".to_string(),
        ),
        (
            AssistantOutcome::Incomplete(IncompleteReason::Provider("paused".into())),
            "provider returned an incomplete response: paused".to_string(),
        ),
    ];
    for (index, (semantic, expected)) in cases.into_iter().enumerate() {
        let (provider, seen) = Provider::mock_recording(vec![
            MockTurn::Outcome {
                blocks: text("partial semantic content"),
                outcome: semantic.clone(),
            },
            MockTurn::Blocks(text("must not retry")),
        ]);
        let mut cfg = compaction_cfg(provider, 200_000, &format!("semantic-{index}")).test_clone();
        cfg.set_test_route_models(&["mock"]);
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("request"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(
            outcome.reason,
            EndReason::Error(TurnError::ProviderOutcome(semantic))
        );
        let EndReason::Error(error) = &outcome.reason else {
            unreachable!("typed provider outcome asserted above")
        };
        assert_eq!(error.to_string(), expected);
        assert_eq!(outcome.final_text, "partial semantic content");
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            history.messages()[1],
            mock_assistant(
                vec![ContentBlock::Text {
                    text: "partial semantic content".into(),
                }],
                "mock",
            )
        );
    }
}

#[tokio::test]
async fn core_rejects_outcome_tool_mismatch_and_invalid_tool_shape_before_dispatch() {
    let cases = [
        MockTurn::Outcome {
            blocks: vec![tool_use("t1", "echo no")],
            outcome: AssistantOutcome::EndTurn,
        },
        MockTurn::Outcome {
            blocks: vec![AssistantBlock::ToolUse {
                id: "t1".into(),
                name: "bash".into(),
                input: json!(["not", "an", "object"]),
            }],
            outcome: AssistantOutcome::ToolUse,
        },
        MockTurn::Outcome {
            blocks: vec![tool_use("dup", "echo one"), tool_use("dup", "echo two")],
            outcome: AssistantOutcome::ToolUse,
        },
    ];
    for (index, turn) in cases.into_iter().enumerate() {
        let provider = Provider::mock_scripted(vec![turn]);
        let cfg = compaction_cfg(provider, 200_000, &format!("invalid-tool-{index}"));
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("do not dispatch"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert!(matches!(outcome.reason, EndReason::Error(_)));
        assert_eq!(history.messages(), &[Message::user_text("do not dispatch")]);
    }
}

/// Unreadable arguments are the model's own mistake, not a broken wire. The
/// round runs the calls it could read, hands that one its text back as a
/// failed result, and the turn goes on — the alternative costs a whole turn to
/// recover something the model said correctly everywhere else.
#[tokio::test]
async fn unreadable_tool_arguments_become_a_failed_result_and_the_turn_goes_on() {
    let provider = Provider::mock(vec![
        vec![
            tool_use("good", "echo ran"),
            AssistantBlock::InvalidToolUse {
                id: "bad".into(),
                name: "bash".into(),
                raw: r#"{"command": "ls", "description": 看一眼}"#.into(),
                error: "expected value at line 1 column 34".into(),
            },
        ],
        vec![AssistantBlock::Text {
            text: "recovered".into(),
        }],
    ]);
    let cfg = crate::tools::testutil::TestConfig::new("invalid-tool-input")
        .provider(provider)
        .max_rounds(Some(10))
        .build();
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("run it"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "recovered");

    let messages = history.messages();
    // The call that could not be read is still a call, with the only input the
    // wire can carry, and it keeps its place among the round's calls.
    assert_eq!(
        messages[1].content[1],
        ContentBlock::ToolUse {
            id: "bad".into(),
            name: "bash".into(),
            input: json!({}),
        }
    );
    let [ran, failed] = &messages[2].content[..] else {
        panic!(
            "expected one result per call, got {:?}",
            messages[2].content
        );
    };
    assert!(matches!(
        ran,
        ContentBlock::ToolResult { tool_use_id, is_error: false, .. } if tool_use_id == "good"
    ));
    let ContentBlock::ToolResult {
        tool_use_id,
        content,
        is_error,
    } = failed
    else {
        panic!("expected a tool result, got {failed:?}");
    };
    assert_eq!(tool_use_id, "bad");
    assert!(is_error);
    let kloop_protocol::ToolResultContent::Text(text) = content else {
        panic!("expected text, got {content:?}");
    };
    // The model gets the parser's complaint and its own text back: without the
    // text it has nothing to compare against and resends the same string.
    assert_eq!(
        text,
        "bash was not run: its arguments were not valid JSON (expected value at line 1 column 34). \
         You sent: {\"command\": \"ls\", \"description\": 看一眼}\nCall bash again with valid JSON arguments."
    );
}

#[tokio::test]
async fn completed_block_without_delta_still_has_one_item_lifecycle() {
    struct EventUi(std::sync::Mutex<Vec<Event>>);
    impl Ui for EventUi {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    let provider = Provider::mock_scripted(vec![MockTurn::BlocksWithoutDeltas(text("final-only"))]);
    let cfg = compaction_cfg(provider, 200_000, "no-delta-item");
    let event_ui = Arc::new(EventUi(std::sync::Mutex::new(Vec::new())));
    let ui: Arc<dyn Ui> = event_ui.clone();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("answer"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        *event_ui.0.lock().unwrap(),
        vec![
            Event::ItemStarted {
                id: "msg-0".into(),
                item: Item::AssistantMessage {
                    text: String::new(),
                    status: crate::event::ItemStatus::InProgress,
                },
            },
            Event::ItemCompleted {
                id: "msg-0".into(),
                item: Item::AssistantMessage {
                    text: "final-only".into(),
                    status: crate::event::ItemStatus::Completed,
                },
            },
            // Every round closes by publishing the context size (plan: the
            // footer gauge must move during a turn, not only at its end).
            Event::Usage(crate::agent::context_estimate(&cfg, &history)),
        ]
    );
}

#[tokio::test]
async fn signed_empty_thinking_is_semantic_history_without_display_item() {
    struct EventUi(std::sync::Mutex<Vec<Event>>);
    impl Ui for EventUi {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }
    let block = AssistantBlock::Thinking {
        thinking: String::new(),
        signature: "signed".into(),
    };
    let provider = Provider::mock_scripted(vec![MockTurn::BlocksWithoutDeltas(vec![block])]);
    let cfg = compaction_cfg(provider, 200_000, "signed-empty-thinking");
    let event_ui = Arc::new(EventUi(std::sync::Mutex::new(Vec::new())));
    let ui: Arc<dyn Ui> = event_ui.clone();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("think"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    // The round's context-size event and nothing else: the empty signed block
    // is semantic history, never a display item.
    assert_eq!(
        *event_ui.0.lock().unwrap(),
        vec![Event::Usage(crate::agent::context_estimate(&cfg, &history))]
    );
    assert_eq!(
        history.messages()[1],
        mock_assistant(
            vec![ContentBlock::Thinking {
                thinking: String::new(),
                signature: "signed".into(),
            }],
            "mock",
        )
    );
}

/// The context gauge is read while a turn is still running (an agentic turn
/// lasts minutes), so every round publishes the size — the front-ends' own
/// post-turn event is only the closing bracket. A sub-agent samples into its
/// own History, so its rounds must stay silent.
#[tokio::test]
async fn context_size_is_published_each_round_and_only_at_depth_zero() {
    struct UsageUi(std::sync::Mutex<Vec<u64>>);
    impl Ui for UsageUi {
        fn emit(&self, event: &Event) {
            if let Event::Usage(used) = event {
                self.0.lock().unwrap().push(*used);
            }
        }
    }

    // One tool round then an answer, each with a provider-reported total that
    // anchors the estimate exactly.
    async fn published_sizes(depth: u8, tag: &str) -> Vec<u64> {
        let provider = Provider::mock_scripted(vec![
            MockTurn::Response {
                blocks: vec![tool_use("t1", "echo hi")],
                outcome: AssistantOutcome::ToolUse,
                usage: usage(1_000),
            },
            MockTurn::Response {
                blocks: text("done"),
                outcome: AssistantOutcome::EndTurn,
                usage: usage(2_000),
            },
        ]);
        let cfg = compaction_cfg(provider, 200_000, tag);
        let usage_ui = Arc::new(UsageUi(std::sync::Mutex::new(Vec::new())));
        let ui: Arc<dyn Ui> = usage_ui.clone();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), depth).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        usage_ui.0.lock().unwrap().clone()
    }

    assert_eq!(
        published_sizes(0, "usage-per-round").await,
        vec![usage(1_000).total(), usage(2_000).total()]
    );
    assert_eq!(
        published_sizes(1, "usage-subagent").await,
        Vec::<u64>::new()
    );
}

/// Transient provider errors are retried in place and the turn still completes.
#[tokio::test]
async fn retry_recovers_from_transient_errors() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Error("blip 1".into()),
        MockTurn::Error("blip 2".into()),
        MockTurn::Blocks(text("made it")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "retry");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "made it");
}

/// The gap that used to end a turn on a transient upstream error: reasoning
/// counts as semantic output, but unsigned reasoning is not replayable, so a
/// stream that dies after thinking and before any text left nothing to continue
/// from and nothing to repeat — and both the retry loop and the resume path
/// declined it. Under a high reasoning effort that window is most of the
/// request, so a single relayed `server_error` cost the whole turn.
#[tokio::test]
async fn a_stream_that_dies_during_reasoning_is_retried_not_ended() {
    use kloop_provider::MockTurn;

    let thinking = vec![AssistantBlock::Thinking {
        thinking: "weighing the options".into(),
        signature: String::new(),
    }];
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::PartialError(
            thinking,
            "openai-responses stream error (server_error)".into(),
        ),
        MockTurn::Blocks(text("the answer")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "reasoning-stream-death");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "the answer");
    // Retried, not resumed: the second request is the same one over again, so it
    // carries no partial assistant turn and no resume nudge. Nothing the caller
    // saw is duplicated, because nothing replayable ever landed.
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].messages, requests[1].messages);
    assert_eq!(
        history.messages(),
        &[
            Message::user_text("hello"),
            Message::assistant_from_provider(
                vec![ContentBlock::Text {
                    text: "the answer".into(),
                }],
                // Boundary 2, not 3: the retry happens inside sampling, so it
                // adds no history event of its own.
                cfg.provider_route.primary_attempt().provenance(2),
            ),
        ],
        "the discarded reasoning leaves no empty assistant turn behind"
    );
}

/// Once text is visible, a broken stream is closed in place and not retried:
/// retrying would duplicate a partial answer in every event-driven front-end.
#[tokio::test]
async fn partial_stream_seals_the_open_item_then_continues_from_it() {
    use kloop_provider::MockTurn;

    struct EventUi(std::sync::Mutex<Vec<Event>>);
    impl Ui for EventUi {
        fn emit(&self, ev: &Event) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::PartialError(text("half answer"), "stream dropped".into()),
        MockTurn::Blocks(text(" and the rest")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "partial-stream").test_clone();
    cfg.set_test_route_models(&["mock"]);
    let cfg = Arc::new(cfg);
    let event_ui = Arc::new(EventUi(std::sync::Mutex::new(Vec::new())));
    let ui: Arc<dyn Ui> = event_ui.clone();
    let session = cfg
        .offload_dir
        .join(format!("partial-session-{}.jsonl", std::process::id()));
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    // Both halves reach the caller: the model was told to continue where it
    // stopped, so the second round carries only the remainder.
    assert_eq!(outcome.final_text, "half answer and the rest");
    // The invariant this test has always guarded is that visible output is never
    // produced twice — stated directly rather than through a request count. The
    // second request is a *continuation* (the partial rides in its history), on
    // the same attempt: the fallback model must never run once the user has seen
    // output, because that is the shape that duplicates it.
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.model, "mock");
    }
    assert!(
        requests[1]
            .messages
            .iter()
            .any(|m| m.content.iter().any(|b| matches!(
                b,
                ContentBlock::Text { text } if text == "half answer"
            ))),
        "the continuation carries the partial instead of re-requesting it"
    );
    assert_eq!(
        history.messages(),
        &[
            Message::user_text("hello"),
            Message::assistant_from_provider(
                vec![ContentBlock::Text {
                    text: "half answer".into(),
                }],
                cfg.provider_route.primary_attempt().provenance(3),
            ),
            Message::injected(Injected::Harness, super::STREAM_RESUME_MSG),
            Message::assistant_from_provider(
                vec![ContentBlock::Text {
                    text: " and the rest".into(),
                }],
                cfg.provider_route.primary_attempt().provenance(5),
            ),
        ],
        "the sealed partial and its continuation must both be recoverable after restart"
    );
    assert_eq!(
        crate::rollout::load_session_snapshot(&session)
            .unwrap()
            .messages,
        history.messages(),
        "the partial assistant must survive a fresh rollout read"
    );
    // Item lifecycle only: the per-round Usage gauge carries a char-heuristic
    // estimate, and pinning that number here would make this test fail for
    // reasons that have nothing to do with what it checks.
    let events: Vec<Event> = event_ui
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|event| !matches!(event, Event::Usage(_)))
        .cloned()
        .collect();
    assert_eq!(
        events,
        vec![
            Event::ItemStarted {
                id: "msg-0".into(),
                item: Item::AssistantMessage {
                    text: String::new(),
                    status: crate::event::ItemStatus::InProgress,
                },
            },
            Event::ItemDelta {
                id: "msg-0".into(),
                delta: Delta::Text("half answer".into()),
            },
            // The open item is sealed as Failed before anything else happens —
            // the UI never leaves a half-streamed message hanging in progress.
            Event::ItemCompleted {
                id: "msg-0".into(),
                item: Item::AssistantMessage {
                    text: "half answer".into(),
                    status: crate::event::ItemStatus::Failed,
                },
            },
            Event::Note(format!(
                "stream interrupted after partial output; continuing from it (1/{}): {}",
                super::STREAM_RESUME_LIMIT,
                "provider transport error: stream dropped"
            )),
            Event::ItemStarted {
                id: "msg-1".into(),
                item: Item::AssistantMessage {
                    text: String::new(),
                    status: crate::event::ItemStatus::InProgress,
                },
            },
            Event::ItemDelta {
                id: "msg-1".into(),
                delta: Delta::Text(" and the rest".into()),
            },
            Event::ItemCompleted {
                id: "msg-1".into(),
                item: Item::AssistantMessage {
                    text: " and the rest".into(),
                    status: crate::event::ItemStatus::Completed,
                },
            },
        ]
    );
    let _ = std::fs::remove_file(session);
}

#[tokio::test]
async fn semantic_partial_preserves_signed_reasoning_provenance() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::BlocksThenError(
            vec![AssistantBlock::Thinking {
                thinking: "summary".into(),
                signature: "opaque".into(),
            }],
            ProviderFailure::transport("reasoning stream dropped"),
        ),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "answer".into(),
        }]),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "partial-reasoning");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("think"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    // The subject is provenance on the salvaged reasoning block, not how the
    // turn ended: signed thinking survives the interruption and is replayed as a
    // provider-attributed assistant message, which is what makes continuing legal.
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert_eq!(
        history.messages()[1],
        mock_assistant(
            vec![ContentBlock::Thinking {
                thinking: "summary".into(),
                signature: "opaque".into(),
            }],
            "mock",
        )
    );
}

#[tokio::test]
async fn complete_tool_block_seals_retry_without_dispatching_it() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::BlocksThenError(
            vec![tool_use("t1", "echo must-not-run")],
            ProviderFailure::transport("stream dropped after tool block"),
        ),
        MockTurn::Blocks(text("must not retry")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "tool-seals-retry").test_clone();
    cfg.set_test_route_models(&["mock"]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("run a tool"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(&outcome.reason, EndReason::Error(error) if error.contains("stream dropped after tool block"))
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        history.messages(),
        &[Message::user_text("run a tool")],
        "the uncommitted tool block must be dropped, not executed or persisted"
    );
    // Both this and `a_stream_that_dies_during_reasoning_is_retried_not_ended`
    // reach sampling with an empty replayable partial — reasoning and a complete
    // tool call are both dropped on the way there. They must not share a fate:
    // the model committed to an action here, and asking again can commit it to a
    // different one.
}

#[tokio::test]
async fn subagent_internal_delta_seals_then_continues_in_its_own_history() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::PartialError(text("private partial"), "child stream dropped".into()),
        MockTurn::Blocks(text(" and the rest")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "subagent-seals-retry").test_clone();
    cfg.set_test_route_models(&["mock"]);
    cfg.local_agent = cfg.local_agent.child("agent-98".parse().unwrap());
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("child work"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 1).await;

    // A sub-agent resumes the same way the root does, and — the point of this
    // test — its partial stays in its OWN history; the fallback model, named to
    // fail loudly if it ever ran, must not be reached.
    assert_eq!(outcome.reason, EndReason::Completed);
    let requests = seen.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.model, "mock");
    }
    assert_eq!(
        history.messages(),
        &[
            Message::user_text("child work"),
            mock_assistant(
                vec![ContentBlock::Text {
                    text: "private partial".into(),
                }],
                "mock",
            ),
            Message::injected(Injected::Harness, super::STREAM_RESUME_MSG),
            Message::assistant_from_provider(
                vec![ContentBlock::Text {
                    text: " and the rest".into(),
                }],
                ProviderResponseProvenance {
                    route_boundary: 4,
                    ..mock_assistant(Vec::new(), "mock")
                        .provider_provenance
                        .unwrap()
                },
            ),
        ],
        "the child's replay-safe partial and its continuation stay in the child's own history"
    );
}

#[tokio::test]
async fn non_retryable_failure_does_not_retry() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Failure(ProviderFailure::protocol("malformed provider frame")),
        MockTurn::Blocks(text("must not retry")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "terminal-failure").test_clone();
    cfg.set_test_route_models(&["mock"]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    let EndReason::Error(TurnError::ProviderFailure(failure)) = &outcome.reason else {
        panic!("expected typed provider failure, got {:?}", outcome.reason)
    };
    assert_eq!(
        failure.kind(),
        &kloop_provider::ProviderFailureKind::Protocol
    );
    assert!(!failure.after_semantic_output());
    assert!(failure.to_string().contains("malformed provider frame"));
    assert_eq!(seen.lock().unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn retry_after_overrides_local_backoff_without_real_waiting() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Failure(ProviderFailure::http(
            429,
            "rate limited",
            Some(std::time::Duration::from_secs(60)),
        )),
        MockTurn::Blocks(text("recovered")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "retry-after");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));
    let started = tokio::time::Instant::now();

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "recovered");
    assert_eq!(
        tokio::time::Instant::now() - started,
        std::time::Duration::from_secs(60)
    );
    assert_eq!(seen.lock().unwrap().len(), 2);
}

/// Three failures with no fallback exhaust the retry budget and surface
/// the error.
#[tokio::test]
async fn retries_exhausted_error_out() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Error("down 1".into()),
        MockTurn::Error("down 2".into()),
        MockTurn::Error("down 3".into()),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "exhausted");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(&outcome.reason, EndReason::Error(e) if e.contains("down 3")),
        "expected the last error surfaced, got {:?}",
        outcome.reason
    );
}

/// A model that never stops calling tools is cut off at max_rounds, with
/// history left legal (every tool_use answered).
#[tokio::test]
async fn endless_tool_calls_hit_max_rounds() {
    let provider = Provider::mock(vec![
        vec![tool_use("t1", "echo 1")],
        vec![tool_use("t2", "echo 2")],
        vec![tool_use("t3", "echo 3")],
        vec![tool_use("t4", "echo 4")],
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "maxrounds").test_clone();
    cfg.max_rounds = Some(3);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("loop forever"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::MaxRounds);
    assert_eq!(outcome.rounds, 3);
    // 1 user + 3 * (assistant + tool_results): every round paired.
    assert_eq!(history.messages().len(), 7);
}

/// The round cap bounds spend; it does not void what the spend bought. Returning
/// an empty string here is how a capped sub-agent used to hand its parent nothing
/// after burning every round it was given — the parent could only redo the work.
#[tokio::test]
async fn max_rounds_returns_the_text_produced_before_the_cap() {
    let provider = Provider::mock(vec![
        vec![
            AssistantBlock::Text {
                text: "finding one".into(),
            },
            tool_use("t1", "echo 1"),
        ],
        vec![
            AssistantBlock::Text {
                text: "finding two".into(),
            },
            tool_use("t2", "echo 2"),
        ],
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "maxrounds-text").test_clone();
    cfg.max_rounds = Some(2);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("review this"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::MaxRounds);
    assert_eq!(outcome.rounds, 2);
    assert_eq!(outcome.final_text, "finding one\n\nfinding two");
}

/// A transient stream failure mid-response used to end the whole turn: sampling
/// cannot replay a request whose output the user already saw, so it gave up. But
/// the partial is recorded first, and `replayable_partial` has already dropped
/// unsigned reasoning and every tool call, so what is in history is a well-formed
/// assistant turn — continuing from it costs one round instead of the turn.
#[tokio::test]
async fn retryable_stream_failure_after_output_continues_the_turn() {
    let provider = Provider::mock_scripted(vec![
        MockTurn::BlocksThenError(
            vec![AssistantBlock::Text {
                text: "first half".into(),
            }],
            ProviderFailure::transport("connection reset"),
        ),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "second half".into(),
        }]),
    ]);
    let cfg = Arc::new(compaction_cfg(provider, 200_000, "stream-resume").test_clone());
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("write it out"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    // Both halves: the model was told to continue where it stopped, so the
    // second round carries only the remainder.
    assert_eq!(outcome.final_text, "first halfsecond half");
    assert_eq!(outcome.rounds, 2);
}

/// The resume is for transient failures only: a fatal one after partial output
/// still ends the turn, and still hands back what was produced.
#[tokio::test]
async fn fatal_stream_failure_after_output_still_ends_the_turn() {
    let provider = Provider::mock_scripted(vec![MockTurn::BlocksThenError(
        vec![AssistantBlock::Text {
            text: "all I got".into(),
        }],
        ProviderFailure::protocol("malformed frame"),
    )]);
    let cfg = Arc::new(compaction_cfg(provider, 200_000, "stream-fatal").test_clone());
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("write it out"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(outcome.reason, EndReason::Error(_)),
        "{:?}",
        outcome.reason
    );
    assert_eq!(outcome.final_text, "all I got");
}

/// Every exit that happens *after* the turn produced something must hand that
/// something back. History and the UI already have it; `final_text` is what the
/// caller — a parent agent, most of all — actually receives, and an empty string
/// there is indistinguishable from "produced nothing". `MaxRounds` was fixed
/// when it was measured; these are the same shape, found by enumeration.
#[tokio::test]
async fn terminal_provider_failure_still_returns_what_the_turn_produced() {
    let provider = Provider::mock_scripted(vec![
        MockTurn::Blocks(vec![
            AssistantBlock::Text {
                text: "finding one".into(),
            },
            tool_use("t1", "echo 1"),
        ]),
        MockTurn::Failure(ProviderFailure::protocol("malformed frame")),
    ]);
    let cfg = Arc::new(compaction_cfg(provider, 200_000, "terminal-keeps").test_clone());
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(outcome.reason, EndReason::Error(_)),
        "{:?}",
        outcome.reason
    );
    assert!(
        outcome.final_text.contains("finding one"),
        "a terminal failure dropped the turn's work: {:?}",
        outcome.final_text
    );
}

#[tokio::test]
async fn overflow_without_compaction_still_returns_what_the_turn_produced() {
    let provider = Provider::mock_scripted(vec![
        MockTurn::Blocks(vec![
            AssistantBlock::Text {
                text: "finding one".into(),
            },
            tool_use("t1", "echo 1"),
        ]),
        MockTurn::Overflow,
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "overflow-keeps").test_clone();
    // No window: the reactive path is unavailable, so the turn ends here.
    cfg.context_window = None;
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(outcome.reason, EndReason::Error(_)),
        "{:?}",
        outcome.reason
    );
    assert!(
        outcome.final_text.contains("finding one"),
        "an unrecoverable overflow dropped the turn's work: {:?}",
        outcome.final_text
    );
}

#[tokio::test]
async fn failed_reactive_compaction_still_returns_what_the_turn_produced() {
    let provider = Provider::mock_scripted(vec![
        MockTurn::Blocks(vec![
            AssistantBlock::Text {
                text: "finding one".into(),
            },
            tool_use("t1", "echo 1"),
        ]),
        MockTurn::Overflow,
        // The compaction request itself fails for a reason shrinking cannot fix.
        MockTurn::Failure(ProviderFailure::protocol("compaction refused")),
    ]);
    let cfg = Arc::new(compaction_cfg(provider, 200_000, "compact-fail-keeps").test_clone());
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(outcome.reason, EndReason::Error(_)),
        "{:?}",
        outcome.reason
    );
    assert!(
        outcome.final_text.contains("finding one"),
        "a failed reactive compaction dropped the turn's work: {:?}",
        outcome.final_text
    );
}

/// With no configured guardrail, the loop continues beyond the former default
/// of 30 rounds and stops only when the model returns no tool call.
#[tokio::test]
async fn no_round_limit_runs_until_completed() {
    let mut turns = (0..31)
        .map(|i| {
            vec![tool_use_named(
                &format!("t{i}"),
                "read_file",
                json!({"path": format!("missing-{i}")}),
            )]
        })
        .collect::<Vec<_>>();
    turns.push(vec![AssistantBlock::Text {
        text: "finished after a long run".into(),
    }]);
    let provider = Provider::mock(turns);
    let mut cfg = compaction_cfg(provider, 200_000, "unbounded").test_clone();
    cfg.max_rounds = None;
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("keep going until done"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.rounds, 32);
    assert_eq!(outcome.final_text, "finished after a long run");
}

/// A token cancelled before the turn starts aborts before sampling.
#[tokio::test]
async fn pre_cancelled_turn_aborts_immediately() {
    let provider = Provider::mock(vec![vec![AssistantBlock::Text {
        text: "never sampled".into(),
    }]]);
    let cfg = compaction_cfg(provider, 200_000, "precancel");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Aborted);
    assert_eq!(outcome.rounds, 0);
    assert_eq!(history.messages().len(), 1, "nothing recorded after abort");
}

/// A denied tool call becomes an is_error tool_result the model can react
/// to — the turn continues instead of ending.
#[tokio::test]
async fn denied_tool_call_continues_the_turn() {
    use crate::permissions::{
        Approver, ConfirmRequest, Decision, Mode, PermissionRules, Permissions,
    };
    use std::pin::Pin;

    struct DenyAll;
    impl Approver for DenyAll {
        fn confirm(
            &self,
            _: ConfirmRequest,
        ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
            Box::pin(async { Decision::Deny })
        }
    }

    let provider = Provider::mock(vec![
        vec![AssistantBlock::ToolUse {
            id: "t1".into(),
            name: "write_file".into(),
            input: json!({"path": "should-not-exist", "content": "x"}),
        }],
        vec![AssistantBlock::Text {
            text: "understood, taking another approach".into(),
        }],
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "denied").test_clone();
    cfg.permissions = Arc::new(
        Permissions::new(
            Mode::Manual,
            &PermissionRules::default(),
            std::env::temp_dir(),
            Some(Arc::new(DenyAll)),
        )
        .unwrap(),
    );
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("write a file"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "understood, taking another approach");
    assert_eq!(outcome.rounds, 2);
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &history.messages()[2].content[0]
    else {
        panic!("expected a tool result for the denied call");
    };
    assert!(is_error);
    let content = content.as_text();
    assert!(content.contains("declined"), "got: {content}");
    assert!(!std::path::Path::new("should-not-exist").exists());
}

fn hooked_cfg(provider: Provider, defs: Vec<crate::hooks::HookDef>, tag: &str) -> Arc<Config> {
    let mut cfg = compaction_cfg(provider, 200_000, tag).test_clone();
    cfg.session_id = format!("session-{tag}");
    cfg.hooks = Arc::new(crate::hooks::Hooks { defs });
    Arc::new(cfg)
}

fn hook(event: crate::hooks::HookEvent, script: &str) -> crate::hooks::HookDef {
    crate::hooks::HookDef {
        event,
        command: crate::hooks::test_shell_command(script),
        matcher: None,
        timeout_ms: crate::hooks::DEFAULT_TIMEOUT_MS,
    }
}

/// All four hook points fire, in order, around a one-tool-call turn.
#[tokio::test]
async fn four_hook_points_fire_in_order() {
    use crate::hooks::HookEvent;
    let marker = std::env::temp_dir().join(format!("kloop-hook-order-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let mark = |event: &str| format!("echo {event} >> {}", marker.display());
    let provider = Provider::mock(vec![
        vec![tool_use("t1", "echo hi")],
        vec![AssistantBlock::Text {
            text: "done".into(),
        }],
    ]);
    let cfg = hooked_cfg(
        provider,
        vec![
            hook(HookEvent::PreTurn, &mark("pre_turn")),
            hook(HookEvent::PostTurn, &mark("post_turn")),
            hook(HookEvent::PreTool, &mark("pre_tool")),
            hook(HookEvent::PostTool, &mark("post_tool")),
        ],
        "order",
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "pre_turn\npre_tool\npost_tool\npost_turn\n"
    );
    let _ = std::fs::remove_file(&marker);
}

/// A blocking pre_tool hook: the command never runs and the model gets an
/// is_error tool_result carrying the hook's reason — the turn continues.
#[tokio::test]
async fn pre_tool_hook_block_becomes_error_tool_result() {
    use crate::hooks::HookEvent;
    let marker = std::env::temp_dir().join(format!("kloop-hook-block-{}", std::process::id()));
    let _ = std::fs::remove_file(&marker);
    let provider = Provider::mock(vec![
        vec![tool_use("t1", &format!("touch {}", marker.display()))],
        vec![AssistantBlock::Text {
            text: "changing course".into(),
        }],
    ]);
    let cfg = hooked_cfg(
        provider,
        vec![hook(
            HookEvent::PreTool,
            "echo rm-like commands are banned 1>&2; exit 2",
        )],
        "block",
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(
        history.messages()[2].content[0],
        ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "blocked by hook: rm-like commands are banned".into(),
            is_error: true,
        }
    );
    assert!(!marker.exists(), "the blocked command must not have run");
}

/// A blocking pre_turn hook: the turn never starts (nothing sampled,
/// nothing recorded) and the user sees the reason.
#[tokio::test]
async fn pre_turn_hook_block_prevents_the_turn() {
    use crate::hooks::HookEvent;
    let provider = Provider::mock(vec![vec![AssistantBlock::Text {
        text: "never sampled".into(),
    }]]);
    let cfg = hooked_cfg(
        provider,
        vec![hook(HookEvent::PreTurn, "echo out of office 1>&2; exit 2")],
        "preturn-block",
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(
        outcome.reason,
        EndReason::Error("turn blocked by pre_turn hook: out of office".into())
    );
    assert_eq!(outcome.rounds, 0);
    assert_eq!(history.messages().len(), 1, "nothing recorded");
}

/// Why a turn stopped is rollout data, not just a UI event: a front end that
/// only forwards `outcome.reason` leaves the transcript unable to say whether
/// the turn finished, failed, or was blocked before it began. Both exits that
/// never reach the loop are covered here — they are the ones that look, from
/// the transcript alone, like the agent simply stopped answering.
#[tokio::test]
async fn every_exit_records_why_the_turn_stopped() {
    use crate::hooks::HookEvent;

    fn terminals(session: &std::path::Path) -> Vec<serde_json::Value> {
        std::fs::read_to_string(session)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|line| line["type"] == "turn_terminal")
            .collect()
    }

    let dir = std::env::temp_dir().join(format!("kloop-terminals-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);

    // A turn that runs and fails: the terminal carries the provider's reason.
    let refused = dir.join("refused.jsonl");
    let cfg = compaction_cfg(
        Provider::mock_scripted(vec![MockTurn::Response {
            blocks: Vec::new(),
            outcome: AssistantOutcome::Refused,
            usage: usage(3),
        }]),
        200_000,
        "terminal-refused",
    );
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(refused.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("go"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(
        outcome.reason,
        EndReason::Error(TurnError::ProviderOutcome(AssistantOutcome::Refused))
    );
    let lines = terminals(&refused);
    assert_eq!(lines.len(), 1, "exactly one terminal per turn");
    assert_eq!(lines[0]["status"], "error");
    assert_eq!(
        lines[0]["typed_error"]["kind"], "provider_outcome",
        "the typed reason survives, not just its rendered text"
    );

    // A turn blocked before it starts: nothing is sampled, but the transcript
    // still says why.
    let blocked = dir.join("blocked.jsonl");
    let cfg = hooked_cfg(
        Provider::mock(vec![vec![AssistantBlock::Text {
            text: "never sampled".into(),
        }]]),
        vec![hook(HookEvent::PreTurn, "echo out of office 1>&2; exit 2")],
        "terminal-blocked",
    );
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(blocked.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("go"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.rounds, 0);
    let lines = terminals(&blocked);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["status"], "error");
    assert_eq!(
        lines[0]["error"], "turn blocked by pre_turn hook: out of office",
        "the blocked turn names the hook that stopped it"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Allowing hooks' stdout lands in history as user-message context, in
/// its documented shape: pre_turn before sampling, post_tool right after
/// the round's tool results.
#[tokio::test]
async fn hook_stdout_is_injected_as_user_context() {
    use crate::hooks::HookEvent;
    use kloop_protocol::Role;
    let provider = Provider::mock(vec![
        vec![tool_use("t1", "echo hi")],
        vec![AssistantBlock::Text {
            text: "done".into(),
        }],
    ]);
    let cfg = hooked_cfg(
        provider,
        vec![
            hook(HookEvent::PreTurn, "echo repo rule: tests first"),
            hook(HookEvent::PostTool, "echo lint passed"),
        ],
        "inject",
    );
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    let msgs = history.messages();
    // [user, user(pre_turn ctx), assistant(tool_use), user(tool_result),
    //  user(post_tool ctx), assistant(text)]
    assert_eq!(
        msgs[1],
        Message::injected(Injected::Hook, "[pre_turn hook]\nrepo rule: tests first")
    );
    assert_eq!(msgs[2].role, Role::Assistant);
    assert_eq!(
        msgs[4],
        Message::injected(Injected::Hook, "[post_tool hook]\nlint passed")
    );
}

/// Project instructions ride every sampling request as a synthetic first
/// user message — and are never recorded to history.
#[tokio::test]
async fn project_instructions_injected_per_request_not_recorded() {
    use kloop_provider::MockTurn;
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use("t1", "echo hi")]),
        MockTurn::Blocks(text("done")),
    ]);
    let instructions = "<project-instructions>reply in haiku</project-instructions>";
    let mut cfg = compaction_cfg(provider, 200_000, "instructions").test_clone();
    cfg.project_instructions = Some(instructions.into());
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    for request in seen.iter() {
        assert_eq!(
            request.messages[0],
            Message::user_text(instructions),
            "every request must start with the injected instructions"
        );
    }
    // The real history follows the synthetic message untouched…
    assert_eq!(seen[1].messages[1], Message::user_text("go"));
    // …and never absorbs it.
    assert!(
        history
            .messages()
            .iter()
            .all(|m| *m != Message::user_text(instructions))
    );
}

/// Skills end to end over Mock: with a skill configured, every request
/// advertises the `skill` tool and carries the skills catalog in the
/// injected first message (progressive disclosure — only name+description,
/// not the body). When the model triggers it, the tool_result is the
/// expanded body (`$ARGUMENTS` substituted), which the model then acts on.
#[tokio::test]
async fn skill_catalog_injected_and_tool_expands_body_inline() {
    use kloop_provider::MockTurn;
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use_named(
            "s1",
            "skill",
            json!({"name": "greet", "arguments": "Ada"}),
        )]),
        MockTurn::Blocks(text("greeted")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "skills-e2e").test_clone();
    cfg.skills = Arc::new(vec![crate::skills::Skill {
        name: "greet".into(),
        description: "Greet a person by name.".into(),
        body: "Please greet $ARGUMENTS warmly.".into(),
        dir: "/skills/greet".into(),
        ..Default::default()
    }]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("say hi to Ada"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let seen = seen.lock().unwrap();
    // The skill tool is advertised, and the catalog (name+description, not
    // the body) rides the injected first user message.
    assert!(seen[0].tools.iter().any(|t| t.name == "skill"));
    let injected = match &seen[0].messages[0].content[0] {
        ContentBlock::Text { text } => text,
        other => panic!("expected injected text, got {other:?}"),
    };
    assert!(
        injected.contains("- greet: Greet a person by name."),
        "{injected}"
    );
    assert!(
        !injected.contains("greet $ARGUMENTS"),
        "body must not leak: {injected}"
    );
    // The trigger's tool_result is the expanded body — the second request
    // carries it back to the model.
    let expanded = seen[1].messages.iter().flat_map(|m| &m.content).any(|b| {
        matches!(b, ContentBlock::ToolResult { tool_use_id, content, is_error: false }
                if tool_use_id == "s1" && content.as_text() == "Please greet Ada warmly.")
    });
    assert!(
        expanded,
        "expanded body missing from follow-up request: {:?}",
        seen[1].messages
    );
}

/// The builtins (plan 119) reach the model with nothing on disk: a registry of
/// exactly `skills::builtin()` — which is what `--mock` and a skill-less
/// repository both get — still advertises the `skill` tool and carries the
/// catalog, with the body still withheld until the skill is triggered.
#[tokio::test]
async fn builtin_skills_alone_advertise_the_tool_and_catalog() {
    use kloop_provider::MockTurn;
    let (provider, seen) = Provider::mock_recording(vec![MockTurn::Blocks(text("done"))]);
    let mut cfg = compaction_cfg(provider, 200_000, "builtin-skills").test_clone();
    cfg.skills = Arc::new(crate::skills::builtin());
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("review 7fed2427"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let seen = seen.lock().unwrap();
    assert!(seen[0].tools.iter().any(|t| t.name == "skill"));
    let injected = match &seen[0].messages[0].content[0] {
        ContentBlock::Text { text } => text,
        other => panic!("expected injected text, got {other:?}"),
    };
    assert!(injected.contains("- code-review:"), "{injected}");
    assert!(
        !injected.contains("failure scenario"),
        "the body stays out until triggered: {injected}"
    );
}

/// A `context: fork` skill runs as an isolated sub-agent: the model triggers
/// it, the body becomes the sub-agent's task, and only the sub-agent's final
/// result comes back as the skill tool_result — the skill body never enters
/// the delegating (parent) model's context.
#[tokio::test]
async fn fork_skill_runs_as_isolated_subagent() {
    use kloop_provider::MockTurn;
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use_named(
            "sk1",
            "skill",
            json!({"name": "research"}),
        )]),
        MockTurn::Blocks(text("FORKED_RESULT")), // the forked sub-agent's turn
        MockTurn::Blocks(text("done")),          // parent wraps up
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "skills-fork").test_clone();
    cfg.skills = Arc::new(vec![crate::skills::Skill {
        name: "research".into(),
        description: "Research something in isolation.".into(),
        body: "SECRET_FORK_BODY — do the research".into(),
        dir: "/skills/research".into(),
        context: crate::skills::SkillContext::Fork,
        ..Default::default()
    }]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let seen = seen.lock().unwrap();
    // The body reached the sub-agent (its own request carries it)…
    let sub_req = seen
        .iter()
        .find(|r| {
            r.messages.iter().any(|m| {
                matches!(
                    &m.content[..],
                    [ContentBlock::Text { text }] if text.contains("SECRET_FORK_BODY")
                )
            })
        })
        .expect("the fork body must reach the sub-agent");
    // …and skills are depth-0 only, so the sub-agent gets neither the skill
    // tool nor the catalog (no re-triggering itself, no per-sub-agent bloat).
    assert!(
        !sub_req.tools.iter().any(|t| t.name == "skill"),
        "a sub-agent must not carry the skill tool"
    );
    assert!(
        !sub_req
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(
                b,
                ContentBlock::Text { text } if text.contains("skills are available")
            )),
        "a sub-agent must not carry the skills catalog"
    );
    // …but the parent's post-skill request gets only the result, not the body.
    let tr = |b: &ContentBlock| match b {
        ContentBlock::ToolResult {
            tool_use_id,
            content,
            ..
        } if tool_use_id == "sk1" => Some(content.as_text().into_owned()),
        _ => None,
    };
    let parent_post = seen
        .iter()
        .find(|r| {
            r.messages
                .iter()
                .flat_map(|m| &m.content)
                .any(|b| tr(b).is_some())
        })
        .expect("a parent request carries the skill tool_result");
    assert_eq!(
        parent_post
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .find_map(tr)
            .unwrap(),
        "FORKED_RESULT"
    );
    assert!(
        !parent_post
            .messages
            .iter()
            .flat_map(|m| &m.content)
            .any(|b| matches!(
                b,
                ContentBlock::Text { text } if text.contains("SECRET_FORK_BODY")
            ) || tr(b).is_some_and(|c| c.contains("SECRET_FORK_BODY"))),
        "fork must keep the body out of the parent context"
    );
}

/// A fork skill's `allowed-tools` restricts its sub-agent's tool set (plan
/// 28 slice 3): the sub-agent is offered only the listed tools plus the
/// always-on coordination tools, mapped from cc names to kloop's.
#[tokio::test]
async fn fork_skill_allowed_tools_restricts_subagent() {
    use kloop_provider::MockTurn;
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use_named(
            "sk1",
            "skill",
            json!({"name": "search"}),
        )]),
        MockTurn::Blocks(text("found")), // sub-agent
        MockTurn::Blocks(text("done")),  // parent
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "skills-allowed").test_clone();
    cfg.skills = Arc::new(vec![crate::skills::Skill {
        name: "search".into(),
        description: "Search, read-only.".into(),
        body: "SEARCH_BODY do the search".into(),
        dir: "/skills/search".into(),
        context: crate::skills::SkillContext::Fork,
        allowed_tools: Some(vec!["grep".into(), "read_file".into()]),
        ..Default::default()
    }]);
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let seen = seen.lock().unwrap();
    let sub = seen
        .iter()
        .find(|r| {
            r.messages.iter().any(|m| {
                matches!(
                    &m.content[..],
                    [ContentBlock::Text { text }] if text.contains("SEARCH_BODY")
                )
            })
        })
        .expect("the sub-agent request carries the body");
    let names: Vec<&str> = sub.tools.iter().map(|t| t.name.as_str()).collect();
    assert!(
        names.contains(&"grep") && names.contains(&"read_file"),
        "allowed tools offered: {names:?}"
    );
    assert!(
        names.contains(&"send_message"),
        "infra tool kept: {names:?}"
    );
    assert!(!names.contains(&"bash"), "restricted out: {names:?}");
    assert!(!names.contains(&"write_file"), "restricted out: {names:?}");
}

/// Code-mode end to end over Mock: the model emits one `run_program`
/// tool_use whose program reads a file twice internally, then returns a
/// summary. The next request to the model carries exactly one run_program
/// tool_result — the summary — and the file content the program handled
/// never reaches the context.
#[tokio::test]
async fn run_program_returns_only_final_output_to_the_model() {
    use kloop_provider::MockTurn;
    let file = std::env::temp_dir().join(format!("kloop-run-program-e2e-{}", std::process::id()));
    std::fs::write(&file, "PAYLOAD_LINE_XYZ").unwrap();
    let path = file.to_string_lossy().replace('\\', "\\\\");
    let source = format!(
        "const a = await tools.read_file({{ path: \"{path}\" }});\n\
             const b = await tools.read_file({{ path: \"{path}\" }});\n\
             return \"read \" + (a.length + b.length) + \" chars total\";"
    );
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use_named(
            "e1",
            "run_program",
            json!({ "source": source }),
        )]),
        MockTurn::Blocks(text("done")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "run-program-e2e");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    // Second request = the one sent after run_program ran. It must show the
    // program's return value and never the file content read inside it.
    let seen = seen.lock().unwrap();
    let dump = format!("{:?}", seen[1].messages);
    assert!(
        dump.contains("read ") && dump.contains("chars total"),
        "{dump}"
    );
    assert!(
        !dump.contains("PAYLOAD_LINE_XYZ"),
        "an intermediate tool result leaked into the context: {dump}"
    );
    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn run_program_binds_source_generation_to_the_sampling_request() {
    struct RefreshingSource {
        definition: ToolDef,
        generation: std::sync::atomic::AtomicU64,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ToolSource for RefreshingSource {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(vec![self.definition.clone()])
        }

        fn definition_generation(&self, _tool: &str) -> u64 {
            self.generation.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
            (tool == self.definition.name).then(|| {
                (
                    self.definition.clone(),
                    self.generation.load(std::sync::atomic::Ordering::SeqCst),
                )
            })
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = anyhow::Result<SourceOutput>> + Send + 'a>,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(SourceOutput::text("unexpected call".into())) })
        }
    }

    let source = Arc::new(RefreshingSource {
        definition: ToolDef {
            name: "srv__sampled".into(),
            description: "The schema exposed to the sampling request".into(),
            schema: json!({"type": "object"}),
        },
        generation: std::sync::atomic::AtomicU64::new(0),
        calls: std::sync::atomic::AtomicUsize::new(0),
    });
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: vec![tool_use_named(
                "sampled-program",
                "run_program",
                json!({"source": "return await tools.srv__sampled({});"}),
            )],
        },
        MockTurn::Blocks(text("done")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "program-sampling-manifest").test_clone();
    cfg.tool_sources = vec![source.clone()];
    // This test is about run_program's generated manifest, so it opts into the
    // surface that ships it (plan 113 made that surface off by default).
    cfg.surface.program = true;
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let refresh = {
        let source = source.clone();
        tokio::spawn(async move {
            started_rx.await.expect("sampling did not start");
            source
                .generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            release_tx.send(()).expect("sampling request was dropped");
        })
    };
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    refresh.await.unwrap();

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(source.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    let result = history
        .messages()
        .iter()
        .flat_map(|message| &message.content)
        .find(|block| {
            matches!(
                block,
                ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "sampled-program"
            )
        })
        .expect("missing Program tool result");
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        unreachable!()
    };
    assert!(is_error);
    assert!(
        content
            .as_text()
            .contains("source changed after this Program API was generated"),
        "{}",
        content.as_text()
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    let run_program = seen[0]
        .tools
        .iter()
        .find(|tool| tool.name == "run_program")
        .expect("run_program definition missing");
    assert!(run_program.description.contains("srv__sampled"));
}

/// Deferred regime end to end over Mock: the request's tool defs shrink
/// to built-ins + tool_search, the notice rides the injected context
/// message (after the instructions) without entering history, and a
/// searched tool becomes callable while an unsearched one stays locked.
#[tokio::test]
async fn deferred_tools_shrink_defs_inject_notice_and_gate_dispatch() {
    use crate::tools::ToolSource;
    use kloop_provider::MockTurn;

    struct Srv {
        defs: Vec<kloop_protocol::ToolDef>,
    }
    impl ToolSource for Srv {
        fn defs(&self) -> Arc<[kloop_protocol::ToolDef]> {
            Arc::from(self.defs.clone())
        }
        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }
        fn call<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = anyhow::Result<crate::tools::SourceOutput>>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move { Ok(crate::tools::SourceOutput::text(format!("ran {tool}"))) })
        }
    }
    let source: Arc<dyn ToolSource> = Arc::new(Srv {
        defs: vec![kloop_protocol::ToolDef {
            name: "srv__lookup".into(),
            description: "Look things up".into(),
            schema: serde_json::json!({"type": "object"}),
        }],
    });

    let (provider, seen) = Provider::mock_recording(vec![
        // Round 1: one locked direct call + one search — both get results.
        MockTurn::Blocks(vec![
            tool_use_named("t1", "srv__lookup", serde_json::json!({})),
            tool_use_named(
                "t2",
                "tool_search",
                serde_json::json!({"query": "select:srv__lookup"}),
            ),
        ]),
        // Round 2: the unlocked tool now runs.
        MockTurn::Blocks(vec![tool_use_named(
            "t3",
            "srv__lookup",
            serde_json::json!({}),
        )]),
        MockTurn::Blocks(text("done")),
    ]);
    let mut cfg = compaction_cfg(provider, 200_000, "deferred").test_clone();
    cfg.project_instructions = Some("INSTR".into());
    cfg.tool_sources = vec![source];
    cfg.defer_threshold = 0;
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 3);
    for request in seen.iter() {
        // Defs: built-ins + tool_search, never the source tool — stable
        // across rounds even after the unlock.
        let names: Vec<&str> = request.tools.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"tool_search"));
        assert!(!names.contains(&"srv__lookup"));
        // The synthetic first message carries instructions + notice.
        let Some(ContentBlock::Text { text }) = request.messages[0].content.first() else {
            panic!("expected injected text message");
        };
        assert!(text.starts_with("INSTR\n\n<system-reminder>"), "{text}");
        assert!(text.contains("srv__lookup"), "{text}");
    }
    // Round 1 results: locked bounce for t1, definitions for t2.
    let round1 = &history.messages()[2];
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = &round1.content[0]
    else {
        panic!("expected tool_result");
    };
    assert!(is_error);
    assert!(
        content.as_text().contains("call tool_search"),
        "{}",
        content.as_text()
    );
    // Round 2: the same call now reaches the source.
    let round2 = &history.messages()[4];
    assert_eq!(
        round2.content[0],
        ContentBlock::ToolResult {
            tool_use_id: "t3".into(),
            content: "ran srv__lookup".into(),
            is_error: false,
        }
    );
    // The notice never entered history.
    assert!(
        history
            .messages()
            .iter()
            .all(|m| m.content.iter().all(|b| !matches!(
                b,
                ContentBlock::Text { text } if text.contains("<system-reminder>")
            )))
    );
}

/// The injected instructions count toward the overflow prediction even
/// though they are not in history — and the compaction request itself
/// runs on plain history, without the injected message.
#[tokio::test]
async fn instructions_count_toward_predictive_compaction() {
    use kloop_provider::MockTurn;
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(text("summary of everything so far")),
        MockTurn::Blocks(text("final answer")),
    ]);
    // window 30_000, growth 23_192 → threshold ≈ 6_808 tokens. History
    // alone estimates ~6_500; the 8_000-char instructions add ~2_000 and
    // push it over, so compaction must fire before sampling.
    let mut cfg = compaction_cfg(provider, 30_000, "instr-predict").test_clone();
    cfg.project_instructions = Some("r".repeat(8_000));
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("x".repeat(13_000)));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "y".repeat(13_000),
    }]));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "final answer");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2, "compaction request + the real request");
    // Request 0 is the compaction summary: plain history, no injection.
    assert!(
        seen[0]
            .messages
            .iter()
            .all(|m| m.content.iter().all(|b| !matches!(
                b,
                ContentBlock::Text { text } if text.starts_with("rrr")
            )))
    );
    // Request 1 is the real one: instructions first, compacted history after.
    assert_eq!(seen[1].messages[0], Message::user_text("r".repeat(8_000)));
}

/// Thinking blocks are recorded to history verbatim (they must replay on
/// the next request) and their text streams to the UI's thinking channel,
/// never the answer channel.
#[tokio::test]
async fn thinking_blocks_recorded_and_streamed_separately() {
    struct SplitUi {
        thinking: std::sync::Mutex<String>,
        text: std::sync::Mutex<String>,
    }
    impl Ui for SplitUi {
        fn emit(&self, ev: &Event) {
            match ev {
                Event::ItemDelta {
                    delta: Delta::Text(s),
                    ..
                } => self.text.lock().unwrap().push_str(s),
                Event::ItemDelta {
                    delta: Delta::Reasoning(s),
                    ..
                } => self.thinking.lock().unwrap().push_str(s),
                _ => {}
            }
        }
    }

    let blocks = vec![
        AssistantBlock::Thinking {
            thinking: "pondering".into(),
            signature: "sig".into(),
        },
        AssistantBlock::Text {
            text: "answer".into(),
        },
    ];
    let provider = Provider::mock(vec![blocks.clone()]);
    let cfg = compaction_cfg(provider, 200_000, "thinking");
    let split = Arc::new(SplitUi {
        thinking: std::sync::Mutex::new(String::new()),
        text: std::sync::Mutex::new(String::new()),
    });
    let ui: Arc<dyn Ui> = split.clone();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("think about it"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "answer");
    assert_eq!(
        history.messages()[1],
        mock_assistant(
            blocks
                .into_iter()
                .map(AssistantBlock::into_content_block)
                .collect(),
            "mock",
        )
    );
    assert_eq!(*split.thinking.lock().unwrap(), "pondering");
    assert_eq!(*split.text.lock().unwrap(), "answer");
}

/// run_agent spawns a sub-agent that consumes its own turns from the
/// same provider and returns its final text as the tool result.
#[tokio::test]
async fn subagent_roundtrip_returns_final_text() {
    let provider = Provider::mock(vec![
        // main agent round 1: spawn the sub-agent
        vec![AssistantBlock::ToolUse {
            id: "t1".into(),
            name: "run_agent".into(),
            input: json!({"prompt": "sub work"}),
        }],
        // consumed by the sub-agent's own run_turn
        vec![AssistantBlock::Text {
            text: "sub result".into(),
        }],
        // main agent round 2: wrap up
        vec![AssistantBlock::Text {
            text: "done".into(),
        }],
    ]);
    let cfg = compaction_cfg(provider, 200_000, "subagent");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("delegate"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "done");
    assert_eq!(
        history.messages()[2],
        Message::tool_results(vec![ContentBlock::ToolResult {
            tool_use_id: "t1".into(),
            content: "sub result".into(),
            is_error: false,
        }]),
        "the sub-agent's final text is the tool result"
    );
}

/// Steering typed during a round's tool execution is delivered as a framed
/// user message at the NEXT round boundary — present in the next request,
/// absent from the one already in flight, and never interleaved with the
/// tool_result blocks.
#[tokio::test]
async fn steering_delivered_at_next_boundary_not_mid_request() {
    use kloop_provider::MockTurn;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    // Pushes one steer the first time any tool starts — i.e. during round
    // 0's dispatch, after round 0's request already went out.
    struct SteerOnToolUi {
        inbox: Arc<Inbox>,
        fired: AtomicBool,
    }
    impl Ui for SteerOnToolUi {
        fn emit(&self, ev: &Event) {
            if matches!(
                ev,
                Event::ItemStarted {
                    item: Item::ToolCall { .. },
                    ..
                }
            ) && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.inbox
                    .push(InboxItem::Steer("also check the logs".into()));
            }
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use("t1", "echo hi")]),
        MockTurn::Blocks(text("done")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "steer-boundary");
    let ui: Arc<dyn Ui> = Arc::new(SteerOnToolUi {
        inbox: cfg.inbox.clone(),
        fired: AtomicBool::new(false),
    });
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.rounds, 2);
    let steer = crate::inbox::InboxItem::Steer("also check the logs".into()).into_user_message();
    // [user go, assistant tool_use, user tool_results, user steer, assistant done]
    assert_eq!(
        history.messages()[3],
        steer,
        "the steer is a framed user message after the round's tool_results"
    );
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(
        !seen[0].messages.contains(&steer),
        "round 0's in-flight request predates the steer"
    );
    assert!(
        seen[1].messages.contains(&steer),
        "round 1's request carries the steer"
    );
}

#[tokio::test]
async fn peer_message_delivered_at_next_boundary_not_mid_request() {
    use kloop_provider::MockTurn;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct SendOnToolUi {
        sender: crate::agent_mailbox::LocalAgentContext,
        target: kloop_protocol::LocalAgentId,
        publish_ui: Arc<dyn Ui>,
        fired: AtomicBool,
    }
    impl Ui for SendOnToolUi {
        fn emit(&self, event: &Event) {
            if matches!(
                event,
                Event::ItemStarted {
                    item: Item::ToolCall { .. },
                    ..
                }
            ) && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.sender
                    .send(
                        self.target.clone(),
                        "review logs".into(),
                        "also check the peer logs".into(),
                        &self.publish_ui,
                    )
                    .unwrap();
            }
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![tool_use("t1", "echo hi")]),
        MockTurn::Blocks(text("done")),
    ]);
    let root = compaction_cfg(provider, 200_000, "peer-boundary");
    let target: kloop_protocol::LocalAgentId = "agent-77".parse().unwrap();
    let sub = Arc::new(root.subagent_from(&root.effective_workspace(), None, target.clone()));
    let publish_ui: Arc<dyn Ui> = Arc::new(NullUi);
    let _lease = sub
        .local_agent
        .register_child(
            Arc::clone(&sub.inbox),
            None,
            "peer boundary child",
            publish_ui.clone(),
        )
        .unwrap();
    let ui: Arc<dyn Ui> = Arc::new(SendOnToolUi {
        sender: root.local_agent.clone(),
        target,
        publish_ui,
        fired: AtomicBool::new(false),
    });
    let mut history = History::new(sub.offload_dir.clone());
    history.record(Message::user_text("child work"));

    let outcome = run_turn(&sub, &mut history, &ui, &CancellationToken::new(), 1).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.rounds, 2);
    let peer_index = history
        .messages()
        .iter()
        .position(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::Text { text } if text.contains("[message-1 from main] review logs"))
            })
        })
        .expect("peer message was recorded");
    assert_eq!(peer_index, 3, "peer message follows the paired tool result");
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert!(!seen[0].messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("message-1")))
    }));
    assert!(seen[1].messages.iter().any(|message| {
        message
            .content
            .iter()
            .any(|block| matches!(block, ContentBlock::Text { text } if text.contains("message-1")))
    }));
}

#[tokio::test]
async fn late_peer_message_keeps_main_turn_going() {
    use kloop_provider::MockTurn;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct SendOnTextUi {
        sender: crate::agent_mailbox::LocalAgentContext,
        target: kloop_protocol::LocalAgentId,
        publish_ui: Arc<dyn Ui>,
        fired: AtomicBool,
    }
    impl Ui for SendOnTextUi {
        fn emit(&self, event: &Event) {
            if matches!(
                event,
                Event::ItemDelta {
                    delta: Delta::Text(_),
                    ..
                }
            ) && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.sender
                    .send(
                        self.target.clone(),
                        "late review".into(),
                        "review this before finishing".into(),
                        &self.publish_ui,
                    )
                    .unwrap();
            }
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(text("first answer")),
        MockTurn::Blocks(text("handled late peer")),
    ]);
    let root = compaction_cfg(provider, 200_000, "peer-late");
    let sender_id: kloop_protocol::LocalAgentId = "agent-78".parse().unwrap();
    let sender = root.local_agent.child(sender_id);
    let sender_inbox = Arc::new(Inbox::default());
    let publish_ui: Arc<dyn Ui> = Arc::new(NullUi);
    let _lease = sender
        .register_child(sender_inbox, None, "late peer sender", publish_ui.clone())
        .unwrap();
    let ui: Arc<dyn Ui> = Arc::new(SendOnTextUi {
        sender,
        target: kloop_protocol::LocalAgentId::Main,
        publish_ui,
        fired: AtomicBool::new(false),
    });
    let mut history = History::new(root.offload_dir.clone());
    history.record(Message::user_text("main work"));

    let outcome = run_turn(&root, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "handled late peer");
    assert_eq!(outcome.rounds, 2);
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(history.messages().iter().any(|message| {
        message.content.iter().any(|block| {
            matches!(block, ContentBlock::Text { text } if text.contains("[message-1 from agent-78] late review"))
        })
    }));
}

/// A steer that lands during the FINAL sampling (a response with no tool
/// calls) is absorbed by the end guard: the turn continues to address it
/// instead of dropping it.
#[tokio::test]
async fn late_steering_keeps_the_turn_going() {
    use kloop_provider::MockTurn;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    struct SteerOnTextUi {
        inbox: Arc<Inbox>,
        fired: AtomicBool,
    }
    impl Ui for SteerOnTextUi {
        fn emit(&self, ev: &Event) {
            if matches!(
                ev,
                Event::ItemDelta {
                    delta: Delta::Text(_),
                    ..
                }
            ) && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.inbox.push(InboxItem::Steer("wait, also do Y".into()));
            }
        }
    }

    let provider = Provider::mock_scripted(vec![
        MockTurn::Blocks(text("first attempt")),
        MockTurn::Blocks(text("addressed the steer")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "steer-late");
    let ui: Arc<dyn Ui> = Arc::new(SteerOnTextUi {
        inbox: cfg.inbox.clone(),
        fired: AtomicBool::new(false),
    });
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("start"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "addressed the steer");
    assert_eq!(
        outcome.rounds, 2,
        "the late steer prevented ending at round 1"
    );
    let steer = crate::inbox::InboxItem::Steer("wait, also do Y".into()).into_user_message();
    assert!(history.messages().contains(&steer));
    assert!(cfg.inbox.is_empty(), "the queue was drained");
}

/// A steer admitted into a turn's window during its final sampling is the
/// turn's to answer; once the turn has taken its last look at the queue, the
/// same turn's window is shut, so a steer naming it is refused instead of
/// being acknowledged and left for a later turn.
#[tokio::test]
async fn the_turn_that_admits_a_steer_answers_it_and_then_closes_its_window() {
    use crate::inbox::SteerRefused;
    use kloop_provider::MockTurn;
    use std::sync::Mutex;

    struct SteerForTurnUi {
        inbox: Arc<Inbox>,
        admitted: Mutex<Option<Result<Option<u64>, SteerRefused>>>,
    }
    impl Ui for SteerForTurnUi {
        fn emit(&self, ev: &Event) {
            let mut admitted = self.admitted.lock().unwrap();
            if matches!(
                ev,
                Event::ItemDelta {
                    delta: Delta::Text(_),
                    ..
                }
            ) && admitted.is_none()
            {
                *admitted = Some(self.inbox.push_steer("wait, also do Y".into(), Some(1)));
            }
        }
    }

    let provider = Provider::mock_scripted(vec![
        MockTurn::Blocks(text("first attempt")),
        MockTurn::Blocks(text("addressed the steer")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "steer-window");
    cfg.inbox.open_steer_window(1);
    let recorder = Arc::new(SteerForTurnUi {
        inbox: cfg.inbox.clone(),
        admitted: Mutex::new(None),
    });
    let ui: Arc<dyn Ui> = recorder.clone();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("start"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert_eq!(*recorder.admitted.lock().unwrap(), Some(Ok(Some(1))));
    assert_eq!(outcome.final_text, "addressed the steer");
    assert_eq!(outcome.rounds, 2);
    assert_eq!(
        cfg.inbox.push_steer("too late".into(), Some(1)),
        Err(SteerRefused::NoActiveTurn)
    );
}

/// A running sub-agent must not drain the PARENT's steering queue: each
/// agent gets its own inbox (run_agent resets it on the cloned Config).
/// A steer pushed to the parent while the sub-agent works is invisible to
/// the sub-agent and delivered to the parent at its own next boundary.
#[tokio::test]
async fn subagent_does_not_drain_parent_steering() {
    use kloop_provider::MockRequest;
    use kloop_provider::MockTurn;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;

    // Pushes a parent steer the first time a SUB-agent (agent != "") starts
    // a tool — i.e. while the sub-agent is mid-turn.
    struct SteerParentUi {
        inbox: Arc<Inbox>,
        fired: AtomicBool,
    }
    impl Ui for SteerParentUi {
        fn emit(&self, ev: &Event) {
            if let Event::ItemStarted {
                item: Item::ToolCall { agent, .. },
                ..
            } = ev
                && !agent.is_empty()
                && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.inbox.push(InboxItem::Steer("parent steer".into()));
            }
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        // parent round 0: spawn a sub-agent
        MockTurn::Blocks(vec![tool_use_named(
            "t1",
            "run_agent",
            json!({"prompt": "sub work"}),
        )]),
        // sub round 0: run a tool (fires the parent steer mid-sub-turn)
        MockTurn::Blocks(vec![tool_use("s1", "echo hi")]),
        // sub round 1: finish
        MockTurn::Blocks(text("sub done")),
        // parent round 1: finish
        MockTurn::Blocks(text("done")),
    ]);
    let cfg = compaction_cfg(provider, 200_000, "steer-isolation");
    let ui: Arc<dyn Ui> = Arc::new(SteerParentUi {
        inbox: cfg.inbox.clone(),
        fired: AtomicBool::new(false),
    });
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("go"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let has_steer = |req: &MockRequest| {
        req.messages.iter().any(|m| {
            m.content.iter().any(
                |b| matches!(b, ContentBlock::Text { text } if text.starts_with(STEERING_PREFIX)),
            )
        })
    };
    let first_text = |req: &MockRequest| match req.messages[0].content.first() {
        Some(ContentBlock::Text { text }) => text.clone(),
        _ => String::new(),
    };
    let seen = seen.lock().unwrap();
    for req in seen.iter() {
        if first_text(req) == "sub work" {
            assert!(
                !has_steer(req),
                "the sub-agent must never see the parent's steer"
            );
        }
    }
    assert!(
        seen.iter()
            .any(|req| first_text(req) == "go" && has_steer(req)),
        "the parent delivers its own steer at its next boundary"
    );
}

/// A turn the user takes back before the model has produced anything.
///
/// The five tests below draw one line: "the model produced nothing" is the only
/// thing that makes a turn retractable, and it is measured on the stream, not
/// on what survives into history.
mod a_turn_that_never_happened {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    /// Cancels the turn the first time the model streams anything at all. The
    /// round has then produced output the user saw, whether or not any of it is
    /// replayable.
    struct CancelOnFirstDelta {
        cancel: CancellationToken,
        fired: AtomicBool,
    }
    impl Ui for CancelOnFirstDelta {
        fn emit(&self, event: &Event) {
            if matches!(event, Event::ItemDelta { .. }) && !self.fired.swap(true, Ordering::SeqCst)
            {
                self.cancel.cancel();
            }
        }
    }

    fn session_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// [`mock_assistant`] with the origin boundary spelled out: these turns
    /// write to a real session file, so the boundary counts its lines.
    fn assistant_at(boundary: u64, content: Vec<ContentBlock>) -> Message {
        Message::assistant_from_provider(
            content,
            ProviderResponseProvenance {
                route_revision: 1,
                route_boundary: boundary,
                provider_id: "test".into(),
                api_family: ProviderApiFamily::Mock,
                endpoint_fingerprint: Provider::mock(Vec::new()).endpoint_fingerprint(),
                model: "mock".into(),
            },
        )
    }

    fn line_kinds(session: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(session)
            .unwrap_or_default()
            .lines()
            .map(|line| {
                serde_json::from_str::<serde_json::Value>(line).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    /// The subject: interrupted before the first token, the turn is in neither
    /// history nor the session file, and the input comes back to be retyped.
    #[tokio::test]
    async fn an_interrupt_before_the_first_token_leaves_no_trace() {
        let dir = session_dir("vanish");
        let session = dir.join("session.jsonl");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (provider, seen) = Provider::mock_recording(vec![MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: text("too late"),
        }]);
        let cfg = compaction_cfg(provider, 200_000, "vanish");
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let session_path = session.clone();
        let handle = tokio::spawn(async move {
            let ui: Arc<dyn Ui> = Arc::new(NullUi);
            let mut history = History::new(cfg.offload_dir.clone());
            history.attach_rollout(crate::rollout::Rollout::new(session_path));
            let (outcome, returned) = run_turn_with_input(
                &cfg,
                &mut history,
                &ui,
                &child_cancel,
                0,
                Message::user_text("teh quick brown fox"),
            )
            .await;
            (outcome, returned, history)
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("sampling did not start")
            .expect("sampling gate dropped");
        cancel.cancel();
        let (outcome, returned, history) = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the turn ignored cancellation")
            .expect("the turn panicked");
        drop(release_tx);

        assert_eq!(outcome.reason, EndReason::Aborted);
        assert_eq!(returned, Some(Message::user_text("teh quick brown fox")));
        assert_eq!(history.messages(), &[]);
        // The request went out — it is the *conversation* that is untouched,
        // not the wire — so the model did see what it was asked.
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(
            seen.lock().unwrap()[0].messages,
            vec![Message::user_text("teh quick brown fox")]
        );
        // Only the opening route line: no message, and no `aborted` terminal
        // for a turn that has nothing to stand for.
        assert_eq!(line_kinds(&session), vec!["provider_route_initial"]);
        let _ = std::fs::remove_dir_all(dir);
    }

    /// One word from the model and the turn is real: both sides of it are kept,
    /// and the terminal line says how it ended.
    #[tokio::test]
    async fn an_interrupt_after_the_model_speaks_keeps_the_turn() {
        let dir = session_dir("spoke");
        let session = dir.join("session.jsonl");
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
        let provider = Provider::mock_scripted(vec![MockTurn::DeltasThenGate {
            release: release_rx,
            deltas: text("half an ans"),
        }]);
        let cfg = compaction_cfg(provider, 200_000, "spoke");
        let cancel = CancellationToken::new();
        let ui: Arc<dyn Ui> = Arc::new(CancelOnFirstDelta {
            cancel: cancel.clone(),
            fired: AtomicBool::new(false),
        });
        let mut history = History::new(cfg.offload_dir.clone());
        history.attach_rollout(crate::rollout::Rollout::new(session.clone()));

        let (outcome, returned) = run_turn_with_input(
            &cfg,
            &mut history,
            &ui,
            &cancel,
            0,
            Message::user_text("a real question"),
        )
        .await;

        assert_eq!(outcome.reason, EndReason::Aborted);
        assert_eq!(returned, None);
        assert_eq!(
            history.messages(),
            &[
                Message::user_text("a real question"),
                assistant_at(
                    3,
                    vec![ContentBlock::Text {
                        text: "half an ans".into()
                    }]
                ),
            ]
        );
        assert_eq!(
            line_kinds(&session),
            vec![
                "provider_route_initial",
                "message",
                "message",
                "turn_terminal"
            ]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// The distinction the whole feature turns on: unsigned reasoning is
    /// dropped on the way into history, so nothing is recorded for the round —
    /// but the user watched it think, and that is not "nothing happened". The
    /// input stays.
    #[tokio::test]
    async fn an_interrupt_after_visible_reasoning_keeps_the_input() {
        let dir = session_dir("pondered");
        let session = dir.join("session.jsonl");
        let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
        let provider = Provider::mock_scripted(vec![MockTurn::DeltasThenGate {
            release: release_rx,
            deltas: vec![AssistantBlock::Thinking {
                thinking: "let me work through this".into(),
                signature: String::new(),
            }],
        }]);
        let cfg = compaction_cfg(provider, 200_000, "pondered");
        let cancel = CancellationToken::new();
        let ui: Arc<dyn Ui> = Arc::new(CancelOnFirstDelta {
            cancel: cancel.clone(),
            fired: AtomicBool::new(false),
        });
        let mut history = History::new(cfg.offload_dir.clone());
        history.attach_rollout(crate::rollout::Rollout::new(session.clone()));

        let (outcome, returned) = run_turn_with_input(
            &cfg,
            &mut history,
            &ui,
            &cancel,
            0,
            Message::user_text("think about it"),
        )
        .await;

        assert_eq!(outcome.reason, EndReason::Aborted);
        assert_eq!(returned, None);
        // The reasoning itself is unreplayable and gone; the question it was
        // answering is not.
        assert_eq!(
            history.messages(),
            &[Message::user_text("think about it")],
            "unsigned reasoning records nothing, but the turn still happened"
        );
        assert_eq!(
            line_kinds(&session),
            vec!["provider_route_initial", "message", "turn_terminal"]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Only an interrupt retracts a turn. A failure is worth keeping: "I asked
    /// this and it broke" is history, and the transcript would otherwise show
    /// an error with nothing in front of it.
    #[tokio::test]
    async fn a_provider_failure_keeps_the_input_it_failed_on() {
        let provider = Provider::mock_scripted(vec![MockTurn::Failure(ProviderFailure::protocol(
            "malformed frame",
        ))]);
        let cfg = compaction_cfg(provider, 200_000, "failed");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());

        let (outcome, returned) = run_turn_with_input(
            &cfg,
            &mut history,
            &ui,
            &CancellationToken::new(),
            0,
            Message::user_text("a question that breaks"),
        )
        .await;

        assert!(matches!(outcome.reason, EndReason::Error(_)));
        assert_eq!(returned, None);
        assert_eq!(
            history.messages(),
            &[Message::user_text("a question that breaks")]
        );
    }

    /// Steering typed during the turn answers the message being steered. Taking
    /// that message back would leave the steer pointing at nothing, so an
    /// interrupt with a queue behind it keeps the turn.
    #[tokio::test]
    async fn an_interrupt_with_steering_queued_keeps_the_input() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let provider = Provider::mock_scripted(vec![MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: text("too late"),
        }]);
        let cfg = compaction_cfg(provider, 200_000, "steered");
        let cancel = CancellationToken::new();
        let child_cancel = cancel.clone();
        let inbox = Arc::clone(&cfg.inbox);
        let handle = tokio::spawn(async move {
            let ui: Arc<dyn Ui> = Arc::new(NullUi);
            let mut history = History::new(cfg.offload_dir.clone());
            let (outcome, returned) = run_turn_with_input(
                &cfg,
                &mut history,
                &ui,
                &child_cancel,
                0,
                Message::user_text("do the thing"),
            )
            .await;
            (outcome, returned, history)
        });
        tokio::time::timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("sampling did not start")
            .expect("sampling gate dropped");
        inbox.push(InboxItem::Steer("actually, the other thing".into()));
        cancel.cancel();
        let (outcome, returned, history) = tokio::time::timeout(Duration::from_secs(2), handle)
            .await
            .expect("the turn ignored cancellation")
            .expect("the turn panicked");
        drop(release_tx);

        assert_eq!(outcome.reason, EndReason::Aborted);
        assert_eq!(returned, None);
        assert_eq!(history.messages(), &[Message::user_text("do the thing")]);
    }
}

/// Plan 200: what a request's messages look like with every text dropped —
/// roles, block kinds and ids. Reduction may change text and nothing else.
fn request_skeleton(messages: &[Message]) -> Vec<(Role, Vec<(&'static str, String)>)> {
    messages
        .iter()
        .map(|message| {
            let blocks = message
                .content
                .iter()
                .map(|block| match block {
                    ContentBlock::Text { .. } => ("text", String::new()),
                    ContentBlock::Thinking { .. } => ("thinking", String::new()),
                    ContentBlock::RedactedThinking { .. } => ("redacted", String::new()),
                    ContentBlock::Image { .. } => ("image", String::new()),
                    ContentBlock::ToolUse { id, .. } => ("tool_use", id.clone()),
                    ContentBlock::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } => ("tool_result", format!("{tool_use_id}/{is_error}")),
                })
                .collect();
            (message.role, blocks)
        })
        .collect()
}

fn sent_result<'a>(messages: &'a [Message], id: &str) -> &'a str {
    messages
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content: ToolResultContent::Text(text),
                ..
            } if tool_use_id == id => Some(text.as_str()),
            _ => None,
        })
        .unwrap()
}

const BIG_OUTPUT: &str = "head -c 5000 /dev/zero | tr '\\0' x";

fn big_output_turn() -> Vec<MockTurn> {
    vec![
        MockTurn::Blocks(vec![tool_use("b1", BIG_OUTPUT)]),
        MockTurn::Blocks(vec![tool_use("b2", "true")]),
        MockTurn::Blocks(vec![tool_use("b3", "true")]),
        MockTurn::Blocks(text("done")),
    ]
}

fn reduction_session(
    tag: &str,
    script: Vec<MockTurn>,
) -> (
    Arc<Config>,
    std::path::PathBuf,
    Arc<std::sync::Mutex<Vec<kloop_provider::MockRequest>>>,
) {
    let (provider, seen) = Provider::mock_recording(script);
    let cfg = crate::tools::testutil::TestConfig::new(tag)
        .provider(provider)
        .max_rounds(Some(10))
        .build();
    let session = cfg.offload_dir.join(format!("{tag}.jsonl"));
    let _ = std::fs::remove_file(&session);
    (cfg, session, seen)
}

/// Plan 200: a stub replaces an old result in what is sent, only once the cache
/// is cold, and never in the history or the session file. Every retry sends
/// the same bytes, and the request keeps its shape.
#[tokio::test]
async fn an_old_result_goes_out_as_a_stub_while_history_keeps_it_whole() {
    let mut script = big_output_turn();
    script.extend([
        MockTurn::Error("blip one".into()),
        MockTurn::Error("blip two".into()),
        MockTurn::Blocks(vec![tool_use("b4", "true")]),
        MockTurn::Blocks(text("done again")),
    ]);
    let (cfg, session, seen) = reduction_session("plan200-in-process", script);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("build it"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    let original = "x".repeat(5000);
    // One warm turn: b1 is old enough by its last request, but that request's
    // cache was a moment old.
    for request in seen.lock().unwrap().iter().skip(1) {
        assert_eq!(sent_result(&request.messages, "b1"), original);
    }
    let items_before = history.messages().to_vec();
    let file_before = std::fs::read(&session).unwrap();

    history.let_request_caches_expire();
    history.record(Message::user_text("again"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(outcome.reason, EndReason::Completed);

    let seen = seen.lock().unwrap();
    let second_turn = &seen[4..];
    let stub = sent_result(&second_turn[0].messages, "b1");
    assert!(
        stub.starts_with("[bash output trimmed from this request: 5000 chars"),
        "{stub}"
    );
    let saved = stub
        .split("saved to ")
        .nth(1)
        .and_then(|rest| rest.split(';').next())
        .unwrap();
    assert_eq!(std::fs::read_to_string(saved).unwrap(), original);
    // The two failed attempts and the one that answered sent the same bytes.
    assert_eq!(second_turn[0].messages, second_turn[1].messages);
    assert_eq!(second_turn[1].messages, second_turn[2].messages);
    // The next round, now warm, repeats the stub exactly.
    assert_eq!(sent_result(&second_turn[3].messages, "b1"), stub);
    // Same messages, blocks and ids as history — only a text changed.
    let sent = &second_turn[0].messages;
    let n = items_before.len() + 1;
    assert_eq!(
        request_skeleton(&sent[sent.len() - n..]),
        request_skeleton(&history.messages()[..n])
    );

    assert_eq!(history.messages()[..items_before.len()], items_before[..]);
    assert_eq!(
        sent_result(history.messages(), "b1"),
        original,
        "history is never reduced"
    );
    let file_after = std::fs::read(&session).unwrap();
    assert_eq!(file_after[..file_before.len()], file_before[..]);
    assert_eq!(
        crate::rollout::resume_session(&session).unwrap().messages,
        history.messages()
    );
    let _ = std::fs::remove_file(session);
}

/// Plan 200: a resumed session judges the cache by how long its file has been
/// quiet — a moment ago is warm, past the TTL is cold and its first request
/// carries new stubs — and sends the stubs its file recorded exactly as they
/// were sent before, without writing their originals again.
#[tokio::test]
async fn a_resumed_session_stubs_only_once_its_cache_can_have_expired() {
    let mut script = big_output_turn();
    script.extend([
        MockTurn::Blocks(text("fresh")),
        MockTurn::Blocks(text("quiet")),
        MockTurn::Blocks(text("again")),
    ]);
    let (cfg, session, seen) = reduction_session("plan200-resume", script);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("build it"));
    run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    drop(history);
    let original = "x".repeat(5000);

    let resume = || {
        History::resume(
            cfg.offload_dir.clone(),
            crate::rollout::resume_session(&session).unwrap(),
        )
    };
    let mut fresh = resume();
    fresh.record(Message::user_text("still there?"));
    run_turn(&cfg, &mut fresh, &ui, &CancellationToken::new(), 0).await;
    drop(fresh);
    assert_eq!(
        sent_result(&seen.lock().unwrap()[4].messages, "b1"),
        original
    );

    let ten_minutes_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(600);
    std::fs::File::options()
        .write(true)
        .open(&session)
        .unwrap()
        .set_modified(ten_minutes_ago)
        .unwrap();
    let mut quiet = resume();
    quiet.record(Message::user_text("back"));
    run_turn(&cfg, &mut quiet, &ui, &CancellationToken::new(), 0).await;
    let stub = sent_result(&seen.lock().unwrap()[5].messages, "b1").to_string();
    assert!(stub.starts_with("[bash output trimmed"), "{stub}");
    assert_eq!(sent_result(quiet.messages(), "b1"), original);
    drop(quiet);

    // Straight back in: the cache still holds that stub, so it goes out again.
    let offload_files = || std::fs::read_dir(&cfg.offload_dir).unwrap().count();
    let files_before = offload_files();
    let mut again = resume();
    again.record(Message::user_text("once more"));
    run_turn(&cfg, &mut again, &ui, &CancellationToken::new(), 0).await;
    assert_eq!(sent_result(&seen.lock().unwrap()[6].messages, "b1"), stub);
    assert_eq!(offload_files(), files_before);
    let _ = std::fs::remove_file(session);
}

/// Plan 200: rewinding onto a branch keeps sending what the cache holds, and
/// writes into the branch's file any stub it would otherwise not know about.
#[tokio::test]
async fn a_rewound_branch_records_the_stubs_it_keeps_sending() {
    let mut script = big_output_turn();
    script.push(MockTurn::Blocks(text("later")));
    let (cfg, session, seen) = reduction_session("plan200-rebase", script);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(
        crate::rollout::Rollout::new_with_initial_route(session.clone(), &cfg.provider_route)
            .unwrap(),
    );
    history.record(Message::user_text("build it"));
    run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    // Branch off after the first turn: b1 is in it, no stub yet.
    let sessions = cfg.offload_dir.join("plan200-rebase-forks");
    let _ = std::fs::remove_dir_all(&sessions);
    let fork = crate::rollout::fork_session(&session, None, &sessions).unwrap();
    history.let_request_caches_expire();
    history.record(Message::user_text("again"));
    run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    let stub = sent_result(&seen.lock().unwrap()[4].messages, "b1").to_string();
    assert!(stub.starts_with("[bash output trimmed"), "{stub}");

    assert!(
        crate::rollout::resume_session(&fork)
            .unwrap()
            .request_stubs
            .is_empty()
    );
    history.rebase(crate::rollout::resume_session(&fork).unwrap());

    let branch = crate::rollout::resume_session(&fork).unwrap();
    assert_eq!(branch.request_stubs.len(), 1);
    assert_eq!(branch.request_stubs[0].stub, stub);
    let _ = std::fs::remove_file(session);
    let _ = std::fs::remove_dir_all(sessions);
}

/// Plan 200: `[context] request_reduction = false` sends history as it is.
#[tokio::test]
async fn reduction_switched_off_sends_every_result_whole() {
    let mut script = big_output_turn();
    script.push(MockTurn::Blocks(text("again")));
    let (provider, seen) = Provider::mock_recording(script);
    let mut cfg = crate::tools::testutil::TestConfig::new("plan200-off")
        .provider(provider)
        .max_rounds(Some(10))
        .build()
        .test_clone();
    cfg.request_reduction = false;
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("build it"));
    run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
    // With reduction on, this request would carry b1 as a stub.
    history.let_request_caches_expire();
    history.record(Message::user_text("again"));
    run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 5);
    assert_eq!(sent_result(&seen[4].messages, "b1"), "x".repeat(5000));
}
