use std::collections::BTreeMap;
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
const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;
const MAX_DIAGNOSTIC_CHARS: usize = 80;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
}

impl TaskStatus {
    fn parse(value: &Value, tool: &str) -> Result<Self> {
        match value.as_str() {
            Some("pending") => Ok(Self::Pending),
            Some("in_progress") => Ok(Self::InProgress),
            Some("completed") => Ok(Self::Completed),
            Some(other) => bail!("{tool}: unknown status `{}`", bounded_diagnostic(other)),
            None => bail!("{tool}: 'status' must be a string when provided"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredTask {
    id: u64,
    subject: String,
    description: String,
    status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskView {
    id: String,
    subject: String,
    description: String,
    status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskGraphTask {
    pub id: String,
    pub subject: String,
    pub status: TaskStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskGraphSnapshot {
    pub revision: u64,
    pub tasks: Vec<TaskGraphTask>,
}

struct TaskRegistryState {
    next_id: u64,
    revision: u64,
    tasks: BTreeMap<u64, StoredTask>,
}

impl Default for TaskRegistryState {
    fn default() -> Self {
        Self {
            next_id: 1,
            revision: 0,
            tasks: BTreeMap::new(),
        }
    }
}

#[derive(Default)]
pub struct TaskRegistry {
    state: RwLock<TaskRegistryState>,
}

impl TaskRegistry {
    fn create(&self, input: TaskCreateInput) -> Result<(TaskView, TaskGraphSnapshot)> {
        validate_task_text(&input.subject, &input.description, "task_create")?;
        let mut state = self.state.write().unwrap();
        let rollover = !state.tasks.is_empty()
            && state
                .tasks
                .values()
                .all(|task| task.status == TaskStatus::Completed);
        if !rollover && state.tasks.len() >= MAX_TASKS {
            bail!("task_create: task limit of {MAX_TASKS} reached");
        }
        let id = state.next_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| anyhow!("task_create: task id space exhausted"))?;
        let revision = next_revision(state.revision, "task_create")?;
        let task = StoredTask {
            id,
            subject: input.subject,
            description: input.description,
            status: TaskStatus::Pending,
        };
        if rollover {
            state.tasks.clear();
        }
        state.tasks.insert(id, task);
        state.next_id = next_id;
        state.revision = revision;
        let task = task_view(&state.tasks, id).expect("created task exists");
        Ok((task, task_graph_snapshot(&state)))
    }

    fn get(&self, id: u64) -> Result<TaskView> {
        let state = self.state.read().unwrap();
        task_view(&state.tasks, id).ok_or_else(|| anyhow!("task_get: task {id} not found"))
    }

    fn update(&self, id: u64, patch: TaskPatch) -> Result<(TaskView, Option<TaskGraphSnapshot>)> {
        let mut state = self.state.write().unwrap();
        let current = state
            .tasks
            .get(&id)
            .cloned()
            .ok_or_else(|| anyhow!("task_update: task {id} not found"))?;
        let mut candidate = current.clone();
        if let Some(subject) = patch.subject {
            candidate.subject = subject;
        }
        if let Some(description) = patch.description {
            candidate.description = description;
        }
        if let Some(status) = patch.status {
            candidate.status = status;
        }

        validate_task_text(&candidate.subject, &candidate.description, "task_update")?;

        if candidate == current {
            let task = task_view(&state.tasks, id).expect("unchanged task exists");
            return Ok((task, None));
        }
        let display_changed =
            candidate.subject != current.subject || candidate.status != current.status;
        let revision = display_changed
            .then(|| next_revision(state.revision, "task_update"))
            .transpose()?;
        state.tasks.insert(id, candidate);
        if let Some(revision) = revision {
            state.revision = revision;
        }
        let task = task_view(&state.tasks, id).expect("updated task exists");
        let snapshot = display_changed.then(|| task_graph_snapshot(&state));
        Ok((task, snapshot))
    }

    fn list(&self) -> Vec<TaskGraphTask> {
        let state = self.state.read().unwrap();
        task_graph_tasks(&state.tasks)
    }

    pub fn snapshot(&self) -> TaskGraphSnapshot {
        let state = self.state.read().unwrap();
        task_graph_snapshot(&state)
    }

    pub fn clear(&self) -> Result<(usize, TaskGraphSnapshot)> {
        let mut state = self.state.write().unwrap();
        let revision = next_revision(state.revision, "task_clear")?;
        let cleared_count = state.tasks.len();
        state.tasks.clear();
        state.revision = revision;
        Ok((cleared_count, task_graph_snapshot(&state)))
    }
}

fn next_revision(revision: u64, tool: &str) -> Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("{tool}: task graph revision exhausted"))
}

fn task_graph_snapshot(state: &TaskRegistryState) -> TaskGraphSnapshot {
    TaskGraphSnapshot {
        revision: state.revision,
        tasks: task_graph_tasks(&state.tasks),
    }
}

fn task_graph_tasks(tasks: &BTreeMap<u64, StoredTask>) -> Vec<TaskGraphTask> {
    tasks
        .keys()
        .map(|id| task_graph_task(tasks, *id).expect("listed task exists"))
        .collect()
}

fn validate_task_text(subject: &str, description: &str, tool: &str) -> Result<()> {
    validate_single_line(subject, "subject", MAX_SUBJECT_CHARS, tool)?;
    if description.trim().is_empty() {
        bail!("{tool}: description must not be empty");
    }
    if description.len() > MAX_DESCRIPTION_BYTES {
        bail!("{tool}: description exceeds the {MAX_DESCRIPTION_BYTES}-byte limit");
    }
    if description
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        bail!("{tool}: description contains an unsupported control character");
    }
    Ok(())
}

fn validate_single_line(value: &str, field: &str, max_chars: usize, tool: &str) -> Result<()> {
    if value.trim().is_empty() {
        bail!("{tool}: {field} must not be empty");
    }
    if value.chars().count() > max_chars {
        bail!("{tool}: {field} exceeds the {max_chars}-character limit");
    }
    if value
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        bail!("{tool}: {field} must be a single line without control characters");
    }
    Ok(())
}

fn task_view(tasks: &BTreeMap<u64, StoredTask>, id: u64) -> Option<TaskView> {
    let task = tasks.get(&id)?;
    Some(TaskView {
        id: task.id.to_string(),
        subject: task.subject.clone(),
        description: task.description.clone(),
        status: task.status,
    })
}

fn task_graph_task(tasks: &BTreeMap<u64, StoredTask>, id: u64) -> Option<TaskGraphTask> {
    let task = tasks.get(&id)?;
    Some(TaskGraphTask {
        id: task.id.to_string(),
        subject: task.subject.clone(),
        status: task.status,
    })
}

struct TaskCreateInput {
    subject: String,
    description: String,
}

struct TaskPatch {
    subject: Option<String>,
    description: Option<String>,
    status: Option<TaskStatus>,
}

pub(super) fn task_create_def() -> ToolDef {
    ToolDef {
        name: "task_create".into(),
        description: "Create one pending task in this live session's root-owned task graph. Returns a stable opaque task ID. This records work only — it does not start an Agent, assign work, claim a mailbox, persist across resume, or create a background execution.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "subject": {"type": "string", "maxLength": MAX_SUBJECT_CHARS, "description": "Short single-line task title"},
                "description": {"type": "string", "description": "Complete task instructions"}
            },
            "required": ["subject", "description"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_get_def() -> ToolDef {
    ToolDef {
        name: "task_get".into(),
        description: "Get one task from the task graph by its stable ID. Returns subject, description, and status.".into(),
        schema: json!({
            "type": "object",
            "properties": {"task_id": {"type": "string", "description": "Stable task ID returned by task_create"}},
            "required": ["task_id"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_update_def() -> ToolDef {
    ToolDef {
        name: "task_update".into(),
        description: "Atomically patch one task in the task graph. Omitted fields stay unchanged; status may move in any direction.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Stable task ID returned by task_create"},
                "subject": {"type": "string", "maxLength": MAX_SUBJECT_CHARS},
                "description": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
            },
            "required": ["task_id"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_list_def() -> ToolDef {
    ToolDef {
        name: "task_list".into(),
        description: "List every task in the task graph, ordered by numeric task ID. Returns compact records with subject and status; use task_get for the full description. Takes no filters or pagination arguments.".into(),
        schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_clear_def() -> ToolDef {
    ToolDef {
        name: "task_clear".into(),
        description: "Clear every task from the task graph and start a new task epoch. Keeps the stable task ID high-water mark and does not clear the conversation or stop any Agent, Program, Workflow, or Bash execution. Use only when abandoning or replacing an unfinished graph.".into(),
        schema: json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_create_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let parsed = parse_create(input)?;
    let (task, snapshot) = ctx.cfg.tasks.create(parsed)?;
    ctx.ui.emit(&Event::TaskGraphUpdated(snapshot));
    Ok(json!({"task": task}).to_string())
}

pub(super) fn task_get_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let object = strict_object(input, &["task_id"], "task_get")?;
    let id = required_task_id(object, "task_get")?;
    let task = ctx.cfg.tasks.get(id)?;
    Ok(json!({"task": task}).to_string())
}

pub(super) fn task_update_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let (id, patch) = parse_update(input)?;
    let (task, snapshot) = ctx.cfg.tasks.update(id, patch)?;
    if let Some(snapshot) = snapshot {
        ctx.ui.emit(&Event::TaskGraphUpdated(snapshot));
    }
    Ok(json!({"task": task}).to_string())
}

pub(super) fn task_list_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    strict_object(input, &[], "task_list")?;
    Ok(json!({"tasks": ctx.cfg.tasks.list()}).to_string())
}

pub(super) fn task_clear_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    strict_object(input, &[], "task_clear")?;
    let (cleared_count, snapshot) = ctx.cfg.tasks.clear()?;
    ctx.ui.emit(&Event::TaskGraphUpdated(snapshot.clone()));
    Ok(json!({"cleared_count": cleared_count, "graph": snapshot}).to_string())
}

fn parse_create(input: &Value) -> Result<TaskCreateInput> {
    let object = strict_object(input, &["subject", "description"], "task_create")?;
    let subject = required_string(object, "subject", "task_create")?.to_string();
    let description = required_string(object, "description", "task_create")?.to_string();
    Ok(TaskCreateInput {
        subject,
        description,
    })
}

fn parse_update(input: &Value) -> Result<(u64, TaskPatch)> {
    let object = strict_object(
        input,
        &["task_id", "subject", "description", "status"],
        "task_update",
    )?;
    let id = required_task_id(object, "task_update")?;
    if object.len() == 1 {
        bail!("task_update: at least one patch field is required");
    }
    let subject = optional_string(object, "subject", "task_update")?;
    let description = optional_string(object, "description", "task_update")?;
    let status = object
        .get("status")
        .map(|value| TaskStatus::parse(value, "task_update"))
        .transpose()?;
    Ok((
        id,
        TaskPatch {
            subject,
            description,
            status,
        },
    ))
}

fn strict_object<'a>(
    input: &'a Value,
    allowed: &[&str],
    tool: &str,
) -> Result<&'a Map<String, Value>> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: input must be an object"))?;
    if let Some(unexpected) = object
        .keys()
        .find(|candidate| !allowed.contains(&candidate.as_str()))
    {
        bail!("{tool}: unknown field `{}`", bounded_diagnostic(unexpected));
    }
    Ok(object)
}

fn required_string<'a>(object: &'a Map<String, Value>, key: &str, tool: &str) -> Result<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{tool}: missing required string argument '{key}'"))
}

fn optional_string(object: &Map<String, Value>, key: &str, tool: &str) -> Result<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        Some(_) => bail!("{tool}: '{key}' must be a string when provided"),
    }
}

fn required_task_id(object: &Map<String, Value>, tool: &str) -> Result<u64> {
    let raw = required_string(object, "task_id", tool)?;
    parse_task_id(raw, tool)
}

fn parse_task_id(raw: &str, tool: &str) -> Result<u64> {
    if raw.is_empty() || raw.starts_with('0') || !raw.as_bytes().iter().all(u8::is_ascii_digit) {
        bail!("{tool}: invalid task ID `{}`", bounded_diagnostic(raw));
    }
    raw.parse::<u64>()
        .map_err(|_| anyhow!("{tool}: invalid task ID `{}`", bounded_diagnostic(raw)))
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

    async fn create(ctx: &ToolCtx, subject: &str) -> Value {
        let (output, is_error) = run_tool(
            "task_create",
            json!({
                "subject": subject,
                "description": format!("Description for {subject}"),
            }),
            ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        serde_json::from_str(&output).unwrap()
    }

    #[tokio::test]
    async fn create_get_list_and_patch_have_stable_json_without_owner() {
        let ctx = test_ctx(0, "task-basic");
        let first = create(&ctx, "First").await;
        let second = create(&ctx, "Second").await;
        assert_eq!(
            first,
            json!({
                "task": {
                    "id": "1",
                    "subject": "First",
                    "description": "Description for First",
                    "status": "pending",
                }
            })
        );
        assert_eq!(
            second,
            json!({
                "task": {
                    "id": "2",
                    "subject": "Second",
                    "description": "Description for Second",
                    "status": "pending",
                }
            })
        );

        let (output, is_error) = run_tool(
            "task_update",
            json!({"task_id":"1","status":"completed"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let updated: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            updated,
            json!({
                "task": {
                    "id": "1",
                    "subject": "First",
                    "description": "Description for First",
                    "status": "completed",
                }
            })
        );

        let (output, is_error) = run_tool(
            "task_update",
            json!({"task_id":"2","status":"in_progress"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let updated: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(updated["task"]["status"], "in_progress");
        assert!(updated["task"].get("owner").is_none());

        let (output, is_error) = run_tool("task_list", json!({}), &ctx).await;
        assert!(!is_error, "{output}");
        let listed: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(
            listed,
            json!({
                "tasks": [
                    {
                        "id": "1",
                        "subject": "First",
                        "status": "completed",
                    },
                    {
                        "id": "2",
                        "subject": "Second",
                        "status": "in_progress",
                    }
                ]
            })
        );

        let (output, is_error) = run_tool("task_get", json!({"task_id":"2"}), &ctx).await;
        assert!(!is_error, "{output}");
        let fetched: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(fetched, updated);
    }

    #[tokio::test]
    async fn failed_create_does_not_consume_an_id_and_clear_keeps_high_water() {
        let ctx = test_ctx(0, "task-ids");
        let (output, is_error) = run_tool(
            "task_create",
            json!({"subject":"x".repeat(MAX_SUBJECT_CHARS + 1),"description":"bad"}),
            &ctx,
        )
        .await;
        assert!(is_error, "{output}");
        assert_eq!(create(&ctx, "First").await["task"]["id"], "1");
        ctx.cfg.tasks.clear().unwrap();
        assert_eq!(create(&ctx, "Second").await["task"]["id"], "2");
    }

    #[tokio::test]
    async fn owner_inputs_are_rejected_without_id_or_snapshot_mutation() {
        let ctx = test_ctx(0, "task-owner-removed");
        for owner in [json!("assistant"), Value::Null] {
            let (output, is_error) = run_tool(
                "task_create",
                json!({"subject":"invalid","description":"must not exist","owner":owner}),
                &ctx,
            )
            .await;
            assert!(is_error, "{output}");
            assert!(output.contains("unknown field `owner`"), "{output}");
        }
        assert_eq!(create(&ctx, "First").await["task"]["id"], "1");
        let (before, is_error) = run_tool("task_get", json!({"task_id":"1"}), &ctx).await;
        assert!(!is_error, "{before}");

        for owner in [json!("assistant"), Value::Null] {
            let (output, is_error) = run_tool(
                "task_update",
                json!({"task_id":"1","status":"completed","owner":owner}),
                &ctx,
            )
            .await;
            assert!(is_error, "{output}");
            assert!(output.contains("unknown field `owner`"), "{output}");
        }
        let (after, is_error) = run_tool("task_get", json!({"task_id":"1"}), &ctx).await;
        assert!(!is_error, "{after}");
        assert_eq!(after, before);
        let after: Value = serde_json::from_str(&after).unwrap();
        assert_eq!(after["task"]["status"], "pending");
        assert!(after["task"].get("owner").is_none());
    }

    #[tokio::test]
    async fn strict_parsers_reject_unknown_null_and_empty_patch() {
        let ctx = test_ctx(0, "task-strict");
        create(&ctx, "A").await;
        for (name, input) in [
            (
                "task_create",
                json!({"subject":"x","description":"y","extra":true}),
            ),
            (
                "task_create",
                json!({"subject":"x","description":"y","owner":null}),
            ),
            ("task_get", json!({"task_id":"01"})),
            ("task_get", json!({"task_id":1})),
            ("task_update", json!({"task_id":"1"})),
            ("task_update", json!({"task_id":"1","status":null})),
            ("task_update", json!({"task_id":"1","blocked_by":["2"]})),
            ("task_list", json!({"status":"pending"})),
            ("task_clear", json!("not an object")),
        ] {
            let (output, is_error) = run_tool(name, input, &ctx).await;
            assert!(is_error, "{name}: {output}");
        }
    }

    #[test]
    fn concurrent_creates_are_unique_and_independent_registries_do_not_leak() {
        let registry = std::sync::Arc::new(TaskRegistry::default());
        let handles = (0..64)
            .map(|index| {
                let registry = std::sync::Arc::clone(&registry);
                std::thread::spawn(move || {
                    registry
                        .create(TaskCreateInput {
                            subject: format!("Task {index}"),
                            description: "parallel create".into(),
                        })
                        .unwrap()
                        .0
                        .id
                        .parse::<u64>()
                        .unwrap()
                })
            })
            .collect::<Vec<_>>();
        let mut ids = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        assert_eq!(ids, (1..=64).collect::<Vec<_>>());
        assert_eq!(registry.list().len(), 64);

        let independent = TaskRegistry::default();
        assert!(independent.list().is_empty());
        assert_eq!(
            independent
                .create(TaskCreateInput {
                    subject: "Independent".into(),
                    description: "separate live Config".into(),
                })
                .unwrap()
                .0
                .id,
            "1"
        );
    }

    #[tokio::test]
    async fn text_and_registry_budgets_fail_before_mutation() {
        let ctx = test_ctx(0, "task-budgets");
        for input in [
            json!({"subject":"bad\nline","description":"x"}),
            json!({"subject":"x","description":"x\u{0000}"}),
            json!({"subject":"x".repeat(MAX_SUBJECT_CHARS + 1),"description":"x"}),
            json!({"subject":"x","description":"x".repeat(MAX_DESCRIPTION_BYTES + 1)}),
        ] {
            let (output, is_error) = run_tool("task_create", input, &ctx).await;
            assert!(is_error, "{output}");
        }
        assert_eq!(create(&ctx, "First valid").await["task"]["id"], "1");

        let registry = TaskRegistry::default();
        for index in 0..MAX_TASKS {
            registry
                .create(TaskCreateInput {
                    subject: format!("Task {index}"),
                    description: "bounded".into(),
                })
                .unwrap();
        }
        let error = registry
            .create(TaskCreateInput {
                subject: "Too many".into(),
                description: "bounded".into(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("task limit"), "{error}");
    }

    #[test]
    fn snapshots_revision_and_epoch_rollover_are_atomic() {
        fn input(subject: &str) -> TaskCreateInput {
            TaskCreateInput {
                subject: subject.into(),
                description: format!("Description for {subject}"),
            }
        }
        fn patch_status(status: TaskStatus) -> TaskPatch {
            TaskPatch {
                subject: None,
                description: None,
                status: Some(status),
            }
        }

        let registry = TaskRegistry::default();
        assert_eq!(
            registry.snapshot(),
            TaskGraphSnapshot {
                revision: 0,
                tasks: Vec::new(),
            }
        );
        let (_, created) = registry.create(input("First")).unwrap();
        assert_eq!(created.revision, 1);
        assert_eq!(created.tasks[0].id, "1");

        let (description_only, snapshot) = registry
            .update(
                1,
                TaskPatch {
                    subject: None,
                    description: Some("New private description".into()),
                    status: None,
                },
            )
            .unwrap();
        assert_eq!(description_only.description, "New private description");
        assert_eq!(snapshot, None);
        assert_eq!(registry.snapshot().revision, 1);

        let (_, visible) = registry
            .update(
                1,
                TaskPatch {
                    subject: Some("Renamed".into()),
                    description: None,
                    status: None,
                },
            )
            .unwrap();
        let visible = visible.unwrap();
        assert_eq!(visible.revision, 2);
        assert_eq!(visible.tasks[0].subject, "Renamed");

        let (cleared_count, cleared) = registry.clear().unwrap();
        assert_eq!(cleared_count, 1);
        assert_eq!(cleared.revision, 3);
        assert!(cleared.tasks.is_empty());
        let (cleared_count, repeated) = registry.clear().unwrap();
        assert_eq!(cleared_count, 0);
        assert_eq!(repeated.revision, 4);
        let (task, after_clear) = registry.create(input("After clear")).unwrap();
        assert_eq!(task.id, "2");
        assert_eq!(after_clear.revision, 5);

        let rollover = TaskRegistry::default();
        rollover.create(input("Old")).unwrap();
        rollover
            .update(1, patch_status(TaskStatus::Completed))
            .unwrap();
        let (new_task, snapshot) = rollover.create(input("New epoch")).unwrap();
        assert_eq!(new_task.id, "2");
        assert_eq!(
            snapshot
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["2"]
        );

        let unfinished = TaskRegistry::default();
        unfinished.create(input("Still pending")).unwrap();
        let (_, snapshot) = unfinished.create(input("Also pending")).unwrap();
        assert_eq!(snapshot.tasks.len(), 2);

        let failed = TaskRegistry::default();
        failed.create(input("Completed")).unwrap();
        failed
            .update(1, patch_status(TaskStatus::Completed))
            .unwrap();
        let before = failed.snapshot();
        assert!(
            failed
                .create(input(&"x".repeat(MAX_SUBJECT_CHARS + 1)))
                .is_err()
        );
        assert_eq!(failed.snapshot(), before);
        let (task, snapshot) = failed.create(input("Valid new epoch")).unwrap();
        assert_eq!(task.id, "2");
        assert_eq!(snapshot.revision, 3);
        assert_eq!(snapshot.tasks.len(), 1);
        assert_eq!(snapshot.tasks[0].id, "2");
    }

    #[test]
    fn id_and_revision_overflow_fail_before_mutation() {
        let input = |subject: &str| TaskCreateInput {
            subject: subject.into(),
            description: "overflow probe".into(),
        };
        let registry = TaskRegistry::default();
        registry.create(input("Existing")).unwrap();

        registry.state.write().unwrap().next_id = u64::MAX;
        let before = registry.snapshot();
        assert!(registry.create(input("ID overflow")).is_err());
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.state.read().unwrap().next_id, u64::MAX);

        {
            let mut state = registry.state.write().unwrap();
            state.next_id = 2;
            state.revision = u64::MAX;
        }
        let before = registry.snapshot();
        let next_id = registry.state.read().unwrap().next_id;
        assert!(registry.create(input("Revision overflow")).is_err());
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.state.read().unwrap().next_id, next_id);

        assert!(
            registry
                .update(
                    1,
                    TaskPatch {
                        subject: Some("Visible revision overflow".into()),
                        description: None,
                        status: None,
                    },
                )
                .is_err()
        );
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.state.read().unwrap().next_id, next_id);

        assert!(registry.clear().is_err());
        assert_eq!(registry.snapshot(), before);
        assert_eq!(registry.state.read().unwrap().next_id, next_id);
    }

    #[tokio::test]
    async fn mutations_emit_full_snapshots_inside_ordinary_tool_lifecycle() {
        let ui = Arc::new(RecordingUi::default());
        let mut ctx = test_ctx(0, "task-events");
        ctx.ui = ui.clone();

        let (output, is_error) = run_tool(
            "task_create",
            json!({"subject":"Visible","description":"Private"}),
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
            } if name == "task_create"
        ));
        assert!(matches!(
            &events[1],
            Event::TaskGraphUpdated(snapshot)
                if snapshot.revision == 1 && snapshot.tasks.len() == 1
        ));
        assert!(matches!(
            &events[2],
            Event::ItemCompleted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == "task_create"
        ));

        for input in [
            json!({"task_id":"1","description":"Changed privately"}),
            json!({"task_id":"1","description":"Changed privately"}),
        ] {
            let (output, is_error) = run_tool("task_update", input, &ctx).await;
            assert!(!is_error, "{output}");
            let events = ui.take();
            assert_eq!(events.len(), 2);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TaskGraphUpdated(_)))
            );
        }

        let (output, is_error) = run_tool(
            "task_update",
            json!({"task_id":"1","subject":"Visible rename"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let events = ui.take();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[1],
            Event::TaskGraphUpdated(snapshot)
                if snapshot.revision == 2 && snapshot.tasks[0].subject == "Visible rename"
        ));

        let before_failures = ctx.cfg.tasks.snapshot();
        let next_id = ctx.cfg.tasks.state.read().unwrap().next_id;
        let oversized = "x".repeat(MAX_SUBJECT_CHARS + 1);
        for (name, input) in [
            (
                "task_create",
                json!({
                    "subject":oversized,
                    "description":"must not commit",
                }),
            ),
            (
                "task_update",
                json!({
                    "task_id":"1",
                    "subject":oversized,
                }),
            ),
        ] {
            let (output, is_error) = run_tool(name, input, &ctx).await;
            assert!(is_error, "{name}: {output}");
            assert_eq!(ctx.cfg.tasks.snapshot(), before_failures);
            assert_eq!(ctx.cfg.tasks.state.read().unwrap().next_id, next_id);
            let events = ui.take();
            assert_eq!(events.len(), 2);
            assert!(matches!(
                &events[..],
                [
                    Event::ItemStarted {
                        item: Item::ToolCall { name: started, .. },
                        ..
                    },
                    Event::ItemCompleted {
                        item: Item::ToolCall {
                            name: completed,
                            status: crate::event::ItemStatus::Failed,
                            ..
                        },
                        ..
                    }
                ] if started == name && completed == name
            ));
        }

        for (name, input) in [
            ("task_get", json!({"task_id":"1"})),
            ("task_list", json!({})),
        ] {
            let (output, is_error) = run_tool(name, input, &ctx).await;
            assert!(!is_error, "{output}");
            let events = ui.take();
            assert_eq!(events.len(), 2);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TaskGraphUpdated(_)))
            );
        }

        let nonempty_before = ctx.cfg.tasks.snapshot();
        assert_eq!(nonempty_before.tasks.len(), 1);
        let next_id = ctx.cfg.tasks.state.read().unwrap().next_id;
        for input in [json!({"unexpected":true}), json!("not an object")] {
            let (output, is_error) = run_tool("task_clear", input, &ctx).await;
            assert!(is_error, "{output}");
            assert_eq!(ctx.cfg.tasks.snapshot(), nonempty_before);
            assert_eq!(ctx.cfg.tasks.state.read().unwrap().next_id, next_id);
            let events = ui.take();
            assert_eq!(events.len(), 2);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TaskGraphUpdated(_)))
            );
        }

        let (output, is_error) = run_tool("task_clear", json!({}), &ctx).await;
        assert!(!is_error, "{output}");
        assert_eq!(
            serde_json::from_str::<Value>(&output).unwrap(),
            json!({
                "cleared_count": 1,
                "graph": {"revision": 3, "tasks": []},
            })
        );
        let events = ui.take();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            &events[1],
            Event::TaskGraphUpdated(snapshot)
                if snapshot.revision == 3 && snapshot.tasks.is_empty()
        ));
        assert_eq!(events[1].as_note(), None);

        let before = ctx.cfg.tasks.snapshot();
        for input in [json!({"unexpected":true}), json!("not an object")] {
            let (output, is_error) = run_tool("task_clear", input, &ctx).await;
            assert!(is_error, "{output}");
            assert_eq!(ctx.cfg.tasks.snapshot(), before);
            let events = ui.take();
            assert_eq!(events.len(), 2);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TaskGraphUpdated(_)))
            );
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
                    "must survive task_clear",
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

        create(&ctx, "Discarded task").await;
        let (output, is_error) = run_tool("task_clear", json!({}), &ctx).await;
        assert!(!is_error, "{output}");
        assert!(ctx.cfg.tasks.snapshot().tasks.is_empty());
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
    fn definitions_are_strict_and_root_only() {
        let defs = [
            task_create_def(),
            task_get_def(),
            task_update_def(),
            task_list_def(),
            task_clear_def(),
        ];
        let task_names = defs
            .iter()
            .map(|definition| definition.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            task_names,
            vec![
                "task_create",
                "task_get",
                "task_update",
                "task_list",
                "task_clear",
            ]
        );
        for definition in defs {
            assert_eq!(definition.schema["additionalProperties"], false);
            assert!(definition.schema["properties"].get("owner").is_none());
        }

        for depth in [0, 1, 2] {
            let names = crate::tools::tool_defs(
                depth,
                &crate::shell_programs::ShellPrograms::test_fixture(),
            )
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
            for task_tool in [
                "task_create",
                "task_get",
                "task_update",
                "task_list",
                "task_clear",
            ] {
                assert_eq!(
                    names.iter().any(|name| name == task_tool),
                    depth == 0,
                    "{task_tool} at depth {depth}"
                );
            }
            assert!(!names.iter().any(|name| name == "todo_write"));
        }
    }
}
