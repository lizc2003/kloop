use std::collections::BTreeMap;
use std::collections::BTreeSet;
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
const MAX_BLOCKERS: usize = 256;
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

    fn rank(self) -> u8 {
        match self {
            Self::Pending => 0,
            Self::InProgress => 1,
            Self::Completed => 2,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct StoredTask {
    id: u64,
    subject: String,
    description: String,
    status: TaskStatus,
    blocked_by: Vec<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskView {
    id: String,
    subject: String,
    description: String,
    status: TaskStatus,
    blocked_by: Vec<String>,
    blocks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TaskGraphTask {
    pub id: String,
    pub subject: String,
    pub status: TaskStatus,
    pub blocked_by: Vec<String>,
    pub blocks: Vec<String>,
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
            && input.blocked_by.is_empty()
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
        validate_dependencies(&state.tasks, id, &input.blocked_by, "task_create")?;
        let revision = next_revision(state.revision, "task_create")?;
        let task = StoredTask {
            id,
            subject: input.subject,
            description: input.description,
            status: TaskStatus::Pending,
            blocked_by: input.blocked_by,
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
        if let Some(blocked_by) = patch.blocked_by {
            candidate.blocked_by = blocked_by;
        }

        validate_task_text(&candidate.subject, &candidate.description, "task_update")?;
        if candidate.status.rank() < current.status.rank() {
            bail!(
                "task_update: status cannot move backward from {} to {}",
                status_name(current.status),
                status_name(candidate.status)
            );
        }
        validate_dependencies(&state.tasks, id, &candidate.blocked_by, "task_update")?;
        if creates_cycle(&state.tasks, id, &candidate.blocked_by) {
            bail!("task_update: blocked_by would create a dependency cycle");
        }
        validate_blocker_statuses(&state.tasks, &candidate)?;

        if candidate == current {
            let task = task_view(&state.tasks, id).expect("unchanged task exists");
            return Ok((task, None));
        }
        let display_changed = candidate.subject != current.subject
            || candidate.status != current.status
            || candidate.blocked_by != current.blocked_by;
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

fn validate_dependencies(
    tasks: &BTreeMap<u64, StoredTask>,
    task_id: u64,
    blocked_by: &[u64],
    tool: &str,
) -> Result<()> {
    if blocked_by.len() > MAX_BLOCKERS {
        bail!("{tool}: blocked_by exceeds the {MAX_BLOCKERS}-task limit");
    }
    let mut seen = BTreeSet::new();
    for blocker in blocked_by {
        if *blocker == task_id {
            bail!("{tool}: task {task_id} cannot block itself");
        }
        if !seen.insert(*blocker) {
            bail!("{tool}: blocked_by contains duplicate task {blocker}");
        }
        if !tasks.contains_key(blocker) {
            bail!("{tool}: blocker task {blocker} not found");
        }
    }
    Ok(())
}

fn creates_cycle(
    tasks: &BTreeMap<u64, StoredTask>,
    task_id: u64,
    candidate_blockers: &[u64],
) -> bool {
    let mut stack = candidate_blockers.to_vec();
    let mut visited = BTreeSet::new();
    while let Some(id) = stack.pop() {
        if id == task_id {
            return true;
        }
        if !visited.insert(id) {
            continue;
        }
        if let Some(task) = tasks.get(&id) {
            stack.extend(task.blocked_by.iter().copied());
        }
    }
    false
}

fn validate_blocker_statuses(tasks: &BTreeMap<u64, StoredTask>, task: &StoredTask) -> Result<()> {
    if task.status == TaskStatus::Pending {
        return Ok(());
    }
    for blocker_id in &task.blocked_by {
        let blocker = tasks
            .get(blocker_id)
            .expect("dependencies were validated before status");
        if blocker.status != TaskStatus::Completed {
            bail!(
                "task_update: task {} is blocked by incomplete task {blocker_id}",
                task.id
            );
        }
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
        blocked_by: ids_as_strings(&task.blocked_by),
        blocks: blocks_for(tasks, id),
    })
}

fn task_graph_task(tasks: &BTreeMap<u64, StoredTask>, id: u64) -> Option<TaskGraphTask> {
    let task = tasks.get(&id)?;
    Some(TaskGraphTask {
        id: task.id.to_string(),
        subject: task.subject.clone(),
        status: task.status,
        blocked_by: ids_as_strings(&task.blocked_by),
        blocks: blocks_for(tasks, id),
    })
}

fn blocks_for(tasks: &BTreeMap<u64, StoredTask>, id: u64) -> Vec<String> {
    tasks
        .values()
        .filter(|task| task.blocked_by.contains(&id))
        .map(|task| task.id.to_string())
        .collect()
}

fn ids_as_strings(ids: &[u64]) -> Vec<String> {
    ids.iter().map(u64::to_string).collect()
}

fn status_name(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Pending => "pending",
        TaskStatus::InProgress => "in_progress",
        TaskStatus::Completed => "completed",
    }
}

struct TaskCreateInput {
    subject: String,
    description: String,
    blocked_by: Vec<u64>,
}

struct TaskPatch {
    subject: Option<String>,
    description: Option<String>,
    status: Option<TaskStatus>,
    blocked_by: Option<Vec<u64>>,
}

pub(super) fn task_create_def() -> ToolDef {
    ToolDef {
        name: "task_create".into(),
        description: "Create one pending task in the root-owned task graph for this live session. Returns a stable opaque task ID. subject and description are required; blocked_by may reference existing task IDs. This records work only — it does not start an Agent, assign work, claim a mailbox, persist across resume, or create a background execution.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "subject": {"type": "string", "maxLength": MAX_SUBJECT_CHARS, "description": "Short single-line task title"},
                "description": {"type": "string", "description": "Complete task instructions"},
                "blocked_by": {"type": "array", "maxItems": MAX_BLOCKERS, "items": {"type": "string"}, "description": "Existing task IDs that must complete first"}
            },
            "required": ["subject", "description"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_get_def() -> ToolDef {
    ToolDef {
        name: "task_get".into(),
        description: "Get one task from this live session's root-owned task graph by its stable ID. Returns subject, description, status, direct blocked_by dependencies, and the computed reverse blocks projection.".into(),
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
        description: "Atomically patch one task in this live session's root-owned task graph. Omitted fields stay unchanged; blocked_by replaces the complete dependency list. Status may move forward from pending to in_progress or completed, or from in_progress to completed, but never backward. A task cannot enter a non-pending state until every blocker is completed. Missing dependencies, duplicate dependencies, self-dependencies, and cycles are rejected without changing the graph.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "task_id": {"type": "string", "description": "Stable task ID returned by task_create"},
                "subject": {"type": "string", "maxLength": MAX_SUBJECT_CHARS},
                "description": {"type": "string"},
                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]},
                "blocked_by": {"type": "array", "maxItems": MAX_BLOCKERS, "items": {"type": "string"}, "description": "Complete replacement dependency list; [] clears it"}
            },
            "required": ["task_id"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn task_list_def() -> ToolDef {
    ToolDef {
        name: "task_list".into(),
        description: "List all tasks in this live session's root-owned task graph, ordered by numeric task ID. Returns compact records with subject, status, blocked_by, and computed blocks; use task_get for a task's full description. Takes no filters or pagination arguments.".into(),
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
        description: "Clear every task from this live session's root-owned task graph and start a new task epoch. Keeps the stable task ID high-water mark and does not clear the conversation or stop any Agent, Program, Workflow, or Bash execution. Use only when the root agent is explicitly abandoning or replacing an unfinished graph.".into(),
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
    let object = strict_object(
        input,
        &["subject", "description", "blocked_by"],
        "task_create",
    )?;
    let subject = required_string(object, "subject", "task_create")?.to_string();
    let description = required_string(object, "description", "task_create")?.to_string();
    let blocked_by = optional_blocked_by(object, "task_create")?.unwrap_or_default();
    Ok(TaskCreateInput {
        subject,
        description,
        blocked_by,
    })
}

fn parse_update(input: &Value) -> Result<(u64, TaskPatch)> {
    let object = strict_object(
        input,
        &["task_id", "subject", "description", "status", "blocked_by"],
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
    let blocked_by = optional_blocked_by(object, "task_update")?;
    Ok((
        id,
        TaskPatch {
            subject,
            description,
            status,
            blocked_by,
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

fn optional_blocked_by(object: &Map<String, Value>, tool: &str) -> Result<Option<Vec<u64>>> {
    let Some(value) = object.get("blocked_by") else {
        return Ok(None);
    };
    let values = value
        .as_array()
        .ok_or_else(|| anyhow!("{tool}: 'blocked_by' must be an array when provided"))?;
    if values.len() > MAX_BLOCKERS {
        bail!("{tool}: blocked_by exceeds the {MAX_BLOCKERS}-task limit");
    }
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            let raw = value
                .as_str()
                .ok_or_else(|| anyhow!("{tool}: blocked_by[{index}] must be a task ID string"))?;
            parse_task_id(raw, tool)
        })
        .collect::<Result<Vec<_>>>()
        .map(Some)
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

    async fn create(ctx: &ToolCtx, subject: &str, blocked_by: &[&str]) -> Value {
        let (output, is_error) = run_tool(
            "task_create",
            json!({
                "subject": subject,
                "description": format!("Description for {subject}"),
                "blocked_by": blocked_by,
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
        let first = create(&ctx, "First", &[]).await;
        let second = create(&ctx, "Second", &["1"]).await;
        assert_eq!(
            first,
            json!({
                "task": {
                    "id": "1",
                    "subject": "First",
                    "description": "Description for First",
                    "status": "pending",
                    "blocked_by": [],
                    "blocks": [],
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
                    "blocked_by": ["1"],
                    "blocks": [],
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
                    "blocked_by": [],
                    "blocks": ["2"],
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
                        "blocked_by": [],
                        "blocks": ["2"],
                    },
                    {
                        "id": "2",
                        "subject": "Second",
                        "status": "in_progress",
                        "blocked_by": ["1"],
                        "blocks": [],
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
    async fn graph_constraints_are_atomic() {
        let ctx = test_ctx(0, "task-graph");
        create(&ctx, "A", &[]).await;
        create(&ctx, "B", &["1"]).await;
        create(&ctx, "C", &["2"]).await;

        for (input, needle) in [
            (
                json!({"task_id":"1","blocked_by":["1"]}),
                "cannot block itself",
            ),
            (json!({"task_id":"1","blocked_by":["99"]}), "not found"),
            (json!({"task_id":"1","blocked_by":["2","2"]}), "duplicate"),
            (json!({"task_id":"1","blocked_by":["2"]}), "cycle"),
            (json!({"task_id":"1","blocked_by":["3"]}), "cycle"),
        ] {
            let (output, is_error) = run_tool("task_update", input, &ctx).await;
            assert!(is_error, "{output}");
            assert!(output.contains(needle), "{output}");
        }
        let (output, is_error) = run_tool("task_get", json!({"task_id":"1"}), &ctx).await;
        assert!(!is_error, "{output}");
        let task: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(task["task"]["blocked_by"], json!([]));
    }

    #[tokio::test]
    async fn blockers_gate_forward_status_and_status_never_moves_backward() {
        let ctx = test_ctx(0, "task-status");
        create(&ctx, "Blocker", &[]).await;
        create(&ctx, "Dependent", &["1"]).await;

        for status in ["in_progress", "completed"] {
            let (output, is_error) =
                run_tool("task_update", json!({"task_id":"2","status":status}), &ctx).await;
            assert!(is_error, "{output}");
            assert!(output.contains("incomplete task 1"), "{output}");
        }
        for (task_id, status) in [
            ("2", "pending"),
            ("1", "completed"),
            ("1", "completed"),
            ("2", "in_progress"),
            ("2", "in_progress"),
            ("2", "completed"),
            ("2", "completed"),
        ] {
            let (output, is_error) = run_tool(
                "task_update",
                json!({"task_id":task_id,"status":status}),
                &ctx,
            )
            .await;
            assert!(!is_error, "{output}");
            let updated: Value = serde_json::from_str(&output).unwrap();
            assert_eq!(updated["task"]["status"], status);
        }
        let (output, is_error) = run_tool(
            "task_update",
            json!({"task_id":"2","status":"pending"}),
            &ctx,
        )
        .await;
        assert!(is_error, "{output}");
        assert!(output.contains("cannot move backward"), "{output}");
    }

    #[tokio::test]
    async fn failed_create_does_not_consume_an_id_and_clear_keeps_high_water() {
        let ctx = test_ctx(0, "task-ids");
        let (output, is_error) = run_tool(
            "task_create",
            json!({"subject":"bad","description":"bad","blocked_by":["99"]}),
            &ctx,
        )
        .await;
        assert!(is_error, "{output}");
        assert_eq!(create(&ctx, "First", &[]).await["task"]["id"], "1");
        ctx.cfg.tasks.clear().unwrap();
        assert_eq!(create(&ctx, "Second", &[]).await["task"]["id"], "2");
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
        assert_eq!(create(&ctx, "First", &[]).await["task"]["id"], "1");
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
        create(&ctx, "A", &[]).await;
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
            ("task_update", json!({"task_id":"1","blocked_by":null})),
            ("task_list", json!({"status":"pending"})),
            ("task_clear", json!("not an object")),
        ] {
            let (output, is_error) = run_tool(name, input, &ctx).await;
            assert!(is_error, "{name}: {output}");
        }
    }

    #[tokio::test]
    async fn combined_dependency_and_status_patch_validates_the_candidate_atomically() {
        let ctx = test_ctx(0, "task-candidate");
        create(&ctx, "Completed blocker", &[]).await;
        create(&ctx, "Incomplete blocker", &[]).await;
        create(&ctx, "Work", &[]).await;
        let (output, is_error) = run_tool(
            "task_update",
            json!({"task_id":"1","status":"completed"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");

        let (output, is_error) = run_tool(
            "task_update",
            json!({
                "task_id":"3",
                "subject":"Renamed work",
                "description":"Updated description",
                "status":"in_progress",
                "blocked_by":["1","2"]
            }),
            &ctx,
        )
        .await;
        assert!(is_error, "{output}");
        assert!(output.contains("incomplete task 2"), "{output}");
        let (unchanged, is_error) = run_tool("task_get", json!({"task_id":"3"}), &ctx).await;
        assert!(!is_error, "{unchanged}");
        let unchanged: Value = serde_json::from_str(&unchanged).unwrap();
        assert_eq!(unchanged["task"]["subject"], "Work");
        assert_eq!(unchanged["task"]["status"], "pending");
        assert_eq!(unchanged["task"]["blocked_by"], json!([]));

        let (output, is_error) = run_tool(
            "task_update",
            json!({"task_id":"2","status":"completed"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let (output, is_error) = run_tool(
            "task_update",
            json!({
                "task_id":"3",
                "subject":"Renamed work",
                "description":"Updated description",
                "status":"in_progress",
                "blocked_by":["1","2"]
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let task: Value = serde_json::from_str(&output).unwrap();
        assert_eq!(task["task"]["subject"], "Renamed work");
        assert_eq!(task["task"]["blocked_by"], json!(["1", "2"]));
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
                            blocked_by: Vec::new(),
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
                    blocked_by: Vec::new(),
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
        assert_eq!(create(&ctx, "First valid", &[]).await["task"]["id"], "1");

        let registry = TaskRegistry::default();
        for index in 0..MAX_TASKS {
            registry
                .create(TaskCreateInput {
                    subject: format!("Task {index}"),
                    description: "bounded".into(),
                    blocked_by: Vec::new(),
                })
                .unwrap();
        }
        let error = registry
            .create(TaskCreateInput {
                subject: "Too many".into(),
                description: "bounded".into(),
                blocked_by: Vec::new(),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("task limit"), "{error}");
    }

    #[test]
    fn snapshots_revision_and_epoch_rollover_are_atomic() {
        fn input(subject: &str, blocked_by: Vec<u64>) -> TaskCreateInput {
            TaskCreateInput {
                subject: subject.into(),
                description: format!("Description for {subject}"),
                blocked_by,
            }
        }
        fn patch_status(status: TaskStatus) -> TaskPatch {
            TaskPatch {
                subject: None,
                description: None,
                status: Some(status),
                blocked_by: None,
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
        let (_, created) = registry.create(input("First", Vec::new())).unwrap();
        assert_eq!(created.revision, 1);
        assert_eq!(created.tasks[0].id, "1");

        let (description_only, snapshot) = registry
            .update(
                1,
                TaskPatch {
                    subject: None,
                    description: Some("New private description".into()),
                    status: None,
                    blocked_by: None,
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
                    blocked_by: None,
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
        let (task, after_clear) = registry.create(input("After clear", Vec::new())).unwrap();
        assert_eq!(task.id, "2");
        assert_eq!(after_clear.revision, 5);

        let rollover = TaskRegistry::default();
        rollover.create(input("Old", Vec::new())).unwrap();
        rollover
            .update(1, patch_status(TaskStatus::Completed))
            .unwrap();
        let (new_task, snapshot) = rollover.create(input("New epoch", Vec::new())).unwrap();
        assert_eq!(new_task.id, "2");
        assert_eq!(
            snapshot
                .tasks
                .iter()
                .map(|task| task.id.as_str())
                .collect::<Vec<_>>(),
            vec!["2"]
        );

        let dependent = TaskRegistry::default();
        dependent.create(input("Old", Vec::new())).unwrap();
        dependent
            .update(1, patch_status(TaskStatus::Completed))
            .unwrap();
        let (_, snapshot) = dependent.create(input("Same graph", vec![1])).unwrap();
        assert_eq!(
            snapshot,
            TaskGraphSnapshot {
                revision: 3,
                tasks: vec![
                    TaskGraphTask {
                        id: "1".into(),
                        subject: "Old".into(),
                        status: TaskStatus::Completed,
                        blocked_by: Vec::new(),
                        blocks: vec!["2".into()],
                    },
                    TaskGraphTask {
                        id: "2".into(),
                        subject: "Same graph".into(),
                        status: TaskStatus::Pending,
                        blocked_by: vec!["1".into()],
                        blocks: Vec::new(),
                    },
                ],
            }
        );
        dependent
            .create(input("Replacement blocker", Vec::new()))
            .unwrap();
        let (_, snapshot) = dependent
            .update(
                2,
                TaskPatch {
                    subject: None,
                    description: None,
                    status: None,
                    blocked_by: Some(vec![3]),
                },
            )
            .unwrap();
        assert_eq!(
            snapshot.unwrap(),
            TaskGraphSnapshot {
                revision: 5,
                tasks: vec![
                    TaskGraphTask {
                        id: "1".into(),
                        subject: "Old".into(),
                        status: TaskStatus::Completed,
                        blocked_by: Vec::new(),
                        blocks: Vec::new(),
                    },
                    TaskGraphTask {
                        id: "2".into(),
                        subject: "Same graph".into(),
                        status: TaskStatus::Pending,
                        blocked_by: vec!["3".into()],
                        blocks: Vec::new(),
                    },
                    TaskGraphTask {
                        id: "3".into(),
                        subject: "Replacement blocker".into(),
                        status: TaskStatus::Pending,
                        blocked_by: Vec::new(),
                        blocks: vec!["2".into()],
                    },
                ],
            }
        );

        let unfinished = TaskRegistry::default();
        unfinished
            .create(input("Still pending", Vec::new()))
            .unwrap();
        let (_, snapshot) = unfinished
            .create(input("Also pending", Vec::new()))
            .unwrap();
        assert_eq!(snapshot.tasks.len(), 2);

        let failed = TaskRegistry::default();
        failed.create(input("Completed", Vec::new())).unwrap();
        failed
            .update(1, patch_status(TaskStatus::Completed))
            .unwrap();
        let before = failed.snapshot();
        assert!(failed.create(input("Invalid", vec![99])).is_err());
        assert_eq!(failed.snapshot(), before);
        let (task, snapshot) = failed.create(input("Valid new epoch", Vec::new())).unwrap();
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
            blocked_by: Vec::new(),
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
                        blocked_by: None,
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
        for (name, input) in [
            (
                "task_create",
                json!({
                    "subject":"Invalid create",
                    "description":"must not commit",
                    "blocked_by":["99"]
                }),
            ),
            (
                "task_update",
                json!({
                    "task_id":"1",
                    "subject":"Invalid candidate",
                    "blocked_by":["99"]
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

        create(&ctx, "Discarded task", &[]).await;
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
