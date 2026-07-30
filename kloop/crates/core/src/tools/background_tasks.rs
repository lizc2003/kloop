//! Background async-task registry + the `wait`/`stop_agent` tools (plan 26).
//!
//! Two producers register here, both with the identical fire-and-forget
//! lifecycle (detached run on an own cancel token, reinject a result into the
//! parent's inbox when done): a `task {"background": true}` sub-agent (plan 26)
//! and a `run_program {"background": true}` program (plan 24). This registry
//! tracks those in-flight tasks so the parent can block on any of them (`wait`)
//! or cancel a runaway (`stop_agent`), and so a concurrency cap catches fork
//! bombs. The label prefix (`agent-N` / `program-N`) tells them apart.
//!
//! Still deliberately SEPARATE from [`crate::tools::BackgroundShells`]: a shell
//! has an output file and no reinjection — a different lifecycle. Programs, by
//! contrast, share this registry precisely because their lifecycle is identical
//! to a sub-agent's.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use anyhow::Result;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use super::str_arg;
use super::ToolCtx;

/// How many background sub-agents may run at once. A loose cap to catch runaway
/// fan-out. The parent collects results via `wait`, so this bounds concurrency,
/// not total work.
const MAX_BACKGROUND_TASKS: usize = 8;

const DEFAULT_WAIT_MS: u64 = 30_000;
const MIN_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 3_600_000;

/// Terminal state of a background task. `MaxRounds` only arises for a sub-agent
/// (a program has no round limit); the rest are shared by both producers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskStatus {
    Running,
    Completed,
    Failed,
    MaxRounds,
    Aborted,
}

fn status_text(status: TaskStatus) -> &'static str {
    match status {
        TaskStatus::Running => "running",
        TaskStatus::Completed => "completed",
        TaskStatus::Failed => "failed",
        TaskStatus::MaxRounds => "stopped at round limit",
        TaskStatus::Aborted => "stopped",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TaskState {
    Running,
    CancelRequested,
    Finishing(TaskStatus),
    Terminal(TaskStatus),
}

impl TaskState {
    fn is_active(self) -> bool {
        !matches!(self, Self::Terminal(_))
    }

    #[cfg(test)]
    fn report_status(self) -> TaskStatus {
        match self {
            Self::Running => TaskStatus::Running,
            Self::CancelRequested => TaskStatus::Aborted,
            Self::Finishing(status) | Self::Terminal(status) => status,
        }
    }
}

struct Entry {
    /// First line of the prompt/source, for reporting what is outstanding.
    task: String,
    state: TaskState,
    /// The task's OWN cancel token — independent of any turn's cancel, so a
    /// finished parent turn never kills a still-running background task.
    cancel: CancellationToken,
    /// The worker, not its supervisor. Explicit session shutdown first requests
    /// cooperative cancellation, then aborts this handle at its deadline; the
    /// supervisor observes the JoinError and still publishes one terminal state.
    abort: Option<AbortHandle>,
}

#[derive(Default)]
struct Registry {
    closed: bool,
    tasks: HashMap<String, Entry>,
}

/// Session-scoped registry of background tasks — sub-agents (`agent-N`) and
/// programs (`program-N`). Normal shutdown is explicit because detached workers
/// themselves hold an `Arc` to this registry; last-Arc `Drop` is only a fallback.
pub struct BackgroundTasks {
    state: Mutex<Registry>,
    activity: watch::Sender<u64>,
}

impl Default for BackgroundTasks {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            state: Mutex::new(Registry::default()),
            activity,
        }
    }
}

impl BackgroundTasks {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Reserve a slot for a newly spawned task. Duplicate ids and a closing
    /// session are rejected rather than silently replacing a live cancellation
    /// handle.
    pub fn register(&self, id: &str, task: &str, cancel: CancellationToken) -> Result<(), String> {
        let mut registry = self.state.lock().unwrap();
        if registry.closed {
            return Err("the session is closing; no new background tasks may start".into());
        }
        if registry.tasks.contains_key(id) {
            return Err(format!("background task id {id} is already registered"));
        }
        let running = registry
            .tasks
            .values()
            .filter(|entry| entry.state.is_active())
            .count();
        if running >= MAX_BACKGROUND_TASKS {
            return Err(format!(
                "too many background tasks already running ({running}); wait for some to \
                 finish (use the wait tool) before dispatching more"
            ));
        }
        registry.tasks.insert(
            id.into(),
            Entry {
                task: task.into(),
                state: TaskState::Running,
                cancel,
                abort: None,
            },
        );
        Ok(())
    }

    /// Attach the worker after spawning it. Shutdown can race this step: a task
    /// already marked for cancellation is aborted immediately instead of becoming
    /// an untracked detached worker.
    pub(super) fn attach_abort(&self, id: &str, abort: AbortHandle) {
        let abort_now = {
            let mut registry = self.state.lock().unwrap();
            let closed = registry.closed;
            match registry.tasks.get_mut(id) {
                Some(entry) if entry.state == TaskState::Running && !closed => {
                    entry.abort = Some(abort);
                    return;
                }
                Some(entry) if entry.state.is_active() => {
                    entry.abort = Some(abort.clone());
                    true
                }
                _ => true,
            }
        };
        if abort_now {
            abort.abort();
        }
    }

    /// Atomically arbitrate natural completion against cancellation and publish
    /// the corresponding inbox activity while the registry remains in an active
    /// `Finishing` state. A waiter awakened by `publish` cannot observe zero
    /// running tasks before the delivery exists. Returns the terminal state only
    /// for the caller that won the one-shot transition.
    pub(super) fn finish(
        &self,
        id: &str,
        observed: TaskStatus,
        publish: impl FnOnce(TaskStatus, bool),
    ) -> Option<TaskStatus> {
        debug_assert!(observed != TaskStatus::Running);
        let (terminal, deliver) = {
            let mut registry = self.state.lock().unwrap();
            let entry = registry.tasks.get_mut(id)?;
            let (terminal, deliver) = match entry.state {
                TaskState::Running => (observed, true),
                TaskState::CancelRequested => (TaskStatus::Aborted, false),
                TaskState::Finishing(_) | TaskState::Terminal(_) => return None,
            };
            entry.state = TaskState::Finishing(terminal);
            (terminal, deliver)
        };
        publish(terminal, deliver);
        {
            let mut registry = self.state.lock().unwrap();
            let entry = registry
                .tasks
                .get_mut(id)
                .expect("finishing background task disappeared");
            debug_assert_eq!(entry.state, TaskState::Finishing(terminal));
            entry.state = TaskState::Terminal(terminal);
        }
        let next = (*self.activity.borrow()).wrapping_add(1);
        self.activity.send_replace(next);
        Some(terminal)
    }

    pub fn running_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .tasks
            .values()
            .filter(|entry| entry.state.is_active())
            .count()
    }

    #[cfg(test)]
    fn status(&self, id: &str) -> Option<TaskStatus> {
        self.state
            .lock()
            .unwrap()
            .tasks
            .get(id)
            .map(|entry| entry.state.report_status())
    }

    /// Linearize `stop_agent` against natural completion. The state changes
    /// under the registry lock before the token fires, so a completion that
    /// arrives afterward is suppressed even if its model request already ended.
    fn request_stop(&self, id: &str) -> Result<String, String> {
        let (task, cancel) = {
            let mut registry = self.state.lock().unwrap();
            let Some(entry) = registry.tasks.get_mut(id) else {
                return Err(format!("no background task with id {id}"));
            };
            match entry.state {
                TaskState::Running => {
                    entry.state = TaskState::CancelRequested;
                    (entry.task.clone(), entry.cancel.clone())
                }
                TaskState::CancelRequested => {
                    return Err(format!("stop already requested for {id}"));
                }
                TaskState::Finishing(status) | TaskState::Terminal(status) => {
                    return Err(format!(
                        "{id} is not running (status: {})",
                        status_text(status)
                    ));
                }
            }
        };
        cancel.cancel();
        Ok(task)
    }

    async fn wait_idle_until(&self, deadline: tokio::time::Instant) -> bool {
        let mut activity = self.activity.subscribe();
        loop {
            if self.running_count() == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, activity.changed())
                .await
                .is_err()
            {
                return false;
            }
        }
    }

    /// Close the registry, cooperatively stop every active worker, and wait for
    /// their supervisors to publish terminal states. At the deadline abort the
    /// workers; supervisors still run and finish the lifecycle.
    pub(crate) async fn shutdown(&self, timeout: Duration) -> usize {
        {
            let mut registry = self.state.lock().unwrap();
            registry.closed = true;
            for entry in registry.tasks.values_mut() {
                if entry.state == TaskState::Running {
                    entry.state = TaskState::CancelRequested;
                    entry.cancel.cancel();
                }
            }
        }

        let deadline = tokio::time::Instant::now() + timeout;
        if self.wait_idle_until(deadline).await {
            return 0;
        }

        let aborts: Vec<AbortHandle> = self
            .state
            .lock()
            .unwrap()
            .tasks
            .values()
            .filter(|entry| entry.state.is_active())
            .filter_map(|entry| entry.abort.clone())
            .collect();
        for abort in aborts {
            abort.abort();
        }
        let _ = self
            .wait_idle_until(tokio::time::Instant::now() + Duration::from_secs(1))
            .await;
        self.running_count()
    }
}

impl Drop for BackgroundTasks {
    /// Synchronous fallback for runtimes that cannot call explicit shutdown.
    /// Normal session teardown uses [`BackgroundTasks::shutdown`] because live
    /// detached workers retain this registry and can postpone last-Arc `Drop`.
    fn drop(&mut self) {
        for entry in self.state.lock().unwrap().tasks.values() {
            if entry.state.is_active() {
                entry.cancel.cancel();
                if let Some(abort) = &entry.abort {
                    abort.abort();
                }
            }
        }
    }
}

/// `wait`: block until a background task finishes (or new input arrives, or
/// the deadline passes), then return a status line. Deliberately does NOT drain
/// — the result is delivered as a user message at the next round boundary.
pub(super) async fn wait_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let timeout_ms = input["timeout_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_WAIT_MS)
        .clamp(MIN_WAIT_MS, MAX_WAIT_MS);
    let inbox = &ctx.cfg.inbox;
    let tasks = &ctx.cfg.background_tasks;
    // Subscribe before checking either source. A completion that races with the
    // checks advances this receiver, while activity drained before this call is
    // already part of its baseline and cannot wake a later generation of work.
    let mut activity = inbox.subscribe_activity();

    if tasks.running_count() == 0 && inbox.is_empty() {
        return Ok(
            "No background tasks are running and nothing is pending. Dispatch work with \
             task {\"background\": true} or run_program {\"background\": true}, or just \
             continue."
                .into(),
        );
    }
    // Something is already pending: return without draining; the round boundary
    // will deliver it.
    if !inbox.is_empty() {
        return Ok(status_report(
            "Background activity is ready and will be delivered on the next step.",
            tasks.running_count(),
        ));
    }
    tokio::select! {
        _ = activity.changed() => Ok(status_report(
            "A background task finished or new input arrived; it will be delivered on the next step.",
            tasks.running_count(),
        )),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => Ok(status_report(
            &format!("Timed out after {}s with no completion.", timeout_ms / 1000),
            tasks.running_count(),
        )),
        _ = ctx.cancel.cancelled() => Ok("Wait interrupted.".into()),
    }
}

fn status_report(head: &str, running: usize) -> String {
    match running {
        0 => format!("{head} No background tasks are still running."),
        1 => format!("{head} 1 background task is still running."),
        n => format!("{head} {n} background tasks are still running."),
    }
}

/// `stop_agent`: cancel a running background task (sub-agent or program) by id.
/// It ends Aborted and reports no result.
pub(super) async fn stop_agent_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = str_arg(input, "agent_id", "stop_agent")?;
    match ctx.cfg.background_tasks.request_stop(id) {
        Ok(task) => Ok(format!(
            "Stopping background task {id} ({task}). It will not report a result."
        )),
        Err(error) => bail!(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::InboxItem;
    use crate::tools::testutil::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn wait_returns_immediately_when_nothing_is_running() {
        let ctx = test_ctx(0, "wait-idle");
        let (out, is_error) = run_tool("wait", json!({}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("No background tasks are running"), "{out}");
    }

    #[tokio::test]
    async fn wait_reports_pending_without_draining() {
        let ctx = test_ctx(0, "wait-pending");
        ctx.cfg.inbox.push(InboxItem::SubAgentResult {
            label: "agent-1".into(),
            summary: "done".into(),
        });
        let (out, is_error) = run_tool("wait", json!({}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("ready and will be delivered"), "{out}");
        assert!(!ctx.cfg.inbox.is_empty(), "wait must not drain the inbox");
    }

    #[test]
    fn register_enforces_the_concurrency_cap() {
        let registry = BackgroundTasks::default();
        for i in 0..MAX_BACKGROUND_TASKS {
            registry
                .register(&format!("agent-{i}"), "t", CancellationToken::new())
                .unwrap();
        }
        assert_eq!(registry.running_count(), MAX_BACKGROUND_TASKS);
        let error = registry
            .register("agent-over", "t", CancellationToken::new())
            .unwrap_err();
        assert!(error.contains("too many background tasks"), "{error}");
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let registry = BackgroundTasks::default();
        registry
            .register("agent-1", "first", CancellationToken::new())
            .unwrap();
        let error = registry
            .register("agent-1", "second", CancellationToken::new())
            .unwrap_err();
        assert!(error.contains("already registered"), "{error}");
    }

    #[test]
    fn finishing_frees_a_slot_once_delivery_exists() {
        let registry = BackgroundTasks::default();
        for i in 0..MAX_BACKGROUND_TASKS {
            registry
                .register(&format!("agent-{i}"), "t", CancellationToken::new())
                .unwrap();
        }
        let published = AtomicUsize::new(0);
        registry.finish("agent-0", TaskStatus::Completed, |status, deliver| {
            assert_eq!(status, TaskStatus::Completed);
            assert!(deliver);
            assert_eq!(
                registry.running_count(),
                MAX_BACKGROUND_TASKS,
                "Finishing must remain active until delivery exists"
            );
            published.store(1, Ordering::SeqCst);
        });
        assert_eq!(published.load(Ordering::SeqCst), 1);
        assert_eq!(registry.running_count(), MAX_BACKGROUND_TASKS - 1);
        registry
            .register("agent-new", "t", CancellationToken::new())
            .unwrap();
    }

    #[test]
    fn stop_wins_the_terminal_race_and_suppresses_delivery() {
        let registry = BackgroundTasks::default();
        let cancel = CancellationToken::new();
        registry
            .register("agent-1", "build the thing", cancel.clone())
            .unwrap();
        let task = registry.request_stop("agent-1").unwrap();
        assert_eq!(task, "build the thing");
        assert!(cancel.is_cancelled());
        assert!(registry
            .request_stop("agent-1")
            .unwrap_err()
            .contains("already requested"));

        let delivered = AtomicUsize::new(0);
        assert_eq!(
            registry.finish("agent-1", TaskStatus::Completed, |status, deliver| {
                assert_eq!(status, TaskStatus::Aborted);
                assert!(!deliver);
                delivered.fetch_add(usize::from(deliver), Ordering::SeqCst);
            }),
            Some(TaskStatus::Aborted)
        );
        assert_eq!(delivered.load(Ordering::SeqCst), 0);
        assert_eq!(registry.status("agent-1"), Some(TaskStatus::Aborted));
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn completion_wins_before_stop_and_finishes_only_once() {
        let registry = BackgroundTasks::default();
        registry
            .register("agent-1", "done", CancellationToken::new())
            .unwrap();
        let deliveries = AtomicUsize::new(0);
        assert_eq!(
            registry.finish("agent-1", TaskStatus::Completed, |_, deliver| {
                deliveries.fetch_add(usize::from(deliver), Ordering::SeqCst);
            }),
            Some(TaskStatus::Completed)
        );
        assert_eq!(
            registry.finish("agent-1", TaskStatus::Failed, |_, _| {
                deliveries.fetch_add(1, Ordering::SeqCst);
            }),
            None
        );
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert!(registry
            .request_stop("agent-1")
            .unwrap_err()
            .contains("completed"));
    }

    #[tokio::test]
    async fn explicit_shutdown_cancels_real_worker_topology() {
        let registry = BackgroundTasks::new();
        let cancel = CancellationToken::new();
        registry.register("agent-1", "t", cancel.clone()).unwrap();
        let worker = tokio::spawn({
            let cancel = cancel.clone();
            async move { cancel.cancelled().await }
        });
        registry.attach_abort("agent-1", worker.abort_handle());
        let supervisor = tokio::spawn({
            let registry = registry.clone();
            async move {
                let _ = worker.await;
                registry.finish("agent-1", TaskStatus::Aborted, |_, _| {});
            }
        });

        assert_eq!(registry.shutdown(Duration::from_secs(1)).await, 0);
        supervisor.await.unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(registry.status("agent-1"), Some(TaskStatus::Aborted));
    }

    #[test]
    fn drop_is_a_best_effort_fallback() {
        let cancel = CancellationToken::new();
        {
            let registry = BackgroundTasks::default();
            registry.register("agent-1", "t", cancel.clone()).unwrap();
        }
        assert!(cancel.is_cancelled());
    }
}
