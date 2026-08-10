use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::background_executions::ExecutionKind;
use super::background_executions::ExecutionStatus;
use super::str_arg;
use super::ToolCtx;
use crate::agent::run_structured_turn;
use crate::agent::run_turn;
use crate::agent::EndReason;
use crate::agent::TurnOutcome;
use crate::agent_type::AgentType;
use crate::config::Config;
use crate::config::EffectiveWorkspace;
use crate::event::BackgroundTask;
use crate::event::BackgroundTaskKind;
use crate::event::BackgroundTaskStatus;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use crate::history::History;
use crate::inbox::InboxItem;
use crate::rollout::session_path;
use crate::rollout::Rollout;
use crate::skills::Skill;
use crate::worktree;
use kloop_protocol::Message;

/// Cap on a background sub-agent's reinjected error text (~900 tokens, codex's
/// error-branch limit). A successful result is passed through verbatim; only a
/// failure is truncated, since its noise shouldn't crowd the parent's context.
const MAX_REINJECT_ERROR_CHARS: usize = 3600;

/// Process-global so parallel run_agent calls (and any future spawner) never hand
/// out the same label — same reasoning as the offload counter (lesson 2).
static AGENT_SEQ: AtomicUsize = AtomicUsize::new(1);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RunAgentInput {
    #[serde(default)]
    description: Option<String>,
    prompt: String,
    #[serde(default)]
    agent_type: Option<String>,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    max_rounds: Option<u64>,
    #[serde(default)]
    isolation: Option<String>,
}

pub(super) async fn run_agent_tool(
    input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    if ctx.depth >= 1 {
        bail!("run_agent: sub-agents cannot spawn further sub-agents");
    }
    let parsed: RunAgentInput =
        serde_json::from_value(input.clone()).context("run_agent: invalid input")?;
    let description = super::optional_display_description(input, "run_agent")?;
    let prompt = parsed.prompt;
    let _ = parsed.description;
    let max_rounds = match parsed.max_rounds {
        None => None,
        Some(n) => {
            if n == 0 {
                bail!("run_agent: max_rounds must be a positive integer");
            }
            Some(usize::try_from(n).unwrap_or(usize::MAX))
        }
    };
    let background = parsed.background;
    // A custom agent type overrides the sub-agent's system prompt, model and
    // tool set; an unknown name is an is_error result naming the available
    // types. Omitting agent_type keeps the general-purpose inherit-everything
    // sub-agent.
    let agent_type = match parsed.agent_type.as_deref() {
        Some(name) => Some(
            AgentType::lookup(&ctx.cfg.agent_types, name).map_err(|e| anyhow!("run_agent: {e}"))?,
        ),
        None => None,
    };
    // `isolation: "worktree"` gives the sub-agent its own git worktree so it
    // can edit files without racing sibling sub-agents on the shared tree
    // (plan 35). Any other value is an error — an unrecognized isolation must
    // not silently degrade to the shared cwd.
    let isolate = match parsed.isolation.as_deref() {
        None | Some("shared") => false,
        Some("worktree") => true,
        Some(other) => bail!("run_agent: unknown isolation '{other}' (expected \"worktree\")"),
    };
    let agent = next_agent_label();
    let agent_type_name = agent_type.map(|agent_type| agent_type.name.clone());
    let mut sub = build_sub_config(ctx, workspace, max_rounds, agent.clone(), agent_type);
    let ui = ctx.ui.clone();
    let depth = ctx.depth + 1;
    // A custom label improves human-facing lifecycle rows without changing the
    // task prompt. Preserve the configured agent type as a display decoration.
    let description = description.unwrap_or_else(|| agent_preview(&prompt));
    let preview = match agent_type {
        Some(at) => format!("[{}] {description}", at.name),
        None => description,
    };

    // Create the worktree BEFORE spawning and rewire the sub-agent's cwd
    // anchors onto it. Fail-closed: a creation error is the tool's error, never
    // a fall back to the shared cwd (cc's shape).
    let worktree = if isolate {
        let wt = worktree::create(&workspace.cwd, &agent)
            .await
            .map_err(|e| anyhow!("run_agent: {e:#}"))?;
        Some(bind_subagent_worktree(&mut sub, wt, "run_agent").await?)
    } else {
        None
    };
    let sub_cfg = Arc::new(sub);

    if background {
        return spawn_background(
            ctx,
            sub_cfg,
            agent,
            &preview,
            agent_type_name,
            prompt,
            depth,
            ui,
            worktree,
        )
        .await;
    }

    run_sub_agent_sync(
        ctx,
        sub_cfg,
        agent,
        preview,
        agent_type_name,
        prompt,
        depth,
        "run_agent",
        worktree,
    )
    .await
}

pub(super) async fn structured_agent(input: &Value, schema: Value, ctx: &ToolCtx) -> Result<Value> {
    if ctx.depth >= 1 {
        bail!("workflow agent: nested sub-agents are unavailable");
    }
    crate::structured_output::validate_schema(&schema)?;
    let prompt = str_arg(input, "prompt", "workflow agent")?.to_string();
    let max_rounds = match input.get("max_rounds") {
        None | Some(Value::Null) => None,
        Some(value) => {
            let rounds = value
                .as_u64()
                .filter(|rounds| *rounds > 0)
                .ok_or_else(|| anyhow!("workflow agent: max_rounds must be a positive integer"))?;
            Some(usize::try_from(rounds).unwrap_or(usize::MAX))
        }
    };
    let agent_type = match input["agent_type"].as_str() {
        Some(name) => Some(
            AgentType::lookup(&ctx.cfg.agent_types, name)
                .map_err(|error| anyhow!("workflow agent: {error}"))?,
        ),
        None => None,
    };
    let isolate = match input["isolation"].as_str() {
        None | Some("shared") => false,
        Some("worktree") => true,
        Some(other) => bail!("workflow agent: unknown isolation '{other}' (expected \"worktree\")"),
    };
    let agent = next_agent_label();
    let workspace = ctx.cfg.effective_workspace();
    let mut sub = build_sub_config(ctx, &workspace, max_rounds, agent.clone(), agent_type);
    if let Some(model) = input["model"].as_str() {
        sub.model = model.to_string();
    }
    let preview = match agent_type {
        Some(agent_type) => format!("[{}] {}", agent_type.name, agent_preview(&prompt)),
        None => agent_preview(&prompt),
    };
    let worktree = if isolate {
        let worktree = worktree::create(&workspace.cwd, &agent)
            .await
            .map_err(|error| anyhow!("workflow agent: {error:#}"))?;
        Some(bind_subagent_worktree(&mut sub, worktree, "workflow agent").await?)
    } else {
        None
    };
    let sub_cfg = Arc::new(sub);
    let ui = ctx.ui.clone();
    let cancel = ctx.cancel.clone();
    let depth = ctx.depth + 1;
    let subagent_of = ctx.parent_rollout_id.clone();
    let (lease, worktree) = register_child_with_cleanup(
        &sub_cfg,
        agent_type.map(|agent_type| agent_type.name.as_str()),
        &preview,
        &ui,
        &agent,
        "workflow agent",
        worktree,
    )
    .await?;
    emit_agent_start(&ui, &agent, &preview);
    let handle = tokio::spawn({
        let ui = ui.clone();
        let label = agent.clone();
        async move {
            let _lease = lease;
            let mut history = sub_history(&sub_cfg, &label, subagent_of.as_deref());
            history.record(Message::user_text(prompt));
            run_structured_turn(&sub_cfg, &mut history, &ui, &cancel, depth, schema).await
        }
    });
    let outcome = match handle.await {
        Ok(outcome) => outcome,
        Err(error) => {
            let cleanup_error = if let Some(worktree) = worktree {
                worktree::finish(worktree).await.err()
            } else {
                None
            };
            emit_agent_end(&ui, &agent, false);
            return match cleanup_error {
                Some(cleanup) => Err(anyhow!(
                    "workflow agent: sub-agent panicked: {error}; worktree cleanup failed: {cleanup:#}"
                )),
                None => Err(anyhow!("workflow agent: sub-agent panicked: {error}")),
            };
        }
    };
    let mut result = match outcome.reason {
        EndReason::Completed => outcome.structured_output.ok_or_else(|| {
            anyhow!("workflow agent: child completed without valid structured_output")
        }),
        EndReason::MaxRounds => Err(anyhow!(
            "workflow agent: child stopped at its round limit without valid structured_output"
        )),
        EndReason::Aborted => Err(anyhow!("workflow agent: child was interrupted")),
        EndReason::Error(error) => Err(anyhow!("workflow agent: child failed: {error}")),
    };
    if let Some(worktree) = worktree {
        match worktree::finish(worktree).await {
            Ok(Some(note)) => ui.emit(&Event::Note(note.trim().to_string())),
            Ok(None) => {}
            Err(error) => {
                result = Err(anyhow!(
                    "workflow agent: worktree cleanup failed: {error:#}"
                ));
            }
        }
    }
    emit_agent_end(&ui, &agent, result.is_ok());
    result
}

/// Point a sub-agent's cwd anchors at its worktree: cwd, the permission gate
/// (so acceptEdits allows writes inside the tree), the OS sandbox (so its bash
/// may write the tree), and the working-directory line the model reads in the
/// system prompt — without that last rewrite the model builds ABSOLUTE paths
/// from the parent's cwd and writes straight past the worktree (found the hard
/// way in the plan-35 real-key run). Everything else — offload/sessions dirs,
/// hooks, tool sources, the instruction files and the git snapshot — stays
/// inherited: a HEAD-based worktree has byte-identical instruction files, and
/// re-discovering them (plus a fresh worktree git snapshot) needs the CLI's IO
/// and is left for a later slice.
fn rewire_for_worktree(sub: &mut Config, wt: &worktree::Worktree) -> Result<()> {
    let (permissions, sandbox, system) = worktree::compute_overrides(
        &sub.cwd,
        &sub.permissions,
        &sub.sandbox,
        &sub.system,
        &wt.path,
    )?;
    sub.cwd = wt.path.clone();
    sub.permissions = permissions;
    sub.sandbox = sandbox;
    sub.system = system;
    Ok(())
}

async fn bind_subagent_worktree(
    sub: &mut Config,
    worktree: worktree::Worktree,
    who: &str,
) -> Result<worktree::Worktree> {
    if let Err(error) = rewire_for_worktree(sub, &worktree) {
        return match worktree::finish(worktree).await {
            Ok(_) => Err(anyhow!("{who}: {error:#}")),
            Err(cleanup) => Err(anyhow!(
                "{who}: {error:#}; worktree cleanup failed: {cleanup:#}"
            )),
        };
    }
    Ok(worktree)
}

async fn register_child_with_cleanup(
    sub_cfg: &Arc<Config>,
    agent_type: Option<&str>,
    preview: &str,
    ui: &Arc<dyn crate::agent::Ui>,
    agent: &str,
    who: &str,
    worktree: Option<worktree::Worktree>,
) -> Result<(
    crate::agent_mailbox::LiveAgentLease,
    Option<worktree::Worktree>,
)> {
    match sub_cfg.local_agent.register_child(
        Arc::clone(&sub_cfg.inbox),
        agent_type,
        preview,
        ui.clone(),
    ) {
        Ok(lease) => Ok((lease, worktree)),
        Err(error) => {
            let cleanup_error = match worktree {
                Some(worktree) => worktree::finish(worktree).await.err(),
                None => None,
            };
            match cleanup_error {
                Some(cleanup) => Err(anyhow!(
                    "{who}: cannot register {agent}: {error}; worktree cleanup failed: {cleanup:#}"
                )),
                None => Err(anyhow!("{who}: cannot register {agent}: {error}")),
            }
        }
    }
}

/// Process-global monotonic agent label, so parallel spawners never collide.
fn next_agent_label() -> String {
    format!("agent-{}", AGENT_SEQ.fetch_add(1, Ordering::Relaxed))
}

/// A sub-agent began working on a description; its item id is its label. Shared
/// with the code-mode program runner, whose background agent has the same lifecycle.
pub(super) fn emit_agent_start(ui: &Arc<dyn crate::agent::Ui>, label: &str, description: &str) {
    ui.emit(&Event::ItemStarted {
        id: label.to_string(),
        item: Item::SubAgent {
            label: label.to_string(),
            task: description.to_string(),
            status: ItemStatus::InProgress,
        },
    });
}

/// A sub-agent finished. The completed item drops the task text — no front-end
/// reads it at completion (a UI resolves the row it opened by label).
pub(super) fn emit_agent_end(ui: &Arc<dyn crate::agent::Ui>, label: &str, ok: bool) {
    ui.emit(&Event::ItemCompleted {
        id: label.to_string(),
        item: Item::SubAgent {
            label: label.to_string(),
            task: String::new(),
            status: if ok {
                ItemStatus::Completed
            } else {
                ItemStatus::Failed
            },
        },
    });
}

pub(super) fn emit_background_task(
    ui: &Arc<dyn crate::agent::Ui>,
    label: &str,
    run_id: Option<&str>,
    kind: BackgroundTaskKind,
    description: &str,
    status: ExecutionStatus,
    detail: Option<String>,
) {
    let status = match status {
        ExecutionStatus::Running => BackgroundTaskStatus::Running,
        ExecutionStatus::Completed | ExecutionStatus::MaxRounds => BackgroundTaskStatus::Completed,
        ExecutionStatus::Failed => BackgroundTaskStatus::Failed,
        ExecutionStatus::Aborted => BackgroundTaskStatus::Cancelled,
    };
    ui.emit(&Event::BackgroundTaskUpdated(BackgroundTask {
        id: label.to_string(),
        run_id: run_id.map(str::to_string),
        kind,
        description: description.to_string(),
        status,
        output_path: None,
        detail,
    }));
}

/// Run a sub-agent synchronously and map its outcome to a tool result. The
/// sub-agent runs as its OWN tokio task — besides matching the semantics, this
/// breaks the recursion cycle (execute_tool -> run_turn -> dispatch_tools ->
/// execute_tool): the caller only holds a JoinHandle, which is Send regardless
/// of the recursive future's type. Shared by the `run_agent` tool and a `fork`
/// skill; `who` prefixes the error messages.
#[allow(clippy::too_many_arguments)]
async fn run_sub_agent_sync(
    ctx: &ToolCtx,
    sub_cfg: Arc<Config>,
    agent: String,
    preview: String,
    agent_type: Option<String>,
    prompt: String,
    depth: u8,
    who: &str,
    worktree: Option<worktree::Worktree>,
) -> Result<String> {
    let ui = ctx.ui.clone();
    let cancel = ctx.cancel.clone();
    let subagent_of = ctx.parent_rollout_id.clone();
    let (lease, worktree) = register_child_with_cleanup(
        &sub_cfg,
        agent_type.as_deref(),
        &preview,
        &ui,
        &agent,
        who,
        worktree,
    )
    .await?;
    emit_agent_start(&ui, &agent, &preview);
    let handle = tokio::spawn({
        let ui = ui.clone();
        let label = agent.clone();
        async move {
            let _lease = lease;
            let mut history = sub_history(&sub_cfg, &label, subagent_of.as_deref());
            history.record(Message::user_text(prompt));
            run_turn(&sub_cfg, &mut history, &ui, &cancel, depth).await
        }
    });
    let outcome = match handle.await {
        Ok(outcome) => outcome,
        Err(e) => {
            let cleanup_error = if let Some(wt) = worktree {
                worktree::finish(wt).await.err()
            } else {
                None
            };
            emit_agent_end(&ui, &agent, false);
            return match cleanup_error {
                Some(cleanup) => Err(anyhow!(
                    "{who}: sub-agent panicked: {e}; worktree cleanup failed: {cleanup:#}"
                )),
                None => Err(anyhow!("{who}: sub-agent panicked: {e}")),
            };
        }
    };
    let mut result = match outcome.reason {
        EndReason::Completed => Ok(outcome.final_text),
        EndReason::MaxRounds => Ok(format!(
            "[sub-agent stopped at its round limit]\n{}",
            outcome.final_text
        )),
        EndReason::Aborted => Err(anyhow!("{who}: sub-agent interrupted")),
        EndReason::Error(e) => Err(anyhow!("{who}: sub-agent failed: {e}")),
    };
    // Tear down or preserve the worktree, and tell the model where a preserved
    // one lives (only on a success result — an error already routes to is_error
    // guidance; the changes still sit on the branch for the user).
    if let Some(wt) = worktree {
        match worktree::finish(wt).await {
            Ok(Some(note)) => {
                if let Ok(text) = &mut result {
                    text.push_str(&note);
                }
            }
            Ok(None) => {}
            Err(error) => {
                result = Err(anyhow!("{who}: worktree cleanup failed: {error:#}"));
            }
        }
    }
    emit_agent_end(&ui, &agent, result.is_ok());
    result
}

/// Run a `context: fork` skill (plan 28 slice 2) as an isolated sub-agent: the
/// expanded body is the sub-agent's task, an optional `model` overrides its
/// model, and only the final result returns — the skill's intermediate work
/// stays out of the delegating model's context. A sub-agent cannot spawn one
/// (depth ≥ 1), so it there degrades to inline (returns the body), matching the
/// `run_agent` depth rule without dead-ending the skill.
pub(crate) async fn fork_skill(
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    skill: &Skill,
    body: String,
) -> Result<String> {
    if ctx.depth >= 1 {
        return Ok(body);
    }
    let agent = next_agent_label();
    let mut sub = clone_for_subagent(ctx, workspace, None, agent.clone());
    if let Some(model) = &skill.model {
        sub.model = model.clone();
    }
    // `allowed-tools` restricts the sub-agent's tool set (like an agent_type's
    // tools) — a capability limit, not a permission grant; read_offloaded stays
    // available regardless (see `agent_type::tool_available`).
    if let Some(tools) = &skill.allowed_tools {
        sub.tool_allowlist = Some(Arc::new(tools.iter().cloned().collect()));
    }
    let preview = format!("[skill:{}] {}", skill.name, agent_preview(&body));
    run_sub_agent_sync(
        ctx,
        Arc::new(sub),
        agent,
        preview,
        None,
        body,
        ctx.depth + 1,
        "skill",
        None,
    )
    .await
}

/// Fire-and-forget spawn (plan 26): register the agent, launch a DETACHED tokio
/// worker, and return immediately. Unlike the synchronous path the sub-agent runs
/// on its OWN cancel token (registered for `stop_agent`) — a finished parent
/// turn must never kill a still-running background agent. When it ends, it
/// reinjects its result into the PARENT's inbox (captured before `build_sub_config`
/// reset the sub-agent's own inbox to fresh).
#[allow(clippy::too_many_arguments)]
async fn spawn_background(
    ctx: &ToolCtx,
    sub_cfg: Arc<Config>,
    agent: String,
    preview: &str,
    agent_type: Option<String>,
    prompt: String,
    depth: u8,
    ui: Arc<dyn crate::agent::Ui>,
    worktree: Option<worktree::Worktree>,
) -> Result<String> {
    let lease = match sub_cfg.local_agent.register_child(
        Arc::clone(&sub_cfg.inbox),
        agent_type.as_deref(),
        preview,
        ui.clone(),
    ) {
        Ok(lease) => lease,
        Err(error) => {
            if let Some(worktree) = worktree {
                worktree::finish(worktree).await.map_err(|cleanup| {
                    anyhow!(
                        "run_agent: cannot register {agent}: {error}; worktree cleanup failed: {cleanup:#}"
                    )
                })?;
            }
            return Err(anyhow!("run_agent: cannot register {agent}: {error}"));
        }
    };
    let own_cancel = CancellationToken::new();
    if let Err(msg) = ctx.cfg.background_executions.register(
        ExecutionKind::Agent,
        &agent,
        preview,
        own_cancel.clone(),
    ) {
        // The slot couldn't be reserved: nothing will run, so undo the worktree
        // now instead of leaking an empty tree.
        if let Some(wt) = worktree {
            worktree::finish(wt)
                .await
                .map_err(|error| anyhow!("run_agent: {msg}; worktree cleanup failed: {error:#}"))?;
        }
        return Err(anyhow!("run_agent: {msg}"));
    }
    let parent_inbox = ctx.cfg.inbox.clone();
    let background_executions = ctx.cfg.background_executions.clone();
    let subagent_of = ctx.parent_rollout_id.clone();
    let session_note = child_session_note(&sub_cfg, &agent, subagent_of.as_deref());
    let description = preview.to_string();
    emit_background_task(
        &ui,
        &agent,
        None,
        BackgroundTaskKind::Agent,
        &description,
        ExecutionStatus::Running,
        None,
    );

    // The worker owns only the model turn. A supervisor awaits its JoinHandle so
    // panic/forced abort still reaches worktree cleanup, one terminal registry
    // transition, one inbox publication, and one frontend event.
    let worker = tokio::spawn({
        let ui = ui.clone();
        let label = agent.clone();
        async move {
            let _lease = lease;
            let mut history = sub_history(&sub_cfg, &label, subagent_of.as_deref());
            history.record(Message::user_text(prompt));
            run_turn(&sub_cfg, &mut history, &ui, &own_cancel, depth).await
        }
    });
    background_executions.attach_abort(&agent, worker.abort_handle());
    tokio::spawn({
        let label = agent.clone();
        let ui = ui.clone();
        let description = description.clone();
        async move {
            let (mut status, mut reinject) = match worker.await {
                Ok(outcome) => classify_background(outcome),
                Err(error) if error.is_cancelled() => (ExecutionStatus::Aborted, None),
                Err(error) => (
                    ExecutionStatus::Failed,
                    Some(format!(
                        "[sub-agent failed] background worker panicked: {error}\nYou may re-dispatch it or try another approach."
                    )),
                ),
            };
            // Tear down or preserve the worktree. Natural completion folds the
            // location into the reinjected result; cancellation keeps it on the
            // session-scoped terminal event so the preserved tree is never hidden.
            let mut cleanup_detail = None;
            if let Some(wt) = worktree {
                match worktree::finish(wt).await {
                    Ok(Some(note)) => {
                        cleanup_detail = Some(note.trim().to_string());
                        match &mut reinject {
                            Some(summary) => summary.push_str(&note),
                            None => reinject = Some(note.trim_start().to_string()),
                        }
                    }
                    Ok(None) => {}
                    Err(error) => {
                        status = ExecutionStatus::Failed;
                        let note = format!("[worktree cleanup failed] {error:#}");
                        cleanup_detail = Some(note.clone());
                        match &mut reinject {
                            Some(summary) => {
                                summary.push('\n');
                                summary.push_str(&note);
                            }
                            None => reinject = Some(note),
                        }
                    }
                }
            }
            let terminal = background_executions.finish(&label, status, |actual, deliver| {
                if deliver {
                    if let Some(summary) = reinject {
                        parent_inbox.push(InboxItem::SubAgentResult {
                            label: label.clone(),
                            summary,
                        });
                    } else {
                        parent_inbox.notify_activity();
                    }
                } else {
                    debug_assert_eq!(actual, ExecutionStatus::Aborted);
                    parent_inbox.notify_activity();
                }
            });
            if let Some(terminal) = terminal {
                emit_background_task(
                    &ui,
                    &label,
                    None,
                    BackgroundTaskKind::Agent,
                    &description,
                    terminal,
                    background_terminal_detail(terminal, cleanup_detail),
                );
            }
        }
    });
    Ok(format!(
        "Agent({description}) started in the background.{session_note}\nAgent ID: {agent}\nKeep working; its result will be delivered automatically as a message when it finishes. Call wait_for_activity once only if you need to block for any activity, or stop it with stop_agent {{\"agent_id\": \"{agent}\"}}."
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
fn classify_background(outcome: TurnOutcome) -> (ExecutionStatus, Option<String>) {
    match outcome.reason {
        EndReason::Completed => (ExecutionStatus::Completed, Some(outcome.final_text)),
        EndReason::MaxRounds => (
            ExecutionStatus::MaxRounds,
            Some(format!(
                "[sub-agent stopped at its round limit]\n{}",
                outcome.final_text
            )),
        ),
        EndReason::Error(e) => (
            ExecutionStatus::Failed,
            Some(format!(
                "[sub-agent failed] {}\nYou may re-dispatch it or try another approach.",
                truncate_error(&e)
            )),
        ),
        EndReason::Aborted => (ExecutionStatus::Aborted, None),
    }
}

pub(super) fn execution_status_detail(status: ExecutionStatus) -> Option<String> {
    match status {
        ExecutionStatus::Running | ExecutionStatus::Completed => None,
        ExecutionStatus::Failed => Some("background agent failed".into()),
        ExecutionStatus::MaxRounds => Some("stopped at round limit".into()),
        ExecutionStatus::Aborted => Some("stopped".into()),
    }
}

fn background_terminal_detail(
    status: ExecutionStatus,
    cleanup_detail: Option<String>,
) -> Option<String> {
    let mut detail = execution_status_detail(status);
    if status == ExecutionStatus::Aborted {
        if let Some(cleanup) = cleanup_detail {
            match &mut detail {
                Some(text) => {
                    text.push_str(": ");
                    text.push_str(&cleanup);
                }
                None => detail = Some(cleanup),
            }
        }
    }
    detail
}

fn truncate_error(e: &str) -> String {
    if e.chars().count() <= MAX_REINJECT_ERROR_CHARS {
        return e.to_string();
    }
    let truncated: String = e.chars().take(MAX_REINJECT_ERROR_CHARS).collect();
    format!("{truncated}… (error truncated)")
}

/// Build a child through Config's lifecycle constructor, freezing the one
/// workspace generation selected by the parent call.
fn clone_for_subagent(
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    max_rounds: Option<usize>,
    agent: String,
) -> Config {
    ctx.cfg.subagent_from(
        workspace,
        max_rounds,
        agent.parse().expect("generated agent label is canonical"),
    )
}

/// Build the `run_agent` sub-agent's Config: the shared clone plus any agent_type
/// overrides (system prompt, model, tool allowlist). Returned unwrapped so the
/// caller can still rewire it (worktree isolation) before sharing the Arc.
fn build_sub_config(
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    max_rounds: Option<usize>,
    agent: String,
    agent_type: Option<&AgentType>,
) -> Config {
    let mut sub = clone_for_subagent(ctx, workspace, max_rounds, agent);
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
    sub
}

/// First line of the prompt, truncated — the label a UI shows next to the
/// agent while it runs.
fn agent_preview(prompt: &str) -> String {
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
    use kloop_protocol::AssistantBlock;
    use kloop_protocol::ContentBlock;
    use kloop_provider::Provider;
    use serde_json::json;

    #[test]
    fn subagent_file_observations_are_fresh() {
        use crate::file_state::FileObservation;
        use crate::file_state::FileStateUpdate;

        let path =
            std::env::temp_dir().join(format!("kloop-subagent-file-state-{}", std::process::id()));
        std::fs::write(&path, b"parent read\n").unwrap();
        let path = std::fs::canonicalize(path).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let ctx = test_ctx(0, "subagent-file-state");
        ctx.cfg.file_state.apply(FileStateUpdate::Replace {
            path: path.clone(),
            observation: FileObservation::full(b"parent read\n", &metadata),
        });

        let workspace = ctx.cfg.effective_workspace();
        let sub = clone_for_subagent(&ctx, &workspace, None, "agent-999".into());
        assert!(!Arc::ptr_eq(&ctx.cfg.file_state, &sub.file_state));
        assert!(Arc::ptr_eq(
            &ctx.cfg.powershell_execution_gate,
            &sub.powershell_execution_gate
        ));
        assert!(sub.file_state.observation(&path).is_none());
        assert!(ctx.cfg.file_state.observation(&path).is_some());
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn child_is_registered_before_sampling_and_can_list_main() {
        let (provider, seen) = Provider::mock_recording(vec![
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::ToolUse {
                id: "list".into(),
                name: "list_agents".into(),
                input: json!({}),
            }]),
            kloop_provider::MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "listed".into(),
            }]),
        ]);
        let ctx = with_provider(test_ctx(0, "child-roster"), provider);
        let (output, is_error) = run_tool(
            "run_agent",
            json!({"prompt":"list the live local Agents"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        assert_eq!(output, "listed");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(seen[1].messages.iter().any(|message| {
            message.content.iter().any(|block| {
                matches!(block, ContentBlock::ToolResult { content, is_error: false, .. }
                    if content.as_text().contains("\"id\":\"main\""))
            })
        }));
    }

    #[tokio::test]
    async fn run_agent_is_refused_at_depth_one() {
        let ctx = test_ctx(1, "depth");
        let (out, is_error) = run_tool("run_agent", json!({"prompt": "recurse"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("cannot spawn"));
    }

    #[tokio::test]
    async fn run_agent_rejects_non_positive_round_limit() {
        let ctx = test_ctx(0, "run-agent-zero-rounds");
        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "keep going", "max_rounds": 0}),
            &ctx,
        )
        .await;

        assert!(is_error);
        assert_eq!(out, "run_agent: max_rounds must be a positive integer");
    }

    /// Omitting max_rounds must not inherit the parent's guardrail or the old
    /// sub-agent default of 15: the child runs until it produces a final answer.
    #[tokio::test]
    async fn run_agent_without_round_limit_runs_until_completed() {
        let mut turns = (0..16)
            .map(|i| {
                vec![AssistantBlock::ToolUse {
                    id: format!("r{i}"),
                    name: "read_file".into(),
                    input: json!({"path": format!("missing-{i}")}),
                }]
            })
            .collect::<Vec<_>>();
        turns.push(vec![AssistantBlock::Text {
            text: "finished after sixteen tool rounds".into(),
        }]);
        let ctx = with_provider(test_ctx(0, "run-agent-unbounded"), Provider::mock(turns));

        let (out, is_error) = run_tool("run_agent", json!({"prompt": "keep going"}), &ctx).await;

        assert!(!is_error, "{out}");
        assert_eq!(out, "finished after sixteen tool rounds");
    }

    /// Point a ctx's cwd at `repo` so an isolated sub-agent branches from it
    /// (worktree mode off — sub-agent isolation doesn't use the enter/exit
    /// tools). `temp_git_repo` comes from testutil, shared with slice 2.
    fn ctx_in(ctx: ToolCtx, repo: &std::path::Path) -> ToolCtx {
        git_ctx(ctx, repo, false)
    }

    /// A worktree sub-agent's relative write lands in its OWN tree, never the
    /// main repo; a tree left dirty is preserved and the result names its
    /// branch so the parent (or user) can merge it.
    #[tokio::test]
    async fn worktree_isolation_confines_writes_and_reports_branch() {
        let repo = temp_git_repo("confine");
        let provider = Provider::mock(vec![
            vec![AssistantBlock::ToolUse {
                id: "w1".into(),
                name: "write_file".into(),
                input: json!({"path": "isolated.txt", "content": "sub work"}),
            }],
            vec![AssistantBlock::Text {
                text: "done".into(),
            }],
        ]);
        let ctx = ctx_in(with_provider(test_ctx(0, "confine"), provider), &repo);

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "go", "isolation": "worktree"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(
            out.starts_with("done"),
            "carries the sub-agent result: {out}"
        );
        assert!(
            out.contains("worktree-agent-"),
            "the kept tree's branch is named: {out}"
        );

        // The write is in the worktree, NOT the main repo.
        assert!(
            !repo.join("isolated.txt").exists(),
            "main repo must be untouched"
        );
        let trees: Vec<_> = std::fs::read_dir(repo.join(".claude/worktrees"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(trees.len(), 1, "one worktree kept");
        assert!(
            trees[0].join("isolated.txt").exists(),
            "the sub-agent's write landed in its worktree"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn isolated_subagent_branches_from_the_active_workspace_head() {
        let repo = temp_git_repo("active-base");
        let provider = Provider::mock(vec![
            vec![AssistantBlock::ToolUse {
                id: "w1".into(),
                name: "write_file".into(),
                input: json!({"path": "child.txt", "content": "child"}),
            }],
            vec![AssistantBlock::Text {
                text: "done".into(),
            }],
        ]);
        let ctx = git_ctx(
            with_provider(test_ctx(0, "active-base"), provider),
            &repo,
            true,
        );
        worktree::enter(&ctx.cfg, "active-parent").await.unwrap();
        let active = ctx.cfg.effective_workspace().cwd;
        std::fs::write(active.join("active-only.txt"), "active\n").unwrap();
        for args in [
            &["add", "active-only.txt"][..],
            &["commit", "-qm", "active-only"],
        ] {
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&active)
                .args(args)
                .status()
                .unwrap()
                .success());
        }

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "go", "isolation": "worktree"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let children: Vec<_> = std::fs::read_dir(repo.join(".claude/worktrees"))
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path != &active)
            .collect();
        assert_eq!(children.len(), 1, "one child worktree: {children:?}");
        assert_eq!(
            std::fs::read_to_string(children[0].join("active-only.txt")).unwrap(),
            "active\n",
            "child did not branch from the active workspace HEAD"
        );

        worktree::exit(&ctx.cfg, worktree::ExitAction::Keep, false)
            .await
            .unwrap();
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// The worktree sub-agent's system prompt reports ITS cwd, not the
    /// parent's — otherwise the model builds absolute paths from the parent's
    /// working directory and writes straight past the worktree.
    #[tokio::test]
    async fn worktree_subagent_system_reports_the_worktree_cwd() {
        use kloop_provider::MockTurn;
        let repo = temp_git_repo("sys");
        let (provider, seen) =
            Provider::mock_recording(vec![MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "ok".into(),
            }])]);
        let base = ctx_in(with_provider(test_ctx(0, "sys"), provider), &repo);
        let mut cfg = base.cfg.test_clone();
        cfg.system = format!(
            "You are a coding agent.\n\n# Environment\n- Working directory: {}\n- Platform: macos",
            repo.display()
        );
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "go", "isolation": "worktree"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");

        let reqs = seen.lock().unwrap();
        let system = &reqs[0].system;
        assert!(
            system
                .replace('\\', "/")
                .contains(".claude/worktrees/agent-"),
            "system points at the worktree: {system}"
        );
        assert!(
            !system.contains(&format!("- Working directory: {}\n", repo.display())),
            "the parent's working-directory line is gone: {system}"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// A worktree sub-agent that leaves the tree clean has it (and its branch)
    /// torn down — no leftover trees, no branch-name note.
    #[tokio::test]
    async fn clean_worktree_subagent_is_torn_down() {
        let repo = temp_git_repo("clean");
        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "looked around".into(),
        }]]);
        let ctx = ctx_in(with_provider(test_ctx(0, "clean"), provider), &repo);

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "just look", "isolation": "worktree"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "looked around", "no branch note for a clean tree");
        let trees: Vec<_> = std::fs::read_dir(repo.join(".claude/worktrees"))
            .map(|d| d.filter_map(|e| e.ok()).collect())
            .unwrap_or_default();
        assert!(
            trees.is_empty(),
            "the untouched tree was removed: {trees:?}"
        );
        let branches = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["branch", "--list", "worktree-*"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "branch deleted"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[tokio::test]
    async fn registration_failure_cleans_an_already_bound_worktree() {
        let repo = temp_git_repo("register-close");
        let ctx = ctx_in(test_ctx(0, "register-close"), &repo);
        let workspace = ctx.cfg.effective_workspace();
        let agent = "agent-9000".to_string();
        let mut sub = build_sub_config(&ctx, &workspace, None, agent.clone(), None);
        let worktree = worktree::create(&workspace.cwd, &agent).await.unwrap();
        let worktree = bind_subagent_worktree(&mut sub, worktree, "test")
            .await
            .unwrap();
        let sub_cfg = Arc::new(sub);
        ctx.cfg.local_agent.shutdown(&ctx.ui);

        let error = match register_child_with_cleanup(
            &sub_cfg,
            None,
            "registration race",
            &ctx.ui,
            &agent,
            "run_agent",
            Some(worktree),
        )
        .await
        {
            Err(error) => error.to_string(),
            Ok(_) => panic!("registration unexpectedly succeeded during shutdown"),
        };
        assert!(error.contains("session is closing"), "{error}");
        let trees = std::fs::read_dir(repo.join(".claude/worktrees"))
            .map(|entries| entries.filter_map(Result::ok).collect::<Vec<_>>())
            .unwrap_or_default();
        assert!(trees.is_empty(), "registration failure leaked {trees:?}");
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// TWO isolated sub-agents in one parallel batch each write the same
    /// relative filename — the point of worktrees. Each write lands in its
    /// OWN tree (both kept), and the main repo stays clean.
    #[tokio::test]
    async fn parallel_worktree_subagents_write_their_own_trees() {
        let repo = temp_git_repo("parallel");
        let write = AssistantBlock::ToolUse {
            id: "w".into(),
            name: "write_file".into(),
            input: json!({"path": "out.txt", "content": "x"}),
        };
        // `max_rounds: 1` makes each sub-agent sample EXACTLY once, and so the two
        // queued write turns map one-to-one onto the two sub-agents regardless
        // of how their sampling interleaves (a shared mock queue can't otherwise
        // guarantee each sub-agent gets a write). The write executes, then the
        // round limit stops it.
        let provider = Provider::mock(vec![vec![write.clone()], vec![write]]);
        let ctx = ctx_in(with_provider(test_ctx(0, "parallel"), provider), &repo);

        let results = dispatch_tools(
            vec![
                (
                    "t1".into(),
                    "run_agent".into(),
                    json!({"prompt": "a", "isolation": "worktree", "max_rounds": 1}),
                ),
                (
                    "t2".into(),
                    "run_agent".into(),
                    json!({"prompt": "b", "isolation": "worktree", "max_rounds": 1}),
                ),
            ],
            &ctx,
        )
        .await;
        assert_eq!(results.len(), 2);

        assert!(!repo.join("out.txt").exists(), "main repo stays clean");
        let mut trees: Vec<_> = std::fs::read_dir(repo.join(".claude/worktrees"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        trees.sort();
        assert_eq!(trees.len(), 2, "both trees kept: {trees:?}");
        for t in &trees {
            assert_eq!(
                std::fs::read_to_string(t.join("out.txt")).unwrap(),
                "x",
                "each sub-agent's write is in its own tree: {t:?}"
            );
        }
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// An unrecognized isolation value is a hard error — never a silent
    /// fall back to the shared cwd.
    #[tokio::test]
    async fn unknown_isolation_errors() {
        let ctx = test_ctx(0, "iso-bad");
        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "x", "isolation": "sandbox"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("unknown isolation 'sandbox'"), "{out}");
    }

    /// Requesting isolation outside a git repository is an error (fail-closed),
    /// not a shared-cwd fall back.
    #[tokio::test]
    async fn worktree_isolation_requires_a_git_repo() {
        let dir = std::env::temp_dir().join(format!("kloop-agent-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ctx_in(test_ctx(0, "nogit"), &dir);
        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "x", "isolation": "worktree"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("git repository"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Records the sub-agent lifecycle notifications.
    struct RecUi(std::sync::Mutex<Vec<String>>);
    impl Ui for RecUi {
        fn emit(&self, ev: &Event) {
            match ev {
                Event::ItemStarted {
                    item: Item::SubAgent { label, task, .. },
                    ..
                } => self.0.lock().unwrap().push(format!("start {label} {task}")),
                Event::ItemCompleted {
                    item: Item::SubAgent { label, status, .. },
                    ..
                } => {
                    let ok = *status == ItemStatus::Completed;
                    self.0.lock().unwrap().push(format!("end {label} {ok}"));
                }
                Event::BackgroundTaskUpdated(task) => self
                    .0
                    .lock()
                    .unwrap()
                    .push(format!("background {} {:?}", task.id, task.status)),
                Event::AgentMessageUpdated(update) => self
                    .0
                    .lock()
                    .unwrap()
                    .push(format!("message {} {:?}", update.id, update.status)),
                _ => {}
            }
        }
    }

    struct StopUi {
        events: std::sync::Mutex<Vec<String>>,
        bash_started: std::sync::atomic::AtomicBool,
    }

    impl Ui for StopUi {
        fn emit(&self, event: &Event) {
            match event {
                Event::ItemStarted {
                    item: Item::ToolCall { name, agent, .. },
                    ..
                } if name == "bash" && !agent.is_empty() => {
                    self.bash_started.store(true, Ordering::Release);
                }
                Event::BackgroundTaskUpdated(task) => self
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("background {} {:?}", task.id, task.status)),
                Event::AgentMessageUpdated(update) => self
                    .events
                    .lock()
                    .unwrap()
                    .push(format!("message {} {:?}", update.id, update.status)),
                _ => {}
            }
        }
    }

    /// Records the full session-scoped lifecycle projection without conflating it
    /// with the foreground SubAgent item lifecycle.
    #[derive(Default)]
    struct BackgroundTaskUi(std::sync::Mutex<Vec<BackgroundTask>>);
    impl Ui for BackgroundTaskUi {
        fn emit(&self, event: &Event) {
            if let Event::BackgroundTaskUpdated(task) = event {
                self.0.lock().unwrap().push(task.clone());
            }
        }
    }

    /// Two run_agent calls in one batch really run in parallel: each sub-agent's
    /// bash waits for a file the OTHER sub-agent creates, so finishing fast
    /// at all proves concurrency (serial execution takes the full 3s poll).
    /// Each start gets a matching successful end with its own label.
    #[tokio::test]
    async fn consecutive_run_agent_calls_run_as_parallel_subagents() {
        let dir = std::env::temp_dir().join(format!("kloop-paragent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shell_dir = dir.to_string_lossy().replace('\\', "/");
        let barrier = |create: &str, wait: &str| {
            format!(
                "touch {dir}/{create}; for i in $(seq 60); do [ -f {dir}/{wait} ] && exit 0; sleep 0.05; done; exit 1",
                dir = shell_dir
            )
        };
        let tool_use = |id: &str, cmd: String| AssistantBlock::ToolUse {
            id: id.into(),
            name: "bash".into(),
            input: json!({"command": cmd}),
        };
        let done = vec![AssistantBlock::Text {
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
                ("t1".into(), "run_agent".into(), json!({"prompt": "one"})),
                ("t2".into(), "run_agent".into(), json!({"prompt": "two"})),
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
        assert_ne!(
            starts[0], starts[1],
            "each run_agent call gets its own label"
        );
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

    /// One bad call in a run_agent batch fails alone: results stay paired to their
    /// tool_use ids in request order and the healthy sibling completes.
    #[tokio::test]
    async fn failing_run_agent_does_not_sink_the_batch() {
        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "solo done".into(),
        }]]);
        let ctx = with_provider(test_ctx(0, "run-agent-fail"), provider);
        let results = dispatch_tools(
            vec![
                ("t1".into(), "run_agent".into(), json!({"prompt": "solo"})),
                ("t2".into(), "run_agent".into(), json!({})), // missing prompt
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
        assert!(content.as_text().contains("missing field `prompt`"));
    }

    /// agent_type overrides route to the sub-agent's request: its system
    /// prompt, model, and tool set are all the type's, and the tool set is
    /// filtered to the allowlist (plus the always-on read_offloaded).
    #[tokio::test]
    async fn agent_type_routes_system_model_and_tools() {
        use kloop_provider::MockTurn;
        let (provider, seen) =
            Provider::mock_recording(vec![MockTurn::Blocks(vec![AssistantBlock::Text {
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
        let mut cfg = base.cfg.test_clone();
        cfg.agent_types = Arc::new(types);
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (out, is_error) = run_tool(
            "run_agent",
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
        let mut cfg = base.cfg.test_clone();
        cfg.agent_types = Arc::new(types);
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };
        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "x", "agent_type": "ghost"}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("unknown agent_type 'ghost'"), "{out}");
        assert!(out.contains("researcher"), "lists what's available: {out}");
    }

    #[tokio::test]
    async fn run_agent_rejects_wrong_background_type_and_unknown_fields() {
        let ctx = test_ctx(0, "run-agent-strict-input");
        let (wrong_type, type_error) = run_tool(
            "run_agent",
            json!({"prompt": "must not run", "background": "true"}),
            &ctx,
        )
        .await;
        assert!(type_error);
        assert!(wrong_type.contains("invalid type"), "{wrong_type}");

        let (unknown, unknown_error) = run_tool(
            "run_agent",
            json!({"prompt": "must not run", "run_in_background": true}),
            &ctx,
        )
        .await;
        assert!(unknown_error);
        assert!(
            unknown.contains("unknown field `run_in_background`"),
            "{unknown}"
        );
        assert_eq!(ctx.cfg.background_executions.running_count(), 0);
    }

    #[tokio::test]
    async fn run_agent_rejects_invalid_descriptions_before_background_side_effects() {
        let mut ctx = test_ctx(0, "run-agent-description-invalid");
        let ui = std::sync::Arc::new(BackgroundTaskUi::default());
        ctx.ui = ui.clone();
        let cases = [
            (Value::Null, "must be a string"),
            (json!(7), "invalid type"),
            (json!("  "), "must not be empty"),
            (json!("two\tparts"), "single line"),
            (json!("x".repeat(201)), "200-character limit"),
        ];
        for (description, expected) in cases {
            let (output, is_error) = run_tool(
                "run_agent",
                json!({
                    "prompt": "must not run",
                    "description": description,
                    "background": true
                }),
                &ctx,
            )
            .await;
            assert!(is_error, "{output}");
            assert!(output.contains(expected), "{output}");
        }
        assert_eq!(ctx.cfg.background_executions.running_count(), 0);
        assert!(ui.0.lock().unwrap().is_empty());
        assert!(ctx.cfg.inbox.is_empty());
    }

    /// A sub-agent's todo_write writes to its own fresh list, never the
    /// parent's — the Config clone would otherwise share the Arc.
    #[tokio::test]
    async fn subagent_todos_are_isolated_from_the_parent() {
        use crate::tools::TodoItem;
        use crate::tools::TodoStatus;

        let provider = Provider::mock(vec![
            // sub-agent round 1: rewrite its (empty) todo list
            vec![AssistantBlock::ToolUse {
                id: "s1".into(),
                name: "todo_write".into(),
                input: json!({"todos": [
                    {"content": "sub task", "activeForm": "doing sub task", "status": "in_progress"}
                ]}),
            }],
            // sub-agent round 2: wrap up
            vec![AssistantBlock::Text {
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
            vec![("t1".into(), "run_agent".into(), json!({"prompt": "go"}))],
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

    /// Fire-and-forget: run_agent {background:true} returns a "started" message
    /// immediately (NOT the result), and the detached sub-agent reinjects its
    /// final text into the PARENT's inbox as a framed SubAgentResult when done.
    #[tokio::test]
    async fn background_agent_returns_immediately_and_reinjects() {
        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "sub result".into(),
        }]]);
        let mut ctx = with_provider(test_ctx(0, "bg-reinject"), provider);
        let rec = std::sync::Arc::new(RecUi(std::sync::Mutex::new(Vec::new())));
        ctx.ui = rec.clone();

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "go do it", "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("started in the background"), "{out}");
        assert!(out.contains("Agent(go do it)"), "{out}");
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
        let label = match &items[0] {
            InboxItem::SubAgentResult { label, summary } => {
                assert!(label.starts_with("agent-"), "{label}");
                assert_eq!(summary, "sub result");
                label.clone()
            }
            other => panic!("expected SubAgentResult, got {other:?}"),
        };
        assert_eq!(
            ctx.cfg.background_executions.running_count(),
            0,
            "slot freed"
        );
        assert_eq!(
            rec.0.lock().unwrap().clone(),
            vec![
                format!("background {label} Running"),
                format!("background {label} Completed"),
            ]
        );
    }

    #[tokio::test]
    async fn background_agent_uses_explicit_description_for_every_lifecycle_surface() {
        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "finished".into(),
        }]]);
        let mut ctx = with_provider(test_ctx(0, "bg-description"), provider);
        let ui = std::sync::Arc::new(BackgroundTaskUi::default());
        ctx.ui = ui.clone();

        let (output, is_error) = run_tool(
            "run_agent",
            json!({
                "prompt": "inspect the private implementation prompt",
                "description": "audit lifecycle UX",
                "background": true
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        assert!(
            output.starts_with("Agent(audit lifecycle UX) started in the background."),
            "{output}"
        );
        assert!(
            !output.contains("private implementation prompt"),
            "{output}"
        );
        let agent_id = output
            .lines()
            .find_map(|line| line.strip_prefix("Agent ID: "))
            .unwrap()
            .to_string();

        for _ in 0..300 {
            if !ctx.cfg.inbox.is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        for _ in 0..300 {
            if ui.0.lock().unwrap().len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let tasks = ui.0.lock().unwrap().clone();
        assert_eq!(
            tasks,
            vec![
                BackgroundTask {
                    id: agent_id.clone(),
                    run_id: None,
                    kind: BackgroundTaskKind::Agent,
                    description: "audit lifecycle UX".into(),
                    status: BackgroundTaskStatus::Running,
                    output_path: None,
                    detail: None,
                },
                BackgroundTask {
                    id: agent_id,
                    run_id: None,
                    kind: BackgroundTaskKind::Agent,
                    description: "audit lifecycle UX".into(),
                    status: BackgroundTaskStatus::Completed,
                    output_path: None,
                    detail: None,
                },
            ]
        );
    }

    /// A background sub-agent cancelled via stop_agent ends Aborted and
    /// reinjects NOTHING (codex's is_final) — only a wake so a blocked wait
    /// re-evaluates.
    #[tokio::test]
    async fn stopped_background_agent_does_not_reinject() {
        // Sub-agent blocks on a long bash so stop_agent can catch it running.
        let provider = Provider::mock(vec![vec![AssistantBlock::ToolUse {
            id: "s1".into(),
            name: "bash".into(),
            input: json!({"command": "sleep 30"}),
        }]]);
        let mut ctx = with_provider(test_ctx(0, "bg-stopped"), provider);
        let rec = std::sync::Arc::new(StopUi {
            events: std::sync::Mutex::new(Vec::new()),
            bash_started: std::sync::atomic::AtomicBool::new(false),
        });
        ctx.ui = rec.clone();

        let (out, _) = run_tool(
            "run_agent",
            json!({"prompt": "long", "background": true}),
            &ctx,
        )
        .await;
        let agent = out
            .split_whitespace()
            .find(|w| w.starts_with("agent-"))
            .unwrap()
            .to_string();
        // Let the sub-agent get into its bash before stopping it.
        for _ in 0..100 {
            if rec.bash_started.load(Ordering::Acquire) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            rec.bash_started.load(Ordering::Acquire),
            "sub-agent never entered its in-flight bash"
        );
        ctx.cfg
            .local_agent
            .send(
                agent.parse().unwrap(),
                "stop pending".into(),
                "message must fail with the stopped target".into(),
                &ctx.ui,
            )
            .unwrap();
        let (stop_out, is_error) = run_tool("stop_agent", json!({"agent_id": agent}), &ctx).await;
        assert!(!is_error, "{stop_out}");
        assert!(stop_out.contains("Stopping"), "{stop_out}");

        // Wait for it to actually wind down, then assert nothing was reinjected.
        for _ in 0..300 {
            if ctx.cfg.background_executions.running_count() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            ctx.cfg.background_executions.running_count(),
            0,
            "stopped sub-agent did not reach a terminal state"
        );
        let batch = ctx
            .cfg
            .local_agent
            .claim_boundary()
            .expect("stopped target must notify the sender");
        assert!(matches!(
            &batch.items()[0],
            crate::inbox::InboxItem::AgentMessageUndeliverable { failures, .. }
                if failures[0].message_ids[0].as_str() == "message-1"
        ));
        batch.commit(&ctx.ui);
        assert!(
            ctx.cfg.inbox.is_empty(),
            "an interrupted sub-agent reinjects no completion result"
        );
        assert_eq!(
            rec.events.lock().unwrap().clone(),
            vec![
                format!("background {agent} Running"),
                "message message-1 Queued".into(),
                "message message-1 Undeliverable".into(),
                format!("background {agent} Cancelled"),
            ]
        );
    }

    #[tokio::test]
    async fn session_shutdown_cancels_real_background_subagent() {
        let provider = Provider::mock(vec![vec![AssistantBlock::ToolUse {
            id: "s1".into(),
            name: "bash".into(),
            input: json!({"command": "sleep 30"}),
        }]]);
        let mut ctx = with_provider(test_ctx(0, "bg-session-shutdown"), provider);
        let rec = std::sync::Arc::new(RecUi(std::sync::Mutex::new(Vec::new())));
        ctx.ui = rec.clone();

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "long", "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let agent = out
            .split_whitespace()
            .find(|word| word.starts_with("agent-"))
            .unwrap()
            .to_string();
        assert_eq!(ctx.cfg.shutdown_background_work(&ctx.ui).await, 0);
        assert_eq!(ctx.cfg.background_executions.running_count(), 0);
        assert!(ctx.cfg.inbox.is_empty());
        assert_eq!(
            rec.0.lock().unwrap().clone(),
            vec![
                format!("background {agent} Running"),
                format!("background {agent} Cancelled"),
            ]
        );

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "too late", "background": true}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("session is closing"), "{out}");
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

        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "sub result".into(),
        }]]);
        let base = with_provider(test_ctx(0, "subpersist"), provider);
        let mut cfg = base.cfg.test_clone();
        cfg.session_id = "20260714-000000".into();
        cfg.sessions_dir = sessions.clone();
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            parent_rollout_id: Some("20260714-000000#2".into()),
            ..base
        };

        let (out, is_error) =
            run_tool("run_agent", json!({"prompt": "do the sub thing"}), &ctx).await;
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
    async fn background_agent_notes_child_session_and_persists() {
        use crate::rollout::{is_subagent_session, sessions_by_recency};

        let root = std::env::temp_dir().join(format!("kloop-bgpersist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let sessions = root.join("sessions");

        let provider = Provider::mock(vec![vec![AssistantBlock::Text {
            text: "bg result".into(),
        }]]);
        let base = with_provider(test_ctx(0, "bgpersist"), provider);
        let mut cfg = base.cfg.test_clone();
        cfg.session_id = "20260714-111111".into();
        cfg.sessions_dir = sessions.clone();
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            parent_rollout_id: Some("20260714-111111#2".into()),
            ..base
        };

        let (out, is_error) = run_tool(
            "run_agent",
            json!({"prompt": "go", "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(
            out.contains("Its session log is 20260714-111111-agent-"),
            "the started message points at the child session log: {out}"
        );

        // Wait for the detached sub-agent to finish and flush its file.
        for _ in 0..300 {
            if ctx.cfg.background_executions.running_count() == 0
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
    async fn background_agent_without_session_notes_nothing() {
        let provider = Provider::mock(vec![vec![AssistantBlock::Text { text: "x".into() }]]);
        let ctx = with_provider(test_ctx(0, "bg-nosession"), provider);
        let (out, _) = run_tool(
            "run_agent",
            json!({"prompt": "go", "background": true}),
            &ctx,
        )
        .await;
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
            structured_output: None,
        };
        // Success passes through verbatim.
        assert_eq!(
            classify_background(outcome(EndReason::Completed)),
            (ExecutionStatus::Completed, Some("the answer".into()))
        );
        // Round limit is framed but still carries the text.
        let (status, msg) = classify_background(outcome(EndReason::MaxRounds));
        assert_eq!(status, ExecutionStatus::MaxRounds);
        assert!(msg.unwrap().contains("the answer"));
        // A failure is framed with re-dispatch guidance.
        let (status, msg) = classify_background(outcome(EndReason::Error("boom".into())));
        assert_eq!(status, ExecutionStatus::Failed);
        assert!(msg.unwrap().contains("boom"));
        // Interrupted reinjects nothing.
        assert_eq!(
            classify_background(outcome(EndReason::Aborted)),
            (ExecutionStatus::Aborted, None)
        );
    }

    #[test]
    fn cancelled_worktree_location_survives_as_terminal_detail() {
        assert_eq!(
            background_terminal_detail(
                ExecutionStatus::Aborted,
                Some("worktree kept at /tmp/agent-1".into())
            )
            .as_deref(),
            Some("stopped: worktree kept at /tmp/agent-1")
        );
        assert_eq!(
            background_terminal_detail(
                ExecutionStatus::Completed,
                Some("worktree kept at /tmp/agent-1".into())
            ),
            None,
            "natural completion reports the location through its reinjected result"
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
    fn agent_preview_takes_first_line_truncated() {
        assert_eq!(agent_preview("fix the bug\nthen test"), "fix the bug");
        assert_eq!(agent_preview(""), "");
        let long = "x".repeat(100);
        let preview = agent_preview(&long);
        assert_eq!(preview.chars().count(), 81);
        assert!(preview.ends_with('…'));
    }
}
