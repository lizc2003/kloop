//! todo_write — the model's structured task list (cc's TodoWrite). One tool,
//! full-table replacement: the model sends the ENTIRE list every call, so
//! there is no incremental state to drift. The list is session-scoped process
//! state held on the Config, not history: it survives across turns within a
//! session, resets empty for each sub-agent, and starts empty on resume (the
//! model rebuilds it from its own todo_write calls replayed in history).

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use serde::Deserialize;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

use super::ToolCtx;
use kloop_protocol::ToolDef;

/// One task-list entry. cc/claw shape: no id (full-table replace makes ids
/// pointless), `activeForm` is the present-continuous label a UI shows while
/// the item runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub content: String,
    #[serde(rename = "activeForm")]
    pub active_form: String,
    pub status: TodoStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

pub(super) fn todo_write_def() -> ToolDef {
    ToolDef {
        name: "todo_write".into(),
        description: "Maintain a structured task list for the work in progress. Use it to plan a \
multi-step task up front and to track progress as you go. Send the ENTIRE list every call (full \
replacement, including already-completed items) — it is not incremental. Each item has: content \
(imperative, e.g. \"Add the parser\"), activeForm (present-continuous label shown while it runs, \
e.g. \"Adding the parser\"), and status (pending | in_progress | completed). Keep one item \
in_progress while you work on it and mark it completed the moment it is done, before starting the \
next. Use this for tasks with three or more distinct steps; skip it for trivial single-step work."
            .into(),
        schema: json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete task list (replaces any previous list)",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "Imperative description of the task"},
                            "activeForm": {"type": "string", "description": "Present-continuous form shown while the task runs"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "activeForm", "status"]
                    }
                }
            },
            "required": ["todos"]
        }),
    }
}

pub(super) async fn todo_write_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let todos: Vec<TodoItem> = serde_json::from_value(input.get("todos").cloned().unwrap_or(Value::Null))
        .map_err(|e| {
            anyhow!("todo_write: invalid 'todos' (expected an array of {{content, activeForm, status}}): {e}")
        })?;
    validate(&todos)?;
    // Full-table replace; the model owns the whole list every call.
    *ctx.cfg.todos.lock().unwrap() = todos.clone();
    ctx.ui.todo_update(&ctx.cfg.agent_label, &todos);
    Ok(summarize(&todos))
}

/// Parse a `todo_write` tool_use `input` back into items — used by the TUI to
/// replay a historical todo call as its checklist on resume. Returns None if
/// the payload is missing or malformed (a bad historical call just doesn't
/// render).
pub fn parse_todos(input: &Value) -> Option<Vec<TodoItem>> {
    serde_json::from_value(input.get("todos")?.clone()).ok()
}

fn validate(todos: &[TodoItem]) -> Result<()> {
    if todos.is_empty() {
        bail!("todo_write: the list must not be empty");
    }
    for todo in todos {
        if todo.content.trim().is_empty() {
            bail!("todo_write: content must not be empty");
        }
        if todo.active_form.trim().is_empty() {
            bail!("todo_write: activeForm must not be empty");
        }
    }
    // Multiple in_progress items are allowed (matches claw; cc's
    // one-at-a-time is guidance in the tool description, not a hard rule).
    Ok(())
}

/// A compact confirmation the model reads back as the tool result.
fn summarize(todos: &[TodoItem]) -> String {
    let done = count(todos, TodoStatus::Completed);
    let in_progress = count(todos, TodoStatus::InProgress);
    let pending = count(todos, TodoStatus::Pending);
    format!(
        "Updated todo list: {} item(s) — {done} completed, {in_progress} in progress, {pending} pending",
        todos.len()
    )
}

fn count(todos: &[TodoItem], status: TodoStatus) -> usize {
    todos.iter().filter(|t| t.status == status).count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;

    fn todo(content: &str, active: &str, status: &str) -> Value {
        json!({"content": content, "activeForm": active, "status": status})
    }

    #[tokio::test]
    async fn full_table_replace_stores_and_summarizes() {
        let ctx = test_ctx(0, "todo-replace");
        let (out, is_error) = run_tool(
            "todo_write",
            json!({"todos": [
                todo("Write the parser", "Writing the parser", "in_progress"),
                todo("Add tests", "Adding tests", "pending"),
            ]}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(
            out,
            "Updated todo list: 2 item(s) — 0 completed, 1 in progress, 1 pending"
        );
        assert_eq!(
            *ctx.cfg.todos.lock().unwrap(),
            vec![
                TodoItem {
                    content: "Write the parser".into(),
                    active_form: "Writing the parser".into(),
                    status: TodoStatus::InProgress,
                },
                TodoItem {
                    content: "Add tests".into(),
                    active_form: "Adding tests".into(),
                    status: TodoStatus::Pending,
                },
            ]
        );

        // A second call replaces the list wholesale — no merge, no drift.
        let (_, is_error) = run_tool(
            "todo_write",
            json!({"todos": [todo("Write the parser", "Writing the parser", "completed")]}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        let stored = ctx.cfg.todos.lock().unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].status, TodoStatus::Completed);
    }

    #[tokio::test]
    async fn multiple_in_progress_is_allowed() {
        let ctx = test_ctx(0, "todo-multi");
        let (out, is_error) = run_tool(
            "todo_write",
            json!({"todos": [
                todo("A", "Doing A", "in_progress"),
                todo("B", "Doing B", "in_progress"),
            ]}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(ctx.cfg.todos.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn validation_rejects_empty_list_fields_and_bad_status() {
        let ctx = test_ctx(0, "todo-bad");

        let (out, is_error) = run_tool("todo_write", json!({"todos": []}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("must not be empty"), "{out}");

        let (out, is_error) = run_tool(
            "todo_write",
            json!({"todos": [todo("  ", "Doing it", "pending")]}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("content must not be empty"), "{out}");

        let (out, is_error) = run_tool(
            "todo_write",
            json!({"todos": [todo("Do it", "  ", "pending")]}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("activeForm must not be empty"), "{out}");

        // Unknown status is a deserialize error surfaced as is_error.
        let (out, is_error) = run_tool(
            "todo_write",
            json!({"todos": [todo("Do it", "Doing it", "blocked")]}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("invalid 'todos'"), "{out}");

        // A malformed write never mutates the stored list.
        assert!(ctx.cfg.todos.lock().unwrap().is_empty());
    }
}
