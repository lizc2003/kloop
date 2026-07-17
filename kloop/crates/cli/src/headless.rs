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

/// The `turn/completed` notification params for a turn's end reason, matching
/// the server's `turn_completed_params` shape verbatim (one wire, two uses).
fn turn_completed_params(reason: &EndReason) -> Value {
    match reason {
        EndReason::Completed => json!({"reason": "completed"}),
        EndReason::MaxRounds => json!({"reason": "maxRounds"}),
        EndReason::Aborted => json!({"reason": "aborted"}),
        EndReason::Error(e) => json!({"reason": "error", "message": e}),
    }
}

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
    fn emit(&self, method: &str, mut params: Value) {
        params["threadId"] = Value::String(self.thread_id.clone());
        let line = json!({"method": method, "params": params});
        if let Ok(mut w) = self.out.lock() {
            let _ = writeln!(w, "{line}");
            let _ = w.flush();
        }
    }
}

impl<W: Write + Send + 'static> Ui for JsonUi<W> {
    fn text_delta(&self, s: &str) {
        self.emit("text/delta", json!({"text": s}));
    }

    fn note(&self, s: &str) {
        self.emit("note", json!({"text": s}));
    }

    fn tool_start(&self, agent: &str, id: &str, name: &str, summary: &str, _input: &Value) {
        let mut params = json!({"callId": id, "name": name, "summary": summary});
        if !agent.is_empty() {
            params["agent"] = Value::String(agent.to_string());
        }
        self.emit("tool/started", params);
    }

    fn tool_end(&self, agent: &str, id: &str, ok: bool, _output: &str) {
        let mut params = json!({"callId": id, "ok": ok});
        if !agent.is_empty() {
            params["agent"] = Value::String(agent.to_string());
        }
        self.emit("tool/completed", params);
    }

    fn agent_start(&self, agent: &str, task: &str) {
        self.emit("agent/started", json!({"agent": agent, "task": task}));
    }

    fn agent_end(&self, agent: &str, ok: bool) {
        self.emit("agent/completed", json!({"agent": agent, "ok": ok}));
    }

    fn todo_update(&self, agent: &str, todos: &[kloop_core::tools::TodoItem]) {
        let mut params = json!({"todos": todos});
        if !agent.is_empty() {
            params["agent"] = Value::String(agent.to_string());
        }
        self.emit("todo/updated", params);
    }
}

/// The text front-end: the final answer goes to stdout once, so `$(kloop --headless …)`
/// captures a clean result. Progress (notes, tool calls) is human context and
/// goes to stderr; streamed assistant deltas are suppressed to avoid printing
/// the answer twice.
struct HeadlessTextUi;

impl Ui for HeadlessTextUi {
    fn text_delta(&self, _s: &str) {}

    fn note(&self, s: &str) {
        eprintln!("\x1b[2m[{s}]\x1b[0m");
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
        ui.emit("turn/started", json!({}));
        let dyn_ui: Arc<dyn Ui> = ui.clone();
        let outcome = run_turn(&cfg, &mut history, &dyn_ui, &cancel, 0).await;
        ui.emit("turn/completed", turn_completed_params(&outcome.reason));
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

    /// The `--json` events must be byte-for-byte the server's notification
    /// shapes (sorted keys, injected `threadId`) so one client vocabulary
    /// serves both front-ends.
    #[test]
    fn json_ui_matches_server_wire_shapes() {
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        let ui = JsonUi {
            thread_id: "t".into(),
            out: buf.clone(),
        };
        ui.emit("turn/started", json!({}));
        ui.text_delta("hi");
        ui.note("saved");
        ui.tool_start("", "c1", "bash", "ls", &json!({"command": "ls"}));
        ui.tool_start("agent-1", "c2", "grep", "x", &json!({"pattern": "x"}));
        ui.tool_end("", "c1", true, "ok");
        ui.agent_start("agent-1", "do x");
        ui.agent_end("agent-1", false);
        ui.emit(
            "turn/completed",
            turn_completed_params(&EndReason::Completed),
        );

        let out = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines,
            vec![
                r#"{"method":"turn/started","params":{"threadId":"t"}}"#,
                r#"{"method":"text/delta","params":{"text":"hi","threadId":"t"}}"#,
                r#"{"method":"note","params":{"text":"saved","threadId":"t"}}"#,
                r#"{"method":"tool/started","params":{"callId":"c1","name":"bash","summary":"ls","threadId":"t"}}"#,
                r#"{"method":"tool/started","params":{"agent":"agent-1","callId":"c2","name":"grep","summary":"x","threadId":"t"}}"#,
                r#"{"method":"tool/completed","params":{"callId":"c1","ok":true,"threadId":"t"}}"#,
                r#"{"method":"agent/started","params":{"agent":"agent-1","task":"do x","threadId":"t"}}"#,
                r#"{"method":"agent/completed","params":{"agent":"agent-1","ok":false,"threadId":"t"}}"#,
                r#"{"method":"turn/completed","params":{"reason":"completed","threadId":"t"}}"#,
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
            max_rounds: 10,
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            offload_dir: std::env::temp_dir().join("kloop-headless-offload"),
            sessions_dir: std::env::temp_dir().join("kloop-headless-sessions"),
            context_window: None,
            fallback_model: None,
            permissions: Arc::new(Permissions::allow_all()),
            tool_sources: Vec::new(),
            session_id: "hl".into(),
            agent_label: String::new(),
            hooks: Arc::new(kloop_core::hooks::Hooks::none()),
            background_shells: kloop_core::tools::BackgroundShells::new(),
            background_tasks: kloop_core::tools::BackgroundTasks::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
            inbox: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(std::sync::RwLock::new(None)),
            worktree_enabled: false,
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
        assert_eq!(
            methods,
            vec!["turn/started", "text/delta", "turn/completed"]
        );
        // Every event is thread-tagged with the session id (server parity).
        assert!(events.iter().all(|e| e["params"]["threadId"] == "hl"));
        let deltas: String = events
            .iter()
            .filter(|e| e["method"] == "text/delta")
            .map(|e| e["params"]["text"].as_str().unwrap())
            .collect();
        assert_eq!(deltas, "hello there");
        assert_eq!(events.last().unwrap()["params"]["reason"], "completed");
    }
}
