use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;

use serde_json::json;
use serde_json::Value;

use super::dispatch_tools;
use super::testutil::test_ctx;
use super::ToolCtx;
use crate::agent::Ui;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use kloop_protocol::ContentBlock;

static TEMP_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

struct TempRoot(PathBuf);

impl TempRoot {
    fn new(tag: &str) -> Self {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-plan50-parity-{tag}-{}-{sequence}",
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

fn ctx_at(root: &Path, ui: Arc<RecordingUi>) -> ToolCtx {
    let mut ctx = test_ctx(0, "plan50-parity");
    let mut config = (*ctx.cfg).clone();
    config.cwd = root.to_path_buf();
    ctx.cfg = Arc::new(config);
    ctx.ui = ui;
    ctx
}

fn status_name(status: ItemStatus) -> &'static str {
    match status {
        ItemStatus::InProgress => "in_progress",
        ItemStatus::Completed => "completed",
        ItemStatus::Failed => "failed",
    }
}

fn project_events(events: Vec<Event>) -> Vec<Value> {
    events
        .into_iter()
        .filter_map(|event| match event {
            Event::ItemStarted {
                id,
                item: Item::ToolCall { name, status, .. },
            } => Some(json!({
                "phase": "start",
                "tool_use_id": id,
                "tool_name": if name == "bash" { "Bash" } else { &name },
                "status": status_name(status),
            })),
            Event::ItemCompleted {
                id,
                item: Item::ToolCall { name, status, .. },
            } => Some(json!({
                "phase": "finish",
                "tool_use_id": id,
                "tool_name": if name == "bash" { "Bash" } else { &name },
                "status": status_name(status),
            })),
            _ => None,
        })
        .collect()
}

fn normalize_text(text: &str, root: &Path) -> String {
    let resolved = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    text.replace(&resolved.display().to_string(), "<WORKSPACE>")
        .replace(&root.display().to_string(), "<WORKSPACE>")
}

fn project_calls(calls: &[(String, String, Value)], root: &Path) -> Vec<Value> {
    calls
        .iter()
        .map(|(tool_use_id, name, input)| {
            let mut input = input.clone();
            if let Some(command) = input["command"].as_str() {
                input["command"] = Value::String(normalize_text(command, root));
            }
            json!({
                "tool_use_id": tool_use_id,
                "tool_name": if name == "bash" { "Bash" } else { name },
                "input": input,
            })
        })
        .collect()
}

fn project_results(results: &[ContentBlock], root: &Path) -> Vec<Value> {
    results
        .iter()
        .map(|result| match result {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "tool_use_id": tool_use_id,
                "content": normalize_text(content.as_text().as_ref(), root),
                "is_error": is_error,
            }),
            _ => panic!("expected tool result"),
        })
        .collect()
}

fn assert_success(results: &[ContentBlock], expected_ids: &[&str]) {
    assert_eq!(results.len(), expected_ids.len());
    for (result, expected_id) in results.iter().zip(expected_ids) {
        let ContentBlock::ToolResult {
            tool_use_id,
            is_error,
            ..
        } = result
        else {
            panic!("expected tool result")
        };
        assert_eq!(tool_use_id, expected_id);
        assert!(!is_error);
    }
}

async fn bash_batching_report() -> Value {
    let root = TempRoot::new("batching");
    let ui = Arc::new(RecordingUi::default());
    let ctx = ctx_at(root.path(), ui.clone());

    let readonly_ids = ["toolu_plan50_bash_safe_pwd", "toolu_plan50_bash_safe_ls"];
    let readonly_calls = vec![
        (
            readonly_ids[0].into(),
            "bash".into(),
            json!({"command": "pwd"}),
        ),
        (
            readonly_ids[1].into(),
            "bash".into(),
            json!({"command": "ls -d ."}),
        ),
    ];
    let readonly_call_report = project_calls(&readonly_calls, root.path());
    let readonly = dispatch_tools(readonly_calls, &ctx).await;
    assert_success(&readonly, &readonly_ids);
    let readonly_events = project_events(ui.take());
    assert_eq!(readonly_events.len(), 4);
    assert!(readonly_events[..2]
        .iter()
        .all(|event| event["phase"] == "start"));
    assert!(readonly_events[2..]
        .iter()
        .all(|event| event["phase"] == "finish"));

    let unsafe_ids = ["toolu_plan50_bash_unsafe_a", "toolu_plan50_bash_unsafe_b"];
    let unsafe_calls = vec![
        (
            unsafe_ids[0].into(),
            "bash".into(),
            json!({
                "command": format!(
                    "printf 'A\\n' > '{}'",
                    root.path().join("bash-concurrency-a.txt").display()
                )
            }),
        ),
        (
            unsafe_ids[1].into(),
            "bash".into(),
            json!({
                "command": format!(
                    "printf 'B\\n' > '{}'",
                    root.path().join("bash-concurrency-b.txt").display()
                )
            }),
        ),
    ];
    let unsafe_call_report = project_calls(&unsafe_calls, root.path());
    let unsafe_results = dispatch_tools(unsafe_calls, &ctx).await;
    assert_success(&unsafe_results, &unsafe_ids);
    let unsafe_events = project_events(ui.take());
    assert_eq!(
        unsafe_events
            .iter()
            .map(|event| (
                event["phase"].as_str().unwrap(),
                event["tool_use_id"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("start", unsafe_ids[0]),
            ("finish", unsafe_ids[0]),
            ("start", unsafe_ids[1]),
            ("finish", unsafe_ids[1]),
        ]
    );

    let a = std::fs::read_to_string(root.path().join("bash-concurrency-a.txt")).unwrap();
    let b = std::fs::read_to_string(root.path().join("bash-concurrency-b.txt")).unwrap();
    assert_eq!((a.as_str(), b.as_str()), ("A\n", "B\n"));

    json!({
        "readonly": {
            "calls": readonly_call_report,
            "events": readonly_events,
            "results": project_results(&readonly, root.path()),
        },
        "unsafe": {
            "calls": unsafe_call_report,
            "events": unsafe_events,
            "results": project_results(&unsafe_results, root.path()),
        },
        "workspace": {
            "bash-concurrency-a.txt": a,
            "bash-concurrency-b.txt": b,
        },
    })
}

#[tokio::test]
async fn emit_plan50_parity_report() {
    let report = json!({
        "schema_version": 1,
        "scenarios": {
            "bash_batching": bash_batching_report().await,
        },
    });
    if let Some(path) = std::env::var_os("KLOOP_PLAN50_PARITY_REPORT") {
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
