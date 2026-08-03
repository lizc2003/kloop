//! Standalone, always-background Workflow orchestration.

use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use serde::Deserialize;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::background_tasks::TaskStatus;
use super::codemode::journal::agent_call_key;
use super::codemode::journal::Claim;
use super::codemode::journal::Journal;
use super::run_store::RunDir;
use super::run_store::RunId;
use super::run_store::RunLease;
use super::run_store::RunNamespace;
use super::run_store::RunStore;
use super::ToolCtx;
use crate::event::BackgroundTask;
use crate::event::BackgroundTaskKind;
use crate::event::BackgroundTaskStatus;
use crate::event::Event;
use crate::inbox::InboxItem;
use kloop_codemode::BoxFuture;
use kloop_codemode::HostBridge;
use kloop_codemode::PreparedWorkflow;
use kloop_protocol::ToolDef;

const MAX_REINJECT_CHARS: usize = 8_000;
static WORKFLOW_SEQ: AtomicUsize = AtomicUsize::new(1);
static WORKFLOW_RUN_SEQ: AtomicUsize = AtomicUsize::new(1);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowInput {
    #[serde(default)]
    script: Option<String>,
    #[serde(default)]
    args: Option<Value>,
    #[serde(default)]
    script_path: Option<String>,
    #[serde(default)]
    resume_from_run_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

pub(super) fn workflow_def() -> ToolDef {
    ToolDef {
        name: "workflow".into(),
        description: "Run a deterministic JavaScript Workflow in the background. Use only when the user explicitly requested multi-agent orchestration. The script must begin with `export const meta = { name, description, phases }`; its body can use args, agent(), log(), phase(), parallel(), and pipeline(). Workflow scripts have no tools object, filesystem, network, process, imports, Date, or randomness. The tool returns task/run IDs immediately; the result is persisted and delivered at a later step boundary. Structured agent schemas are supported by the internal structured_output protocol.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "script": {"type": "string", "maxLength": 524288},
                "args": {},
                "script_path": {"type": "string", "description": "Managed script path returned by a previous workflow launch; use with resume_from_run_id."},
                "resume_from_run_id": {"type": "string", "pattern": "^wf_[A-Za-z0-9_-]+$"},
                "name": {"type": "string", "description": "Reserved; named workflow registry is not implemented."},
                "description": {"type": "string", "description": "Ignored; set meta.description in the script."},
                "title": {"type": "string", "description": "Ignored; set meta.name in the script."}
            },
            "additionalProperties": false
        }),
    }
}

pub(super) async fn workflow_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    if ctx.depth != 0 || !ctx.cfg.surface.workflow {
        bail!("workflow: only an enabled top-level session can launch workflows");
    }
    let parsed: WorkflowInput =
        serde_json::from_value(input.clone()).context("workflow: invalid input")?;
    if parsed.name.is_some() {
        bail!("workflow: named workflow registry is not implemented; pass script instead");
    }
    let _ignored_display_fields = (&parsed.description, &parsed.title);
    if parsed.script.is_some() && parsed.script_path.is_some() {
        bail!("workflow: pass only one of script or script_path");
    }
    if parsed.script_path.is_some() && parsed.resume_from_run_id.is_none() {
        bail!("workflow: script_path is accepted only with resume_from_run_id");
    }

    let store = RunStore::new(&ctx.cfg.offload_dir, RunNamespace::Workflow)
        .context("workflow: cannot open run store")?;
    let (run_id, run_dir, source, args) = if let Some(raw) = parsed.resume_from_run_id.as_deref() {
        if !raw.starts_with("wf_") || raw.len() == 3 {
            bail!("workflow: resume_from_run_id must start with wf_");
        }
        let run_id = RunId::parse(raw).context("workflow: invalid resume_from_run_id")?;
        let run_dir = store
            .open(&run_id)
            .with_context(|| format!("workflow: cannot resume {raw}"))?;
        let source = match (parsed.script, parsed.script_path) {
            (Some(script), None) => script,
            (None, Some(path)) => read_managed_script(&run_dir, &path)?,
            (None, None) => String::from_utf8(run_dir.read("script.js")?)
                .context("workflow: stored script is not UTF-8")?,
            (Some(_), Some(_)) => unreachable!("checked above"),
        };
        let args = match parsed.args {
            Some(args) => args,
            None => serde_json::from_slice(&run_dir.read("args.json")?)
                .context("workflow: stored args are invalid")?,
        };
        kloop_codemode::prepare_workflow_args(&args)?;
        (run_id, run_dir, source, args)
    } else {
        let source = parsed
            .script
            .ok_or_else(|| anyhow!("workflow: a new run requires script"))?;
        let args = parsed.args.unwrap_or(Value::Null);
        // Validate before creating any task or durable run state.
        kloop_codemode::prepare_workflow(&source)?;
        kloop_codemode::prepare_workflow_args(&args)?;
        let run_id = RunId::parse(&new_run_id()).expect("generated Workflow run id is valid");
        let run_dir = store
            .create(&run_id)
            .context("workflow: cannot create run")?;
        (run_id, run_dir, source, args)
    };
    let prepared = kloop_codemode::prepare_workflow(&source)?;
    let lease = run_dir
        .acquire()
        .context("workflow: run is already active")?;
    persist_inputs(&run_dir, &prepared, &source, &args)?;
    let journal = Arc::new(Journal::open_run(run_dir.clone(), "journal.jsonl"));
    launch_workflow(ctx, run_id, run_dir, lease, prepared, args, journal)
}

fn read_managed_script(run_dir: &RunDir, supplied: &str) -> Result<String> {
    let expected = run_dir.file_path("script.js")?;
    let supplied = std::fs::canonicalize(supplied)
        .with_context(|| format!("workflow: cannot resolve script_path {supplied}"))?;
    let expected =
        std::fs::canonicalize(&expected).context("workflow: cannot resolve managed script path")?;
    if supplied != expected {
        bail!("workflow: script_path must be the managed script for resume_from_run_id");
    }
    String::from_utf8(run_dir.read("script.js")?).context("workflow: script is not UTF-8")
}

fn persist_inputs(
    run_dir: &RunDir,
    prepared: &PreparedWorkflow,
    source: &str,
    args: &Value,
) -> Result<()> {
    run_dir.write_atomic("script.js", source.as_bytes())?;
    run_dir.write_atomic("args.json", &serde_json::to_vec_pretty(args)?)?;
    run_dir.write_atomic(
        "manifest.json",
        &serde_json::to_vec_pretty(&json!({
            "version": 1,
            "runId": run_dir.id().as_str(),
            "meta": prepared.meta,
        }))?,
    )?;
    Ok(())
}

fn new_run_id() -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!(
        "wf_{seconds}-{}",
        WORKFLOW_RUN_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

fn launch_workflow(
    ctx: &ToolCtx,
    run_id: RunId,
    run_dir: RunDir,
    lease: RunLease,
    prepared: PreparedWorkflow,
    args: Value,
    journal: Arc<Journal>,
) -> Result<String> {
    let output_script = run_dir.file_path("script.js")?;
    let run_id_text = run_id.as_str().to_string();
    let task_id = format!("workflow-{}", WORKFLOW_SEQ.fetch_add(1, Ordering::Relaxed));
    let cancel = CancellationToken::new();
    ctx.cfg
        .background_tasks
        .register(&task_id, &prepared.meta.description, cancel.clone())
        .map_err(|error| anyhow!("workflow: {error}"))?;
    let ui = ctx.ui.clone();
    let background_tasks = ctx.cfg.background_tasks.clone();
    let parent_inbox = ctx.cfg.inbox.clone();
    let limits = ctx.cfg.program_limits;
    let mut workflow_ctx = ctx.clone();
    workflow_ctx.cancel = cancel.clone();
    let description = prepared.meta.description.clone();
    let bridge = Arc::new(WorkflowBridge::new(
        workflow_ctx,
        limits,
        journal,
        task_id.clone(),
        run_id_text.clone(),
        description.clone(),
    ));
    emit_workflow(
        &ui,
        &task_id,
        &run_id_text,
        &description,
        TaskStatus::Running,
        None,
        None,
    );

    let worker_cancel = cancel.clone();
    let worker = tokio::spawn(async move {
        let _lease = lease;
        kloop_codemode::run_workflow(&prepared, &args, bridge, worker_cancel, limits).await
    });
    background_tasks.attach_abort(&task_id, worker.abort_handle());
    let task_id_for_supervisor = task_id.clone();
    let supervisor_run_id = run_id_text.clone();
    let supervisor_description = description.clone();
    tokio::spawn(async move {
        let (observed, summary, output_path) = match worker.await {
            Ok(Ok(value)) => match run_dir.write_atomic(
                "result.json",
                &serde_json::to_vec_pretty(&value).unwrap_or_default(),
            ) {
                Ok(path) => (
                    TaskStatus::Completed,
                    truncate(&value.to_string(), MAX_REINJECT_CHARS),
                    path,
                ),
                Err(error) => persist_error(&run_dir, format!("cannot persist result: {error:#}")),
            },
            Ok(Err(_error)) if cancel.is_cancelled() => (
                TaskStatus::Aborted,
                String::new(),
                run_dir.path().to_path_buf(),
            ),
            Ok(Err(error)) => persist_error(&run_dir, format!("{error:#}")),
            Err(error) if error.is_cancelled() => (
                TaskStatus::Aborted,
                String::new(),
                run_dir.path().to_path_buf(),
            ),
            Err(error) => persist_error(&run_dir, format!("workflow task panicked: {error}")),
        };
        let terminal =
            background_tasks.finish(&task_id_for_supervisor, observed, |actual, deliver| {
                if deliver && actual != TaskStatus::Aborted {
                    parent_inbox.push(InboxItem::WorkflowResult {
                        task_id: task_id_for_supervisor.clone(),
                        run_id: supervisor_run_id.clone(),
                        summary: summary.clone(),
                        output_path: output_path.to_string_lossy().to_string(),
                    });
                } else {
                    parent_inbox.notify_activity();
                }
            });
        if let Some(terminal) = terminal {
            emit_workflow(
                &ui,
                &task_id_for_supervisor,
                &supervisor_run_id,
                &supervisor_description,
                terminal,
                Some(output_path.to_string_lossy().to_string()),
                task_status_detail(terminal),
            );
        }
    });

    Ok(format!(
        "Workflow launched in background. Task ID: {task_id}\nSummary: {}\nScript file: {}\nRun ID: {}\nTo resume after editing the managed script, call workflow with script_path and resume_from_run_id.\n\nYou will be notified when it completes; use wait or stop_agent with the task ID.",
        description,
        output_script.display(),
        run_id_text,
    ))
}

fn persist_error(run_dir: &RunDir, error: String) -> (TaskStatus, String, std::path::PathBuf) {
    let summary = truncate(&error, MAX_REINJECT_CHARS);
    match run_dir.write_atomic("error.txt", error.as_bytes()) {
        Ok(path) => (TaskStatus::Failed, summary, path),
        Err(_) => (TaskStatus::Failed, summary, run_dir.path().to_path_buf()),
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let prefix: String = text.chars().take(max).collect();
    format!("{prefix}… (truncated; see output file)")
}

fn task_status_detail(status: TaskStatus) -> Option<String> {
    match status {
        TaskStatus::Running => None,
        TaskStatus::Completed => Some("completed".into()),
        TaskStatus::Failed => Some("failed".into()),
        TaskStatus::MaxRounds => Some("stopped at round limit".into()),
        TaskStatus::Aborted => Some("stopped".into()),
    }
}

fn emit_workflow(
    ui: &Arc<dyn crate::agent::Ui>,
    id: &str,
    run_id: &str,
    description: &str,
    status: TaskStatus,
    output_path: Option<String>,
    detail: Option<String>,
) {
    let status = match status {
        TaskStatus::Running => BackgroundTaskStatus::Running,
        TaskStatus::Completed | TaskStatus::MaxRounds => BackgroundTaskStatus::Completed,
        TaskStatus::Failed => BackgroundTaskStatus::Failed,
        TaskStatus::Aborted => BackgroundTaskStatus::Cancelled,
    };
    ui.emit(&Event::BackgroundTaskUpdated(BackgroundTask {
        id: id.to_string(),
        run_id: Some(run_id.to_string()),
        kind: BackgroundTaskKind::Workflow,
        description: description.to_string(),
        status,
        output_path,
        detail,
    }));
}

struct WorkflowBridge {
    ctx: ToolCtx,
    agent_count: AtomicU64,
    max_agents: u64,
    journal: Arc<Journal>,
    task_id: String,
    run_id: String,
    description: String,
}

impl WorkflowBridge {
    fn new(
        ctx: ToolCtx,
        limits: kloop_codemode::Limits,
        journal: Arc<Journal>,
        task_id: String,
        run_id: String,
        description: String,
    ) -> Self {
        Self {
            ctx,
            agent_count: AtomicU64::new(0),
            max_agents: limits.max_agents,
            journal,
            task_id,
            run_id,
            description,
        }
    }
}

impl HostBridge for WorkflowBridge {
    fn call_tool(&self, _name: String, _args: Value) -> BoxFuture<Result<Value, String>> {
        Box::pin(async { Err("Workflow scripts cannot call tools directly".into()) })
    }

    fn call_agent(
        &self,
        seq: u32,
        prompt: String,
        opts: Value,
    ) -> BoxFuture<Result<Value, String>> {
        let key = agent_call_key(&prompt, &opts);
        let claim = self.journal.claim(seq, &key);
        let live = matches!(claim, Claim::Miss);
        let count = live.then(|| self.agent_count.fetch_add(1, Ordering::Relaxed));
        let max = self.max_agents;
        let ctx = self.ctx.clone();
        let journal = self.journal.clone();
        Box::pin(async move {
            if let Claim::Hit(value) = claim {
                return Ok(value);
            }
            if count.is_some_and(|count| count >= max) {
                return Err(format!("workflow exceeds the agent cap of {max}"));
            }
            let schema = opts.get("schema").cloned();
            let mut input = json!({"prompt": prompt});
            copy_option(&opts, &mut input, "agent_type", "agent_type");
            copy_option(&opts, &mut input, "agentType", "agent_type");
            copy_option(&opts, &mut input, "max_rounds", "max_rounds");
            copy_option(&opts, &mut input, "maxRounds", "max_rounds");
            copy_option(&opts, &mut input, "isolation", "isolation");
            copy_option(&opts, &mut input, "model", "model");
            let result = match schema {
                Some(schema) => super::task::structured_task(&input, schema, &ctx)
                    .await
                    .map_err(|error| format!("{error:#}")),
                None => super::task::task_tool(&input, &ctx)
                    .await
                    .map(Value::String)
                    .map_err(|error| format!("{error:#}")),
            };
            if let Ok(value) = &result {
                journal.record(seq, key, value.clone());
            }
            result
        })
    }

    fn log(&self, message: String) {
        self.ctx.ui.emit(&Event::Note(message));
    }

    fn phase(&self, title: String) {
        emit_workflow(
            &self.ctx.ui,
            &self.task_id,
            &self.run_id,
            &self.description,
            TaskStatus::Running,
            None,
            Some(title),
        );
    }
}

fn copy_option(opts: &Value, output: &mut Value, source: &str, target: &str) {
    if let Some(value) = opts.get(source) {
        output[target] = value.clone();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;

    fn enabled_ctx(tag: &str) -> ToolCtx {
        let mut ctx = test_ctx(0, tag);
        let mut cfg = (*ctx.cfg).clone();
        cfg.surface.workflow = true;
        let root =
            std::env::temp_dir().join(format!("kloop-workflow-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        cfg.offload_dir = root.join("offload");
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    #[derive(Default)]
    struct RecordingUi {
        events: std::sync::Mutex<Vec<Event>>,
    }

    impl crate::agent::Ui for RecordingUi {
        fn emit(&self, event: &Event) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    impl RecordingUi {
        fn background(&self) -> Vec<BackgroundTask> {
            self.events
                .lock()
                .unwrap()
                .iter()
                .filter_map(|event| match event {
                    Event::BackgroundTaskUpdated(task) => Some(task.clone()),
                    _ => None,
                })
                .collect()
        }
    }

    async fn wait_idle(ctx: &ToolCtx) {
        for _ in 0..400 {
            if ctx.cfg.background_tasks.running_count() == 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        panic!("Workflow did not reach a terminal state");
    }

    fn launch_value<'a>(launched: &'a str, prefix: &str) -> &'a str {
        launched
            .lines()
            .find_map(|line| line.split_once(prefix).map(|(_, value)| value))
            .unwrap_or_else(|| panic!("missing {prefix:?} in launch result: {launched}"))
    }

    #[test]
    fn definition_is_standalone_and_strict() {
        let def = workflow_def();
        assert_eq!(def.name, "workflow");
        assert_eq!(def.schema["additionalProperties"], false);
        assert!(!def.description.contains("tools."));
    }

    #[test]
    fn registration_is_surface_and_depth_gated() {
        let enabled = crate::config::SurfaceCapabilities {
            workflow: true,
            ..Default::default()
        };
        assert!(super::super::all_tool_defs(0, &[], 30, enabled)
            .iter()
            .any(|definition| definition.name == "workflow"));
        assert!(!super::super::all_tool_defs(1, &[], 30, enabled)
            .iter()
            .any(|definition| definition.name == "workflow"));
        assert!(!super::super::all_tool_defs(0, &[], 30, Default::default())
            .iter()
            .any(|definition| definition.name == "workflow"));
    }

    #[tokio::test]
    async fn invalid_script_fails_before_registering_a_task() {
        let ctx = enabled_ctx("workflow-invalid");
        let error = workflow_tool(&json!({"script": "return 1"}), &ctx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("meta"), "{error:#}");
        assert_eq!(ctx.cfg.background_tasks.running_count(), 0);
    }

    #[tokio::test]
    async fn invalid_inputs_and_oversized_args_never_register_tasks() {
        let mut ctx = enabled_ctx("workflow-invalid-inputs");
        let ui = Arc::new(RecordingUi::default());
        ctx.ui = ui.clone();
        let valid = "export const meta = { name: 'valid', description: 'valid' }; return null;";
        let cases = vec![
            json!({"script": valid, "script_path": "/tmp/script.js"}),
            json!({"script_path": "/tmp/script.js"}),
            json!({}),
            json!({"name": "saved"}),
            json!({"script": valid, "unexpected": true}),
            json!({"resume_from_run_id": "program_1"}),
            json!({"resume_from_run_id": "../wf_1"}),
            json!({"resume_from_run_id": "wf_missing"}),
        ];
        for input in cases {
            assert!(
                workflow_tool(&input, &ctx).await.is_err(),
                "accepted {input}"
            );
            assert_eq!(ctx.cfg.background_tasks.running_count(), 0);
            assert!(ctx.cfg.inbox.is_empty());
        }

        let error = workflow_tool(
            &json!({
                "script": valid,
                "args": {"blob": "x".repeat(512 * 1024)}
            }),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("args exceeds"), "{error:#}");
        assert_eq!(ctx.cfg.background_tasks.running_count(), 0);
        assert!(ui.background().is_empty());
    }

    #[tokio::test]
    async fn workflow_launches_immediately_persists_and_reinjects() {
        let mut ctx = enabled_ctx("workflow-launch");
        let ui = Arc::new(RecordingUi::default());
        ctx.ui = ui.clone();
        let launched = workflow_tool(
            &json!({
                "script": "export const meta = { name: 'minimal', description: 'return marker', phases: [{ title: 'Run' }] }; phase('Run'); return { marker: 'ok', args };",
                "args": {"value": 53}
            }),
            &ctx,
        )
        .await
        .unwrap();
        assert!(launched.contains("Workflow launched in background"));
        let launched_task_id = launch_value(&launched, "Task ID: ").to_string();
        let launched_run_id = launch_value(&launched, "Run ID: ").to_string();
        assert!(launched_run_id.starts_with("wf_"));
        assert_eq!(ctx.cfg.background_tasks.running_count(), 1);

        wait_idle(&ctx).await;
        let items = ctx.cfg.inbox.drain();
        let [InboxItem::WorkflowResult {
            task_id,
            run_id,
            summary,
            output_path,
        }] = items.as_slice()
        else {
            panic!("expected one Workflow result: {items:?}");
        };
        assert_eq!(task_id, &launched_task_id);
        assert_eq!(run_id, &launched_run_id);
        assert!(summary.contains("\"marker\":\"ok\""));
        let persisted: Value =
            serde_json::from_slice(&std::fs::read(output_path).unwrap()).unwrap();
        assert_eq!(persisted, json!({"marker": "ok", "args": {"value": 53}}));
        let run_dir = std::path::Path::new(output_path).parent().unwrap();
        let manifest: Value =
            serde_json::from_slice(&std::fs::read(run_dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest["version"], 1);
        assert_eq!(manifest["runId"], launched_run_id);

        let events = ui.background();
        assert!(events.len() >= 3, "{events:?}");
        assert!(events.iter().all(|event| event.id == launched_task_id));
        assert!(events
            .iter()
            .all(|event| event.run_id.as_deref() == Some(launched_run_id.as_str())));
        assert!(events
            .iter()
            .all(|event| event.description == "return marker"));
        assert_eq!(
            events.first().unwrap().status,
            BackgroundTaskStatus::Running
        );
        assert!(events.iter().any(|event| {
            event.status == BackgroundTaskStatus::Running && event.detail.as_deref() == Some("Run")
        }));
        assert_eq!(
            events.last().unwrap().status,
            BackgroundTaskStatus::Completed
        );
        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[tokio::test]
    async fn workflow_failure_persists_error_and_emits_one_terminal_event() {
        let mut ctx = enabled_ctx("workflow-failure");
        let ui = Arc::new(RecordingUi::default());
        ctx.ui = ui.clone();
        let launched = workflow_tool(
            &json!({
                "script": "export const meta = { name: 'failure', description: 'expected failure' }; phase('Failing'); throw new Error('boom-53');"
            }),
            &ctx,
        )
        .await
        .unwrap();
        let task_id = launch_value(&launched, "Task ID: ").to_string();
        let run_id = launch_value(&launched, "Run ID: ").to_string();
        wait_idle(&ctx).await;

        let items = ctx.cfg.inbox.drain();
        let [InboxItem::WorkflowResult {
            task_id: inbox_task,
            run_id: inbox_run,
            summary,
            output_path,
        }] = items.as_slice()
        else {
            panic!("expected failed Workflow result: {items:?}");
        };
        assert_eq!(inbox_task, &task_id);
        assert_eq!(inbox_run, &run_id);
        assert!(summary.contains("boom-53"), "{summary}");
        assert!(output_path.ends_with("error.txt"), "{output_path}");
        assert!(std::fs::read_to_string(output_path)
            .unwrap()
            .contains("boom-53"));

        let events = ui.background();
        assert!(events.iter().all(|event| event.id == task_id));
        assert!(events
            .iter()
            .all(|event| event.run_id.as_deref() == Some(run_id.as_str())));
        assert!(events
            .iter()
            .all(|event| event.description == "expected failure"));
        assert_eq!(
            events
                .iter()
                .filter(|event| event.status != BackgroundTaskStatus::Running)
                .count(),
            1
        );
        assert_eq!(events.last().unwrap().status, BackgroundTaskStatus::Failed);
        let run_dir = std::path::Path::new(output_path).parent().unwrap();
        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[tokio::test]
    async fn stopped_workflow_has_one_cancelled_terminal_and_no_inbox_result() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut ctx = enabled_ctx("workflow-stop");
        let ui = Arc::new(RecordingUi::default());
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(kloop_provider::Provider::mock_scripted(vec![
            kloop_provider::MockTurn::Gate {
                started: started_tx,
                release: release_rx,
                blocks: vec![kloop_protocol::ContentBlock::Text {
                    text: "too late".into(),
                }],
            },
        ]));
        ctx.cfg = Arc::new(cfg);
        ctx.ui = ui.clone();
        let launched = workflow_tool(
            &json!({
                "script": "export const meta = { name: 'stop', description: 'blocked child' }; return await agent('wait');"
            }),
            &ctx,
        )
        .await
        .unwrap();
        let task_id = launch_value(&launched, "Task ID: ").to_string();
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("child sampling did not start")
            .expect("child sampling gate dropped");
        crate::tools::background_tasks::stop_agent_tool(&json!({"agent_id": task_id}), &ctx)
            .await
            .unwrap();
        let _ = release_tx.send(());
        wait_idle(&ctx).await;

        assert!(ctx.cfg.inbox.is_empty());
        let events = ui.background();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.status == BackgroundTaskStatus::Cancelled)
                .count(),
            1
        );
        assert!(!events
            .iter()
            .any(|event| event.status == BackgroundTaskStatus::Completed));
        if let Some(path) = launched
            .lines()
            .find_map(|line| line.strip_prefix("Script file: "))
            .and_then(|path| std::path::Path::new(path).parent())
        {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[tokio::test]
    async fn shutdown_cancels_workflow_without_late_delivery() {
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut ctx = enabled_ctx("workflow-shutdown");
        let ui = Arc::new(RecordingUi::default());
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(kloop_provider::Provider::mock_scripted(vec![
            kloop_provider::MockTurn::Gate {
                started: started_tx,
                release: release_rx,
                blocks: vec![kloop_protocol::ContentBlock::Text {
                    text: "too late".into(),
                }],
            },
        ]));
        ctx.cfg = Arc::new(cfg);
        ctx.ui = ui.clone();
        let launched = workflow_tool(
            &json!({
                "script": "export const meta = { name: 'shutdown', description: 'shutdown child' }; return await agent('wait');"
            }),
            &ctx,
        )
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
            .await
            .expect("child sampling did not start")
            .expect("child sampling gate dropped");
        assert_eq!(
            ctx.cfg
                .background_tasks
                .shutdown(std::time::Duration::from_secs(1))
                .await,
            0
        );
        let _ = release_tx.send(());
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(ctx.cfg.inbox.is_empty());
        let events = ui.background();
        assert_eq!(
            events
                .iter()
                .filter(|event| event.status == BackgroundTaskStatus::Cancelled)
                .count(),
            1
        );
        assert!(!events
            .iter()
            .any(|event| event.status == BackgroundTaskStatus::Completed));
        let script_path = launch_value(&launched, "Script file: ");
        if let Some(path) = std::path::Path::new(script_path).parent() {
            let _ = std::fs::remove_dir_all(path);
        }
    }

    #[tokio::test]
    async fn resume_replays_json_objects_and_misses_after_script_edit() {
        let script = "export const meta = { name: 'resume', description: 'resume object' }; const value = await agent('return count', { schema: { type: 'object', properties: { count: { type: 'integer' } }, required: ['count'], additionalProperties: false } }); return { got: value.count };";
        let changed_script = "export const meta = { name: 'resume', description: 'resume object' }; const value = await agent('return changed count', { schema: { type: 'object', properties: { count: { type: 'integer' } }, required: ['count'], additionalProperties: false } }); return { got: value.count };";
        let mut ctx = enabled_ctx("workflow-resume");
        let (provider, first_seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![kloop_protocol::ContentBlock::ToolUse {
                id: "structured-first".into(),
                name: "structured_output".into(),
                input: json!({"count": 7}),
            }]),
        ]);
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(provider);
        ctx.cfg = Arc::new(cfg);
        let launched = workflow_tool(&json!({"script": script}), &ctx)
            .await
            .unwrap();
        let run_id = launch_value(&launched, "Run ID: ").to_string();
        let script_path = launch_value(&launched, "Script file: ").to_string();
        wait_idle(&ctx).await;
        assert_eq!(first_seen.lock().unwrap().len(), 1);
        let first = ctx.cfg.inbox.drain();
        assert!(matches!(
            first.as_slice(),
            [InboxItem::WorkflowResult { summary, .. }] if summary.contains("\"got\":7")
        ));

        let (provider, hit_seen) = kloop_provider::Provider::mock_recording(Vec::new());
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(provider);
        ctx.cfg = Arc::new(cfg);
        workflow_tool(
            &json!({
                "script_path": script_path,
                "resume_from_run_id": run_id
            }),
            &ctx,
        )
        .await
        .unwrap();
        wait_idle(&ctx).await;
        assert!(
            hit_seen.lock().unwrap().is_empty(),
            "cache hit sampled provider"
        );
        let hit = ctx.cfg.inbox.drain();
        assert!(matches!(
            hit.as_slice(),
            [InboxItem::WorkflowResult { summary, .. }] if summary.contains("\"got\":7")
        ));

        let error = workflow_tool(
            &json!({
                "script_path": script_path,
                "resume_from_run_id": run_id,
                "args": {"blob": "x".repeat(512 * 1024)}
            }),
            &ctx,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("args exceeds"), "{error:#}");
        assert_eq!(ctx.cfg.background_tasks.running_count(), 0);

        std::fs::write(&script_path, changed_script).unwrap();
        let (provider, miss_seen) = kloop_provider::Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![kloop_protocol::ContentBlock::ToolUse {
                id: "structured-changed".into(),
                name: "structured_output".into(),
                input: json!({"count": 9}),
            }]),
        ]);
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(provider);
        ctx.cfg = Arc::new(cfg);
        workflow_tool(
            &json!({
                "script_path": script_path,
                "resume_from_run_id": run_id
            }),
            &ctx,
        )
        .await
        .unwrap();
        wait_idle(&ctx).await;
        assert_eq!(miss_seen.lock().unwrap().len(), 1);
        let miss = ctx.cfg.inbox.drain();
        let [InboxItem::WorkflowResult {
            summary,
            output_path,
            ..
        }] = miss.as_slice()
        else {
            panic!("expected resumed Workflow result: {miss:?}");
        };
        assert!(summary.contains("\"got\":9"), "{summary}");
        let run_dir = std::path::Path::new(output_path).parent().unwrap();
        let _ = std::fs::remove_dir_all(run_dir);
    }

    #[tokio::test]
    async fn workflow_structured_agent_returns_object_and_journals_value() {
        let mut ctx = enabled_ctx("workflow-structured");
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(kloop_provider::Provider::mock(vec![vec![
            kloop_protocol::ContentBlock::ToolUse {
                id: "structured-1".into(),
                name: "structured_output".into(),
                input: json!({"count": 7}),
            },
        ]]));
        ctx.cfg = Arc::new(cfg);
        workflow_tool(
            &json!({
                "script": "export const meta = { name: 'structured', description: 'structured child' }; const value = await agent('return count', { schema: { type: 'object', properties: { count: { type: 'integer' } }, required: ['count'], additionalProperties: false } }); return { got: value.count };"
            }),
            &ctx,
        )
        .await
        .unwrap();
        for _ in 0..400 {
            if ctx.cfg.background_tasks.running_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let items = ctx.cfg.inbox.drain();
        let [InboxItem::WorkflowResult {
            summary,
            output_path,
            ..
        }] = items.as_slice()
        else {
            panic!("expected structured Workflow result: {items:?}");
        };
        assert!(summary.contains("\"got\":7"), "{summary}");
        let run_dir = std::path::Path::new(output_path).parent().unwrap();
        let journal = std::fs::read_to_string(run_dir.join("journal.jsonl")).unwrap();
        assert!(journal.contains("\"result\":{\"count\":7}"), "{journal}");
        let _ = std::fs::remove_dir_all(run_dir);
    }
}
