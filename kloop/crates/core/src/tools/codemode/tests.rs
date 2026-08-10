use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;

use crate::agent::Ui;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use crate::permissions::Mode;
use crate::permissions::PermissionRules;
use crate::permissions::Permissions;
#[cfg(windows)]
use crate::tools::testutil::assert_powershell_done;
#[cfg(windows)]
use crate::tools::testutil::powershell_output_is_done_and_successful;
use crate::tools::testutil::run_tool;
use crate::tools::testutil::test_ctx;
use crate::tools::testutil::test_ctx_with_sources;
use crate::tools::testutil::with_defer_threshold;
#[cfg(windows)]
use crate::tools::testutil::with_powershell_gate_probe;
use crate::tools::testutil::with_provider;
use crate::tools::ToolCtx;
use crate::tools::ToolSource;
use kloop_protocol::ToolDef;

use super::*;

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("kloop-codemode-{}-{}", std::process::id(), name))
}

/// Records the UI signals a running program emits, so tests can assert what the
/// user actually sees while the program executes.
#[derive(Default)]
struct RecordUi(Mutex<Vec<String>>);
impl RecordUi {
    fn events(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }

    fn background_events(&self) -> Vec<String> {
        self.events()
            .into_iter()
            .filter(|event| event.starts_with("background "))
            .collect()
    }
}

#[derive(Default)]
struct BackgroundTaskUi(Mutex<Vec<crate::event::BackgroundTask>>);
impl Ui for BackgroundTaskUi {
    fn emit(&self, event: &Event) {
        if let Event::BackgroundTaskUpdated(task) = event {
            self.0.lock().unwrap().push(task.clone());
        }
    }
}

impl Ui for RecordUi {
    fn emit(&self, ev: &Event) {
        match ev {
            Event::Note(s) => self.0.lock().unwrap().push(format!("note: {s}")),
            Event::ItemStarted {
                item: Item::ToolCall { name, .. },
                ..
            } => self.0.lock().unwrap().push(format!("tool_start: {name}")),
            Event::ItemCompleted {
                item: Item::ToolCall { status, .. },
                ..
            } => {
                let ok = *status == ItemStatus::Completed;
                self.0.lock().unwrap().push(format!("tool_end: {ok}"));
            }
            Event::BackgroundTaskUpdated(task) => self
                .0
                .lock()
                .unwrap()
                .push(format!("background {} {:?}", task.id, task.status)),
            _ => {}
        }
    }
}

fn with_ui(mut ctx: ToolCtx, ui: Arc<RecordUi>) -> ToolCtx {
    ctx.ui = ui;
    ctx
}

fn with_permissions(mut ctx: ToolCtx, perms: Permissions) -> ToolCtx {
    let mut cfg = ctx.cfg.test_clone();
    cfg.permissions = Arc::new(perms);
    ctx.cfg = Arc::new(cfg);
    ctx
}

fn with_program_limits(mut ctx: ToolCtx, limits: kloop_codemode::Limits) -> ToolCtx {
    let mut cfg = ctx.cfg.test_clone();
    cfg.program_limits = limits;
    ctx.cfg = Arc::new(cfg);
    ctx
}

async fn run(source: &str, ctx: &ToolCtx) -> (String, bool) {
    run_tool("run_program", json!({ "source": source }), ctx).await
}

fn seed_program_source(ctx: &ToolCtx, run_id: &str, source: &str) {
    let store = RunStore::new(&ctx.cfg.offload_dir, RunNamespace::Program).unwrap();
    let id = RunId::parse(run_id).unwrap();
    let run_dir = store.open(&id).unwrap();
    persist_program_source(&run_dir, source).unwrap();
}

#[tokio::test]
async fn program_reads_a_file_through_the_real_dispatch() {
    let file = tmp("read");
    std::fs::write(&file, "hello codemode").unwrap();
    let ctx = test_ctx(0, "read");
    let (out, is_error) = run(
        &format!(r#"return await tools.read_file({{ path: {:?} }});"#, file),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("hello codemode"), "{out}");
    let _ = std::fs::remove_file(&file);
}

/// The op layer re-enters the same permission gate: a denied tool is refused
/// *inside* the program (the model catches it), while a read-only tool in the
/// very same program passes. This is the whole safety story of code-mode.
#[tokio::test]
async fn tool_calls_pass_the_permission_gate() {
    let readable = tmp("gate-read");
    std::fs::write(&readable, "visible").unwrap();
    let forbidden = tmp("gate-write");
    let _ = std::fs::remove_file(&forbidden);

    let rules = PermissionRules {
        allow: vec![],
        deny: vec!["write_file".into()],
        ask: vec![],
    };
    let perms = Permissions::new(Mode::Manual, &rules, std::env::temp_dir(), None).unwrap();
    let ctx = with_permissions(test_ctx(0, "gate"), perms);

    let (out, is_error) = run(
        &format!(
            r#"let w;
               try {{ await tools.write_file({{ path: {forbidden:?}, content: "x" }}); w = "WROTE"; }}
               catch (e) {{ w = "BLOCKED"; }}
               const r = await tools.read_file({{ path: {readable:?} }});
               return w + ":" + r.includes("visible");"#,
        ),
        &ctx,
    )
    .await;

    assert!(!is_error, "{out}");
    assert_eq!(out, "BLOCKED:true");
    assert!(
        !forbidden.exists(),
        "the denied write must not have touched disk"
    );
    let _ = std::fs::remove_file(&readable);
}

#[tokio::test]
async fn program_cannot_detach_a_background_shell() {
    let ctx = test_ctx(0, "no-detached-shell");
    let (output, is_error) = run(
        r#"try {
               await tools.bash({ command: "sleep 30", background: true });
               return "RAN";
           } catch (error) {
               return error.message;
           }"#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{output}");
    assert!(output.contains("must stay foreground"), "{output}");
    assert_eq!(ctx.cfg.background_shells.running_count(), 0);
}

/// `agent()` reuses the run_agent seam, spawning a real sub-agent that samples the
/// (scripted) provider and returns its final text.
#[tokio::test]
async fn agent_call_spawns_a_subagent() {
    let provider =
        kloop_provider::Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
            text: "sub-agent result".into(),
        }]]);
    let ctx = with_provider(test_ctx(0, "agent"), provider);
    let (out, is_error) = run(r#"return await agent("do the thing");"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "sub-agent result");
}

/// The agent cap refuses runaway sub-agent fan-out: with max_agents=2 the third
/// agent() call throws (caught in-program here) before it can spawn, so only two
/// sub-agents ever run.
#[tokio::test]
async fn agent_cap_refuses_runaway_fanout() {
    let text = |t: &str| vec![kloop_protocol::AssistantBlock::Text { text: t.into() }];
    let provider = kloop_provider::Provider::mock(vec![text("one"), text("two")]);
    let ctx = with_program_limits(
        with_provider(test_ctx(0, "agentcap"), provider),
        kloop_codemode::Limits {
            max_agents: 2,
            ..Default::default()
        },
    );
    let (out, is_error) = run(
        r#"
        const r = [];
        for (let i = 0; i < 3; i++) {
            try { r.push(await agent("go " + i)); }
            catch (e) { r.push("ERR:" + e.message); }
        }
        return JSON.stringify(r);
        "#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(
        out.contains("one") && out.contains("two"),
        "first two ran: {out}"
    );
    assert!(out.contains("agent cap (2"), "the third hit the cap: {out}");
}

#[tokio::test]
async fn program_agent_concurrency_limit_queues_excess_calls() {
    let (first_started_tx, first_started_rx) = tokio::sync::oneshot::channel();
    let (first_release_tx, first_release_rx) = tokio::sync::oneshot::channel();
    let (second_started_tx, mut second_started_rx) = tokio::sync::oneshot::channel();
    let (second_release_tx, second_release_rx) = tokio::sync::oneshot::channel();
    let provider = kloop_provider::Provider::mock_scripted(vec![
        kloop_provider::MockTurn::Gate {
            started: first_started_tx,
            release: first_release_rx,
            blocks: vec![kloop_protocol::AssistantBlock::Text {
                text: "first".into(),
            }],
        },
        kloop_provider::MockTurn::Gate {
            started: second_started_tx,
            release: second_release_rx,
            blocks: vec![kloop_protocol::AssistantBlock::Text {
                text: "second".into(),
            }],
        },
    ]);
    let ctx = with_program_limits(
        with_provider(test_ctx(0, "program-concurrency-limit"), provider),
        kloop_codemode::Limits {
            max_concurrency: 1,
            ..kloop_codemode::Limits::default()
        },
    );
    let run_ctx = ctx.clone();
    let running = tokio::spawn(async move {
        run_tool(
            "run_program",
            json!({
                "source": "return JSON.stringify(await parallel([(scope) => scope.agent('one'), (scope) => scope.agent('two')]));"
            }),
            &run_ctx,
        )
        .await
    });

    tokio::time::timeout(std::time::Duration::from_secs(2), first_started_rx)
        .await
        .expect("first Program child did not start")
        .expect("first Program child start dropped");
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(100),
            &mut second_started_rx
        )
        .await
        .is_err(),
        "second Program child started before the only slot was released"
    );
    first_release_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), second_started_rx)
        .await
        .expect("second Program child did not start after release")
        .expect("second Program child start dropped");
    second_release_tx.send(()).unwrap();
    let (output, is_error) = running.await.unwrap();
    assert!(!is_error, "{output}");
    assert!(
        output.contains("first") && output.contains("second"),
        "{output}"
    );
}

/// Fire-and-forget: run_program {"background": true} returns a "started" message
/// (NOT the result), and the detached program reinjects its return value into
/// the PARENT's inbox as a framed ProgramResult when it finishes.
#[tokio::test]
async fn background_program_returns_immediately_and_reinjects() {
    use crate::inbox::InboxItem;
    let ui = Arc::new(RecordUi::default());
    let ctx = with_ui(test_ctx(0, "bg-program"), ui.clone());
    let (out, is_error) = run_tool(
        "run_program",
        json!({
            "source": "return 'PROG_DONE';",
            "description": "background smoke",
            "background": true
        }),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.contains("started in the background"), "{out}");
    let program_id = out
        .lines()
        .find_map(|line| line.strip_prefix("Program ID: "))
        .expect("background response must expose program-N");
    let run_id_from_launch = out
        .lines()
        .find_map(|line| line.strip_prefix("Run ID: "))
        .expect("background response must expose durable run-*");
    assert!(program_id.starts_with("program-"), "{out}");
    assert!(run_id_from_launch.starts_with("run-"), "{out}");
    assert_ne!(
        program_id, run_id_from_launch,
        "transient and durable IDs are distinct"
    );
    let stored_source = std::fs::read_to_string(
        ctx.cfg
            .offload_dir
            .parent()
            .unwrap_or(&ctx.cfg.offload_dir)
            .join("program-runs")
            .join(run_id_from_launch)
            .join("source.js"),
    )
    .unwrap();
    assert_eq!(stored_source, "return 'PROG_DONE';");
    assert!(
        !out.contains("PROG_DONE"),
        "the result is NOT returned inline: {out}"
    );

    for _ in 0..300 {
        if !ctx.cfg.inbox.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let items = ctx.cfg.inbox.drain();
    assert_eq!(items.len(), 1, "one reinjected result");
    let label = match &items[0] {
        InboxItem::ProgramResult {
            label,
            run_id,
            summary,
        } => {
            assert!(label.starts_with("program-"), "{label}");
            assert_eq!(run_id, run_id_from_launch);
            assert_eq!(summary, "PROG_DONE");
            label.clone()
        }
        other => panic!("expected ProgramResult, got {other:?}"),
    };
    assert_eq!(
        ctx.cfg.background_executions.running_count(),
        0,
        "slot freed"
    );
    assert_eq!(
        ui.background_events(),
        vec![
            format!("background {label} Running"),
            format!("background {label} Completed"),
        ]
    );
}

#[tokio::test]
async fn background_program_projects_one_description_and_durable_run_id() {
    use crate::event::BackgroundTask;
    use crate::event::BackgroundTaskKind;
    use crate::event::BackgroundTaskStatus;

    let mut ctx = test_ctx(0, "bg-program-description");
    let ui = Arc::new(BackgroundTaskUi::default());
    ctx.ui = ui.clone();
    let source = "return 'DONE';";
    let (output, is_error) = run_tool(
        "run_program",
        json!({
            "source": source,
            "description": "compile release notes",
            "background": true
        }),
        &ctx,
    )
    .await;
    assert!(!is_error, "{output}");
    assert!(
        output.starts_with("Program(compile release notes) started in the background."),
        "{output}"
    );
    assert!(!output.contains(source), "{output}");
    let program_id = output
        .lines()
        .find_map(|line| line.strip_prefix("Program ID: "))
        .unwrap()
        .to_string();
    let run_id = output
        .lines()
        .find_map(|line| line.strip_prefix("Run ID: "))
        .unwrap()
        .to_string();

    for _ in 0..300 {
        if !ctx.cfg.inbox.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(matches!(
        ctx.cfg.inbox.drain().as_slice(),
        [crate::inbox::InboxItem::ProgramResult {
            label,
            run_id: delivered_run_id,
            summary,
        }] if label == &program_id && delivered_run_id == &run_id && summary == "DONE"
    ));
    for _ in 0..300 {
        if ui.0.lock().unwrap().len() == 2 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        ui.0.lock().unwrap().clone(),
        vec![
            BackgroundTask {
                id: program_id.clone(),
                run_id: Some(run_id.clone()),
                kind: BackgroundTaskKind::Program,
                description: "compile release notes".into(),
                status: BackgroundTaskStatus::Running,
                output_path: None,
                detail: None,
            },
            BackgroundTask {
                id: program_id,
                run_id: Some(run_id.clone()),
                kind: BackgroundTaskKind::Program,
                description: "compile release notes".into(),
                status: BackgroundTaskStatus::Completed,
                output_path: None,
                detail: None,
            },
        ]
    );

    let run_dir = ctx
        .cfg
        .offload_dir
        .parent()
        .unwrap_or(&ctx.cfg.offload_dir)
        .join("program-runs")
        .join(run_id);
    assert_eq!(
        std::fs::read_to_string(run_dir.join("source.js")).unwrap(),
        source
    );
    let manifest = std::fs::read_to_string(run_dir.join("manifest.json")).unwrap();
    assert!(!manifest.contains("compile release notes"), "{manifest}");
    let journal = run_dir.join("journal.jsonl");
    if journal.exists() {
        let journal = std::fs::read_to_string(journal).unwrap();
        assert!(!journal.contains("compile release notes"), "{journal}");
    }
}

#[cfg(windows)]
fn assert_serial_powershell_gate(
    gate: &crate::config::PowerShellGateController,
    active: usize,
    entries: usize,
) {
    assert_eq!(
        gate.snapshot(),
        crate::config::PowerShellGateSnapshot {
            active,
            max_active: 1,
            entries,
        }
    );
}

#[cfg(windows)]
async fn start_background_powershell_program(ctx: &ToolCtx) {
    let (output, is_error) = run_tool(
        "run_program",
        json!({
            "source": "return await tools.powershell({ command: 'Write-Output done' });",
            "background": true
        }),
        ctx,
    )
    .await;
    assert!(!is_error, "{output}");
    assert!(output.contains("started in the background"), "{output}");
}

#[cfg(windows)]
async fn wait_for_background_program_results(
    ctx: &ToolCtx,
    activity: &mut tokio::sync::watch::Receiver<u64>,
    expected: usize,
) -> Vec<crate::inbox::InboxItem> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    let mut results = Vec::new();
    loop {
        results.extend(ctx.cfg.inbox.drain());
        if results.len() >= expected {
            assert_eq!(results.len(), expected, "{results:?}");
            return results;
        }
        tokio::time::timeout_at(deadline, activity.changed())
            .await
            .expect("background PowerShell program did not finish")
            .expect("inbox activity sender stays open");
    }
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_gate_serializes_direct_and_foreground_program_executor_entry() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (ctx, mut gate) =
            with_powershell_gate_probe(test_ctx(0, "powershell-direct-foreground"));
        let direct_ctx = ctx.clone();
        let direct = tokio::spawn(async move {
            run_tool(
                "powershell",
                json!({"command": "Write-Output done"}),
                &direct_ctx,
            )
            .await
        });
        gate.wait_attempted().await;
        gate.wait_entered().await;
        assert_serial_powershell_gate(&gate, 1, 1);

        let program_ctx = ctx.clone();
        let program = tokio::spawn(async move {
            run(
                "return await tools.powershell({ command: 'Write-Output done' });",
                &program_ctx,
            )
            .await
        });
        gate.wait_attempted().await;
        assert_serial_powershell_gate(&gate, 1, 1);

        gate.release_one();
        gate.wait_entered().await;
        assert_serial_powershell_gate(&gate, 1, 2);
        gate.release_one();

        let (direct, program) = tokio::join!(direct, program);
        assert_powershell_done(direct.unwrap());
        assert_powershell_done(program.unwrap());
        assert_serial_powershell_gate(&gate, 0, 2);
    })
    .await
    .expect("direct/foreground PowerShell gate test stalled");
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_gate_serializes_two_background_program_executor_entries() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (ctx, mut gate) =
            with_powershell_gate_probe(test_ctx(0, "powershell-background-programs"));
        let mut activity = ctx.cfg.inbox.subscribe_activity();

        start_background_powershell_program(&ctx).await;
        gate.wait_attempted().await;
        gate.wait_entered().await;
        assert_serial_powershell_gate(&gate, 1, 1);

        start_background_powershell_program(&ctx).await;
        gate.wait_attempted().await;
        assert_serial_powershell_gate(&gate, 1, 1);

        gate.release_one();
        gate.wait_entered().await;
        assert_serial_powershell_gate(&gate, 1, 2);
        gate.release_one();

        let results = wait_for_background_program_results(&ctx, &mut activity, 2).await;
        assert!(
            results.iter().all(|item| matches!(
                item,
                crate::inbox::InboxItem::ProgramResult { summary, .. }
                    if powershell_output_is_done_and_successful(summary)
            )),
            "{results:?}"
        );
        assert_serial_powershell_gate(&gate, 0, 2);
    })
    .await
    .expect("background/background PowerShell gate test stalled");
}

#[cfg(windows)]
#[tokio::test]
async fn powershell_gate_serializes_direct_and_background_program_executor_entry() {
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let (ctx, mut gate) =
            with_powershell_gate_probe(test_ctx(0, "powershell-direct-background"));
        let mut activity = ctx.cfg.inbox.subscribe_activity();
        let direct_ctx = ctx.clone();
        let direct = tokio::spawn(async move {
            run_tool(
                "powershell",
                json!({"command": "Write-Output done"}),
                &direct_ctx,
            )
            .await
        });
        gate.wait_attempted().await;
        gate.wait_entered().await;
        assert_serial_powershell_gate(&gate, 1, 1);

        start_background_powershell_program(&ctx).await;
        gate.wait_attempted().await;
        assert_serial_powershell_gate(&gate, 1, 1);

        gate.release_one();
        gate.wait_entered().await;
        assert_serial_powershell_gate(&gate, 1, 2);
        gate.release_one();

        assert_powershell_done(direct.await.unwrap());
        let results = wait_for_background_program_results(&ctx, &mut activity, 1).await;
        assert!(matches!(
            results.as_slice(),
            [crate::inbox::InboxItem::ProgramResult { summary, .. }]
                if powershell_output_is_done_and_successful(summary)
        ));
        assert_serial_powershell_gate(&gate, 0, 2);
    })
    .await
    .expect("direct/background PowerShell gate test stalled");
}

#[tokio::test]
async fn run_program_rejects_wrong_background_type_and_unknown_fields() {
    let ctx = test_ctx(0, "run-program-strict-input");
    let (wrong_type, type_error) = run_tool(
        "run_program",
        json!({"source": "return 'must not run';", "background": "true"}),
        &ctx,
    )
    .await;
    assert!(type_error);
    assert!(wrong_type.contains("invalid type"), "{wrong_type}");

    let (unknown, unknown_error) = run_tool(
        "run_program",
        json!({"source": "return 'must not run';", "run_in_background": true}),
        &ctx,
    )
    .await;
    assert!(unknown_error);
    assert!(
        unknown.contains("unknown field `run_in_background`"),
        "{unknown}"
    );
    assert_eq!(ctx.cfg.background_executions.running_count(), 0);
}

#[tokio::test]
async fn run_program_rejects_invalid_descriptions_before_creating_run_artifacts() {
    let mut ctx = test_ctx(0, "run-program-description-invalid");
    let root = std::env::temp_dir().join(format!(
        "kloop-codemode-description-invalid-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&root);
    let mut cfg = ctx.cfg.test_clone();
    cfg.offload_dir = root.join("offload");
    ctx.cfg = Arc::new(cfg);
    let ui = Arc::new(BackgroundTaskUi::default());
    ctx.ui = ui.clone();
    let run_root = root.join("program-runs");
    assert!(!run_root.exists());

    let cases = [
        (Value::Null, "must be a string"),
        (json!(false), "invalid type"),
        (json!(" \t"), "must not be empty"),
        (json!("two\nlines"), "single line"),
        (json!("界".repeat(201)), "200-character limit"),
    ];
    for (description, expected) in cases {
        let (output, is_error) = run_tool(
            "run_program",
            json!({
                "source": "return 'must not run';",
                "description": description,
                "background": true
            }),
            &ctx,
        )
        .await;
        assert!(is_error, "{output}");
        assert!(output.contains(expected), "{output}");
    }
    assert!(
        !run_root.exists(),
        "invalid metadata must not open the run store"
    );
    assert_eq!(ctx.cfg.background_executions.running_count(), 0);
    assert!(ui.0.lock().unwrap().is_empty());
    assert!(ctx.cfg.inbox.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

/// A background program cancelled via stop_program ends Aborted and reinjects
/// NOTHING — only an activity wake is published.
#[tokio::test]
async fn stopped_background_program_does_not_reinject() {
    let ui = Arc::new(RecordUi::default());
    let ctx = with_ui(test_ctx(0, "bg-prog-stop"), ui.clone());
    // The program blocks on a long bash so stop_program can catch it running.
    let (out, _) = run_tool(
        "run_program",
        json!({ "source": "return await tools.bash({ command: 'sleep 30' });", "background": true }),
        &ctx,
    )
    .await;
    let id = out
        .split_whitespace()
        .find(|w| w.starts_with("program-"))
        .unwrap()
        .to_string();
    for _ in 0..100 {
        if ctx.cfg.background_executions.running_count() == 1 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (stop_out, is_error) = run_tool("stop_program", json!({ "program_id": id }), &ctx).await;
    assert!(!is_error, "{stop_out}");
    assert!(stop_out.contains("Stopping"), "{stop_out}");

    for _ in 0..300 {
        if ctx.cfg.background_executions.running_count() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        ctx.cfg.background_executions.running_count(),
        0,
        "stopped program did not reach a terminal state"
    );
    assert!(
        ctx.cfg.inbox.is_empty(),
        "an interrupted program reinjects nothing"
    );
    assert_eq!(
        ui.background_events(),
        vec![
            format!("background {id} Running"),
            format!("background {id} Cancelled"),
        ]
    );
}

/// Journal resume: a completed agent() call is replayed from the journal on a
/// resume run instead of being re-spawned. Two different provider turns — a
/// re-spawn would return the second; a journal hit returns the first (cached).
#[tokio::test]
async fn resume_replays_completed_agent_calls_from_the_journal() {
    let run_id = format!("ktr-{}", std::process::id());
    let jdir = std::env::temp_dir().join("program-runs").join(&run_id);
    let _ = std::fs::remove_dir_all(&jdir);
    std::fs::create_dir_all(&jdir).unwrap();

    let text = |t: &str| vec![kloop_protocol::AssistantBlock::Text { text: t.into() }];
    let provider = kloop_provider::Provider::mock(vec![text("FIRST"), text("SECOND")]);
    let ctx = with_provider(test_ctx(0, "resume"), provider);
    let src = r#"return await agent("do the work");"#;
    seed_program_source(&ctx, &run_id, src);
    let first_args = json!({
        "source": src,
        "resume_from_run_id": run_id,
        "description": "first display label"
    });

    // Run 1: spawns the sub-agent, samples "FIRST", journals it.
    let (out1, e1) = run_tool("run_program", first_args, &ctx).await;
    assert!(!e1, "{out1}");
    assert_eq!(out1, "FIRST");

    // Run 2 (same run_id + source, different display metadata): the agent() call
    // hits the journal — no re-spawn, so the second provider turn is never consumed.
    let (out2, e2) = run_tool(
        "run_program",
        json!({
            "source": src,
            "resume_from_run_id": run_id,
            "description": "second display label"
        }),
        &ctx,
    )
    .await;
    assert!(!e2, "{out2}");
    assert_eq!(
        out2, "FIRST",
        "resume replays the cached result, not a re-spawn"
    );
    let _ = std::fs::remove_dir_all(&jdir);
}

#[tokio::test]
async fn resume_rejects_changed_source_before_spawning_an_agent() {
    let run_id = format!("ktr-source-{}", std::process::id());
    let run_dir = std::env::temp_dir().join("program-runs").join(&run_id);
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir).unwrap();
    let provider =
        kloop_provider::Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
            text: "MUST_NOT_RUN".into(),
        }]]);
    let ctx = with_provider(test_ctx(0, "resume-source-mismatch"), provider);
    seed_program_source(&ctx, &run_id, "return await agent('original');");

    let (output, is_error) = run_tool(
        "run_program",
        json!({
            "source": "return await agent('changed');",
            "resume_from_run_id": run_id
        }),
        &ctx,
    )
    .await;
    assert!(is_error, "{output}");
    assert!(output.contains("byte-identical source"), "{output}");
    assert!(
        !run_dir.join("journal.jsonl").exists(),
        "source mismatch must fail before opening or writing the journal"
    );
    let _ = std::fs::remove_dir_all(&run_dir);
}

#[tokio::test]
async fn legacy_program_run_without_source_manifest_fails_closed() {
    let run_id = format!("ktr-legacy-{}", std::process::id());
    let run_dir = std::env::temp_dir().join("program-runs").join(&run_id);
    let _ = std::fs::remove_dir_all(&run_dir);
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(
        run_dir.join("journal.jsonl"),
        r#"{"version":1,"seq":0,"key":"old","result":"cached"}
"#,
    )
    .unwrap();
    let ctx = test_ctx(0, "resume-legacy-source");

    let (output, is_error) = run_tool(
        "run_program",
        json!({"source": "return 'new';", "resume_from_run_id": run_id}),
        &ctx,
    )
    .await;
    assert!(is_error, "{output}");
    assert!(output.contains("no source manifest"), "{output}");
    let _ = std::fs::remove_dir_all(&run_dir);
}

#[tokio::test]
async fn concurrent_resume_of_one_program_run_is_rejected() {
    let run_id = format!("ktrlock-{}", std::process::id());
    let jdir = std::env::temp_dir().join("program-runs").join(&run_id);
    let _ = std::fs::remove_dir_all(&jdir);
    std::fs::create_dir_all(&jdir).unwrap();

    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let provider = kloop_provider::Provider::mock_scripted(vec![kloop_provider::MockTurn::Gate {
        started: started_tx,
        release: release_rx,
        blocks: vec![kloop_protocol::AssistantBlock::Text {
            text: "LOCKED_RESULT".into(),
        }],
    }]);
    let ctx = with_provider(test_ctx(0, "resume-lock"), provider);
    let src = "return await agent('hold the run lock');";
    seed_program_source(&ctx, &run_id, src);
    let args = json!({
        "source": src,
        "resume_from_run_id": run_id
    });
    let first_ctx = ctx.clone();
    let first_args = args.clone();
    let first = tokio::spawn(async move { run_tool("run_program", first_args, &first_ctx).await });
    tokio::time::timeout(std::time::Duration::from_secs(2), started_rx)
        .await
        .expect("first program did not reach sampling")
        .expect("sampling gate dropped");

    let (second, is_error) = run_tool("run_program", args, &ctx).await;
    assert!(is_error, "{second}");
    assert!(second.contains("already active"), "{second}");
    let _ = release_tx.send(());
    let (first_out, first_error) = first.await.unwrap();
    assert!(!first_error, "{first_out}");
    assert_eq!(first_out, "LOCKED_RESULT");
    let _ = std::fs::remove_dir_all(&jdir);
}

/// A program that fails after completing an agent() call reports its run_id and
/// how to resume — so the model can skip the completed work on retry.
#[tokio::test]
async fn failure_after_agent_reports_a_resumable_run_id() {
    let run_id = format!("ktrf-{}", std::process::id());
    let jdir = std::env::temp_dir().join("program-runs").join(&run_id);
    let _ = std::fs::remove_dir_all(&jdir);
    std::fs::create_dir_all(&jdir).unwrap();

    let text = |t: &str| vec![kloop_protocol::AssistantBlock::Text { text: t.into() }];
    let provider = kloop_provider::Provider::mock(vec![text("STEP_ONE_DONE")]);
    let ctx = with_provider(test_ctx(0, "resumefail"), provider);
    let src = r#"await agent("step one"); throw new Error("boom after step one");"#;
    seed_program_source(&ctx, &run_id, src);

    let (out, is_error) = run_tool(
        "run_program",
        json!({ "source": src, "resume_from_run_id": run_id }),
        &ctx,
    )
    .await;
    assert!(is_error, "{out}");
    assert!(out.contains("boom after step one"), "{out}");
    assert!(
        out.contains(&run_id) && out.contains("resume_from_run_id"),
        "reports how to resume: {out}"
    );
    let _ = std::fs::remove_dir_all(&jdir);
}

/// Intermediate tool results live in program variables; only the return value
/// comes back. Two full file reads happen, but their content never appears in
/// the tool_result — exactly the context-window saving code-mode exists for.
#[tokio::test]
async fn intermediate_results_stay_off_the_result() {
    let file = tmp("intermediate");
    std::fs::write(&file, "SECRET_PAYLOAD").unwrap();
    let ctx = test_ctx(0, "intermediate");
    let (out, is_error) = run(
        &format!(
            r#"const a = await tools.read_file({{ path: {file:?} }});
               const b = await tools.read_file({{ path: {file:?} }});
               return "combined_len:" + (a.length + b.length);"#,
        ),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert!(out.starts_with("combined_len:"), "{out}");
    assert!(
        !out.contains("SECRET_PAYLOAD"),
        "file content must not leak into the result: {out}"
    );
    let _ = std::fs::remove_file(&file);
}

#[tokio::test]
async fn program_error_surfaces_the_exception_without_logs() {
    let ctx = test_ctx(0, "err");
    let (out, is_error) = run(r#"log("before"); throw new Error("kaboom");"#, &ctx).await;
    assert!(is_error);
    assert!(out.contains("kaboom"), "{out}");
    // log() is a live user-facing channel, not part of the model's result.
    assert!(
        !out.contains("before"),
        "logs must not leak into the result: {out}"
    );
}

/// A running program is observable: its `log()` output streams live to the UI
/// (not buried in the final result) and each `tools.<name>()` op shows as a
/// tool line — so the program is not a black box while it runs.
#[tokio::test]
async fn program_logs_and_ops_stream_to_the_ui() {
    let file = tmp("observe");
    std::fs::write(&file, "data").unwrap();
    let rec = Arc::new(RecordUi::default());
    let ctx = with_ui(test_ctx(0, "observe"), rec.clone());
    let (out, is_error) = run(
        &format!(
            r#"log("phase 1");
               await tools.read_file({{ path: {file:?} }});
               log("phase 2");
               return "ok";"#,
        ),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "ok", "only the return value comes back, no logs");

    let events = rec.events();
    // Both logs surfaced live, in order, with the inner op's tool line between
    // them.
    let pos = |needle: &str| {
        events
            .iter()
            .position(|e| e == needle)
            .unwrap_or_else(|| panic!("missing {needle:?} in {events:?}"))
    };
    assert!(
        pos("note: phase 1") < pos("tool_start: read_file")
            && pos("tool_start: read_file") < pos("note: phase 2"),
        "expected phase 1 → read_file → phase 2 in {events:?}"
    );
    assert!(events.iter().any(|e| e == "tool_end: true"), "{events:?}");
    let _ = std::fs::remove_file(&file);
}

#[test]
fn run_program_def_renders_a_typescript_api() {
    let defs = vec![
        ToolDef {
            name: "read_file".into(),
            description: "Read   a text\nfile".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "limit": {"type": "integer"}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "grep".into(),
            description: "Search".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "output_mode": {"type": "string", "enum": ["content", "count"]}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "bash".into(),
            description: "Run foreground or background".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "background": {"type": "boolean"}
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "run_agent".into(),
            description: "spawn".into(),
            schema: json!({"type": "object"}),
        },
    ];
    let def = run_program_def(&defs, &[], &[]);
    let d = &def.description;
    assert_eq!(def.name, "run_program");
    assert!(d.contains("read_file(args: {"), "{d}");
    assert!(d.contains("path: string"), "{d}");
    assert!(d.contains("limit?: number"), "{d}");
    // Whitespace in the source description is collapsed to one line.
    assert!(d.contains("/** Read a text file */"), "{d}");
    // String enums render as a union.
    assert!(d.contains(r#"output_mode?: "content" | "count""#), "{d}");
    assert!(d.contains("declare function agent("), "{d}");
    assert!(d.contains("declare function parallel<T>"), "{d}");
    assert!(d.contains("declare function pipeline("), "{d}");
    // run_agent is not callable from a program (agent() replaces it); run_program
    // (the tool itself) isn't either.
    assert!(!d.contains("run_agent(args"), "{d}");
    assert!(!d.contains("run_program(args"), "{d}");
    assert!(!d.contains("background?: boolean"), "{d}");
    assert!(d.contains("Program cannot detach shell resources"), "{d}");
}

#[test]
fn ts_type_covers_common_shapes() {
    assert_eq!(ts_type(&json!({"type": "string"})), "string");
    assert_eq!(ts_type(&json!({"type": "integer"})), "number");
    assert_eq!(ts_type(&json!({"type": "boolean"})), "boolean");
    assert_eq!(
        ts_type(&json!({"type": "string", "enum": ["a", "b"]})),
        r#""a" | "b""#
    );
    assert_eq!(
        ts_type(&json!({"type": "array", "items": {"type": "string"}})),
        "Array<string>"
    );
    assert_eq!(
        ts_type(&json!({
            "type": "object",
            "properties": {"x": {"type": "integer"}, "y": {"type": "string"}},
            "required": ["x"]
        })),
        "{ x: number; y?: string }"
    );
    // Unknown shapes degrade rather than lie.
    assert_eq!(ts_type(&json!({})), "unknown");
}

#[test]
fn program_surface_excludes_run_program_and_run_agent() {
    let names = program_tool_names(&[], &crate::shell_programs::ShellPrograms::test_fixture());
    assert!(names.iter().any(|n| n == "read_file"));
    assert!(names.iter().any(|n| n == "bash"));
    assert!(!names.iter().any(|n| n == "run_program"));
    assert!(!names.iter().any(|n| n == "run_agent"));
    assert!(!names.iter().any(|n| n == "bash_output"));
    assert!(!names.iter().any(|n| n == "stop_bash"));
    assert!(!names.iter().any(|n| n == "send_message"));
    assert!(!names.iter().any(|n| n == "list_agents"));
}

#[tokio::test]
async fn core_bridge_rejects_names_outside_the_program_catalog() {
    let bridge = CoreBridge::new(
        test_ctx(0, "bridge-allowlist"),
        kloop_codemode::Limits::default(),
        None,
        &["read_file".into()],
    );
    let error = bridge
        .call_tool("run_agent".into(), json!({"prompt": "escape"}))
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "program tool 'run_agent' is not in this run's callable catalog"
    );
}

// ---- External source (MCP) tools exposed to programs (plan 27) ----

/// A minimal external tool source: `srv__echo` (read-only, echoes its `text`
/// arg), `srv__danger` (a mutating tool), and `srv__data` (returns a structured
/// CallToolResult). Mirrors the shape an MCP tool reaches the bridge with.
struct Srv {
    defs: Vec<ToolDef>,
}

fn srv() -> std::sync::Arc<dyn ToolSource> {
    let def = |name: &str, desc: &str| ToolDef {
        name: name.into(),
        description: desc.into(),
        schema: json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"]
        }),
    };
    std::sync::Arc::new(Srv {
        defs: vec![
            def("srv__echo", "Echo the text argument back"),
            def("srv__danger", "A mutating tool"),
            def("srv__data", "Return a structured result"),
        ],
    })
}

impl ToolSource for Srv {
    fn defs(&self) -> Arc<[ToolDef]> {
        Arc::from(self.defs.clone())
    }
    fn is_readonly(&self, tool: &str) -> bool {
        tool == "srv__echo" || tool == "srv__data"
    }
    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a serde_json::Value,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = anyhow::Result<crate::tools::SourceOutput>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("?");
            if tool == "srv__data" {
                // A real MCP CallToolResult: flat text plus structuredContent.
                return Ok(crate::tools::SourceOutput {
                    text: format!("count={}", text.len()),
                    blocks: None,
                    structured: Some(json!({
                        "content": [{"type": "text", "text": format!("count={}", text.len())}],
                        "structuredContent": {"len": text.len(), "echo": text}
                    })),
                });
            }
            Ok(crate::tools::SourceOutput::text(format!(
                "{tool} echoes {text}"
            )))
        })
    }
}

/// Slice 1: below the defer threshold, an external source tool is callable from
/// a program by its name, routed through the real gate to the source.
#[tokio::test]
async fn program_calls_an_mcp_source_tool() {
    let ctx = test_ctx_with_sources(0, "mcp-inline", vec![srv()]);
    let (out, is_error) = run(r#"return await tools.srv__echo({ text: "hi" });"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "srv__echo echoes hi");
}

/// Slice 1: the permission gate still applies inside a program — a denied
/// source tool is refused exactly like a denied built-in.
#[tokio::test]
async fn denied_mcp_source_tool_is_refused_in_a_program() {
    let rules = PermissionRules {
        allow: vec![],
        deny: vec!["srv__echo".into()],
        ask: vec![],
    };
    let perms = Permissions::new(Mode::Manual, &rules, std::env::temp_dir(), None).unwrap();
    let ctx = with_permissions(test_ctx_with_sources(0, "mcp-deny", vec![srv()]), perms);
    let (out, is_error) = run(
        r#"try { await tools.srv__echo({ text: "x" }); return "RAN"; }
           catch (e) { return "BLOCKED"; }"#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "BLOCKED");
}

/// Slice 2: past the defer threshold the source tool is deferred — a top-level
/// direct call bounces on the lock gate — yet it stays callable from inside a
/// program, which bypasses that gate (the tool is exposed on `tools`). Every
/// other gate still runs; here the permission gate allows.
#[tokio::test]
async fn program_calls_a_deferred_mcp_tool_that_top_level_cannot() {
    let ctx = with_defer_threshold(test_ctx_with_sources(0, "mcp-deferred", vec![srv()]), 0);

    // Top-level direct call bounces: the model would have to tool_search first.
    let (out, is_error) = run_tool("srv__echo", json!({ "text": "x" }), &ctx).await;
    assert!(is_error, "{out}");
    assert!(out.contains("deferred and not loaded"), "{out}");

    // The same tool, called from inside a program, runs.
    let (out, is_error) = run(r#"return await tools.srv__echo({ text: "hi" });"#, &ctx).await;
    assert!(!is_error, "{out}");
    assert_eq!(out, "srv__echo echoes hi");
}

/// The program's callable surface always includes source tools, deferred or not.
#[test]
fn program_surface_includes_source_tools() {
    let names = program_tool_names(
        &[srv()],
        &crate::shell_programs::ShellPrograms::test_fixture(),
    );
    assert!(names.iter().any(|n| n == "srv__echo"));
    assert!(names.iter().any(|n| n == "srv__danger"));
    assert!(names.iter().any(|n| n == "bash"));
    assert!(!names.iter().any(|n| n == "run_program"));
}

#[test]
fn program_surface_excludes_control_names_from_external_sources() {
    let control = |name: &str| ToolDef {
        name: name.into(),
        description: "must stay outside Program".into(),
        schema: json!({"type": "object"}),
    };
    let source: Arc<dyn ToolSource> = Arc::new(Srv {
        defs: vec![
            control("workflow"),
            control("run_agent"),
            control("srv__ok"),
        ],
    });
    let names = program_tool_names(
        &[source],
        &crate::shell_programs::ShellPrograms::test_fixture(),
    );
    assert!(names.iter().any(|name| name == "srv__ok"));
    assert!(!names.iter().any(|name| name == "workflow"));
    assert!(!names.iter().any(|name| name == "run_agent"));
}

/// Slice 3: an MCP tool with a structured result reaches the program as the
/// `CallToolResult` object (content blocks + structuredContent), not flat text,
/// so the program reads typed fields directly. A built-in in the same program
/// still returns a plain string.
#[tokio::test]
async fn program_receives_structured_calltoolresult_from_mcp() {
    let ctx = test_ctx_with_sources(0, "mcp-structured", vec![srv()]);
    let (out, is_error) = run(
        r#"const r = await tools.srv__data({ text: "hello" });
           const b = typeof (await tools.grep({ pattern: "zzz_nomatch_zzz" }));
           return JSON.stringify({
               structured: r.structuredContent.len,
               echo: r.structuredContent.echo,
               firstBlock: r.content[0].text,
               builtinType: b,
           });"#,
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    // structuredContent.len = "hello".len() = 5; content[0].text is the flat
    // text; a built-in tool still resolves to a string.
    assert_eq!(
        out,
        r#"{"structured":5,"echo":"hello","firstBlock":"count=5","builtinType":"string"}"#
    );
}

/// Slice 3: inline source tools are declared returning `Promise<CallToolResult>`
/// (built-ins keep `Promise<string>`), and the `CallToolResult` type is defined.
#[test]
fn run_program_def_types_source_tools_as_calltoolresult() {
    let builtins = vec![ToolDef {
        name: "read_file".into(),
        description: "Read".into(),
        schema: json!({"type": "object"}),
    }];
    let sources = vec![ToolDef {
        name: "srv__data".into(),
        description: "Structured".into(),
        schema: json!({"type": "object"}),
    }];
    let def = run_program_def(&builtins, &sources, &[]);
    let d = &def.description;
    assert!(d.contains("type CallToolResult"), "{d}");
    assert!(
        d.contains("srv__data(args: Record<string, unknown>): Promise<CallToolResult>;"),
        "{d}"
    );
    // Built-ins keep the string contract.
    assert!(
        d.contains("read_file(args: Record<string, unknown>): Promise<string>;"),
        "{d}"
    );
}

/// `glob` is the one built-in whose result is naturally a list: a program gets a
/// `string[]` of paths (not the newline-joined text the model sees), and the
/// declaration says so.
#[tokio::test]
async fn program_receives_glob_paths_as_an_array() {
    let dir = tmp("glob-array");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "x").unwrap();
    std::fs::write(dir.join("b.txt"), "y").unwrap();
    let ctx = test_ctx(0, "glob-array");
    let (out, is_error) = run(
        &format!(
            r#"const files = await tools.glob({{ pattern: "*.txt", path: {dir:?} }});
               return JSON.stringify({{ isArray: Array.isArray(files), count: files.length }});"#,
        ),
        &ctx,
    )
    .await;
    assert!(!is_error, "{out}");
    assert_eq!(out, r#"{"isArray":true,"count":2}"#);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_program_def_declares_glob_as_string_array() {
    let def = run_program_def(
        &[ToolDef {
            name: "glob".into(),
            description: "Find files".into(),
            schema: json!({"type": "object"}),
        }],
        &[],
        &[],
    );
    assert!(
        def.description
            .contains("glob(args: Record<string, unknown>): Promise<string[]>;"),
        "{}",
        def.description
    );
}
