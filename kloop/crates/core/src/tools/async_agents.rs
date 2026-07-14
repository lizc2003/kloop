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
//! has an output file and no reinjection — a different lifecycle. codex keeps
//! shells and agents in distinct mechanisms and cc only unifies the *state*
//! model, not spawn, so shells stay out. Programs, by contrast, share this
//! registry precisely because their lifecycle is identical to a sub-agent's
//! (the "third consumer" that made generalizing worth it, plan 26).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::str_arg;
use super::ToolCtx;

/// How many background sub-agents may run at once. A loose cap to catch runaway
/// fan-out (codex's V2 residency is 3; cc batches at 10). The parent collects
/// results via `wait`, so this only bounds concurrency, not total work.
const MAX_BACKGROUND_AGENTS: usize = 8;

const DEFAULT_WAIT_MS: u64 = 30_000;
const MIN_WAIT_MS: u64 = 10_000;
const MAX_WAIT_MS: u64 = 3_600_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Running,
    Completed,
    Failed,
    MaxRounds,
    Aborted,
}

fn status_text(s: AgentStatus) -> &'static str {
    match s {
        AgentStatus::Running => "running",
        AgentStatus::Completed => "completed",
        AgentStatus::Failed => "failed",
        AgentStatus::MaxRounds => "stopped at round limit",
        AgentStatus::Aborted => "stopped",
    }
}

struct Entry {
    /// First line of the prompt, for reporting what is outstanding.
    task: String,
    status: AgentStatus,
    /// The agent's OWN cancel token — independent of any turn's cancel, so a
    /// finished parent turn never kills a still-running background agent.
    cancel: CancellationToken,
}

/// Session-scoped registry of background sub-agents (one per Config; sub-agents
/// share the parent's through the Config clone, though only the depth-0 agent
/// spawns into it).
#[derive(Default)]
pub struct AsyncAgents {
    agents: Mutex<HashMap<String, Entry>>,
}

impl AsyncAgents {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// Reserve a slot for a newly spawned agent. Err (model-facing) if the
    /// concurrency cap is already reached.
    pub fn register(&self, id: &str, task: &str, cancel: CancellationToken) -> Result<(), String> {
        let mut agents = self.agents.lock().unwrap();
        let running = agents
            .values()
            .filter(|e| e.status == AgentStatus::Running)
            .count();
        if running >= MAX_BACKGROUND_AGENTS {
            return Err(format!(
                "too many background tasks already running ({running}); wait for some to \
                 finish (use the wait tool) before dispatching more"
            ));
        }
        agents.insert(
            id.into(),
            Entry {
                task: task.into(),
                status: AgentStatus::Running,
                cancel,
            },
        );
        Ok(())
    }

    pub fn set_status(&self, id: &str, status: AgentStatus) {
        if let Some(e) = self.agents.lock().unwrap().get_mut(id) {
            e.status = status;
        }
    }

    pub fn running_count(&self) -> usize {
        self.agents
            .lock()
            .unwrap()
            .values()
            .filter(|e| e.status == AgentStatus::Running)
            .count()
    }

    /// Flag a running agent's own cancel token; Err carries the model-facing
    /// reason. Cancelling makes the sub-agent end Aborted, which does NOT
    /// reinject (codex's is_final: an interrupted child's partial output is
    /// noise).
    fn request_stop(&self, id: &str) -> Result<String, String> {
        let agents = self.agents.lock().unwrap();
        let Some(e) = agents.get(id) else {
            return Err(format!("no background task with id {id}"));
        };
        if e.status != AgentStatus::Running {
            return Err(format!(
                "{id} is not running (status: {})",
                status_text(e.status)
            ));
        }
        e.cancel.cancel();
        Ok(e.task.clone())
    }
}

impl Drop for AsyncAgents {
    /// Best-effort reaping at session end: cancel every still-running agent so
    /// detached tasks don't outlive the session.
    fn drop(&mut self) {
        for e in self.agents.lock().unwrap().values() {
            if e.status == AgentStatus::Running {
                e.cancel.cancel();
            }
        }
    }
}

/// `wait`: block until a background sub-agent finishes (or new input arrives, or
/// the deadline passes), then return a status line. Deliberately does NOT drain
/// — the result is delivered as a user message at the next round boundary, like
/// codex's wait (signal, don't carry). Interruptible by user steering (a push
/// to the inbox) and by the turn's cancel.
pub(super) async fn wait_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let timeout_ms = input["timeout_ms"]
        .as_u64()
        .unwrap_or(DEFAULT_WAIT_MS)
        .clamp(MIN_WAIT_MS, MAX_WAIT_MS);
    let inbox = &ctx.cfg.inbox;
    let agents = &ctx.cfg.async_agents;

    if agents.running_count() == 0 && inbox.is_empty() {
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
            agents.running_count(),
        ));
    }
    // Register interest BEFORE re-checking, so a completion that races in
    // between is not lost (notify_one leaves a permit).
    let notified = inbox.notified();
    tokio::pin!(notified);
    tokio::select! {
        _ = &mut notified => Ok(status_report(
            "A background task finished or new input arrived; it will be delivered on the next step.",
            agents.running_count(),
        )),
        _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => Ok(status_report(
            &format!("Timed out after {}s with no completion.", timeout_ms / 1000),
            agents.running_count(),
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
    match ctx.cfg.async_agents.request_stop(id) {
        Ok(task) => Ok(format!(
            "Stopping background task {id} ({task}). It will not report a result."
        )),
        Err(e) => bail!(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::InboxItem;
    use crate::tools::testutil::*;
    use serde_json::json;

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
        // wait signals but does NOT consume — the round boundary delivers it.
        assert!(!ctx.cfg.inbox.is_empty(), "wait must not drain the inbox");
    }

    #[test]
    fn register_enforces_the_concurrency_cap() {
        let reg = AsyncAgents::default();
        for i in 0..MAX_BACKGROUND_AGENTS {
            reg.register(&format!("agent-{i}"), "t", CancellationToken::new())
                .unwrap();
        }
        assert_eq!(reg.running_count(), MAX_BACKGROUND_AGENTS);
        let err = reg
            .register("agent-over", "t", CancellationToken::new())
            .unwrap_err();
        assert!(err.contains("too many background tasks"), "{err}");
    }

    #[test]
    fn finishing_frees_a_slot() {
        let reg = AsyncAgents::default();
        for i in 0..MAX_BACKGROUND_AGENTS {
            reg.register(&format!("agent-{i}"), "t", CancellationToken::new())
                .unwrap();
        }
        reg.set_status("agent-0", AgentStatus::Completed);
        assert_eq!(reg.running_count(), MAX_BACKGROUND_AGENTS - 1);
        // A slot opened up, so one more registers.
        reg.register("agent-new", "t", CancellationToken::new())
            .unwrap();
    }

    #[test]
    fn stop_cancels_a_running_agent_and_rejects_the_rest() {
        let reg = AsyncAgents::default();
        let cancel = CancellationToken::new();
        reg.register("agent-1", "build the thing", cancel.clone())
            .unwrap();
        let task = reg.request_stop("agent-1").unwrap();
        assert_eq!(task, "build the thing");
        assert!(cancel.is_cancelled());
        // Unknown id and non-running both error.
        assert!(reg
            .request_stop("agent-99")
            .unwrap_err()
            .contains("no background"));
        reg.set_status("agent-1", AgentStatus::Aborted);
        assert!(reg
            .request_stop("agent-1")
            .unwrap_err()
            .contains("not running"));
    }

    #[test]
    fn drop_cancels_running_agents() {
        let cancel = CancellationToken::new();
        {
            let reg = AsyncAgents::default();
            reg.register("agent-1", "t", cancel.clone()).unwrap();
        }
        assert!(
            cancel.is_cancelled(),
            "session drop reaps the detached task"
        );
    }
}
