use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::background_tasks::TaskStatus;
use super::str_arg;
use super::ToolCtx;
use crate::agent::run_turn;
use crate::agent::EndReason;
use crate::agent::TurnOutcome;
use crate::agents::AgentType;
use crate::config::Config;
use crate::history::History;
use crate::inbox::Inbox;
use crate::inbox::InboxItem;
use crate::rollout::session_path;
use crate::rollout::Rollout;
use crate::skills::Skill;
use kloop_protocol::Message;

const SUBAGENT_MAX_ROUNDS: usize = 15;

/// Cap on a background sub-agent's reinjected error text (~900 tokens, codex's
/// error-branch limit). A successful result is passed through verbatim; only a
/// failure is truncated, since its noise shouldn't crowd the parent's context.
const MAX_REINJECT_ERROR_CHARS: usize = 3600;

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
    let background = input["background"].as_bool().unwrap_or(false);
    // A custom agent type overrides the sub-agent's system prompt, model and
    // tool set; an unknown name is an is_error result naming the available
    // types. Omitting agent_type keeps the general-purpose inherit-everything
    // sub-agent.
    let agent_type = match input["agent_type"].as_str() {
        Some(name) => {
            Some(AgentType::lookup(&ctx.cfg.agent_types, name).map_err(|e| anyhow!("task: {e}"))?)
        }
        None => None,
    };
    let agent = next_agent_label();
    let sub_cfg = build_sub_config(ctx, max_rounds, agent.clone(), agent_type);
    let ui = ctx.ui.clone();
    let depth = ctx.depth + 1;
    // The label shown next to the running agent carries its type, if any.
    let preview = match agent_type {
        Some(at) => format!("[{}] {}", at.name, task_preview(&prompt)),
        None => task_preview(&prompt),
    };

    if background {
        return spawn_background(ctx, sub_cfg, agent, &preview, prompt, depth, ui);
    }

    run_sub_agent_sync(ctx, sub_cfg, agent, preview, prompt, depth, "task").await
}

/// Process-global monotonic agent label, so parallel spawners never collide.
fn next_agent_label() -> String {
    format!("agent-{}", AGENT_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Run a sub-agent synchronously and map its outcome to a tool result. The
/// sub-agent runs as its OWN tokio task — besides matching the semantics, this
/// breaks the recursion cycle (execute_tool -> run_turn -> dispatch_tools ->
/// execute_tool): the caller only holds a JoinHandle, which is Send regardless
/// of the recursive future's type. Shared by the `task` tool and a `fork`
/// skill; `who` prefixes the error messages.
async fn run_sub_agent_sync(
    ctx: &ToolCtx,
    sub_cfg: Arc<Config>,
    agent: String,
    preview: String,
    prompt: String,
    depth: u8,
    who: &str,
) -> Result<String> {
    let ui = ctx.ui.clone();
    let cancel = ctx.cancel.clone();
    let subagent_of = ctx.parent_rollout_id.clone();
    ui.agent_start(&agent, &preview);
    let handle = tokio::spawn({
        let ui = ui.clone();
        let label = agent.clone();
        async move {
            let mut history = sub_history(&sub_cfg, &label, subagent_of.as_deref());
            history.record(Message::user_text(prompt));
            run_turn(&sub_cfg, &mut history, &ui, &cancel, depth).await
        }
    });
    let outcome = match handle.await {
        Ok(outcome) => outcome,
        Err(e) => {
            ui.agent_end(&agent, false);
            return Err(anyhow!("{who}: sub-agent panicked: {e}"));
        }
    };
    let result = match outcome.reason {
        EndReason::Completed => Ok(outcome.final_text),
        EndReason::MaxRounds => Ok(format!(
            "[sub-agent stopped at its round limit]\n{}",
            outcome.final_text
        )),
        EndReason::Aborted => Err(anyhow!("{who}: sub-agent interrupted")),
        EndReason::Error(e) => Err(anyhow!("{who}: sub-agent failed: {e}")),
    };
    ui.agent_end(&agent, result.is_ok());
    result
}

/// Run a `context: fork` skill (plan 28 slice 2) as an isolated sub-agent: the
/// expanded body is the sub-agent's task, an optional `model` overrides its
/// model, and only the final result returns — the skill's intermediate work
/// stays out of the delegating model's context. A sub-agent cannot spawn one
/// (depth ≥ 1), so it there degrades to inline (returns the body), matching the
/// `task` depth rule without dead-ending the skill.
pub(crate) async fn fork_skill(ctx: &ToolCtx, skill: &Skill, body: String) -> Result<String> {
    if ctx.depth >= 1 {
        return Ok(body);
    }
    let agent = next_agent_label();
    let mut sub = clone_for_subagent(ctx, SUBAGENT_MAX_ROUNDS, agent.clone());
    if let Some(model) = &skill.model {
        sub.model = model.clone();
    }
    let preview = format!("[skill:{}] {}", skill.name, task_preview(&body));
    run_sub_agent_sync(
        ctx,
        Arc::new(sub),
        agent,
        preview,
        body,
        ctx.depth + 1,
        "skill",
    )
    .await
}

/// Fire-and-forget spawn (plan 26): register the agent, launch a DETACHED tokio
/// task, and return immediately. Unlike the synchronous path the sub-agent runs
/// on its OWN cancel token (registered for `stop_agent`) — a finished parent
/// turn must never kill a still-running background agent. When it ends, it
/// reinjects its result into the PARENT's inbox (captured before `build_sub_config`
/// reset the sub-agent's own inbox to fresh).
fn spawn_background(
    ctx: &ToolCtx,
    sub_cfg: Arc<Config>,
    agent: String,
    preview: &str,
    prompt: String,
    depth: u8,
    ui: Arc<dyn crate::agent::Ui>,
) -> Result<String> {
    let own_cancel = CancellationToken::new();
    ctx.cfg
        .background_tasks
        .register(&agent, preview, own_cancel.clone())
        .map_err(|msg| anyhow!("task: {msg}"))?;
    let parent_inbox = ctx.cfg.inbox.clone();
    let background_tasks = ctx.cfg.background_tasks.clone();
    let subagent_of = ctx.parent_rollout_id.clone();
    let session_note = child_session_note(&sub_cfg, &agent, subagent_of.as_deref());
    ui.agent_start(&agent, preview);
    tokio::spawn({
        let ui = ui.clone();
        let label = agent.clone();
        async move {
            let mut history = sub_history(&sub_cfg, &label, subagent_of.as_deref());
            history.record(Message::user_text(prompt));
            let outcome = run_turn(&sub_cfg, &mut history, &ui, &own_cancel, depth).await;
            let (status, reinject) = classify_background(outcome);
            background_tasks.set_status(&label, status);
            match reinject {
                Some(summary) => parent_inbox.push(InboxItem::SubAgentResult {
                    label: label.clone(),
                    summary,
                }),
                // Terminal with no reinjection (interrupted): still wake a
                // blocked `wait` so it re-evaluates instead of blocking out its
                // full deadline.
                None => parent_inbox.notify_activity(),
            }
            ui.agent_end(
                &label,
                matches!(status, TaskStatus::Completed | TaskStatus::MaxRounds),
            );
        }
    });
    Ok(format!(
        "Sub-agent {agent} started in the background.{session_note} Keep working; its result will \
         be delivered to you as a message when it finishes. Block for it with the wait tool, or \
         stop it with stop_agent."
    ))
}

/// Build the sub-agent's History, persisting to its own session file when the
/// parent runs in a persistent session (plan 17 slice 3). The child file is
/// `{parent session_id}-{agent label}` under the shared sessions dir, and its
/// first line records `subagent_of` = the parent turn that spawned it, so the
/// transcript is auditable and separately resumable, yet kept out of the
/// default resume picker. A parent with no session (mock, tests) or with a
/// dropped rollout leaves the sub-agent in-memory, exactly as before.
fn sub_history(cfg: &Config, agent: &str, subagent_of: Option<&str>) -> History {
    let mut history = History::new(cfg.offload_dir.clone());
    if let Some(parent_line) = subagent_of {
        if !cfg.session_id.is_empty() {
            let path = session_path(&cfg.sessions_dir, &child_session_id(cfg, agent));
            history.attach_rollout(Rollout::new_subagent(path, parent_line.to_string()));
        }
    }
    history
}

/// The sub-agent's session id: parent id + its label, so the file name itself
/// shows the lineage and stays unique (parent id is unique, the label is
/// process-global monotonic).
fn child_session_id(cfg: &Config, agent: &str) -> String {
    format!("{}-{}", cfg.session_id, agent)
}

/// A pointer to the child's session log for the parent's tool_result — so a
/// human auditing the parent session can jump to what the sub-agent did.
/// Empty when the sub-agent isn't being persisted (mock, tests).
fn child_session_note(cfg: &Config, agent: &str, subagent_of: Option<&str>) -> String {
    if subagent_of.is_some() && !cfg.session_id.is_empty() {
        format!(" Its session log is {}.", child_session_id(cfg, agent))
    } else {
        String::new()
    }
}

/// Map a background sub-agent's terminal outcome to (registry status, optional
/// reinjection). Success/round-limit pass through verbatim (codex); a failure
/// is truncated; an interrupted agent reinjects nothing (codex's is_final —
/// its partial output is noise, and the model that stopped it already knows).
fn classify_background(outcome: TurnOutcome) -> (TaskStatus, Option<String>) {
    match outcome.reason {
        EndReason::Completed => (TaskStatus::Completed, Some(outcome.final_text)),
        EndReason::MaxRounds => (
            TaskStatus::MaxRounds,
            Some(format!(
                "[sub-agent stopped at its round limit]\n{}",
                outcome.final_text
            )),
        ),
        EndReason::Error(e) => (
            TaskStatus::Failed,
            Some(format!(
                "[sub-agent failed] {}\nYou may re-dispatch it or try another approach.",
                truncate_error(&e)
            )),
        ),
        EndReason::Aborted => (TaskStatus::Aborted, None),
    }
}

fn truncate_error(e: &str) -> String {
    if e.chars().count() <= MAX_REINJECT_ERROR_CHARS {
        return e.to_string();
    }
    let truncated: String = e.chars().take(MAX_REINJECT_ERROR_CHARS).collect();
    format!("{truncated}… (error truncated)")
}

/// Clone the parent's Config for a sub-agent, with a fresh todo list and inbox
/// (a running sub-agent must never drain the parent's steering, and its own
/// todo_write must not touch the parent's list — the Config clone would
/// otherwise share both Arcs). Callers layer their own overrides on top
/// (agent_type for `task`, model for a `fork` skill).
fn clone_for_subagent(ctx: &ToolCtx, max_rounds: usize, agent: String) -> Config {
    Config {
        max_rounds,
        agent_label: agent,
        todos: Arc::new(std::sync::Mutex::new(Vec::new())),
        inbox: Arc::new(Inbox::default()),
        ..(*ctx.cfg).clone()
    }
}

/// Build the `task` sub-agent's Config: the shared clone plus any agent_type
/// overrides (system prompt, model, tool allowlist).
fn build_sub_config(
    ctx: &ToolCtx,
    max_rounds: usize,
    agent: String,
    agent_type: Option<&AgentType>,
) -> Arc<Config> {
    let mut sub = clone_for_subagent(ctx, max_rounds, agent);
    if let Some(at) = agent_type {
        if let Some(system) = &at.system {
            sub.system = system.clone();
        }
        if let Some(model) = &at.model {
            sub.model = model.clone();
        }
        if let Some(tools) = &at.tools {
            sub.tool_allowlist = Some(Arc::new(tools.iter().cloned().collect()));
        }
    }
    Arc::new(sub)
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

    /// agent_type overrides route to the sub-agent's request: its system
    /// prompt, model, and tool set are all the type's, and the tool set is
    /// filtered to the allowlist (plus the always-on read_offloaded).
    #[tokio::test]
    async fn agent_type_routes_system_model_and_tools() {
        use kloop_provider::MockTurn;
        let (provider, seen) =
            Provider::mock_recording(vec![MockTurn::Blocks(vec![ContentBlock::Text {
                text: "researched".into(),
            }])]);
        let types = vec![AgentType {
            name: "researcher".into(),
            description: "searches".into(),
            system: Some("You are a research agent.".into()),
            model: Some("cheap-model".into()),
            tools: Some(vec!["grep".into(), "read_file".into()]),
        }];
        let base = with_provider(test_ctx(0, "atype"), provider);
        let mut cfg = (*base.cfg).clone();
        cfg.agent_types = Arc::new(types);
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (out, is_error) = run_tool(
            "task",
            json!({"prompt": "find X", "agent_type": "researcher"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "researched");

        let reqs = seen.lock().unwrap();
        assert_eq!(reqs.len(), 1, "only the sub-agent sampled");
        assert_eq!(reqs[0].model, "cheap-model");
        assert_eq!(reqs[0].system, "You are a research agent.");
        let names: Vec<&str> = reqs[0].tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"grep") && names.contains(&"read_file"));
        assert!(
            names.contains(&"read_offloaded"),
            "infra tool kept: {names:?}"
        );
        assert!(
            !names.contains(&"bash"),
            "non-whitelisted filtered: {names:?}"
        );
        assert!(!names.contains(&"write_file"));
    }

    #[tokio::test]
    async fn unknown_agent_type_errors_with_available_list() {
        let types = vec![AgentType {
            name: "researcher".into(),
            description: "searches".into(),
            system: None,
            model: None,
            tools: None,
        }];
        let base = test_ctx(0, "atype-bad");
        let mut cfg = (*base.cfg).clone();
        cfg.agent_types = Arc::new(types);
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };
        let (out, is_error) =
            run_tool("task", json!({"prompt": "x", "agent_type": "ghost"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("unknown agent_type 'ghost'"), "{out}");
        assert!(out.contains("researcher"), "lists what's available: {out}");
    }

    /// A sub-agent's todo_write writes to its own fresh list, never the
    /// parent's — the Config clone would otherwise share the Arc.
    #[tokio::test]
    async fn subagent_todos_are_isolated_from_the_parent() {
        use crate::tools::TodoItem;
        use crate::tools::TodoStatus;

        let provider = Provider::mock(vec![
            // sub-agent round 1: rewrite its (empty) todo list
            vec![ContentBlock::ToolUse {
                id: "s1".into(),
                name: "todo_write".into(),
                input: json!({"todos": [
                    {"content": "sub task", "activeForm": "doing sub task", "status": "in_progress"}
                ]}),
            }],
            // sub-agent round 2: wrap up
            vec![ContentBlock::Text {
                text: "sub done".into(),
            }],
        ]);
        let ctx = with_provider(test_ctx(0, "todo-isolation"), provider);
        // The parent already has a task list of its own.
        *ctx.cfg.todos.lock().unwrap() = vec![TodoItem {
            content: "parent task".into(),
            active_form: "doing parent task".into(),
            status: TodoStatus::Pending,
        }];

        let results = dispatch_tools(
            vec![("t1".into(), "task".into(), json!({"prompt": "go"}))],
            &ctx,
        )
        .await;
        assert_eq!(
            results[0],
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "sub done".into(),
                is_error: false,
            }
        );

        // The parent's list is untouched by the sub-agent's todo_write.
        let parent = ctx.cfg.todos.lock().unwrap();
        assert_eq!(parent.len(), 1);
        assert_eq!(parent[0].content, "parent task");
        assert_eq!(parent[0].status, TodoStatus::Pending);
    }

    /// Fire-and-forget: task {background:true} returns a "started" message
    /// immediately (NOT the result), and the detached sub-agent reinjects its
    /// final text into the PARENT's inbox as a framed SubAgentResult when done.
    #[tokio::test]
    async fn background_task_returns_immediately_and_reinjects() {
        let provider = Provider::mock(vec![vec![ContentBlock::Text {
            text: "sub result".into(),
        }]]);
        let ctx = with_provider(test_ctx(0, "bg-reinject"), provider);

        let (out, is_error) = run_tool(
            "task",
            json!({"prompt": "go do it", "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("started in the background"), "{out}");
        assert!(
            !out.contains("sub result"),
            "the result is NOT returned inline: {out}"
        );

        // The detached sub-agent finishes and reinjects into the parent inbox.
        for _ in 0..300 {
            if !ctx.cfg.inbox.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let items = ctx.cfg.inbox.drain();
        assert_eq!(items.len(), 1, "one reinjected result");
        match &items[0] {
            InboxItem::SubAgentResult { label, summary } => {
                assert!(label.starts_with("agent-"), "{label}");
                assert_eq!(summary, "sub result");
            }
            other => panic!("expected SubAgentResult, got {other:?}"),
        }
        assert_eq!(ctx.cfg.background_tasks.running_count(), 0, "slot freed");
    }

    /// A background sub-agent cancelled via stop_agent ends Aborted and
    /// reinjects NOTHING (codex's is_final) — only a wake so a blocked wait
    /// re-evaluates.
    #[tokio::test]
    async fn stopped_background_task_does_not_reinject() {
        // Sub-agent blocks on a long bash so stop_agent can catch it running.
        let provider = Provider::mock(vec![vec![ContentBlock::ToolUse {
            id: "s1".into(),
            name: "bash".into(),
            input: json!({"command": "sleep 30"}),
        }]]);
        let ctx = with_provider(test_ctx(0, "bg-stopped"), provider);

        let (out, _) = run_tool("task", json!({"prompt": "long", "background": true}), &ctx).await;
        let agent = out
            .split_whitespace()
            .find(|w| w.starts_with("agent-"))
            .unwrap()
            .to_string();
        // Let the sub-agent get into its bash before stopping it.
        for _ in 0..100 {
            if ctx.cfg.background_tasks.running_count() == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (stop_out, is_error) = run_tool("stop_agent", json!({"agent_id": agent}), &ctx).await;
        assert!(!is_error, "{stop_out}");
        assert!(stop_out.contains("Stopping"), "{stop_out}");

        // Wait for it to actually wind down, then assert nothing was reinjected.
        for _ in 0..300 {
            if ctx.cfg.background_tasks.running_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            ctx.cfg.inbox.is_empty(),
            "an interrupted sub-agent reinjects nothing"
        );
    }

    /// A sub-agent spawned by a PERSISTENT parent writes its own session file:
    /// named `{parent id}-{agent-N}`, first line stamped `subagent_of` = the
    /// parent turn that spawned it, classified as a sub-agent (so it stays out
    /// of the resume picker), and replaying to the sub-agent's own transcript.
    #[tokio::test]
    async fn subagent_persists_to_its_own_session_file() {
        use crate::rollout::{
            is_subagent_session, load_session, session_id_of, session_origin, sessions_by_recency,
            SessionOrigin,
        };

        let root = std::env::temp_dir().join(format!("kloop-subpersist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions = root.join("sessions");

        let provider = Provider::mock(vec![vec![ContentBlock::Text {
            text: "sub result".into(),
        }]]);
        let base = with_provider(test_ctx(0, "subpersist"), provider);
        let mut cfg = (*base.cfg).clone();
        cfg.session_id = "20260714-000000".into();
        cfg.sessions_dir = sessions.clone();
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            parent_rollout_id: Some("20260714-000000#2".into()),
            ..base
        };

        let (out, is_error) = run_tool("task", json!({"prompt": "do the sub thing"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "sub result");

        let files = sessions_by_recency(&sessions);
        assert_eq!(files.len(), 1, "the sub-agent left one session file");
        let path = &files[0];
        assert!(
            session_id_of(path).starts_with("20260714-000000-agent-"),
            "child id shows lineage: {path:?}"
        );
        assert_eq!(
            session_origin(path),
            Some(SessionOrigin::SubAgent("20260714-000000#2".into())),
            "first line points back at the spawning parent turn"
        );
        assert!(is_subagent_session(path), "kept out of the resume picker");
        assert_eq!(
            load_session(path).unwrap(),
            vec![
                Message::user_text("do the sub thing"),
                Message::assistant(vec![ContentBlock::Text {
                    text: "sub result".into()
                }]),
            ],
            "replays to the sub-agent's own transcript"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A persistent background spawn names the child's session log in the
    /// "started" message (so a human auditing the parent can jump to it) and
    /// the detached sub-agent's file lands on disk.
    #[tokio::test]
    async fn background_task_notes_child_session_and_persists() {
        use crate::rollout::{is_subagent_session, sessions_by_recency};

        let root = std::env::temp_dir().join(format!("kloop-bgpersist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions = root.join("sessions");

        let provider = Provider::mock(vec![vec![ContentBlock::Text {
            text: "bg result".into(),
        }]]);
        let base = with_provider(test_ctx(0, "bgpersist"), provider);
        let mut cfg = (*base.cfg).clone();
        cfg.session_id = "20260714-111111".into();
        cfg.sessions_dir = sessions.clone();
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            parent_rollout_id: Some("20260714-111111#2".into()),
            ..base
        };

        let (out, is_error) =
            run_tool("task", json!({"prompt": "go", "background": true}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(
            out.contains("Its session log is 20260714-111111-agent-"),
            "the started message points at the child session log: {out}"
        );

        // Wait for the detached sub-agent to finish and flush its file.
        for _ in 0..300 {
            if ctx.cfg.background_tasks.running_count() == 0
                && !sessions_by_recency(&sessions).is_empty()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let files = sessions_by_recency(&sessions);
        assert_eq!(
            files.len(),
            1,
            "the background sub-agent persisted its file"
        );
        assert!(is_subagent_session(&files[0]));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without a persistent parent (empty session_id, as in mock/tests) a
    /// background spawn names no session log and writes nothing.
    #[tokio::test]
    async fn background_task_without_session_notes_nothing() {
        let provider = Provider::mock(vec![vec![ContentBlock::Text { text: "x".into() }]]);
        let ctx = with_provider(test_ctx(0, "bg-nosession"), provider);
        let (out, _) = run_tool("task", json!({"prompt": "go", "background": true}), &ctx).await;
        assert!(
            !out.contains("session log"),
            "an ephemeral parent has no child session log to name: {out}"
        );
    }

    #[test]
    fn classify_background_maps_outcomes() {
        let outcome = |reason| TurnOutcome {
            reason,
            final_text: "the answer".into(),
            rounds: 1,
        };
        // Success passes through verbatim.
        assert_eq!(
            classify_background(outcome(EndReason::Completed)),
            (TaskStatus::Completed, Some("the answer".into()))
        );
        // Round limit is framed but still carries the text.
        let (status, msg) = classify_background(outcome(EndReason::MaxRounds));
        assert_eq!(status, TaskStatus::MaxRounds);
        assert!(msg.unwrap().contains("the answer"));
        // A failure is framed with re-dispatch guidance.
        let (status, msg) = classify_background(outcome(EndReason::Error("boom".into())));
        assert_eq!(status, TaskStatus::Failed);
        assert!(msg.unwrap().contains("boom"));
        // Interrupted reinjects nothing.
        assert_eq!(
            classify_background(outcome(EndReason::Aborted)),
            (TaskStatus::Aborted, None)
        );
    }

    #[test]
    fn truncate_error_caps_only_long_failures() {
        assert_eq!(truncate_error("short"), "short");
        let long = "x".repeat(MAX_REINJECT_ERROR_CHARS + 500);
        let out = truncate_error(&long);
        assert!(out.ends_with("… (error truncated)"));
        assert!(out.chars().count() < long.chars().count());
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
