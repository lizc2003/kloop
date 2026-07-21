use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::config::Config;
use crate::event::Event;
use crate::history::History;
use crate::inbox::Inbox;
use crate::tools::all_tool_defs;
use crate::tools::dispatch_tools;
use crate::tools::ToolCtx;
use kloop_protocol::ContentBlock;
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
    // A sub-agent (agent_label set) fires subagent_start/subagent_stop instead
    // of pre_turn/post_turn — the split both cc and codex converge on (a
    // sub-agent's turn boundary is its own event, carrying its transcript and
    // result). The main agent keeps the plain turn hooks.
    let agent = &cfg.agent_label;
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
            };
        }
        crate::hooks::HookDecision::Allow { context } => {
            for text in context {
                history.record(Message::user_text(text));
            }
        }
    }
    let outcome = turn_rounds(cfg, history, ui, cancel, depth).await;
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
) -> TurnOutcome {
    let mut tools = all_tool_defs(
        depth,
        &cfg.tool_sources,
        cfg.defer_threshold,
        cfg.worktree_enabled,
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
    // At depth 0 the task tool exists; list the configured agent types in its
    // description so the model knows what it can dispatch to.
    if depth == 0 && !cfg.agent_types.is_empty() {
        if let Some(task) = tools.iter_mut().find(|t| t.name == "task") {
            task.description
                .push_str(&crate::agent_type::agent_types_hint(&cfg.agent_types));
        }
    }
    // A sub-agent's text is its deliverable and returns via the tool result;
    // streaming it to the main UI would interleave with the parent's output.
    let stream_text = depth == 0;
    let growth = compact::max_turn_growth(MAX_OUTPUT_TOKENS);
    // The injected context message is not part of history, so the overflow
    // prediction must account for it separately.
    let instructions_tokens = injected_context(cfg, depth).map_or(0, |s| s.len() as u64 / 4);
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
    for round in 0..cfg.max_rounds {
        // Step-boundary steering: deliver anything the user typed during the
        // previous round (tool execution / sampling) as a user message before
        // this round's request. At round 0 the queue is empty (the turn just
        // started) so this is a no-op. Never touches an in-flight request.
        drain_inbox(&cfg.inbox, history);
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
            stop_reason,
        } = match sample_with_retry(
            cfg,
            &active_model,
            history.messages(),
            &tools,
            ui,
            cancel,
            stream_text,
            depth,
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
                        }
                    }
                }
            }
            Sampled::Cancelled => {
                return TurnOutcome {
                    reason: EndReason::Aborted,
                    final_text: String::new(),
                    rounds: round,
                }
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
                };
            }
        };
        history.record(Message::assistant(blocks.clone()));
        if let Some(usage) = usage {
            // total() = uncached + cached input + output = full context size
            // at this request; anchors the char-heuristic estimate for items
            // recorded after this point.
            history.note_usage(usage.total());
        }

        let tool_uses: Vec<(String, String, Value)> = blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { id, name, input } => {
                    Some((id.clone(), name.clone(), input.clone()))
                }
                _ => None,
            })
            .collect();
        if tool_uses.is_empty() {
            // The turn would end here — but a steer that landed during this
            // final sampling must not be lost. Absorb it and keep going, so a
            // late "wait, also do X" is answered instead of dropped. (Steers
            // during tool execution are already delivered at the loop top.)
            if drain_inbox(&cfg.inbox, history) {
                continue;
            }
            // If the response was cut off by the output limit, ending would
            // strand it mid-thought. Nudge the model to continue, a bounded
            // number of times per turn. (A truncated response WITH tool calls
            // needs no special handling: the loop continues naturally and the
            // model resumes itself.)
            let round_text = last_text(&blocks);
            if is_truncated(stop_reason.as_deref())
                && truncation_recoveries < TRUNCATION_RECOVERY_LIMIT
            {
                truncation_recoveries += 1;
                // Keep the cut-off segment. A sub-agent (stream_text=false)
                // delivers ONLY through final_text, so without this a long
                // answer that overran the output limit would reach the parent
                // as just its tail — the front would be lost.
                truncated_prefix.push_str(&round_text);
                ui.emit(&Event::Note(format!(
                    "response truncated by output limit; asking the model to continue ({truncation_recoveries}/{TRUNCATION_RECOVERY_LIMIT})"
                )));
                history.record(Message::user_text(TRUNCATION_CONTINUE_MSG));
                continue;
            }
            let final_text = if truncated_prefix.is_empty() {
                round_text
            } else {
                format!("{truncated_prefix}{round_text}")
            };
            return TurnOutcome {
                reason: EndReason::Completed,
                final_text,
                rounds: round + 1,
            };
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
        let results = dispatch_tools(tool_uses, &ctx).await;
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
            };
        }
    }
    TurnOutcome {
        reason: EndReason::MaxRounds,
        final_text: String::new(),
        rounds: cfg.max_rounds,
    }
}

/// The response was cut off by the output token limit ("max_tokens" on the
/// Anthropic wire, "length" on OpenAI-compat). This is the one legitimate use
/// of stop_reason: not to decide continuation, but to detect an ungraceful
/// ending worth recovering from.
fn is_truncated(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, Some("max_tokens") | Some("length"))
}

/// The last text block of a sampled response — the assistant's answer for the
/// round. Used both to end a turn and to preserve the cut-off segment of a
/// truncated round before the continuation nudge.
fn last_text(blocks: &[ContentBlock]) -> String {
    blocks
        .iter()
        .rev()
        .find_map(|b| match b {
            ContentBlock::Text { text } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

const TRUNCATION_RECOVERY_LIMIT: u32 = 3;
const TRUNCATION_CONTINUE_MSG: &str = "Your previous response was cut off by the output token \
limit. Continue exactly where you left off; break the remaining work into smaller pieces.";

/// Drain the step-boundary injection queue into history as user messages, each
/// framed by its own kind ([`InboxItem::into_message`]: steering vs a background
/// sub-agent's result). Returns true if anything was injected. Called only at
/// round boundaries (top of the loop, and just before the turn would end) —
/// never mid-request, so an in-flight sampling never sees a partial write and
/// tool_result blocks are never interleaved with the injected user message.
fn drain_inbox(inbox: &Inbox, history: &mut History) -> bool {
    let pending = inbox.drain();
    if pending.is_empty() {
        return false;
    }
    for item in pending {
        history.record(Message::user_text(item.into_message()));
    }
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
/// that order. Every part but the mode reminder is session-stable, and the mode
/// toggles rarely, so the composed message is stable enough for the prompt-cache
/// prefix to survive across rounds. The skills catalog rides only depth-0
/// requests (skills are a top-level feature; see the `skill` tool registration
/// in `turn_rounds`).
fn injected_context(cfg: &Config, depth: u8) -> Option<String> {
    let plan_reminder = (cfg.effective_permissions().mode() == crate::permissions::Mode::Plan)
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
