use std::sync::RwLock;

use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use kloop_protocol::ToolDef;
use serde::Serialize;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

use super::ToolCtx;
use crate::event::Event;

const MAX_TASKS: usize = 256;
const MAX_SUBJECT_CHARS: usize = 200;
const MAX_DIAGNOSTIC_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn parse(value: Option<&Value>) -> Result<Self> {
        match value.and_then(Value::as_str) {
            Some("pending") => Ok(Self::Pending),
            Some("in_progress") => Ok(Self::InProgress),
            Some("completed") => Ok(Self::Completed),
            Some(other) => bail!("task_write: unknown status `{}`", bounded_diagnostic(other)),
            None => bail!("task_write: every task needs a 'status' string"),
        }
    }
}

/// One row, in the single shape the model writes, the registry stores and the
/// TUI panel draws. Whole-table overwrite leaves nothing to address, so a row
/// carries no ID — plan 188 deleted the ID space, its high-water mark and the
/// epoch rollover that existed to reuse it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskGraphTask {
    pub subject: String,
    pub status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskGraphSnapshot {
    pub revision: u64,
    pub tasks: Vec<TaskGraphTask>,
}

#[derive(Default)]
struct TaskRegistryState {
    revision: u64,
    tasks: Vec<TaskGraphTask>,
}

#[derive(Default)]
pub struct TaskRegistry {
    state: RwLock<TaskRegistryState>,
}

impl TaskRegistry {
    /// Replace the whole table. Returns the live table, plus the snapshot to
    /// publish — `None` when the write changed nothing, so a model that
    /// restates the same list every round never flickers the panel.
    ///
    /// Everything is validated before the lock is taken and before a single
    /// row is stored: a rejected write leaves the previous table exactly as it
    /// was.
    fn write(
        &self,
        tasks: Vec<TaskGraphTask>,
    ) -> Result<(Vec<TaskGraphTask>, Option<TaskGraphSnapshot>)> {
        if tasks.len() > MAX_TASKS {
            bail!("task_write: task limit of {MAX_TASKS} exceeded");
        }
        for task in &tasks {
            validate_subject(&task.subject)?;
        }
        let mut state = self.state.write().unwrap();
        if tasks == state.tasks {
            return Ok((tasks, None));
        }
        let revision = next_revision(state.revision, "task_write")?;
        state.tasks = tasks;
        state.revision = revision;
        let published = graph_snapshot(&state);
        Ok((published.tasks.clone(), Some(published)))
    }

    pub fn snapshot(&self) -> TaskGraphSnapshot {
        graph_snapshot(&self.state.read().unwrap())
    }

    /// The user's half of the registry: `/clear` empties the table, and the
    /// model has no tool that does. The revision advances unconditionally —
    /// it is the fence the TUI uses to reject a late pre-clear snapshot.
    pub fn clear(&self) -> Result<(usize, TaskGraphSnapshot)> {
        let mut state = self.state.write().unwrap();
        let revision = next_revision(state.revision, "clear")?;
        let cleared_count = state.tasks.len();
        state.tasks.clear();
        state.revision = revision;
        Ok((cleared_count, graph_snapshot(&state)))
    }
}

fn next_revision(revision: u64, tool: &str) -> Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("{tool}: task graph revision exhausted"))
}

fn graph_snapshot(state: &TaskRegistryState) -> TaskGraphSnapshot {
    TaskGraphSnapshot {
        revision: state.revision,
        tasks: state.tasks.clone(),
    }
}

fn validate_subject(subject: &str) -> Result<()> {
    if subject.trim().is_empty() {
        bail!("task_write: subject must not be empty");
    }
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        bail!("task_write: subject exceeds the {MAX_SUBJECT_CHARS}-character limit");
    }
    if subject
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        bail!("task_write: subject must be a single line without control characters");
    }
    Ok(())
}

pub(super) fn task_write_def() -> ToolDef {
    ToolDef {
        name: "task_write".into(),
        description: "Record this session's task list. Every call replaces the whole list, so send every task you are still tracking with its current status; `[]` clears it. This records work only — it starts nothing, assigns nothing, and does not survive resume.".into(),
        schema: json!({
            "type": "object",
            "properties": {"tasks": {
                "type": "array",
                "maxItems": MAX_TASKS,
                "description": "The complete list, in the order it should be read",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string", "maxLength": MAX_SUBJECT_CHARS, "description": "Short single-line task title"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                    },
                    "required": ["subject", "status"],
                    "additionalProperties": false
                }
            }},
            "required": ["tasks"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_write_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let (tasks, snapshot) = ctx.cfg.tasks.write(parse_write(input)?)?;
    if let Some(snapshot) = snapshot {
        ctx.ui.emit(&Event::TaskGraphUpdated(snapshot));
    }
    Ok(json!({"tasks": tasks}).to_string())
}

fn parse_write(input: &Value) -> Result<Vec<TaskGraphTask>> {
    let object = strict_object(input, &["tasks"], "input")?;
    object
        .get("tasks")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("task_write: missing required array argument 'tasks'"))?
        .iter()
        .map(parse_task)
        .collect()
}

fn parse_task(row: &Value) -> Result<TaskGraphTask> {
    let object = strict_object(row, &["subject", "status"], "each task")?;
    let subject = object
        .get("subject")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("task_write: every task needs a 'subject' string"))?;
    Ok(TaskGraphTask {
        subject: subject.to_string(),
        status: TaskStatus::parse(object.get("status"))?,
    })
}

fn strict_object<'a>(
    value: &'a Value,
    allowed: &[&str],
    what: &str,
) -> Result<&'a Map<String, Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("task_write: {what} must be an object"))?;
    if let Some(unexpected) = object
        .keys()
        .find(|candidate| !allowed.contains(&candidate.as_str()))
    {
        bail!(
            "task_write: unknown field `{}` in {what}",
            bounded_diagnostic(unexpected)
        );
    }
    Ok(object)
}

fn bounded_diagnostic(value: &str) -> String {
    let mut output = String::new();
    for ch in value.chars().take(MAX_DIAGNOSTIC_CHARS) {
        let ch = if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
            ' '
        } else {
            ch
        };
        output.push(ch);
    }
    if value.chars().count() > MAX_DIAGNOSTIC_CHARS {
        output.push('…');
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Ui;
    use crate::event::Item;
    use crate::tools::background_executions::ExecutionKind;
    use crate::tools::background_executions::ExecutionStatus;
    use crate::tools::testutil::run_tool;
    use crate::tools::testutil::test_ctx;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[derive(Default)]
    struct RecordingUi(Mutex<Vec<Event>>);

    impl Ui for RecordingUi {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    impl RecordingUi {
        fn take(&self) -> Vec<Event> {
            std::mem::take(&mut *self.0.lock().unwrap())
        }
    }

    fn table(rows: &[(&str, TaskStatus)]) -> Vec<TaskGraphTask> {
        rows.iter()
            .map(|(subject, status)| TaskGraphTask {
                subject: (*subject).into(),
                status: *status,
            })
            .collect()
    }

    async fn write(ctx: &ToolCtx, tasks: Value) -> Value {
        let (output, is_error) = run_tool("task_write", json!({"tasks": tasks}), ctx).await;
        assert!(!is_error, "{output}");
        serde_json::from_str(&output).unwrap()
    }

    #[tokio::test]
    async fn every_write_replaces_the_whole_table_and_returns_it() {
        let ctx = test_ctx(0, "task-write");
        assert_eq!(
            write(
                &ctx,
                json!([
                    {"subject": "First", "status": "in_progress"},
                    {"subject": "Second", "status": "pending"},
                ]),
            )
            .await,
            json!({"tasks": [
                {"subject": "First", "status": "in_progress"},
                {"subject": "Second", "status": "pending"},
            ]})
        );

        // A row dropped, a row renamed, a status moved backwards, a row added:
        // the second write is the whole truth, not a patch against the first.
        assert_eq!(
            write(
                &ctx,
                json!([
                    {"subject": "Second", "status": "completed"},
                    {"subject": "Third", "status": "pending"},
                ]),
            )
            .await,
            json!({"tasks": [
                {"subject": "Second", "status": "completed"},
                {"subject": "Third", "status": "pending"},
            ]})
        );
        assert_eq!(
            write(&ctx, json!([{"subject": "Second", "status": "pending"}])).await,
            json!({"tasks": [{"subject": "Second", "status": "pending"}]})
        );

        assert_eq!(write(&ctx, json!([])).await, json!({"tasks": []}));
        assert_eq!(
            ctx.cfg.tasks.snapshot(),
            TaskGraphSnapshot {
                revision: 4,
                tasks: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn the_retired_tools_and_their_fields_are_gone() {
        let ctx = test_ctx(0, "task-retired");
        for name in [
            "task_create",
            "task_get",
            "task_update",
            "task_list",
            "task_clear",
            "todo_write",
        ] {
            let (output, is_error) = run_tool(name, json!({}), &ctx).await;
            assert!(is_error, "{name}: {output}");
        }

        for (field, row) in [
            (
                "id",
                json!({"subject": "x", "status": "pending", "id": "1"}),
            ),
            (
                "task_id",
                json!({"subject": "x", "status": "pending", "task_id": "1"}),
            ),
            (
                "description",
                json!({"subject": "x", "status": "pending", "description": "instructions"}),
            ),
            (
                "owner",
                json!({"subject": "x", "status": "pending", "owner": Value::Null}),
            ),
            (
                "blocked_by",
                json!({"subject": "x", "status": "pending", "blocked_by": ["2"]}),
            ),
        ] {
            let (output, is_error) = run_tool("task_write", json!({"tasks": [row]}), &ctx).await;
            assert!(is_error, "{field}: {output}");
            assert!(
                output.contains(&format!("unknown field `{field}`")),
                "{output}"
            );
        }
        assert!(ctx.cfg.tasks.snapshot().tasks.is_empty());
    }

    #[tokio::test]
    async fn strict_parsing_rejects_every_malformed_table() {
        let ctx = test_ctx(0, "task-strict");
        for input in [
            json!("not an object"),
            json!({}),
            json!({"task": []}),
            json!({"tasks": {}}),
            json!({"tasks": ["First"]}),
            json!({"tasks": [{"subject": "x"}]}),
            json!({"tasks": [{"status": "pending"}]}),
            json!({"tasks": [{"subject": "x", "status": "done"}]}),
            json!({"tasks": [{"subject": "x", "status": Value::Null}]}),
            json!({"tasks": [{"subject": 1, "status": "pending"}]}),
        ] {
            let (output, is_error) = run_tool("task_write", input.clone(), &ctx).await;
            assert!(is_error, "{input}: {output}");
        }
        assert!(ctx.cfg.tasks.snapshot().tasks.is_empty());
    }

    #[tokio::test]
    async fn budgets_and_bad_text_fail_before_the_table_moves() {
        let ctx = test_ctx(0, "task-budgets");
        write(&ctx, json!([{"subject": "Keep me", "status": "pending"}])).await;
        let before = ctx.cfg.tasks.snapshot();

        let oversized = (0..=MAX_TASKS)
            .map(|index| json!({"subject": format!("Task {index}"), "status": "pending"}))
            .collect::<Vec<_>>();
        for tasks in [
            json!([{"subject": "", "status": "pending"}]),
            json!([{"subject": "   ", "status": "pending"}]),
            json!([{"subject": "two\nlines", "status": "pending"}]),
            json!([{"subject": "x".repeat(MAX_SUBJECT_CHARS + 1), "status": "pending"}]),
            // The last row is the bad one: a partially applied write would
            // leave the earlier rows behind.
            json!([
                {"subject": "Fine", "status": "pending"},
                {"subject": "bad\u{2028}line", "status": "pending"},
            ]),
            Value::Array(oversized),
        ] {
            let (output, is_error) = run_tool("task_write", json!({"tasks": tasks}), &ctx).await;
            assert!(is_error, "{output}");
            assert_eq!(ctx.cfg.tasks.snapshot(), before);
        }

        // One row under the limits, and the whole budget's worth of them.
        write(
            &ctx,
            Value::Array(
                (0..MAX_TASKS)
                    .map(|index| json!({"subject": format!("Task {index}"), "status": "pending"}))
                    .collect(),
            ),
        )
        .await;
        assert_eq!(ctx.cfg.tasks.snapshot().tasks.len(), MAX_TASKS);
    }

    #[test]
    fn rewriting_the_same_table_publishes_nothing() {
        let registry = TaskRegistry::default();
        let rows = table(&[("First", TaskStatus::Pending)]);
        let (live, snapshot) = registry.write(rows.clone()).unwrap();
        assert_eq!(live, rows);
        assert_eq!(
            snapshot,
            Some(TaskGraphSnapshot {
                revision: 1,
                tasks: rows.clone(),
            })
        );

        let (live, snapshot) = registry.write(rows.clone()).unwrap();
        assert_eq!(live, rows);
        assert_eq!(snapshot, None);
        assert_eq!(registry.snapshot().revision, 1);

        // Order is part of the table: the same rows, moved, are a new revision.
        let (_, snapshot) = registry
            .write(table(&[
                ("Second", TaskStatus::Pending),
                ("First", TaskStatus::Pending),
            ]))
            .unwrap();
        assert_eq!(snapshot.unwrap().revision, 2);
        let (_, snapshot) = registry
            .write(table(&[
                ("First", TaskStatus::Pending),
                ("Second", TaskStatus::Pending),
            ]))
            .unwrap();
        assert_eq!(snapshot.unwrap().revision, 3);
    }

    #[test]
    fn revision_overflow_fails_before_mutation() {
        let registry = TaskRegistry::default();
        registry
            .write(table(&[("Existing", TaskStatus::Pending)]))
            .unwrap();
        registry.state.write().unwrap().revision = u64::MAX;
        let before = registry.snapshot();

        assert!(
            registry
                .write(table(&[("New", TaskStatus::Pending)]))
                .is_err()
        );
        assert_eq!(registry.snapshot(), before);
        assert!(registry.clear().is_err());
        assert_eq!(registry.snapshot(), before);

        // An unchanged rewrite never needs a revision, so it still succeeds.
        let (_, snapshot) = registry
            .write(table(&[("Existing", TaskStatus::Pending)]))
            .unwrap();
        assert_eq!(snapshot, None);
    }

    #[test]
    fn concurrent_writes_are_serialized_and_registries_stay_independent() {
        let registry = Arc::new(TaskRegistry::default());
        let handles = (0..64)
            .map(|index| {
                let registry = Arc::clone(&registry);
                std::thread::spawn(move || {
                    registry
                        .write(table(&[(&format!("Task {index}"), TaskStatus::Pending)]))
                        .unwrap()
                        .0
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap().len(), 1);
        }
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.revision, 64);

        let independent = TaskRegistry::default();
        assert_eq!(
            independent.snapshot(),
            TaskGraphSnapshot {
                revision: 0,
                tasks: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn mutations_emit_full_snapshots_inside_ordinary_tool_lifecycle() {
        let ui = Arc::new(RecordingUi::default());
        let mut ctx = test_ctx(0, "task-events");
        ctx.ui = ui.clone();

        let (output, is_error) = run_tool(
            "task_write",
            json!({"tasks": [{"subject": "Visible", "status": "pending"}]}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let events = ui.take();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[0],
            Event::ItemStarted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == "task_write"
        ));
        assert!(matches!(
            &events[1],
            Event::TaskGraphUpdated(snapshot)
                if snapshot.revision == 1 && snapshot.tasks[0].subject == "Visible"
        ));
        assert_eq!(events[1].as_note(), None);
        assert!(matches!(
            &events[2],
            Event::ItemCompleted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == "task_write"
        ));

        // A repeated identical write and a rejected write both leave the panel
        // alone: two lifecycle events, no snapshot.
        let before = ctx.cfg.tasks.snapshot();
        let quiet = |events: Vec<Event>| {
            assert_eq!(events.len(), 2);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TaskGraphUpdated(_)))
            );
        };
        let (output, is_error) = run_tool(
            "task_write",
            json!({"tasks": [{"subject": "Visible", "status": "pending"}]}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        quiet(ui.take());
        assert_eq!(ctx.cfg.tasks.snapshot(), before);

        for input in [
            json!({"tasks": [{"subject": "x".repeat(MAX_SUBJECT_CHARS + 1), "status": "pending"}]}),
            json!({"tasks": "not an array"}),
        ] {
            let (output, is_error) = run_tool("task_write", input, &ctx).await;
            assert!(is_error, "{output}");
            assert_eq!(ctx.cfg.tasks.snapshot(), before);
            quiet(ui.take());
        }
    }

    #[tokio::test]
    async fn clear_changes_only_the_task_registry() {
        let ctx = test_ctx(0, "task-clear-scope");
        let executions = [
            (ExecutionKind::Agent, "agent-201"),
            (ExecutionKind::Program, "program-201"),
            (ExecutionKind::Workflow, "workflow-201"),
        ];
        for (kind, id) in executions {
            ctx.cfg
                .background_executions
                .register(
                    kind,
                    id,
                    "must survive /clear",
                    tokio_util::sync::CancellationToken::new(),
                )
                .unwrap();
        }

        #[cfg(unix)]
        let bash_id = {
            let (output, is_error) = run_tool(
                "bash",
                json!({"command":"sleep 30","background":true}),
                &ctx,
            )
            .await;
            assert!(!is_error, "{output}");
            output
                .strip_prefix("Command running in background with ID: ")
                .and_then(|rest| rest.split('.').next())
                .expect("background Bash result has an id")
                .to_string()
        };

        write(&ctx, json!([{"subject": "Discarded", "status": "pending"}])).await;
        let (cleared_count, snapshot) = ctx.cfg.tasks.clear().unwrap();
        assert_eq!(cleared_count, 1);
        assert_eq!(
            snapshot,
            TaskGraphSnapshot {
                revision: 2,
                tasks: Vec::new(),
            }
        );
        assert_eq!(ctx.cfg.background_executions.running_count(), 3);
        #[cfg(unix)]
        assert_eq!(ctx.cfg.background_shells.running_count(), 1);

        #[cfg(unix)]
        {
            let (output, is_error) = run_tool("stop_bash", json!({"bash_id":bash_id}), &ctx).await;
            assert!(!is_error, "{output}");
        }
        for (_, id) in executions {
            assert_eq!(
                ctx.cfg
                    .background_executions
                    .finish(id, ExecutionStatus::Completed, |_, _| {}),
                Some(ExecutionStatus::Completed)
            );
        }
    }

    #[test]
    fn the_definition_is_one_strict_root_only_tool() {
        let definition = task_write_def();
        assert_eq!(definition.name, "task_write");
        // Plan 188's budget: five definitions cost 1597 characters, and the
        // model called none of them. One table, one call, one short paragraph.
        assert!(
            definition.description.chars().count() < 400,
            "{}",
            definition.description.chars().count()
        );
        assert_eq!(definition.schema["additionalProperties"], false);
        let row = &definition.schema["properties"]["tasks"]["items"];
        assert_eq!(row["additionalProperties"], false);
        assert_eq!(row["required"], json!(["subject", "status"]));
        for retired in ["id", "task_id", "description", "owner", "blocked_by"] {
            assert!(row["properties"].get(retired).is_none(), "{retired}");
        }

        for depth in [0, 1, 2] {
            let names = crate::tools::tool_defs(
                depth,
                &crate::shell_programs::ShellPrograms::test_fixture(),
            )
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
            assert_eq!(
                names.iter().any(|name| name == "task_write"),
                depth == 0,
                "task_write at depth {depth}"
            );
            for retired in [
                "task_create",
                "task_get",
                "task_update",
                "task_list",
                "task_clear",
                "todo_write",
            ] {
                assert!(!names.iter().any(|name| name == retired), "{retired}");
            }
        }
    }
}
