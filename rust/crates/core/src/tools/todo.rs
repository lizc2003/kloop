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

const MAX_TODOS: usize = 256;
const MAX_SUBJECT_CHARS: usize = 200;
const MAX_DIAGNOSTIC_CHARS: usize = 80;

/// Depth-0 round boundaries the list must stand still before the model is
/// reminded it exists (plan 190). The reminder is a low-frequency backstop,
/// not a metronome: the model's only other view of the list is its own
/// `todo_write` arguments, which sink out of reach within a few rounds of
/// tool output.
pub(crate) const REMINDER_STALE_ROUNDS: u32 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

impl TodoStatus {
    fn parse(value: Option<&Value>) -> Result<Self> {
        match value.and_then(Value::as_str) {
            Some("pending") => Ok(Self::Pending),
            Some("in_progress") => Ok(Self::InProgress),
            Some("completed") => Ok(Self::Completed),
            Some(other) => bail!("todo_write: unknown status `{}`", bounded_diagnostic(other)),
            None => bail!("todo_write: every todo needs a 'status' string"),
        }
    }

    /// The wire word, reused by the reminder so its rows read exactly as the
    /// model wrote them.
    const fn label(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::InProgress => "in_progress",
            Self::Completed => "completed",
        }
    }
}

/// One row, in the single shape the model writes, the registry stores and the
/// TUI panel draws. Whole-table overwrite leaves nothing to address, so a row
/// carries no ID — plan 188 deleted the ID space, its high-water mark and the
/// epoch rollover that existed to reuse it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TodoItem {
    pub subject: String,
    pub status: TodoStatus,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct TodoSnapshot {
    pub revision: u64,
    pub todos: Vec<TodoItem>,
}

#[derive(Default)]
struct TodoRegistryState {
    revision: u64,
    todos: Vec<TodoItem>,
    reminder: ReminderState,
}

/// Throttle for the round-boundary reminder. It lives beside the list rather
/// than in the turn: the list is session-scoped, so "rounds since it last
/// changed" has to survive the turn that wrote it.
#[derive(Default)]
struct ReminderState {
    /// Revision seen at the previous round boundary.
    seen_revision: u64,
    /// Round boundaries since the list last changed.
    rounds_unchanged: u32,
    /// Revision already announced; every state is announced once, not every
    /// `REMINDER_STALE_ROUNDS` rounds for as long as it stands. One rule
    /// covers the empty list too: it only leaves revision 0 through a write
    /// (which fills it) or `/clear` (which starts a new conversation), so
    /// "once per revision" is "once per conversation" for it.
    announced_revision: Option<u64>,
}

#[derive(Default)]
pub struct TodoRegistry {
    state: RwLock<TodoRegistryState>,
}

impl TodoRegistry {
    /// Replace the whole table. Returns the live table, plus the snapshot to
    /// publish — `None` when the write changed nothing, so a model that
    /// restates the same list every round never flickers the panel.
    ///
    /// Everything is validated before the lock is taken and before a single
    /// row is stored: a rejected write leaves the previous table exactly as it
    /// was.
    fn write(&self, todos: Vec<TodoItem>) -> Result<(Vec<TodoItem>, Option<TodoSnapshot>)> {
        if todos.len() > MAX_TODOS {
            bail!("todo_write: todo limit of {MAX_TODOS} exceeded");
        }
        for todo in &todos {
            validate_subject(&todo.subject)?;
        }
        let mut state = self.state.write().unwrap();
        if todos == state.todos {
            return Ok((todos, None));
        }
        let revision = next_revision(state.revision, "todo_write")?;
        state.todos = todos;
        state.revision = revision;
        let published = graph_snapshot(&state);
        Ok((published.todos.clone(), Some(published)))
    }

    pub fn snapshot(&self) -> TodoSnapshot {
        graph_snapshot(&self.state.read().unwrap())
    }

    /// The user's half of the registry: `/clear` empties the table, and the
    /// model has no tool that does. The revision advances unconditionally —
    /// it is the fence the TUI uses to reject a late pre-clear snapshot.
    pub fn clear(&self) -> Result<(usize, TodoSnapshot)> {
        let mut state = self.state.write().unwrap();
        let revision = next_revision(state.revision, "clear")?;
        let cleared_count = state.todos.len();
        state.todos.clear();
        state.revision = revision;
        // `/clear` starts a new conversation, so the reminder starts over with
        // it: the new revision re-arms the notice, and the staleness count
        // restarts from the empty list it just produced.
        state.reminder = ReminderState {
            seen_revision: revision,
            ..ReminderState::default()
        };
        Ok((cleared_count, graph_snapshot(&state)))
    }

    /// One depth-0 round boundary: advance the staleness count and return the
    /// reminder text when one is due (plan 190).
    ///
    /// Due means the list has stood still for [`REMINDER_STALE_ROUNDS`]
    /// boundaries and this revision has not been announced yet. A write resets
    /// the count, so a model that keeps its list current is never reminded.
    ///
    /// What it says depends on whether anything in the list is still open. A
    /// list with open rows is handed back as it stands; a list with none —
    /// empty, or finished to the last row — is the same situation as having
    /// written none at all, so it gets the "write one if this work needs one"
    /// notice instead of a projection nobody can act on.
    pub fn round_boundary_reminder(&self) -> Option<String> {
        let mut state = self.state.write().unwrap();
        if state.reminder.seen_revision != state.revision {
            state.reminder.seen_revision = state.revision;
            state.reminder.rounds_unchanged = 0;
            return None;
        }
        state.reminder.rounds_unchanged = state.reminder.rounds_unchanged.saturating_add(1);
        if state.reminder.rounds_unchanged < REMINDER_STALE_ROUNDS
            || state.reminder.announced_revision == Some(state.revision)
        {
            return None;
        }
        state.reminder.announced_revision = Some(state.revision);
        let open = state
            .todos
            .iter()
            .any(|todo| todo.status != TodoStatus::Completed);
        Some(match open {
            true => render_reminder(&graph_snapshot(&state)),
            false => NO_LIVE_LIST_REMINDER.to_string(),
        })
    }
}

/// What the model is told when nothing in the list is open — it wrote none, or
/// it finished the one it wrote. There is nothing to project that it could act
/// on, so this says the one thing it can act on instead.
///
/// It never suggests clearing the finished list. Clearing costs a round and
/// buys nothing: the panel has already left the composer, and the list reaches
/// the context through this reminder and nowhere else. A new list is a whole
/// list, so writing one is the only call worth making here.
const NO_LIVE_LIST_REMINDER: &str = "<system-reminder>\nYou have no unfinished todos recorded. If the work in front of you has several steps, write the list down with todo_write now — a plan that exists only in your last message is out of reach by the next round, and the list is also what the user sees. A write replaces the whole list, so a finished one needs no clearing first. If this work does not need a list, ignore this. Never mention this reminder to the user.\n</system-reminder>";

/// The reminder body: a direct projection of the snapshot, so the panel, the
/// tool result and this text can never disagree about what the list says.
fn render_reminder(snapshot: &TodoSnapshot) -> String {
    let mut out = String::from(
        "<system-reminder>\nYour todo list has not changed for several rounds. It currently reads:\n",
    );
    for todo in &snapshot.todos {
        out.push_str(&format!("\n- [{}] {}", todo.status.label(), todo.subject));
    }
    out.push_str("\n\nIf that is no longer what you are doing, rewrite the whole list with todo_write in the same round as your next piece of work: mark what is finished, and mark what you are on now as in_progress. If it is still accurate, ignore this and carry on. Never mention this reminder to the user.\n</system-reminder>");
    out
}

fn next_revision(revision: u64, tool: &str) -> Result<u64> {
    revision
        .checked_add(1)
        .ok_or_else(|| anyhow!("{tool}: todo list revision exhausted"))
}

fn graph_snapshot(state: &TodoRegistryState) -> TodoSnapshot {
    TodoSnapshot {
        revision: state.revision,
        todos: state.todos.clone(),
    }
}

fn validate_subject(subject: &str) -> Result<()> {
    if subject.trim().is_empty() {
        bail!("todo_write: subject must not be empty");
    }
    if subject.chars().count() > MAX_SUBJECT_CHARS {
        bail!("todo_write: subject exceeds the {MAX_SUBJECT_CHARS}-character limit");
    }
    if subject
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        bail!("todo_write: subject must be a single line without control characters");
    }
    Ok(())
}

pub(super) fn todo_write_def() -> ToolDef {
    ToolDef {
        name: "todo_write".into(),
        description: "Record this session's todo list. Every call replaces the whole list, so send every todo you are still tracking with its current status; `[]` clears it. This records work only — it starts nothing, assigns nothing, and does not survive resume.".into(),
        schema: json!({
            "type": "object",
            "properties": {"todos": {
                "type": "array",
                "maxItems": MAX_TODOS,
                "description": "The complete list, in the order it should be read",
                "items": {
                    "type": "object",
                    "properties": {
                        "subject": {"type": "string", "maxLength": MAX_SUBJECT_CHARS, "description": "Short single-line todo title"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                    },
                    "required": ["subject", "status"],
                    "additionalProperties": false
                }
            }},
            "required": ["todos"],
            "additionalProperties": false
        }),
    }
}

pub(super) fn todo_write_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let (todos, snapshot) = ctx.cfg.todos.write(parse_write(input)?)?;
    if let Some(snapshot) = snapshot {
        ctx.ui.emit(&Event::TodoUpdated(snapshot));
    }
    Ok(json!({"todos": todos}).to_string())
}

fn parse_write(input: &Value) -> Result<Vec<TodoItem>> {
    let object = strict_object(input, &["todos"], "input")?;
    object
        .get("todos")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("todo_write: missing required array argument 'todos'"))?
        .iter()
        .map(parse_todo)
        .collect()
}

fn parse_todo(row: &Value) -> Result<TodoItem> {
    let object = strict_object(row, &["subject", "status"], "each todo")?;
    let subject = object
        .get("subject")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("todo_write: every todo needs a 'subject' string"))?;
    Ok(TodoItem {
        subject: subject.to_string(),
        status: TodoStatus::parse(object.get("status"))?,
    })
}

fn strict_object<'a>(
    value: &'a Value,
    allowed: &[&str],
    what: &str,
) -> Result<&'a Map<String, Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| anyhow!("todo_write: {what} must be an object"))?;
    if let Some(unexpected) = object
        .keys()
        .find(|candidate| !allowed.contains(&candidate.as_str()))
    {
        bail!(
            "todo_write: unknown field `{}` in {what}",
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

    fn table(rows: &[(&str, TodoStatus)]) -> Vec<TodoItem> {
        rows.iter()
            .map(|(subject, status)| TodoItem {
                subject: (*subject).into(),
                status: *status,
            })
            .collect()
    }

    async fn write(ctx: &ToolCtx, todos: Value) -> Value {
        let (output, is_error) = run_tool("todo_write", json!({"todos": todos}), ctx).await;
        assert!(!is_error, "{output}");
        serde_json::from_str(&output).unwrap()
    }

    #[tokio::test]
    async fn every_write_replaces_the_whole_table_and_returns_it() {
        let ctx = test_ctx(0, "todo-write");
        assert_eq!(
            write(
                &ctx,
                json!([
                    {"subject": "First", "status": "in_progress"},
                    {"subject": "Second", "status": "pending"},
                ]),
            )
            .await,
            json!({"todos": [
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
            json!({"todos": [
                {"subject": "Second", "status": "completed"},
                {"subject": "Third", "status": "pending"},
            ]})
        );
        assert_eq!(
            write(&ctx, json!([{"subject": "Second", "status": "pending"}])).await,
            json!({"todos": [{"subject": "Second", "status": "pending"}]})
        );

        assert_eq!(write(&ctx, json!([])).await, json!({"todos": []}));
        assert_eq!(
            ctx.cfg.todos.snapshot(),
            TodoSnapshot {
                revision: 4,
                todos: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn strict_parsing_rejects_every_malformed_table() {
        let ctx = test_ctx(0, "todo-strict");
        for input in [
            json!("not an object"),
            json!({}),
            json!({"todo": []}),
            json!({"todos": {}}),
            json!({"todos": ["First"]}),
            json!({"todos": [{"subject": "x"}]}),
            json!({"todos": [{"status": "pending"}]}),
            json!({"todos": [{"subject": "x", "status": "done"}]}),
            json!({"todos": [{"subject": "x", "status": Value::Null}]}),
            json!({"todos": [{"subject": 1, "status": "pending"}]}),
            json!({"todos": [{"subject": "x", "status": "pending", "note": "extra"}]}),
        ] {
            let (output, is_error) = run_tool("todo_write", input.clone(), &ctx).await;
            assert!(is_error, "{input}: {output}");
        }
        assert!(ctx.cfg.todos.snapshot().todos.is_empty());
    }

    #[tokio::test]
    async fn budgets_and_bad_text_fail_before_the_table_moves() {
        let ctx = test_ctx(0, "todo-budgets");
        write(&ctx, json!([{"subject": "Keep me", "status": "pending"}])).await;
        let before = ctx.cfg.todos.snapshot();

        let oversized = (0..=MAX_TODOS)
            .map(|index| json!({"subject": format!("Todo {index}"), "status": "pending"}))
            .collect::<Vec<_>>();
        for todos in [
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
            let (output, is_error) = run_tool("todo_write", json!({"todos": todos}), &ctx).await;
            assert!(is_error, "{output}");
            assert_eq!(ctx.cfg.todos.snapshot(), before);
        }

        // One row under the limits, and the whole budget's worth of them.
        write(
            &ctx,
            Value::Array(
                (0..MAX_TODOS)
                    .map(|index| json!({"subject": format!("Todo {index}"), "status": "pending"}))
                    .collect(),
            ),
        )
        .await;
        assert_eq!(ctx.cfg.todos.snapshot().todos.len(), MAX_TODOS);
    }

    #[test]
    fn rewriting_the_same_table_publishes_nothing() {
        let registry = TodoRegistry::default();
        let rows = table(&[("First", TodoStatus::Pending)]);
        let (live, snapshot) = registry.write(rows.clone()).unwrap();
        assert_eq!(live, rows);
        assert_eq!(
            snapshot,
            Some(TodoSnapshot {
                revision: 1,
                todos: rows.clone(),
            })
        );

        let (live, snapshot) = registry.write(rows.clone()).unwrap();
        assert_eq!(live, rows);
        assert_eq!(snapshot, None);
        assert_eq!(registry.snapshot().revision, 1);

        // Order is part of the table: the same rows, moved, are a new revision.
        let (_, snapshot) = registry
            .write(table(&[
                ("Second", TodoStatus::Pending),
                ("First", TodoStatus::Pending),
            ]))
            .unwrap();
        assert_eq!(snapshot.unwrap().revision, 2);
        let (_, snapshot) = registry
            .write(table(&[
                ("First", TodoStatus::Pending),
                ("Second", TodoStatus::Pending),
            ]))
            .unwrap();
        assert_eq!(snapshot.unwrap().revision, 3);
    }

    /// The staleness count, not a metronome: a list that keeps moving is never
    /// announced, and one that stands still is announced once per revision.
    #[test]
    fn a_standing_list_is_announced_once_per_revision() {
        let registry = TodoRegistry::default();
        registry
            .write(table(&[
                ("Wire the reminder", TodoStatus::InProgress),
                ("Update the design note", TodoStatus::Pending),
            ]))
            .unwrap();

        // The first boundary after a write only notices the new revision; the
        // count runs from there.
        for round in 0..REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None, "round {round}");
        }
        let reminder = registry.round_boundary_reminder().unwrap();
        assert!(
            reminder.starts_with("<system-reminder>\n") && reminder.ends_with("</system-reminder>"),
            "{reminder}"
        );
        assert!(
            reminder.contains("\n- [in_progress] Wire the reminder"),
            "{reminder}"
        );
        assert!(
            reminder.contains("\n- [pending] Update the design note"),
            "{reminder}"
        );
        assert!(reminder.contains("todo_write"), "{reminder}");

        // Standing still longer is not a reason to say it again, and neither is
        // restating the same list — that write advances no revision, so as far
        // as the reminder is concerned nothing moved.
        for _ in 0..4 * REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None);
        }
        registry
            .write(table(&[
                ("Wire the reminder", TodoStatus::InProgress),
                ("Update the design note", TodoStatus::Pending),
            ]))
            .unwrap();
        for _ in 0..4 * REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None);
        }

        // A real change re-arms it, and the second reminder projects the list
        // as it now stands — finished rows included, as long as one is open.
        registry
            .write(table(&[
                ("Wire the reminder", TodoStatus::Completed),
                ("Dogfood it", TodoStatus::Pending),
            ]))
            .unwrap();
        for _ in 0..REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None);
        }
        let reminder = registry.round_boundary_reminder().unwrap();
        assert!(
            reminder.contains("\n- [completed] Wire the reminder"),
            "{reminder}"
        );
        assert!(reminder.contains("\n- [pending] Dogfood it"), "{reminder}");
        assert!(!reminder.contains("Update the design note"), "{reminder}");
    }

    /// The empty list is the case plan 190 came from — a model that listed its
    /// steps in prose and wrote no list at all. A list finished to the last row
    /// is the same situation: nothing in it is open, so it gets the same
    /// notice rather than a projection nobody can act on. Once per revision
    /// covers both, and for the empty list that is once per conversation.
    #[test]
    fn a_list_with_nothing_open_is_announced_once_per_revision() {
        let registry = TodoRegistry::default();
        for round in 0..REMINDER_STALE_ROUNDS - 1 {
            assert_eq!(registry.round_boundary_reminder(), None, "round {round}");
        }
        let reminder = registry.round_boundary_reminder().unwrap();
        assert_eq!(reminder, NO_LIVE_LIST_REMINDER);
        // Nothing here tells the model to clear anything: that call would cost
        // a round and buy nothing.
        assert!(!reminder.contains("[]"), "{reminder}");
        for _ in 0..4 * REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None);
        }

        // A list written after the notice is on the ordinary standing-list
        // clock, and its rows are handed back while any of them is open.
        registry
            .write(table(&[
                ("Wrote one after all", TodoStatus::Completed),
                ("Still going", TodoStatus::InProgress),
            ]))
            .unwrap();
        for _ in 0..REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None);
        }
        assert!(
            registry
                .round_boundary_reminder()
                .unwrap()
                .contains("Still going")
        );

        // Finishing the last open row is a new revision, and with nothing left
        // open the list is no longer worth handing back.
        registry
            .write(table(&[
                ("Wrote one after all", TodoStatus::Completed),
                ("Still going", TodoStatus::Completed),
            ]))
            .unwrap();
        for _ in 0..REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None);
        }
        assert_eq!(
            registry.round_boundary_reminder().unwrap(),
            NO_LIVE_LIST_REMINDER
        );

        registry.clear().unwrap();
        for round in 0..REMINDER_STALE_ROUNDS - 1 {
            assert_eq!(registry.round_boundary_reminder(), None, "round {round}");
        }
        assert_eq!(
            registry.round_boundary_reminder().unwrap(),
            NO_LIVE_LIST_REMINDER
        );
    }

    /// `/clear` advances the revision and empties the list; the throttle has to
    /// come with it, or the first list of the new conversation is judged
    /// against the old one's count.
    #[test]
    fn clear_resets_the_reminder_clock_with_the_list() {
        let registry = TodoRegistry::default();
        registry
            .write(table(&[("From the old conversation", TodoStatus::Pending)]))
            .unwrap();
        for _ in 0..REMINDER_STALE_ROUNDS / 2 {
            assert_eq!(registry.round_boundary_reminder(), None);
        }
        registry.clear().unwrap();
        registry
            .write(table(&[("From the new one", TodoStatus::InProgress)]))
            .unwrap();

        // The half-run count from before the clear buys the new list nothing:
        // it waits the full stretch.
        for round in 0..REMINDER_STALE_ROUNDS {
            assert_eq!(registry.round_boundary_reminder(), None, "round {round}");
        }
        let reminder = registry.round_boundary_reminder().unwrap();
        assert!(reminder.contains("From the new one"), "{reminder}");
        assert!(
            !reminder.contains("From the old conversation"),
            "{reminder}"
        );
    }

    #[test]
    fn revision_overflow_fails_before_mutation() {
        let registry = TodoRegistry::default();
        registry
            .write(table(&[("Existing", TodoStatus::Pending)]))
            .unwrap();
        registry.state.write().unwrap().revision = u64::MAX;
        let before = registry.snapshot();

        assert!(
            registry
                .write(table(&[("New", TodoStatus::Pending)]))
                .is_err()
        );
        assert_eq!(registry.snapshot(), before);
        assert!(registry.clear().is_err());
        assert_eq!(registry.snapshot(), before);

        // An unchanged rewrite never needs a revision, so it still succeeds.
        let (_, snapshot) = registry
            .write(table(&[("Existing", TodoStatus::Pending)]))
            .unwrap();
        assert_eq!(snapshot, None);
    }

    #[test]
    fn concurrent_writes_are_serialized_and_registries_stay_independent() {
        let registry = Arc::new(TodoRegistry::default());
        let handles = (0..64)
            .map(|index| {
                let registry = Arc::clone(&registry);
                std::thread::spawn(move || {
                    registry
                        .write(table(&[(&format!("Todo {index}"), TodoStatus::Pending)]))
                        .unwrap()
                        .0
                })
            })
            .collect::<Vec<_>>();
        for handle in handles {
            assert_eq!(handle.join().unwrap().len(), 1);
        }
        let snapshot = registry.snapshot();
        assert_eq!(snapshot.todos.len(), 1);
        assert_eq!(snapshot.revision, 64);

        let independent = TodoRegistry::default();
        assert_eq!(
            independent.snapshot(),
            TodoSnapshot {
                revision: 0,
                todos: Vec::new(),
            }
        );
    }

    #[tokio::test]
    async fn mutations_emit_full_snapshots_inside_ordinary_tool_lifecycle() {
        let ui = Arc::new(RecordingUi::default());
        let mut ctx = test_ctx(0, "todo-events");
        ctx.ui = ui.clone();

        let (output, is_error) = run_tool(
            "todo_write",
            json!({"todos": [{"subject": "Visible", "status": "pending"}]}),
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
            } if name == "todo_write"
        ));
        assert!(matches!(
            &events[1],
            Event::TodoUpdated(snapshot)
                if snapshot.revision == 1 && snapshot.todos[0].subject == "Visible"
        ));
        assert_eq!(events[1].as_note(), None);
        assert!(matches!(
            &events[2],
            Event::ItemCompleted {
                item: Item::ToolCall { name, .. },
                ..
            } if name == "todo_write"
        ));

        // A repeated identical write and a rejected write both leave the panel
        // alone: two lifecycle events, no snapshot.
        let before = ctx.cfg.todos.snapshot();
        let quiet = |events: Vec<Event>| {
            assert_eq!(events.len(), 2);
            assert!(
                !events
                    .iter()
                    .any(|event| matches!(event, Event::TodoUpdated(_)))
            );
        };
        let (output, is_error) = run_tool(
            "todo_write",
            json!({"todos": [{"subject": "Visible", "status": "pending"}]}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        quiet(ui.take());
        assert_eq!(ctx.cfg.todos.snapshot(), before);

        for input in [
            json!({"todos": [{"subject": "x".repeat(MAX_SUBJECT_CHARS + 1), "status": "pending"}]}),
            json!({"todos": "not an array"}),
        ] {
            let (output, is_error) = run_tool("todo_write", input, &ctx).await;
            assert!(is_error, "{output}");
            assert_eq!(ctx.cfg.todos.snapshot(), before);
            quiet(ui.take());
        }
    }

    #[tokio::test]
    async fn clear_changes_only_the_todo_registry() {
        let ctx = test_ctx(0, "todo-clear-scope");
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
        let (cleared_count, snapshot) = ctx.cfg.todos.clear().unwrap();
        assert_eq!(cleared_count, 1);
        assert_eq!(
            snapshot,
            TodoSnapshot {
                revision: 2,
                todos: Vec::new(),
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
        let definition = todo_write_def();
        assert_eq!(definition.name, "todo_write");
        // Plan 188's budget: five definitions cost 1597 characters, and the
        // model called none of them. One table, one call, one short paragraph.
        assert!(
            definition.description.chars().count() < 400,
            "{}",
            definition.description.chars().count()
        );
        assert_eq!(definition.schema["additionalProperties"], false);
        let row = &definition.schema["properties"]["todos"]["items"];
        assert_eq!(row["additionalProperties"], false);
        assert_eq!(row["required"], json!(["subject", "status"]));
        assert_eq!(
            row["properties"]
                .as_object()
                .unwrap()
                .keys()
                .collect::<Vec<_>>(),
            ["status", "subject"]
        );

        for depth in [0, 1, 2] {
            let names = crate::tools::tool_defs(
                depth,
                &crate::shell_programs::ShellPrograms::test_fixture(),
            )
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
            assert_eq!(
                names.iter().any(|name| name == "todo_write"),
                depth == 0,
                "todo_write at depth {depth}"
            );
        }
    }
}
