use super::*;
use crate::event::Delta;
use crate::event::Item;
use crate::inbox::InboxItem;
use crate::inbox::STEERING_PREFIX;
use crate::tools::SourceOutput;
use crate::tools::ToolSource;
use kloop_protocol::Role;
use kloop_protocol::ToolDef;
use kloop_provider::MockTurn;
use kloop_provider::Provider;
use kloop_provider::ProviderFailure;
use serde_json::json;

struct NullUi;
impl Ui for NullUi {
    fn emit(&self, _: &Event) {}
}

fn tool_use(id: &str, cmd: &str) -> ContentBlock {
    tool_use_named(id, "bash", json!({"command": cmd}))
}

fn tool_use_named(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
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
    let mut cfg = (*ctx.cfg).clone();
    cfg.provider = Arc::new(provider);
    cfg.max_rounds = Some(10);
    cfg.agent_label = "agent-structured".into();
    (Arc::new(cfg), seen)
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
        MockTurn::Blocks(vec![ContentBlock::Text {
            text: "plain".into(),
        }]),
        MockTurn::Blocks(vec![ContentBlock::Text {
            text: "still plain".into(),
        }]),
        MockTurn::Blocks(vec![ContentBlock::Text {
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
        tool_use_named("s1", "structured_output", json!([1, 2])),
        tool_use_named("o2", "test__blocking_read", json!({"value": "second"})),
    ])]);
    let mut cfg = (*base).clone();
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
    let (cfg, seen) = structured_config(vec![MockTurn::Blocks(vec![ContentBlock::Text {
        text: "plain result".into(),
    }])]);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("ordinary child"));
    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 1).await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "plain result");
    assert!(seen.lock().unwrap()[0]
        .tools
        .iter()
        .all(|tool| tool.name != "structured_output"));
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
        vec![ContentBlock::Text {
            text: "all done".into(),
        }],
    ]);
    let cfg = Arc::new(Config {
        provider: Arc::new(provider),
        model: "mock".into(),
        system: "test".into(),
        project_instructions: None,
        max_rounds: Some(10),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        offload_dir: std::env::temp_dir().join("kloop-test-e2e"),
        sessions_dir: std::env::temp_dir().join("kloop-test-e2e-sessions"),
        context_window: None,
        fallback_model: None,
        permissions: Arc::new(crate::permissions::Permissions::allow_all()),
        questioner: None,
        file_state: Default::default(),
        tool_sources: Vec::new(),
        session_id: String::new(),
        agent_label: String::new(),
        hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
        background_shells: crate::tools::BackgroundShells::new(),
        shell_programs: std::sync::Arc::new(crate::shell_programs::ShellPrograms::test_fixture()),
        powershell_execution_gate: Default::default(),
        sandbox: None,
        agent_types: Arc::new(Vec::new()),
        tool_allowlist: None,
        defer_threshold: 30,
        unlocked_tools: Default::default(),
        todos: Default::default(),
        inbox: Default::default(),
        scheduler: crate::scheduler::Scheduler::in_memory(Default::default()),
        background_tasks: Default::default(),
        program_limits: Default::default(),
        skills: Default::default(),
        active_worktree: std::sync::Arc::new(crate::worktree::ActiveWorktreeState::default()),
        surface: Default::default(),
    });
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
    use crate::hooks::{HookDef, HookEvent, Hooks, DEFAULT_TIMEOUT_MS};
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

    let provider = Provider::mock(vec![vec![ContentBlock::Text {
        text: "sub answer".into(),
    }]]);
    let mut cfg = (*compaction_cfg(provider, 200_000, "subhook")).clone();
    cfg.agent_label = "agent-7".into();
    cfg.session_id = "parent-sess".into();
    cfg.hooks = Arc::new(hooks);
    let cfg = Arc::new(cfg);

    // Give the sub-agent a session file so the stop payload has a transcript.
    let session = dir.join("agent-7.jsonl");
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(crate::rollout::Rollout::new(session.clone()));
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
    assert_eq!(payload["agent"], "agent-7");
    assert_eq!(
        payload["agent_transcript_path"],
        session.to_string_lossy().as_ref()
    );
    assert_eq!(payload["last_assistant_message"], "sub answer");
    let _ = std::fs::remove_dir_all(&dir);
}

fn compaction_cfg(provider: Provider, window: u64, tag: &str) -> Arc<Config> {
    Arc::new(Config {
        provider: Arc::new(provider),
        model: "mock".into(),
        system: "test".into(),
        project_instructions: None,
        max_rounds: Some(10),
        cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
        offload_dir: std::env::temp_dir().join(format!("kloop-test-{tag}")),
        sessions_dir: std::env::temp_dir().join(format!("kloop-test-{tag}-sessions")),
        context_window: Some(window),
        fallback_model: None,
        permissions: Arc::new(crate::permissions::Permissions::allow_all()),
        questioner: None,
        file_state: Default::default(),
        tool_sources: Vec::new(),
        session_id: String::new(),
        agent_label: String::new(),
        hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
        background_shells: crate::tools::BackgroundShells::new(),
        shell_programs: std::sync::Arc::new(crate::shell_programs::ShellPrograms::test_fixture()),
        powershell_execution_gate: Default::default(),
        sandbox: None,
        agent_types: Arc::new(Vec::new()),
        tool_allowlist: None,
        defer_threshold: 30,
        unlocked_tools: Default::default(),
        todos: Default::default(),
        inbox: Default::default(),
        scheduler: crate::scheduler::Scheduler::in_memory(Default::default()),
        background_tasks: Default::default(),
        program_limits: Default::default(),
        skills: Default::default(),
        active_worktree: std::sync::Arc::new(crate::worktree::ActiveWorktreeState::default()),
        surface: Default::default(),
    })
}

/// Predictive: a fat history under a small (but > growth reserve) window
/// triggers compaction BEFORE the sampling request. Mock turn 1 serves
/// the summary, turn 2 the actual reply.
#[tokio::test]
async fn predictive_compaction_fires_before_sampling() {
    let provider = Provider::mock(vec![
        vec![ContentBlock::Text {
            text: "summary of everything so far".into(),
        }],
        vec![ContentBlock::Text {
            text: "final answer".into(),
        }],
    ]);
    // growth = 8192 + 15_000 = 23_192; window 30_000 → threshold ≈ 6_808
    // tokens ≈ 27k chars. Two fat user messages blow past it.
    let cfg = compaction_cfg(provider, 30_000, "predictive");
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("x".repeat(30_000)));
    history.record(Message::assistant(vec![ContentBlock::Text {
        text: "y".repeat(30_000),
    }]));
    history.record(Message::user_text("now answer briefly"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "final answer");
    let msgs = history.messages();
    // [summary, ...kept tail..., assistant reply]; the fat prefix is gone.
    let ContentBlock::Text { text } = &msgs[0].content[0] else {
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

/// Reactive: the first sampling request is rejected as too large; the
/// loop compacts once (mock turn 2 = summary) and retries successfully
/// (turn 3), with no user-visible error.
#[tokio::test]
async fn overflow_compacts_and_retries() {
    use kloop_provider::MockTurn;
    let provider = Provider::mock_scripted(vec![
        MockTurn::Overflow,
        MockTurn::Blocks(vec![ContentBlock::Text {
            text: "summary of everything so far".into(),
        }]),
        MockTurn::Blocks(vec![ContentBlock::Text {
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
    let ContentBlock::Text { text } = &history.messages()[0].content[0] else {
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
        MockTurn::Blocks(vec![ContentBlock::Text {
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

fn text(t: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::Text { text: t.into() }]
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
        Message::user_text(super::TRUNCATION_CONTINUE_MSG),
        "the continuation nudge must be recorded so history stays legal"
    );
}

/// Truncation nudges are bounded: after the limit the turn completes with
/// whatever text arrived instead of looping.
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

    assert_eq!(outcome.reason, EndReason::Completed);
    // 3 nudges (the limit), so the 4th truncated response ends the turn.
    // Every cut-off segment is accumulated into the final deliverable.
    assert_eq!(outcome.final_text, "cut 1cut 2cut 3cut 4");
    assert_eq!(outcome.rounds, 4);
    let nudges = history
        .messages()
        .iter()
        .filter(|m| *m == &Message::user_text(super::TRUNCATION_CONTINUE_MSG))
        .count();
    assert_eq!(nudges, 3);
}

/// After the primary model exhausts its retries, the turn continues on
/// the fallback model instead of surfacing an error.
#[tokio::test]
async fn fallback_model_takes_over_after_retries() {
    use kloop_provider::MockTurn;
    struct NoteUi(std::sync::Mutex<Vec<String>>);
    impl Ui for NoteUi {
        fn emit(&self, ev: &Event) {
            if let Event::Note(s) = ev {
                self.0.lock().unwrap().push(s.to_string());
            }
        }
    }

    let provider = Provider::mock_scripted(vec![
        MockTurn::Error("boom 1".into()),
        MockTurn::Error("boom 2".into()),
        MockTurn::Error("boom 3".into()),
        MockTurn::Blocks(text("answer from fallback")),
    ]);
    let mut cfg = (*compaction_cfg(provider, 200_000, "fallback")).clone();
    cfg.fallback_model = Some("mock-fallback".into());
    let cfg = Arc::new(cfg);
    let note_ui = Arc::new(NoteUi(std::sync::Mutex::new(Vec::new())));
    let ui: Arc<dyn Ui> = note_ui.clone();
    let cancel = CancellationToken::new();
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.final_text, "answer from fallback");
    let notes = note_ui.0.lock().unwrap();
    assert!(
        notes
            .iter()
            .any(|n| n.contains("switching to fallback model mock-fallback")),
        "expected a fallback-switch note, got {notes:?}"
    );
}

/// Transient provider errors are retried in place; the turn still
/// completes without any fallback configured.
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

/// Once text is visible, a broken stream is closed in place and not retried:
/// retrying would duplicate a partial answer in every event-driven front-end.
#[tokio::test]
async fn partial_stream_error_completes_open_item_without_retry() {
    use kloop_provider::MockTurn;

    struct EventUi(std::sync::Mutex<Vec<Event>>);
    impl Ui for EventUi {
        fn emit(&self, ev: &Event) {
            self.0.lock().unwrap().push(ev.clone());
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::PartialError(text("half answer"), "stream dropped".into()),
        MockTurn::Blocks(text("must not retry")),
    ]);
    let mut cfg = (*compaction_cfg(provider, 200_000, "partial-stream")).clone();
    cfg.fallback_model = Some("must-not-run-after-visible-output".into());
    let cfg = Arc::new(cfg);
    let event_ui = Arc::new(EventUi(std::sync::Mutex::new(Vec::new())));
    let ui: Arc<dyn Ui> = event_ui.clone();
    let session = cfg
        .offload_dir
        .join(format!("partial-session-{}.jsonl", std::process::id()));
    let mut history = History::new(cfg.offload_dir.clone());
    history.attach_rollout(crate::rollout::Rollout::new(session.clone()));
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(&outcome.reason, EndReason::Error(e) if e.contains("stream dropped")),
        "expected the visible stream error, got {:?}",
        outcome.reason
    );
    assert_eq!(
        seen.lock().unwrap().len(),
        1,
        "visible output must not be retried"
    );
    assert_eq!(
        history.messages(),
        &[
            Message::user_text("hello"),
            Message::assistant(vec![ContentBlock::Text {
                text: "half answer".into(),
            }]),
        ],
        "the completed UI item must be recoverable after restart"
    );
    assert_eq!(
        crate::rollout::load_session_snapshot(&session)
            .unwrap()
            .messages,
        history.messages(),
        "the partial assistant must survive a fresh rollout read"
    );
    assert_eq!(
        *event_ui.0.lock().unwrap(),
        vec![
            Event::ItemStarted {
                id: "msg-0".into(),
                item: Item::AssistantMessage {
                    text: String::new(),
                },
            },
            Event::ItemDelta {
                id: "msg-0".into(),
                delta: Delta::Text("half answer".into()),
            },
            Event::ItemCompleted {
                id: "msg-0".into(),
                item: Item::AssistantMessage {
                    text: "half answer".into(),
                },
            },
        ]
    );
    let _ = std::fs::remove_file(session);
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "tool-seals-retry")).clone();
    cfg.fallback_model = Some("must-not-fallback".into());
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
}

#[tokio::test]
async fn subagent_internal_delta_also_seals_retry() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::PartialError(text("private partial"), "child stream dropped".into()),
        MockTurn::Blocks(text("must not retry")),
    ]);
    let mut cfg = (*compaction_cfg(provider, 200_000, "subagent-seals-retry")).clone();
    cfg.fallback_model = Some("must-not-fallback".into());
    cfg.agent_label = "agent-test".into();
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("child work"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 1).await;

    assert!(
        matches!(&outcome.reason, EndReason::Error(error) if error.contains("child stream dropped"))
    );
    assert_eq!(seen.lock().unwrap().len(), 1);
    assert_eq!(
        history.messages(),
        &[Message::user_text("child work")],
        "an internal partial delta blocks replay but remains invisible to parent history"
    );
}

#[tokio::test]
async fn non_retryable_failure_does_not_retry_or_fallback() {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Failure(ProviderFailure::protocol("malformed provider frame")),
        MockTurn::Blocks(text("must not retry")),
    ]);
    let mut cfg = (*compaction_cfg(provider, 200_000, "terminal-failure")).clone();
    cfg.fallback_model = Some("must-not-fallback".into());
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(NullUi);
    let mut history = History::new(cfg.offload_dir.clone());
    history.record(Message::user_text("hello"));

    let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

    assert!(
        matches!(&outcome.reason, EndReason::Error(error) if error.contains("malformed provider frame"))
    );
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
async fn retries_exhausted_without_fallback_error_out() {
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "maxrounds")).clone();
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
    turns.push(vec![ContentBlock::Text {
        text: "finished after a long run".into(),
    }]);
    let provider = Provider::mock(turns);
    let mut cfg = (*compaction_cfg(provider, 200_000, "unbounded")).clone();
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
    let provider = Provider::mock(vec![vec![ContentBlock::Text {
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
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "write_file".into(),
            input: json!({"path": "should-not-exist", "content": "x"}),
        }],
        vec![ContentBlock::Text {
            text: "understood, taking another approach".into(),
        }],
    ]);
    let mut cfg = (*compaction_cfg(provider, 200_000, "denied")).clone();
    cfg.permissions = Arc::new(
        Permissions::new(
            Mode::Manual,
            &PermissionRules::default(),
            std::env::temp_dir(),
            Some(Arc::new(DenyAll)),
            None,
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
    let mut cfg = (*compaction_cfg(provider, 200_000, tag)).clone();
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
        vec![ContentBlock::Text {
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
        vec![ContentBlock::Text {
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
    let provider = Provider::mock(vec![vec![ContentBlock::Text {
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

/// Allowing hooks' stdout lands in history as user-message context, in
/// its documented shape: pre_turn before sampling, post_tool right after
/// the round's tool results.
#[tokio::test]
async fn hook_stdout_is_injected_as_user_context() {
    use crate::hooks::HookEvent;
    use kloop_protocol::Role;
    let provider = Provider::mock(vec![
        vec![tool_use("t1", "echo hi")],
        vec![ContentBlock::Text {
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
        Message::user_text("[pre_turn hook]\nrepo rule: tests first")
    );
    assert_eq!(msgs[2].role, Role::Assistant);
    assert_eq!(msgs[4], Message::user_text("[post_tool hook]\nlint passed"));
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "instructions")).clone();
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
    assert!(history
        .messages()
        .iter()
        .all(|m| *m != Message::user_text(instructions)));
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "skills-e2e")).clone();
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "skills-fork")).clone();
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
/// always-on read_offloaded, mapped from cc names to kloop's.
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "skills-allowed")).clone();
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
        names.contains(&"read_offloaded"),
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
    let mut cfg = (*compaction_cfg(provider, 200_000, "deferred")).clone();
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
    assert!(history
        .messages()
        .iter()
        .all(|m| m.content.iter().all(|b| !matches!(
            b,
            ContentBlock::Text { text } if text.contains("<system-reminder>")
        ))));
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
    let mut cfg = (*compaction_cfg(provider, 30_000, "instr-predict")).clone();
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
    assert!(seen[0]
        .messages
        .iter()
        .all(|m| m.content.iter().all(|b| !matches!(
            b,
            ContentBlock::Text { text } if text.starts_with("rrr")
        ))));
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
        ContentBlock::Thinking {
            thinking: "pondering".into(),
            signature: "sig".into(),
        },
        ContentBlock::Text {
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
    assert_eq!(history.messages()[1], Message::assistant(blocks));
    assert_eq!(*split.thinking.lock().unwrap(), "pondering");
    assert_eq!(*split.text.lock().unwrap(), "answer");
}

/// The task tool spawns a sub-agent that consumes its own turns from the
/// same provider and returns its final text as the tool result.
#[tokio::test]
async fn subagent_roundtrip_returns_final_text() {
    let provider = Provider::mock(vec![
        // main agent round 1: spawn the sub-agent
        vec![ContentBlock::ToolUse {
            id: "t1".into(),
            name: "task".into(),
            input: json!({"prompt": "sub work"}),
        }],
        // consumed by the sub-agent's own run_turn
        vec![ContentBlock::Text {
            text: "sub result".into(),
        }],
        // main agent round 2: wrap up
        vec![ContentBlock::Text {
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
    let steer = Message::user_text(format!("{STEERING_PREFIX}\nalso check the logs"));
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
    let steer = Message::user_text(format!("{STEERING_PREFIX}\nwait, also do Y"));
    assert!(history.messages().contains(&steer));
    assert!(cfg.inbox.is_empty(), "the queue was drained");
}

/// A running sub-agent must not drain the PARENT's steering queue: each
/// agent gets its own inbox (the task tool resets it on the cloned Config).
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
            {
                if !agent.is_empty() && !self.fired.swap(true, Ordering::SeqCst) {
                    self.inbox.push(InboxItem::Steer("parent steer".into()));
                }
            }
        }
    }

    let (provider, seen) = Provider::mock_recording(vec![
        // parent round 0: spawn a sub-agent
        MockTurn::Blocks(vec![tool_use_named(
            "t1",
            "task",
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
