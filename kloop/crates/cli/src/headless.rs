//! Non-interactive headless mode (`--headless`): assemble the prompt from the
//! positional argument and/or piped stdin, run exactly one turn, then either
//! print the final text (human) or stream the run as a NDJSON event line stream
//! (`--json`) and exit 0/1 by outcome.
//!
//! Two deliberate contracts, both converged on by cc's `--print` and codex's
//! `exec` crate:
//! - **No approver is installed** ([`DenyApprover`]): any permission ask is
//!   auto-denied (fail-safe, like server mode's "reply lost = deny"). Loosen
//!   with `--permission-mode accept-edits|bypass` / `KLOOP_ALLOW`, which act at earlier
//!   gate layers and never reach the approver.
//! - **`--json` reuses the server's wire shapes** verbatim (method + params,
//!   `threadId` and all) — one event vocabulary, two front-ends.

use std::io::Write;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::event::Event;
use kloop_core::history::History;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::Config;
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
/// server's notification method names and param shapes, `threadId` injected. A
/// `Mutex` around the writer keeps concurrent sub-agent events from interleaving
/// mid-line.
struct JsonUi<W: Write + Send> {
    thread_id: String,
    out: Arc<Mutex<W>>,
}

impl<W: Write + Send> JsonUi<W> {
    fn notify(&self, method: &str, mut params: Value) {
        params["threadId"] = Value::String(self.thread_id.clone());
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

/// Run one headless turn and return the process exit code. `out` is the machine
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
) -> i32 {
    let msg = if pending_images.is_empty() {
        Message::user_text(prompt)
    } else {
        Message::user_with_blocks(prompt, pending_images)
    };
    history.record(msg);

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
        let outcome = run_turn(&cfg, &mut history, &dyn_ui, &cancel, 0).await;
        // Report the post-turn context size, then close the bracket (server parity).
        ui.emit(&Event::Usage(history.estimated_tokens()));
        ui.notify(
            "turn/completed",
            kloop_server::turn_completed_params(HEADLESS_TURN_ID, &outcome.reason),
        );
        exit_code(&outcome.reason)
    } else {
        let ui: Arc<dyn Ui> = Arc::new(HeadlessTextUi);
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        if let Ok(mut w) = out.lock() {
            let _ = writeln!(w, "{}", outcome.final_text);
        }
        exit_code(&outcome.reason)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use kloop_core::permissions::Permissions;
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
            remember_rules: None,
            preview: None,
        };
        assert_eq!(DenyApprover.confirm(req).await, Decision::Deny);
    }

    /// The `--json` stream is the item vocabulary the server projects (shared
    /// `kloop_server::project_event`), with `threadId` injected. The exact item
    /// shapes are locked in the server's wire unit tests; here we check the
    /// front-end wiring: the projection is used and `threadId` rides every line.
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
                r#"{"method":"turn/started","params":{"threadId":"t","turn":{"id":1}}}"#,
                r#"{"method":"item/delta","params":{"channel":"text","itemId":"msg-0","text":"hi","threadId":"t","turnId":1}}"#,
                r#"{"method":"item/completed","params":{"item":{"id":"c1","input":{"command":"ls"},"name":"bash","output":"ok","status":"completed","type":"toolCall"},"threadId":"t","turnId":1}}"#,
                r#"{"method":"turn/completed","params":{"threadId":"t","turn":{"id":1,"status":"completed"}}}"#,
            ]
        );
    }

    /// A minimal Config over a scripted Mock provider, enough to drive one
    /// headless turn in-process (mirrors the server contract-test factory).
    fn mock_config(turns: Vec<Vec<ContentBlock>>) -> Config {
        Config {
            provider: Arc::new(Provider::mock(turns)),
            model: "mock".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: Some(10),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            offload_dir: std::env::temp_dir().join("kloop-headless-offload"),
            sessions_dir: std::env::temp_dir().join("kloop-headless-sessions"),
            context_window: None,
            fallback_model: None,
            permissions: Arc::new(Permissions::allow_all()),
            questioner: None,
            file_state: Default::default(),
            tool_sources: Vec::new(),
            session_id: "hl".into(),
            agent_label: String::new(),
            hooks: Arc::new(kloop_core::hooks::Hooks::none()),
            background_shells: kloop_core::tools::BackgroundShells::new(),
            shell_programs: std::sync::Arc::new(
                kloop_core::shell_programs::ShellPrograms::test_fixture(),
            ),
            background_tasks: kloop_core::tools::BackgroundTasks::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
            inbox: Default::default(),
            scheduler: kloop_core::scheduler::Scheduler::in_memory(Default::default()),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(
                kloop_core::worktree::ActiveWorktreeState::default(),
            ),
            surface: Default::default(),
        }
    }

    fn text_block(t: &str) -> ContentBlock {
        ContentBlock::Text { text: t.into() }
    }

    #[tokio::test]
    async fn text_mode_prints_final_answer_and_exits_zero() {
        let cfg = Arc::new(mock_config(vec![vec![text_block("the answer is 4")]]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let code = run_headless(
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
        assert_eq!(code, 0);
        assert_eq!(
            String::from_utf8(out.lock().unwrap().clone()).unwrap(),
            "the answer is 4\n"
        );
    }

    #[tokio::test]
    async fn json_mode_brackets_the_turn_with_lifecycle_events() {
        let cfg = Arc::new(mock_config(vec![vec![text_block("hello there")]]));
        let history = History::new(cfg.offload_dir.clone());
        let out = Arc::new(Mutex::new(Vec::<u8>::new()));
        let code = run_headless(
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
        assert_eq!(code, 0);

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
                "thread/tokenUsage/updated",
                "turn/completed",
            ]
        );
        // Every event is thread-tagged with the session id (server parity).
        assert!(events.iter().all(|e| e["params"]["threadId"] == "hl"));
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
