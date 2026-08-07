//! The core-native event stream: one vocabulary every front-end consumes.
//!
//! Before plan 39 each front-end implemented a wide [`Ui`](crate::agent::Ui)
//! trait whose default methods downgraded everything to a note. Now core emits a
//! single [`Event`] stream through [`Ui::emit`](crate::agent::Ui::emit) and each
//! front-end projects it: the TUI into cells, the server/headless into wire
//! notifications, the plain REPL into stdout. An item's life is three-state —
//! [`Event::ItemStarted`], zero or more [`Event::ItemDelta`], then
//! [`Event::ItemCompleted`] (the finalized item, output/status included).
//!
//! The old per-method note downgrade survives as [`Event::as_note`]: a front-end
//! that renders only text and notes matches those two and routes the rest through
//! `as_note`, reproducing the previous default-impl behavior in one place.

use serde_json::Value;

use crate::agent::EndReason;
use crate::permissions::Mode;
use crate::tools::TodoItem;
use crate::tools::TodoStatus;

/// A stable identifier for an item within a turn. Tool calls reuse the model's
/// `tool_use` id; a sub-agent uses its label ("agent-N"); a todo list uses a
/// fixed per-owner slot; assistant/reasoning messages use a turn-local counter.
pub type ItemId = String;

/// Session-scoped background work is not owned by the turn that launched it.
/// Shells, sub-agents, and code-mode programs keep separate registries but share
/// this read-only projection so every frontend can render one lifecycle without
/// inventing a late `turnId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundTaskKind {
    Shell,
    Agent,
    Program,
    Workflow,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackgroundTaskStatus {
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: String,
    /// Durable run identity when the execution has one (Workflow); ordinary shell,
    /// agent, and program work remains execution-id-only.
    pub run_id: Option<String>,
    pub kind: BackgroundTaskKind,
    pub description: String,
    pub status: BackgroundTaskStatus,
    pub output_path: Option<String>,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduledTaskOrigin {
    Cron,
    LoopWakeup,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduledTaskStatus {
    Scheduled,
    Fired,
    Cancelled,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduledTask {
    pub id: String,
    pub origin: ScheduledTaskOrigin,
    pub status: ScheduledTaskStatus,
    pub scheduled_for_ms: Option<i64>,
    pub reason: Option<String>,
    pub detail: Option<String>,
}

/// Everything core tells a front-end about a turn. The turn bracket
/// (`TurnStarted`/`TurnEnded`/`Usage`) is constructed by each front-end's worker
/// around its `run_turn` call; the rest flow through the `Ui::emit` seam as core
/// produces them.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    TurnStarted,
    TurnEnded(EndReason),
    /// An item appeared (a tool call began, a message started streaming, …).
    ItemStarted {
        id: ItemId,
        item: Item,
    },
    /// Streaming content for an open item.
    ItemDelta {
        id: ItemId,
        delta: Delta,
    },
    /// An item reached its final state — the payload is the finalized item
    /// (a tool call with its output/status, the full assistant text, …). This
    /// is the real-time projection of the block appended to History.
    ItemCompleted {
        id: ItemId,
        item: Item,
    },
    /// A session-scoped background shell/agent/program changed state. Unlike an
    /// item event this deliberately has no turn owner: the terminal update may
    /// arrive after the launching turn completed.
    BackgroundTaskUpdated(BackgroundTask),
    /// Owner-scoped scheduler lifecycle. Like background work this is session
    /// scoped and may arrive without the turn that created the job.
    ScheduledTaskUpdated(ScheduledTask),
    /// Full context size after a request (total input+output tokens).
    Usage(u64),
    /// The session entered (`branch = Some`) or left (`None`) a worktree.
    CwdChanged {
        cwd: String,
        branch: Option<String>,
    },
    /// The permission mode changed (e.g. `exit_plan_mode`).
    ModeChanged(Mode),
    /// A system line that is not model output (compaction, retries, …).
    Note(String),
}

/// The content of an item. Reasoning carries only its display text — the
/// thinking signature is a transport detail handled in the protocol/provider
/// layers, never surfaced as a UI event.
#[derive(Clone, Debug, PartialEq)]
pub enum Item {
    AssistantMessage {
        text: String,
        status: ItemStatus,
    },
    Reasoning {
        text: String,
        status: ItemStatus,
    },
    /// `agent` is "" for the main agent's calls and the sub-agent's label
    /// ("agent-N") for calls made inside a task. `output` is set only on the
    /// completed item (bounded for transport).
    ToolCall {
        agent: String,
        name: String,
        input: Value,
        status: ItemStatus,
        output: Option<String>,
    },
    /// A `task` call spawned a sub-agent to work on `task`.
    SubAgent {
        label: String,
        task: String,
        status: ItemStatus,
    },
    /// The model rewrote its task list (full replacement). `agent` is "" for the
    /// main agent, "agent-N" for a sub-agent's internal planning.
    Todo {
        agent: String,
        items: Vec<TodoItem>,
    },
}

/// Streaming content routed to an open item's channel.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    Output(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemStatus {
    InProgress,
    Completed,
    Failed,
}

/// The one-line preview a note-based UI shows for a tool call: the input JSON
/// truncated to 120 characters, matching the pre-plan-39 `summary`.
pub fn tool_summary(input: &Value) -> String {
    input.to_string().chars().take(120).collect()
}

impl Event {
    /// The note a front-end that renders only text and notes shows for this
    /// event, or `None` if the event has no note form (deltas, tool completions,
    /// the turn bracket). Reproduces exactly the note strings the old `Ui`
    /// default methods produced, so a plain front-end keeps identical output.
    pub fn as_note(&self) -> Option<String> {
        match self {
            Event::Note(s) => Some(s.clone()),
            Event::CwdChanged { cwd, branch } => Some(match branch {
                Some(b) => format!("working directory → {cwd} (branch {b})"),
                None => format!("working directory → {cwd}"),
            }),
            Event::ModeChanged(mode) => Some(format!("permission mode → {}", mode.label())),
            Event::ItemStarted {
                item: Item::ToolCall {
                    agent, name, input, ..
                },
                ..
            } => {
                let summary = tool_summary(input);
                Some(if agent.is_empty() {
                    format!("{name} {summary}")
                } else {
                    format!("{agent} · {name} {summary}")
                })
            }
            Event::ItemStarted {
                item: Item::SubAgent { label, task, .. },
                ..
            } => Some(format!("{label} started: {task}")),
            Event::ItemCompleted {
                item: Item::SubAgent { label, status, .. },
                ..
            } => Some(format!(
                "{label} {}",
                if *status == ItemStatus::Completed {
                    "finished"
                } else {
                    "failed"
                }
            )),
            Event::BackgroundTaskUpdated(task) => {
                let kind = match task.kind {
                    BackgroundTaskKind::Shell => "shell",
                    BackgroundTaskKind::Agent => "agent",
                    BackgroundTaskKind::Program => "program",
                    BackgroundTaskKind::Workflow => "workflow",
                };
                let state = match task.status {
                    BackgroundTaskStatus::Running => "started",
                    BackgroundTaskStatus::Completed => "completed",
                    BackgroundTaskStatus::Failed => "failed",
                    BackgroundTaskStatus::Cancelled => "cancelled",
                };
                let detail = task
                    .detail
                    .as_deref()
                    .map(|detail| format!(": {detail}"))
                    .unwrap_or_default();
                Some(format!("background {kind} {} {state}{detail}", task.id))
            }
            Event::ScheduledTaskUpdated(task) => {
                let origin = match task.origin {
                    ScheduledTaskOrigin::Cron => "cron",
                    ScheduledTaskOrigin::LoopWakeup => "loop wakeup",
                };
                let state = match task.status {
                    ScheduledTaskStatus::Scheduled => "scheduled",
                    ScheduledTaskStatus::Fired => "fired",
                    ScheduledTaskStatus::Cancelled => "cancelled",
                    ScheduledTaskStatus::Failed => "failed",
                };
                let detail = task
                    .detail
                    .as_deref()
                    .map(|value| format!(": {value}"))
                    .unwrap_or_default();
                Some(format!("{origin} {} {state}{detail}", task.id))
            }
            Event::ItemCompleted {
                item: Item::Todo { agent, items },
                ..
            } => Some(todo_note(agent, items)),
            _ => None,
        }
    }
}

/// The one-line note for a todo update, matching the pre-plan-39 `todo_update`
/// default: "todos {done}/{total} · now: {active}" while an item runs, else
/// "todos {done}/{total} done", with an "{agent} · " prefix for a sub-agent.
fn todo_note(agent: &str, todos: &[TodoItem]) -> String {
    let done = todos
        .iter()
        .filter(|t| t.status == TodoStatus::Completed)
        .count();
    let prefix = if agent.is_empty() {
        String::new()
    } else {
        format!("{agent} · ")
    };
    match todos.iter().find(|t| t.status == TodoStatus::InProgress) {
        Some(current) => format!(
            "{prefix}todos {done}/{} · now: {}",
            todos.len(),
            current.active_form
        ),
        None => format!("{prefix}todos {done}/{} done", todos.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn todo(content: &str, active: &str, status: TodoStatus) -> TodoItem {
        TodoItem {
            content: content.into(),
            active_form: active.into(),
            status,
        }
    }

    #[test]
    fn tool_start_note_matches_old_default() {
        let ev = Event::ItemStarted {
            id: "t1".into(),
            item: Item::ToolCall {
                agent: String::new(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
                status: ItemStatus::InProgress,
                output: None,
            },
        };
        assert_eq!(ev.as_note().as_deref(), Some(r#"bash {"command":"ls"}"#));
    }

    #[test]
    fn sub_agent_tool_start_note_carries_label() {
        let ev = Event::ItemStarted {
            id: "t1".into(),
            item: Item::ToolCall {
                agent: "agent-1".into(),
                name: "grep".into(),
                input: json!({"pattern": "x"}),
                status: ItemStatus::InProgress,
                output: None,
            },
        };
        assert_eq!(
            ev.as_note().as_deref(),
            Some(r#"agent-1 · grep {"pattern":"x"}"#)
        );
    }

    #[test]
    fn tool_completed_has_no_note() {
        let ev = Event::ItemCompleted {
            id: "t1".into(),
            item: Item::ToolCall {
                agent: String::new(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
                status: ItemStatus::Completed,
                output: Some("file.txt".into()),
            },
        };
        assert_eq!(ev.as_note(), None);
    }

    #[test]
    fn sub_agent_lifecycle_notes() {
        let start = Event::ItemStarted {
            id: "agent-1".into(),
            item: Item::SubAgent {
                label: "agent-1".into(),
                task: "look things up".into(),
                status: ItemStatus::InProgress,
            },
        };
        assert_eq!(
            start.as_note().as_deref(),
            Some("agent-1 started: look things up")
        );
        let ended_ok = Event::ItemCompleted {
            id: "agent-1".into(),
            item: Item::SubAgent {
                label: "agent-1".into(),
                task: String::new(),
                status: ItemStatus::Completed,
            },
        };
        assert_eq!(ended_ok.as_note().as_deref(), Some("agent-1 finished"));
        let ended_fail = Event::ItemCompleted {
            id: "agent-1".into(),
            item: Item::SubAgent {
                label: "agent-1".into(),
                task: String::new(),
                status: ItemStatus::Failed,
            },
        };
        assert_eq!(ended_fail.as_note().as_deref(), Some("agent-1 failed"));
    }

    #[test]
    fn todo_note_matches_old_default() {
        let running = Event::ItemCompleted {
            id: "todos".into(),
            item: Item::Todo {
                agent: String::new(),
                items: vec![
                    todo("A", "Doing A", TodoStatus::Completed),
                    todo("B", "Doing B", TodoStatus::InProgress),
                ],
            },
        };
        assert_eq!(
            running.as_note().as_deref(),
            Some("todos 1/2 · now: Doing B")
        );
        let all_done = Event::ItemCompleted {
            id: "todos-agent-1".into(),
            item: Item::Todo {
                agent: "agent-1".into(),
                items: vec![todo("A", "Doing A", TodoStatus::Completed)],
            },
        };
        assert_eq!(
            all_done.as_note().as_deref(),
            Some("agent-1 · todos 1/1 done")
        );
    }

    #[test]
    fn background_task_note_preserves_terminal_detail() {
        let event = Event::BackgroundTaskUpdated(BackgroundTask {
            id: "program-2".into(),
            run_id: None,
            kind: BackgroundTaskKind::Program,
            description: "run checks".into(),
            status: BackgroundTaskStatus::Cancelled,
            output_path: None,
            detail: Some("session shutdown".into()),
        });
        assert_eq!(
            event.as_note().as_deref(),
            Some("background program program-2 cancelled: session shutdown")
        );
    }

    #[test]
    fn text_and_reasoning_have_no_note() {
        let text = Event::ItemDelta {
            id: "m0".into(),
            delta: Delta::Text("hi".into()),
        };
        assert_eq!(text.as_note(), None);
        let reasoning = Event::ItemStarted {
            id: "r0".into(),
            item: Item::Reasoning {
                text: String::new(),
                status: ItemStatus::InProgress,
            },
        };
        assert_eq!(reasoning.as_note(), None);
    }
}
