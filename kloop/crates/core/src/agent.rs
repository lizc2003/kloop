use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::compact;
use crate::config::Config;
use crate::history::History;
use crate::inbox::Inbox;
use crate::tools::all_tool_defs;
use crate::tools::dispatch_tools;
use crate::tools::ToolCtx;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_protocol::OverflowError;
use kloop_protocol::StreamEvent;
use kloop_protocol::ToolDef;
use kloop_protocol::Usage;
use kloop_protocol::MAX_OUTPUT_TOKENS;

pub trait Ui: Send + Sync {
    fn text_delta(&self, s: &str);
    /// Streaming reasoning text. Display-only and often empty on the wire
    /// (Anthropic display=omitted sends blocks with no text), so the default
    /// drops it and only UIs that render thinking opt in.
    fn thinking_delta(&self, s: &str) {
        let _ = s;
    }
    fn note(&self, s: &str);
    /// Tool-call lifecycle, for UIs that render per-call status rows. `agent`
    /// is "" for the main agent's calls and the sub-agent's label ("agent-N")
    /// for calls made inside a task — parallel sub-agents interleave on this
    /// stream and the label is what tells them apart. The defaults collapse
    /// to the plain note stream so line-based UIs need not care about call ids.
    fn tool_start(&self, agent: &str, id: &str, name: &str, summary: &str) {
        let _ = id;
        if agent.is_empty() {
            self.note(&format!("{name} {summary}"));
        } else {
            self.note(&format!("{agent} · {name} {summary}"));
        }
    }
    fn tool_end(&self, agent: &str, id: &str, ok: bool) {
        let _ = (agent, id, ok);
    }
    /// Sub-agent lifecycle: a task call spawned `agent` to work on `task`
    /// (first line of the prompt, truncated). Ends exactly once per start.
    fn agent_start(&self, agent: &str, task: &str) {
        self.note(&format!("{agent} started: {task}"));
    }
    fn agent_end(&self, agent: &str, ok: bool) {
        self.note(&format!(
            "{agent} {}",
            if ok { "finished" } else { "failed" }
        ));
    }
    /// The model rewrote its task list via todo_write (full replacement).
    /// `agent` is "" for the main agent, "agent-N" for a sub-agent. The
    /// default collapses to a one-line note; UIs that render a checklist opt
    /// in. See [`crate::tools::TodoItem`].
    fn todo_update(&self, agent: &str, todos: &[crate::tools::TodoItem]) {
        use crate::tools::TodoStatus;
        let done = todos
            .iter()
            .filter(|t| t.status == TodoStatus::Completed)
            .count();
        let prefix = if agent.is_empty() {
            String::new()
        } else {
            format!("{agent} · ")
        };
        match todos.iter().find(|t| t.status == TodoStatus::InProgress) {
            Some(current) => self.note(&format!(
                "{prefix}todos {done}/{} · now: {}",
                todos.len(),
                current.active_form
            )),
            None => self.note(&format!("{prefix}todos {done}/{} done", todos.len())),
        }
    }
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
    match cfg.hooks.pre_turn(&cfg.session_id, ui.as_ref()).await {
        crate::hooks::HookDecision::Block { reason } => {
            return TurnOutcome {
                reason: EndReason::Error(format!("turn blocked by pre_turn hook: {reason}")),
                final_text: String::new(),
                rounds: 0,
            }
        }
        crate::hooks::HookDecision::Allow { context } => {
            for text in context {
                history.record(Message::user_text(text));
            }
        }
    }
    let outcome = turn_rounds(cfg, history, ui, cancel, depth).await;
    for text in cfg.hooks.post_turn(&cfg.session_id, ui.as_ref()).await {
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
    let mut tools = all_tool_defs(depth, &cfg.tool_sources, cfg.defer_threshold);
    // A custom agent type may restrict this sub-agent's tools; the main agent
    // (None) keeps them all. read_offloaded is never filtered out.
    if cfg.tool_allowlist.is_some() {
        let allow = cfg.tool_allowlist.as_deref();
        tools.retain(|t| crate::agents::tool_available(allow, &t.name));
    }
    // At depth 0 the task tool exists; list the configured agent types in its
    // description so the model knows what it can dispatch to.
    if depth == 0 && !cfg.agent_types.is_empty() {
        if let Some(task) = tools.iter_mut().find(|t| t.name == "task") {
            task.description
                .push_str(&crate::agents::agent_types_hint(&cfg.agent_types));
        }
    }
    // A sub-agent's text is its deliverable and returns via the tool result;
    // streaming it to the main UI would interleave with the parent's output.
    let stream_text = depth == 0;
    let growth = compact::max_turn_growth(MAX_OUTPUT_TOKENS);
    // The injected context message is not part of history, so the overflow
    // prediction must account for it separately.
    let instructions_tokens = injected_context(cfg).map_or(0, |s| s.len() as u64 / 4);
    // Overflow is recovered at most once per turn: compact, then retry. A
    // second overflow after a successful compaction surfaces as an error.
    let mut overflow_compact_attempted = false;
    // The model can be swapped once per turn: after retries are exhausted on
    // the primary, the rest of the turn runs on the fallback.
    let mut active_model = cfg.model.clone();
    let mut truncation_recoveries = 0u32;
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
                ui.note("predicted context overflow; compacting history");
                match compact::run_compaction(cfg, history, cancel).await {
                    Ok(stats) => ui.note(&format!(
                        "history compacted: {} summarized, {} kept verbatim",
                        stats.summarized, stats.kept
                    )),
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
                        ui.note(&format!("predictive compaction failed: {e:#}"));
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
                ui.note("context window exceeded; compacting and retrying");
                match compact::run_compaction(cfg, history, cancel).await {
                    Ok(stats) => {
                        ui.note(&format!(
                            "history compacted: {} summarized, {} kept verbatim",
                            stats.summarized, stats.kept
                        ));
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
                        ui.note(&format!(
                            "sampling failed on {active_model}; switching to fallback model {fallback}: {e}"
                        ));
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
            if is_truncated(stop_reason.as_deref())
                && truncation_recoveries < TRUNCATION_RECOVERY_LIMIT
            {
                truncation_recoveries += 1;
                ui.note(&format!(
                    "response truncated by output limit; asking the model to continue ({truncation_recoveries}/{TRUNCATION_RECOVERY_LIMIT})"
                ));
                history.record(Message::user_text(TRUNCATION_CONTINUE_MSG));
                continue;
            }
            let final_text = blocks
                .iter()
                .rev()
                .find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default();
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
            program_result: None,
        };
        let results = dispatch_tools(tool_uses, &ctx).await;
        // Record results BEFORE checking cancellation so every tool_use has a
        // paired tool_result and history stays legal for the next request.
        history.record(Message::tool_results(results));
        // Tool-hook stdout follows the results it commented on, as extra
        // user-message context.
        for text in std::mem::take(&mut *ctx.hook_context.lock().unwrap()) {
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

struct SampleOk {
    blocks: Vec<ContentBlock>,
    usage: Option<Usage>,
    stop_reason: Option<String>,
}

enum Sampled {
    Ok(SampleOk),
    Overflow,
    Cancelled,
    Failed(String),
}

enum SampleError {
    Cancelled,
    Overflow,
    Retryable(String),
}

/// The response was cut off by the output token limit ("max_tokens" on the
/// Anthropic wire, "length" on OpenAI-compat). This is the one legitimate use
/// of stop_reason: not to decide continuation, but to detect an ungraceful
/// ending worth recovering from.
fn is_truncated(stop_reason: Option<&str>) -> bool {
    matches!(stop_reason, Some("max_tokens") | Some("length"))
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

const MAX_ATTEMPTS: u32 = 3;

/// The synthetic first user message: project instructions plus the
/// deferred-tools notice. Both parts are session-stable, so the composed
/// message is too — the prompt-cache prefix survives across rounds.
fn injected_context(cfg: &Config) -> Option<String> {
    let notice = crate::tools::deferred_notice(cfg);
    match (&cfg.project_instructions, notice) {
        (None, None) => None,
        (Some(instructions), None) => Some(instructions.clone()),
        (None, Some(notice)) => Some(notice),
        (Some(instructions), Some(notice)) => Some(format!("{instructions}\n\n{notice}")),
    }
}

async fn sample_with_retry(
    cfg: &Arc<Config>,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Sampled {
    // Project instructions and the deferred-tools notice ride every request
    // as a synthetic first user message. Never recorded: resume rereads
    // fresh files, and compaction cannot swallow it.
    let injected;
    let messages = match injected_context(cfg) {
        Some(context) => {
            let mut with_context = Vec::with_capacity(messages.len() + 1);
            with_context.push(Message::user_text(context));
            with_context.extend_from_slice(messages);
            injected = with_context;
            &injected[..]
        }
        None => messages,
    };
    for attempt in 0..MAX_ATTEMPTS {
        match sample_once(cfg, model, messages, tools, ui, cancel, stream_text).await {
            Ok(ok) => return Sampled::Ok(ok),
            Err(SampleError::Cancelled) => return Sampled::Cancelled,
            // Retrying an oversized request verbatim can never succeed; hand
            // it straight to the reactive compaction path.
            Err(SampleError::Overflow) => return Sampled::Overflow,
            Err(SampleError::Retryable(e)) => {
                if attempt + 1 == MAX_ATTEMPTS {
                    return Sampled::Failed(e);
                }
                // Exponential backoff with sub-ms jitter from the clock's
                // nanoseconds — good enough without pulling in `rand`.
                let jitter = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| u64::from(d.subsec_nanos()) % 250)
                    .unwrap_or(0);
                let delay = Duration::from_millis((250 << attempt) + jitter);
                ui.note(&format!(
                    "sampling failed (attempt {}/{MAX_ATTEMPTS}), retrying in {delay:?}: {e}",
                    attempt + 1
                ));
                tokio::select! {
                    _ = cancel.cancelled() => return Sampled::Cancelled,
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        }
    }
    unreachable!("retry loop always returns")
}

#[allow(clippy::too_many_arguments)]
async fn sample_once(
    cfg: &Arc<Config>,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    ui: &Arc<dyn Ui>,
    cancel: &CancellationToken,
    stream_text: bool,
) -> Result<SampleOk, SampleError> {
    let mut rx = cfg.provider.stream(model, &cfg.system, messages, tools);
    let mut blocks = Vec::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Err(SampleError::Cancelled),
            event = rx.recv() => match event {
                None => return Err(SampleError::Retryable("stream closed early".into())),
                Some(Err(e)) => {
                    if e.downcast_ref::<OverflowError>().is_some() {
                        return Err(SampleError::Overflow);
                    }
                    return Err(SampleError::Retryable(format!("{e:#}")));
                }
                Some(Ok(StreamEvent::TextDelta(t))) => {
                    if stream_text {
                        ui.text_delta(&t);
                    }
                }
                Some(Ok(StreamEvent::ThinkingDelta(t))) => {
                    if stream_text {
                        ui.thinking_delta(&t);
                    }
                }
                Some(Ok(StreamEvent::BlockDone(b))) => blocks.push(b),
                Some(Ok(StreamEvent::Done { usage, stop_reason })) => {
                    return Ok(SampleOk { blocks, usage, stop_reason })
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox::InboxItem;
    use crate::inbox::STEERING_PREFIX;
    use kloop_protocol::Role;
    use kloop_provider::Provider;
    use serde_json::json;

    struct NullUi;
    impl Ui for NullUi {
        fn text_delta(&self, _: &str) {}
        fn note(&self, _: &str) {}
    }

    fn tool_use(id: &str, cmd: &str) -> ContentBlock {
        tool_use_named(id, "bash", json!({"command": cmd}))
    }

    fn tool_use_named(id: &str, name: &str, input: Value) -> ContentBlock {
        ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }
    }

    /// End-to-end over the Mock provider: round 1 issues two concurrency-safe
    /// bash calls (one concurrent batch), round 2 one unsafe call (sequential),
    /// round 3 plain text ends the turn. Asserts the full history shape.
    #[tokio::test]
    async fn mock_end_to_end_three_rounds() {
        let provider = Provider::mock(vec![
            vec![tool_use("t1", "echo one"), tool_use("t2", "echo two")],
            vec![tool_use("t3", "true")],
            vec![ContentBlock::Text {
                text: "all done".into(),
            }],
        ]);
        let cfg = Arc::new(Config {
            provider: Arc::new(provider),
            model: "mock".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: 10,
            offload_dir: std::env::temp_dir().join("kloop-test-e2e"),
            context_window: None,
            fallback_model: None,
            permissions: Arc::new(crate::permissions::Permissions::allow_all()),
            tool_sources: Vec::new(),
            session_id: String::new(),
            agent_label: String::new(),
            hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
            background_shells: crate::tools::BackgroundShells::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
            inbox: Default::default(),
            async_agents: Default::default(),
        });
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("run the demo"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "all done");
        assert_eq!(outcome.rounds, 3);

        let msgs = history.messages();
        let shape: Vec<(Role, Vec<&str>)> = msgs
            .iter()
            .map(|m| {
                let kinds = m
                    .content
                    .iter()
                    .map(|b| match b {
                        ContentBlock::Text { .. } => "text",
                        ContentBlock::Thinking { .. } => "thinking",
                        ContentBlock::RedactedThinking { .. } => "redacted_thinking",
                        ContentBlock::ToolUse { .. } => "tool_use",
                        ContentBlock::ToolResult { .. } => "tool_result",
                    })
                    .collect();
                (m.role, kinds)
            })
            .collect();
        assert_eq!(
            shape,
            vec![
                (Role::User, vec!["text"]),
                (Role::Assistant, vec!["tool_use", "tool_use"]),
                (Role::User, vec!["tool_result", "tool_result"]),
                (Role::Assistant, vec!["tool_use"]),
                (Role::User, vec!["tool_result"]),
                (Role::Assistant, vec!["text"]),
            ]
        );

        // The concurrent batch preserved request order and captured output.
        assert_eq!(
            msgs[2].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "one\n".into(),
                is_error: false,
            }
        );
        assert_eq!(
            msgs[2].content[1],
            ContentBlock::ToolResult {
                tool_use_id: "t2".into(),
                content: "two\n".into(),
                is_error: false,
            }
        );
        assert_eq!(
            msgs[4].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "t3".into(),
                content: "(no output)".into(),
                is_error: false,
            }
        );
    }

    fn compaction_cfg(provider: Provider, window: u64, tag: &str) -> Arc<Config> {
        Arc::new(Config {
            provider: Arc::new(provider),
            model: "mock".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: 10,
            offload_dir: std::env::temp_dir().join(format!("kloop-test-{tag}")),
            context_window: Some(window),
            fallback_model: None,
            permissions: Arc::new(crate::permissions::Permissions::allow_all()),
            tool_sources: Vec::new(),
            session_id: String::new(),
            agent_label: String::new(),
            hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
            background_shells: crate::tools::BackgroundShells::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
            inbox: Default::default(),
            async_agents: Default::default(),
        })
    }

    /// Predictive: a fat history under a small (but > growth reserve) window
    /// triggers compaction BEFORE the sampling request. Mock turn 1 serves
    /// the summary, turn 2 the actual reply.
    #[tokio::test]
    async fn predictive_compaction_fires_before_sampling() {
        let provider = Provider::mock(vec![
            vec![ContentBlock::Text {
                text: "summary of everything so far".into(),
            }],
            vec![ContentBlock::Text {
                text: "final answer".into(),
            }],
        ]);
        // growth = 8192 + 15_000 = 23_192; window 30_000 → threshold ≈ 6_808
        // tokens ≈ 27k chars. Two fat user messages blow past it.
        let cfg = compaction_cfg(provider, 30_000, "predictive");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("x".repeat(30_000)));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "y".repeat(30_000),
        }]));
        history.record(Message::user_text("now answer briefly"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "final answer");
        let msgs = history.messages();
        // [summary, ...kept tail..., assistant reply]; the fat prefix is gone.
        let ContentBlock::Text { text } = &msgs[0].content[0] else {
            panic!("expected text summary at history start");
        };
        assert!(
            text.starts_with(crate::compact::SUMMARY_PREFIX),
            "history should start with the compaction summary"
        );
        assert!(
            history.estimated_tokens() < 5_000,
            "compaction should have shrunk the history, got {} tokens",
            history.estimated_tokens()
        );
    }

    /// Reactive: the first sampling request is rejected as too large; the
    /// loop compacts once (mock turn 2 = summary) and retries successfully
    /// (turn 3), with no user-visible error.
    #[tokio::test]
    async fn overflow_compacts_and_retries() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Overflow,
            MockTurn::Blocks(vec![ContentBlock::Text {
                text: "summary of everything so far".into(),
            }]),
            MockTurn::Blocks(vec![ContentBlock::Text {
                text: "recovered answer".into(),
            }]),
        ]);
        // Large window: predictive stays silent, only the reactive path runs.
        let cfg = compaction_cfg(provider, 200_000, "reactive");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("earlier context"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "earlier reply".into(),
        }]));
        history.record(Message::user_text("the request that overflows"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "recovered answer");
        let ContentBlock::Text { text } = &history.messages()[0].content[0] else {
            panic!("expected text summary at history start");
        };
        assert!(text.starts_with(crate::compact::SUMMARY_PREFIX));
    }

    /// A second overflow after a successful compaction must surface as an
    /// error instead of looping.
    #[tokio::test]
    async fn repeated_overflow_surfaces_error() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Overflow,
            MockTurn::Blocks(vec![ContentBlock::Text {
                text: "summary".into(),
            }]),
            MockTurn::Overflow,
        ]);
        let cfg = compaction_cfg(provider, 200_000, "reactive-repeat");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("earlier context"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "earlier reply".into(),
        }]));
        history.record(Message::user_text("still too big"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert!(
            matches!(outcome.reason, EndReason::Error(_)),
            "second overflow must not loop, got {:?}",
            outcome.reason
        );
    }

    fn text(t: &str) -> Vec<ContentBlock> {
        vec![ContentBlock::Text { text: t.into() }]
    }

    /// A truncated final response gets a "continue" nudge instead of ending
    /// the turn mid-thought.
    #[tokio::test]
    async fn truncated_response_recovers_with_continuation() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Truncated(text("part one, cut off mid-")),
            MockTurn::Blocks(text("part two, complete.")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "truncation");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("write something long"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "part two, complete.");
        assert_eq!(outcome.rounds, 2);
        // [user, assistant(truncated), user(continue nudge), assistant(rest)]
        let msgs = history.messages();
        assert_eq!(msgs.len(), 4);
        assert_eq!(
            msgs[2],
            Message::user_text(super::TRUNCATION_CONTINUE_MSG),
            "the continuation nudge must be recorded so history stays legal"
        );
    }

    /// Truncation nudges are bounded: after the limit the turn completes with
    /// whatever text arrived instead of looping.
    #[tokio::test]
    async fn truncation_recovery_is_bounded() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Truncated(text("cut 1")),
            MockTurn::Truncated(text("cut 2")),
            MockTurn::Truncated(text("cut 3")),
            MockTurn::Truncated(text("cut 4")),
            MockTurn::Truncated(text("cut 5")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "truncation-limit");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("write something very long"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        // 3 nudges (the limit), so the 4th truncated response ends the turn.
        assert_eq!(outcome.final_text, "cut 4");
        assert_eq!(outcome.rounds, 4);
        let nudges = history
            .messages()
            .iter()
            .filter(|m| *m == &Message::user_text(super::TRUNCATION_CONTINUE_MSG))
            .count();
        assert_eq!(nudges, 3);
    }

    /// After the primary model exhausts its retries, the turn continues on
    /// the fallback model instead of surfacing an error.
    #[tokio::test]
    async fn fallback_model_takes_over_after_retries() {
        use kloop_provider::MockTurn;
        struct NoteUi(std::sync::Mutex<Vec<String>>);
        impl Ui for NoteUi {
            fn text_delta(&self, _: &str) {}
            fn note(&self, s: &str) {
                self.0.lock().unwrap().push(s.to_string());
            }
        }

        let provider = Provider::mock_scripted(vec![
            MockTurn::Error("boom 1".into()),
            MockTurn::Error("boom 2".into()),
            MockTurn::Error("boom 3".into()),
            MockTurn::Blocks(text("answer from fallback")),
        ]);
        let mut cfg = (*compaction_cfg(provider, 200_000, "fallback")).clone();
        cfg.fallback_model = Some("mock-fallback".into());
        let cfg = Arc::new(cfg);
        let note_ui = Arc::new(NoteUi(std::sync::Mutex::new(Vec::new())));
        let ui: Arc<dyn Ui> = note_ui.clone();
        let cancel = CancellationToken::new();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("hello"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "answer from fallback");
        let notes = note_ui.0.lock().unwrap();
        assert!(
            notes
                .iter()
                .any(|n| n.contains("switching to fallback model mock-fallback")),
            "expected a fallback-switch note, got {notes:?}"
        );
    }

    /// Transient provider errors are retried in place; the turn still
    /// completes without any fallback configured.
    #[tokio::test]
    async fn retry_recovers_from_transient_errors() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Error("blip 1".into()),
            MockTurn::Error("blip 2".into()),
            MockTurn::Blocks(text("made it")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "retry");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("hello"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "made it");
    }

    /// Three failures with no fallback exhaust the retry budget and surface
    /// the error.
    #[tokio::test]
    async fn retries_exhausted_without_fallback_error_out() {
        use kloop_provider::MockTurn;
        let provider = Provider::mock_scripted(vec![
            MockTurn::Error("down 1".into()),
            MockTurn::Error("down 2".into()),
            MockTurn::Error("down 3".into()),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "exhausted");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("hello"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert!(
            matches!(&outcome.reason, EndReason::Error(e) if e.contains("down 3")),
            "expected the last error surfaced, got {:?}",
            outcome.reason
        );
    }

    /// A model that never stops calling tools is cut off at max_rounds, with
    /// history left legal (every tool_use answered).
    #[tokio::test]
    async fn endless_tool_calls_hit_max_rounds() {
        let provider = Provider::mock(vec![
            vec![tool_use("t1", "echo 1")],
            vec![tool_use("t2", "echo 2")],
            vec![tool_use("t3", "echo 3")],
            vec![tool_use("t4", "echo 4")],
        ]);
        let mut cfg = (*compaction_cfg(provider, 200_000, "maxrounds")).clone();
        cfg.max_rounds = 3;
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("loop forever"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::MaxRounds);
        assert_eq!(outcome.rounds, 3);
        // 1 user + 3 * (assistant + tool_results): every round paired.
        assert_eq!(history.messages().len(), 7);
    }

    /// A token cancelled before the turn starts aborts before sampling.
    #[tokio::test]
    async fn pre_cancelled_turn_aborts_immediately() {
        let provider = Provider::mock(vec![vec![ContentBlock::Text {
            text: "never sampled".into(),
        }]]);
        let cfg = compaction_cfg(provider, 200_000, "precancel");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("hello"));

        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;

        assert_eq!(outcome.reason, EndReason::Aborted);
        assert_eq!(outcome.rounds, 0);
        assert_eq!(history.messages().len(), 1, "nothing recorded after abort");
    }

    /// A denied tool call becomes an is_error tool_result the model can react
    /// to — the turn continues instead of ending.
    #[tokio::test]
    async fn denied_tool_call_continues_the_turn() {
        use crate::permissions::{
            Approver, ConfirmRequest, Decision, Mode, PermissionRules, Permissions,
        };
        use std::pin::Pin;

        struct DenyAll;
        impl Approver for DenyAll {
            fn confirm(
                &self,
                _: ConfirmRequest,
            ) -> Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
                Box::pin(async { Decision::Deny })
            }
        }

        let provider = Provider::mock(vec![
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "write_file".into(),
                input: json!({"path": "should-not-exist", "content": "x"}),
            }],
            vec![ContentBlock::Text {
                text: "understood, taking another approach".into(),
            }],
        ]);
        let mut cfg = (*compaction_cfg(provider, 200_000, "denied")).clone();
        cfg.permissions = Arc::new(
            Permissions::new(
                Mode::Default,
                &PermissionRules::default(),
                std::env::temp_dir(),
                Some(Arc::new(DenyAll)),
                None,
            )
            .unwrap(),
        );
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("write a file"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "understood, taking another approach");
        assert_eq!(outcome.rounds, 2);
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &history.messages()[2].content[0]
        else {
            panic!("expected a tool result for the denied call");
        };
        assert!(is_error);
        assert!(content.contains("declined"), "got: {content}");
        assert!(!std::path::Path::new("should-not-exist").exists());
    }

    fn hooked_cfg(provider: Provider, defs: Vec<crate::hooks::HookDef>, tag: &str) -> Arc<Config> {
        let mut cfg = (*compaction_cfg(provider, 200_000, tag)).clone();
        cfg.session_id = format!("session-{tag}");
        cfg.hooks = Arc::new(crate::hooks::Hooks { defs });
        Arc::new(cfg)
    }

    fn hook(event: crate::hooks::HookEvent, script: &str) -> crate::hooks::HookDef {
        crate::hooks::HookDef {
            event,
            command: vec!["sh".into(), "-c".into(), script.into()],
            matcher: None,
            timeout_ms: crate::hooks::DEFAULT_TIMEOUT_MS,
        }
    }

    /// All four hook points fire, in order, around a one-tool-call turn.
    #[tokio::test]
    async fn four_hook_points_fire_in_order() {
        use crate::hooks::HookEvent;
        let marker = std::env::temp_dir().join(format!("kloop-hook-order-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mark = |event: &str| format!("echo {event} >> {}", marker.display());
        let provider = Provider::mock(vec![
            vec![tool_use("t1", "echo hi")],
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        ]);
        let cfg = hooked_cfg(
            provider,
            vec![
                hook(HookEvent::PreTurn, &mark("pre_turn")),
                hook(HookEvent::PostTurn, &mark("post_turn")),
                hook(HookEvent::PreTool, &mark("pre_tool")),
                hook(HookEvent::PostTool, &mark("post_tool")),
            ],
            "order",
        );
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            "pre_turn\npre_tool\npost_tool\npost_turn\n"
        );
        let _ = std::fs::remove_file(&marker);
    }

    /// A blocking pre_tool hook: the command never runs and the model gets an
    /// is_error tool_result carrying the hook's reason — the turn continues.
    #[tokio::test]
    async fn pre_tool_hook_block_becomes_error_tool_result() {
        use crate::hooks::HookEvent;
        let marker = std::env::temp_dir().join(format!("kloop-hook-block-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let provider = Provider::mock(vec![
            vec![tool_use("t1", &format!("touch {}", marker.display()))],
            vec![ContentBlock::Text {
                text: "changing course".into(),
            }],
        ]);
        let cfg = hooked_cfg(
            provider,
            vec![hook(
                HookEvent::PreTool,
                "echo rm-like commands are banned 1>&2; exit 2",
            )],
            "block",
        );
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(
            history.messages()[2].content[0],
            ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "blocked by hook: rm-like commands are banned".into(),
                is_error: true,
            }
        );
        assert!(!marker.exists(), "the blocked command must not have run");
    }

    /// A blocking pre_turn hook: the turn never starts (nothing sampled,
    /// nothing recorded) and the user sees the reason.
    #[tokio::test]
    async fn pre_turn_hook_block_prevents_the_turn() {
        use crate::hooks::HookEvent;
        let provider = Provider::mock(vec![vec![ContentBlock::Text {
            text: "never sampled".into(),
        }]]);
        let cfg = hooked_cfg(
            provider,
            vec![hook(HookEvent::PreTurn, "echo out of office 1>&2; exit 2")],
            "preturn-block",
        );
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(
            outcome.reason,
            EndReason::Error("turn blocked by pre_turn hook: out of office".into())
        );
        assert_eq!(outcome.rounds, 0);
        assert_eq!(history.messages().len(), 1, "nothing recorded");
    }

    /// Allowing hooks' stdout lands in history as user-message context, in
    /// its documented shape: pre_turn before sampling, post_tool right after
    /// the round's tool results.
    #[tokio::test]
    async fn hook_stdout_is_injected_as_user_context() {
        use crate::hooks::HookEvent;
        use kloop_protocol::Role;
        let provider = Provider::mock(vec![
            vec![tool_use("t1", "echo hi")],
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        ]);
        let cfg = hooked_cfg(
            provider,
            vec![
                hook(HookEvent::PreTurn, "echo repo rule: tests first"),
                hook(HookEvent::PostTool, "echo lint passed"),
            ],
            "inject",
        );
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        let msgs = history.messages();
        // [user, user(pre_turn ctx), assistant(tool_use), user(tool_result),
        //  user(post_tool ctx), assistant(text)]
        assert_eq!(
            msgs[1],
            Message::user_text("[pre_turn hook]\nrepo rule: tests first")
        );
        assert_eq!(msgs[2].role, Role::Assistant);
        assert_eq!(msgs[4], Message::user_text("[post_tool hook]\nlint passed"));
    }

    /// Project instructions ride every sampling request as a synthetic first
    /// user message — and are never recorded to history.
    #[tokio::test]
    async fn project_instructions_injected_per_request_not_recorded() {
        use kloop_provider::MockTurn;
        let (provider, seen) = Provider::mock_recording(vec![
            MockTurn::Blocks(vec![tool_use("t1", "echo hi")]),
            MockTurn::Blocks(text("done")),
        ]);
        let instructions = "<project-instructions>reply in haiku</project-instructions>";
        let mut cfg = (*compaction_cfg(provider, 200_000, "instructions")).clone();
        cfg.project_instructions = Some(instructions.into());
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        for request in seen.iter() {
            assert_eq!(
                request.messages[0],
                Message::user_text(instructions),
                "every request must start with the injected instructions"
            );
        }
        // The real history follows the synthetic message untouched…
        assert_eq!(seen[1].messages[1], Message::user_text("go"));
        // …and never absorbs it.
        assert!(history
            .messages()
            .iter()
            .all(|m| *m != Message::user_text(instructions)));
    }

    /// Code-mode end to end over Mock: the model emits one `run_program`
    /// tool_use whose program reads a file twice internally, then returns a
    /// summary. The next request to the model carries exactly one run_program
    /// tool_result — the summary — and the file content the program handled
    /// never reaches the context.
    #[tokio::test]
    async fn run_program_returns_only_final_output_to_the_model() {
        use kloop_provider::MockTurn;
        let file =
            std::env::temp_dir().join(format!("kloop-run-program-e2e-{}", std::process::id()));
        std::fs::write(&file, "PAYLOAD_LINE_XYZ").unwrap();
        let path = file.to_string_lossy().replace('\\', "\\\\");
        let source = format!(
            "const a = await tools.read_file({{ path: \"{path}\" }});\n\
             const b = await tools.read_file({{ path: \"{path}\" }});\n\
             return \"read \" + (a.length + b.length) + \" chars total\";"
        );
        let (provider, seen) = Provider::mock_recording(vec![
            MockTurn::Blocks(vec![tool_use_named(
                "e1",
                "run_program",
                json!({ "source": source }),
            )]),
            MockTurn::Blocks(text("done")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "run-program-e2e");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
        assert_eq!(outcome.reason, EndReason::Completed);

        // Second request = the one sent after run_program ran. It must show the
        // program's return value and never the file content read inside it.
        let seen = seen.lock().unwrap();
        let dump = format!("{:?}", seen[1].messages);
        assert!(
            dump.contains("read ") && dump.contains("chars total"),
            "{dump}"
        );
        assert!(
            !dump.contains("PAYLOAD_LINE_XYZ"),
            "an intermediate tool result leaked into the context: {dump}"
        );
        let _ = std::fs::remove_file(&file);
    }

    /// Deferred regime end to end over Mock: the request's tool defs shrink
    /// to built-ins + tool_search, the notice rides the injected context
    /// message (after the instructions) without entering history, and a
    /// searched tool becomes callable while an unsearched one stays locked.
    #[tokio::test]
    async fn deferred_tools_shrink_defs_inject_notice_and_gate_dispatch() {
        use crate::tools::ToolSource;
        use kloop_provider::MockTurn;

        struct Srv {
            defs: Vec<kloop_protocol::ToolDef>,
        }
        impl ToolSource for Srv {
            fn defs(&self) -> &[kloop_protocol::ToolDef] {
                &self.defs
            }
            fn is_readonly(&self, _tool: &str) -> bool {
                false
            }
            fn call<'a>(
                &'a self,
                tool: &'a str,
                _input: &'a Value,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = anyhow::Result<crate::tools::SourceOutput>>
                        + Send
                        + 'a,
                >,
            > {
                Box::pin(async move { Ok(crate::tools::SourceOutput::text(format!("ran {tool}"))) })
            }
        }
        let source: Arc<dyn ToolSource> = Arc::new(Srv {
            defs: vec![kloop_protocol::ToolDef {
                name: "srv__lookup".into(),
                description: "Look things up".into(),
                schema: serde_json::json!({"type": "object"}),
            }],
        });

        let (provider, seen) = Provider::mock_recording(vec![
            // Round 1: one locked direct call + one search — both get results.
            MockTurn::Blocks(vec![
                tool_use_named("t1", "srv__lookup", serde_json::json!({})),
                tool_use_named(
                    "t2",
                    "tool_search",
                    serde_json::json!({"query": "select:srv__lookup"}),
                ),
            ]),
            // Round 2: the unlocked tool now runs.
            MockTurn::Blocks(vec![tool_use_named(
                "t3",
                "srv__lookup",
                serde_json::json!({}),
            )]),
            MockTurn::Blocks(text("done")),
        ]);
        let mut cfg = (*compaction_cfg(provider, 200_000, "deferred")).clone();
        cfg.project_instructions = Some("INSTR".into());
        cfg.tool_sources = vec![source];
        cfg.defer_threshold = 0;
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3);
        for request in seen.iter() {
            // Defs: built-ins + tool_search, never the source tool — stable
            // across rounds even after the unlock.
            let names: Vec<&str> = request.tools.iter().map(|d| d.name.as_str()).collect();
            assert!(names.contains(&"tool_search"));
            assert!(!names.contains(&"srv__lookup"));
            // The synthetic first message carries instructions + notice.
            let Some(ContentBlock::Text { text }) = request.messages[0].content.first() else {
                panic!("expected injected text message");
            };
            assert!(text.starts_with("INSTR\n\n<system-reminder>"), "{text}");
            assert!(text.contains("srv__lookup"), "{text}");
        }
        // Round 1 results: locked bounce for t1, definitions for t2.
        let round1 = &history.messages()[2];
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &round1.content[0]
        else {
            panic!("expected tool_result");
        };
        assert!(is_error);
        assert!(content.contains("call tool_search"), "{content}");
        // Round 2: the same call now reaches the source.
        let round2 = &history.messages()[4];
        assert_eq!(
            round2.content[0],
            ContentBlock::ToolResult {
                tool_use_id: "t3".into(),
                content: "ran srv__lookup".into(),
                is_error: false,
            }
        );
        // The notice never entered history.
        assert!(history
            .messages()
            .iter()
            .all(|m| m.content.iter().all(|b| !matches!(
                b,
                ContentBlock::Text { text } if text.contains("<system-reminder>")
            ))));
    }

    /// The injected instructions count toward the overflow prediction even
    /// though they are not in history — and the compaction request itself
    /// runs on plain history, without the injected message.
    #[tokio::test]
    async fn instructions_count_toward_predictive_compaction() {
        use kloop_provider::MockTurn;
        let (provider, seen) = Provider::mock_recording(vec![
            MockTurn::Blocks(text("summary of everything so far")),
            MockTurn::Blocks(text("final answer")),
        ]);
        // window 30_000, growth 23_192 → threshold ≈ 6_808 tokens. History
        // alone estimates ~6_500; the 8_000-char instructions add ~2_000 and
        // push it over, so compaction must fire before sampling.
        let mut cfg = (*compaction_cfg(provider, 30_000, "instr-predict")).clone();
        cfg.project_instructions = Some("r".repeat(8_000));
        let cfg = Arc::new(cfg);
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("x".repeat(13_000)));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "y".repeat(13_000),
        }]));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "final answer");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2, "compaction request + the real request");
        // Request 0 is the compaction summary: plain history, no injection.
        assert!(seen[0]
            .messages
            .iter()
            .all(|m| m.content.iter().all(|b| !matches!(
                b,
                ContentBlock::Text { text } if text.starts_with("rrr")
            ))));
        // Request 1 is the real one: instructions first, compacted history after.
        assert_eq!(seen[1].messages[0], Message::user_text("r".repeat(8_000)));
    }

    /// Thinking blocks are recorded to history verbatim (they must replay on
    /// the next request) and their text streams to the UI's thinking channel,
    /// never the answer channel.
    #[tokio::test]
    async fn thinking_blocks_recorded_and_streamed_separately() {
        struct SplitUi {
            thinking: std::sync::Mutex<String>,
            text: std::sync::Mutex<String>,
        }
        impl Ui for SplitUi {
            fn text_delta(&self, s: &str) {
                self.text.lock().unwrap().push_str(s);
            }
            fn thinking_delta(&self, s: &str) {
                self.thinking.lock().unwrap().push_str(s);
            }
            fn note(&self, _: &str) {}
        }

        let blocks = vec![
            ContentBlock::Thinking {
                thinking: "pondering".into(),
                signature: "sig".into(),
            },
            ContentBlock::Text {
                text: "answer".into(),
            },
        ];
        let provider = Provider::mock(vec![blocks.clone()]);
        let cfg = compaction_cfg(provider, 200_000, "thinking");
        let split = Arc::new(SplitUi {
            thinking: std::sync::Mutex::new(String::new()),
            text: std::sync::Mutex::new(String::new()),
        });
        let ui: Arc<dyn Ui> = split.clone();
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("think about it"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "answer");
        assert_eq!(history.messages()[1], Message::assistant(blocks));
        assert_eq!(*split.thinking.lock().unwrap(), "pondering");
        assert_eq!(*split.text.lock().unwrap(), "answer");
    }

    /// The task tool spawns a sub-agent that consumes its own turns from the
    /// same provider and returns its final text as the tool result.
    #[tokio::test]
    async fn subagent_roundtrip_returns_final_text() {
        let provider = Provider::mock(vec![
            // main agent round 1: spawn the sub-agent
            vec![ContentBlock::ToolUse {
                id: "t1".into(),
                name: "task".into(),
                input: json!({"prompt": "sub work"}),
            }],
            // consumed by the sub-agent's own run_turn
            vec![ContentBlock::Text {
                text: "sub result".into(),
            }],
            // main agent round 2: wrap up
            vec![ContentBlock::Text {
                text: "done".into(),
            }],
        ]);
        let cfg = compaction_cfg(provider, 200_000, "subagent");
        let ui: Arc<dyn Ui> = Arc::new(NullUi);
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("delegate"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "done");
        assert_eq!(
            history.messages()[2],
            Message::tool_results(vec![ContentBlock::ToolResult {
                tool_use_id: "t1".into(),
                content: "sub result".into(),
                is_error: false,
            }]),
            "the sub-agent's final text is the tool result"
        );
    }

    /// Steering typed during a round's tool execution is delivered as a framed
    /// user message at the NEXT round boundary — present in the next request,
    /// absent from the one already in flight, and never interleaved with the
    /// tool_result blocks.
    #[tokio::test]
    async fn steering_delivered_at_next_boundary_not_mid_request() {
        use kloop_provider::MockTurn;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        // Pushes one steer the first time any tool starts — i.e. during round
        // 0's dispatch, after round 0's request already went out.
        struct SteerOnToolUi {
            inbox: Arc<Inbox>,
            fired: AtomicBool,
        }
        impl Ui for SteerOnToolUi {
            fn text_delta(&self, _: &str) {}
            fn note(&self, _: &str) {}
            fn tool_start(&self, _: &str, _: &str, _: &str, _: &str) {
                if !self.fired.swap(true, Ordering::SeqCst) {
                    self.inbox
                        .push(InboxItem::Steer("also check the logs".into()));
                }
            }
        }

        let (provider, seen) = Provider::mock_recording(vec![
            MockTurn::Blocks(vec![tool_use("t1", "echo hi")]),
            MockTurn::Blocks(text("done")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "steer-boundary");
        let ui: Arc<dyn Ui> = Arc::new(SteerOnToolUi {
            inbox: cfg.inbox.clone(),
            fired: AtomicBool::new(false),
        });
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.rounds, 2);
        let steer = Message::user_text(format!("{STEERING_PREFIX}\nalso check the logs"));
        // [user go, assistant tool_use, user tool_results, user steer, assistant done]
        assert_eq!(
            history.messages()[3],
            steer,
            "the steer is a framed user message after the round's tool_results"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert!(
            !seen[0].messages.contains(&steer),
            "round 0's in-flight request predates the steer"
        );
        assert!(
            seen[1].messages.contains(&steer),
            "round 1's request carries the steer"
        );
    }

    /// A steer that lands during the FINAL sampling (a response with no tool
    /// calls) is absorbed by the end guard: the turn continues to address it
    /// instead of dropping it.
    #[tokio::test]
    async fn late_steering_keeps_the_turn_going() {
        use kloop_provider::MockTurn;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        struct SteerOnTextUi {
            inbox: Arc<Inbox>,
            fired: AtomicBool,
        }
        impl Ui for SteerOnTextUi {
            fn text_delta(&self, _: &str) {
                if !self.fired.swap(true, Ordering::SeqCst) {
                    self.inbox.push(InboxItem::Steer("wait, also do Y".into()));
                }
            }
            fn note(&self, _: &str) {}
        }

        let provider = Provider::mock_scripted(vec![
            MockTurn::Blocks(text("first attempt")),
            MockTurn::Blocks(text("addressed the steer")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "steer-late");
        let ui: Arc<dyn Ui> = Arc::new(SteerOnTextUi {
            inbox: cfg.inbox.clone(),
            fired: AtomicBool::new(false),
        });
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("start"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;

        assert_eq!(outcome.reason, EndReason::Completed);
        assert_eq!(outcome.final_text, "addressed the steer");
        assert_eq!(
            outcome.rounds, 2,
            "the late steer prevented ending at round 1"
        );
        let steer = Message::user_text(format!("{STEERING_PREFIX}\nwait, also do Y"));
        assert!(history.messages().contains(&steer));
        assert!(cfg.inbox.is_empty(), "the queue was drained");
    }

    /// A running sub-agent must not drain the PARENT's steering queue: each
    /// agent gets its own inbox (the task tool resets it on the cloned Config).
    /// A steer pushed to the parent while the sub-agent works is invisible to
    /// the sub-agent and delivered to the parent at its own next boundary.
    #[tokio::test]
    async fn subagent_does_not_drain_parent_steering() {
        use kloop_provider::MockRequest;
        use kloop_provider::MockTurn;
        use std::sync::atomic::AtomicBool;
        use std::sync::atomic::Ordering;

        // Pushes a parent steer the first time a SUB-agent (agent != "") starts
        // a tool — i.e. while the sub-agent is mid-turn.
        struct SteerParentUi {
            inbox: Arc<Inbox>,
            fired: AtomicBool,
        }
        impl Ui for SteerParentUi {
            fn text_delta(&self, _: &str) {}
            fn note(&self, _: &str) {}
            fn tool_start(&self, agent: &str, _: &str, _: &str, _: &str) {
                if !agent.is_empty() && !self.fired.swap(true, Ordering::SeqCst) {
                    self.inbox.push(InboxItem::Steer("parent steer".into()));
                }
            }
        }

        let (provider, seen) = Provider::mock_recording(vec![
            // parent round 0: spawn a sub-agent
            MockTurn::Blocks(vec![tool_use_named(
                "t1",
                "task",
                json!({"prompt": "sub work"}),
            )]),
            // sub round 0: run a tool (fires the parent steer mid-sub-turn)
            MockTurn::Blocks(vec![tool_use("s1", "echo hi")]),
            // sub round 1: finish
            MockTurn::Blocks(text("sub done")),
            // parent round 1: finish
            MockTurn::Blocks(text("done")),
        ]);
        let cfg = compaction_cfg(provider, 200_000, "steer-isolation");
        let ui: Arc<dyn Ui> = Arc::new(SteerParentUi {
            inbox: cfg.inbox.clone(),
            fired: AtomicBool::new(false),
        });
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("go"));

        let outcome = run_turn(&cfg, &mut history, &ui, &CancellationToken::new(), 0).await;
        assert_eq!(outcome.reason, EndReason::Completed);

        let has_steer = |req: &MockRequest| {
            req.messages.iter().any(|m| {
                m.content.iter().any(
                    |b| matches!(b, ContentBlock::Text { text } if text.starts_with(STEERING_PREFIX)),
                )
            })
        };
        let first_text = |req: &MockRequest| match req.messages[0].content.first() {
            Some(ContentBlock::Text { text }) => text.clone(),
            _ => String::new(),
        };
        let seen = seen.lock().unwrap();
        for req in seen.iter() {
            if first_text(req) == "sub work" {
                assert!(
                    !has_steer(req),
                    "the sub-agent must never see the parent's steer"
                );
            }
        }
        assert!(
            seen.iter()
                .any(|req| first_text(req) == "go" && has_steer(req)),
            "the parent delivers its own steer at its next boundary"
        );
    }
}
