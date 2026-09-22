//! Non-interactive headless mode (`--headless`): assemble the prompt from the
//! positional argument and/or piped stdin, run exactly one turn, then either
//! print the final text (human) or stream the run as a NDJSON event line stream
//! (`--json`) and exit 0/1 by outcome.
//!
//! Two deliberate contracts, both converged on by cc's `--print` and codex's
//! `exec` crate:
//! - **No approver is installed** ([`DenyApprover`]): any permission ask is
//!   auto-denied (fail-safe, like server mode's "reply lost = deny"). Loosen
//!   with `--permission-mode bypass` or previously persisted
//!   current-project allows, which act at earlier gate layers and never reach
//!   the approver.
//! - **`--json` reuses the server's wire shapes** verbatim (method + params,
//!   `thread_id` and all) — one event vocabulary, two front-ends.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use kloop_core::Config;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent::run_turn_with_input;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
#[cfg(test)]
use kloop_protocol::AssistantBlock;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

/// Combine the positional prompt and piped stdin into the turn's user message.
/// Either source alone is fine; both present are joined with a newline (cc's
/// shape, `main.tsx:1030-1061`). Neither present is an error — headless has
/// nothing to run.
pub(crate) fn assemble_prompt(positional: Option<&str>, stdin: Option<&str>) -> Result<String> {
    let positional = positional.map(str::trim).filter(|s| !s.is_empty());
    let stdin = stdin.map(str::trim).filter(|s| !s.is_empty());
    match (positional, stdin) {
        (Some(p), Some(s)) => Ok(format!("{p}\n{s}")),
        (Some(p), None) => Ok(p.to_string()),
        (None, Some(s)) => Ok(s.to_string()),
        (None, None) => {
            bail!("no prompt: pass one as an argument (kloop --headless \"…\") or pipe it on stdin")
        }
    }
}

/// Exit code by end reason: 0 only on a clean finish; error, interruption, and
/// hitting the round cap are all 1 (a script wants to know the task did not run
/// to completion). Mirrors cc (`is_error ? 1 : 0`, and max-turns yields an
/// error result) and codex (`error_seen`/Interrupted → 1).
pub(crate) fn exit_code(reason: &EndReason) -> i32 {
    match reason {
        EndReason::Completed => 0,
        EndReason::MaxRounds | EndReason::Aborted | EndReason::Error(_) => 1,
    }
}

/// Headless runs exactly one turn, so its item events all carry turn id 1.
const HEADLESS_TURN_ID: u64 = 1;

/// Headless permission answer: always deny. There is nobody at the keyboard, so
/// an ask that reaches this layer is refused (the safe default). Bypass/allow
/// flags act before the approver, so they still loosen the gate.
#[derive(Default)]
pub(crate) struct DenyApprover;

impl Approver for DenyApprover {
    fn confirm(
        &self,
        _req: ConfirmRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        Box::pin(async { Decision::Deny })
    }
}

/// The `--json` front-end: every UI event becomes one NDJSON line reusing the
/// server's notification method names and param shapes, `thread_id` injected. A
/// `Mutex` around the writer keeps concurrent sub-agent events from interleaving
/// mid-line.
struct JsonUi<W: Write + Send> {
    thread_id: String,
    out: Arc<Mutex<W>>,
}

impl<W: Write + Send> JsonUi<W> {
    fn notify(&self, method: &str, mut params: Value) {
        params["thread_id"] = Value::String(self.thread_id.clone());
        let line = json!({"method": method, "params": params});
        if let Ok(mut w) = self.out.lock() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
}

impl<W: Write + Send + 'static> Ui for JsonUi<W> {
    /// Project the core [`Event`] stream onto the NDJSON wire via the shared
    /// [`kloop_server::project_event`], so the headless `--json` stream and the
    /// server speak one item vocabulary. The turn bracket is emitted by
    /// `run_headless` (headless is one turn, id [`HEADLESS_TURN_ID`]).
    fn emit(&self, ev: &Event) {
        if let Some((method, params)) = kloop_server::project_event(ev, HEADLESS_TURN_ID) {
            self.notify(method, params);
        }
    }
}

/// The text front-end: the final answer goes to stdout once, so `$(kloop --headless …)`
/// captures a clean result. Progress (notes, tool calls) is human context and
/// goes to stderr; streamed assistant deltas are suppressed to avoid printing
/// the answer twice.
struct HeadlessTextUi;

impl Ui for HeadlessTextUi {
    fn emit(&self, ev: &Event) {
        // Streamed assistant text is suppressed (the final answer prints once to
        // stdout); every event with a note form goes to stderr as dim context.
        if let Some(note) = ev.as_note() {
            eprintln!("\x1b[2m[{note}]\x1b[0m");
        }
    }
}

/// The turn outcome plus the same UI sink used during the run. Session shutdown
/// reuses the sink so lifecycle events committed after `turn/completed` stay on
/// the selected text or NDJSON surface.
pub(crate) struct HeadlessResult {
    pub(crate) code: i32,
    pub(crate) ui: Arc<dyn Ui>,
}

/// Run one headless turn and return its result. `out` is the machine
/// channel (stdout): in `--json` mode every event lands there; otherwise the
/// final text does. Generic over the writer so the whole path is drivable
/// in-process over a buffer, like the server's contract tests.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_headless<W: Write + Send + 'static>(
    cfg: Arc<Config>,
    mut history: History,
    session_id: String,
    prompt: String,
    pending_images: Vec<ContentBlock>,
    json: bool,
    out: Arc<Mutex<W>>,
    cancel: CancellationToken,
) -> HeadlessResult {
    let msg = if pending_images.is_empty() {
        Message::user_text(prompt)
    } else {
        Message::user_with_blocks(prompt, pending_images)
    };
    // Staged, not recorded: interrupted before the model produces anything, the
    // prompt leaves no trace in the session file. Nothing to hand back here —
    // headless has one prompt and no one to give it to.
    if json {
        let ui = Arc::new(JsonUi {
            thread_id: session_id,
            out,
        });
        ui.notify(
            "turn/started",
            kloop_server::turn_started_params(HEADLESS_TURN_ID),
        );
        let dyn_ui: Arc<dyn Ui> = ui.clone();
        let (outcome, _returned) =
            run_turn_with_input(&cfg, &mut history, &dyn_ui, &cancel, 0, msg).await;
        // Report the post-turn context size, then close the bracket (server parity).
        ui.emit(&Event::Usage(history.estimated_tokens()));
        ui.notify(
            "turn/completed",
            kloop_server::turn_completed_params(HEADLESS_TURN_ID, &outcome.reason),
        );
        HeadlessResult {
            code: exit_code(&outcome.reason),
            ui: dyn_ui,
        }
    } else {
        let ui: Arc<dyn Ui> = Arc::new(HeadlessTextUi);
        let (outcome, _returned) =
            run_turn_with_input(&cfg, &mut history, &ui, &cancel, 0, msg).await;
        if !outcome.final_text.is_empty()
            && let Ok(mut w) = out.lock()
        {
            let _ = writeln!(w, "{}", outcome.final_text);
        }
        match &outcome.reason {
            EndReason::Completed => {}
            EndReason::MaxRounds => eprintln!("error: maximum rounds reached"),
            EndReason::Aborted => eprintln!("error: interrupted"),
            EndReason::Error(error) => eprintln!("error: {error}"),
        }
        HeadlessResult {
            code: exit_code(&outcome.reason),
            ui,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use kloop_core::permissions::Permissions;
    use kloop_protocol::AssistantOutcome;
    use kloop_provider::MockTurn;
    use kloop_provider::Provider;

    #[test]
    fn assemble_prompt_combines_sources() {
        assert_eq!(assemble_prompt(Some("fix it"), None).unwrap(), "fix it");
        assert_eq!(
            assemble_prompt(None, Some("from pipe")).unwrap(),
            "from pipe"
        );
        // Both present: positional first, then a newline, then stdin.
        assert_eq!(
            assemble_prompt(Some("summarize"), Some("long text")).unwrap(),
            "summarize\nlong text"
        );
        // Whitespace-only counts as absent; neither source is an error.
        assert_eq!(assemble_prompt(Some("  hi  "), Some("   ")).unwrap(), "hi");
        assert!(assemble_prompt(None, None).is_err());
        assert!(assemble_prompt(Some("   "), None).is_err());
    }

    #[test]
    fn exit_code_maps_end_reasons() {
        assert_eq!(exit_code(&EndReason::Completed), 0);
        assert_eq!(exit_code(&EndReason::MaxRounds), 1);
        assert_eq!(exit_code(&EndReason::Aborted), 1);
        assert_eq!(exit_code(&EndReason::Error("boom".into())), 1);
    }

    #[tokio::test]
    async fn deny_approver_refuses() {
        let req = ConfirmRequest {
            description: "bash: rm x".into(),
            approval_scopes: vec![kloop_core::permissions::ApprovalScope::Once],
            remember_rules: None,
            preview: None,
            ..Default::default()
        };
        assert_eq!(DenyApprover.confirm(req).await, Decision::Deny);
    }

    /// The `--json` stream is the item vocabulary the server projects (shared
    /// `kloop_server::project_event`), with `thread_id` injected. The exact item
    /// shapes are locked in the server's wire unit tests; here we check the
    /// front-end wiring: the projection is used and `thread_id` rides every line.
    #[test]
    fn json_ui_projects_the_item_vocabulary_with_thread_id() {
        use kloop_core::event::Delta;
        use kloop_core::event::Item;
        use kloop_core::event::ItemStatus;

        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let ui = JsonUi {
            thread_id: "t".into(),
            out: buf.clone(),
        };
        ui.notify(
            "turn/started",
            kloop_server::turn_started_params(HEADLESS_TURN_ID),
        );
        ui.emit(&Event::ItemDelta {
            id: "msg-0".into(),
            delta: Delta::Text("hi".into()),
        });
        ui.emit(&Event::ItemCompleted {
            id: "c1".into(),
            item: Item::ToolCall {
                agent: String::new(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
                status: ItemStatus::Completed,
                output: Some("ok".into()),
            },
        });
        ui.notify(
            "turn/completed",
            kloop_server::turn_completed_params(HEADLESS_TURN_ID, &EndReason::Completed),
        );

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            vec![
                r#"{"method":"turn/started","params":{"thread_id":"t","turn":{"id":1}}}"#,
                r#"{"method":"item/delta","params":{"channel":"text","item_id":"msg-0","text":"hi","thread_id":"t","turn_id":1}}"#,
                r#"{"method":"item/completed","params":{"item":{"id":"c1","input":{"command":"ls"},"name":"bash","output":"ok","status":"completed","type":"tool_call"},"thread_id":"t","turn_id":1}}"#,
                r#"{"method":"turn/completed","params":{"thread_id":"t","turn":{"id":1,"status":"completed"}}}"#,
            ]
        );
    }

    /// A minimal Config over a scripted Mock provider, enough to drive one
    /// headless turn in-process (mirrors the server contract-test factory).
    fn mock_config(turns: Vec<Vec<AssistantBlock>>) -> Config {
        mock_provider_config(Provider::mock(turns))
    }

    fn mock_scripted_config(turns: Vec<MockTurn>) -> Config {
        mock_provider_config(Provider::mock_scripted(turns))
    }

    fn mock_provider_config(provider: Provider) -> Config {
        let (provider_catalog, provider_route) =
            kloop_core::provider_route::ProviderCatalog::from_provider(
                "mock",
                provider,
                "mock",
                vec!["mock".into()],
            )
            .unwrap();
        let inbox = Arc::new(kloop_core::inbox::Inbox::default());
        Config {
            provider_catalog,
            provider_route,
            context_budget: kloop_core::config::ContextBudgetSource::Pinned,
            system: "test".into(),
            project_instructions: None,
            max_rounds: Some(10),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            offload_dir: std::env::temp_dir().join("kloop-headless-offload"),
            sessions_dir: std::env::temp_dir().join("kloop-headless-sessions"),
            context_window: None,
            permissions: Arc::new(Permissions::allow_all()),
            questioner: None,
            file_state: Default::default(),
            tool_sources: Vec::new(),
            session_id: "hl".into(),
            local_agent: kloop_core::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
            hooks: Arc::new(kloop_core::hooks::Hooks::none()),
            background_shells: kloop_core::tools::BackgroundShells::new(),
            shell_programs: std::sync::Arc::new(
                kloop_core::shell_programs::ShellPrograms::test_fixture(),
            ),
            powershell_execution_gate: Default::default(),
            background_executions: kloop_core::tools::BackgroundExecutions::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
            inbox: Arc::clone(&inbox),
            scheduler: kloop_core::scheduler::Scheduler::in_memory(inbox),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(
                kloop_core::worktree::ActiveWorktreeState::default(),
            ),
            surface: Default::default(),
        }
    }

    fn text_block(t: &str) -> AssistantBlock {
        AssistantBlock::Text { text: t.into() }
    }

    #[tokio::test]
    async fn text_mode_prints_final_answer_and_exits_zero() {
        let cfg = Arc::new(mock_config(vec![vec![text_block("the answer is 4")]]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg,
            history,
            "hl".into(),
            "what is 2+2".into(),
            Vec::new(),
            false,
            out.clone(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result.code, 0);
        assert_eq!(
            String::from_utf8(out.lock().unwrap().clone()).unwrap(),
            "the answer is 4\n"
        );
    }

    #[tokio::test]
    async fn text_mode_preserves_semantic_error_content_and_exits_one() {
        let cfg = Arc::new(mock_scripted_config(vec![MockTurn::Outcome {
            blocks: vec![text_block("partial refusal")],
            outcome: AssistantOutcome::Refused,
        }]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg,
            history,
            "hl".into(),
            "request".into(),
            Vec::new(),
            false,
            out.clone(),
            CancellationToken::new(),
        )
        .await;

        assert_eq!(result.code, 1);
        assert_eq!(
            String::from_utf8(out.lock().unwrap().clone()).unwrap(),
            "partial refusal\n"
        );
    }

    /// A dropped stream is transient, so the turn resumes and the caller gets the
    /// whole answer — the half already streamed is carried into the final text
    /// rather than replaced by the continuation.
    #[tokio::test]
    async fn text_mode_resumes_a_dropped_stream_and_keeps_both_halves() {
        let cfg = Arc::new(mock_scripted_config(vec![
            MockTurn::PartialError(vec![text_block("half answer")], "stream dropped".into()),
            MockTurn::Blocks(vec![text_block(" and the rest")]),
        ]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg,
            history,
            "hl".into(),
            "request".into(),
            Vec::new(),
            false,
            out.clone(),
            CancellationToken::new(),
        )
        .await;

        assert_eq!(result.code, 0);
        assert_eq!(
            String::from_utf8(out.lock().unwrap().clone()).unwrap(),
            "half answer and the rest\n"
        );
    }

    /// A fatal stream failure still exits one, and still prints what was produced
    /// before it — losing the partial is what the original regression was about.
    #[tokio::test]
    async fn text_mode_preserves_a_fatal_stream_partial_and_exits_one() {
        let cfg = Arc::new(mock_scripted_config(vec![MockTurn::BlocksThenError(
            vec![text_block("half answer")],
            kloop_provider::ProviderFailure::protocol("malformed frame"),
        )]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg,
            history,
            "hl".into(),
            "request".into(),
            Vec::new(),
            false,
            out.clone(),
            CancellationToken::new(),
        )
        .await;

        assert_eq!(result.code, 1);
        assert_eq!(
            String::from_utf8(out.lock().unwrap().clone()).unwrap(),
            "half answer\n"
        );
    }

    #[tokio::test]
    async fn text_mode_emits_nothing_for_an_empty_end_turn() {
        let cfg = Arc::new(mock_scripted_config(vec![MockTurn::Outcome {
            blocks: Vec::new(),
            outcome: AssistantOutcome::EndTurn,
        }]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg,
            history,
            "hl".into(),
            "request".into(),
            Vec::new(),
            false,
            out.clone(),
            CancellationToken::new(),
        )
        .await;

        assert_eq!(result.code, 0);
        assert!(out.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn json_shutdown_reuses_the_wire_sink_for_message_terminals() {
        let cfg = Arc::new(mock_config(vec![vec![text_block("done")]]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg.clone(),
            history,
            "hl".into(),
            "hi".into(),
            Vec::new(),
            true,
            out.clone(),
            CancellationToken::new(),
        )
        .await;
        let child = cfg.local_agent.child("agent-99".parse().unwrap());
        let _lease = child
            .register_child(
                Arc::new(kloop_core::inbox::Inbox::default()),
                None,
                "shutdown target",
                result.ui.clone(),
            )
            .unwrap();
        cfg.local_agent
            .send(
                child.agent_id().clone(),
                "shutdown".into(),
                "pending body".into(),
                &result.ui,
            )
            .unwrap();
        cfg.local_agent.shutdown(&result.ui);

        let raw = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let events = raw
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>();
        let updates = events
            .iter()
            .filter(|event| event["method"] == "thread/agent_message/updated")
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 2);
        assert_eq!(updates[0]["params"]["status"], "queued");
        assert_eq!(updates[1]["params"]["status"], "undeliverable");
        assert!(updates.iter().all(|event| {
            event["params"]["thread_id"] == "hl"
                && event["params"].get("message").is_none()
                && event["params"].get("body").is_none()
        }));
    }

    #[tokio::test]
    async fn json_mode_brackets_the_turn_with_lifecycle_events() {
        let cfg = Arc::new(mock_config(vec![vec![text_block("hello there")]]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let result = run_headless(
            cfg,
            history,
            "hl".into(),
            "hi".into(),
            Vec::new(),
            true,
            out.clone(),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(result.code, 0);

        let raw = String::from_utf8(out.lock().unwrap().clone()).unwrap();
        let events: Vec<Value> = raw
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let methods: Vec<&str> = events.iter().filter_map(|e| e["method"].as_str()).collect();
        // The assistant message is a full item lifecycle: started, deltas, then
        // completed (with the finalized text), unlike the old bare text/delta.
        assert_eq!(
            methods,
            vec![
                "turn/started",
                "item/started",
                "item/delta",
                "item/completed",
                // Twice: the agent loop publishes the context size at the end
                // of every round (a long turn's gauge must move while it runs),
                // then the headless bracket repeats the post-turn total.
                "thread/token_usage/updated",
                "thread/token_usage/updated",
                "turn/completed",
            ]
        );
        // Every event is thread-tagged with the session id (server parity).
        assert!(events.iter().all(|e| e["params"]["thread_id"] == "hl"));
        let deltas: String = events
            .iter()
            .filter(|e| e["method"] == "item/delta")
            .map(|e| e["params"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, "hello there");
        assert_eq!(
            events.last().unwrap()["params"]["turn"]["status"],
            "completed"
        );
    }
}
