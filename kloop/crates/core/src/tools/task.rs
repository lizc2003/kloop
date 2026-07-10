use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use serde_json::Value;

use super::str_arg;
use super::ToolCtx;
use crate::agent::run_turn;
use crate::agent::EndReason;
use crate::config::Config;
use crate::history::History;
use kloop_protocol::Message;

const SUBAGENT_MAX_ROUNDS: usize = 15;

pub(super) async fn task_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    if ctx.depth >= 1 {
        bail!("task: sub-agents cannot spawn further sub-agents");
    }
    let prompt = str_arg(input, "prompt", "task")?.to_string();
    let max_rounds = input["max_rounds"]
        .as_u64()
        .map_or(SUBAGENT_MAX_ROUNDS, |n| {
            (n as usize).clamp(1, SUBAGENT_MAX_ROUNDS)
        });
    let sub_cfg = Arc::new(Config {
        max_rounds,
        ..(*ctx.cfg).clone()
    });
    let ui = ctx.ui.clone();
    let cancel = ctx.cancel.clone();
    let depth = ctx.depth + 1;
    // The sub-agent runs as its own tokio task. Besides matching the
    // semantics, this breaks the recursion cycle (execute_tool -> run_turn ->
    // dispatch_tools -> execute_tool): task_tool only holds a JoinHandle,
    // which is Send regardless of the recursive future's type.
    let handle = tokio::spawn(async move {
        let mut history = History::new(sub_cfg.offload_dir.clone());
        history.record(Message::user_text(prompt));
        run_turn(&sub_cfg, &mut history, &ui, &cancel, depth).await
    });
    let outcome = handle
        .await
        .map_err(|e| anyhow!("task: sub-agent panicked: {e}"))?;
    match outcome.reason {
        EndReason::Completed => Ok(outcome.final_text),
        EndReason::MaxRounds => Ok(format!(
            "[sub-agent stopped at its round limit]\n{}",
            outcome.final_text
        )),
        EndReason::Aborted => Err(anyhow!("task: sub-agent interrupted")),
        EndReason::Error(e) => Err(anyhow!("task: sub-agent failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use crate::tools::testutil::*;
    use serde_json::json;

    #[tokio::test]
    async fn task_is_refused_at_depth_one() {
        let ctx = test_ctx(1, "depth");
        let (out, is_error) = run_tool("task", json!({"prompt": "recurse"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot spawn"));
    }
}
