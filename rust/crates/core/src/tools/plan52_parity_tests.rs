use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kloop_protocol::AssistantBlock;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_provider::MockTurn;
use kloop_provider::Provider;
use serde_json::Value;
use serde_json::json;
use tokio::sync::oneshot;
use tokio::sync::watch;

use super::ToolCtx;
use super::all_tool_defs;
use super::dispatch_tools;
use super::testutil::run_tool;
use super::testutil::test_ctx;
use super::testutil::with_provider;
use crate::agent::Ui;
use crate::agent::run_turn;
use crate::event::BackgroundTaskKind;
use crate::event::BackgroundTaskStatus;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use crate::history::History;
use crate::inbox::Inbox;
use crate::inbox::InboxItem;

const WATCHDOG: Duration = Duration::from_secs(3);

struct RecordingUi {
    events: Mutex<Vec<Event>>,
    activity: watch::Sender<u64>,
}

impl Default for RecordingUi {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            events: Mutex::new(Vec::new()),
            activity,
        }
    }
}

impl RecordingUi {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }

    fn take_events(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }

    async fn wait_for_background(&self, expected: BackgroundTaskStatus) {
        let mut activity = self.activity.subscribe();
        loop {
            if self.events.lock().unwrap().iter().any(|event| {
                matches!(
                    event,
                    Event::BackgroundTaskUpdated(task)
                        if task.kind == BackgroundTaskKind::Agent && task.status == expected
                )
            }) {
                return;
            }
            tokio::time::timeout(WATCHDOG, activity.changed())
                .await
                .expect("background lifecycle event timed out")
                .expect("recording UI activity channel closed");
        }
    }
}

impl Ui for RecordingUi {
    fn emit(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
        let next = (*self.activity.borrow()).wrapping_add(1);
        self.activity.send_replace(next);
    }
}

struct BoundaryUi {
    inbox: Arc<Inbox>,
    injected: AtomicBool,
}

impl BoundaryUi {
    fn new(inbox: Arc<Inbox>) -> Self {
        Self {
            inbox,
            injected: AtomicBool::new(false),
        }
    }
}

impl Ui for BoundaryUi {
    fn emit(&self, event: &Event) {
        if matches!(event, Event::ItemDelta { .. }) && !self.injected.swap(true, Ordering::SeqCst) {
            self.inbox.push(InboxItem::SubAgentResult {
                label: "agent-boundary".into(),
                summary: "BOUNDARY-MARKER-52".into(),
            });
        }
    }
}

fn text_blocks(text: &str) -> Vec<AssistantBlock> {
    vec![AssistantBlock::Text { text: text.into() }]
}

fn gate_turn(text: &str) -> (MockTurn, oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (started_tx, started_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    (
        MockTurn::Gate {
            started: started_tx,
            release: release_rx,
            blocks: text_blocks(text),
        },
        started_rx,
        release_tx,
    )
}

fn ctx_with_provider(provider: Provider, ui: Arc<RecordingUi>, tag: &str) -> ToolCtx {
    let mut ctx = with_provider(test_ctx(0, tag), provider);
    ctx.ui = ui;
    ctx
}

fn result_report(results: &[ContentBlock]) -> Vec<Value> {
    results
        .iter()
        .map(|result| {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = result
            else {
                panic!("expected tool result")
            };
            json!({
                "tool_use_id": tool_use_id,
                "content": content.as_text(),
                "is_error": is_error,
            })
        })
        .collect()
}

fn started_agent_id(output: &str) -> String {
    output
        .lines()
        .find_map(|line| line.strip_prefix("Agent ID: "))
        .expect("background task result must contain its agent id")
        .to_string()
}

fn background_status(status: BackgroundTaskStatus) -> &'static str {
    match status {
        BackgroundTaskStatus::Running => "running",
        BackgroundTaskStatus::Completed => "completed",
        BackgroundTaskStatus::Failed => "failed",
        BackgroundTaskStatus::Cancelled => "cancelled",
    }
}

fn lifecycle_counts(events: &[Event]) -> Value {
    let mut counts: HashMap<&'static str, usize> = HashMap::new();
    for event in events {
        let key = match event {
            Event::ItemStarted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == "run_agent" => Some("tool_started"),
            Event::ItemCompleted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == "run_agent" => Some("tool_completed"),
            Event::ItemStarted {
                item: Item::SubAgent { status, .. },
                ..
            } if *status == ItemStatus::InProgress => Some("agent_started"),
            Event::ItemCompleted {
                item: Item::SubAgent { .. },
                ..
            } => Some("agent_completed"),
            _ => None,
        };
        if let Some(key) = key {
            *counts.entry(key).or_default() += 1;
        }
    }
    json!({
        "tool_started": counts.get("tool_started").copied().unwrap_or(0),
        "tool_completed": counts.get("tool_completed").copied().unwrap_or(0),
        "agent_started": counts.get("agent_started").copied().unwrap_or(0),
        "agent_completed": counts.get("agent_completed").copied().unwrap_or(0),
    })
}

fn projected_background_events(events: &[Event], agent_id: &str) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::BackgroundTaskUpdated(task) if task.kind == BackgroundTaskKind::Agent => {
                assert_eq!(task.id, agent_id);
                Some(json!({
                    "id": "<AGENT>",
                    "kind": "agent",
                    "status": background_status(task.status),
                    "detail": task.detail,
                }))
            }
            _ => None,
        })
        .collect()
}

fn projected_task_events(events: &[Event], tool_name: &str) -> Vec<Value> {
    events
        .iter()
        .filter_map(|event| match event {
            Event::ItemStarted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == tool_name => Some(json!({"kind": "item_started", "tool": name})),
            Event::TaskGraphUpdated(snapshot) => Some(json!({
                "kind": "task_graph_updated",
                "revision": snapshot.revision,
                "subjects": snapshot.tasks.iter().map(|task| task.subject.as_str()).collect::<Vec<_>>(),
            })),
            Event::ItemCompleted {
                item: Item::ToolCall { name, status, .. },
                ..
            } if name == tool_name => Some(json!({
                "kind": "item_completed",
                "tool": name,
                "status": match status {
                    ItemStatus::InProgress => "in_progress",
                    ItemStatus::Completed => "completed",
                    ItemStatus::Failed => "failed",
                },
            })),
            _ => None,
        })
        .collect()
}

async fn sync_task_batch_report() -> Value {
    let (turn_a, started_a, release_a) = gate_turn("SYNC-CHILD-52");
    let (turn_b, started_b, release_b) = gate_turn("SYNC-CHILD-52");
    let (provider, seen) = Provider::mock_recording(vec![turn_a, turn_b]);
    let ui = Arc::new(RecordingUi::default());
    let ctx = ctx_with_provider(provider, ui.clone(), "plan52-sync-batch");
    let calls = vec![
        (
            "toolu_plan52_task_a".into(),
            "run_agent".into(),
            json!({"prompt": "first independent child"}),
        ),
        (
            "toolu_plan52_task_b".into(),
            "run_agent".into(),
            json!({"prompt": "second independent child"}),
        ),
    ];
    let worker = tokio::spawn({
        let ctx = ctx.clone();
        async move { dispatch_tools(calls, &ctx).await }
    });

    tokio::time::timeout(WATCHDOG, async {
        started_a.await.expect("first child never sampled");
        started_b.await.expect("second child never sampled");
    })
    .await
    .expect("task dispatcher serialized two concurrency-safe calls");
    assert_eq!(seen.lock().unwrap().len(), 2);
    let events_before_release = ui.events();
    let before = lifecycle_counts(&events_before_release);
    assert_eq!(before["tool_started"], 2);
    assert_eq!(before["agent_started"], 2);
    assert_eq!(before["tool_completed"], 0);
    assert_eq!(before["agent_completed"], 0);

    release_a.send(()).expect("first release receiver dropped");
    release_b.send(()).expect("second release receiver dropped");
    let results = tokio::time::timeout(WATCHDOG, worker)
        .await
        .expect("task batch did not finish")
        .expect("task batch worker panicked");
    assert!(results.iter().all(|result| {
        matches!(
            result,
            ContentBlock::ToolResult {
                content,
                is_error: false,
                ..
            } if content.as_text().as_ref() == "SYNC-CHILD-52"
        )
    }));
    let after = lifecycle_counts(&ui.events());
    assert_eq!(after["tool_completed"], 2);
    assert_eq!(after["agent_completed"], 2);

    json!({
        "requests_started_before_release": 2,
        "lifecycle_before_release": before,
        "lifecycle_terminal": after,
        "results": result_report(&results),
    })
}

async fn background_reinject_report() -> Value {
    let (turn, started, release) = gate_turn("BACKGROUND-RESULT-52");
    let provider = Provider::mock_scripted(vec![turn]);
    let ui = Arc::new(RecordingUi::default());
    let ctx = ctx_with_provider(provider, ui.clone(), "plan52-background");
    let (start_output, start_error) = run_tool(
        "run_agent",
        json!({"prompt": "background child", "background": true}),
        &ctx,
    )
    .await;
    assert!(!start_error, "{start_output}");
    let agent_id = started_agent_id(&start_output);
    tokio::time::timeout(WATCHDOG, started)
        .await
        .expect("background child did not sample")
        .expect("background child sampling sender dropped");
    release
        .send(())
        .expect("background release receiver dropped");
    ui.wait_for_background(BackgroundTaskStatus::Completed)
        .await;
    assert_eq!(ctx.cfg.background_executions.running_count(), 0);

    let (wait_output, wait_error) = run_tool("wait_for_activity", json!({}), &ctx).await;
    assert!(!wait_error, "{wait_output}");
    let pending = ctx.cfg.inbox.drain();
    assert_eq!(pending.len(), 1);
    let InboxItem::SubAgentResult { label, summary } = &pending[0] else {
        panic!("expected one sub-agent result")
    };
    assert_eq!(label, &agent_id);
    assert_eq!(summary, "BACKGROUND-RESULT-52");

    json!({
        "start_result": start_output.replace(&agent_id, "<AGENT>"),
        "wait_result": wait_output,
        "events": projected_background_events(&ui.events(), &agent_id),
        "inbox": pending.into_iter().map(InboxItem::into_message).map(|text| text.replace(&agent_id, "<AGENT>")).collect::<Vec<_>>(),
        "running_after": ctx.cfg.background_executions.running_count(),
    })
}

async fn stop_and_completion_arbitration_report() -> Value {
    let (turn, started, release) = gate_turn("MUST-NOT-REINJECT-52");
    let provider = Provider::mock_scripted(vec![turn]);
    let ui = Arc::new(RecordingUi::default());
    let ctx = ctx_with_provider(provider, ui.clone(), "plan52-stop-first");
    let (start_output, start_error) = run_tool(
        "run_agent",
        json!({"prompt": "cancelled child", "background": true}),
        &ctx,
    )
    .await;
    assert!(!start_error, "{start_output}");
    let stopped_id = started_agent_id(&start_output);
    tokio::time::timeout(WATCHDOG, started)
        .await
        .expect("stopped child did not sample")
        .expect("stopped child sampling sender dropped");
    let mut inbox_activity = ctx.cfg.inbox.subscribe_activity();
    let (stop_output, stop_error) =
        run_tool("stop_agent", json!({"agent_id": stopped_id}), &ctx).await;
    assert!(!stop_error, "{stop_output}");
    tokio::time::timeout(WATCHDOG, inbox_activity.changed())
        .await
        .expect("cancelled child did not signal activity")
        .expect("cancel activity sender dropped");
    assert!(
        release.send(()).is_err(),
        "cancelled sampling must drop the stream gate receiver"
    );
    ui.wait_for_background(BackgroundTaskStatus::Cancelled)
        .await;
    assert_eq!(ctx.cfg.background_executions.running_count(), 0);
    assert!(ctx.cfg.inbox.is_empty());

    let (turn, started, release) = gate_turn("COMPLETED-FIRST-52");
    let provider = Provider::mock_scripted(vec![turn]);
    let completed_ui = Arc::new(RecordingUi::default());
    let completed_ctx =
        ctx_with_provider(provider, completed_ui.clone(), "plan52-completion-first");
    let (completed_start, completed_start_error) = run_tool(
        "run_agent",
        json!({"prompt": "completed child", "background": true}),
        &completed_ctx,
    )
    .await;
    assert!(!completed_start_error, "{completed_start}");
    let completed_id = started_agent_id(&completed_start);
    tokio::time::timeout(WATCHDOG, started)
        .await
        .expect("completed child did not sample")
        .expect("completed child sampling sender dropped");
    release
        .send(())
        .expect("completed stream release receiver dropped");
    completed_ui
        .wait_for_background(BackgroundTaskStatus::Completed)
        .await;
    assert_eq!(completed_ctx.cfg.background_executions.running_count(), 0);
    let completed_inbox = completed_ctx.cfg.inbox.drain();
    assert_eq!(completed_inbox.len(), 1);
    let (late_stop, late_stop_error) = run_tool(
        "stop_agent",
        json!({"agent_id": completed_id}),
        &completed_ctx,
    )
    .await;
    assert!(late_stop_error);
    assert!(late_stop.contains("not running (status: completed)"));

    json!({
        "stop_first": {
            "stop_result": stop_output.replace(&stopped_id, "<AGENT>"),
            "events": projected_background_events(&ui.events(), &stopped_id),
            "inbox_empty": ctx.cfg.inbox.is_empty(),
            "running_after": ctx.cfg.background_executions.running_count(),
        },
        "completion_first": {
            "events": projected_background_events(&completed_ui.events(), &completed_id),
            "late_stop_result": late_stop.replace(&completed_id, "<AGENT>"),
            "late_stop_is_error": late_stop_error,
            "inbox": completed_inbox.into_iter().map(InboxItem::into_message).map(|text| text.replace(&completed_id, "<AGENT>")).collect::<Vec<_>>(),
            "running_after": completed_ctx.cfg.background_executions.running_count(),
        },
    })
}

async fn inbox_final_boundary_report() -> Value {
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(text_blocks("FIRST-FINAL-52")),
        MockTurn::Blocks(text_blocks("SECOND-FINAL-52")),
    ]);
    let ctx = with_provider(test_ctx(0, "plan52-inbox-boundary"), provider);
    let ui = Arc::new(BoundaryUi::new(ctx.cfg.inbox.clone()));
    let ui_dyn: Arc<dyn Ui> = ui;
    let mut history = History::new(ctx.cfg.offload_dir.clone());
    history.record(Message::user_text("parent request"));
    let outcome = run_turn(&ctx.cfg, &mut history, &ui_dyn, &ctx.cancel, 0).await;
    assert_eq!(outcome.rounds, 2);
    assert_eq!(outcome.final_text, "SECOND-FINAL-52");
    let requests = seen.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let first = serde_json::to_string(&requests[0].messages).unwrap();
    let second = serde_json::to_string(&requests[1].messages).unwrap();
    assert!(!first.contains("BOUNDARY-MARKER-52"));
    assert!(second.contains("BOUNDARY-MARKER-52"));
    let history_value = serde_json::to_value(history.messages()).unwrap();

    json!({
        "request_count": requests.len(),
        "first_request_contains_inbox": false,
        "second_request_contains_inbox": true,
        "rounds": outcome.rounds,
        "final_text": outcome.final_text,
        "history": history_value,
    })
}

async fn native_surface_report() -> Value {
    // `stop_program` left this list with plan 113: it now ships only on the
    // `program` surface, which is off by default, and this report reads the
    // default surface. Its own gating is pinned by
    // `the_program_surface_gates_run_program_and_its_stop_tool`.
    const EXPECTED_NATIVE: [&str; 4] =
        ["run_agent", "task_write", "wait_for_activity", "stop_agent"];
    // Plan 188 collapsed the five CRUD tools into one whole-table write; the
    // names stay listed here because a resumed transcript can still carry them
    // and the depth gate must refuse them as firmly as it refuses the live one.
    const TASK_TOOLS: [&str; 6] = [
        "task_write",
        "task_create",
        "task_get",
        "task_update",
        "task_list",
        "task_clear",
    ];
    const CLAUDE_SURFACES: [&str; 10] = [
        "Agent",
        "TaskCreate",
        "TaskGet",
        "TaskList",
        "TaskUpdate",
        "TaskOutput",
        "TaskStop",
        "SendMessage",
        "ListAgents",
        "ListPeers",
    ];
    let depth_zero = all_tool_defs(
        0,
        &[],
        30,
        Default::default(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let depth_one = all_tool_defs(
        1,
        &[],
        30,
        Default::default(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let names_zero: Vec<&str> = depth_zero.iter().map(|def| def.name.as_str()).collect();
    let names_one: Vec<&str> = depth_one.iter().map(|def| def.name.as_str()).collect();
    let native_zero: Vec<&str> = EXPECTED_NATIVE
        .iter()
        .copied()
        .filter(|name| names_zero.contains(name))
        .collect();
    assert_eq!(native_zero, EXPECTED_NATIVE);
    let depth_one_tasks = TASK_TOOLS
        .iter()
        .copied()
        .filter(|name| names_one.contains(name))
        .collect::<Vec<_>>();
    assert!(depth_one_tasks.is_empty());
    let claude_present: Vec<&str> = CLAUDE_SURFACES
        .iter()
        .copied()
        .filter(|name| names_zero.contains(name))
        .collect();
    assert!(claude_present.is_empty());
    let agent_schema = depth_zero
        .iter()
        .find(|def| def.name == "run_agent")
        .expect("run_agent definition missing")
        .schema
        .clone();
    let task_schema = depth_zero
        .iter()
        .find(|def| def.name == "task_write")
        .expect("task_write definition missing")
        .schema
        .clone();

    let task_ui = Arc::new(RecordingUi::default());
    let mut ctx = test_ctx(0, "plan52-native-surface");
    ctx.ui = task_ui.clone();

    // Every field the graph has shed — plan 73's `owner`, plan 187's
    // `blocked_by`, plan 188's `id`/`task_id`/`description` — is rejected by
    // name rather than ignored, so a transcript written against an older
    // schema fails loudly instead of half-applying.
    let mut retired_field_gate = Vec::new();
    for (field, kind, row) in [
        (
            "owner",
            "string",
            json!({"subject":"forged","status":"pending","owner":"assistant"}),
        ),
        (
            "owner",
            "null",
            json!({"subject":"forged","status":"pending","owner":Value::Null}),
        ),
        (
            "blocked_by",
            "array",
            json!({"subject":"forged","status":"pending","blocked_by":["1"]}),
        ),
        (
            "task_id",
            "string",
            json!({"subject":"forged","status":"pending","task_id":"1"}),
        ),
        (
            "id",
            "string",
            json!({"subject":"forged","status":"pending","id":"1"}),
        ),
        (
            "description",
            "string",
            json!({"subject":"forged","status":"pending","description":"instructions"}),
        ),
    ] {
        let (result, is_error) = run_tool("task_write", json!({"tasks":[row]}), &ctx).await;
        assert!(is_error, "task_write/{field}: {result}");
        assert!(
            result.contains(&format!("unknown field `{field}`")),
            "{result}"
        );
        retired_field_gate.push(json!({
            "field": field,
            "kind": kind,
            "result": result,
            "is_error": is_error,
        }));
    }

    // And the five retired tool names are no longer callable at all.
    let mut retired_tool_gate = Vec::new();
    for name in [
        "task_create",
        "task_get",
        "task_update",
        "task_list",
        "task_clear",
    ] {
        let (result, is_error) = run_tool(name, json!({}), &ctx).await;
        assert!(is_error, "{name}: {result}");
        retired_tool_gate.push(json!({"name": name, "result": result}));
    }

    let _ = task_ui.take_events();
    let (first_write, first_write_error) = run_tool(
        "task_write",
        json!({"tasks":[
            {"subject":"first","status":"in_progress"},
            {"subject":"second","status":"pending"},
        ]}),
        &ctx,
    )
    .await;
    assert!(!first_write_error, "{first_write}");
    let first_write_events = projected_task_events(&task_ui.take_events(), "task_write");

    // A status moving backwards, a row dropped and a row added, all in the one
    // call that is the whole truth about the list.
    let (rewrite, rewrite_error) = run_tool(
        "task_write",
        json!({"tasks":[
            {"subject":"first","status":"completed"},
            {"subject":"second","status":"pending"},
            {"subject":"third","status":"pending"},
        ]}),
        &ctx,
    )
    .await;
    assert!(!rewrite_error, "{rewrite}");
    let rewrite_events = projected_task_events(&task_ui.take_events(), "task_write");

    let (no_op, no_op_error) = run_tool(
        "task_write",
        json!({"tasks":[
            {"subject":"first","status":"completed"},
            {"subject":"second","status":"pending"},
            {"subject":"third","status":"pending"},
        ]}),
        &ctx,
    )
    .await;
    assert!(!no_op_error, "{no_op}");
    let no_op_events = projected_task_events(&task_ui.take_events(), "task_write");

    let mut child_cfg = ctx.cfg.test_clone();
    child_cfg.tool_allowlist = Some(Arc::new(
        TASK_TOOLS.iter().map(|name| (*name).to_string()).collect(),
    ));
    let child_ctx = ToolCtx {
        cfg: Arc::new(child_cfg),
        depth: 1,
        ..ctx.clone()
    };
    let (child_write, child_write_error) = run_tool(
        "task_write",
        json!({"tasks":[{"subject":"forged","status":"pending"}]}),
        &child_ctx,
    )
    .await;
    assert!(child_write_error, "{child_write}");
    assert_eq!(
        child_write,
        "tool 'task_write' is only available to the root agent"
    );
    // The refused child call is an ordinary failed tool call on the same Ui;
    // drop its two lifecycle events so the next scenario reads its own.
    let _ = task_ui.take_events();

    let (cleared, cleared_error) = run_tool("task_write", json!({"tasks":[]}), &ctx).await;
    assert!(!cleared_error, "{cleared}");
    let cleared_events = projected_task_events(&task_ui.take_events(), "task_write");

    let (strict_write, strict_write_error) =
        run_tool("task_write", json!({"unexpected": true}), &ctx).await;
    assert!(strict_write_error, "{strict_write}");
    let strict_write_events = projected_task_events(&task_ui.take_events(), "task_write");

    ctx.cfg
        .inbox
        .push(InboxItem::Steer("WAIT-PENDING-52".into()));
    let (wait_output, wait_error) = run_tool("wait_for_activity", json!({}), &ctx).await;
    assert!(!wait_error, "{wait_output}");
    let pending_after_wait = ctx.cfg.inbox.drain();
    assert_eq!(pending_after_wait.len(), 1);
    let (unknown_stop, unknown_stop_error) = run_tool(
        "stop_agent",
        json!({"agent_id": "agent-does-not-exist"}),
        &ctx,
    )
    .await;
    assert!(unknown_stop_error);

    json!({
        "depth_zero_native_tools": native_zero,
        "depth_one_native_tools": depth_one_tasks,
        "claude_named_tools_present": claude_present,
        "agent_schema": agent_schema,
        "task_schema": task_schema,
        "task_list": {
            "first_write": {"result": first_write, "events": first_write_events},
            "rewrite": {"result": rewrite, "events": rewrite_events},
            "no_op": {"result": no_op, "events": no_op_events},
            "cleared": {"result": cleared, "events": cleared_events},
            "strict_write": {
                "result": strict_write,
                "is_error": strict_write_error,
                "events": strict_write_events,
            },
        },
        "retired_field_gate": retired_field_gate,
        "retired_tool_gate": retired_tool_gate,
        "child_task_gate": child_write,
        "wait_for_activity": {
            "result": wait_output,
            "pending_after_wait": pending_after_wait.into_iter().map(InboxItem::into_message).collect::<Vec<_>>(),
        },
        "unknown_stop": {
            "result": unknown_stop,
            "is_error": unknown_stop_error,
        },
    })
}

#[tokio::test]
async fn emit_plan52_parity_report() {
    let report = json!({
        "schema_version": 1,
        "surface": "kloop-native",
        "scenarios": {
            "sync_task_batch": sync_task_batch_report().await,
            "background_reinject": background_reinject_report().await,
            "stop_completion_arbitration": stop_and_completion_arbitration_report().await,
            "inbox_final_boundary": inbox_final_boundary_report().await,
            "native_surface": native_surface_report().await,
        },
    });
    if let Some(path) = std::env::var_os("KLOOP_PLAN52_PARITY_REPORT") {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("parity report path must be new");
        serde_json::to_writer_pretty(&mut file, &report).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
    }
}
