use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::config::Config;
use crate::event::Event;
use crate::history::History;
use crate::inbox::Inbox;
use crate::tools::dispatch_tools;
use crate::tools::ToolCtx;
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::IncompleteReason;
use kloop_protocol::Message;
use kloop_protocol::MAX_OUTPUT_TOKENS;

mod sampling;

use sampling::sample_with_retry;
use sampling::SampleOk;
use sampling::Sampled;

/// The single output seam: core emits an [`Event`](crate::event::Event) stream
/// and each front-end projects it (the TUI into cells, the server/headless into
/// wire notifications, the plain REPL into stdout). A front-end that renders
/// only text and notes matches those and routes the rest through
/// [`Event::as_note`](crate::event::Event::as_note) — the old wide-trait default
/// downgrade, now in one place. `Approver::confirm` is a separate seam: an
/// approval is a request awaiting an answer, not a produced event.
pub trait Ui: Send + Sync {
    fn emit(&self, ev: &Event);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EndReason {
    Completed,
    MaxRounds,
    Aborted,
    Error(String),
}

#[derive(Clone, Debug)]
pub struct TurnOutcome {
    pub reason: EndReason,
    pub final_text: String,
    pub rounds: usize,
    /// Present only for a Workflow child forced through the internal
    /// structured_output tool; ordinary turns always leave it None.
    pub structured_output: Option<Value>,
}

#[derive(Clone, Debug, Default)]
struct TurnOptions {
    structured_schema: Option<Value>,
}

pub(crate) async fn run_structured_turn(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    schema: Value,
) -> TurnOutcome {
    if let Err(error) = crate::structured_output::validate_schema(&schema) {
        return TurnOutcome {
            reason: EndReason::Error(format!("{error:#}")),
            final_text: String::new(),
            rounds: 0,
            structured_output: None,
        };
    }
    run_turn_with_options(
        cfg,
        history,
        ui,
        cancel,
        depth,
        TurnOptions {
            structured_schema: Some(schema),
        },
    )
    .await
}

/// The agent loop. One "round" = one sampling request plus the tool calls it
/// asked for. Continuation is decided ONLY by the presence of tool_use blocks
/// in the sampled response — never by stop_reason. Hooks bracket the loop:
/// a blocking pre_turn hook means the turn never starts; post_turn runs on
/// every ending of a started turn (sub-agent turns included — they inherit
/// the parent's hook set and session id).
pub async fn run_turn(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
) -> TurnOutcome {
    run_turn_with_options(cfg, history, ui, cancel, depth, TurnOptions::default()).await
}

async fn run_turn_with_options(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    options: TurnOptions,
) -> TurnOutcome {
    // A sub-agent (typed local identity set) fires subagent_start/subagent_stop instead
    // of pre_turn/post_turn — the split both cc and codex converge on (a
    // sub-agent's turn boundary is its own event, carrying its transcript and
    // result). The main agent keeps the plain turn hooks.
    let agent = cfg.agent_label();
    let start = if agent.is_empty() {
        cfg.hooks.pre_turn(&cfg.session_id, ui.as_ref()).await
    } else {
        cfg.hooks
            .subagent_start(&cfg.session_id, agent, ui.as_ref())
            .await
    };
    match start {
        crate::hooks::HookDecision::Block { reason } => {
            let which = if agent.is_empty() {
                "pre_turn"
            } else {
                "subagent_start"
            };
            return TurnOutcome {
                reason: EndReason::Error(format!("turn blocked by {which} hook: {reason}")),
                final_text: String::new(),
                rounds: 0,
                structured_output: None,
            };
        }
        crate::hooks::HookDecision::Allow { context } => {
            for text in context {
                history.record(Message::user_text(text));
            }
        }
    }
    let outcome = turn_rounds(cfg, history, ui, cancel, depth, &options).await;
    let stop_context = if agent.is_empty() {
        cfg.hooks.post_turn(&cfg.session_id, ui.as_ref()).await
    } else {
        // Copy the transcript path out before the mutable record() borrow.
        let transcript = history.rollout_path().map(|p| p.to_path_buf());
        cfg.hooks
            .subagent_stop(
                &cfg.session_id,
                agent,
                transcript.as_deref(),
                &outcome.final_text,
                ui.as_ref(),
            )
            .await
    };
    for text in stop_context {
        history.record(Message::user_text(text));
    }
    outcome
}

async fn turn_rounds(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    options: &TurnOptions,
) -> TurnOutcome {
    let build_tools = || {
        let mut tools = crate::tools::all_tool_defs(
            depth,
            &cfg.tool_sources,
            cfg.defer_threshold,
            cfg.surface,
            &cfg.shell_programs,
        );
        // The `skill` tool exists only at depth 0 (like `task`) and only when a
        // model-invocable skill is loaded — user commands (`SkillSource::Command`)
        // are `/name`-only and don't warrant the tool on their own. Skills are a
        // top-level orchestration feature: a sub-agent gets a focused task, not the
        // whole skills catalog (which would otherwise ride every sub-agent request,
        // and a `fork` skill's own sub-agent could re-trigger it). Added after
        // all_tool_defs so it is not counted toward the defer threshold or exposed
        // to run_program's API — it is a prompt-activation seam, not a source tool.
        // Placed before the allowlist filter so a restricted agent type can gate it
        // like any tool.
        let has_model_skill = cfg
            .skills
            .iter()
            .any(|s| s.source == crate::skills::SkillSource::Skill);
        if depth == 0 && has_model_skill && !tools.iter().any(|t| t.name == "skill") {
            tools.push(crate::tools::skill_tool_def());
        }
        // A custom agent type may restrict this sub-agent's tools; the main agent
        // (None) keeps them all. read_offloaded is never filtered out.
        if cfg.tool_allowlist.is_some() {
            let allow = cfg.tool_allowlist.as_deref();
            tools.retain(|t| crate::agent_type::tool_available(allow, &t.name));
        }
        if let Some(schema) = &options.structured_schema {
            // Appended after agent-type filtering: this is an internal completion
            // protocol, never a user-configurable capability or ordinary tool.
            tools.push(crate::structured_output::tool_def(schema));
        }
        // At depth 0 run_agent exists; list configured agent types in its
        // description so the model knows what it can dispatch to.
        if depth == 0 && !cfg.agent_types.is_empty() {
            if let Some(run_agent) = tools.iter_mut().find(|tool| tool.name == "run_agent") {
                run_agent
                    .description
                    .push_str(&crate::agent_type::agent_types_hint(&cfg.agent_types));
            }
        }
        tools
    };
    let mut tools = build_tools();
    // A sub-agent's text is its deliverable and returns via the tool result;
    // streaming it to the main UI would interleave with the parent's output.
    let stream_text = depth == 0;
    let growth = compact::max_turn_growth(MAX_OUTPUT_TOKENS);
    // Overflow is recovered at most once per turn: compact, then retry. A
    // second overflow after a successful compaction surfaces as an error.
    let mut overflow_compact_attempted = false;
    // The model can be swapped once per turn: after retries are exhausted on
    // the primary, the rest of the turn runs on the fallback.
    let mut active_model = cfg.model.clone();
    let mut truncation_recoveries = 0u32;
    // Cut-off text from truncated rounds, prepended to the final answer so a
    // truncated-then-continued turn returns the whole deliverable.
    let mut truncated_prefix = String::new();
    // Turn-unique counter for streamed assistant/reasoning item ids (`msg-N`,
    // `reasoning-N`): owned here so ids don't reset each round.
    let mut item_seq = 0u64;
    let mut rounds = 0;
    let mut structured_failures = 0usize;
    loop {
        if cfg.max_rounds.is_some_and(|limit| rounds >= limit) {
            return TurnOutcome {
                reason: EndReason::MaxRounds,
                final_text: String::new(),
                rounds,
                structured_output: None,
            };
        }
        if rounds > 0 {
            // MCP list_changed publishes a new source generation between
            // sampling rounds. Never mutate an in-flight request; rebuild the
            // next round from one fresh source snapshot instead.
            tools = build_tools();
        }
        let round = rounds;
        rounds += 1;
        // Step-boundary steering: deliver anything the user typed during the
        // previous round (tool execution / sampling) as a user message before
        // this round's request. At round 0 the queue is empty (the turn just
        // started) so this is a no-op. Never touches an in-flight request.
        drain_inbox(&cfg.inbox, history, ui);
        drain_local_mailbox(cfg, history, ui);
        // The injected context is outside history and a dynamic MCP refresh may
        // replace its deferred-tool notice between rounds, so account for the
        // current version rather than pinning the turn's first estimate.
        let workspace = cfg.effective_workspace();
        let instructions_tokens =
            injected_context(cfg, &workspace, depth).map_or(0, |s| s.len() as u64 / 4);
        // Predictive: compact BEFORE sampling when this round's estimated
        // growth would overflow the window — don't wait to be rejected.
        if let Some(window) = cfg.context_window {
            if history.messages().len() >= 2
                && compact::predicted_overflow(
                    history.estimated_tokens() + instructions_tokens,
                    growth,
                    window,
                )
            {
                ui.emit(&Event::Note(
                    "predicted context overflow; compacting history".into(),
                ));
                match compact::run_compaction(cfg, &active_model, history, cancel).await {
                    Ok(stats) => ui.emit(&Event::Note(format!(
                        "history compacted: {} summarized, {} kept verbatim",
                        stats.summarized, stats.kept
                    ))),
                    Err(e) => {
                        if cancel.is_cancelled() {
                            return TurnOutcome {
                                reason: EndReason::Aborted,
                                final_text: String::new(),
                                rounds: round,
                                structured_output: None,
                            };
                        }
                        // Predictive failure is not fatal: fall through and let
                        // the request itself succeed or overflow reactively.
                        ui.emit(&Event::Note(format!("predictive compaction failed: {e:#}")));
                    }
                }
            }
        }

        let SampleOk {
            blocks,
            usage,
            outcome,
        } = match sample_with_retry(
            cfg,
            &active_model,
            history.messages(),
            &tools,
            ui,
            cancel,
            stream_text,
            depth,
            &workspace,
            &mut item_seq,
        )
        .await
        {
            Sampled::Ok(ok) => ok,
            Sampled::Overflow => {
                if cfg.context_window.is_none() || overflow_compact_attempted {
                    return TurnOutcome {
                        reason: EndReason::Error(
                            "context window exceeded (compaction unavailable or already tried)"
                                .into(),
                        ),
                        final_text: String::new(),
                        rounds: round,
                        structured_output: None,
                    };
                }
                overflow_compact_attempted = true;
                ui.emit(&Event::Note(
                    "context window exceeded; compacting and retrying".into(),
                ));
                match compact::run_compaction(cfg, &active_model, history, cancel).await {
                    Ok(stats) => {
                        ui.emit(&Event::Note(format!(
                            "history compacted: {} summarized, {} kept verbatim",
                            stats.summarized, stats.kept
                        )));
                        continue;
                    }
                    Err(e) => {
                        return TurnOutcome {
                            reason: if cancel.is_cancelled() {
                                EndReason::Aborted
                            } else {
                                EndReason::Error(format!("reactive compaction failed: {e:#}"))
                            },
                            final_text: String::new(),
                            rounds: round,
                            structured_output: None,
                        }
                    }
                }
            }
            Sampled::Cancelled { partial } => {
                let final_text = text_content(&partial);
                if !partial.is_empty() {
                    history.record(Message::assistant(partial));
                }
                return TurnOutcome {
                    reason: EndReason::Aborted,
                    final_text,
                    rounds: round,
                    structured_output: None,
                };
            }
            Sampled::Partial { error, blocks } => {
                let final_text = text_content(&blocks);
                if !blocks.is_empty() {
                    history.record(Message::assistant(blocks));
                }
                return TurnOutcome {
                    reason: EndReason::Error(error),
                    final_text,
                    rounds: round,
                    structured_output: None,
                };
            }
            Sampled::Terminal(error) => {
                return TurnOutcome {
                    reason: EndReason::Error(error),
                    final_text: String::new(),
                    rounds: round,
                    structured_output: None,
                };
            }
            Sampled::Failed(e) => {
                // Retries exhausted on the primary model: switch to the
                // fallback (once) instead of surfacing the error.
                if let Some(fallback) = &cfg.fallback_model {
                    if *fallback != active_model {
                        ui.emit(&Event::Note(format!(
                            "sampling failed on {active_model}; switching to fallback model {fallback}: {e}"
                        )));
                        active_model = fallback.clone();
                        continue;
                    }
                }
                return TurnOutcome {
                    reason: EndReason::Error(e),
                    final_text: String::new(),
                    rounds: round,
                    structured_output: None,
                };
            }
        };
        if let Err(error) = validate_assistant_result(&outcome, &blocks) {
            return TurnOutcome {
                reason: EndReason::Error(error),
                final_text: String::new(),
                rounds: round + 1,
                structured_output: None,
            };
        }
        if !blocks.is_empty() {
            history.record(Message::assistant(blocks.clone()));
        }
        if let Some(usage) = usage {
            // total() = uncached + cached input + output = full context size
            // at this request; anchors the char-heuristic estimate for items
            // recorded after this point.
            history.note_usage(usage.total());
        }

        let tool_uses: Vec<(String, String, Value)> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
            .collect();

        match &outcome {
            AssistantOutcome::Refused => {
                return TurnOutcome {
                    reason: EndReason::Error("model refused the request".into()),
                    final_text: text_content(&blocks),
                    rounds: round + 1,
                    structured_output: None,
                };
            }
            AssistantOutcome::Filtered => {
                return TurnOutcome {
                    reason: EndReason::Error("provider filtered the response".into()),
                    final_text: text_content(&blocks),
                    rounds: round + 1,
                    structured_output: None,
                };
            }
            AssistantOutcome::Incomplete(reason) => {
                let reason = match reason {
                    IncompleteReason::PauseTurn => "pause_turn",
                    IncompleteReason::Provider(reason) => reason,
                };
                return TurnOutcome {
                    reason: EndReason::Error(format!(
                        "provider returned an incomplete response: {reason}"
                    )),
                    final_text: text_content(&blocks),
                    rounds: round + 1,
                    structured_output: None,
                };
            }
            AssistantOutcome::OutputLimit(_) => {
                let round_text = text_content(&blocks);
                if truncation_recoveries < TRUNCATION_RECOVERY_LIMIT {
                    truncation_recoveries += 1;
                    truncated_prefix.push_str(&round_text);
                    ui.emit(&Event::Note(format!(
                        "response truncated by output limit; asking the model to continue ({truncation_recoveries}/{TRUNCATION_RECOVERY_LIMIT})"
                    )));
                    history.record(Message::user_text(TRUNCATION_CONTINUE_MSG));
                    continue;
                }
                let final_text = format!("{truncated_prefix}{round_text}");
                return TurnOutcome {
                    reason: EndReason::Error(format!(
                        "response remained truncated after {TRUNCATION_RECOVERY_LIMIT} continuation attempts"
                    )),
                    final_text,
                    rounds: round + 1,
                    structured_output: None,
                };
            }
            AssistantOutcome::EndTurn => {
                if options.structured_schema.is_some() {
                    structured_failures += 1;
                    if structured_failures >= 3 {
                        return TurnOutcome {
                            reason: EndReason::Error(
                                "structured output was not produced after 3 attempts".into(),
                            ),
                            final_text: String::new(),
                            rounds: round + 1,
                            structured_output: None,
                        };
                    }
                    history.record(Message::user_text(crate::structured_output::nudge()));
                    continue;
                }
                // The turn would end here — but a steer that landed during this
                // final sampling must not be lost. Absorb it and keep going, so a
                // late "wait, also do X" is answered instead of dropped. (Steers
                // during tool execution are already delivered at the loop top.)
                if drain_inbox(&cfg.inbox, history, ui) {
                    continue;
                }
                if !cfg.local_agent.can_finish_naturally() {
                    continue;
                }
                let round_text = text_content(&blocks);
                let final_text = if truncated_prefix.is_empty() {
                    round_text
                } else {
                    format!("{truncated_prefix}{round_text}")
                };
                return TurnOutcome {
                    reason: EndReason::Completed,
                    final_text,
                    rounds: round + 1,
                    structured_output: None,
                };
            }
            AssistantOutcome::ToolUse => {}
        }
        let ctx = ToolCtx {
            cfg: cfg.clone(),
            ui: ui.clone(),
            cancel: cancel.clone(),
            depth,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            // The assistant message carrying these tool_uses was just recorded,
            // so this is the id of the turn that a spawned sub-agent descends
            // from.
            parent_rollout_id: history.rollout_last_id().map(str::to_string),
            program_result: None,
        };
        let (results, structured_output) = match &options.structured_schema {
            Some(schema) => dispatch_structured_tools(tool_uses, &ctx, schema).await,
            None => (dispatch_tools(tool_uses, &ctx).await, None),
        };
        // Record results BEFORE checking cancellation so every tool_use has a
        // paired tool_result and history stays legal for the next request.
        history.record(Message::tool_results(results));
        // Tool-hook stdout follows the results it commented on, as extra
        // user-message context.
        for text in std::mem::take(&mut *ctx.hook_context.lock().unwrap_or_else(|e| e.into_inner()))
        {
            history.record(Message::user_text(text));
        }
        if cancel.is_cancelled() {
            return TurnOutcome {
                reason: EndReason::Aborted,
                final_text: String::new(),
                rounds: round + 1,
                structured_output: None,
            };
        }
        if let Some(value) = structured_output {
            if drain_inbox(&cfg.inbox, history, ui) {
                continue;
            }
            if !cfg.local_agent.can_finish_naturally() {
                continue;
            }
            return TurnOutcome {
                reason: EndReason::Completed,
                final_text: String::new(),
                rounds: round + 1,
                structured_output: Some(value),
            };
        }
        if options.structured_schema.is_some() {
            structured_failures += 1;
            if structured_failures >= 3 {
                return TurnOutcome {
                    reason: EndReason::Error(
                        "valid structured output was not produced after 3 attempts".into(),
                    ),
                    final_text: String::new(),
                    rounds: round + 1,
                    structured_output: None,
                };
            }
            history.record(Message::user_text(crate::structured_output::nudge()));
        }
    }
}

async fn dispatch_structured_tools(
    tool_uses: Vec<(String, String, Value)>,
    ctx: &ToolCtx,
    schema: &Value,
) -> (Vec<ContentBlock>, Option<Value>) {
    let tool_uses = crate::tools::normalize_tool_uses(tool_uses);
    let mut results: Vec<Option<ContentBlock>> = (0..tool_uses.len()).map(|_| None).collect();
    let mut ordinary_positions = Vec::new();
    let mut ordinary_calls = Vec::new();
    let mut accepted = None;

    for (position, (id, name, input)) in tool_uses.into_iter().enumerate() {
        if name == crate::structured_output::TOOL_NAME {
            let result = if accepted.is_some() {
                crate::structured_output::error_result(
                    id,
                    "only one structured_output call may complete a turn",
                )
            } else {
                match crate::structured_output::input_value(schema, &input).and_then(|value| {
                    crate::structured_output::validate_value(schema, value).map(|()| value.clone())
                }) {
                    Ok(value) => {
                        accepted = Some(value);
                        crate::structured_output::success_result(id)
                    }
                    Err(error) => crate::structured_output::error_result(id, &error),
                }
            };
            results[position] = Some(result);
        } else {
            ordinary_positions.push(position);
            ordinary_calls.push((id, name, input));
        }
    }

    let ordinary_results = dispatch_tools(ordinary_calls, ctx).await;
    for (position, result) in ordinary_positions.into_iter().zip(ordinary_results) {
        results[position] = Some(result);
    }

    (
        results
            .into_iter()
            .map(|result| result.expect("every tool use has one result"))
            .collect(),
        accepted,
    )
}

/// Defense in depth at the history/dispatch boundary. Adapters already enforce
/// these rules; core repeats them so no provider implementation can construct
/// replayable or dispatchable state outside the canonical assistant contract.
fn validate_assistant_result(
    outcome: &AssistantOutcome,
    blocks: &[ContentBlock],
) -> Result<(), String> {
    let mut tool_ids = HashSet::new();
    let mut has_tool = false;
    for block in blocks {
        match block {
            ContentBlock::Text { text } if text.is_empty() => {
                return Err("provider returned an empty assistant text block".into())
            }
            ContentBlock::RedactedThinking { data } if data.is_empty() => {
                return Err("provider returned an empty redacted thinking block".into())
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } if thinking.is_empty() && signature.is_empty() => {
                return Err("provider returned an empty thinking block".into())
            }
            ContentBlock::ToolUse { id, name, input } => {
                has_tool = true;
                if id.is_empty() || name.is_empty() {
                    return Err("provider returned a tool call with an empty identity".into());
                }
                if !input.is_object() {
                    return Err(format!(
                        "provider returned non-object input for tool {name}"
                    ));
                }
                if !tool_ids.insert(id) {
                    return Err("provider returned duplicate tool call ids".into());
                }
            }
            ContentBlock::Image { .. } | ContentBlock::ToolResult { .. } => {
                return Err("provider returned a non-assistant output block".into())
            }
            ContentBlock::Text { .. }
            | ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. } => {}
        }
    }
    match (matches!(outcome, AssistantOutcome::ToolUse), has_tool) {
        (true, false) => Err("provider reported tool use without a tool call".into()),
        (false, true) => Err("provider returned a tool call for a non-tool outcome".into()),
        _ => Ok(()),
    }
}

/// All text blocks of a sampled response, in provider order. Used both to end
/// a turn and to preserve every cut-off segment before a continuation nudge.
fn text_content(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

const TRUNCATION_RECOVERY_LIMIT: u32 = 3;
const TRUNCATION_CONTINUE_MSG: &str = "Your previous response was cut off by the output token \
limit. Continue exactly where you left off; break the remaining work into smaller pieces.";

/// Drain the step-boundary injection queue into history as user messages, each
/// framed by its own kind ([`InboxItem::into_message`]: steering, detached-task
/// results, or a background-shell terminal notification). Returns true if anything was injected. Called only at
/// round boundaries (top of the loop, and just before the turn would end) —
/// never mid-request, so an in-flight sampling never sees a partial write and
/// tool_result blocks are never interleaved with the injected user message.
fn drain_inbox(inbox: &Inbox, history: &mut History, ui: &Arc<dyn Ui>) -> bool {
    let pending = inbox.drain();
    if pending.is_empty() {
        return false;
    }
    for item in pending {
        let item = match item {
            crate::inbox::InboxItem::SubAgentResult { label, summary } => {
                crate::inbox::InboxItem::SubAgentResult {
                    label,
                    summary: history.offload_text(summary),
                }
            }
            crate::inbox::InboxItem::ProgramResult {
                label,
                run_id,
                summary,
            } => crate::inbox::InboxItem::ProgramResult {
                label,
                run_id,
                summary: history.offload_text(summary),
            },
            other => other,
        };
        match &item {
            crate::inbox::InboxItem::ScheduledPrompt {
                id,
                origin,
                scheduled_for_ms,
                reason,
                missed,
                ..
            } => ui.emit(&Event::ScheduledTaskUpdated(crate::event::ScheduledTask {
                id: id.clone(),
                origin: match origin {
                    crate::inbox::ScheduledOrigin::Cron => crate::event::ScheduledTaskOrigin::Cron,
                    crate::inbox::ScheduledOrigin::LoopWakeup => {
                        crate::event::ScheduledTaskOrigin::LoopWakeup
                    }
                },
                status: crate::event::ScheduledTaskStatus::Fired,
                scheduled_for_ms: Some(*scheduled_for_ms),
                reason: reason.clone(),
                detail: (*missed).then(|| "missed while inactive; confirmation required".into()),
            })),
            crate::inbox::InboxItem::SchedulerFailure { summary } => {
                ui.emit(&Event::ScheduledTaskUpdated(crate::event::ScheduledTask {
                    id: "scheduler".into(),
                    origin: crate::event::ScheduledTaskOrigin::Cron,
                    status: crate::event::ScheduledTaskStatus::Failed,
                    scheduled_for_ms: None,
                    reason: None,
                    detail: Some(summary.clone()),
                }))
            }
            _ => {}
        }
        history.record(Message::user_text(item.into_message()));
    }
    true
}

fn drain_local_mailbox(cfg: &Config, history: &mut History, ui: &Arc<dyn Ui>) -> bool {
    let Some(batch) = cfg.local_agent.claim_boundary() else {
        return false;
    };
    for item in batch.items().iter().cloned() {
        history.record(Message::user_text(item.into_message()));
    }
    batch.commit(ui);
    true
}

/// The plan-mode operating instructions, injected while the session is in plan
/// mode (plan 37): the hard gate blocks writes, this tells the model what to do
/// instead. Rides every depth — a sub-agent is read-only in plan mode too.
const PLAN_MODE_REMINDER: &str = "<plan-mode>\nThis session is in PLAN MODE. Only read-only \
exploration is allowed: read files, search, and run read-only commands to understand the task. \
Do NOT modify files or run commands with side effects — such calls are blocked by the permission \
gate. Produce a concrete, step-by-step plan for the requested change. When the plan is ready, \
call the exit_plan_mode tool with the full plan text to present it to the user; wait for their \
approval before making any changes.\n</plan-mode>";

/// The synthetic first user message: the plan-mode reminder (when in plan mode),
/// project instructions, the skills catalog, and the deferred-tools notice, in
/// that order. Project instructions and the skills catalog are session-stable;
/// a dynamic MCP catalog may replace the deferred-tools notice at a round
/// boundary, while the mode reminder changes only on explicit mode toggles. The
/// skills catalog rides only depth-0 requests (skills are a top-level feature;
/// see the `skill` tool registration in `turn_rounds`).
fn injected_context(
    cfg: &Config,
    workspace: &crate::config::EffectiveWorkspace,
    depth: u8,
) -> Option<String> {
    let plan_reminder = (workspace.permissions.mode() == crate::permissions::Mode::Plan)
        .then(|| PLAN_MODE_REMINDER.to_string());
    let skills_catalog = (depth == 0)
        .then(|| crate::skills::skills_catalog(&cfg.skills))
        .flatten();
    let parts: Vec<String> = [
        plan_reminder,
        cfg.project_instructions.clone(),
        skills_catalog,
        crate::tools::deferred_notice(cfg),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

#[cfg(test)]
mod tests;
