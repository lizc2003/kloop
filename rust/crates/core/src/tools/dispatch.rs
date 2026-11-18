//! One round of tool calls: batching, the per-call gate, hooks, deadlines, and
//! the single text every call hands back.
//!
//! The catalog decides what a call may name; this module decides whether that
//! call runs, how long it may run, and what the model hears when it stops.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use kloop_protocol::ContentBlock;
use kloop_protocol::ToolResultContent;

use super::Builtin;
use super::CANCEL_SETTLE_GRACE;
use super::MAX_CONCURRENT_TOOL_CALLS;
use super::SourceCallBinding;
use super::SourceRouteState;
use super::ToolCtx;
use super::agent_message;
use super::background_executions;
use super::bash;
use super::catalog::analyze_shell_call;
use super::catalog::find_source;
use super::catalog::is_concurrency_safe_with;
use super::catalog::may_have_effects;
use super::codemode;
use super::fs;
use super::normalize_tool_uses;
use super::notebook;
use super::plan_mode;
use super::powershell;
use super::question;
use super::scheduler;
use super::search;
use super::skill;
use super::subagent;
use super::todo;
use super::tool_search;
use super::workflow;
use super::worktree_tool;
use crate::config::EffectiveWorkspace;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use crate::permissions::Refusal;
use crate::permissions::user_stop;

/// The budget for one call, from whoever owns the tool: the built-in table, or
/// the source that advertised it. An unknown name gets none — it fails on the
/// next line anyway, and a deadline is not how that should be reported.
fn call_timeout(name: &str, input: &Value, ctx: &ToolCtx) -> Option<Duration> {
    match Builtin::from_name(name) {
        Some(builtin) => builtin.timeout(input),
        None => {
            find_source(&ctx.cfg.tool_sources, name).and_then(|source| source.call_timeout(name))
        }
    }
}

struct ToolExecution {
    result: Result<ToolResultContent>,
    file_state_update: Option<(
        Arc<crate::file_state::FileState>,
        crate::file_state::FileStateUpdate,
    )>,
    path_lock: Option<tokio::sync::OwnedMutexGuard<()>>,
}

struct PreparedExecution<'a> {
    read: Option<&'a fs::PreparedRead>,
    mutation: Option<&'a fs::PreparedMutation>,
    source: Option<SourceCallBinding>,
    /// The gate's execution-side decision; tools read it and never re-judge.
    verdict: &'a crate::permissions::Verdict,
    /// The one parse of this call's bash command, when it has one.
    facts: Option<&'a crate::shell::BashFacts>,
    local_send_committed: &'a AtomicBool,
}

impl ToolExecution {
    fn from_result(result: Result<ToolResultContent>) -> Self {
        Self {
            result,
            file_state_update: None,
            path_lock: None,
        }
    }
}

/// Execute one round of tool calls. Consecutive concurrency-safe calls run as
/// one concurrent batch, at most [`MAX_CONCURRENT_TOOL_CALLS`] of them at a
/// time; everything else runs sequentially. Every tool_use always gets a paired
/// tool_result: cancellation patches the remaining calls with is_error
/// "interrupted" results so history stays legal.
pub async fn dispatch_tools(
    tool_uses: Vec<(String, String, Value)>,
    ctx: &ToolCtx,
) -> Vec<ContentBlock> {
    // call_tool envelopes are unwrapped before anything else looks at the
    // calls: concurrency batching, hooks, permissions and the UI must all
    // judge the inner tool, never the wrapper.
    let tool_uses = normalize_tool_uses(tool_uses);
    let sources = &ctx.cfg.tool_sources;
    let expected_program_source = |name: &str| {
        ctx.from_program
            .then(|| tool_search::current_source_binding(name, &ctx.cfg))
            .flatten()
    };
    let mut results = Vec::with_capacity(tool_uses.len());
    // One parse per bash call, before anything asks a question about it. The
    // batch decision, the gate and execution all consume these exact facts.
    let analyses: Vec<Option<Arc<crate::shell::BashFacts>>> = tool_uses
        .iter()
        .map(|(_, name, input)| analyze_shell_call(name, input))
        .collect();
    let mut i = 0;
    while i < tool_uses.len() {
        let safe = is_concurrency_safe_with(
            &tool_uses[i].1,
            &tool_uses[i].2,
            sources,
            analyses[i].as_deref(),
        );
        let mut j = i + 1;
        while j < tool_uses.len()
            && is_concurrency_safe_with(
                &tool_uses[j].1,
                &tool_uses[j].2,
                sources,
                analyses[j].as_deref(),
            ) == safe
        {
            j += 1;
        }
        let batch = &tool_uses[i..j];
        if ctx.cancel.is_cancelled() {
            results.extend(batch.iter().map(|(id, _, _)| interrupted(id)));
        } else if safe {
            // One permit per call in flight. join_all still yields results in
            // request order, and a finished call drops its permit at once, so
            // the cap counts calls that are actually running rather than slots
            // held by results waiting to be collected. A call that waits for a
            // permit through a cancellation never reaches its executor:
            // `run_one` observes the cancelled token first and settles as
            // interrupted.
            let limiter = tokio::sync::Semaphore::new(MAX_CONCURRENT_TOOL_CALLS);
            let futs = batch.iter().enumerate().map(|(offset, (id, name, input))| {
                let call = run_one(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                    ctx.clone(),
                    expected_program_source(name),
                    analyses[i + offset].clone(),
                );
                let limiter = &limiter;
                async move {
                    let _permit = limiter
                        .acquire()
                        .await
                        .expect("the batch limiter outlives its batch and is never closed");
                    call.await
                }
            });
            results.extend(futures::future::join_all(futs).await);
        } else {
            for (offset, (id, name, input)) in batch.iter().enumerate() {
                if ctx.cancel.is_cancelled() {
                    results.push(interrupted(id));
                } else {
                    results.push(
                        run_one(
                            id.clone(),
                            name.clone(),
                            input.clone(),
                            ctx.clone(),
                            expected_program_source(name),
                            analyses[i + offset].clone(),
                        )
                        .await,
                    );
                }
            }
        }
        i = j;
    }
    results
}

/// The in-process cancellation result: the call knows how far it got. A killed
/// session's orphans get their own two texts from rollout repair instead.
pub(crate) fn interrupted(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: "interrupted".into(),
        is_error: true,
    }
}

/// A call the user's stop reached before it ran: never started, or its
/// approval was still waiting when another one was answered No.
pub(super) const NOT_RUN_STOPPED: &str = "Not run: the user stopped the turn before this call ran.";

fn not_run(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: NOT_RUN_STOPPED.into(),
        is_error: true,
    }
}

/// Wait for an answer from the user unless the turn is stopped first. A prompt
/// still queued behind the No that stopped the turn is withdrawn (its future is
/// dropped, which closes the reply channel) rather than asked. `None` = stopped.
pub(super) async fn unless_stopped<T>(
    stop: &CancellationToken,
    asking: impl Future<Output = T>,
) -> Option<T> {
    tokio::select! {
        biased;
        _ = stop.cancelled() => None,
        answer = asking => Some(answer),
    }
}

/// A call to a tool that cannot run at all — a retired name, a control this
/// depth is not given, a capability this front-end does not offer, a shell this
/// host has none of. Rejected before hooks, the permission gate or the registry
/// handler can observe it: none of them has anything to decide about a call
/// that was never legal.
fn reject_unavailable(name: &str, ctx: &ToolCtx) -> Result<()> {
    // The same gate table the request's tool array was built from: root-owned
    // controls a sub-agent is not sent, front-end capabilities this session did
    // not enable, a shell this host resolved no interpreter for. The catalog
    // hides all three, but hiding is not refusing — stale context from before a
    // compaction, a resumed rollout and a forged call all reach dispatch
    // without passing a builder, and must fail before allowlists, hooks,
    // permissions or the registry handler can observe them.
    if let Some(reason) = Builtin::from_name(name)
        .and_then(|tool| tool.unoffered(ctx.depth, ctx.cfg.surface, &ctx.cfg.shell_programs))
    {
        bail!(reason.message(name));
    }
    // A custom agent type's tool allowlist is a capability gate: the tool
    // is filtered out of this sub-agent's defs, so a call to it is a
    // hallucination — reject before hooks or the human are consulted.
    // (The main agent has no allowlist, so this never fires for it.)
    if !crate::agent_type::tool_available(ctx.cfg.tool_allowlist.as_deref(), name) {
        bail!("tool '{name}' is not available to this agent type");
    }
    Ok(())
}

/// Which source this call is bound to, frozen before hooks run. Every later
/// re-check compares against this: a source that changes underneath a call is a
/// discovery error, never a silently re-routed execution.
struct SourceGate {
    /// The tool came through the deferred-capability gate, so it also has to
    /// still be unlocked after the hook.
    discovery_gated: bool,
    /// The binding the call must execute against.
    expected: Option<SourceCallBinding>,
    /// The binding as it read before the hook ran.
    before: Option<SourceCallBinding>,
}

/// Resolve the call's source and its deferred-capability standing.
fn classify_source(
    name: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    expected_program_source: Option<SourceCallBinding>,
) -> Result<SourceGate> {
    let before = match tool_search::current_source_route(name, &ctx.cfg) {
        SourceRouteState::Missing => None,
        SourceRouteState::Available(binding) => Some(binding),
        SourceRouteState::Unavailable(reason) => bail!(reason.clone()),
    };
    if ctx.from_program {
        if before != expected_program_source {
            bail!(
                "tool '{name}' source changed after this Program API was generated; run the Program again from a fresh sampling round"
            );
        }
        return Ok(SourceGate {
            discovery_gated: false,
            expected: expected_program_source,
            before,
        });
    }
    // A source can never claim a built-in name (`reserved_names`), so neither
    // defer sample below can be true for one. Answering here also keeps a
    // built-in call from walking the source catalog twice.
    if Builtin::from_name(name).is_some() {
        return Ok(SourceGate {
            discovery_gated: false,
            expected: before,
            before,
        });
    }
    // Two samples, deliberately. A dynamic source publishes a new
    // catalog between them, so a tool can be absent on the first look
    // and deferred on the second — and a tool that appeared at any point
    // during classification has never been through discovery. Either
    // observation therefore gates it: `||`, not the single read the
    // names invite. `catalog_appearance_during_classification_still_requires_discovery`
    // fails if this collapses into one call.
    let deferred_before = tool_search::is_deferred(name, &ctx.cfg);
    let deferred_after = tool_search::is_deferred(name, &ctx.cfg);
    let discovery_gated = deferred_before || deferred_after;
    if !discovery_gated {
        return Ok(SourceGate {
            discovery_gated,
            expected: before,
            before,
        });
    }
    let source = match tool_search::unlocked_source_for_dispatch(name, ctx, workspace) {
        Some(source) => source,
        None => match tool_search::current_source_route(name, &ctx.cfg) {
            SourceRouteState::Unavailable(reason) => bail!(reason),
            SourceRouteState::Missing | SourceRouteState::Available(_) => {
                bail!(
                    "tool '{name}' is deferred and not loaded yet; call tool_search with query \"select:{name}\" to load its definition, then retry"
                )
            }
        },
    };
    if Some(source) != before {
        bail!(
            "tool '{name}' source changed while its deferred capability was classified; call tool_search with query \"select:{name}\" to load its definition again, then retry"
        );
    }
    Ok(SourceGate {
        discovery_gated,
        expected: Some(source),
        before,
    })
}

/// Let the owning source inspect the call before it runs. Asked twice — once on
/// the frozen binding, once after the pre-tool hook — because a hook may have
/// changed what the source would say.
fn preflight_source(gate: &SourceGate, name: &str, input: &Value, ctx: &ToolCtx) -> Result<()> {
    let Some(binding) = gate.expected else {
        return Ok(());
    };
    let source = ctx
        .cfg
        .tool_sources
        .get(binding.source_slot)
        .context("source preflight binding is no longer registered")?;
    source.preflight(name, input)
}

/// pre_tool hooks run BEFORE the permission gate: hooks are automation policy,
/// permissions are the human's last word — a hook block means there is nothing
/// left to ask about.
pub(super) async fn run_pre_tool_hook(name: &str, input: &Value, ctx: &ToolCtx) -> Result<()> {
    match ctx
        .cfg
        .hooks
        .pre_tool(
            &ctx.cfg.session_id,
            ctx.cfg.agent_label(),
            name,
            input,
            ctx.ui.as_ref(),
        )
        .await
    {
        crate::hooks::HookDecision::Block { reason } => bail!("blocked by hook: {reason}"),
        crate::hooks::HookDecision::Allow { context } => {
            ctx.hook_context.lock().unwrap().extend(context);
            Ok(())
        }
    }
}

/// A hook runs arbitrary code, and a source may republish while it does. The
/// call executes against the binding it was classified on or not at all.
fn recheck_source_after_hook(
    gate: &SourceGate,
    name: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<()> {
    let after = match tool_search::current_source_route(name, &ctx.cfg) {
        SourceRouteState::Missing => None,
        SourceRouteState::Available(binding) => Some(binding),
        SourceRouteState::Unavailable(reason) => bail!(reason),
    };
    if after != gate.before {
        let guidance = if gate.discovery_gated {
            format!(
                "call tool_search with query \"select:{name}\" to load its definition again, then retry"
            )
        } else if ctx.from_program {
            "run the Program again from a fresh sampling round".to_string()
        } else {
            "retry from a fresh sampling round".to_string()
        };
        bail!("tool '{name}' source changed while its pre-tool hook ran; {guidance}");
    }
    if gate.discovery_gated
        && tool_search::unlocked_source_for_dispatch(name, ctx, workspace) != gate.expected
    {
        bail!(
            "tool '{name}' capability changed while its pre-tool hook ran; call tool_search with query \"select:{name}\" to load its definition again, then retry"
        );
    }
    Ok(())
}

/// What `prepare` resolved for this call. Prepared after pre-hooks but before
/// permission, so the gate sees the canonical effective target while the
/// executor retains an open parent directory handle across any approval wait.
struct PreparedCall {
    mutation: Option<fs::PreparedMutation>,
    read: Option<fs::PreparedRead>,
}

impl PreparedCall {
    async fn resolve(name: &str, input: &Value, workspace: &EffectiveWorkspace) -> Result<Self> {
        if name == "notebook_edit" {
            notebook::request_from_input(input)?;
        }
        // A malformed call (an unknown field such as the removed
        // `disable_sandbox`, plan 283) fails here, before anyone is asked
        // about a call that could never run.
        if name == "bash" {
            bash::validate_input(input)?;
        }
        let mutation = if matches!(name, "write_file" | "edit_file" | "notebook_edit") {
            Some(if name == "notebook_edit" {
                fs::prepare_notebook_mutation_input(input, workspace).await?
            } else {
                fs::prepare_mutation_input(name, input, workspace).await?
            })
        } else {
            None
        };
        let read = if name == "read_file" {
            Some(fs::prepare_read(input, workspace).await?)
        } else {
            None
        };
        Ok(Self { mutation, read })
    }

    /// The human's last word on this call, against the resolved target.
    async fn authorize(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolCtx,
        workspace: &EffectiveWorkspace,
        facts: Option<&crate::shell::BashFacts>,
    ) -> Result<crate::permissions::Verdict> {
        let preview_context = self
            .mutation
            .as_ref()
            .and_then(fs::PreparedMutation::preview_context);
        let sandbox_auto_allow = bash::execution_context(name, workspace, &ctx.cfg.shell_login_env);
        let gated = unless_stopped(
            &ctx.stop,
            workspace.permissions.gate_call(
                name,
                input,
                self.mutation
                    .as_ref()
                    .map(fs::PreparedMutation::resolved_path)
                    .or(self.read.as_ref().map(fs::PreparedRead::resolved_path)),
                preview_context.as_ref(),
                ctx.depth,
                sandbox_auto_allow,
                facts,
            ),
        )
        .await;
        let permission = match gated {
            Some(outcome) => {
                ctx.journal_approval(outcome.approvals).await;
                Some(outcome.result)
            }
            None => None,
        };
        match permission {
            None => bail!(NOT_RUN_STOPPED),
            Some(Ok(verdict)) => {
                if let Some(notice) = &verdict.notice {
                    ctx.ui.emit(&Event::Note(notice.message.clone()));
                }
                Ok(verdict)
            }
            Some(Err(Refusal::Denied(reason))) => bail!(reason),
            Some(Err(Refusal::Stopped)) => {
                ctx.stop.cancel();
                bail!(user_stop(name));
            }
        }
    }
}

/// post_tool hooks (and other text-only surfaces) see the flattened text; an
/// image result renders as an `[image: <media_type>]` tag.
async fn run_post_tool_hook(name: &str, input: &Value, execution: &ToolExecution, ctx: &ToolCtx) {
    let (text, is_error) = match &execution.result {
        Ok(content) => (content.as_text().into_owned(), false),
        Err(e) => (format!("{e:#}"), true),
    };
    let context = ctx
        .cfg
        .hooks
        .post_tool(
            &ctx.cfg.session_id,
            ctx.cfg.agent_label(),
            name,
            input,
            &text,
            is_error,
            ctx.ui.as_ref(),
        )
        .await;
    ctx.hook_context.lock().unwrap().extend(context);
}

/// Everything a cancelled turn is allowed to drop: the checks, the hooks, the
/// permission wait and the execution itself. Cancellation kills this future
/// outright unless one of the two flags says an irreversible commit is already
/// under way, in which case [`run_one`] lets it finish.
#[allow(clippy::too_many_arguments)]
async fn run_gated(
    name: &str,
    input: &Value,
    ctx: &ToolCtx,
    expected_program_source: Option<SourceCallBinding>,
    facts: Option<&crate::shell::BashFacts>,
    foreground_shell_started: &AtomicBool,
    local_send_committed: &AtomicBool,
    deadline: &CallDeadline,
) -> Result<ToolExecution> {
    reject_unavailable(name, ctx)?;
    // Freeze the workspace before validating a deferred capability. A stale
    // call is a discovery error, so neither hooks nor the human permission
    // gate should observe it. The same workspace snapshot is then used for
    // prepare, permission, sandbox and execution.
    let workspace = ctx.cfg.effective_workspace();
    let gate = classify_source(name, ctx, &workspace, expected_program_source)?;
    preflight_source(&gate, name, input, ctx)?;
    run_pre_tool_hook(name, input, ctx).await?;
    recheck_source_after_hook(&gate, name, ctx, &workspace)?;
    preflight_source(&gate, name, input, ctx)?;
    let prepared = PreparedCall::resolve(name, input, &workspace).await?;
    let verdict = prepared
        .authorize(name, input, ctx, &workspace, facts)
        .await?;
    let powershell_guard = if name == "powershell" {
        Some(ctx.cfg.powershell_execution_gate.lock().await)
    } else {
        None
    };
    #[cfg(all(test, windows))]
    let powershell_executor_probe = match &powershell_guard {
        Some(guard) => Some(guard.enter_executor().await),
        None => None,
    };
    // Every gate is behind us and nothing below can refuse the call, so this is
    // the one point where "started" is true and not yet stale.
    if let Some(journal) = &ctx.journal
        && may_have_effects(name, input, &ctx.cfg.tool_sources, facts)
    {
        journal.started().await;
    }
    let foreground_shell =
        name == "powershell" || (name == "bash" && !input["background"].as_bool().unwrap_or(false));
    if foreground_shell {
        foreground_shell_started.store(true, Ordering::Release);
    }
    let executed = deadline
        .arm(
            &ctx.cancel,
            execute_tool(
                name,
                input,
                PreparedExecution {
                    read: prepared.read.as_ref(),
                    mutation: prepared.mutation.as_ref(),
                    source: gate.expected,
                    verdict: &verdict,
                    facts,
                    local_send_committed,
                },
                ctx,
                &workspace,
            ),
        )
        .await;
    // Dropped without stopping. There is no verdict to report and no aftermath
    // to release; `report_timeout` replaces this text with the honest one.
    let execution = executed
        .unwrap_or_else(|| ToolExecution::from_result(Err(anyhow!("{name}: did not stop"))));
    if foreground_shell {
        foreground_shell_started.store(false, Ordering::Release);
    }
    #[cfg(all(test, windows))]
    drop(powershell_executor_probe);
    drop(powershell_guard);
    run_post_tool_hook(name, input, &execution, ctx).await;
    Ok(execution)
}

/// One call's time budget, and whether it ran out.
///
/// Armed around the executor alone, not around the whole gated run. The budget
/// is for the work: pre-tool hooks, and above all a human deciding whether to
/// approve the call, are not the tool hanging. A read a reviewer took two
/// minutes over must not come back to the model as a timeout — and in this
/// repo's own logs the slowest calls by far are exactly that wait (`skill` at
/// 3 304s, `write_file` at 211s), so this is not a hypothetical ordering.
struct CallDeadline {
    budget: Option<Duration>,
    expired: AtomicBool,
}

impl CallDeadline {
    fn new(name: &str, input: &Value, ctx: &ToolCtx) -> Self {
        Self {
            budget: call_timeout(name, input, ctx),
            expired: AtomicBool::new(false),
        }
    }

    /// Run `work` under the budget, in two stages, and answer `None` if it never
    /// stopped.
    ///
    /// **Cancel first, and wait.** A tool that watches its token unwinds and
    /// hands back a real verdict — its process killed, its temp file removed,
    /// its lock released. Tearing the future up at the instant the budget
    /// expires would strand exactly those.
    ///
    /// **Then drop.** Waiting forever for a tool that is not listening is the
    /// hang this whole mechanism exists to end, and for the bucket that
    /// motivated it there is nothing else available: a `ToolSource` call gets no
    /// token at this seam, so dropping its future IS its cancellation — Rust
    /// unwinds the in-flight request through `Drop`, which the TypeScript
    /// harness this design comes from cannot do at any price. That is why the
    /// order matters rather than the choice: whoever can stop cleanly does, and
    /// only whoever cannot gets dropped.
    async fn arm<T>(&self, cancel: &CancellationToken, work: impl Future<Output = T>) -> Option<T> {
        let Some(budget) = self.budget else {
            return Some(work.await);
        };
        let mut work = std::pin::pin!(work);
        tokio::select! {
            done = &mut work => return Some(done),
            _ = tokio::time::sleep(budget) => {}
        }
        self.expired.store(true, Ordering::Release);
        cancel.cancel();
        // A call given 50ms to work gets 50ms to stop; one given five minutes
        // gets the full window. Scaling the smaller case is what keeps a test
        // that arms a millisecond budget from waiting seconds to observe it.
        tokio::time::timeout(budget.min(CANCEL_SETTLE_GRACE), work)
            .await
            .ok()
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }
}

/// What a finished call leaves for its caller to release once the final
/// `tool_result` exists: the write authority it earned and the path lock it
/// held. A cancelled or failed call leaves neither.
#[derive(Default)]
struct Aftermath {
    file_state_update: Option<(
        Arc<crate::file_state::FileState>,
        crate::file_state::FileStateUpdate,
    )>,
    path_lock: Option<tokio::sync::OwnedMutexGuard<()>>,
}

/// The gated run's verdict as the turn's `tool_result`. `None` is a
/// cancellation that nothing irreversible had started.
fn settle_execution(id: &str, gated: Option<Result<ToolExecution>>) -> (ContentBlock, Aftermath) {
    let error_result = |e: anyhow::Error| ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: format!("{e:#}").into(),
        is_error: true,
    };
    match gated {
        None => (interrupted(id), Aftermath::default()),
        Some(Ok(execution)) => {
            let ToolExecution {
                result,
                file_state_update,
                path_lock,
            } = execution;
            let result = match result {
                Ok(content) => ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content,
                    is_error: false,
                },
                Err(e) => error_result(e),
            };
            (
                result,
                Aftermath {
                    file_state_update,
                    path_lock,
                },
            )
        }
        Some(Err(e)) => (error_result(e), Aftermath::default()),
    }
}

/// Append a model-visible notice to a finished tool result. Appending to the
/// result rather than recording a history entry of its own is what keeps it
/// ordered against the round's other results and replayable from a rollout.
fn append_notice(result: &mut ContentBlock, notice: &str) {
    let ContentBlock::ToolResult { content, .. } = result else {
        return;
    };
    match content {
        ToolResultContent::Text(text) => {
            text.push('\n');
            text.push_str(notice);
        }
        ToolResultContent::Blocks(blocks) => blocks.push(ContentBlock::Text {
            text: notice.to_string(),
        }),
    }
}

/// Replace a timed-out call's verdict with one that says so.
///
/// Only when the call settled as a failure. A tool that honoured the
/// cancellation and still finished its work did the work, and handing the model
/// a timeout instead of the answer it has would be a lie that costs a round.
///
/// The wording is the honest part. This budget cancels; it does not kill. A
/// tool that ignores its token is running still, and the model is told that
/// rather than left to assume the process is gone — deepseek-harness documents
/// the same limitation for the same mechanism.
fn report_timeout(result: &mut ContentBlock, name: &str, deadline: Duration) {
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        return;
    };
    if !*is_error {
        return;
    }
    // `{:?}` on a Duration prints "60s" / "40ms" rather than rounding a
    // sub-second budget to the "0s" that `as_secs` would.
    *content = ToolResultContent::Text(format!(
        "{name}: timed out after {deadline:?}. The call was asked to cancel and this \
         result is what it settled to. Nothing killed it: a tool that does not honour \
         cancellation may still be running in the background, and its side effects \
         may still land."
    ));
}

pub(super) async fn run_one(
    id: String,
    name: String,
    input: Value,
    ctx: ToolCtx,
    expected_program_source: Option<SourceCallBinding>,
    facts: Option<Arc<crate::shell::BashFacts>>,
) -> ContentBlock {
    // Checked here rather than in the batch loop so it also reaches a call that
    // sat behind the concurrency limiter, and a program's calls, which arrive
    // one by one. Such a call never showed a row, so it emits nothing either —
    // the same as one the batch loop marks interrupted.
    if ctx.stop.is_cancelled() {
        return not_run(&id);
    }
    // This call's own cancellation scope. Cancelling the parent still reaches
    // it, so an interrupted turn behaves exactly as before; what the child buys
    // is the other direction — the deadline can stop THIS call without
    // cancelling the rest of the batch, and the two are told apart by which
    // token fired rather than by a timer race.
    let call_cancel = ctx.cancel.child_token();
    let ctx = ToolCtx {
        cancel: call_cancel.clone(),
        journal: ctx.journal.map(|journal| journal.for_call(&id)),
        ..ctx
    };
    let event_input = agent_message::event_input(&name, &input);
    ctx.ui.emit(&Event::ItemStarted {
        id: id.clone(),
        item: Item::ToolCall {
            agent: ctx.cfg.agent_label().to_string(),
            name: name.clone(),
            input: event_input.clone(),
            status: ItemStatus::InProgress,
            output: None,
        },
    });
    // Once a foreground shell has spawned, its own cancellation branch must
    // finish process-tree cleanup before we emit interrupted. Earlier
    // cancellation (hooks/permission) still drops the gated future, so no
    // process can appear after the turn was cancelled.
    let foreground_shell_started = AtomicBool::new(false);
    // Local send is an irreversible in-memory commit. If cancellation lands
    // during a post-hook, finish the paired queued result instead of reporting
    // `interrupted` after the recipient mailbox already changed.
    let local_send_committed = AtomicBool::new(false);
    let deadline = CallDeadline::new(&name, &input, &ctx);
    let mut gated = Box::pin(run_gated(
        &name,
        &input,
        &ctx,
        expected_program_source,
        facts.as_deref(),
        &foreground_shell_started,
        &local_send_committed,
        &deadline,
    ));
    let gated_result = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            // `deadline.expired` is what keeps our own cancellation from being
            // read as the turn's. Without it this branch and the finished
            // `gated` are both ready at every timeout, and whichever select
            // picks decides whether the model hears "interrupted" or "timed
            // out" — the same scoping deepseek-harness needs so a nested outer
            // deadline reads as ordinary upstream cancellation.
            if deadline.expired()
                || foreground_shell_started.load(Ordering::Acquire)
                || local_send_committed.load(Ordering::Acquire)
            {
                Some(gated.await)
            } else {
                None
            }
        }
        result = &mut gated => Some(result),
    };
    let (mut result, aftermath) = settle_execution(&id, gated_result);
    if deadline.expired() {
        report_timeout(
            &mut result,
            &name,
            deadline.budget.expect("a budget expired"),
        );
    }
    // The model has a successful Read/Write/Edit only once the final tool_result
    // exists. Executor-local reads and work canceled while a post-hook runs do
    // not create write authority. Mutation executors clear authority before
    // touching disk, so an interrupted commit remains conservative.
    //
    // The reread advisory hangs off the same condition for the same reason: a
    // read whose result the model never sees must not count as one it has.
    if let Some((state, update)) = aftermath.file_state_update {
        if let Some(advisory) = fs::reread_advisory(&state, &update) {
            append_notice(&mut result, &advisory);
        }
        state.apply(update);
    }
    drop(aftermath.path_lock);
    let ContentBlock::ToolResult {
        tool_use_id,
        is_error,
        content,
    } = &result
    else {
        unreachable!("run_one always builds a tool_result")
    };
    // Bound the transport: a UI previews only a few lines, and a huge bash
    // output would otherwise be cloned onto the event channel wholesale. The UI
    // truncates further for display.
    let output: String = content.as_text().chars().take(4000).collect();
    ctx.ui.emit(&Event::ItemCompleted {
        id: tool_use_id.clone(),
        item: Item::ToolCall {
            agent: ctx.cfg.agent_label().to_string(),
            name: name.clone(),
            input: event_input,
            status: if *is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            },
            output: (!output.is_empty()).then_some(output),
        },
    });
    result
}

/// Returns an explicitly type-erased future: this is the recursion boundary
/// (execute_tool -> run_agent -> run_turn -> dispatch_tools -> execute_tool), and
/// the `dyn Future + Send` signature is what lets rustc resolve the otherwise
/// cyclic Send inference for the recursive async call graph.
fn execute_tool<'a>(
    name: &'a str,
    input: &'a Value,
    prepared: PreparedExecution<'a>,
    ctx: &'a ToolCtx,
    workspace: &'a EffectiveWorkspace,
) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>> {
    Box::pin(async move {
        let Some(builtin) = Builtin::from_name(name) else {
            // Not a built-in, so it is an external source tool — which may also
            // return images, hence the same non-text path the file reads take.
            // A source can never claim a built-in name (`reserved_names`), so
            // this branch and the match below cannot both be right for one call.
            let Some(binding) = prepared.source else {
                return ToolExecution::from_result(Err(anyhow!("unknown tool: {name}")));
            };
            let Some(source) = ctx.cfg.tool_sources.get(binding.source_slot) else {
                return ToolExecution::from_result(Err(anyhow!(
                    "source binding for tool '{name}' is no longer registered"
                )));
            };
            return match source
                .call_at_version(name, input, Some(binding.version))
                .await
            {
                Ok(out) => {
                    if let Some(slot) = &ctx.program_result {
                        *slot.lock().unwrap() = out.structured.clone();
                    }
                    ToolExecution::from_result(Ok(out.into_content()))
                }
                Err(error) => ToolExecution::from_result(Err(error)),
            };
        };
        // Exhaustive on purpose: a new built-in that reaches dispatch without an
        // arm here is a compile error, not an `unknown tool` at runtime.
        let text: Result<String> = match builtin {
            // read_file is the sole BUILT-IN that can return non-text: on an
            // image file it returns an image block (ToolResultContent::Blocks).
            Builtin::ReadFile => {
                let Some(prepared) = prepared.read else {
                    return ToolExecution::from_result(Err(anyhow!(
                        "read_file: target was not prepared"
                    )));
                };
                let state = Arc::clone(&workspace.file_state);
                return match fs::read_file_tool(input, prepared).await {
                    Ok(output) => ToolExecution {
                        result: Ok(output.content),
                        file_state_update: Some((state, output.state_update)),
                        path_lock: None,
                    },
                    Err(error) => ToolExecution::from_result(Err(error)),
                };
            }
            // The file mutations carry a state update and a path lock back out,
            // so they also return before the text path.
            Builtin::WriteFile | Builtin::EditFile | Builtin::NotebookEdit => {
                let Some(prepared_mutation) = prepared.mutation else {
                    return ToolExecution::from_result(Err(anyhow!(
                        "{name}: mutation target was not prepared"
                    )));
                };
                let state = Arc::clone(&workspace.file_state);
                let output = match builtin {
                    Builtin::WriteFile => {
                        fs::write_file_tool(input, prepared_mutation, ctx, workspace).await
                    }
                    Builtin::EditFile => {
                        fs::edit_file_tool(input, prepared_mutation, ctx, workspace).await
                    }
                    _ => fs::notebook_edit_tool(input, prepared_mutation, ctx, workspace).await,
                };
                return match output {
                    Ok(output) => ToolExecution {
                        result: Ok(ToolResultContent::Text(output.content)),
                        file_state_update: Some((state, output.state_update)),
                        path_lock: Some(output.path_lock),
                    },
                    Err(error) => ToolExecution::from_result(Err(error)),
                };
            }
            Builtin::Bash => {
                bash::bash_tool(input, ctx, workspace, prepared.verdict, prepared.facts).await
            }
            Builtin::PowerShell => powershell::powershell_tool(input, ctx, workspace).await,
            Builtin::BashOutput => bash::bash_output_tool(input, ctx).await,
            Builtin::StopBash => bash::stop_bash_tool(input, ctx).await,
            Builtin::Grep => {
                search::grep_tool(input, &workspace.cwd, Arc::clone(&workspace.permissions)).await
            }
            // glob hands a program its path list as a string[] (built-ins are
            // otherwise strings); the model-facing text is unchanged.
            Builtin::Glob => {
                search::glob_tool(
                    input,
                    &workspace.cwd,
                    ctx.program_result.as_ref(),
                    Arc::clone(&workspace.permissions),
                )
                .await
            }
            Builtin::TodoWrite => todo::todo_write_tool(input, ctx),
            Builtin::Skill => skill::skill_tool(input, ctx, workspace).await,
            Builtin::ToolSearch => tool_search::tool_search_tool(input, ctx, workspace).await,
            // Only malformed envelopes reach this arm — well-formed ones were
            // rewritten to the inner call at dispatch entry. Re-run the shared
            // envelope parser so the error is exactly the one unwrapping saw.
            Builtin::CallTool => match tool_search::parse_call_tool(input) {
                Ok(_) => Err(anyhow!(
                    "call_tool: envelope reached execution without being unwrapped"
                )),
                Err(error) => Err(error),
            },
            Builtin::RunAgent => subagent::run_agent_tool(input, ctx, workspace).await,
            Builtin::SendMessage => {
                let result = agent_message::send_message_tool(input, ctx);
                if result.is_ok() {
                    prepared.local_send_committed.store(true, Ordering::Release);
                }
                result
            }
            Builtin::ListAgents => agent_message::list_agents_tool(input, ctx),
            Builtin::AskUserQuestion => question::ask_user_question_tool(input, ctx).await,
            Builtin::EnterPlanMode => plan_mode::enter_plan_mode_tool(input, ctx, workspace).await,
            Builtin::ExitPlanMode => plan_mode::exit_plan_mode_tool(input, ctx, workspace).await,
            Builtin::EnterWorktree => worktree_tool::enter_worktree_tool(input, ctx).await,
            Builtin::ExitWorktree => worktree_tool::exit_worktree_tool(input, ctx).await,
            Builtin::WaitForActivity => {
                background_executions::wait_for_activity_tool(input, ctx).await
            }
            Builtin::StopAgent => background_executions::stop_agent_tool(input, ctx).await,
            Builtin::StopProgram => background_executions::stop_program_tool(input, ctx).await,
            Builtin::StopWorkflow => background_executions::stop_workflow_tool(input, ctx).await,
            Builtin::CronCreate => scheduler::cron_create_tool(input, ctx).await,
            Builtin::CronDelete => scheduler::cron_delete_tool(input, ctx).await,
            Builtin::CronList => scheduler::cron_list_tool(input, ctx).await,
            Builtin::ScheduleWakeup => scheduler::schedule_wakeup_tool(input, ctx).await,
            Builtin::RunProgram => codemode::run_program_tool(input, ctx, workspace).await,
            Builtin::Workflow => workflow::workflow_tool_in_workspace(input, ctx, workspace).await,
        };
        ToolExecution::from_result(text.map(ToolResultContent::Text))
    })
}
