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
            "kloop-plan49-parity-{tag}-{}-{sequence}",
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

fn ctx_at(root: &Path, ui: Arc<RecordingUi>, tag: &str) -> ToolCtx {
    let mut ctx = test_ctx(0, tag);
    let mut config = (*ctx.cfg).clone();
    config.cwd = root.to_path_buf();
    ctx.cfg = Arc::new(config);
    ctx.ui = ui;
    ctx
}

fn logical_name(name: &str) -> &str {
    match name {
        "read_file" => "Read",
        "write_file" => "Write",
        "edit_file" => "Edit",
        "glob" => "Glob",
        "grep" => "Grep",
        other => other,
    }
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
                "tool_name": logical_name(&name),
                "status": status_name(status),
            })),
            Event::ItemCompleted {
                id,
                item: Item::ToolCall { name, status, .. },
            } => Some(json!({
                "phase": "finish",
                "tool_use_id": id,
                "tool_name": logical_name(&name),
                "status": status_name(status),
            })),
            _ => None,
        })
        .collect()
}

fn project_calls(calls: &[(String, String, Value)]) -> Vec<Value> {
    calls
        .iter()
        .map(|(tool_use_id, name, input)| {
            json!({
                "tool_use_id": tool_use_id,
                "tool_name": logical_name(name),
                "input": input,
            })
        })
        .collect()
}

fn project_results(results: &[ContentBlock]) -> Vec<Value> {
    results
        .iter()
        .map(|result| match result {
            ContentBlock::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => json!({
                "tool_use_id": tool_use_id,
                "is_error": is_error,
            }),
            _ => panic!("expected tool result"),
        })
        .collect()
}

fn assert_success(results: &[ContentBlock], expected_ids: &[&str]) {
    let projected = project_results(results);
    assert_eq!(
        projected,
        expected_ids
            .iter()
            .map(|id| json!({"tool_use_id": id, "is_error": false}))
            .collect::<Vec<_>>()
    );
}

async fn file_search_report() -> Value {
    let root = TempRoot::new("search");
    let policy = root.path().join("policy");
    std::fs::create_dir_all(&policy).unwrap();
    std::fs::write(policy.join("visible-new.txt"), "alpha new\n").unwrap();
    std::fs::write(policy.join("visible-old.txt"), "alpha old\n").unwrap();
    std::fs::write(policy.join("mixed.txt"), "MIXED-ORIGINAL\n").unwrap();

    let ui = Arc::new(RecordingUi::default());
    let ctx = ctx_at(root.path(), ui.clone(), "parity-search");
    let readonly_ids = [
        "toolu_plan49_policy_read",
        "toolu_plan49_policy_glob",
        "toolu_plan49_policy_grep",
        "toolu_plan49_policy_read_old",
    ];
    let readonly_calls = vec![
        (
            readonly_ids[0].into(),
            "read_file".into(),
            json!({"path": "policy/visible-new.txt"}),
        ),
        (
            readonly_ids[1].into(),
            "glob".into(),
            json!({"pattern": "**/*", "path": "policy"}),
        ),
        (
            readonly_ids[2].into(),
            "grep".into(),
            json!({
                "pattern": "alpha",
                "path": "policy",
                "output_mode": "files_with_matches",
            }),
        ),
        (
            readonly_ids[3].into(),
            "read_file".into(),
            json!({"path": "policy/visible-old.txt"}),
        ),
    ];
    let readonly_call_report = project_calls(&readonly_calls);
    let readonly = dispatch_tools(readonly_calls, &ctx).await;
    assert_success(&readonly, &readonly_ids);
    let readonly_events = project_events(ui.take());
    assert_eq!(readonly_events.len(), 8);
    assert!(readonly_events[..4]
        .iter()
        .all(|event| event["phase"] == "start"));
    assert!(readonly_events[4..]
        .iter()
        .all(|event| event["phase"] == "finish"));

    let mixed_ids = [
        "toolu_plan49_policy_mixed_read",
        "toolu_plan49_policy_mixed_write",
    ];
    let mixed_calls = vec![
        (
            mixed_ids[0].into(),
            "read_file".into(),
            json!({"path": "policy/mixed.txt"}),
        ),
        (
            mixed_ids[1].into(),
            "write_file".into(),
            json!({"path": "policy/mixed.txt", "content": "MIXED-WRITTEN\n"}),
        ),
    ];
    let mixed_call_report = project_calls(&mixed_calls);
    let mixed = dispatch_tools(mixed_calls, &ctx).await;
    assert_success(&mixed, &mixed_ids);
    let mixed_events = project_events(ui.take());
    assert_eq!(
        mixed_events
            .iter()
            .map(|event| (
                event["phase"].as_str().unwrap(),
                event["tool_use_id"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("start", mixed_ids[0]),
            ("finish", mixed_ids[0]),
            ("start", mixed_ids[1]),
            ("finish", mixed_ids[1]),
        ]
    );
    let final_text = std::fs::read_to_string(policy.join("mixed.txt")).unwrap();
    assert_eq!(final_text, "MIXED-WRITTEN\n");

    json!({
        "readonly": {
            "calls": readonly_call_report,
            "events": readonly_events,
            "results": project_results(&readonly),
        },
        "mixed": {
            "calls": mixed_call_report,
            "events": mixed_events,
            "results": project_results(&mixed),
        },
        "workspace": {"policy/mixed.txt": final_text},
    })
}

async fn mutation_seriality_report() -> Value {
    let root = TempRoot::new("mutation");
    let serial = root.path().join("serial");
    std::fs::create_dir_all(&serial).unwrap();
    std::fs::write(serial.join("target.txt"), "A\n").unwrap();

    let ui = Arc::new(RecordingUi::default());
    let ctx = ctx_at(root.path(), ui.clone(), "parity-mutation");
    let read_id = "toolu_plan49_serial_read";
    let read_calls = vec![(
        read_id.into(),
        "read_file".into(),
        json!({"path": "serial/target.txt"}),
    )];
    let read_call_report = project_calls(&read_calls);
    let read = dispatch_tools(read_calls, &ctx).await;
    assert_success(&read, &[read_id]);
    let read_events = project_events(ui.take());

    let mutation_ids = [
        "toolu_plan49_serial_edit_a_b",
        "toolu_plan49_serial_edit_b_c",
        "toolu_plan49_serial_write_d",
        "toolu_plan49_serial_write_e",
    ];
    let mutation_calls = vec![
        (
            mutation_ids[0].into(),
            "edit_file".into(),
            json!({
                "path": "serial/target.txt",
                "old_string": "A",
                "new_string": "B",
            }),
        ),
        (
            mutation_ids[1].into(),
            "edit_file".into(),
            json!({
                "path": "serial/target.txt",
                "old_string": "B",
                "new_string": "C",
            }),
        ),
        (
            mutation_ids[2].into(),
            "write_file".into(),
            json!({"path": "serial/target.txt", "content": "D\n"}),
        ),
        (
            mutation_ids[3].into(),
            "write_file".into(),
            json!({"path": "serial/target.txt", "content": "E\n"}),
        ),
    ];
    let mutation_call_report = project_calls(&mutation_calls);
    let mutations = dispatch_tools(mutation_calls, &ctx).await;
    assert_success(&mutations, &mutation_ids);
    let mutation_events = project_events(ui.take());
    assert_eq!(
        mutation_events
            .iter()
            .map(|event| (
                event["phase"].as_str().unwrap(),
                event["tool_use_id"].as_str().unwrap()
            ))
            .collect::<Vec<_>>(),
        mutation_ids
            .iter()
            .flat_map(|id| [("start", *id), ("finish", *id)])
            .collect::<Vec<_>>()
    );
    let final_text = std::fs::read_to_string(serial.join("target.txt")).unwrap();
    assert_eq!(final_text, "E\n");

    json!({
        "read": {
            "calls": read_call_report,
            "events": read_events,
            "results": project_results(&read),
        },
        "mutations": {
            "calls": mutation_call_report,
            "events": mutation_events,
            "results": project_results(&mutations),
        },
        "workspace": {"serial/target.txt": final_text},
    })
}

#[tokio::test]
async fn emit_plan49_parity_report() {
    let report = json!({
        "schema_version": 2,
        "scenarios": {
            "file_search_batching": file_search_report().await,
            "mutation_seriality": mutation_seriality_report().await,
        },
    });
    if let Some(path) = std::env::var_os("KLOOP_PLAN49_PARITY_REPORT") {
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
