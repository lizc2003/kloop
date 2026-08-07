use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use kloop_protocol::ContentBlock;
use serde_json::json;
use serde_json::Value;

use super::all_tool_defs;
use super::dispatch_tools;
use super::testutil::git_ctx;
use super::testutil::run_tool;
use super::testutil::temp_git_repo;
use super::testutil::test_ctx;
use crate::agent::Ui;
use crate::event::Event;
use crate::permissions::Approver;
use crate::permissions::ConfirmRequest;
use crate::permissions::Decision;
use crate::permissions::Mode;
use crate::permissions::PermissionRules;
use crate::permissions::Permissions;

#[derive(Default)]
struct RecordingUi {
    events: Mutex<Vec<Event>>,
}

impl RecordingUi {
    fn cwd_events(&self) -> Vec<Value> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::CwdChanged { cwd, branch } => Some(json!({
                    "cwd": cwd,
                    "branch": branch,
                })),
                _ => None,
            })
            .collect()
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

    fn requests(&self) -> Vec<ConfirmRequest> {
        self.requests.lock().unwrap().clone()
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
            .expect("missing scripted approval decision");
        Box::pin(async move { decision })
    }
}

fn git(repository: &std::path::Path, arguments: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(crate::worktree::git_compatible_path(repository))
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn normalized(text: &str, repository: &std::path::Path) -> String {
    let normalized = text.replace(repository.to_string_lossy().as_ref(), "<REPO>");
    if cfg!(windows) {
        normalized.replace(std::path::MAIN_SEPARATOR, "/")
    } else {
        normalized
    }
}

fn result_values(results: Vec<ContentBlock>, repository: &std::path::Path) -> Vec<Value> {
    results
        .into_iter()
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
                "content": normalized(&content.as_text(), repository),
                "is_error": is_error,
            })
        })
        .collect()
}

fn schema_report() -> Value {
    let definitions = all_tool_defs(
        0,
        &[],
        30,
        crate::config::SurfaceCapabilities {
            worktree: true,
            ..Default::default()
        },
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let enter = definitions
        .iter()
        .find(|definition| definition.name == "enter_worktree")
        .unwrap();
    let exit = definitions
        .iter()
        .find(|definition| definition.name == "exit_worktree")
        .unwrap();
    assert_eq!(
        enter.schema,
        json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Optional name. Each '/'-separated segment may contain letters, digits, dots, underscores, and dashes; max 64 chars total."
                },
                "path": {
                    "type": "string",
                    "description": "Optional path to an existing registered worktree. Mutually exclusive with name."
                }
            },
            "additionalProperties": false
        })
    );
    assert_eq!(exit.schema["required"], json!(["action"]));
    assert_eq!(exit.schema["additionalProperties"], false);
    assert_eq!(
        exit.schema["properties"]["action"]["enum"],
        json!(["keep", "remove"])
    );
    assert_eq!(
        exit.schema["properties"]["discard_changes"]["type"],
        "boolean"
    );
    assert_eq!(
        exit.schema["properties"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["action", "discard_changes"]
    );
    json!({"enter": enter.schema, "exit": exit.schema})
}

async fn parser_report() -> Value {
    let repository = temp_git_repo("plan56-parser");
    let context = git_ctx(test_ctx(0, "plan56-parser"), &repository, true);
    let mut cases = Vec::new();
    for (label, tool, input) in [
        ("enter_name_null", "enter_worktree", json!({"name": null})),
        ("enter_path_null", "enter_worktree", json!({"path": null})),
        ("enter_name_type", "enter_worktree", json!({"name": 56})),
        ("enter_path_type", "enter_worktree", json!({"path": 56})),
        (
            "enter_both",
            "enter_worktree",
            json!({"name": "both", "path": "."}),
        ),
        ("enter_unknown", "enter_worktree", json!({"unknown": true})),
        ("exit_missing", "exit_worktree", json!({})),
        ("exit_action", "exit_worktree", json!({"action": "discard"})),
        (
            "exit_discard_null",
            "exit_worktree",
            json!({"action": "remove", "discard_changes": null}),
        ),
        (
            "exit_discard_type",
            "exit_worktree",
            json!({"action": "remove", "discard_changes": "yes"}),
        ),
        (
            "exit_unknown",
            "exit_worktree",
            json!({"action": "keep", "unknown": true}),
        ),
    ] {
        let (output, is_error) = run_tool(tool, input, &context).await;
        assert!(is_error, "{label}: {output}");
        cases.push(json!({
            "case": label,
            "content": normalized(&output, &repository),
            "is_error": is_error,
        }));
    }

    let (created, is_error) = run_tool(
        "enter_worktree",
        json!({"name": "feature/plan56"}),
        &context,
    )
    .await;
    assert!(!is_error, "{created}");
    let nested_path = repository.join(".claude/worktrees/feature+plan56");
    assert_eq!(context.cfg.effective_cwd(), nested_path);
    let (removed, is_error) =
        run_tool("exit_worktree", json!({"action": "remove"}), &context).await;
    assert!(!is_error, "{removed}");

    let (default_created, is_error) = run_tool("enter_worktree", json!({}), &context).await;
    assert!(!is_error, "{default_created}");
    let generated_path = context.cfg.effective_cwd();
    assert!(generated_path.starts_with(repository.join(".claude/worktrees")));
    let (_, is_error) = run_tool("exit_worktree", json!({"action": "remove"}), &context).await;
    assert!(!is_error);

    let _ = std::fs::remove_dir_all(&repository);
    json!({
        "invalid": cases,
        "nested": {
            "created": normalized(&created, &repository),
            "removed": normalized(&removed, &repository),
            "path": "<REPO>/.claude/worktrees/feature+plan56",
        },
        "default_generated": true,
    })
}

async fn lifecycle_report() -> Value {
    let repository = temp_git_repo("plan56-life");
    let ui = Arc::new(RecordingUi::default());
    let mut context = git_ctx(test_ctx(0, "plan56-life"), &repository, true);
    context.ui = ui.clone();

    let calls = vec![
        (
            "toolu_plan56_enter".into(),
            "enter_worktree".into(),
            json!({"name": "serial"}),
        ),
        (
            "toolu_plan56_exit".into(),
            "exit_worktree".into(),
            json!({"action": "remove"}),
        ),
    ];
    let results = dispatch_tools(calls, &context).await;
    assert!(results.iter().all(|result| matches!(
        result,
        ContentBlock::ToolResult {
            is_error: false,
            ..
        }
    )));
    assert_eq!(context.cfg.effective_cwd(), repository);
    assert!(!repository.join(".claude/worktrees/serial").exists());
    let cwd_events = ui.cwd_events();
    assert_eq!(cwd_events.len(), 2);

    let (entered, is_error) =
        run_tool("enter_worktree", json!({"name": "ignored"}), &context).await;
    assert!(!is_error, "{entered}");
    let worktree = context.cfg.effective_cwd();
    std::fs::write(worktree.join(".gitignore"), "ignored.txt\n").unwrap();
    git(&worktree, &["add", ".gitignore"]);
    git(&worktree, &["commit", "-qm", "ignore"]);
    std::fs::write(worktree.join("ignored.txt"), "ignored\n").unwrap();
    let (refused, is_error) =
        run_tool("exit_worktree", json!({"action": "remove"}), &context).await;
    assert!(is_error, "ignored content must fail closed: {refused}");
    assert_eq!(context.cfg.effective_cwd(), worktree);
    let (discarded, is_error) = run_tool(
        "exit_worktree",
        json!({"action": "remove", "discard_changes": true}),
        &context,
    )
    .await;
    assert!(!is_error, "{discarded}");

    let report = json!({
        "same_round": {
            "results": result_values(results, &repository),
            "cwd_events": cwd_events.into_iter().map(|event| {
                let mut event = event;
                if let Some(cwd) = event["cwd"].as_str() {
                    event["cwd"] = Value::String(normalized(cwd, &repository));
                }
                event
            }).collect::<Vec<_>>(),
            "residue": false,
        },
        "ignored_file": {
            "refused": normalized(&refused, &repository),
            "discarded": normalized(&discarded, &repository),
            "active_after_refusal": true,
        },
    });
    let _ = std::fs::remove_dir_all(repository);
    report
}

async fn ownership_report() -> Value {
    let repository = temp_git_repo("plan56-owner");
    let external = repository.join("external");
    let external_git = crate::worktree::git_compatible_path(&external);
    git(
        &repository,
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "external-branch",
            external_git.to_str().unwrap(),
            "HEAD",
        ],
    );
    let context = git_ctx(test_ctx(0, "plan56-owner"), &repository, true);
    let (entered, is_error) = run_tool("enter_worktree", json!({"path": external}), &context).await;
    assert!(!is_error, "{entered}");
    let (refused, is_error) = run_tool(
        "exit_worktree",
        json!({"action": "remove", "discard_changes": true}),
        &context,
    )
    .await;
    assert!(is_error, "{refused}");
    assert!(external.exists());
    assert!(context.cfg.active_worktree.read().unwrap().is_some());
    let (kept, is_error) = run_tool("exit_worktree", json!({"action": "keep"}), &context).await;
    assert!(!is_error, "{kept}");
    assert!(external.exists());
    let report = json!({
        "entered": normalized(&entered, &repository),
        "remove_refused": normalized(&refused, &repository),
        "kept": normalized(&kept, &repository),
        "external_exists": true,
    });
    let _ = std::fs::remove_dir_all(repository);
    report
}

async fn permission_report() -> Value {
    let approver = ScriptedApprover::new([
        Decision::Allow(crate::permissions::ApprovalScope::Once),
        Decision::Allow(crate::permissions::ApprovalScope::Once),
    ]);
    let permissions = Permissions::new(
        Mode::Manual,
        &PermissionRules::default(),
        std::path::PathBuf::from("/work"),
        Some(approver.clone()),
    )
    .unwrap();
    permissions
        .check("enter_worktree", &json!({"name": "safe"}), 0)
        .await
        .unwrap();
    permissions
        .check("enter_worktree", &json!({"path": "/work/existing"}), 0)
        .await
        .unwrap();
    permissions
        .check("exit_worktree", &json!({"action": "keep"}), 0)
        .await
        .unwrap();
    permissions
        .check("exit_worktree", &json!({"action": "remove"}), 0)
        .await
        .unwrap();
    let requests = approver.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].description.contains("existing worktree"));
    assert!(requests[1].description.contains("destructive"));

    let shared_approver = ScriptedApprover::new([
        Decision::Allow(crate::permissions::ApprovalScope::WorkspaceSession),
        Decision::Allow(crate::permissions::ApprovalScope::Once),
    ]);
    let base = Permissions::new(
        Mode::Manual,
        &PermissionRules::default(),
        std::path::PathBuf::from("/work"),
        Some(shared_approver.clone()),
    )
    .unwrap();
    let rebased = base.for_workspace(crate::project::WorkspaceIdentity::resolve(
        std::path::Path::new("/work/tree"),
    ));
    rebased
        .check("bash", &json!({"command": "cargo build"}), 0)
        .await
        .unwrap();
    base.check("bash", &json!({"command": "cargo build --release"}), 0)
        .await
        .unwrap();
    assert_eq!(shared_approver.requests().len(), 2);

    json!({
        "automatic": ["enter_name", "exit_keep"],
        "asked": requests.into_iter().map(|request| request.description).collect::<Vec<_>>(),
        "workspace_session_isolated_after_rebase": true,
    })
}

async fn provenance_report() -> Value {
    let repository = temp_git_repo("plan56-provenance");
    let context = git_ctx(test_ctx(0, "plan56-provenance"), &repository, true);
    let (_, is_error) = run_tool("enter_worktree", json!({"name": "provenance"}), &context).await;
    assert!(!is_error);
    let worktree = context.cfg.effective_cwd();
    git(&worktree, &["branch", "-m", "replacement-branch"]);
    let (output, is_error) = run_tool(
        "exit_worktree",
        json!({"action": "remove", "discard_changes": true}),
        &context,
    )
    .await;
    assert!(is_error, "{output}");
    assert!(output.contains("branch identity changed"), "{output}");
    assert_eq!(context.cfg.effective_cwd(), worktree);
    assert!(worktree.exists());
    assert!(context.cfg.active_worktree.read().unwrap().is_some());
    let report = json!({
        "remove_refused": normalized(&output, &repository),
        "active_after_refusal": true,
        "cwd_preserved": true,
        "worktree_exists": true,
    });
    let _ = std::fs::remove_dir_all(repository);
    report
}

async fn no_active_report() -> Value {
    let repository = temp_git_repo("plan56-no-active");
    let ui = Arc::new(RecordingUi::default());
    let mut context = git_ctx(test_ctx(0, "plan56-no-active"), &repository, true);
    context.ui = ui.clone();
    let (output, is_error) = run_tool("exit_worktree", json!({"action": "keep"}), &context).await;
    assert!(is_error, "{output}");
    let cwd_events = ui.cwd_events();
    assert!(cwd_events.is_empty());
    assert_eq!(context.cfg.effective_cwd(), repository);
    let report = json!({
        "result": normalized(&output, &repository),
        "is_error": true,
        "cwd_events": cwd_events,
        "cwd_unchanged": true,
    });
    let _ = std::fs::remove_dir_all(repository);
    report
}

async fn effective_context_report() -> Value {
    let repository = temp_git_repo("plan56-context");
    let mut context = git_ctx(test_ctx(0, "plan56-context"), &repository, true);
    let mut config = context.cfg.test_clone();
    config.system = format!(
        "# Environment\n- Working directory: {}",
        repository.display()
    );
    context.cfg = Arc::new(config);

    let (_, is_error) = run_tool("enter_worktree", json!({"name": "context"}), &context).await;
    assert!(!is_error);
    let worktree = context.cfg.effective_cwd();
    let system_reanchored = context
        .cfg
        .effective_system()
        .contains(&format!("Working directory: {}", worktree.display()));
    assert!(system_reanchored);

    let mut successful_tools = Vec::new();
    let (output, is_error) = run_tool(
        "write_file",
        json!({"path": "context.txt", "content": "alpha\n"}),
        &context,
    )
    .await;
    assert!(!is_error, "{output}");
    successful_tools.push("write_file");
    let (output, is_error) = run_tool("read_file", json!({"path": "context.txt"}), &context).await;
    assert!(!is_error && output.contains("alpha"), "{output}");
    successful_tools.push("read_file");
    let (output, is_error) = run_tool(
        "edit_file",
        json!({"path": "context.txt", "old_string": "alpha", "new_string": "beta"}),
        &context,
    )
    .await;
    assert!(!is_error, "{output}");
    successful_tools.push("edit_file");
    let (output, is_error) = run_tool("glob", json!({"pattern": "*.txt"}), &context).await;
    assert!(!is_error && output.contains("context.txt"), "{output}");
    successful_tools.push("glob");
    let (output, is_error) = run_tool(
        "grep",
        json!({"pattern": "beta", "path": ".", "output_mode": "files_with_matches"}),
        &context,
    )
    .await;
    assert!(!is_error && output.contains("context.txt"), "{output}");
    successful_tools.push("grep");
    let pwd_command = if cfg!(windows) { "pwd -W" } else { "pwd" };
    let (output, is_error) = run_tool("bash", json!({"command": pwd_command}), &context).await;
    let output = output.replace('\\', "/");
    let expected = crate::worktree::git_compatible_path(&worktree)
        .to_string_lossy()
        .replace('\\', "/");
    assert!(!is_error && output.contains(&expected), "{output}");
    successful_tools.push("bash");
    assert!(!repository.join("context.txt").exists());

    let (_, is_error) = run_tool(
        "exit_worktree",
        json!({"action": "remove", "discard_changes": true}),
        &context,
    )
    .await;
    assert!(!is_error);
    let report = json!({
        "system_reanchored": system_reanchored,
        "successful_tools": successful_tools,
        "main_tree_untouched": true,
        "worktree": "<REPO>/.claude/worktrees/context",
    });
    let _ = std::fs::remove_dir_all(repository);
    report
}

async fn existing_path_report() -> Value {
    let repository = temp_git_repo("plan56-path");
    let context = git_ctx(test_ctx(0, "plan56-path"), &repository, true);
    let (missing, is_error) = run_tool(
        "enter_worktree",
        json!({"path": repository.join("missing")}),
        &context,
    )
    .await;
    assert!(
        is_error && missing.contains("Cannot enter worktree"),
        "{missing}"
    );
    let unregistered = repository.join("unregistered");
    std::fs::create_dir(&unregistered).unwrap();
    let (output, is_error) =
        run_tool("enter_worktree", json!({"path": unregistered}), &context).await;
    assert!(
        is_error && output.contains("not a registered worktree"),
        "{output}"
    );
    assert_eq!(context.cfg.effective_cwd(), repository);
    let report = json!({
        "missing_refused": normalized(&missing, &repository),
        "unregistered_refused": normalized(&output, &repository),
        "cwd_unchanged": true,
    });
    let _ = std::fs::remove_dir_all(repository);
    report
}

#[tokio::test]
async fn provenance_mismatch_preserves_active_tree() {
    provenance_report().await;
}

#[tokio::test]
async fn no_active_exit_emits_no_cwd_transition() {
    no_active_report().await;
}

#[tokio::test]
async fn effective_context_reanchors_file_search_bash_and_system() {
    effective_context_report().await;
}

#[tokio::test]
async fn existing_path_must_exist_and_be_registered() {
    existing_path_report().await;
}

#[tokio::test]
async fn emit_plan56_parity_report() {
    let report = json!({
        "schema_version": 1,
        "surface": "kloop-native",
        "scenarios": {
            "schema": schema_report(),
            "parser": parser_report().await,
            "lifecycle": lifecycle_report().await,
            "ownership": ownership_report().await,
            "permission": permission_report().await,
            "provenance": provenance_report().await,
            "no_active": no_active_report().await,
            "effective_context": effective_context_report().await,
            "existing_path": existing_path_report().await,
        },
    });
    if let Some(path) = std::env::var_os("KLOOP_PLAN56_PARITY_REPORT") {
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
