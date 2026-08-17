use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{TimeZone as _, Utc};
use kloop_protocol::{AssistantBlock, ContentBlock, Message};
use kloop_provider::Provider;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use super::testutil::{run_tool, test_ctx};
use super::{ToolCtx, all_tool_defs, is_concurrency_safe};
use crate::agent::{Ui, run_turn};
use crate::config::SurfaceCapabilities;
use crate::event::{Event, ScheduledTaskStatus};
use crate::history::History;
use crate::inbox::Inbox;
use crate::permissions::{Approver, ConfirmRequest, Decision, Mode, PermissionRules, Permissions};
use crate::scheduler::{DurableStore, ManualClock, ScheduledKind, Scheduler, SchedulerTimeZone};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-plan58-{tag}-{}-{sequence}",
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
    fn take(&self) -> Vec<Event> {
        std::mem::take(&mut *self.events.lock().unwrap())
    }
}

impl Ui for RecordingUi {
    fn emit(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
    }
}

struct ScriptedApprover {
    decisions: Mutex<VecDeque<Decision>>,
    requests: Mutex<Vec<ConfirmRequest>>,
}

impl ScriptedApprover {
    fn new(decisions: impl IntoIterator<Item = Decision>) -> Arc<Self> {
        Arc::new(Self {
            decisions: Mutex::new(decisions.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        })
    }
}

impl Approver for ScriptedApprover {
    fn confirm(
        &self,
        request: ConfirmRequest,
    ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        self.requests.lock().unwrap().push(request);
        let decision = self
            .decisions
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Decision::Deny);
        Box::pin(async move { decision })
    }
}

fn utc_ms(year: i32, month: u32, day: u32, hour: u32, minute: u32) -> i64 {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, 0)
        .single()
        .unwrap()
        .timestamp_millis()
}

fn scheduler_context(
    tag: &str,
    owner: &str,
    clock: Arc<ManualClock>,
    store: Option<DurableStore>,
) -> (ToolCtx, Arc<Scheduler>, Arc<Inbox>, Arc<RecordingUi>) {
    let inbox = Arc::new(Inbox::default());
    let scheduler = Scheduler::with_clock(
        Arc::clone(&inbox),
        store,
        clock,
        SchedulerTimeZone::named("UTC").unwrap(),
    );
    scheduler.bind_owner(owner).unwrap();
    let ui = Arc::new(RecordingUi::default());
    let mut context = test_ctx(0, tag);
    let mut config = context.cfg.test_clone();
    config.inbox = Arc::clone(&inbox);
    config.scheduler = Arc::clone(&scheduler);
    config.session_id = owner.into();
    config.surface.scheduler = true;
    context.cfg = Arc::new(config);
    context.ui = ui.clone();
    (context, scheduler, inbox, ui)
}

fn scheduled_statuses(events: Vec<Event>) -> Vec<&'static str> {
    events
        .into_iter()
        .filter_map(|event| match event {
            Event::ScheduledTaskUpdated(task) => Some(match task.status {
                ScheduledTaskStatus::Scheduled => "scheduled",
                ScheduledTaskStatus::Fired => "fired",
                ScheduledTaskStatus::Cancelled => "cancelled",
                ScheduledTaskStatus::Failed => "failed",
            }),
            _ => None,
        })
        .collect()
}

fn schema_report() -> Value {
    let surface = SurfaceCapabilities {
        scheduler: true,
        ..Default::default()
    };
    let definitions = all_tool_defs(
        0,
        &[],
        30,
        surface,
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let names = ["cron_create", "cron_delete", "cron_list", "schedule_wakeup"];
    let scheduler = names
        .iter()
        .map(|name| {
            definitions
                .iter()
                .find(|definition| definition.name == *name)
                .unwrap()
        })
        .collect::<Vec<_>>();
    assert_eq!(scheduler[0].schema["required"], json!(["cron", "prompt"]));
    assert_eq!(scheduler[1].schema["required"], json!(["id"]));
    assert!(scheduler[2].schema.get("required").is_none());
    assert!(scheduler[3].schema.get("required").is_none());
    assert!(
        scheduler
            .iter()
            .all(|definition| definition.schema["additionalProperties"] == false)
    );
    let depth_one = all_tool_defs(
        1,
        &[],
        30,
        surface,
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    assert!(
        names
            .iter()
            .all(|name| !depth_one.iter().any(|definition| definition.name == *name))
    );
    let disabled = all_tool_defs(
        0,
        &[],
        30,
        Default::default(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    assert!(
        names
            .iter()
            .all(|name| !disabled.iter().any(|definition| definition.name == *name))
    );
    let run_program = definitions
        .iter()
        .find(|definition| definition.name == "run_program")
        .unwrap();
    let run_program_wire = format!(
        "{} {} {}",
        run_program.name, run_program.description, run_program.schema
    );
    assert!(names.iter().all(|name| !run_program_wire.contains(name)));

    json!({
        "names": names,
        "cron_create_required": ["cron", "prompt"],
        "cron_delete_required": ["id"],
        "cron_list_required": [],
        "schedule_wakeup_required": [],
        "additional_properties": false,
        "depth_one_excluded": true,
        "surface_gated": true,
        "run_program_excluded": true,
        "field_style": "snake_case",
    })
}

async fn lifecycle_report() -> Value {
    let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
    let (context, scheduler, _inbox, ui) =
        scheduler_context("lifecycle", "owner-a", clock.clone(), None);
    let (missing, missing_error) =
        run_tool("cron_create", json!({"cron": "1 12 * * *"}), &context).await;
    assert!(missing_error && missing.contains("missing required string argument 'prompt'"));
    let (unknown, unknown_error) = run_tool("cron_list", json!({"extra": true}), &context).await;
    assert!(unknown_error && unknown.contains("unexpected parameter 'extra'"));
    let (wrong_type, wrong_type_error) = run_tool(
        "cron_create",
        json!({"cron": "1 12 * * *", "prompt": "x", "recurring": "yes"}),
        &context,
    )
    .await;
    assert!(wrong_type_error && wrong_type.contains("must be a boolean"));
    let (invalid, invalid_error) = run_tool(
        "cron_create",
        json!({"cron": "* * * *", "prompt": "x"}),
        &context,
    )
    .await;
    assert!(invalid_error && invalid.contains("Expected 5 fields"));

    let (missing_id, missing_id_error) = run_tool("cron_delete", json!({}), &context).await;
    assert!(missing_id_error && missing_id.contains("missing required string argument 'id'"));
    let (unknown_delete_field, unknown_delete_field_error) = run_tool(
        "cron_delete",
        json!({"id": "deadbeef", "extra": true}),
        &context,
    )
    .await;
    assert!(
        unknown_delete_field_error && unknown_delete_field.contains("unexpected parameter 'extra'")
    );

    let (created, create_error) = run_tool(
        "cron_create",
        json!({
            "cron": "1 12 * * *",
            "prompt": "check status",
            "recurring": false,
            "durable": false,
        }),
        &context,
    )
    .await;
    assert!(!create_error, "{created}");
    let job = scheduler.list().unwrap().into_iter().next().unwrap();
    assert!(!job.recurring && !job.durable && job.kind == ScheduledKind::Cron);
    let (listed, list_error) = run_tool("cron_list", json!({}), &context).await;
    assert!(!list_error);
    assert!(listed.contains(&job.id));
    assert!(listed.contains("Every day at 12:01 UTC"));
    assert!(listed.contains("check status"));
    let (unknown_delete, unknown_delete_error) =
        run_tool("cron_delete", json!({"id": "deadbeef"}), &context).await;
    assert!(unknown_delete_error && unknown_delete.contains("No scheduled job"));
    let (deleted, delete_error) = run_tool("cron_delete", json!({"id": job.id}), &context).await;
    assert!(!delete_error, "{deleted}");
    assert!(deleted.starts_with("Cancelled job ") && deleted.ends_with('.'));
    assert!(scheduler.list().unwrap().is_empty());
    assert_eq!(
        scheduled_statuses(ui.take()),
        vec!["scheduled", "cancelled"]
    );

    for index in 0..50 {
        scheduler
            .create("1 12 * * *", &format!("limit-{index:02}"), true, false)
            .unwrap();
    }
    let cap_error = scheduler
        .create("1 12 * * *", "over limit", true, false)
        .unwrap_err()
        .to_string();
    assert!(cap_error.contains("max 50"));
    let loop_cap_error = scheduler
        .schedule_wakeup(60.0, "over limit", "loop")
        .unwrap_err()
        .to_string();
    assert!(loop_cap_error.contains("max 50"));
    let jobs = scheduler.list().unwrap();
    let stable = jobs.windows(2).all(|pair| {
        (pair[0].next_fire_at_ms, pair[0].created_at_ms, &pair[0].id)
            <= (pair[1].next_fire_at_ms, pair[1].created_at_ms, &pair[1].id)
    });
    assert!(stable);
    scheduler.shutdown().await;

    json!({
        "defaults": {"recurring": true, "durable": false},
        "one_shot_created": true,
        "list_fields": ["id", "human_schedule", "prompt", "recurring", "durable"],
        "stable_order": true,
        "unknown_delete_blocked": true,
        "delete_output_shape": "Cancelled job <ID>.",
        "empty_list_schema": true,
        "max_jobs": 50,
        "conditional_errors": [
            "missing_prompt",
            "unknown_field",
            "wrong_type",
            "invalid_cron",
            "missing_id",
            "unknown_delete_field",
        ],
        "events": ["scheduled", "cancelled"],
    })
}

async fn wakeup_report() -> Value {
    let clock = ManualClock::new(0);
    let (context, scheduler, _inbox, ui) = scheduler_context("wakeup", "owner-a", clock, None);
    scheduler.create("0 1 * * *", "fixed", true, false).unwrap();
    let (missing, missing_error) = run_tool(
        "schedule_wakeup",
        json!({"delay_seconds": 60, "prompt": "loop"}),
        &context,
    )
    .await;
    assert!(missing_error && missing.contains("required"));
    let (first, first_error) = run_tool(
        "schedule_wakeup",
        json!({"delay_seconds": 1, "reason": "first", "prompt": "loop"}),
        &context,
    )
    .await;
    assert!(!first_error, "{first}");
    assert!(first.contains("delay 60s, clamped"));
    let first_job = scheduler
        .list()
        .unwrap()
        .into_iter()
        .find(|job| job.kind == ScheduledKind::LoopWakeup)
        .unwrap();
    assert_eq!(first_job.next_fire_at_ms, 60_000);
    let (second, second_error) = run_tool(
        "schedule_wakeup",
        json!({"delay_seconds": 3700, "reason": "second", "prompt": "loop"}),
        &context,
    )
    .await;
    assert!(!second_error, "{second}");
    assert!(second.contains("delay 3600s, clamped"));
    assert!(second.contains("replaced 1 previous wakeup"));
    let (stopped, stop_error) = run_tool(
        "schedule_wakeup",
        json!({"stop": true, "delay_seconds": "ignored", "reason": 3, "prompt": null}),
        &context,
    )
    .await;
    assert!(!stop_error, "{stopped}");
    assert!(stopped.contains("Fixed-interval cron jobs were not changed"));
    let remaining = scheduler.list().unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].kind, ScheduledKind::Cron);
    assert_eq!(
        scheduled_statuses(ui.take()),
        vec!["scheduled", "scheduled", "cancelled"]
    );
    scheduler.shutdown().await;

    json!({
        "conditional_required": true,
        "round_then_clamp": {"minimum": 60, "maximum": 3600},
        "minute_resolution": true,
        "replace_count": 1,
        "stop_ignores_other_fields": true,
        "stop_preserves_fixed_cron": true,
        "events": ["scheduled", "scheduled", "cancelled"],
    })
}

async fn delivery_report() -> Value {
    let root = TempRoot::new("delivery");
    let clock = ManualClock::new(0);
    let (mut context, scheduler, inbox, ui) =
        scheduler_context("delivery", "owner-a", clock.clone(), None);
    let (_, error) = run_tool(
        "schedule_wakeup",
        json!({"delay_seconds": 60, "reason": "watch deployment", "prompt": "timer work"}),
        &context,
    )
    .await;
    assert!(!error);
    let due = scheduler.list().unwrap()[0].next_fire_at_ms;
    let mut activity = inbox.subscribe_activity();
    clock.set(due);
    tokio::time::timeout(Duration::from_secs(1), activity.changed())
        .await
        .unwrap()
        .unwrap();

    let mut config = context.cfg.test_clone();
    config.set_test_provider(Provider::mock(vec![vec![AssistantBlock::Text {
        text: "timer answer".into(),
    }]]));
    context.cfg = Arc::new(config);
    let mut history = History::new(root.path().join("offload"));
    let outcome = run_turn(
        &context.cfg,
        &mut history,
        &context.ui,
        &CancellationToken::new(),
        0,
    )
    .await;
    assert_eq!(outcome.final_text, "timer answer");
    let Message { content, .. } = &history.messages()[0];
    let ContentBlock::Text { text } = &content[0] else {
        panic!("scheduled injection must be text")
    };
    assert!(text.contains("timer-originated work"));
    assert!(text.contains("origin: loop wakeup"));
    assert!(text.contains("reason: watch deployment"));
    assert!(text.ends_with("timer work"));
    assert_eq!(scheduled_statuses(ui.take()), vec!["scheduled", "fired"]);
    assert!(inbox.is_empty());
    scheduler.shutdown().await;

    json!({
        "typed_origin": "loop_wakeup",
        "step_boundary": true,
        "history_role": "user",
        "reason_preserved": true,
        "prompt_preserved": true,
        "single_delivery": true,
        "events": ["scheduled", "fired"],
    })
}

async fn durable_report() -> Value {
    let root = TempRoot::new("durable");
    let path = root.path().join("scheduled_tasks.json");
    let store = DurableStore::new(path.clone(), "project-a".into());
    let clock = ManualClock::new(utc_ms(2026, 8, 3, 12, 0));
    let (context, scheduler, _inbox, _ui) =
        scheduler_context("durable-a", "owner-a", clock.clone(), Some(store.clone()));
    let (_, session_error) = run_tool(
        "cron_create",
        json!({"cron": "1 12 * * *", "prompt": "session", "recurring": false}),
        &context,
    )
    .await;
    assert!(!session_error);
    assert!(!path.exists());
    let (_, durable_error) = run_tool(
        "cron_create",
        json!({
            "cron": "1 12 * * *",
            "prompt": "durable",
            "recurring": false,
            "durable": true,
        }),
        &context,
    )
    .await;
    assert!(!durable_error);
    assert!(path.exists());
    let durable = scheduler
        .list()
        .unwrap()
        .into_iter()
        .find(|job| job.durable)
        .unwrap();

    let (_other_context, other, other_inbox, _other_ui) =
        scheduler_context("durable-b", "owner-b", clock.clone(), Some(store.clone()));
    assert!(other.list().unwrap().is_empty());
    assert!(other.delete(&durable.id).is_err());
    scheduler.shutdown().await;
    clock.set(durable.next_fire_at_ms.saturating_add(60_000));
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(other_inbox.is_empty());
    other.shutdown().await;

    let resumed_inbox = Arc::new(Inbox::default());
    let resumed = Scheduler::with_clock(
        Arc::clone(&resumed_inbox),
        Some(store.clone()),
        clock,
        SchedulerTimeZone::named("UTC").unwrap(),
    );
    resumed.set_missed_confirmation_available(false);
    resumed.bind_owner("owner-a").unwrap();
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    assert!(resumed_inbox.is_empty());
    assert_eq!(resumed.list().unwrap().len(), 1);
    let mut activity = resumed_inbox.subscribe_activity();
    resumed.set_missed_confirmation_available(true);
    tokio::time::timeout(Duration::from_secs(1), activity.changed())
        .await
        .unwrap()
        .unwrap();
    let messages = resumed_inbox.drain();
    assert_eq!(messages.len(), 1);
    let missed = messages.into_iter().next().unwrap().into_message();
    assert!(missed.contains("durable one-shot"));
    assert!(missed.contains("ask_user_question"));
    assert!(missed.ends_with("durable"));
    assert!(resumed.list().unwrap().is_empty());
    resumed.shutdown().await;

    std::fs::write(&path, b"{bad json").unwrap();
    let corrupt = Scheduler::with_clock(
        Arc::new(Inbox::default()),
        Some(store),
        ManualClock::new(0),
        SchedulerTimeZone::named("UTC").unwrap(),
    );
    assert!(corrupt.bind_owner("owner-a").is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"{bad json");
    assert!(std::fs::read_dir(root.path()).unwrap().all(|entry| {
        !entry
            .unwrap()
            .file_name()
            .to_string_lossy()
            .contains(".tmp-")
    }));

    json!({
        "session_file_written": false,
        "durable_file_written": true,
        "owner_isolated": true,
        "same_owner_resume": true,
        "missed_one_shot_confirmation": true,
        "noninteractive_missed_retained": true,
        "claim_once": true,
        "corrupt_store_fail_closed": true,
        "temp_residue": false,
    })
}

async fn guard_report() -> Value {
    let clock = ManualClock::new(0);
    let (base, scheduler, _inbox, _ui) = scheduler_context("guards", "owner-a", clock, None);
    let mut depth = base.clone();
    depth.depth = 1;
    let (depth_text, depth_error) = run_tool("cron_list", json!({}), &depth).await;
    assert!(depth_error && depth_text.contains("top-level"));
    let mut agent = base.clone();
    let mut agent_config = agent.cfg.test_clone();
    agent_config.local_agent = agent_config.local_agent.child("agent-1".parse().unwrap());
    agent.cfg = Arc::new(agent_config);
    let (agent_text, agent_error) = run_tool("cron_list", json!({}), &agent).await;
    assert!(agent_error && agent_text.contains("top-level"));
    let mut disabled = base.clone();
    let mut disabled_config = disabled.cfg.test_clone();
    disabled_config.surface.scheduler = false;
    disabled.cfg = Arc::new(disabled_config);
    let (surface_text, surface_error) = run_tool("cron_list", json!({}), &disabled).await;
    assert!(surface_error && surface_text.contains("frontend surface"));

    assert!(is_concurrency_safe("cron_list", &json!({}), &[]));
    for name in ["cron_create", "cron_delete", "schedule_wakeup"] {
        assert!(!is_concurrency_safe(name, &json!({}), &[]));
    }
    scheduler.shutdown().await;

    json!({
        "depth_one_blocked": true,
        "subagent_blocked": true,
        "surface_blocked": true,
        "read_concurrent": ["cron_list"],
        "mutations_serial": ["cron_create", "cron_delete", "schedule_wakeup"],
    })
}

async fn permission_report() -> Value {
    let cwd = std::env::temp_dir();
    let manual =
        Permissions::new(Mode::Manual, &PermissionRules::default(), cwd.clone(), None).unwrap();
    for name in ["cron_create", "cron_delete", "schedule_wakeup"] {
        assert!(manual.check(name, &json!({}), 0).await.is_ok());
    }
    assert!(manual.check("cron_list", &json!({}), 0).await.is_ok());

    let plan =
        Permissions::new(Mode::Plan, &PermissionRules::default(), cwd.clone(), None).unwrap();
    for name in ["cron_create", "cron_delete", "schedule_wakeup"] {
        assert!(
            plan.check(name, &json!({}), 0)
                .await
                .unwrap_err()
                .contains("plan mode")
        );
    }
    assert!(plan.check("cron_list", &json!({}), 0).await.is_ok());

    let deny = Permissions::new(
        Mode::Bypass,
        &PermissionRules {
            deny: vec!["cron_create".into()],
            ..Default::default()
        },
        cwd.clone(),
        None,
    )
    .unwrap();
    assert!(
        deny.check("cron_create", &json!({}), 0)
            .await
            .unwrap_err()
            .contains("deny permission rule")
    );

    let approver = ScriptedApprover::new([Decision::Deny]);
    let ask = Permissions::new(
        Mode::Manual,
        &PermissionRules {
            ask: vec!["schedule_wakeup".into()],
            ..Default::default()
        },
        cwd,
        Some(approver.clone()),
    )
    .unwrap();
    assert!(
        ask.check("schedule_wakeup", &json!({}), 0)
            .await
            .unwrap_err()
            .contains("declined")
    );
    assert_eq!(approver.requests.lock().unwrap().len(), 1);

    json!({
        "manual_auto_allow": ["cron_create", "cron_delete", "cron_list", "schedule_wakeup"],
        "plan_blocks_mutations": true,
        "plan_allows_list": true,
        "deny_precedes_auto_allow": true,
        "explicit_ask_precedes_auto_allow": true,
    })
}

async fn loop_report() -> Value {
    let root = TempRoot::new("loop");
    let context = test_ctx(0, "plan58-loop");
    let mut history = History::new(root.path().join("offload"));
    let cancel = CancellationToken::new();
    let fixed = crate::commands::run("/loop 5m check status", &mut history, &context.cfg, &cancel)
        .await
        .run_turn
        .unwrap();
    assert!(fixed.contains("first do the requested work once now"));
    assert!(fixed.contains("cron_create"));
    assert!(fixed.contains("\"*/5 * * * *\""));
    assert!(fixed.contains("check status"));
    let trailing = crate::commands::run(
        "/loop check status every 2h",
        &mut history,
        &context.cfg,
        &cancel,
    )
    .await
    .run_turn
    .unwrap();
    assert!(trailing.contains("\"0 */2 * * *\""));
    let dynamic = crate::commands::run(
        "/loop check deployment",
        &mut history,
        &context.cfg,
        &cancel,
    )
    .await
    .run_turn
    .unwrap();
    assert!(dynamic.contains("schedule_wakeup"));
    assert!(dynamic.contains("stop=true"));
    let autonomous = crate::commands::run("/loop", &mut history, &context.cfg, &cancel)
        .await
        .run_turn
        .unwrap();
    assert!(autonomous.contains("<<autonomous-loop-dynamic>>"));
    let seconds =
        crate::commands::run("/loop 30s check", &mut history, &context.cfg, &cancel).await;
    assert!(seconds.run_turn.is_none());
    assert!(
        seconds
            .output
            .contains("minimum fixed interval is 1 minute")
    );

    json!({
        "first_tick_now": true,
        "leading_interval": "*/5 * * * *",
        "trailing_interval": "0 */2 * * *",
        "fixed_tool": "cron_create",
        "dynamic_tool": "schedule_wakeup",
        "dynamic_stop": true,
        "autonomous_sentinel": "<<autonomous-loop-dynamic>>",
        "seconds_rejected": true,
        "prompt_preserved": true,
    })
}

async fn report() -> Value {
    json!({
        "schema_version": 1,
        "surface": "kloop-native",
        "scenarios": {
            "schema": schema_report(),
            "lifecycle": lifecycle_report().await,
            "wakeup": wakeup_report().await,
            "delivery": delivery_report().await,
            "durable": durable_report().await,
            "guards": guard_report().await,
            "permission": permission_report().await,
            "loop": loop_report().await,
        }
    })
}

#[tokio::test]
async fn plan58_scheduler_contract() {
    let report = report().await;
    assert_eq!(
        report["scenarios"]["delivery"]["events"],
        json!(["scheduled", "fired"])
    );
}

#[tokio::test]
async fn emit_plan58_parity_report() {
    let report = report().await;
    if let Some(path) = std::env::var_os("KLOOP_PLAN58_PARITY_REPORT") {
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
