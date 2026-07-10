use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
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

/// Process-global so parallel task calls (and any future spawner) never hand
/// out the same label — same reasoning as the offload counter (lesson 2).
static AGENT_SEQ: AtomicUsize = AtomicUsize::new(1);

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
    let agent = format!("agent-{}", AGENT_SEQ.fetch_add(1, Ordering::Relaxed));
    let sub_cfg = Arc::new(Config {
        max_rounds,
        agent_label: agent.clone(),
        ..(*ctx.cfg).clone()
    });
    let ui = ctx.ui.clone();
    let cancel = ctx.cancel.clone();
    let depth = ctx.depth + 1;
    ui.agent_start(&agent, &task_preview(&prompt));
    // The sub-agent runs as its own tokio task. Besides matching the
    // semantics, this breaks the recursion cycle (execute_tool -> run_turn ->
    // dispatch_tools -> execute_tool): task_tool only holds a JoinHandle,
    // which is Send regardless of the recursive future's type.
    let handle = tokio::spawn({
        let ui = ui.clone();
        async move {
            let mut history = History::new(sub_cfg.offload_dir.clone());
            history.record(Message::user_text(prompt));
            run_turn(&sub_cfg, &mut history, &ui, &cancel, depth).await
        }
    });
    let outcome = match handle.await {
        Ok(outcome) => outcome,
        Err(e) => {
            ui.agent_end(&agent, false);
            return Err(anyhow!("task: sub-agent panicked: {e}"));
        }
    };
    let result = match outcome.reason {
        EndReason::Completed => Ok(outcome.final_text),
        EndReason::MaxRounds => Ok(format!(
            "[sub-agent stopped at its round limit]\n{}",
            outcome.final_text
        )),
        EndReason::Aborted => Err(anyhow!("task: sub-agent interrupted")),
        EndReason::Error(e) => Err(anyhow!("task: sub-agent failed: {e}")),
    };
    ui.agent_end(&agent, result.is_ok());
    result
}

/// First line of the prompt, truncated — the label a UI shows next to the
/// agent while it runs.
fn task_preview(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or("");
    let mut preview: String = line.chars().take(80).collect();
    if preview.len() < line.len() {
        preview.push('…');
    }
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Ui;
    use crate::tools::dispatch_tools;
    use crate::tools::testutil::*;
    use kloop_protocol::ContentBlock;
    use kloop_provider::Provider;
    use serde_json::json;

    #[tokio::test]
    async fn task_is_refused_at_depth_one() {
        let ctx = test_ctx(1, "depth");
        let (out, is_error) = run_tool("task", json!({"prompt": "recurse"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot spawn"));
    }

    /// Records the sub-agent lifecycle notifications.
    struct RecUi(std::sync::Mutex<Vec<String>>);
    impl Ui for RecUi {
        fn text_delta(&self, _: &str) {}
        fn note(&self, _: &str) {}
        fn agent_start(&self, agent: &str, task: &str) {
            self.0.lock().unwrap().push(format!("start {agent} {task}"));
        }
        fn agent_end(&self, agent: &str, ok: bool) {
            self.0.lock().unwrap().push(format!("end {agent} {ok}"));
        }
    }

    /// Two task calls in one batch really run in parallel: each sub-agent's
    /// bash waits for a file the OTHER sub-agent creates, so finishing fast
    /// at all proves concurrency (serial execution takes the full 3s poll).
    /// Each start gets a matching successful end with its own label.
    #[tokio::test]
    async fn consecutive_tasks_run_as_parallel_subagents() {
        let dir = std::env::temp_dir().join(format!("kloop-partask-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let barrier = |create: &str, wait: &str| {
            format!(
                "touch {dir}/{create}; for i in $(seq 60); do [ -f {dir}/{wait} ] && exit 0; sleep 0.05; done; exit 1",
                dir = dir.display()
            )
        };
        let tool_use = |id: &str, cmd: String| ContentBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: json!({"command": cmd}),
        };
        let done = vec![ContentBlock::Text {
            text: "sub done".into(),
        }];
        // Whichever sub-agent samples first gets the A-side; the pair is
        // symmetric so the race doesn't matter. Both finals are identical
        // because their assignment races too.
        let provider = Provider::mock(vec![
            vec![tool_use("s1", barrier("A", "B"))],
            vec![tool_use("s2", barrier("B", "A"))],
            done.clone(),
            done,
        ]);
        let mut ctx = with_provider(test_ctx(0, "partask"), provider);
        let rec = std::sync::Arc::new(RecUi(std::sync::Mutex::new(Vec::new())));
        ctx.ui = rec.clone();

        let started = std::time::Instant::now();
        let results = dispatch_tools(
            vec![
                ("t1".into(), "task".into(), json!({"prompt": "one"})),
                ("t2".into(), "task".into(), json!({"prompt": "two"})),
            ],
            &ctx,
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(
            results,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "sub done".into(),
                    is_error: false,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "sub done".into(),
                    is_error: false,
                },
            ]
        );
        assert!(
            elapsed < std::time::Duration::from_millis(2500),
            "the barrier only resolves when both sub-agents run at once; serial \
             execution polls out the full 3s (took {elapsed:?})"
        );
        let events = rec.0.lock().unwrap().clone();
        let starts: Vec<&String> = events.iter().filter(|e| e.starts_with("start ")).collect();
        let ends: Vec<&String> = events.iter().filter(|e| e.starts_with("end ")).collect();
        assert_eq!(starts.len(), 2);
        assert_ne!(starts[0], starts[1], "each task gets its own label");
        for start in &starts {
            let label = start.split_whitespace().nth(1).unwrap();
            assert!(label.starts_with("agent-"), "got {start}");
            assert!(
                ends.iter().any(|e| *e == &format!("end {label} true")),
                "no successful end for {label}: {events:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// One bad call in a task batch fails alone: results stay paired to their
    /// tool_use ids in request order and the healthy sibling completes.
    #[tokio::test]
    async fn failing_task_does_not_sink_the_batch() {
        let provider = Provider::mock(vec![vec![ContentBlock::Text {
            text: "solo done".into(),
        }]]);
        let ctx = with_provider(test_ctx(0, "taskfail"), provider);
        let results = dispatch_tools(
            vec![
                ("t1".into(), "task".into(), json!({"prompt": "solo"})),
                ("t2".into(), "task".into(), json!({})), // missing prompt
            ],
            &ctx,
        )
        .await;
        assert_eq!(
            results[0],
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "solo done".into(),
                is_error: false,
            }
        );
        let ContentBlock::ToolResult {
            tool_use_id,
            content,
            is_error,
        } = &results[1]
        else {
            panic!("expected tool result");
        };
        assert_eq!(tool_use_id, "t2");
        assert!(is_error);
        assert!(content.contains("missing required string argument 'prompt'"));
    }

    #[test]
    fn task_preview_takes_first_line_truncated() {
        assert_eq!(task_preview("fix the bug\nthen test"), "fix the bug");
        assert_eq!(task_preview(""), "");
        let long = "x".repeat(100);
        let preview = task_preview(&long);
        assert_eq!(preview.chars().count(), 81);
        assert!(preview.ends_with('…'));
    }
}
