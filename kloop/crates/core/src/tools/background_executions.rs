//! Session-scoped registry for detached Agent, Program, and Workflow executions.
//!
//! All three producers run on their own cancellation token and reinject their
//! result into the parent's inbox. The registry keeps their shared lifecycle and
//! concurrency cap while retaining the resource kind needed by `stop_agent`,
//! `stop_program`, and `stop_workflow`. `wait_for_activity` observes this registry
//! plus background shells and the inbox without draining any result.
//!
//! [`crate::tools::BackgroundShells`] remains separate: a shell owns an output
//! file and reinjects only a terminal pointer, not a result body.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::watch;
use tokio::task::AbortHandle;
use tokio_util::sync::CancellationToken;

use super::strict_str_arg;
use super::ToolCtx;

/// How many background executions may run at once. A loose cap to catch runaway
/// fan-out. The parent collects results at step boundaries and may observe activity
/// through `wait_for_activity`, so this bounds concurrency, not total work.
const MAX_BACKGROUND_EXECUTIONS: usize = 8;

const DEFAULT_WAIT_MS: u64 = 30_000;
const MIN_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 3_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExecutionKind {
    Agent,
    Program,
    Workflow,
}

impl ExecutionKind {
    fn id_prefix(self) -> &'static str {
        match self {
            Self::Agent => "agent-",
            Self::Program => "program-",
            Self::Workflow => "workflow-",
        }
    }

    fn noun(self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::Program => "program",
            Self::Workflow => "workflow",
        }
    }

    fn stop_tool(self) -> &'static str {
        match self {
            Self::Agent => "stop_agent",
            Self::Program => "stop_program",
            Self::Workflow => "stop_workflow",
        }
    }

    fn id_arg(self) -> &'static str {
        match self {
            Self::Agent => "agent_id",
            Self::Program => "program_id",
            Self::Workflow => "workflow_id",
        }
    }
}

fn kind_from_id(id: &str) -> Option<ExecutionKind> {
    match id {
        id if id.starts_with("agent-") => Some(ExecutionKind::Agent),
        id if id.starts_with("program-") => Some(ExecutionKind::Program),
        id if id.starts_with("workflow-") => Some(ExecutionKind::Workflow),
        _ => None,
    }
}

pub(super) fn execution_stop_hint(id: &str) -> Option<String> {
    kind_from_id(id).map(|kind| {
        format!(
            "{id} is a {} id; use {} {{{}: \"{id}\"}} instead",
            kind.noun(),
            kind.stop_tool(),
            kind.id_arg()
        )
    })
}

/// Terminal state of a background execution. `MaxRounds` only arises for a
/// sub-agent; the rest are shared by all three producers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutionStatus {
    Running,
    Completed,
    Failed,
    MaxRounds,
    Aborted,
}

fn status_text(status: ExecutionStatus) -> &'static str {
    match status {
        ExecutionStatus::Running => "running",
        ExecutionStatus::Completed => "completed",
        ExecutionStatus::Failed => "failed",
        ExecutionStatus::MaxRounds => "stopped at round limit",
        ExecutionStatus::Aborted => "stopped",
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ExecutionState {
    Running,
    CancelRequested,
    Finishing(ExecutionStatus),
    Terminal(ExecutionStatus),
}

impl ExecutionState {
    fn is_active(self) -> bool {
        !matches!(self, Self::Terminal(_))
    }

    #[cfg(test)]
    fn report_status(self) -> ExecutionStatus {
        match self {
            Self::Running => ExecutionStatus::Running,
            Self::CancelRequested => ExecutionStatus::Aborted,
            Self::Finishing(status) | Self::Terminal(status) => status,
        }
    }
}

struct Entry {
    kind: ExecutionKind,
    /// First line of the prompt/source, for reporting what is outstanding.
    description: String,
    state: ExecutionState,
    /// The execution's OWN cancel token — independent of any turn's cancel, so a
    /// finished parent turn never kills still-running background work.
    cancel: CancellationToken,
    /// The worker, not its supervisor. Explicit session shutdown first requests
    /// cooperative cancellation, then aborts this handle at its deadline; the
    /// supervisor observes the JoinError and still publishes one terminal state.
    abort: Option<AbortHandle>,
}

#[derive(Default)]
struct Registry {
    closed: bool,
    executions: HashMap<String, Entry>,
}

/// Session-scoped registry of background agent, program, and Workflow executions.
/// Normal shutdown is explicit because detached workers themselves hold an `Arc`
/// to this registry; last-Arc `Drop` is only a fallback.
pub struct BackgroundExecutions {
    state: Mutex<Registry>,
    activity: watch::Sender<u64>,
}

impl Default for BackgroundExecutions {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            state: Mutex::new(Registry::default()),
            activity,
        }
    }
}

impl BackgroundExecutions {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Reserve a slot for a newly spawned execution. Duplicate ids, a mismatched
    /// resource prefix, and a closing session fail instead of replacing a live
    /// cancellation handle.
    pub(super) fn register(
        &self,
        kind: ExecutionKind,
        id: &str,
        description: &str,
        cancel: CancellationToken,
    ) -> Result<(), String> {
        if !id.starts_with(kind.id_prefix()) {
            return Err(format!(
                "{} id must start with {} (got {id})",
                kind.noun(),
                kind.id_prefix()
            ));
        }
        let mut registry = self.state.lock().unwrap();
        if registry.closed {
            return Err("the session is closing; no new background executions may start".into());
        }
        if registry.executions.contains_key(id) {
            return Err(format!(
                "background execution id {id} is already registered"
            ));
        }
        let running = registry
            .executions
            .values()
            .filter(|entry| entry.state.is_active())
            .count();
        if running >= MAX_BACKGROUND_EXECUTIONS {
            return Err(format!(
                "too many background executions already running ({running}); wait for some to \
                 finish (use wait_for_activity) before dispatching more"
            ));
        }
        registry.executions.insert(
            id.into(),
            Entry {
                kind,
                description: description.into(),
                state: ExecutionState::Running,
                cancel,
                abort: None,
            },
        );
        Ok(())
    }

    /// Attach the worker after spawning it. Shutdown can race this step: an execution
    /// already marked for cancellation is aborted immediately instead of becoming
    /// an untracked detached worker.
    pub(super) fn attach_abort(&self, id: &str, abort: AbortHandle) {
        let abort_now = {
            let mut registry = self.state.lock().unwrap();
            let closed = registry.closed;
            match registry.executions.get_mut(id) {
                Some(entry) if entry.state == ExecutionState::Running && !closed => {
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
    /// running executions before the delivery exists. Returns the terminal state only
    /// for the caller that won the one-shot transition.
    pub(super) fn finish(
        &self,
        id: &str,
        observed: ExecutionStatus,
        publish: impl FnOnce(ExecutionStatus, bool),
    ) -> Option<ExecutionStatus> {
        debug_assert!(observed != ExecutionStatus::Running);
        let (terminal, deliver) = {
            let mut registry = self.state.lock().unwrap();
            let entry = registry.executions.get_mut(id)?;
            let (terminal, deliver) = match entry.state {
                ExecutionState::Running => (observed, true),
                ExecutionState::CancelRequested => (ExecutionStatus::Aborted, false),
                ExecutionState::Finishing(_) | ExecutionState::Terminal(_) => return None,
            };
            entry.state = ExecutionState::Finishing(terminal);
            (terminal, deliver)
        };
        publish(terminal, deliver);
        {
            let mut registry = self.state.lock().unwrap();
            let entry = registry
                .executions
                .get_mut(id)
                .expect("finishing background execution disappeared");
            debug_assert_eq!(entry.state, ExecutionState::Finishing(terminal));
            entry.state = ExecutionState::Terminal(terminal);
        }
        let next = (*self.activity.borrow()).wrapping_add(1);
        self.activity.send_replace(next);
        Some(terminal)
    }

    pub fn running_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .executions
            .values()
            .filter(|entry| entry.state.is_active())
            .count()
    }

    #[cfg(test)]
    fn status(&self, id: &str) -> Option<ExecutionStatus> {
        self.state
            .lock()
            .unwrap()
            .executions
            .get(id)
            .map(|entry| entry.state.report_status())
    }

    /// Linearize a resource-specific stop against natural completion. A known ID
    /// from another resource points the caller at that resource's stop tool.
    fn request_stop(&self, expected: ExecutionKind, id: &str) -> Result<String, String> {
        if let Some(actual) = kind_from_id(id) {
            if actual != expected {
                return Err(execution_stop_hint(id).expect("typed execution id has a stop hint"));
            }
        } else if id.starts_with("bg-") {
            return Err(format!(
                "{id} is a bash id; use stop_bash {{bash_id: \"{id}\"}} instead"
            ));
        } else if id.starts_with("wf_") {
            return Err(format!(
                "{id} is a durable workflow run id; stop_workflow requires the workflow-N execution id returned by workflow"
            ));
        }

        let (description, cancel) = {
            let mut registry = self.state.lock().unwrap();
            let Some(entry) = registry.executions.get_mut(id) else {
                return Err(format!(
                    "no running background {} with id {id}",
                    expected.noun()
                ));
            };
            if entry.kind != expected {
                return Err(format!(
                    "{id} belongs to a {}; use {} instead",
                    entry.kind.noun(),
                    entry.kind.stop_tool()
                ));
            }
            match entry.state {
                ExecutionState::Running => {
                    entry.state = ExecutionState::CancelRequested;
                    (entry.description.clone(), entry.cancel.clone())
                }
                ExecutionState::CancelRequested => {
                    return Err(format!("stop already requested for {id}"));
                }
                ExecutionState::Finishing(status) | ExecutionState::Terminal(status) => {
                    return Err(format!(
                        "{id} is not running (status: {})",
                        status_text(status)
                    ));
                }
            }
        };
        cancel.cancel();
        Ok(description)
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
            for entry in registry.executions.values_mut() {
                if entry.state == ExecutionState::Running {
                    entry.state = ExecutionState::CancelRequested;
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
            .executions
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

impl Drop for BackgroundExecutions {
    /// Synchronous fallback for runtimes that cannot call explicit shutdown.
    /// Normal session teardown uses [`BackgroundExecutions::shutdown`] because live
    /// detached workers retain this registry and can postpone last-Arc `Drop`.
    fn drop(&mut self) {
        for entry in self.state.lock().unwrap().executions.values() {
            if entry.state.is_active() {
                entry.cancel.cancel();
                if let Some(abort) = &entry.abort {
                    abort.abort();
                }
            }
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WaitForActivityInput {
    #[serde(default)]
    timeout_ms: Option<u64>,
}

/// Block until a background execution finishes, a shell reaches terminal state,
/// new inbox input arrives, or the deadline passes. This deliberately does NOT
/// drain: results are delivered at the next round boundary.
pub(super) async fn wait_for_activity_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let parsed: WaitForActivityInput =
        serde_json::from_value(input.clone()).context("wait_for_activity: invalid input")?;
    let timeout_ms = parsed
        .timeout_ms
        .unwrap_or(DEFAULT_WAIT_MS)
        .clamp(MIN_WAIT_MS, MAX_WAIT_MS);
    let inbox = &ctx.cfg.inbox;
    let executions = &ctx.cfg.background_executions;
    let shells = &ctx.cfg.background_shells;
    // Subscribe before checking all sources. Every producer publishes terminal
    // delivery through the inbox, so a completion racing these checks advances
    // this receiver instead of becoming a lost wakeup.
    let mut activity = inbox.subscribe_activity();

    let execution_count = executions.running_count();
    let shell_count = shells.running_count();
    if execution_count == 0 && shell_count == 0 && inbox.is_empty() {
        return Ok(
            "No background activity is running and nothing is pending. Start work with \
             run_agent {\"background\": true}, run_program {\"background\": true}, \
             workflow, or bash {\"background\": true}; otherwise continue."
                .into(),
        );
    }
    if !inbox.is_empty() {
        return Ok(status_report(
            "Background activity is ready and will be delivered on the next step.",
            execution_count,
            shell_count,
        ));
    }
    tokio::select! {
        _ = activity.changed() => Ok(status_report(
            "Background work finished or new input arrived; it will be delivered on the next step.",
            executions.running_count(),
            shells.running_count(),
        )),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => Ok(status_report(
            &format!("Timed out after {}s with no activity.", timeout_ms / 1000),
            executions.running_count(),
            shells.running_count(),
        )),
        _ = ctx.cancel.cancelled() => Ok("Wait interrupted.".into()),
    }
}

fn status_report(head: &str, executions: usize, shells: usize) -> String {
    let total = executions + shells;
    match total {
        0 => format!("{head} No background work is still running."),
        1 => format!("{head} 1 background item is still running."),
        _ => format!(
            "{head} {total} background items are still running ({executions} \
             agent/program/workflow, {shells} shell)."
        ),
    }
}

async fn stop_tool(
    input: &Value,
    ctx: &ToolCtx,
    kind: ExecutionKind,
    tool: &str,
) -> Result<String> {
    let id = strict_str_arg(input, kind.id_arg(), tool)?;
    match ctx.cfg.background_executions.request_stop(kind, id) {
        Ok(description) => Ok(format!(
            "Stopping background {} {id} ({description}). It will not report a result.",
            kind.noun()
        )),
        Err(error) => bail!(error),
    }
}

pub(super) async fn stop_agent_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    stop_tool(input, ctx, ExecutionKind::Agent, "stop_agent").await
}

pub(super) async fn stop_program_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    stop_tool(input, ctx, ExecutionKind::Program, "stop_program").await
}

pub(super) async fn stop_workflow_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    stop_tool(input, ctx, ExecutionKind::Workflow, "stop_workflow").await
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
        let (out, is_error) = run_tool("wait_for_activity", json!({}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("No background activity is running"), "{out}");
    }

    #[tokio::test]
    async fn wait_reports_pending_without_draining() {
        let ctx = test_ctx(0, "wait-pending");
        ctx.cfg.inbox.push(InboxItem::SubAgentResult {
            label: "agent-1".into(),
            summary: "done".into(),
        });
        let (out, is_error) = run_tool("wait_for_activity", json!({}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("ready and will be delivered"), "{out}");
        assert!(!ctx.cfg.inbox.is_empty(), "wait must not drain the inbox");
    }

    #[tokio::test]
    async fn wait_rejects_resource_ids_and_wrong_timeout_types() {
        let ctx = test_ctx(0, "wait-strict-input");
        let (with_id, id_error) =
            run_tool("wait_for_activity", json!({"agent_id": "agent-1"}), &ctx).await;
        assert!(id_error);
        assert!(with_id.contains("unknown field `agent_id`"), "{with_id}");

        let (wrong_timeout, timeout_error) =
            run_tool("wait_for_activity", json!({"timeout_ms": "30000"}), &ctx).await;
        assert!(timeout_error);
        assert!(wrong_timeout.contains("invalid type"), "{wrong_timeout}");
    }

    #[tokio::test]
    async fn stop_inputs_reject_unknown_fields() {
        let ctx = test_ctx(0, "stop-strict-input");
        for (tool, input, field) in [
            (
                "stop_agent",
                json!({"agent_id": "agent-1", "program_id": "program-1"}),
                "program_id",
            ),
            (
                "stop_program",
                json!({"program_id": "program-1", "agent_id": "agent-1"}),
                "agent_id",
            ),
            (
                "stop_workflow",
                json!({"workflow_id": "workflow-1", "program_id": "program-1"}),
                "program_id",
            ),
        ] {
            let (output, is_error) = run_tool(tool, input, &ctx).await;
            assert!(is_error, "{tool}: {output}");
            assert!(
                output.contains(&format!("unknown field `{field}`")),
                "{output}"
            );
        }
    }

    #[tokio::test]
    async fn wait_observes_a_background_shell_without_draining_its_result() {
        let ctx = test_ctx(0, "wait-shell");
        let (launched, launch_error) = run_tool(
            "bash",
            json!({"command": "sleep 0.2; printf shell-done", "background": true}),
            &ctx,
        )
        .await;
        assert!(!launch_error, "{launched}");
        let (waited, wait_error) =
            run_tool("wait_for_activity", json!({"timeout_ms": 10_000}), &ctx).await;
        assert!(!wait_error, "{waited}");
        assert!(!waited.contains("No background activity"), "{waited}");
        assert!(
            !ctx.cfg.inbox.is_empty(),
            "wait must not drain shell completion"
        );
    }

    #[test]
    fn resource_kind_controls_registration_and_matching_stop() {
        let registry = BackgroundExecutions::default();
        let mismatch = registry
            .register(
                ExecutionKind::Program,
                "agent-1",
                "wrong prefix",
                CancellationToken::new(),
            )
            .unwrap_err();
        assert!(mismatch.contains("program id must start with program-"));

        for (kind, id, description) in [
            (ExecutionKind::Agent, "agent-1", "agent"),
            (ExecutionKind::Program, "program-1", "program"),
            (ExecutionKind::Workflow, "workflow-1", "workflow"),
        ] {
            let cancel = CancellationToken::new();
            registry
                .register(kind, id, description, cancel.clone())
                .unwrap();
            assert_eq!(registry.request_stop(kind, id).unwrap(), description);
            assert!(
                cancel.is_cancelled(),
                "{kind:?} cancellation was not signalled"
            );
        }
    }

    #[tokio::test]
    async fn typed_stop_rejects_every_other_resource_id_with_a_directed_hint() {
        let ctx = test_ctx(0, "typed-stop-boundaries");
        let cases = [
            (
                "stop_agent",
                json!({"agent_id": "program-1"}),
                "use stop_program {program_id: \"program-1\"}",
            ),
            (
                "stop_agent",
                json!({"agent_id": "workflow-1"}),
                "use stop_workflow {workflow_id: \"workflow-1\"}",
            ),
            (
                "stop_agent",
                json!({"agent_id": "bg-1"}),
                "use stop_bash {bash_id: \"bg-1\"}",
            ),
            (
                "stop_program",
                json!({"program_id": "agent-1"}),
                "use stop_agent {agent_id: \"agent-1\"}",
            ),
            (
                "stop_program",
                json!({"program_id": "workflow-1"}),
                "use stop_workflow {workflow_id: \"workflow-1\"}",
            ),
            (
                "stop_program",
                json!({"program_id": "bg-1"}),
                "use stop_bash {bash_id: \"bg-1\"}",
            ),
            (
                "stop_workflow",
                json!({"workflow_id": "agent-1"}),
                "use stop_agent {agent_id: \"agent-1\"}",
            ),
            (
                "stop_workflow",
                json!({"workflow_id": "program-1"}),
                "use stop_program {program_id: \"program-1\"}",
            ),
            (
                "stop_workflow",
                json!({"workflow_id": "bg-1"}),
                "use stop_bash {bash_id: \"bg-1\"}",
            ),
        ];
        for (tool, input, hint) in cases {
            let (output, is_error) = run_tool(tool, input, &ctx).await;
            assert!(is_error, "{tool}: {output}");
            assert!(output.contains(hint), "{tool}: {output}");
        }

        let (durable, durable_error) =
            run_tool("stop_workflow", json!({"workflow_id": "wf_plan66"}), &ctx).await;
        assert!(durable_error);
        assert!(durable.contains("durable workflow run id"), "{durable}");
        assert!(durable.contains("workflow-N execution id"), "{durable}");
    }

    #[test]
    fn register_enforces_the_concurrency_cap() {
        let registry = BackgroundExecutions::default();
        for i in 0..MAX_BACKGROUND_EXECUTIONS {
            registry
                .register(
                    ExecutionKind::Agent,
                    &format!("agent-{i}"),
                    "t",
                    CancellationToken::new(),
                )
                .unwrap();
        }
        assert_eq!(registry.running_count(), MAX_BACKGROUND_EXECUTIONS);
        let error = registry
            .register(
                ExecutionKind::Agent,
                "agent-over",
                "t",
                CancellationToken::new(),
            )
            .unwrap_err();
        assert!(error.contains("too many background executions"), "{error}");
    }

    #[test]
    fn duplicate_registration_is_rejected() {
        let registry = BackgroundExecutions::default();
        registry
            .register(
                ExecutionKind::Agent,
                "agent-1",
                "first",
                CancellationToken::new(),
            )
            .unwrap();
        let error = registry
            .register(
                ExecutionKind::Agent,
                "agent-1",
                "second",
                CancellationToken::new(),
            )
            .unwrap_err();
        assert!(error.contains("already registered"), "{error}");
    }

    #[test]
    fn finishing_frees_a_slot_once_delivery_exists() {
        let registry = BackgroundExecutions::default();
        for i in 0..MAX_BACKGROUND_EXECUTIONS {
            registry
                .register(
                    ExecutionKind::Agent,
                    &format!("agent-{i}"),
                    "t",
                    CancellationToken::new(),
                )
                .unwrap();
        }
        let published = AtomicUsize::new(0);
        registry.finish("agent-0", ExecutionStatus::Completed, |status, deliver| {
            assert_eq!(status, ExecutionStatus::Completed);
            assert!(deliver);
            assert_eq!(
                registry.running_count(),
                MAX_BACKGROUND_EXECUTIONS,
                "Finishing must remain active until delivery exists"
            );
            published.store(1, Ordering::SeqCst);
        });
        assert_eq!(published.load(Ordering::SeqCst), 1);
        assert_eq!(registry.running_count(), MAX_BACKGROUND_EXECUTIONS - 1);
        registry
            .register(
                ExecutionKind::Agent,
                "agent-new",
                "t",
                CancellationToken::new(),
            )
            .unwrap();
    }

    #[test]
    fn stop_wins_the_terminal_race_and_suppresses_delivery() {
        let registry = BackgroundExecutions::default();
        let cancel = CancellationToken::new();
        registry
            .register(
                ExecutionKind::Agent,
                "agent-1",
                "build the thing",
                cancel.clone(),
            )
            .unwrap();
        let description = registry
            .request_stop(ExecutionKind::Agent, "agent-1")
            .unwrap();
        assert_eq!(description, "build the thing");
        assert!(cancel.is_cancelled());
        assert!(registry
            .request_stop(ExecutionKind::Agent, "agent-1")
            .unwrap_err()
            .contains("already requested"));

        let delivered = AtomicUsize::new(0);
        assert_eq!(
            registry.finish("agent-1", ExecutionStatus::Completed, |status, deliver| {
                assert_eq!(status, ExecutionStatus::Aborted);
                assert!(!deliver);
                delivered.fetch_add(usize::from(deliver), Ordering::SeqCst);
            }),
            Some(ExecutionStatus::Aborted)
        );
        assert_eq!(delivered.load(Ordering::SeqCst), 0);
        assert_eq!(registry.status("agent-1"), Some(ExecutionStatus::Aborted));
        assert_eq!(registry.running_count(), 0);
    }

    #[test]
    fn completion_wins_before_stop_and_finishes_only_once() {
        let registry = BackgroundExecutions::default();
        registry
            .register(
                ExecutionKind::Agent,
                "agent-1",
                "done",
                CancellationToken::new(),
            )
            .unwrap();
        let deliveries = AtomicUsize::new(0);
        assert_eq!(
            registry.finish("agent-1", ExecutionStatus::Completed, |_, deliver| {
                deliveries.fetch_add(usize::from(deliver), Ordering::SeqCst);
            }),
            Some(ExecutionStatus::Completed)
        );
        assert_eq!(
            registry.finish("agent-1", ExecutionStatus::Failed, |_, _| {
                deliveries.fetch_add(1, Ordering::SeqCst);
            }),
            None
        );
        assert_eq!(deliveries.load(Ordering::SeqCst), 1);
        assert!(registry
            .request_stop(ExecutionKind::Agent, "agent-1")
            .unwrap_err()
            .contains("completed"));
    }

    #[tokio::test]
    async fn explicit_shutdown_cancels_real_worker_topology() {
        let registry = BackgroundExecutions::new();
        let cancel = CancellationToken::new();
        registry
            .register(ExecutionKind::Agent, "agent-1", "t", cancel.clone())
            .unwrap();
        let worker = tokio::spawn({
            let cancel = cancel.clone();
            async move { cancel.cancelled().await }
        });
        registry.attach_abort("agent-1", worker.abort_handle());
        let supervisor = tokio::spawn({
            let registry = registry.clone();
            async move {
                let _ = worker.await;
                registry.finish("agent-1", ExecutionStatus::Aborted, |_, _| {});
            }
        });

        assert_eq!(registry.shutdown(Duration::from_secs(1)).await, 0);
        supervisor.await.unwrap();
        assert!(cancel.is_cancelled());
        assert_eq!(registry.status("agent-1"), Some(ExecutionStatus::Aborted));
    }

    #[test]
    fn drop_is_a_best_effort_fallback() {
        let cancel = CancellationToken::new();
        {
            let registry = BackgroundExecutions::default();
            registry
                .register(ExecutionKind::Agent, "agent-1", "t", cancel.clone())
                .unwrap();
        }
        assert!(cancel.is_cancelled());
    }
}
