use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, bail};
use kloop_protocol::{AssistantBlock, Message, ToolDef};
use kloop_provider::{MockTurn, Provider};
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::testutil::{run_tool, test_ctx, test_ctx_with_sources, with_defer_threshold};
use super::{SourceOutput, ToolSource, all_tool_defs};
use crate::agent::{EndReason, run_turn};
use crate::config::SurfaceCapabilities;
use crate::history::History;
use crate::inbox::{Inbox, InboxItem};
use crate::interaction::{QuestionAnswer, QuestionOutcome, QuestionRequest, Questioner};
use crate::permissions::{Approver, ConfirmRequest, Decision, Mode, PermissionRules, Permissions};
use crate::scheduler::{ManualClock, Scheduler, SchedulerTimeZone};

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-plan59-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(std::fs::canonicalize(path).unwrap())
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempRoot {
    fn drop(&mut self) {
        let removed = std::fs::remove_dir_all(&self.0);
        if !std::thread::panicking() {
            removed.unwrap();
            assert!(!self.0.exists(), "temporary root was not removed");
        }
    }
}

struct ScriptedQuestioner(Mutex<VecDeque<QuestionOutcome>>);

impl ScriptedQuestioner {
    fn answered(answer: QuestionAnswer) -> Arc<Self> {
        Arc::new(Self(Mutex::new(VecDeque::from([
            QuestionOutcome::Answered(vec![answer]),
        ]))))
    }

    fn cancelled() -> Arc<Self> {
        Arc::new(Self(Mutex::new(VecDeque::from([
            QuestionOutcome::Cancelled,
        ]))))
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
            .expect("missing scripted answer");
        Box::pin(async move { outcome })
    }
}

struct AllowApprover;

impl Approver for AllowApprover {
    fn confirm(
        &self,
        _request: ConfirmRequest,
    ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        Box::pin(async { Decision::Allow(crate::permissions::ApprovalScope::Once) })
    }
}

struct DenyApprover;

impl Approver for DenyApprover {
    fn confirm(
        &self,
        _request: ConfirmRequest,
    ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        Box::pin(async { Decision::Deny })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PermissionTraceEvent {
    Defer,
    Unlock,
    Approval,
    Dispatch { tool: String, generation: u64 },
}

type PermissionTrace = Arc<Mutex<Vec<PermissionTraceEvent>>>;

struct RecordingApprover {
    requests: Mutex<Vec<ConfirmRequest>>,
    trace: PermissionTrace,
    decision: Decision,
}

impl RecordingApprover {
    fn new(trace: PermissionTrace, decision: Decision) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            trace,
            decision,
        }
    }

    fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl Approver for RecordingApprover {
    fn confirm(
        &self,
        request: ConfirmRequest,
    ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
        self.requests.lock().unwrap().push(request);
        self.trace
            .lock()
            .unwrap()
            .push(PermissionTraceEvent::Approval);
        let decision = self.decision;
        Box::pin(async move { decision })
    }
}

struct SeamSource {
    calls: Mutex<Vec<String>>,
    generation: AtomicU64,
    trace: PermissionTrace,
}

impl SeamSource {
    fn new(trace: PermissionTrace) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            generation: AtomicU64::new(0),
            trace,
        }
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn refresh(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

impl ToolSource for SeamSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        let generation = self.generation.load(Ordering::SeqCst);
        Arc::from([
            ToolDef {
                name: "read_mcp_resource".into(),
                description: format!("Read a local MCP resource seam generation {generation}"),
                schema: json!({
                    "type": "object",
                    "properties": {"uri": {"type": "string"}},
                    "required": ["uri"],
                    "additionalProperties": false
                }),
            },
            ToolDef {
                name: "web_search".into(),
                description: "Search a local web seam".into(),
                schema: json!({
                    "type": "object",
                    "properties": {"query": {"type": "string"}},
                    "required": ["query"],
                    "additionalProperties": false
                }),
            },
        ])
    }

    fn definition_generation(&self, _tool: &str) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            self.trace
                .lock()
                .unwrap()
                .push(PermissionTraceEvent::Dispatch {
                    tool: tool.to_string(),
                    generation: self.generation.load(Ordering::SeqCst),
                });
            self.calls.lock().unwrap().push(tool.to_string());
            match tool {
                "read_mcp_resource" => match input.get("uri").and_then(Value::as_str) {
                    Some("mcp://local/plan59") => Ok(SourceOutput::text("MCP-RESOURCE-59".into())),
                    Some(uri) => bail!("missing MCP resource: {uri}"),
                    None => bail!("missing MCP resource URI"),
                },
                "web_search" => Ok(SourceOutput::text(format!(
                    "WEB-RESULT-59:{}",
                    input["query"].as_str().unwrap_or_default()
                ))),
                _ => bail!("unsupported seam tool: {tool}"),
            }
        })
    }

    fn is_readonly(&self, _tool: &str) -> bool {
        true
    }

    fn should_defer(&self, tool: &str) -> bool {
        tool == "read_mcp_resource"
    }
}

fn git(repo: &Path, args: &[&str]) -> String {
    let home = repo.join(".plan59-home");
    let xdg = home.join(".config");
    std::fs::create_dir_all(&xdg).unwrap();
    let mut command = std::process::Command::new("git");
    command.args(args).current_dir(repo).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    let output = command
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", &xdg)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

fn init_repo(root: &Path) {
    git(root, &["init", "-q"]);
    git(root, &["config", "user.email", "plan59@example.invalid"]);
    git(root, &["config", "user.name", "Plan 59"]);
    git(root, &["config", "commit.gpgSign", "false"]);
    git(root, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(root.join("chain.txt"), "alpha\nbeta\n").unwrap();
    git(root, &["add", "chain.txt"]);
    git(root, &["commit", "-qm", "seed"]);
}

fn names(surface: SurfaceCapabilities) -> Vec<String> {
    all_tool_defs(
        0,
        &[],
        30,
        surface,
        &crate::shell_programs::ShellPrograms::native_posix(),
    )
    .into_iter()
    .map(|definition| definition.name)
    .collect()
}

async fn file_search_bash_worktree_report() -> Value {
    let root = TempRoot::new("file-worktree");
    init_repo(root.path());
    let isolated_home = root.path().join(".plan59-home");
    let isolated_xdg = isolated_home.join(".config");
    std::fs::write(isolated_home.join(".gitconfig"), "[invalid\n").unwrap();
    let _git_environment =
        crate::worktree::isolate_test_git_environment(root.path(), &isolated_home, &isolated_xdg);
    let mut context = test_ctx(0, "plan59-file-worktree");
    let mut config = context.cfg.test_clone();
    config.cwd = root.path().to_path_buf();
    config.permissions = Arc::new(
        Permissions::new(
            Mode::AcceptEdits,
            &PermissionRules::default(),
            root.path().to_path_buf(),
            Some(Arc::new(AllowApprover)),
        )
        .unwrap(),
    );
    config.surface.worktree = true;
    context.cfg = Arc::new(config);
    let base_file_state = context.cfg.effective_file_state();

    let (read, read_error) = run_tool(
        "read_file",
        json!({"path": root.path().join("chain.txt")}),
        &context,
    )
    .await;
    assert!(!read_error && read.contains("alpha"));
    let (edited, edit_error) = run_tool(
        "edit_file",
        json!({
            "path": root.path().join("chain.txt"),
            "old_string": "alpha",
            "new_string": "ALPHA"
        }),
        &context,
    )
    .await;
    assert!(!edit_error && edited.contains("edited"));
    let (globbed, glob_error) = run_tool("glob", json!({"pattern": "*.txt"}), &context).await;
    assert!(!glob_error && globbed.contains("chain.txt"));
    let (grepped, grep_error) = run_tool(
        "grep",
        json!({"pattern": "ALPHA", "path": ".", "output_mode": "files_with_matches"}),
        &context,
    )
    .await;
    assert!(!grep_error && grepped.contains("chain.txt"));
    let bash_command = format!(
        "HOME='{}' XDG_CONFIG_HOME='{}' GIT_CONFIG_NOSYSTEM=1 sh -c \"printf 'BASH-59:'; grep ALPHA chain.txt\"",
        isolated_home.display(),
        isolated_xdg.display()
    );
    let (bash, bash_error) = run_tool("bash", json!({"command": bash_command}), &context).await;
    assert!(!bash_error && bash.contains("BASH-59:ALPHA"));

    let (entered, enter_error) =
        run_tool("enter_worktree", json!({"name": "plan59-chain"}), &context).await;
    assert!(!enter_error && entered.contains("plan59-chain"));
    let worktree = root.path().join(".kloop-worktrees/plan59-chain");
    assert!(worktree.is_dir());
    assert_eq!(context.cfg.effective_cwd(), worktree);
    let worktree_file_state = context.cfg.effective_file_state();
    assert!(!Arc::ptr_eq(&base_file_state, &worktree_file_state));
    let (stale_edit, stale_error) = run_tool(
        "edit_file",
        json!({
            "path": worktree.join("chain.txt"),
            "old_string": "alpha",
            "new_string": "STALE"
        }),
        &context,
    )
    .await;
    assert!(stale_error && stale_edit.contains("read"));
    let (fresh_read, fresh_read_error) = run_tool(
        "read_file",
        json!({"path": worktree.join("chain.txt")}),
        &context,
    )
    .await;
    assert!(!fresh_read_error && fresh_read.contains("alpha"));
    let (worktree_edit, worktree_edit_error) = run_tool(
        "edit_file",
        json!({
            "path": worktree.join("chain.txt"),
            "old_string": "alpha",
            "new_string": "WORKTREE"
        }),
        &context,
    )
    .await;
    assert!(!worktree_edit_error && worktree_edit.contains("edited"));
    let (exited, exit_error) = run_tool(
        "exit_worktree",
        json!({"action": "remove", "discard_changes": true}),
        &context,
    )
    .await;
    assert!(!exit_error && exited.contains("removed"));
    assert_eq!(context.cfg.effective_cwd(), root.path());
    assert!(Arc::ptr_eq(
        &base_file_state,
        &context.cfg.effective_file_state()
    ));
    assert!(!worktree.exists());
    assert!(git(root.path(), &["branch", "--list", "worktree-plan59-chain"]).is_empty());
    assert_eq!(
        std::fs::read_to_string(root.path().join("chain.txt")).unwrap(),
        "ALPHA\nbeta\n"
    );

    json!({
        "rust_scope": "full",
        "sequence": ["read_file", "edit_file", "glob", "grep", "bash", "enter_worktree", "read_file", "edit_file", "exit_worktree"],
        "search_observed_edit": true,
        "bash_observed_edit": true,
        "temporary_home_isolated": true,
        "worktree_fresh_base": true,
        "file_state_rebound": true,
        "cwd_restored": true,
        "branch_removed": true,
        "path_removed": true,
    })
}

async fn background_agent_scheduler_report() -> Value {
    let root = TempRoot::new("background-agent-scheduler");
    let inbox = Arc::new(Inbox::default());
    let clock = ManualClock::new(0);
    let scheduler = Scheduler::with_clock(
        Arc::clone(&inbox),
        None,
        clock.clone(),
        SchedulerTimeZone::named("UTC").unwrap(),
    );
    scheduler.bind_owner("plan59-owner").unwrap();
    let (live_started_tx, live_started_rx) = oneshot::channel();
    let (live_release_tx, live_release_rx) = oneshot::channel();
    let (provider, seen) = Provider::mock_recording(vec![
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "AGENT-RESULT-59".into(),
        }]),
        MockTurn::Blocks(vec![AssistantBlock::Text {
            text: "PARENT-REINJECTED-59".into(),
        }]),
        MockTurn::Gate {
            started: live_started_tx,
            release: live_release_rx,
            blocks: vec![AssistantBlock::Text {
                text: "MUST-NOT-DELIVER-59".into(),
            }],
        },
    ]);
    let mut context = test_ctx(0, "plan59-background-agent-scheduler");
    let mut config = context.cfg.test_clone();
    config.cwd = root.path().to_path_buf();
    config.offload_dir = root.path().join("offload");
    config.inbox = Arc::clone(&inbox);
    config.scheduler = Arc::clone(&scheduler);
    config.session_id = "plan59-owner".into();
    config.set_test_provider(provider);
    config.surface.scheduler = true;
    context.cfg = Arc::new(config);

    let shell_command = format!(
        "HOME='{}' XDG_CONFIG_HOME='{}' GIT_CONFIG_NOSYSTEM=1 printf SHELL-RESULT-59",
        root.path().join("home").display(),
        root.path().join("home/.config").display()
    );
    let (shell_start, shell_start_error) = run_tool(
        "bash",
        json!({"command": shell_command, "background": true}),
        &context,
    )
    .await;
    assert!(!shell_start_error, "{shell_start}");
    let shell_id = shell_start
        .split("ID: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("background shell id")
        .trim_end_matches('.');
    let (shell_output, shell_output_error) =
        run_tool("bash_output", json!({"bash_id": shell_id}), &context).await;
    assert!(
        !shell_output_error && shell_output.contains("SHELL-RESULT-59"),
        "start={shell_start}; output={shell_output}"
    );

    let mut agent_activity = inbox.subscribe_activity();
    let (agent_start, agent_start_error) = run_tool(
        "run_agent",
        json!({"prompt": "return the marker", "background": true}),
        &context,
    )
    .await;
    assert!(
        !agent_start_error && agent_start.starts_with("Agent(return the marker)"),
        "{agent_start}"
    );
    if context.cfg.background_executions.running_count() != 0 {
        tokio::time::timeout(Duration::from_secs(2), agent_activity.changed())
            .await
            .expect("background agent did not finish")
            .expect("agent activity channel closed");
    }
    assert_eq!(context.cfg.background_executions.running_count(), 0);

    let (wakeup, wakeup_error) = run_tool(
        "schedule_wakeup",
        json!({
            "delay_seconds": 60,
            "reason": "plan59 acceptance",
            "prompt": "WAKEUP-PROMPT-59"
        }),
        &context,
    )
    .await;
    assert!(!wakeup_error && wakeup.contains("Scheduled"));
    let due = scheduler.list().unwrap()[0].next_fire_at_ms;
    let mut wakeup_activity = inbox.subscribe_activity();
    clock.set(due);
    tokio::time::timeout(Duration::from_secs(2), wakeup_activity.changed())
        .await
        .expect("wakeup did not fire")
        .expect("wakeup activity channel closed");
    assert!(scheduler.list().unwrap().is_empty());

    let mut history = History::new(root.path().join("history"));
    history.record(Message::user_text("consume all background notifications"));
    let reinjected = run_turn(&context.cfg, &mut history, &context.ui, &context.cancel, 0).await;
    assert_eq!(reinjected.reason, EndReason::Completed);
    assert_eq!(reinjected.final_text, "PARENT-REINJECTED-59");
    assert!(inbox.is_empty());
    let parent_request = {
        let requests = seen.lock().unwrap();
        assert_eq!(requests.len(), 2);
        serde_json::to_string(&requests[1].messages).unwrap()
    };
    let shell_marker = "A background shell command you started has changed state.";
    let agent_marker = "A background sub-agent you dispatched has finished.";
    let wakeup_marker = "A scheduled task is due.";
    let shell_delivery_count = parent_request.matches(shell_marker).count();
    let agent_delivery_count = parent_request.matches(agent_marker).count();
    let wakeup_delivery_count = parent_request.matches(wakeup_marker).count();
    assert_eq!(
        (
            shell_delivery_count,
            agent_delivery_count,
            wakeup_delivery_count,
        ),
        (1, 1, 1),
        "duplicate or missing delivery: {parent_request}"
    );
    assert_eq!(parent_request.matches("AGENT-RESULT-59").count(), 1);
    assert_eq!(parent_request.matches("WAKEUP-PROMPT-59").count(), 1);
    let mut delivery_positions = vec![
        (parent_request.find(shell_marker).unwrap(), "shell"),
        (parent_request.find(agent_marker).unwrap(), "agent"),
        (parent_request.find(wakeup_marker).unwrap(), "wakeup"),
    ];
    delivery_positions.sort_by_key(|(index, _)| *index);
    let delivery_order = delivery_positions
        .into_iter()
        .map(|(_, source)| source)
        .collect::<Vec<_>>();
    assert_eq!(delivery_order, ["shell", "agent", "wakeup"]);

    let (live_agent, live_agent_error) = run_tool(
        "run_agent",
        json!({"prompt": "remain live until session shutdown", "background": true}),
        &context,
    )
    .await;
    assert!(
        !live_agent_error && live_agent.starts_with("Agent(remain live until session shutdown)"),
        "{live_agent}"
    );
    tokio::time::timeout(Duration::from_secs(2), live_started_rx)
        .await
        .expect("live agent never sampled")
        .expect("live agent start channel closed");
    let (timed_out, timeout_error) = run_tool(
        "bash",
        json!({"command": "sleep 30", "timeout_ms": 10}),
        &context,
    )
    .await;
    assert!(timeout_error && timed_out.contains("timed out"));
    let (live_shell, live_shell_error) = run_tool(
        "bash",
        json!({"command": "sleep 30", "background": true}),
        &context,
    )
    .await;
    assert!(!live_shell_error, "{live_shell}");
    let live_shell_id = live_shell
        .split("ID: ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .expect("live shell id")
        .trim_end_matches('.')
        .to_string();
    let (pending_wakeup, pending_wakeup_error) = run_tool(
        "schedule_wakeup",
        json!({
            "delay_seconds": 3600,
            "reason": "must be removed on shutdown",
            "prompt": "MUST-NOT-WAKE-59"
        }),
        &context,
    )
    .await;
    assert!(!pending_wakeup_error && pending_wakeup.contains("Scheduled"));
    assert_eq!(context.cfg.background_executions.running_count(), 1);
    assert_eq!(scheduler.list().unwrap().len(), 1);
    assert_eq!(context.cfg.shutdown_background_work(&context.ui).await, 0);
    drop(live_release_tx);
    assert_eq!(context.cfg.background_executions.running_count(), 0);
    assert!(scheduler.list().unwrap_err().to_string().contains("closed"));
    let (cancelled_shell, cancelled_shell_error) =
        run_tool("bash_output", json!({"bash_id": live_shell_id}), &context).await;
    assert!(
        !cancelled_shell_error && cancelled_shell.contains("killed (session shutdown)"),
        "cancelled shell output: {cancelled_shell}"
    );
    let shutdown_items = inbox.drain();
    assert!(shutdown_items.iter().all(|item| !matches!(
        item,
        InboxItem::SubAgentResult { .. } | InboxItem::ScheduledPrompt { .. }
    )));

    let registered = names(context.cfg.surface);
    assert!(
        !registered
            .iter()
            .any(|name| name == "monitor" || name == "Monitor")
    );

    json!({
        "rust_scope": "surface-gate",
        "sequence": ["bash(background)", "bash_output", "task(background)", "schedule_wakeup", "run_turn(reinject)", "task(live)", "bash(timeout)", "bash(live)", "schedule_wakeup(pending)", "session shutdown"],
        "shell_completed": true,
        "agent_delivery_count": agent_delivery_count,
        "wakeup_delivery_count": wakeup_delivery_count,
        "delivery_order": delivery_order,
        "step_boundary_reinjection": true,
        "bash_timeout": true,
        "live_shutdown": ["agent", "shell", "wakeup"],
        "monitor_registered": false,
        "monitor_reason": "no native Monitor surface in the accepted profile",
        "running_after": 0,
        "shutdown_residue": 0,
    })
}

async fn mcp_web_toolsource_report() -> Value {
    let root = TempRoot::new("mcp-web-toolsource");
    let trace: PermissionTrace = Arc::new(Mutex::new(Vec::new()));
    let source = Arc::new(SeamSource::new(Arc::clone(&trace)));
    let erased: Arc<dyn ToolSource> = source.clone();
    let approver = Arc::new(RecordingApprover::new(
        Arc::clone(&trace),
        Decision::Allow(crate::permissions::ApprovalScope::Once),
    ));
    let mut context = with_defer_threshold(
        test_ctx_with_sources(0, "plan59-mcp-web", vec![erased]),
        usize::MAX,
    );
    let mut config = context.cfg.test_clone();
    config.permissions = Arc::new(
        Permissions::new(
            Mode::Manual,
            &PermissionRules::default(),
            root.path().to_path_buf(),
            Some(approver.clone()),
        )
        .unwrap(),
    );
    context.cfg = Arc::new(config);
    let definitions = all_tool_defs(
        0,
        &context.cfg.tool_sources,
        context.cfg.defer_threshold,
        context.cfg.surface,
        &context.cfg.shell_programs,
    );
    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == "tool_search")
    );
    assert!(
        definitions
            .iter()
            .all(|definition| definition.name != "read_mcp_resource")
    );
    assert!(
        definitions
            .iter()
            .any(|definition| definition.name == "web_search")
    );

    let (locked, locked_error) = run_tool(
        "read_mcp_resource",
        json!({"uri": "mcp://local/plan59"}),
        &context,
    )
    .await;
    assert!(locked_error && locked.contains("deferred and not loaded"));
    assert_eq!(approver.count(), 0);
    trace.lock().unwrap().push(PermissionTraceEvent::Defer);
    let (selected, selected_error) = run_tool(
        "tool_search",
        json!({"query": "select:read_mcp_resource"}),
        &context,
    )
    .await;
    assert!(!selected_error && selected.contains("generation 0"));
    trace.lock().unwrap().push(PermissionTraceEvent::Unlock);
    let (resource, resource_error) = run_tool(
        "read_mcp_resource",
        json!({"uri": "mcp://local/plan59"}),
        &context,
    )
    .await;
    assert!(!resource_error && resource == "MCP-RESOURCE-59");
    assert_eq!(approver.count(), 1);
    let (missing, missing_error) = run_tool(
        "read_mcp_resource",
        json!({"uri": "mcp://local/missing"}),
        &context,
    )
    .await;
    assert!(missing_error && missing.contains("missing MCP resource"));
    assert_eq!(approver.count(), 2);

    source.refresh();
    let (stale, stale_error) = run_tool(
        "read_mcp_resource",
        json!({"uri": "mcp://local/plan59"}),
        &context,
    )
    .await;
    assert!(stale_error && stale.contains("deferred and not loaded"));
    assert_eq!(approver.count(), 2);
    trace.lock().unwrap().push(PermissionTraceEvent::Defer);
    let (refreshed, refreshed_error) = run_tool(
        "tool_search",
        json!({"query": "select:read_mcp_resource"}),
        &context,
    )
    .await;
    assert!(!refreshed_error && refreshed.contains("generation 1"));
    trace.lock().unwrap().push(PermissionTraceEvent::Unlock);
    let (resource_after_refresh, resource_after_refresh_error) = run_tool(
        "read_mcp_resource",
        json!({"uri": "mcp://local/plan59"}),
        &context,
    )
    .await;
    assert!(!resource_after_refresh_error && resource_after_refresh == "MCP-RESOURCE-59");
    let (web, web_error) = run_tool(
        "web_search",
        json!({"query": "plan59 local seam"}),
        &context,
    )
    .await;
    assert!(!web_error && web == "WEB-RESULT-59:plan59 local seam");
    assert_eq!(approver.count(), 4);
    assert_eq!(
        source.calls(),
        [
            "read_mcp_resource",
            "read_mcp_resource",
            "read_mcp_resource",
            "web_search"
        ]
    );

    let observed_trace = trace.lock().unwrap().clone();
    assert_eq!(
        observed_trace,
        [
            PermissionTraceEvent::Defer,
            PermissionTraceEvent::Unlock,
            PermissionTraceEvent::Approval,
            PermissionTraceEvent::Dispatch {
                tool: "read_mcp_resource".into(),
                generation: 0,
            },
            PermissionTraceEvent::Approval,
            PermissionTraceEvent::Dispatch {
                tool: "read_mcp_resource".into(),
                generation: 0,
            },
            PermissionTraceEvent::Defer,
            PermissionTraceEvent::Unlock,
            PermissionTraceEvent::Approval,
            PermissionTraceEvent::Dispatch {
                tool: "read_mcp_resource".into(),
                generation: 1,
            },
            PermissionTraceEvent::Approval,
            PermissionTraceEvent::Dispatch {
                tool: "web_search".into(),
                generation: 1,
            },
        ]
    );
    let permission_order = observed_trace[..4]
        .iter()
        .map(|event| match event {
            PermissionTraceEvent::Defer => "defer",
            PermissionTraceEvent::Unlock => "unlock",
            PermissionTraceEvent::Approval => "approval",
            PermissionTraceEvent::Dispatch { .. } => "dispatch",
        })
        .collect::<Vec<_>>();

    let denied_trace: PermissionTrace = Arc::new(Mutex::new(Vec::new()));
    let denied_source = Arc::new(SeamSource::new(Arc::clone(&denied_trace)));
    let denied_erased: Arc<dyn ToolSource> = denied_source.clone();
    let denied_approver = Arc::new(RecordingApprover::new(
        Arc::clone(&denied_trace),
        Decision::Deny,
    ));
    let mut denied_context = with_defer_threshold(
        test_ctx_with_sources(0, "plan59-mcp-web-denied", vec![denied_erased]),
        usize::MAX,
    );
    let mut denied_config = denied_context.cfg.test_clone();
    denied_config.permissions = Arc::new(
        Permissions::new(
            Mode::Manual,
            &PermissionRules::default(),
            root.path().to_path_buf(),
            Some(denied_approver.clone()),
        )
        .unwrap(),
    );
    denied_context.cfg = Arc::new(denied_config);
    let (_, denied_select_error) = run_tool(
        "tool_search",
        json!({"query": "select:read_mcp_resource"}),
        &denied_context,
    )
    .await;
    assert!(!denied_select_error);
    let (denied, denied_error) = run_tool(
        "read_mcp_resource",
        json!({"uri": "mcp://local/plan59"}),
        &denied_context,
    )
    .await;
    assert!(denied_error, "{denied}");
    assert_eq!(denied_approver.count(), 1);
    assert!(denied_source.calls().is_empty());
    assert_eq!(
        denied_trace.lock().unwrap().as_slice(),
        &[PermissionTraceEvent::Approval]
    );

    json!({
        "rust_scope": "seam-only",
        "sequence": ["locked resource", "tool_search", "resource success", "resource failure", "catalog refresh", "stale unlock rejection", "tool_search refresh", "resource success", "web search"],
        "source": "local ToolSource stub",
        "external_network": false,
        "mcp_transport": false,
        "selective_defer": true,
        "catalog_refresh": true,
        "permission_order": permission_order,
        "denied_before_dispatch": true,
        "resource_success": true,
        "resource_failure": true,
        "web_success": true,
        "source_call_count": 4,
    })
}

async fn ask_plan_workflow_headless_report() -> Value {
    let root = TempRoot::new("ask-plan-workflow");
    let (workflow_started_tx, workflow_started_rx) = oneshot::channel();
    let (workflow_release_tx, workflow_release_rx) = oneshot::channel();
    let workflow_provider = Provider::mock_scripted(vec![MockTurn::Gate {
        started: workflow_started_tx,
        release: workflow_release_rx,
        blocks: vec![AssistantBlock::Text {
            text: "MUST-NOT-DELIVER-WORKFLOW-59".into(),
        }],
    }]);
    let questioner = ScriptedQuestioner::answered(QuestionAnswer {
        question_index: 0,
        selected: vec![1],
        other: None,
        notes: Some("accepted locally".into()),
    });
    let permissions = Permissions::new(
        Mode::AcceptEdits,
        &PermissionRules::default(),
        root.path().to_path_buf(),
        Some(Arc::new(AllowApprover)),
    )
    .unwrap();
    let mut context = test_ctx(0, "plan59-ask-plan-workflow");
    let mut config = context.cfg.test_clone();
    config.cwd = root.path().to_path_buf();
    config.offload_dir = root.path().join("offload");
    config.set_test_provider(workflow_provider);
    config.questioner = Some(questioner);
    config.permissions = Arc::new(permissions);
    config.surface = SurfaceCapabilities {
        questions: true,
        plan_control: true,
        workflow: true,
        ..Default::default()
    };
    context.cfg = Arc::new(config);

    let question_input = json!({
        "questions": [{
            "question": "Proceed with the local workflow?",
            "header": "Proceed",
            "options": [
                {"label": "No", "description": "stop"},
                {"label": "Yes", "description": "continue"}
            ],
            "multiSelect": false
        }]
    });
    let (answer, answer_error) =
        run_tool("ask_user_question", question_input.clone(), &context).await;
    assert!(!answer_error && answer.contains("Yes") && answer.contains("accepted locally"));
    let (entered, entered_error) = run_tool("enter_plan_mode", json!({}), &context).await;
    assert!(!entered_error && entered.contains("Entered plan mode"));
    assert_eq!(context.cfg.permissions.mode(), Mode::Plan);

    let mut workflow_activity = context.cfg.inbox.subscribe_activity();
    let (workflow, workflow_error) = run_tool(
        "workflow",
        json!({
            "script": "export const meta = { name: 'plan59-chain', description: 'local acceptance', phases: [{ title: 'Run' }] }; phase('Run'); return { marker: 'WORKFLOW-RESULT-59' };"
        }),
        &context,
    )
    .await;
    assert!(!workflow_error && workflow.contains("Workflow ID: workflow-"));
    if context.cfg.background_executions.running_count() != 0 {
        tokio::time::timeout(Duration::from_secs(2), workflow_activity.changed())
            .await
            .expect("workflow did not finish")
            .expect("workflow activity channel closed");
    }
    let workflow_items = context.cfg.inbox.drain();
    let workflow_delivery_count = workflow_items.len();
    assert_eq!(workflow_delivery_count, 1);
    let [
        InboxItem::WorkflowResult {
            summary,
            output_path,
            ..
        },
    ] = workflow_items.as_slice()
    else {
        panic!("expected one workflow result: {workflow_items:?}")
    };
    assert!(summary.contains("WORKFLOW-RESULT-59"));
    assert_eq!(
        serde_json::from_slice::<Value>(&std::fs::read(output_path).unwrap()).unwrap(),
        json!({"marker": "WORKFLOW-RESULT-59"})
    );
    let plan = "1. ask\n2. run local workflow\n3. exit";
    let (exited, exited_error) = run_tool("exit_plan_mode", json!({"plan": plan}), &context).await;
    assert!(!exited_error && exited.contains("approved"));
    assert_eq!(context.cfg.permissions.mode(), Mode::AcceptEdits);

    let mut cancelled_context = context.clone();
    let mut cancelled_config = cancelled_context.cfg.test_clone();
    cancelled_config.questioner = Some(ScriptedQuestioner::cancelled());
    cancelled_context.cfg = Arc::new(cancelled_config);
    let (cancelled, cancelled_error) =
        run_tool("ask_user_question", question_input, &cancelled_context).await;
    assert!(!cancelled_error && cancelled.contains("cancelled"));

    let rejected_permissions = Permissions::new(
        Mode::Plan,
        &PermissionRules::default(),
        root.path().to_path_buf(),
        Some(Arc::new(DenyApprover)),
    )
    .unwrap();
    let mut rejected_context = test_ctx(0, "plan59-plan-rejected");
    let mut rejected_config = rejected_context.cfg.test_clone();
    rejected_config.permissions = Arc::new(rejected_permissions);
    rejected_config.surface.plan_control = true;
    rejected_context.cfg = Arc::new(rejected_config);
    let (rejected, rejected_error) = run_tool(
        "exit_plan_mode",
        json!({"plan": "rejected plan"}),
        &rejected_context,
    )
    .await;
    assert!(!rejected_error && rejected.contains("keep planning"));
    assert_eq!(rejected_context.cfg.permissions.mode(), Mode::Plan);

    let (live_workflow, live_workflow_error) = run_tool(
        "workflow",
        json!({
            "script": "export const meta = { name: 'plan59-live', description: 'shutdown acceptance', phases: [{ title: 'Run' }] }; phase('Run'); return await agent('remain live');"
        }),
        &context,
    )
    .await;
    assert!(!live_workflow_error && live_workflow.contains("Workflow ID: workflow-"));
    tokio::time::timeout(Duration::from_secs(2), workflow_started_rx)
        .await
        .expect("live workflow never sampled")
        .expect("live workflow start channel closed");
    assert_eq!(context.cfg.shutdown_background_work(&context.ui).await, 0);
    drop(workflow_release_tx);
    assert_eq!(context.cfg.background_executions.running_count(), 0);
    assert!(context.cfg.inbox.is_empty());

    let enabled = names(context.cfg.surface);
    let disabled = names(Default::default());
    for tool in [
        "ask_user_question",
        "enter_plan_mode",
        "exit_plan_mode",
        "workflow",
    ] {
        assert!(enabled.iter().any(|name| name == tool));
        assert!(disabled.iter().all(|name| name != tool));
    }
    assert!(
        enabled
            .iter()
            .all(|name| name != "pty" && name != "PTY" && name != "process_group")
    );

    json!({
        "rust_scope": "surface-gate",
        "sequence": ["ask_user_question", "enter_plan_mode", "workflow", "exit_plan_mode", "ask_user_question(cancel)", "exit_plan_mode(reject)", "workflow(live)", "session shutdown"],
        "question_answered": "Yes",
        "question_cancelled": true,
        "plan_rejected": true,
        "plan_modes": ["accept_edits", "plan", "accept_edits"],
        "workflow_delivery_count": workflow_delivery_count,
        "workflow_cancelled_on_shutdown": true,
        "registration_absent_when_disabled": true,
        "pty_registered": false,
        "pty_reason": "no native PTY surface in the accepted profile",
        "pty_cleanup": "n/a-no-native-surface",
        "shutdown_residue": 0,
    })
}

async fn notebook_lsp_file_report() -> Value {
    let root = TempRoot::new("notebook-lsp-file");
    let path = root.path().join("acceptance.ipynb");
    let initial = json!({
        "cells": [{
            "cell_type": "code",
            "execution_count": null,
            "id": "code-059",
            "metadata": {},
            "outputs": [],
            "source": "print('before')\n"
        }],
        "metadata": {},
        "nbformat": 4,
        "nbformat_minor": 5
    });
    std::fs::write(&path, serde_json::to_vec(&initial).unwrap()).unwrap();
    let mut context = test_ctx(0, "plan59-notebook-lsp-file");
    let mut config = context.cfg.test_clone();
    config.cwd = root.path().to_path_buf();
    config.permissions = Arc::new(
        Permissions::new(
            Mode::AcceptEdits,
            &PermissionRules::default(),
            root.path().to_path_buf(),
            None,
        )
        .unwrap(),
    );
    context.cfg = Arc::new(config);
    let absolute = path.to_string_lossy().into_owned();

    let (unread, unread_error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "code-059",
            "new_source": "print('unread')\n"
        }),
        &context,
    )
    .await;
    assert!(unread_error && unread.contains("not been read"));
    let (read, read_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!read_error && read.contains("code-059"));
    let mut external = initial.clone();
    external["metadata"]["external"] = json!(true);
    let external_bytes = serde_json::to_vec(&external).unwrap();
    std::fs::write(&path, &external_bytes).unwrap();
    let (stale, stale_error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "code-059",
            "new_source": "print('stale')\n"
        }),
        &context,
    )
    .await;
    assert!(stale_error && stale.contains("modified since read"));
    let (_, reread_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!reread_error);
    let (edited, edited_error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "code-059",
            "new_source": "print('accepted-59')\n"
        }),
        &context,
    )
    .await;
    assert!(!edited_error && edited.contains("Updated cell code-059"));
    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["cells"][0]["source"], "print('accepted-59')\n");
    assert_eq!(value["metadata"]["external"], true);

    let (_, final_read_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!final_read_error);
    let current = String::from_utf8(std::fs::read(&path).unwrap()).unwrap();
    let (_, write_error) = run_tool(
        "write_file",
        json!({"path": absolute, "content": current}),
        &context,
    )
    .await;
    assert!(!write_error);
    let (generic, generic_error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "code-059",
            "new_source": "print('generic')\n"
        }),
        &context,
    )
    .await;
    assert!(generic_error && generic.contains("not been read as a complete notebook"));
    let (_, cancel_read_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!cancel_read_error);
    let before_cancel = std::fs::read(&path).unwrap();
    context.cancel.cancel();
    let (cancelled, cancelled_error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "code-059",
            "new_source": "print('cancelled')\n"
        }),
        &context,
    )
    .await;
    assert!(cancelled_error && cancelled.contains("interrupted"));
    assert_eq!(std::fs::read(&path).unwrap(), before_cancel);

    let registered = names(Default::default());
    assert!(registered.iter().all(|name| name != "lsp" && name != "LSP"));
    let entries = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    assert_eq!(entries, [std::ffi::OsString::from("acceptance.ipynb")]);

    json!({
        "rust_scope": "negative-boundary",
        "sequence": ["notebook_edit unread", "read_file", "external modification", "notebook_edit stale", "read_file", "notebook_edit", "read_file", "write_file", "notebook_edit unqualified", "read_file", "notebook_edit cancelled"],
        "unread_blocked": true,
        "stale_read_blocked": true,
        "notebook_edit_succeeded": true,
        "notebook_cancelled": true,
        "generic_write_revoked_notebook_qualification": true,
        "unknown_fields_preserved": true,
        "lsp_registered": false,
        "lsp_reason": "no native LSP surface in the accepted profile",
        "lsp_process_cleanup": "n/a-no-native-surface",
        "temporary_residue": 0,
    })
}

#[tokio::test]
async fn emit_plan59_acceptance_report() {
    let report = json!({
        "schema_version": 1,
        "surface": "kloop-native",
        "scope": {
            "target": "claude-code-2.1.220",
            "platform": "darwin-arm64",
            "target_entrypoint": "local-cli",
            "kloop_entrypoint": "core-dispatch-test-harness",
            "team": false,
            "remote": false,
            "external_network": false,
        },
        "scenarios": {
            "file_search_bash_worktree": file_search_bash_worktree_report().await,
            "background_agent_scheduler": background_agent_scheduler_report().await,
            "mcp_web_toolsource": mcp_web_toolsource_report().await,
            "ask_plan_workflow_headless": ask_plan_workflow_headless_report().await,
            "notebook_lsp_file": notebook_lsp_file_report().await,
        },
    });
    if let Some(path) = std::env::var_os("KLOOP_PLAN59_ACCEPTANCE_REPORT") {
        assert_eq!(
            std::env::consts::OS,
            "macos",
            "Plan 59 report is macOS-only"
        );
        assert_eq!(
            std::env::consts::ARCH,
            "aarch64",
            "Plan 59 report is arm64-only"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .expect("acceptance report path must be new");
        serde_json::to_writer_pretty(&mut file, &report).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
    }
}
