use std::collections::HashSet;
use std::collections::VecDeque;
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
use kloop_protocol::Message;

mod sampling;

pub use crate::rollout::TurnError;
use crate::rollout::TurnTerminal;
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
            Self::MaxRounds => "max_rounds",
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

    pub fn terminal(&self) -> TurnTerminal {
        TurnTerminal {
            status: self.terminal_status().into(),
            error: self.terminal_error().map(ToString::to_string),
            typed_error: self.terminal_error().cloned(),
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

/// Run a turn on `input` — the message the user just sent — staging it instead
/// of recording it. A turn the user interrupts before the model has produced
/// anything never happened: `items` and the session file are left exactly as
/// they were, no terminal line is written, and the input comes back as
/// `Some(message)` for the front-end to put in front of the user again. That is
/// what makes "I mistyped, esc" leave nothing behind to answer later.
///
/// Anything else — the model said a word, a tool ran, the provider failed, a
/// hook blocked the turn — records the input on its way, and the second element
/// is `None`.
pub async fn run_turn_with_input(
    cfg: &Arc<Config>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    depth: u8,
    input: Message,
) -> (TurnOutcome, Option<Message>) {
    history.stage(input);
    let outcome = run_turn_with_options(
        cfg,
        history,
        ui,
        cancel,
        depth,
        TurnOptions::default(),
        None,
    )
    .await;
    // The input is the first thing staged, so it is the first thing back. Hook
    // context staged behind it is discarded with the turn that asked for it.
    let returned = history.take_staged().into_iter().next();
    (outcome, returned)
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

/// Every exit records the turn's terminal state, so a rollout always says why
/// the turn stopped — including the two exits that never reach the loop. Front
/// ends must not record it themselves: they would write a second line for the
/// same turn.
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
        let reason =
            EndReason::Error(format!("provider route initialization failed: {error}").into());
        history.record_turn_terminal(reason.terminal());
        return TurnOutcome {
            reason,
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
    // The type run_agent dispatched to, read once from the live directory: a
    // sub-agent registers before its turn task is spawned and its lease outlives
    // the turn, so both hook points see the same value. The label ("agent-N") is
    // a spawn counter, never the type.
    let agent_type = if agent.is_empty() {
        None
    } else {
        cfg.agent_type()
    };
    let start = if agent.is_empty() {
        cfg.hooks.pre_turn(&cfg.session_id, ui.as_ref()).await
    } else {
        cfg.hooks
            .subagent_start(&cfg.session_id, agent, agent_type.as_deref(), ui.as_ref())
            .await
    };
    match start {
        crate::hooks::HookDecision::Block { reason } => {
            let which = if agent.is_empty() {
                "pre_turn"
            } else {
                "subagent_start"
            };
            let blocked =
                EndReason::Error(format!("turn blocked by {which} hook: {reason}").into());
            history.record_turn_terminal(blocked.terminal());
            return TurnOutcome {
                reason: blocked,
                final_text: String::new(),
                rounds: 0,
                structured_output: None,
            };
        }
        crate::hooks::HookDecision::Allow { context } => {
            for text in context {
                // Staged behind the turn's input, not recorded: context for a
                // turn that never runs is context for nothing, and recording it
                // here would also commit the input this turn may yet give back.
                history.stage(Message::user_text(text));
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
                agent_type.as_deref(),
                transcript.as_deref(),
                &outcome.final_text,
                ui.as_ref(),
            )
            .await
    };
    // A turn the user interrupted before the model produced anything is a turn
    // that did not happen. Its input is still staged — nothing has been written
    // all turn, because every write commits the stage first — so the session
    // file has no record of it, and writing a terminal (or the stop hook's
    // context) here would both commit that input and leave an `aborted` line
    // standing for a turn with no content. Two interrupts stay on the recorded
    // path: an `Error` is worth keeping ("this one failed" is history), and an
    // interrupt with steering queued behind it must keep the message that
    // steering answers.
    if outcome.reason == EndReason::Aborted && history.has_staged() && cfg.inbox.is_empty() {
        return outcome;
    }
    for text in stop_context {
        history.record(Message::user_text(text));
    }
    // The turn's last rollout line. Every message this turn produced — the stop
    // hook's injected context included — is already recorded, so the terminal is
    // a true separator and not a marker some later write can slip behind.
    history.record_turn_terminal(outcome.reason.terminal());
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

/// Build this round's tool array and the program-tool manifest it was built
/// from. Retried until one source snapshot survives the whole build: a dynamic
/// source (MCP `list_changed`) may publish a new catalog mid-build, and a
/// request assembled from two different catalogs is not a request either of
/// them would have answered.
fn build_tools(
    cfg: &Arc<Config>,
    depth: u8,
    options: &TurnOptions,
) -> std::result::Result<
    (
        Vec<kloop_protocol::ToolDef>,
        Arc<crate::tools::ProgramToolManifest>,
    ),
    String,
> {
    for _ in 0..8 {
        let before =
            crate::tools::capture_program_tool_manifest(&cfg.tool_sources, &cfg.shell_programs);
        let mut tools = crate::tools::all_tool_defs(
            depth,
            &cfg.tool_sources,
            cfg.defer_threshold,
            cfg.surface,
            &cfg.shell_programs,
        );
        let after =
            crate::tools::capture_program_tool_manifest(&cfg.tool_sources, &cfg.shell_programs);
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
        let has_model_skill = cfg.skills.iter().any(|s| s.source.model_invocable());
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
    Err(
        "tool catalog changed repeatedly while building the provider request; retry the turn"
            .into(),
    )
}

/// What a round phase decided about the rest of the turn.
enum RoundStep {
    /// Something was recorded that the model has to see; sample again.
    Retry,
    /// The turn is over.
    Stop(Ending),
}

/// One turn's state across its rounds. Every phase below takes it as `&mut
/// self` instead of a dozen positional borrows, and the loop in
/// [`turn_rounds`] is then only the order the phases run in.
struct Turn<'a> {
    cfg: &'a Arc<Config>,
    history: &'a mut History,
    ui: &'a Arc<dyn Ui>,
    cancel: &'a CancellationToken,
    depth: u8,
    options: &'a TurnOptions,
    enclosing_execution: Option<&'a ExecutionRef>,
    /// The complete route, frozen once. Every round, compaction and child
    /// admission in this operation derives attempts from this same snapshot.
    active_attempt: FrozenProviderAttempt,
    /// Per rail, not per build: the Responses/Chat cap is four times the
    /// Anthropic one, and a round that may produce four times the output has to
    /// reserve for it.
    growth: u64,
    /// A sub-agent's text is its deliverable and returns via the tool result;
    /// streaming it to the main UI would interleave with the parent's output.
    stream_text: bool,
    tools: Vec<kloop_protocol::ToolDef>,
    program_tool_manifest: Arc<crate::tools::ProgramToolManifest>,
    /// Overflow is recovered at most once per turn: compact, then retry. A
    /// second overflow after a successful compaction surfaces as an error.
    overflow_compact_attempted: bool,
    truncation_recoveries: u32,
    /// How many times a retryable stream error was resumed in this turn.
    /// Bounded so a persistently failing upstream still terminates the turn.
    stream_resumes: u32,
    /// Cut-off text from truncated rounds, prepended to the final answer so a
    /// truncated-then-continued turn returns the whole deliverable.
    truncated_prefix: String,
    /// Turn-unique counter for streamed assistant/reasoning item ids (`msg-N`,
    /// `reasoning-N`): owned here so ids don't reset each round.
    item_seq: u64,
    rounds: usize,
    structured_failures: usize,
    /// Everything the assistant said across the turn's rounds. Every exit inside
    /// the loop hands this back, because the caller — a parent agent, most of all
    /// — receives only `final_text`, and an empty string there is indistinguishable
    /// from "produced nothing". History and the UI already hold the work; this is
    /// the one channel that used to drop it. `MaxRounds` was the instance that got
    /// measured; enumerating the exits found seven more of the same shape, so the
    /// rule is now uniform: an early exit never spells its result `String::new()`.
    /// (Exits *before* the loop legitimately do — nothing has been produced yet.)
    produced_text: String,
}

impl Turn<'_> {
    /// MCP list_changed publishes a new source generation between sampling
    /// rounds. Never mutate an in-flight request; rebuild the next round from
    /// one fresh source snapshot instead.
    fn refresh_tools(&mut self) -> Result<(), Ending> {
        match build_tools(self.cfg, self.depth, self.options) {
            Ok((tools, manifest)) => {
                self.tools = tools;
                self.program_tool_manifest = manifest;
                Ok(())
            }
            Err(error) => Err(Ending {
                reason: EndReason::Error(error.into()),
                text: None,
                rounds: self.rounds,
                structured: None,
            }),
        }
    }

    /// Predictive: compact BEFORE sampling when this round's estimated growth
    /// would overflow the window — don't wait to be rejected.
    async fn compact_predictively(
        &mut self,
        estimated_tokens: u64,
        round: usize,
    ) -> Option<Ending> {
        let window = self.cfg.context_window?;
        if self.history.messages().len() < 2
            || !compact::predicted_overflow(
                estimated_tokens,
                self.growth,
                // A configured window is a claim; a rejection already observed
                // this session is a measurement, and it wins.
                self.history.effective_window(window),
            )
        {
            return None;
        }
        self.ui.emit(&Event::Note(
            "predicted context overflow; compacting history".into(),
        ));
        let compaction = compact::compact_once(
            self.cfg,
            &self.active_attempt,
            compact::CompactionTrigger::Predictive,
            self.history,
            self.cancel,
        )
        .await;
        match compaction {
            Ok(compact::CompactionOutcome::Applied(receipt)) => {
                self.ui.emit(&Event::Note(format!(
                    "history compacted: {} summarized, {} kept verbatim",
                    receipt.summarized, receipt.kept
                )));
                None
            }
            Ok(compact::CompactionOutcome::NoOp(_)) => None,
            Err(e) => {
                if self.cancel.is_cancelled() {
                    return Some(Ending {
                        reason: EndReason::Aborted,
                        text: None,
                        rounds: round,
                        structured: None,
                    });
                }
                // Predictive failure is not fatal: fall through and let
                // the request itself succeed or overflow reactively.
                self.ui
                    .emit(&Event::Note(format!("predictive compaction failed: {e:#}")));
                None
            }
        }
    }

    /// Reactive: the provider rejected the request for size. Compact once per
    /// turn and retry; a second overflow is an error.
    async fn recover_from_overflow(&mut self, round: usize) -> RoundStep {
        if self.cfg.context_window.is_none() || self.overflow_compact_attempted {
            return RoundStep::Stop(Ending {
                reason: EndReason::Error(
                    "context window exceeded (compaction unavailable or already tried)".into(),
                ),
                text: None,
                rounds: round,
                structured: None,
            });
        }
        self.overflow_compact_attempted = true;
        self.ui.emit(&Event::Note(
            "context window exceeded; compacting and retrying".into(),
        ));
        let compaction = compact::compact_once(
            self.cfg,
            &self.active_attempt,
            compact::CompactionTrigger::Reactive,
            self.history,
            self.cancel,
        )
        .await;
        match compaction {
            Ok(compact::CompactionOutcome::Applied(receipt)) => {
                self.ui.emit(&Event::Note(compact::describe(&receipt)));
                RoundStep::Retry
            }
            Ok(compact::CompactionOutcome::NoOp(_)) => RoundStep::Stop(Ending {
                reason: EndReason::Error("reactive compaction made no changes".into()),
                text: None,
                rounds: round,
                structured: None,
            }),
            Err(e) => RoundStep::Stop(Ending {
                reason: if self.cancel.is_cancelled() {
                    EndReason::Aborted
                } else {
                    EndReason::Error(format!("reactive compaction failed: {e:#}").into())
                },
                text: None,
                rounds: round,
                structured: None,
            }),
        }
    }

    /// The stream died mid-response after the model had already said something.
    /// Sampling cannot retry that — replaying the same request would re-emit
    /// what the user has seen — but continuing is a different move: `blocks` is
    /// the replayable partial (no unsigned reasoning, no tool call), so what
    /// just landed is a well-formed assistant turn, and the next request
    /// carries it as context instead of repeating it. That costs one round
    /// where ending the turn costs the whole turn. It also stays on the same
    /// attempt: no fallback switch, nothing the user saw sent twice.
    ///
    /// Empty means nothing landed — a complete-but-undispatched tool call, say.
    /// "Continue where you left off" with no assistant turn to continue from is
    /// worse than ending here.
    fn resume_after_partial(
        &mut self,
        error: kloop_provider::ProviderFailure,
        blocks: Vec<ContentBlock>,
        round: usize,
    ) -> RoundStep {
        let round_text = text_content(&blocks);
        let partial_landed = !blocks.is_empty();
        record_provider_assistant(self.history, &self.active_attempt, blocks);
        append_produced(&mut self.produced_text, &round_text);
        if partial_landed && error.is_retryable() && self.stream_resumes < STREAM_RESUME_LIMIT {
            self.stream_resumes += 1;
            let resumes = self.stream_resumes;
            self.ui.emit(&Event::Note(format!(
                "stream interrupted after partial output; continuing from it ({resumes}/{STREAM_RESUME_LIMIT}): {error}"
            )));
            // Same carry as a truncated round, and for the same reason:
            // the model is told to continue where it stopped, so the next
            // round returns only the remainder. Without this the resumed
            // half is the whole answer the caller sees.
            self.truncated_prefix.push_str(&round_text);
            self.history.record(Message::user_text(STREAM_RESUME_MSG));
            return RoundStep::Retry;
        }
        RoundStep::Stop(Ending {
            reason: EndReason::Error(TurnError::ProviderFailure(error)),
            text: Some(format!("{}{round_text}", self.truncated_prefix)),
            rounds: round,
            structured: None,
        })
    }

    /// Turn a sampling verdict into this round's assistant output, or into the
    /// decision that ends (or restarts) the round.
    async fn settle_sample(
        &mut self,
        sampled: Sampled,
        round: usize,
    ) -> std::result::Result<SampleOk, RoundStep> {
        match sampled {
            Sampled::Ok(ok) => Ok(ok),
            Sampled::Overflow => Err(self.recover_from_overflow(round).await),
            Sampled::Cancelled {
                partial,
                produced_nothing,
            } => {
                let final_text = text_content(&partial);
                if !produced_nothing {
                    // The round produced something even if none of it is
                    // replayable, so the turn happened and this turn's input
                    // belongs in the conversation. `record_provider_assistant`
                    // below cannot do it: an empty partial records nothing.
                    self.history.commit_staged();
                }
                record_provider_assistant(self.history, &self.active_attempt, partial);
                Err(RoundStep::Stop(Ending {
                    reason: EndReason::Aborted,
                    text: Some(final_text),
                    rounds: round,
                    structured: None,
                }))
            }
            Sampled::Partial { error, blocks } => {
                Err(self.resume_after_partial(error, blocks, round))
            }
            Sampled::Terminal(error) | Sampled::Failed(error) => Err(RoundStep::Stop(Ending {
                reason: EndReason::Error(TurnError::ProviderFailure(error)),
                text: None,
                rounds: round,
                structured: None,
            })),
        }
    }

    /// Record one accepted round: check the provider's own claim against what
    /// it sent, ledger the usage, append the assistant message, and publish the
    /// running context estimate.
    fn absorb_round(
        &mut self,
        blocks: &[ContentBlock],
        usage: Option<kloop_protocol::Usage>,
        outcome: &AssistantOutcome,
        round: usize,
    ) -> Result<(), Ending> {
        // The round was accepted, so the turn is real and this turn's input
        // joins the conversation ahead of everything the round produced — the
        // usage line included, which records no place in the transcript and so
        // commits nothing itself. Explicit because an accepted round can have
        // nothing to record: an empty `blocks` appends no assistant message.
        self.history.commit_staged();
        if let Err(error) = validate_assistant_result(outcome, blocks) {
            return Err(Ending {
                reason: EndReason::Error(error.into()),
                text: None,
                rounds: round + 1,
                structured: None,
            });
        }
        if let Some(usage) = usage {
            self.history
                .record_provider_usage(ProviderUsageRecord::from_attempt(
                    self.active_attempt.identity(),
                    UsageOperation::Sampling,
                    usage,
                ));
        }
        record_provider_assistant(self.history, &self.active_attempt, blocks.to_vec());
        append_produced(&mut self.produced_text, &text_content(blocks));
        if let Some(usage) = usage {
            // total() = uncached + cached input + output = full context size
            // at this request; anchors the char-heuristic estimate for items
            // recorded after this point.
            self.history.note_usage(usage.total());
        }
        // Publish the context size every round, not just at turn end: an
        // agentic turn runs for minutes and its gauge is watched while it runs
        // (a first turn would otherwise sit at the session's opening 0 the
        // whole time). A sub-agent's History is its own, so only the main
        // loop's estimate describes the session.
        if self.depth == 0 {
            let estimated = estimate_request(self.history, || {
                let workspace = self.cfg.effective_workspace();
                request_overhead_tokens(
                    &workspace.system,
                    &self.tools,
                    injected_context(self.cfg, &workspace, self.depth).as_deref(),
                )
            });
            self.ui.emit(&Event::Usage(estimated));
        }
        Ok(())
    }

    /// What the provider said this round was. `None` means tool calls: the
    /// round goes on to dispatch them.
    fn classify_outcome(
        &mut self,
        outcome: &AssistantOutcome,
        blocks: &[ContentBlock],
        round: usize,
    ) -> Option<RoundStep> {
        match outcome {
            AssistantOutcome::Refused
            | AssistantOutcome::Filtered
            | AssistantOutcome::Incomplete(_) => Some(RoundStep::Stop(Ending {
                reason: EndReason::Error(TurnError::ProviderOutcome(outcome.clone())),
                text: Some(text_content(blocks)),
                rounds: round + 1,
                structured: None,
            })),
            AssistantOutcome::OutputLimit(_) => {
                let round_text = text_content(blocks);
                if self.truncation_recoveries < TRUNCATION_RECOVERY_LIMIT {
                    self.truncation_recoveries += 1;
                    self.truncated_prefix.push_str(&round_text);
                    let recoveries = self.truncation_recoveries;
                    self.ui.emit(&Event::Note(format!(
                        "response truncated by output limit; asking the model to continue ({recoveries}/{TRUNCATION_RECOVERY_LIMIT})"
                    )));
                    self.history
                        .record(Message::user_text(TRUNCATION_CONTINUE_MSG));
                    return Some(RoundStep::Retry);
                }
                Some(RoundStep::Stop(Ending {
                    reason: EndReason::Error(TurnError::ProviderOutcome(outcome.clone())),
                    text: Some(format!("{}{round_text}", self.truncated_prefix)),
                    rounds: round + 1,
                    structured: None,
                }))
            }
            AssistantOutcome::EndTurn => {
                if self.options.structured_schema.is_some() {
                    return Some(self.nudge_structured(
                        "structured output was not produced after 3 attempts",
                        round,
                    ));
                }
                // The turn would end here — but a steer that landed during this
                // final sampling must not be lost. Absorb it and keep going, so a
                // late "wait, also do X" is answered instead of dropped. (Steers
                // during tool execution are already delivered at the loop top.)
                if let Some(step) = self.keep_going_for_late_work() {
                    return Some(step);
                }
                let round_text = text_content(blocks);
                let final_text = if self.truncated_prefix.is_empty() {
                    round_text
                } else {
                    format!("{}{round_text}", self.truncated_prefix)
                };
                Some(RoundStep::Stop(Ending {
                    reason: EndReason::Completed,
                    text: Some(final_text),
                    rounds: round + 1,
                    structured: None,
                }))
            }
            AssistantOutcome::ToolUse => None,
        }
    }

    /// A steer that landed during this round, or a local agent that is not
    /// allowed to finish on its own, both mean the turn is not over.
    ///
    /// The inbox drain goes last and closes the steer window when it finds
    /// nothing: past it the turn only winds down, so a steer naming this turn
    /// must be refused rather than acknowledged and left for the next one.
    fn keep_going_for_late_work(&mut self) -> Option<RoundStep> {
        if !self.cfg.local_agent.can_finish_naturally() {
            drain_inbox(&self.cfg.inbox, self.history, self.ui);
            return Some(RoundStep::Retry);
        }
        let pending = self.cfg.inbox.drain_or_close_steer_window();
        if inject_pending(pending, self.history, self.ui) {
            return Some(RoundStep::Retry);
        }
        None
    }

    /// A structured turn that produced no structured output: ask again, three
    /// times, then give up with `failure`.
    fn nudge_structured(&mut self, failure: &str, round: usize) -> RoundStep {
        self.structured_failures += 1;
        if self.structured_failures >= 3 {
            return RoundStep::Stop(Ending {
                reason: EndReason::Error(failure.into()),
                text: None,
                rounds: round + 1,
                structured: None,
            });
        }
        self.history
            .record(Message::user_text(crate::structured_output::nudge()));
        RoundStep::Retry
    }

    /// Run this round's tool calls and fold their results into history.
    ///
    /// `invalid` names the calls whose arguments the model did not write as
    /// valid JSON. They are real `tool_use` blocks in `blocks` and each still
    /// needs its `tool_result`, but running one would mean running a command
    /// nobody wrote: they get the parser's complaint as a failed result and
    /// the model rewrites them next round.
    async fn dispatch_round(
        &mut self,
        blocks: &[ContentBlock],
        invalid: &[(String, String)],
        round: usize,
    ) -> RoundStep {
        let tool_uses: Vec<(String, String, Value)> = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::ToolUse { id, name, input } => {
                    (!invalid.iter().any(|(invalid_id, _)| invalid_id == id))
                        .then(|| (id.clone(), name.clone(), input.clone()))
                }
                ContentBlock::Text { .. }
                | ContentBlock::Thinking { .. }
                | ContentBlock::RedactedThinking { .. }
                | ContentBlock::Image { .. }
                | ContentBlock::ToolResult { .. } => None,
            })
            .collect();
        let ctx = ToolCtx {
            cfg: self.cfg.clone(),
            ui: self.ui.clone(),
            cancel: self.cancel.clone(),
            depth: self.depth,
            enclosing_execution: self.enclosing_execution.cloned(),
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: Some(Arc::clone(&self.program_tool_manifest)),
            // The assistant message carrying these tool_uses was just recorded,
            // so this is the id of the turn that a spawned sub-agent descends
            // from.
            parent_rollout_id: self.history.rollout_last_id().map(str::to_string),
            program_result: None,
        };
        let (results, structured_output) = match &self.options.structured_schema {
            Some(schema) => dispatch_structured_tools(tool_uses, &ctx, schema).await,
            None => (dispatch_tools(tool_uses, &ctx).await, None),
        };
        let results = merge_invalid_results(blocks, invalid, results);
        // Record results BEFORE checking cancellation so every tool_use has a
        // paired tool_result and history stays legal for the next request.
        self.history.record(Message::tool_results(results));
        // Tool-hook stdout follows the results it commented on, as extra
        // user-message context.
        for text in std::mem::take(&mut *ctx.hook_context.lock().unwrap_or_else(|e| e.into_inner()))
        {
            self.history.record(Message::user_text(text));
        }
        if self.cancel.is_cancelled() {
            return RoundStep::Stop(Ending {
                reason: EndReason::Aborted,
                text: None,
                rounds: round + 1,
                structured: None,
            });
        }
        if let Some(value) = structured_output {
            if let Some(step) = self.keep_going_for_late_work() {
                return step;
            }
            return RoundStep::Stop(Ending {
                reason: EndReason::Completed,
                text: None,
                rounds: round + 1,
                structured: Some(value),
            });
        }
        if self.options.structured_schema.is_some() {
            return self.nudge_structured(
                "valid structured output was not produced after 3 attempts",
                round,
            );
        }
        RoundStep::Retry
    }
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
    let (tools, program_tool_manifest) = match build_tools(cfg, depth, options) {
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
    let mut turn = Turn {
        cfg,
        history,
        ui,
        cancel,
        depth,
        options,
        enclosing_execution,
        active_attempt: cfg.provider_route.primary_attempt(),
        growth: compact::max_turn_growth(cfg.provider_route.api_family().max_output_tokens()),
        stream_text: depth == 0,
        tools,
        program_tool_manifest,
        overflow_compact_attempted: false,
        truncation_recoveries: 0,
        stream_resumes: 0,
        truncated_prefix: String::new(),
        item_seq: 0,
        rounds: 0,
        structured_failures: 0,
        produced_text: String::new(),
    };
    let ending = 'turn: loop {
        if cfg.max_rounds.is_some_and(|limit| turn.rounds >= limit) {
            break 'turn Ending {
                reason: EndReason::MaxRounds,
                // The default is exactly right here: hand back what was produced.
                text: None,
                rounds: turn.rounds,
                structured: None,
            };
        }
        if turn.rounds > 0
            && let Err(ending) = turn.refresh_tools()
        {
            break 'turn ending;
        }
        let round = turn.rounds;
        turn.rounds += 1;
        // Step-boundary steering: deliver anything the user typed during the
        // previous round (tool execution / sampling) as a user message before
        // this round's request. At round 0 the queue is empty (the turn just
        // started) so this is a no-op. Never touches an in-flight request.
        drain_inbox(&cfg.inbox, turn.history, ui);
        drain_local_mailbox(cfg, turn.history, ui);
        remind_todos(cfg, turn.history, depth);
        remind_changed_reads(cfg, turn.history);
        // Without an anchor the system prompt, this round's tools and the
        // injected context are estimated here; a dynamic MCP refresh may have
        // replaced the tools or the deferred-tool notice since last round.
        let workspace = cfg.effective_workspace();
        let estimated = estimate_request(turn.history, || {
            request_overhead_tokens(
                &workspace.system,
                &turn.tools,
                injected_context(cfg, &workspace, depth).as_deref(),
            )
        });
        if let Some(ending) = turn.compact_predictively(estimated, round).await {
            break 'turn ending;
        }
        let sampled = sample_with_retry(
            cfg,
            &turn.active_attempt,
            turn.history,
            &turn.tools,
            ui,
            cancel,
            turn.stream_text,
            depth,
            &workspace,
            &mut turn.item_seq,
        )
        .await;
        let SampleOk {
            blocks,
            usage,
            outcome,
            invalid_tool_inputs,
        } = match turn.settle_sample(sampled, round).await {
            Ok(ok) => ok,
            Err(RoundStep::Retry) => continue,
            Err(RoundStep::Stop(ending)) => break 'turn ending,
        };
        if let Err(ending) = turn.absorb_round(&blocks, usage, &outcome, round) {
            break 'turn ending;
        }
        let step = match turn.classify_outcome(&outcome, &blocks, round) {
            Some(step) => step,
            None => {
                turn.dispatch_round(&blocks, &invalid_tool_inputs, round)
                    .await
            }
        };
        match step {
            RoundStep::Retry => continue,
            RoundStep::Stop(ending) => break 'turn ending,
        }
    };

    // The turn's one exit. Everything above decides *why* it ended; only here is
    // the result assembled, and only here does `final_text` get a value — so an
    // exit that says nothing about text hands back what the turn produced instead
    // of an empty string. Exits *before* the loop still return directly: nothing
    // has been produced yet, and there is nothing to lose.
    TurnOutcome {
        reason: ending.reason,
        final_text: ending.text.unwrap_or(turn.produced_text),
        rounds: ending.rounds,
        structured_output: ending.structured,
    }
}

/// Put the failed results for unreadable calls back where their calls were.
/// Pairing is by id, but order is what a person reads in the transcript, so a
/// result sits next to the call it answers.
fn merge_invalid_results(
    blocks: &[ContentBlock],
    invalid: &[(String, String)],
    dispatched: Vec<ContentBlock>,
) -> Vec<ContentBlock> {
    if invalid.is_empty() {
        return dispatched;
    }
    let mut dispatched = VecDeque::from(dispatched);
    let mut merged = Vec::with_capacity(dispatched.len() + invalid.len());
    for block in blocks {
        let ContentBlock::ToolUse { id, .. } = block else {
            continue;
        };
        match invalid.iter().find(|(invalid_id, _)| invalid_id == id) {
            Some((id, message)) => merged.push(ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: message.clone().into(),
                is_error: true,
            }),
            // Ordinary calls come back in request order, so the front of the
            // queue is this block's result.
            None => {
                if let Some(result) = dispatched.pop_front() {
                    merged.push(result);
                }
            }
        }
    }
    // A dispatcher that returned more results than there were calls (an
    // envelope unwrapped into several, say) keeps all of them: dropping a
    // result would orphan a call.
    merged.extend(dispatched);
    merged
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

/// Cap on resuming a turn after a retryable mid-response stream failure. Bounds
/// a flapping upstream: each resume costs a round, and a stream that keeps dying
/// is a real outage the turn should surface rather than grind against.
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
    inject_pending(inbox.drain(), history, ui)
}

/// The body of [`drain_inbox`], for a drain that has already taken the items.
fn inject_pending(
    pending: Vec<crate::inbox::InboxItem>,
    history: &mut History,
    ui: &Arc<dyn Ui>,
) -> bool {
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
        history.record(item.into_user_message());
    }
    true
}

/// Hand the model its own todo list back when the list has stood still (plan
/// 190). Its only other view of the list is its own `todo_write` arguments,
/// which a few rounds of tool output push out of reach.
///
/// This belongs here, at the round boundary, and nowhere near
/// [`injected_context`]: that is the synthetic first user message, at the head
/// of the cache prefix, where a per-round change would break the prefix for
/// the whole session. Appended at the end of history it is a small increment
/// after the prefix instead. `todo_write` is depth-0 only, so a sub-agent —
/// which cannot see or write the list — is never reminded of it.
fn remind_todos(cfg: &Config, history: &mut History, depth: u8) -> bool {
    if depth > 0 {
        return false;
    }
    let Some(reminder) = cfg.todos.round_boundary_reminder() else {
        return false;
    };
    let reminder = history.offload_text(reminder);
    history.record(Message::user_text(reminder));
    true
}

/// Name the files the model read that changed on disk since (plan 197), as a
/// message of its own at the end of history — the same place and shape as
/// [`remind_todos`], and for the same reason: after the cache prefix, and in
/// the rollout so a replay or resume shows it where the model saw it.
///
/// Every depth: a sub-agent reads files too, into a `FileState` of its own.
fn remind_changed_reads(cfg: &Config, history: &mut History) -> bool {
    let workspace = cfg.effective_workspace();
    let Some(reminder) =
        crate::tools::changed_reads_reminder(&workspace.file_state, &workspace.cwd)
    else {
        return false;
    };
    let reminder = history.offload_text(reminder);
    history.record(Message::user_text(reminder));
    true
}

fn drain_local_mailbox(cfg: &Config, history: &mut History, ui: &Arc<dyn Ui>) -> bool {
    let Some(batch) = cfg.local_agent.claim_boundary() else {
        return false;
    };
    for item in batch.items().iter().cloned() {
        history.record(item.into_user_message());
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
    let parts: Vec<String> = injected_segments(cfg, workspace, depth)
        .into_iter()
        .map(|(_, text)| text)
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n\n"))
}

/// The parts of [`injected_context`], labeled, in request order. `/context`
/// reads the same list, so the breakdown cannot drift from what is sent.
pub(crate) fn injected_segments(
    cfg: &Config,
    workspace: &crate::config::EffectiveWorkspace,
    depth: u8,
) -> Vec<(&'static str, String)> {
    let plan_reminder = (workspace.permissions.mode() == crate::permissions::Mode::Plan)
        .then(|| PLAN_MODE_REMINDER.to_string());
    let skills_catalog = (depth == 0)
        .then(|| crate::skills::skills_catalog(&cfg.skills))
        .flatten();
    [
        ("plan-mode reminder", plan_reminder),
        ("project instructions", cfg.project_instructions.clone()),
        ("skills catalog", skills_catalog),
        ("deferred-tool notice", crate::tools::deferred_notice(cfg)),
    ]
    .into_iter()
    .filter_map(|(label, text)| text.map(|text| (label, text)))
    .collect()
}

/// What the next request is estimated to cost. A provider-reported anchor is
/// the whole previous request — system prompt, tool array and injected first
/// message included — so only what history added since is on top of it.
/// Without one, history's estimate covers history alone and `overhead` supplies
/// the other three; adding them on top of an anchor would count them twice.
fn estimate_request(history: &History, overhead: impl FnOnce() -> u64) -> u64 {
    match history.has_usage_anchor() {
        true => history.estimated_tokens(),
        false => history.estimated_tokens() + overhead(),
    }
}

/// The part of a request that is not history.
fn request_overhead_tokens(
    system: &str,
    tools: &[kloop_protocol::ToolDef],
    injected: Option<&str>,
) -> u64 {
    let tools: u64 = tools
        .iter()
        .map(crate::history::estimate_tool_def_tokens)
        .sum();
    crate::history::estimate_text_tokens(system)
        + tools
        + injected.map_or(0, crate::history::estimate_text_tokens)
}

/// The context size a front end shows between turns (`/cost`, the gauge): the
/// next depth-0 request, estimated as [`estimate_request`] does inside a turn.
pub fn context_estimate(cfg: &Arc<Config>, history: &History) -> u64 {
    estimate_request(history, || {
        let workspace = cfg.effective_workspace();
        // A catalog that will not hold still long enough to build is left out
        // rather than failing a display number; the turn itself will say so.
        let tools = top_level_tool_defs(cfg).unwrap_or_default();
        request_overhead_tokens(
            &workspace.system,
            &tools,
            injected_context(cfg, &workspace, 0).as_deref(),
        )
    })
}

/// The depth-0 tool array a turn started now would send. `/context` sizes it.
pub(crate) fn top_level_tool_defs(
    cfg: &Arc<Config>,
) -> std::result::Result<Vec<kloop_protocol::ToolDef>, String> {
    build_tools(cfg, 0, &TurnOptions::default()).map(|(tools, _)| tools)
}

#[cfg(test)]
mod tests;
