use std::collections::HashSet;
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::config::Config;
use crate::event::Event;
use crate::execution_provenance::ExecutionRef;
use crate::history::History;
use crate::inbox::Inbox;
use crate::provider_route::FrozenProviderAttempt;
use crate::tools::ToolCtx;
use crate::tools::dispatch_tools;
use crate::usage::{ProviderUsageRecord, UsageOperation};
use kloop_protocol::AssistantOutcome;
use kloop_protocol::ContentBlock;
use kloop_protocol::MAX_OUTPUT_TOKENS;
use kloop_protocol::Message;

mod sampling;

pub use crate::rollout::TurnError;
use sampling::SampleOk;
use sampling::Sampled;
use sampling::sample_with_retry;

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
    Error(TurnError),
}

impl EndReason {
    pub fn terminal_status(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::MaxRounds => "maxRounds",
            Self::Aborted => "aborted",
            Self::Error(_) => "error",
        }
    }

    pub fn terminal_error(&self) -> Option<&TurnError> {
        match self {
            Self::Error(error) => Some(error),
            Self::Completed | Self::MaxRounds | Self::Aborted => None,
        }
    }
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

#[cfg(test)]
pub(crate) async fn run_structured_turn(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    schema: Value,
) -> TurnOutcome {
    run_structured_turn_in_context(cfg, history, ui, cancel, depth, schema, None).await
}

pub(crate) async fn run_structured_turn_in_execution(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    schema: Value,
    execution: ExecutionRef,
) -> TurnOutcome {
    run_structured_turn_in_context(cfg, history, ui, cancel, depth, schema, Some(execution)).await
}

#[allow(clippy::too_many_arguments)]
async fn run_structured_turn_in_context(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    schema: Value,
    enclosing_execution: Option<ExecutionRef>,
) -> TurnOutcome {
    if let Err(error) = crate::structured_output::validate_schema(&schema) {
        return TurnOutcome {
            reason: EndReason::Error(format!("{error:#}").into()),
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
        enclosing_execution,
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
    run_turn_with_options(
        cfg,
        history,
        ui,
        cancel,
        depth,
        TurnOptions::default(),
        None,
    )
    .await
}

pub(crate) async fn run_turn_in_execution(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    execution: ExecutionRef,
) -> TurnOutcome {
    run_turn_with_options(
        cfg,
        history,
        ui,
        cancel,
        depth,
        TurnOptions::default(),
        Some(execution),
    )
    .await
}

async fn run_turn_with_options(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    options: TurnOptions,
    enclosing_execution: Option<ExecutionRef>,
) -> TurnOutcome {
    if let Err(error) = history.ensure_initial_provider_route(&cfg.provider_route) {
        return TurnOutcome {
            reason: EndReason::Error(
                format!("provider route initialization failed: {error}").into(),
            ),
            final_text: String::new(),
            rounds: 0,
            structured_output: None,
        };
    }
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
                reason: EndReason::Error(format!("turn blocked by {which} hook: {reason}").into()),
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
    let outcome = turn_rounds(
        cfg,
        history,
        ui,
        cancel,
        depth,
        &options,
        enclosing_execution.as_ref(),
    )
    .await;
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

fn specialize_run_agent_def(
    tools: &mut [kloop_protocol::ToolDef],
    agent_types: &[crate::agent_type::AgentType],
) {
    let Some(run_agent) = tools.iter_mut().find(|tool| tool.name == "run_agent") else {
        return;
    };
    let properties = run_agent.schema["properties"]
        .as_object_mut()
        .expect("run_agent schema properties are an object");
    if agent_types.is_empty() {
        properties.remove("agent_type");
        return;
    }
    let agent_type = properties
        .get_mut("agent_type")
        .expect("run_agent schema declares agent_type");
    agent_type["enum"] = Value::Array(
        std::iter::once(Value::Null)
            .chain(
                agent_types
                    .iter()
                    .map(|agent_type| Value::String(agent_type.name.clone())),
            )
            .collect(),
    );
    run_agent
        .description
        .push_str(&crate::agent_type::agent_types_hint(agent_types));
}

async fn turn_rounds(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    options: &TurnOptions,
    enclosing_execution: Option<&ExecutionRef>,
) -> TurnOutcome {
    let build_tools = ||
     -> std::result::Result<
        (
            Vec<kloop_protocol::ToolDef>,
            Arc<crate::tools::ProgramToolManifest>,
        ),
        String,
    > {
        for _ in 0..8 {
            let before = crate::tools::capture_program_tool_manifest(
                &cfg.tool_sources,
                &cfg.shell_programs,
            );
            let mut tools = crate::tools::all_tool_defs(
                depth,
                &cfg.tool_sources,
                cfg.defer_threshold,
                cfg.surface,
                &cfg.shell_programs,
            );
            let after = crate::tools::capture_program_tool_manifest(
                &cfg.tool_sources,
                &cfg.shell_programs,
            );
            if !before.is_consistent() || !after.is_consistent() || before != after {
                continue;
            }
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
            // (None) keeps them all.
            if cfg.tool_allowlist.is_some() {
                let allow = cfg.tool_allowlist.as_deref();
                tools.retain(|t| crate::agent_type::tool_available(allow, &t.name));
            }
            if let Some(schema) = &options.structured_schema {
                // Appended after agent-type filtering: this is an internal completion
                // protocol, never a user-configurable capability or ordinary tool.
                tools.push(crate::structured_output::tool_def(schema));
            }
            if depth == 0 {
                specialize_run_agent_def(&mut tools, &cfg.agent_types);
            }
            return Ok((tools, Arc::new(after)));
        }
        Err("tool catalog changed repeatedly while building the provider request; retry the turn"
            .into())
    };
    let (mut tools, mut program_tool_manifest) = match build_tools() {
        Ok(built) => built,
        Err(error) => {
            return TurnOutcome {
                reason: EndReason::Error(error.into()),
                final_text: String::new(),
                rounds: 0,
                structured_output: None,
            };
        }
    };
    // A sub-agent's text is its deliverable and returns via the tool result;
    // streaming it to the main UI would interleave with the parent's output.
    let stream_text = depth == 0;
    let growth = compact::max_turn_growth(MAX_OUTPUT_TOKENS);
    // Overflow is recovered at most once per turn: compact, then retry. A
    // second overflow after a successful compaction surfaces as an error.
    let mut overflow_compact_attempted = false;
    // Freeze the complete route once. Every round, compaction and child
    // admission in this operation derives attempts from this same snapshot.
    let frozen_route = cfg.provider_route.clone();
    let mut active_attempt = frozen_route.primary_attempt();
    let mut truncation_recoveries = 0u32;
    // Cut-off text from truncated rounds, prepended to the final answer so a
    // truncated-then-continued turn returns the whole deliverable.
    let mut truncated_prefix = String::new();
    // Turn-unique counter for streamed assistant/reasoning item ids (`msg-N`,
    // `reasoning-N`): owned here so ids don't reset each round.
    let mut item_seq = 0u64;
    let mut rounds = 0;
    let mut structured_failures = 0usize;
    // Everything the assistant said across the turn's rounds. Every exit inside
    // the loop hands this back, because the caller — a parent agent, most of all
    // — receives only `final_text`, and an empty string there is indistinguishable
    // from "produced nothing". History and the UI already hold the work; this is
    // the one channel that used to drop it. `MaxRounds` was the instance that got
    // measured; enumerating the exits found seven more of the same shape, so the
    // rule is now uniform: an early exit never spells its result `String::new()`.
    // (Exits *before* the loop legitimately do — nothing has been produced yet.)
    let mut produced_text = String::new();
    // How many times a retryable stream error was resumed in this turn. Bounded
    // so a persistently failing upstream still terminates the turn.
    let mut stream_resumes = 0u32;
    let ending = 'turn: loop {
        if cfg.max_rounds.is_some_and(|limit| rounds >= limit) {
            break 'turn Ending {
                reason: EndReason::MaxRounds,
                // The default is exactly right here: hand back what was produced.
                text: None,
                rounds,
                structured: None,
            };
        }
        if rounds > 0 {
            // MCP list_changed publishes a new source generation between
            // sampling rounds. Never mutate an in-flight request; rebuild the
            // next round from one fresh source snapshot instead.
            match build_tools() {
                Ok((next_tools, next_manifest)) => {
                    tools = next_tools;
                    program_tool_manifest = next_manifest;
                }
                Err(error) => {
                    break 'turn Ending {
                        reason: EndReason::Error(error.into()),
                        text: None,
                        rounds,
                        structured: None,
                    };
                }
            }
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
        if let Some(window) = cfg.context_window
            && history.messages().len() >= 2
            && compact::predicted_overflow(
                history.estimated_tokens() + instructions_tokens,
                growth,
                // A configured window is a claim; a rejection already observed
                // this session is a measurement, and it wins.
                history.effective_window(window),
            )
        {
            ui.emit(&Event::Note(
                "predicted context overflow; compacting history".into(),
            ));
            let compaction = compact::compact_once(
                cfg,
                &active_attempt,
                compact::CompactionTrigger::Predictive,
                history,
                cancel,
            )
            .await;
            match compaction {
                Ok(compact::CompactionOutcome::Applied(receipt)) => ui.emit(&Event::Note(format!(
                    "history compacted: {} summarized, {} kept verbatim",
                    receipt.summarized, receipt.kept
                ))),
                Ok(compact::CompactionOutcome::NoOp(_)) => {}
                Err(e) => {
                    if cancel.is_cancelled() {
                        break 'turn Ending {
                            reason: EndReason::Aborted,
                            text: None,
                            rounds: round,
                            structured: None,
                        };
                    }
                    // Predictive failure is not fatal: fall through and let
                    // the request itself succeed or overflow reactively.
                    ui.emit(&Event::Note(format!("predictive compaction failed: {e:#}")));
                }
            }
        }

        let SampleOk {
            blocks,
            usage,
            outcome,
        } = match sample_with_retry(
            cfg,
            &active_attempt,
            history,
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
                    break 'turn Ending {
                        reason: EndReason::Error(
                            "context window exceeded (compaction unavailable or already tried)"
                                .into(),
                        ),
                        text: None,
                        rounds: round,
                        structured: None,
                    };
                }
                overflow_compact_attempted = true;
                ui.emit(&Event::Note(
                    "context window exceeded; compacting and retrying".into(),
                ));
                let compaction = compact::compact_once(
                    cfg,
                    &active_attempt,
                    compact::CompactionTrigger::Reactive,
                    history,
                    cancel,
                )
                .await;
                match compaction {
                    Ok(compact::CompactionOutcome::Applied(receipt)) => {
                        ui.emit(&Event::Note(compact::describe(&receipt)));
                        continue;
                    }
                    Ok(compact::CompactionOutcome::NoOp(_)) => {
                        break 'turn Ending {
                            reason: EndReason::Error("reactive compaction made no changes".into()),
                            text: None,
                            rounds: round,
                            structured: None,
                        };
                    }
                    Err(e) => {
                        break 'turn Ending {
                            reason: if cancel.is_cancelled() {
                                EndReason::Aborted
                            } else {
                                EndReason::Error(
                                    format!("reactive compaction failed: {e:#}").into(),
                                )
                            },
                            text: None,
                            rounds: round,
                            structured: None,
                        };
                    }
                }
            }
            Sampled::Cancelled { partial } => {
                let final_text = text_content(&partial);
                record_provider_assistant(history, &active_attempt, partial);
                break 'turn Ending {
                    reason: EndReason::Aborted,
                    text: Some(final_text),
                    rounds: round,
                    structured: None,
                };
            }
            Sampled::Partial { error, blocks } => {
                let round_text = text_content(&blocks);
                let partial_landed = !blocks.is_empty();
                record_provider_assistant(history, &active_attempt, blocks);
                append_produced(&mut produced_text, &round_text);
                // The stream died mid-response after the model had already said
                // something. Sampling cannot retry that — replaying the same
                // request would re-emit what the user has seen — but continuing
                // is a different move: `blocks` is the replayable partial (no
                // unsigned reasoning, no tool call), so what just landed is a
                // well-formed assistant turn, and the next request carries it as
                // context instead of repeating it. That costs one round where
                // ending the turn costs the whole turn. It also stays on the same
                // attempt: no fallback switch, nothing the user saw sent twice.
                //
                // Empty means nothing landed — a complete-but-undispatched tool
                // call, say. "Continue where you left off" with no assistant turn
                // to continue from is worse than ending here.
                if partial_landed && error.is_retryable() && stream_resumes < STREAM_RESUME_LIMIT {
                    stream_resumes += 1;
                    ui.emit(&Event::Note(format!(
                        "stream interrupted after partial output; continuing from it ({stream_resumes}/{STREAM_RESUME_LIMIT}): {error}"
                    )));
                    // Same carry as a truncated round, and for the same reason:
                    // the model is told to continue where it stopped, so the next
                    // round returns only the remainder. Without this the resumed
                    // half is the whole answer the caller sees.
                    truncated_prefix.push_str(&round_text);
                    history.record(Message::user_text(STREAM_RESUME_MSG));
                    continue;
                }
                break 'turn Ending {
                    reason: EndReason::Error(TurnError::ProviderFailure(error)),
                    text: Some(format!("{truncated_prefix}{round_text}")),
                    rounds: round,
                    structured: None,
                };
            }
            Sampled::Terminal(error) => {
                break 'turn Ending {
                    reason: EndReason::Error(TurnError::ProviderFailure(error)),
                    text: None,
                    rounds: round,
                    structured: None,
                };
            }
            Sampled::Failed(error) => {
                if active_attempt.identity().attempt_kind
                    == kloop_protocol::ProviderAttemptKind::Primary
                    && let Some(fallback) = frozen_route.fallback_attempt()
                {
                    ui.emit(&Event::Note(format!(
                        "sampling failed on {}; switching to fallback model {}: {error}",
                        active_attempt.model(),
                        fallback.model()
                    )));
                    active_attempt = fallback;
                    continue;
                }
                break 'turn Ending {
                    reason: EndReason::Error(TurnError::ProviderFailure(error)),
                    text: None,
                    rounds: round,
                    structured: None,
                };
            }
        };
        if let Err(error) = validate_assistant_result(&outcome, &blocks) {
            break 'turn Ending {
                reason: EndReason::Error(error.into()),
                text: None,
                rounds: round + 1,
                structured: None,
            };
        }
        if let Some(usage) = usage {
            history.record_provider_usage(ProviderUsageRecord::from_attempt(
                active_attempt.identity(),
                UsageOperation::Sampling,
                usage,
            ));
        }
        record_provider_assistant(history, &active_attempt, blocks.clone());
        append_produced(&mut produced_text, &text_content(&blocks));
        if let Some(usage) = usage {
            // total() = uncached + cached input + output = full context size
            // at this request; anchors the char-heuristic estimate for items
            // recorded after this point.
            history.note_usage(usage.total());
        }
        // Publish the context size every round, not just at turn end: an
        // agentic turn runs for minutes and its gauge is watched while it runs
        // (a first turn would otherwise sit at the session's opening 0 the
        // whole time). A sub-agent's History is its own, so only the main
        // loop's estimate describes the session.
        if depth == 0 {
            ui.emit(&Event::Usage(history.estimated_tokens()));
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
            AssistantOutcome::Refused
            | AssistantOutcome::Filtered
            | AssistantOutcome::Incomplete(_) => {
                break 'turn Ending {
                    reason: EndReason::Error(TurnError::ProviderOutcome(outcome.clone())),
                    text: Some(text_content(&blocks)),
                    rounds: round + 1,
                    structured: None,
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
                break 'turn Ending {
                    reason: EndReason::Error(TurnError::ProviderOutcome(outcome.clone())),
                    text: Some(final_text),
                    rounds: round + 1,
                    structured: None,
                };
            }
            AssistantOutcome::EndTurn => {
                if options.structured_schema.is_some() {
                    structured_failures += 1;
                    if structured_failures >= 3 {
                        break 'turn Ending {
                            reason: EndReason::Error(
                                "structured output was not produced after 3 attempts".into(),
                            ),
                            text: None,
                            rounds: round + 1,
                            structured: None,
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
                break 'turn Ending {
                    reason: EndReason::Completed,
                    text: Some(final_text),
                    rounds: round + 1,
                    structured: None,
                };
            }
            AssistantOutcome::ToolUse => {}
        }
        let ctx = ToolCtx {
            cfg: cfg.clone(),
            ui: ui.clone(),
            cancel: cancel.clone(),
            depth,
            enclosing_execution: enclosing_execution.cloned(),
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: Some(Arc::clone(&program_tool_manifest)),
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
            break 'turn Ending {
                reason: EndReason::Aborted,
                text: None,
                rounds: round + 1,
                structured: None,
            };
        }
        if let Some(value) = structured_output {
            if drain_inbox(&cfg.inbox, history, ui) {
                continue;
            }
            if !cfg.local_agent.can_finish_naturally() {
                continue;
            }
            break 'turn Ending {
                reason: EndReason::Completed,
                text: None,
                rounds: round + 1,
                structured: Some(value),
            };
        }
        if options.structured_schema.is_some() {
            structured_failures += 1;
            if structured_failures >= 3 {
                break 'turn Ending {
                    reason: EndReason::Error(
                        "valid structured output was not produced after 3 attempts".into(),
                    ),
                    text: None,
                    rounds: round + 1,
                    structured: None,
                };
            }
            history.record(Message::user_text(crate::structured_output::nudge()));
        }
    };

    // The turn's one exit. Everything above decides *why* it ended; only here is
    // the result assembled, and only here does `final_text` get a value — so an
    // exit that says nothing about text hands back what the turn produced instead
    // of an empty string. Exits *before* the loop still return directly: nothing
    // has been produced yet, and there is nothing to lose.
    TurnOutcome {
        reason: ending.reason,
        final_text: ending.text.unwrap_or(produced_text),
        rounds: ending.rounds,
        structured_output: ending.structured,
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
                return Err("provider returned an empty assistant text block".into());
            }
            ContentBlock::RedactedThinking { data } if data.is_empty() => {
                return Err("provider returned an empty redacted thinking block".into());
            }
            ContentBlock::Thinking {
                thinking,
                signature,
            } if thinking.is_empty() && signature.is_empty() => {
                return Err("provider returned an empty thinking block".into());
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
                return Err("provider returned a non-assistant output block".into());
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

fn record_provider_assistant(
    history: &mut History,
    attempt: &FrozenProviderAttempt,
    blocks: Vec<ContentBlock>,
) {
    if blocks.is_empty() {
        return;
    }
    history.record_provider_assistant(blocks, attempt);
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

/// Cap on resuming a turn after a retryable mid-response stream failure. Bounds
/// a flapping upstream: each resume costs a round, and a stream that keeps dying
/// is a real outage the turn should surface rather than grind against.
/// How one round decides the turn ends. Exists so the loop cannot build a
/// `TurnOutcome` itself: there is exactly one place that does, and it is the
/// place that knows what the turn produced. `text: None` means "hand back
/// everything the turn produced" and is the default an exit gets by saying
/// nothing; a site that means something narrower — the completed answer, a
/// truncated-then-continued deliverable — says so with `Some`. Writing
/// `Some(String::new())` is still possible, but now it is a visible claim that
/// this exit really has nothing, not an oversight.
struct Ending {
    reason: EndReason,
    text: Option<String>,
    rounds: usize,
    structured: Option<serde_json::Value>,
}

const STREAM_RESUME_LIMIT: u32 = 3;
const STREAM_RESUME_MSG: &str = "Your previous response was cut off by a transient \
connection failure, not by you. Continue exactly where you left off.";

/// Append one round's assistant text to the turn's running record, keeping the
/// rounds separated and skipping the (common) rounds that only issued tool calls.
fn append_produced(produced: &mut String, round_text: &str) {
    if round_text.trim().is_empty() {
        return;
    }
    if !produced.is_empty() {
        produced.push_str("\n\n");
    }
    produced.push_str(round_text);
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
