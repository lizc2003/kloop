use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kloop_protocol::{ContentBlock, Message};
use kloop_provider::{MockTurn, Provider};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::testutil::{run_tool, test_ctx};
use super::{all_tool_defs, is_concurrency_safe, ToolCtx};
use crate::agent::{run_structured_turn, run_turn, EndReason, Ui};
use crate::config::SurfaceCapabilities;
use crate::event::{BackgroundTaskStatus, Event};
use crate::history::History;
use crate::inbox::InboxItem;
use crate::interaction::{QuestionAnswer, QuestionOutcome, QuestionRequest, Questioner};
use crate::permissions::{Approver, ConfirmRequest, Decision, Mode, PermissionRules, Permissions};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-plan53-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct RecordingUi {
    events: Mutex<Vec<Event>>,
}

impl RecordingUi {
    fn events(&self) -> Vec<Event> {
        self.events.lock().unwrap().clone()
    }
}

impl Ui for RecordingUi {
    fn emit(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
    }
}

struct ScriptedQuestioner(Mutex<VecDeque<QuestionOutcome>>);

impl ScriptedQuestioner {
    fn new(outcomes: impl IntoIterator<Item = QuestionOutcome>) -> Arc<Self> {
        Arc::new(Self(Mutex::new(outcomes.into_iter().collect())))
    }
}

impl Questioner for ScriptedQuestioner {
    fn ask(
        &self,
        _request: QuestionRequest,
    ) -> Pin<Box<dyn Future<Output = QuestionOutcome> + Send + '_>> {
        let outcome = self
            .0
            .lock()
            .unwrap()
            .pop_front()
            .expect("missing scripted question outcome");
        Box::pin(async move { outcome })
    }
}

struct ScriptedApprover {
    decisions: Mutex<VecDeque<Decision>>,
    previews: Mutex<Vec<Option<String>>>,
}

impl ScriptedApprover {
    fn new(decisions: impl IntoIterator<Item = Decision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            previews: Mutex::new(Vec::new()),
        })
    }
}

impl Approver for ScriptedApprover {
    fn confirm(
        &self,
        request: ConfirmRequest,
    ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        self.previews.lock().unwrap().push(request.preview);
        let decision = self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Decision::Deny);
        Box::pin(async move { decision })
    }
}

fn tool_use(id: &str, name: &str, input: Value) -> ContentBlock {
    ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    }
}

fn enabled_surface() -> SurfaceCapabilities {
    SurfaceCapabilities {
        questions: true,
        plan_control: true,
        workflow: true,
        ..Default::default()
    }
}

fn question_input() -> Value {
    json!({
        "questions": [{
            "question": "Which layout?",
            "header": "Layout",
            "options": [
                {"label": "Rows", "description": "stack", "preview": "ROW"},
                {"label": "Columns", "description": "split", "preview": "COL"}
            ],
            "multiSelect": false
        }]
    })
}

fn context_with_questioner(tag: &str, questioner: Arc<dyn Questioner>) -> ToolCtx {
    let mut context = test_ctx(0, tag);
    let mut config = context.cfg.test_clone();
    config.questioner = Some(questioner);
    config.surface = enabled_surface();
    context.cfg = Arc::new(config);
    context
}

fn context_with_permissions(
    tag: &str,
    mode: Mode,
    approver: Arc<dyn Approver>,
) -> (ToolCtx, Arc<RecordingUi>) {
    let root = std::env::temp_dir().join(format!("kloop-plan53-permissions-{tag}"));
    let permissions =
        Permissions::new(mode, &PermissionRules::default(), root, Some(approver)).unwrap();
    let ui = Arc::new(RecordingUi::default());
    let mut context = test_ctx(0, tag);
    let mut config = context.cfg.test_clone();
    config.permissions = Arc::new(permissions);
    config.surface = enabled_surface();
    context.cfg = Arc::new(config);
    context.ui = ui.clone();
    (context, ui)
}

fn native_surface_report() -> Value {
    let enabled = all_tool_defs(
        0,
        &[],
        30,
        enabled_surface(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let disabled = all_tool_defs(
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
        enabled_surface(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let names = [
        "ask_user_question",
        "enter_plan_mode",
        "exit_plan_mode",
        "workflow",
    ];
    assert!(names
        .iter()
        .all(|name| enabled.iter().any(|definition| definition.name == *name)));
    assert!(names
        .iter()
        .all(|name| disabled.iter().all(|definition| definition.name != *name)));
    assert!(names
        .iter()
        .all(|name| depth_one.iter().all(|definition| definition.name != *name)));
    assert!(enabled
        .iter()
        .all(|definition| definition.name != "structured_output"));
    let run_program = enabled
        .iter()
        .find(|definition| definition.name == "run_program")
        .unwrap();
    let run_program_wire = format!("{} {}", run_program.description, run_program.schema);
    assert!(names.iter().all(|name| !run_program_wire.contains(name)));

    json!({
        "enabled_depth_zero": names,
        "disabled_surface_excludes_all": true,
        "depth_one_excludes_all": true,
        "structured_output_ordinary_absent": true,
        "run_program_excludes_controls": true,
    })
}

async fn question_report() -> Value {
    let definition = all_tool_defs(
        0,
        &[],
        30,
        enabled_surface(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    )
    .into_iter()
    .find(|definition| definition.name == "ask_user_question")
    .unwrap();
    assert_eq!(definition.schema["required"], json!(["questions"]));
    assert_eq!(definition.schema["properties"]["questions"]["minItems"], 1);
    assert_eq!(definition.schema["properties"]["questions"]["maxItems"], 4);
    let options = &definition.schema["properties"]["questions"]["items"]["properties"]["options"];
    assert_eq!(options["minItems"], 2);
    assert_eq!(options["maxItems"], 4);
    let questioner = ScriptedQuestioner::new([
        QuestionOutcome::Answered(vec![QuestionAnswer {
            question_index: 0,
            selected: vec![0],
            other: None,
            notes: None,
        }]),
        QuestionOutcome::Cancelled,
        QuestionOutcome::Unavailable("closed".into()),
    ]);
    let context = context_with_questioner("question", questioner);
    let mut answered_input = question_input();
    answered_input["answers"] = json!({"Which layout?": "Columns"});
    let (answered, answered_error) = run_tool("ask_user_question", answered_input, &context).await;
    assert!(!answered_error, "{answered}");
    assert!(answered.contains("Rows") && !answered.contains("Columns"));
    let (cancelled, cancelled_error) =
        run_tool("ask_user_question", question_input(), &context).await;
    assert!(!cancelled_error && cancelled.contains("cancelled"));
    let (unavailable, unavailable_error) =
        run_tool("ask_user_question", question_input(), &context).await;
    assert!(unavailable_error && unavailable.contains("closed"));
    let mut one_option = question_input();
    one_option["questions"][0]["options"] = json!([
        {"label": "Rows", "description": "Use rows"}
    ]);
    let mut five_options = question_input();
    five_options["questions"][0]["options"] = json!([
        {"label": "One", "description": "one"},
        {"label": "Two", "description": "two"},
        {"label": "Three", "description": "three"},
        {"label": "Four", "description": "four"},
        {"label": "Five", "description": "five"}
    ]);
    let mut question_null = question_input();
    question_null["questions"][0]["question"] = Value::Null;
    let mut header_null = question_input();
    header_null["questions"][0]["header"] = Value::Null;
    let mut options_null = question_input();
    options_null["questions"][0]["options"] = Value::Null;
    let mut multi_select_null = question_input();
    multi_select_null["questions"][0]["multiSelect"] = Value::Null;
    let mut option_label_null = question_input();
    option_label_null["questions"][0]["options"][0]["label"] = Value::Null;
    let mut option_description_null = question_input();
    option_description_null["questions"][0]["options"][0]["description"] = Value::Null;
    let mut unknown_field = question_input();
    unknown_field["unexpected"] = json!(true);
    let invalid_cases = [
        ("empty_questions", json!({"questions": []})),
        ("questions_wrong_type", json!({"questions": "invalid"})),
        ("questions_null", json!({"questions": null})),
        ("one_option", one_option),
        ("five_options", five_options),
        ("question_null", question_null),
        ("header_null", header_null),
        ("options_null", options_null),
        ("multi_select_null", multi_select_null),
        ("option_label_null", option_label_null),
        ("option_description_null", option_description_null),
        ("unknown_field", unknown_field),
    ];
    for (name, input) in &invalid_cases {
        let (invalid, invalid_error) = run_tool("ask_user_question", input.clone(), &context).await;
        assert!(invalid_error, "{name}: {invalid}");
    }
    let mut depth_one = context.clone();
    depth_one.depth = 1;
    let (blocked, blocked_error) =
        run_tool("ask_user_question", question_input(), &depth_one).await;
    assert!(blocked_error && blocked.contains("top-level"));
    assert!(is_concurrency_safe(
        "ask_user_question",
        &question_input(),
        &[]
    ));

    json!({
        "schema_required": ["questions"],
        "bounds": {"questions": [1, 4], "options": [2, 4]},
        "prefilled_answers_ignored": true,
        "answered": true,
        "cancel_is_paired_non_error": true,
        "unavailable_is_error": true,
        "invalid_cases": invalid_cases.iter().map(|(name, _)| *name).collect::<Vec<_>>(),
        "depth_one_blocked": true,
        "concurrency_safe": true,
    })
}

async fn plan_control_report() -> Value {
    let approver =
        ScriptedApprover::new([Decision::Allow(crate::permissions::ApprovalScope::Once)]);
    let (context, ui) = context_with_permissions("plan", Mode::AcceptEdits, approver.clone());
    let names_before: Vec<String> =
        all_tool_defs(0, &[], 30, context.cfg.surface, &context.cfg.shell_programs)
            .into_iter()
            .map(|definition| definition.name)
            .collect();
    let (entered, entered_error) = run_tool("enter_plan_mode", json!({}), &context).await;
    assert!(!entered_error && entered.contains("Entered plan mode"));
    assert_eq!(context.cfg.permissions.mode(), Mode::Plan);
    let (repeat, repeat_error) = run_tool("enter_plan_mode", json!({}), &context).await;
    assert!(!repeat_error && repeat.contains("Already in plan mode"));
    let (unknown, unknown_error) =
        run_tool("enter_plan_mode", json!({"unexpected": true}), &context).await;
    assert!(!unknown_error && unknown.contains("Already in plan mode"));
    let plan = "1. inspect\n2. implement";
    let (exited, exited_error) = run_tool("exit_plan_mode", json!({"plan": plan}), &context).await;
    assert!(!exited_error && exited.contains("approved"));
    assert_eq!(context.cfg.permissions.mode(), Mode::AcceptEdits);
    assert_eq!(
        approver.previews.lock().unwrap().as_slice(),
        &[Some(plan.to_string())]
    );
    let names_after: Vec<String> =
        all_tool_defs(0, &[], 30, context.cfg.surface, &context.cfg.shell_programs)
            .into_iter()
            .map(|definition| definition.name)
            .collect();
    assert_eq!(names_before, names_after);
    assert!(!is_concurrency_safe("enter_plan_mode", &json!({}), &[]));
    assert!(!is_concurrency_safe(
        "exit_plan_mode",
        &json!({"plan": plan}),
        &[]
    ));
    let modes: Vec<&'static str> = ui
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::ModeChanged(Mode::Plan) => Some("plan"),
            Event::ModeChanged(Mode::AcceptEdits) => Some("accept_edits"),
            Event::ModeChanged(_) => Some("other"),
            _ => None,
        })
        .collect();
    assert_eq!(modes, ["plan", "accept_edits"]);

    let rejected_approver = ScriptedApprover::new([Decision::Deny]);
    let (rejected_context, _) =
        context_with_permissions("plan-reject", Mode::Plan, rejected_approver);
    let (rejected, rejected_error) = run_tool(
        "exit_plan_mode",
        json!({"plan": "draft"}),
        &rejected_context,
    )
    .await;
    assert!(!rejected_error && rejected.contains("keep planning"));
    assert_eq!(rejected_context.cfg.permissions.mode(), Mode::Plan);

    json!({
        "entered": true,
        "repeat_idempotent": true,
        "enter_unknown_field_accepted": true,
        "approved_restored": "accept_edits",
        "rejected_stayed_plan": true,
        "inline_plan_preview": true,
        "serial": true,
        "mode_events": modes,
        "tool_array_stable": true,
    })
}

async fn workflow_report() -> Value {
    let root = TempRoot::new("workflow");
    let ui = Arc::new(RecordingUi::default());
    let mut context = test_ctx(0, "plan53-workflow");
    let mut config = context.cfg.test_clone();
    config.offload_dir = root.path().join("offload");
    config.surface.workflow = true;
    context.cfg = Arc::new(config);
    context.ui = ui.clone();

    let (invalid, invalid_error) =
        run_tool("workflow", json!({"script": "return 1"}), &context).await;
    assert!(invalid_error && invalid.contains("meta"));
    assert_eq!(context.cfg.background_tasks.running_count(), 0);
    assert!(context.cfg.inbox.is_empty());

    let mut activity = context.cfg.inbox.subscribe_activity();
    let (launched, launch_error) = run_tool(
        "workflow",
        json!({
            "script": "export const meta = { name: 'plan53', description: 'plan53 report', phases: [{ title: 'Run' }] }; phase('Run'); return { marker: 'WORKFLOW-53' };"
        }),
        &context,
    )
    .await;
    assert!(!launch_error, "{launched}");
    assert!(launched.contains("Task ID: workflow-") && launched.contains("Run ID: wf_"));
    if context.cfg.background_tasks.running_count() != 0 {
        tokio::time::timeout(Duration::from_secs(2), activity.changed())
            .await
            .expect("Workflow did not signal completion")
            .expect("Workflow activity channel closed");
    }
    assert_eq!(context.cfg.background_tasks.running_count(), 0);
    let items = context.cfg.inbox.drain();
    let [InboxItem::WorkflowResult {
        summary,
        output_path,
        ..
    }] = items.as_slice()
    else {
        panic!("expected one Workflow result: {items:?}")
    };
    assert!(summary.contains("WORKFLOW-53"));
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(output_path).unwrap()).unwrap(),
        json!({"marker": "WORKFLOW-53"})
    );
    let background = ui
        .events()
        .into_iter()
        .filter_map(|event| match event {
            Event::BackgroundTaskUpdated(task) => Some(task),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(background.len() >= 3);
    assert_eq!(
        background.first().unwrap().status,
        BackgroundTaskStatus::Running
    );
    assert_eq!(
        background.last().unwrap().status,
        BackgroundTaskStatus::Completed
    );
    assert_eq!(
        background
            .iter()
            .filter(|task| task.status != BackgroundTaskStatus::Running)
            .count(),
        1
    );
    assert!(is_concurrency_safe("workflow", &json!({}), &[]));
    let statuses = background
        .iter()
        .map(|task| match task.status {
            BackgroundTaskStatus::Running => "running",
            BackgroundTaskStatus::Completed => "completed",
            BackgroundTaskStatus::Failed => "failed",
            BackgroundTaskStatus::Cancelled => "cancelled",
        })
        .collect::<Vec<_>>();
    assert_eq!(statuses, ["running", "running", "completed"]);
    assert_eq!(context.cfg.shutdown_background_work().await, 0);

    json!({
        "standalone": true,
        "strict_invalid_prelaunch": true,
        "immediate_identity": ["task_id", "run_id", "script_path"],
        "concurrency_safe": true,
        "event_statuses": statuses,
        "terminal_count": 1,
        "inbox_delivery_count": 1,
        "result_persisted": true,
        "running_after": 0,
        "shutdown_residue": 0,
    })
}

fn structured_config(
    root: &Path,
    turns: Vec<MockTurn>,
) -> (
    Arc<crate::config::Config>,
    Arc<Mutex<Vec<kloop_provider::MockRequest>>>,
) {
    let (provider, seen) = Provider::mock_recording(turns);
    let context = test_ctx(1, "plan53-structured");
    let mut config = context.cfg.test_clone();
    config.provider = Arc::new(provider);
    config.max_rounds = Some(10);
    config.offload_dir = root.join("offload");
    config.agent_label = "plan53-structured".into();
    (Arc::new(config), seen)
}

async fn structured_output_report() -> Value {
    let root = TempRoot::new("structured");
    let schema = json!({
        "type": "object",
        "properties": {"count": {"type": "integer"}},
        "required": ["count"],
        "additionalProperties": false
    });
    let (config, seen) = structured_config(
        root.path(),
        vec![
            MockTurn::Blocks(vec![tool_use(
                "structured-invalid",
                "structured_output",
                json!({"count": "bad"}),
            )]),
            MockTurn::Blocks(vec![tool_use(
                "structured-valid",
                "structured_output",
                json!({"count": 53}),
            )]),
        ],
    );
    let ui: Arc<dyn Ui> = Arc::new(RecordingUi::default());
    let mut history = History::new(config.offload_dir.clone());
    history.record(Message::user_text("return a count"));
    let outcome = run_structured_turn(
        &config,
        &mut history,
        &ui,
        &CancellationToken::new(),
        1,
        schema.clone(),
    )
    .await;
    assert_eq!(outcome.reason, EndReason::Completed);
    assert_eq!(outcome.structured_output, Some(json!({"count": 53})));
    assert_eq!(seen.lock().unwrap().len(), 2);
    assert!(seen.lock().unwrap().iter().all(|request| request
        .tools
        .iter()
        .any(|definition| definition.name == "structured_output" && definition.schema == schema)));
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

    let (ordinary_config, ordinary_seen) = structured_config(
        root.path(),
        vec![MockTurn::Blocks(vec![ContentBlock::Text {
            text: "plain".into(),
        }])],
    );
    let mut ordinary_history = History::new(root.path().join("ordinary"));
    ordinary_history.record(Message::user_text("ordinary"));
    let ordinary = run_turn(
        &ordinary_config,
        &mut ordinary_history,
        &ui,
        &CancellationToken::new(),
        1,
    )
    .await;
    assert_eq!(ordinary.reason, EndReason::Completed);
    assert!(ordinary_seen.lock().unwrap()[0]
        .tools
        .iter()
        .all(|definition| definition.name != "structured_output"));

    json!({
        "ordinary_turn_absent": true,
        "schema_specialized": true,
        "invalid_then_valid": true,
        "rounds": 2,
        "result": {"count": 53},
        "tool_results_paired": true,
    })
}

#[tokio::test]
async fn emit_plan53_parity_report() {
    let report = json!({
        "schema_version": 1,
        "surface": "kloop-native",
        "scenarios": {
            "native_surface": native_surface_report(),
            "question": question_report().await,
            "plan_control": plan_control_report().await,
            "workflow": workflow_report().await,
            "structured_output": structured_output_report().await,
        },
    });
    if let Some(path) = std::env::var_os("KLOOP_PLAN53_PARITY_REPORT") {
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
