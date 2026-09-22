use std::collections::VecDeque;
use std::fs::OpenOptions;
use std::future::Future;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use kloop_protocol::ContentBlock;
use kloop_protocol::ToolResultContent;
use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use super::all_tool_defs;
use super::dispatch_tools;
use super::testutil::git_ctx;
use super::testutil::run_tool;
use super::testutil::temp_git_repo;
use super::testutil::test_ctx;
use crate::agent::Ui;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use crate::permissions::Approver;
use crate::permissions::ConfirmPreview;
use crate::permissions::ConfirmRequest;
use crate::permissions::Decision;
use crate::permissions::Mode;
use crate::permissions::PermissionRules;
use crate::permissions::Permissions;

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);
const PNG_BASE64: &str =
    "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-plan57-{tag}-{}-{sequence}",
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
            .unwrap_or(Decision::Deny);
        Box::pin(async move { decision })
    }
}

fn ctx_at(root: &Path, ui: Arc<dyn Ui>, tag: &str) -> ToolCtx {
    let mut context = test_ctx(0, tag);
    let mut config = context.cfg.test_clone();
    config.cwd = root.to_path_buf();
    context.cfg = Arc::new(config);
    context.ui = ui;
    context
}

fn notebook_text() -> String {
    format!(
        concat!(
            "{{\n",
            " \"nbformat\": 4,\n",
            " \"nbformat_minor\": 5,\n",
            " \"metadata\": {{\"language_info\": {{\"name\": \"python\"}}, \"keep\": true}},\n",
            " \"cells\": [\n",
            "  {{\"cell_type\":\"markdown\",\"id\":\"md-001\",\"metadata\":{{\"tag\":\"keep\"}},\"source\":[\"# Title\\n\",\"Text\\n\"],\"attachments\":{{\"ignored.png\":{{\"image/png\":\"{}\"}}}},\"unknown\":1}},\n",
            "  {{\"cell_type\":\"code\",\"id\":\"code-002\",\"metadata\":{{\"collapsed\":false}},\"source\":[\"print('hi')\\n\"],\"execution_count\":7,\"outputs\":[{{\"output_type\":\"stream\",\"name\":\"stdout\",\"text\":[\"hi\\n\"]}},{{\"output_type\":\"execute_result\",\"execution_count\":7,\"data\":{{\"text/plain\":[\"'hi'\\n\"]}},\"metadata\":{{}}}},{{\"output_type\":\"display_data\",\"data\":{{\"image/png\":\"{}\"}},\"metadata\":{{}}}}],\"unknown\":2}},\n",
            "  {{\"cell_type\":\"raw\",\"id\":\"raw-003\",\"metadata\":{{}},\"source\":[\"raw\\n\"]}},\n",
            "  {{\"cell_type\":\"markdown\",\"metadata\":{{\"fallback\":true}},\"source\":\"missing id\\n\"}}\n",
            " ],\n",
            " \"unknown_top\": {{\"preserve\": true}}\n",
            "}}\n"
        ),
        PNG_BASE64, PNG_BASE64
    )
}

fn write_notebook(root: &Path, name: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, notebook_text()).unwrap();
    path
}

async fn call(name: &str, input: Value, context: &ToolCtx) -> ContentBlock {
    dispatch_tools(
        vec![(format!("toolu_plan57_{name}"), name.to_string(), input)],
        context,
    )
    .await
    .into_iter()
    .next()
    .unwrap()
}

fn result_parts(result: ContentBlock) -> (ToolResultContent, bool) {
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        panic!("expected tool result")
    };
    (content, is_error)
}

fn schema_report() -> Value {
    let definitions = all_tool_defs(
        0,
        &[],
        30,
        Default::default(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    let notebook = definitions
        .iter()
        .find(|definition| definition.name == "notebook_edit")
        .unwrap();
    assert_eq!(
        notebook.schema["required"],
        json!(["notebook_path", "new_source"])
    );
    assert_eq!(notebook.schema["additionalProperties"], false);
    assert_eq!(
        notebook.schema["properties"]["cell_type"]["enum"],
        json!(["code", "markdown"])
    );
    assert_eq!(
        notebook.schema["properties"]["edit_mode"]["enum"],
        json!(["replace", "insert", "delete"])
    );
    let depth_one = all_tool_defs(
        1,
        &[],
        30,
        Default::default(),
        &crate::shell_programs::ShellPrograms::native_posix(),
    );
    assert!(
        depth_one
            .iter()
            .any(|definition| definition.name == "notebook_edit")
    );
    json!({
        "name": notebook.name,
        "schema": notebook.schema,
        "depth_one": true,
    })
}

async fn read_report() -> Value {
    let root = TempRoot::new("read");
    let path = write_notebook(root.path(), "fixture.ipynb");
    let context = ctx_at(root.path(), Arc::new(RecordingUi::default()), "plan57-read");
    let result = call(
        "read_file",
        json!({"path": path.to_string_lossy()}),
        &context,
    )
    .await;
    let (content, is_error) = result_parts(result);
    assert!(!is_error);
    let ToolResultContent::Blocks(blocks) = &content else {
        panic!("rich notebook must return multimodal blocks")
    };
    assert_eq!(blocks.len(), 3);
    let ContentBlock::Text { text: before_image } = &blocks[0] else {
        panic!("first block must be notebook text")
    };
    let ContentBlock::Image { .. } = &blocks[1] else {
        panic!("second block must be code output image")
    };
    let ContentBlock::Text { text: after_image } = &blocks[2] else {
        panic!("third block must be trailing cell text")
    };
    assert!(before_image.contains("<cell id=\"md-001\"><cell_type>markdown</cell_type>"));
    assert!(before_image.contains("<cell id=\"code-002\">"));
    assert!(after_image.contains("<cell id=\"raw-003\"><cell_type>raw</cell_type>"));
    assert!(after_image.contains("<cell id=\"cell-3\"><cell_type>markdown</cell_type>"));
    let key = std::fs::canonicalize(&path).unwrap();
    let observation = context.cfg.file_state.observation(&key).unwrap();
    assert!(observation.is_complete() && observation.is_notebook());

    json!({
        "block_kinds": blocks.iter().map(|block| match block {
            ContentBlock::Text { .. } => "text",
            ContentBlock::Image { .. } => "image",
            _ => "unexpected",
        }).collect::<Vec<_>>(),
        "fallback_id": "cell-3",
        "markdown_attachment_images": 0,
        "code_output_images": 1,
        "notebook_qualified": true,
    })
}

async fn mutation_report() -> Value {
    let root = TempRoot::new("mutations");
    let path = write_notebook(root.path(), "fixture.ipynb");
    let context = ctx_at(
        root.path(),
        Arc::new(RecordingUi::default()),
        "plan57-mutations",
    );
    let absolute = path.to_string_lossy().into_owned();
    let (_, error) = result_parts(call("read_file", json!({"path": absolute}), &context).await);
    assert!(!error);

    let (replace, error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "code-002",
            "new_source": "print('replaced')\n"
        }),
        &context,
    )
    .await;
    assert!(!error, "{replace}");
    let (insert, error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "md-001",
            "new_source": "## inserted\n",
            "cell_type": "markdown",
            "edit_mode": "insert"
        }),
        &context,
    )
    .await;
    assert!(!error, "{insert}");
    let (delete, error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": absolute,
            "cell_id": "raw-003",
            "new_source": "",
            "edit_mode": "delete"
        }),
        &context,
    )
    .await;
    assert!(!error, "{delete}");

    let value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(value["cells"][2]["source"], "print('replaced')\n");
    assert_eq!(value["cells"][2]["execution_count"], Value::Null);
    assert_eq!(value["cells"][2]["outputs"], json!([]));
    assert_eq!(value["cells"][2]["metadata"]["collapsed"], false);
    assert_eq!(value["unknown_top"]["preserve"], true);
    assert!(
        value["cells"]
            .as_array()
            .unwrap()
            .iter()
            .all(|cell| cell["id"] != "raw-003")
    );
    let inserted_id = value["cells"][1]["id"].as_str().unwrap();
    assert_eq!(inserted_id.len(), 8);
    assert!(
        inserted_id
            .chars()
            .all(|character| character.is_ascii_hexdigit())
    );
    assert!(!std::fs::read(&path).unwrap().ends_with(b"\n"));

    json!({
        "replace": replace,
        "insert_prefix": insert.starts_with("Inserted cell "),
        "insert_id_shape": "8-lower-hex",
        "delete": delete,
        "cell_ids": value["cells"].as_array().unwrap().iter().map(|cell| {
            cell["id"].as_str().unwrap_or("<missing>")
        }).collect::<Vec<_>>(),
        "code_execution_count": value["cells"][2]["execution_count"],
        "code_outputs": value["cells"][2]["outputs"],
        "unknown_fields_preserved": true,
        "trailing_newline": false,
    })
}

async fn guard_report() -> Value {
    let root = TempRoot::new("guards");
    let path = write_notebook(root.path(), "fixture.ipynb");
    let context = ctx_at(
        root.path(),
        Arc::new(RecordingUi::default()),
        "plan57-guards",
    );
    let absolute = path.to_string_lossy().into_owned();
    let original = std::fs::read(&path).unwrap();

    let (unread, unread_error) = run_tool(
        "notebook_edit",
        json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "x"}),
        &context,
    )
    .await;
    assert!(unread_error && unread.contains("File has not been read yet"));
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let (_, read_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!read_error);
    let mut external: Value = serde_json::from_slice(&original).unwrap();
    external["unknown_top"] = json!({"external": true});
    let external_bytes = serde_json::to_vec_pretty(&external).unwrap();
    std::fs::write(&path, &external_bytes).unwrap();
    let (stale, stale_error) = run_tool(
        "notebook_edit",
        json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "stale"}),
        &context,
    )
    .await;
    assert!(stale_error && stale.contains("File has been modified since read"));
    assert_eq!(std::fs::read(&path).unwrap(), external_bytes);

    let (_, reread_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!reread_error);
    let (missing, missing_error) = run_tool(
        "notebook_edit",
        json!({"notebook_path": absolute, "cell_id": "missing", "new_source": "x"}),
        &context,
    )
    .await;
    assert!(missing_error && missing.contains("not found in notebook"));
    assert_eq!(std::fs::read(&path).unwrap(), external_bytes);

    // Plan 57 cleared the notebook qualification on every failure, "conservatively";
    // plan 195 found what that cost. The refusal above never wrote, so the
    // complete-notebook read it was granted is still a true statement and the
    // retry lands. Before, one wrong `cell_id` was reported a second time as
    // never having read the file — one root cause, two diagnoses.
    let (retried, retry_error) = run_tool(
        "notebook_edit",
        json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "x"}),
        &context,
    )
    .await;
    assert!(!retry_error, "{retried}");
    assert_ne!(std::fs::read(&path).unwrap(), external_bytes);

    let (_, reread_error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!reread_error);
    let (_, write_error) = run_tool(
        "write_file",
        json!({
            "path": absolute,
            "content": String::from_utf8(external_bytes.clone()).unwrap()
        }),
        &context,
    )
    .await;
    assert!(!write_error);
    let (generic, generic_error) = run_tool(
        "notebook_edit",
        json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "x"}),
        &context,
    )
    .await;
    assert!(generic_error && generic.contains("not been read as a complete notebook"));

    json!({
        "unread": unread,
        "stale": stale,
        "missing": missing,
        "failure_kept_qualification": retried,
        "ordinary_mutation_revoked_qualification": generic,
        "failure_bytes_unchanged": true,
    })
}

fn project_events(events: Vec<Event>) -> Vec<Value> {
    events
        .into_iter()
        .filter_map(|event| match event {
            Event::ItemStarted {
                id,
                item: Item::ToolCall { name, status, .. },
            }
            | Event::ItemCompleted {
                id,
                item: Item::ToolCall { name, status, .. },
            } => Some(json!({
                "tool_use_id": id,
                "tool_name": name,
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

async fn seriality_report() -> Value {
    let root = TempRoot::new("seriality");
    let path = write_notebook(root.path(), "fixture.ipynb");
    let ui = Arc::new(RecordingUi::default());
    let context = ctx_at(root.path(), ui.clone(), "plan57-seriality");
    let absolute = path.to_string_lossy().into_owned();
    let (_, error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!error);
    ui.take();

    let results = dispatch_tools(
        vec![
            (
                "toolu_plan57_serial_code".into(),
                "notebook_edit".into(),
                json!({
                    "notebook_path": absolute,
                    "cell_id": "code-002",
                    "new_source": "serial code\n"
                }),
            ),
            (
                "toolu_plan57_serial_markdown".into(),
                "notebook_edit".into(),
                json!({
                    "notebook_path": absolute,
                    "cell_id": "md-001",
                    "new_source": "serial markdown\n"
                }),
            ),
        ],
        &context,
    )
    .await;
    assert!(results.iter().all(|result| matches!(
        result,
        ContentBlock::ToolResult {
            is_error: false,
            ..
        }
    )));
    let events = project_events(ui.take());
    assert_eq!(
        events
            .iter()
            .map(|event| (
                event["tool_use_id"].as_str().unwrap(),
                event["status"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("toolu_plan57_serial_code", "in_progress"),
            ("toolu_plan57_serial_code", "completed"),
            ("toolu_plan57_serial_markdown", "in_progress"),
            ("toolu_plan57_serial_markdown", "completed"),
        ]
    );
    json!({"events": events, "result_count": results.len()})
}

async fn permission_report() -> Value {
    let root = TempRoot::new("permission");
    let path = write_notebook(root.path(), "fixture.ipynb");
    let approver =
        ScriptedApprover::new([Decision::Allow(crate::permissions::ApprovalScope::Once)]);
    // The notebook lives inside the gate's cwd, so it is a contained write
    // that never asks; the ask rule is what a user writes to review every
    // edit, and what puts this contract back in front of the approver.
    let permissions = Arc::new(
        Permissions::new(
            Mode::Manual,
            &PermissionRules {
                ask: vec!["notebook_edit(**)".into()],
                ..Default::default()
            },
            root.path().to_path_buf(),
            Some(approver.clone()),
        )
        .unwrap(),
    );
    let mut context = ctx_at(
        root.path(),
        Arc::new(RecordingUi::default()),
        "plan57-permission",
    );
    let mut config = context.cfg.test_clone();
    config.permissions = permissions;
    context.cfg = Arc::new(config);
    let absolute = path.to_string_lossy().into_owned();
    std::fs::create_dir(root.path().join("sub")).unwrap();
    let approval_alias = root
        .path()
        .join("sub")
        .join("..")
        .join("fixture.ipynb")
        .to_string_lossy()
        .into_owned();
    let (_, error) = run_tool("read_file", json!({"path": absolute}), &context).await;
    assert!(!error);
    let (edited, error) = run_tool(
        "notebook_edit",
        json!({"notebook_path": approval_alias, "cell_id": "code-002", "new_source": "approved\n"}),
        &context,
    )
    .await;
    assert!(!error, "{edited}");
    let requests = approver.requests();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].description.contains("notebook_edit"));
    assert!(requests[0].description.contains(&absolute));
    assert!(!requests[0].description.contains("/sub/../"));
    assert!(requests[0].preview.as_ref().is_some_and(|preview| {
        preview
            .text()
            .starts_with("(replace notebook cell code-002)")
    }));

    let contained = Permissions::new(
        Mode::Manual,
        &PermissionRules::default(),
        root.path().to_path_buf(),
        None,
    )
    .unwrap();
    assert!(
        contained
            .check(
                "notebook_edit",
                &json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "x"}),
                0,
            )
            .await
            .is_ok()
    );
    let plan = Permissions::new(
        Mode::Plan,
        &PermissionRules::default(),
        root.path().to_path_buf(),
        None,
    )
    .unwrap();
    let plan_error = plan
        .check(
            "notebook_edit",
            &json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "x"}),
            0,
        )
        .await
        .unwrap_err();
    let deny = Permissions::new(
        Mode::Manual,
        &PermissionRules {
            deny: vec!["notebook_edit(**/*.ipynb)".into()],
            ..Default::default()
        },
        root.path().to_path_buf(),
        None,
    )
    .unwrap();
    let deny_error = deny
        .check(
            "notebook_edit",
            &json!({"notebook_path": absolute, "cell_id": "code-002", "new_source": "x"}),
            0,
        )
        .await
        .unwrap_err();

    json!({
        "approval_count": requests.len(),
        "description": requests[0].description,
        "preview": requests[0].preview.as_ref().map(ConfirmPreview::text),
        "accept_edits": true,
        "plan_blocked": plan_error.contains("plan mode"),
        "deny_blocked": deny_error.contains("deny permission rule"),
    })
}

fn git(repository: &Path, arguments: &[&str]) {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(arguments)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {}: {}",
        arguments.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
}

async fn worktree_report() -> Value {
    let repository = temp_git_repo("plan57-worktree");
    let main_path = write_notebook(&repository, "fixture.ipynb");
    git(&repository, &["add", "fixture.ipynb"]);
    git(&repository, &["commit", "-qm", "notebook fixture"]);
    let main_before = std::fs::read(&main_path).unwrap();
    let context = git_ctx(test_ctx(0, "plan57-worktree"), &repository, true);
    let (_, error) = run_tool(
        "read_file",
        json!({"path": main_path.to_string_lossy()}),
        &context,
    )
    .await;
    assert!(!error);
    let (_, error) = run_tool(
        "enter_worktree",
        json!({"name": "plan57-notebook"}),
        &context,
    )
    .await;
    assert!(!error);
    let worktree_path = context.cfg.effective_cwd().join("fixture.ipynb");
    let (unread, unread_error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": worktree_path.to_string_lossy(),
            "cell_id": "code-002",
            "new_source": "must read worktree\n"
        }),
        &context,
    )
    .await;
    assert!(unread_error && unread.contains("File has not been read yet"));
    let (_, error) = run_tool(
        "read_file",
        json!({"path": worktree_path.to_string_lossy()}),
        &context,
    )
    .await;
    assert!(!error);
    let (_, error) = run_tool(
        "notebook_edit",
        json!({
            "notebook_path": worktree_path.to_string_lossy(),
            "cell_id": "code-002",
            "new_source": "worktree only\n"
        }),
        &context,
    )
    .await;
    assert!(!error);
    assert_eq!(std::fs::read(&main_path).unwrap(), main_before);
    let worktree: Value = serde_json::from_slice(&std::fs::read(&worktree_path).unwrap()).unwrap();
    assert_eq!(worktree["cells"][1]["source"], "worktree only\n");
    let (_, error) = run_tool("exit_worktree", json!({"action": "keep"}), &context).await;
    assert!(!error);
    let _ = std::fs::remove_dir_all(&repository);
    json!({
        "main_unchanged": true,
        "worktree_changed": true,
        "qualification_leaked": false,
    })
}

async fn report() -> Value {
    json!({
        "schema_version": 1,
        "surface": "kloop-native",
        "scenarios": {
            "schema": schema_report(),
            "read": read_report().await,
            "mutations": mutation_report().await,
            "guards": guard_report().await,
            "seriality": seriality_report().await,
            "permission": permission_report().await,
            "worktree": worktree_report().await,
        }
    })
}

#[tokio::test]
async fn plan57_notebook_contract() {
    let report = report().await;
    assert_eq!(
        report["scenarios"]["read"]["block_kinds"],
        json!(["text", "image", "text"])
    );
}

#[tokio::test]
async fn emit_plan57_parity_report() {
    let report = report().await;
    if let Some(path) = std::env::var_os("KLOOP_PLAN57_PARITY_REPORT") {
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
