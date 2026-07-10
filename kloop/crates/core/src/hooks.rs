//! External command hooks on four events: before/after a turn, before/after
//! a tool call. The event is JSON on the hook's stdin; exit code 0 lets the
//! action proceed, exit code 2 blocks it (pre_* events only; the reason is
//! read from stderr — stdout is the context channel). A block must be an
//! explicit signal: every other outcome — any other exit code, a spawn
//! failure, a timeout — is treated as a hook malfunction and fails OPEN with
//! a warning, because a broken hook script must not brick the agent (the
//! permission gate is the enforcement layer; hooks are automation policy on
//! top). Whatever an allowing hook prints on stdout is injected into history
//! as extra user-message context. Same semantics as cc's hooks, minus the
//! structured-JSON stdout protocol.

use std::process::Stdio;
use std::time::Duration;

use serde_json::json;
use serde_json::Value;
use tokio::io::AsyncWriteExt;

use crate::agent::Ui;

pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    PreTurn,
    PostTurn,
    PreTool,
    PostTool,
}

impl HookEvent {
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreTurn => "pre_turn",
            HookEvent::PostTurn => "post_turn",
            HookEvent::PreTool => "pre_tool",
            HookEvent::PostTool => "post_tool",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pre_turn" => Some(HookEvent::PreTurn),
            "post_turn" => Some(HookEvent::PostTurn),
            "pre_tool" => Some(HookEvent::PreTool),
            "post_tool" => Some(HookEvent::PostTool),
            _ => None,
        }
    }

    pub fn is_tool_event(self) -> bool {
        matches!(self, HookEvent::PreTool | HookEvent::PostTool)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookDef {
    pub event: HookEvent,
    /// argv; not passed through a shell.
    pub command: Vec<String>,
    /// Exact tool-name filter; None matches every tool. Only meaningful on
    /// tool events (config loading rejects it elsewhere).
    pub matcher: Option<String>,
    pub timeout_ms: u64,
}

/// The outcome of running every hook registered for one event, in config
/// order. A block short-circuits: later hooks don't run and contexts
/// gathered so far are dropped (the block reason is the only product).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HookDecision {
    Allow { context: Vec<String> },
    Block { reason: String },
}

/// The full hook set, shared via `Arc` in `Config` so sub-agents inherit it.
#[derive(Debug, Default)]
pub struct Hooks {
    pub defs: Vec<HookDef>,
}

/// Injected-context form: a user message the model can attribute to the hook
/// rather than to the human.
pub fn context_message(event: HookEvent, stdout: &str) -> String {
    format!("[{} hook]\n{}", event.name(), stdout.trim_end())
}

impl Hooks {
    pub fn none() -> Self {
        Hooks::default()
    }

    pub async fn pre_turn(&self, session_id: &str, ui: &dyn Ui) -> HookDecision {
        let event = json!({"event": "pre_turn", "session_id": session_id});
        self.run_event(HookEvent::PreTurn, None, &event, ui).await
    }

    pub async fn post_turn(&self, session_id: &str, ui: &dyn Ui) -> Vec<String> {
        let event = json!({"event": "post_turn", "session_id": session_id});
        match self.run_event(HookEvent::PostTurn, None, &event, ui).await {
            HookDecision::Allow { context } => context,
            HookDecision::Block { .. } => unreachable!("post events never block"),
        }
    }

    pub async fn pre_tool(
        &self,
        session_id: &str,
        tool_name: &str,
        tool_input: &Value,
        ui: &dyn Ui,
    ) -> HookDecision {
        let event = json!({
            "event": "pre_tool",
            "session_id": session_id,
            "tool_name": tool_name,
            "tool_input": tool_input,
        });
        self.run_event(HookEvent::PreTool, Some(tool_name), &event, ui)
            .await
    }

    pub async fn post_tool(
        &self,
        session_id: &str,
        tool_name: &str,
        tool_input: &Value,
        tool_result: &str,
        is_error: bool,
        ui: &dyn Ui,
    ) -> Vec<String> {
        let event = json!({
            "event": "post_tool",
            "session_id": session_id,
            "tool_name": tool_name,
            "tool_input": tool_input,
            "tool_result": tool_result,
            "is_error": is_error,
        });
        match self
            .run_event(HookEvent::PostTool, Some(tool_name), &event, ui)
            .await
        {
            HookDecision::Allow { context } => context,
            HookDecision::Block { .. } => unreachable!("post events never block"),
        }
    }

    async fn run_event(
        &self,
        event: HookEvent,
        tool_name: Option<&str>,
        payload: &Value,
        ui: &dyn Ui,
    ) -> HookDecision {
        let mut context = Vec::new();
        for def in &self.defs {
            if def.event != event {
                continue;
            }
            if let (Some(matcher), Some(tool)) = (&def.matcher, tool_name) {
                if matcher != tool {
                    continue;
                }
            }
            match run_hook(def, payload).await {
                HookRun::Allow { stdout } => {
                    if !stdout.trim().is_empty() {
                        context.push(context_message(event, &stdout));
                    }
                }
                HookRun::NonZero {
                    code,
                    stdout,
                    stderr,
                } => {
                    // Only the explicit block signal (exit 2 on a pre_*
                    // event) blocks; any other non-zero exit is a hook
                    // malfunction and fails open.
                    if code == BLOCK_EXIT_CODE
                        && matches!(event, HookEvent::PreTurn | HookEvent::PreTool)
                    {
                        return HookDecision::Block {
                            reason: block_reason(&stderr, &stdout),
                        };
                    }
                    ui.note(&format!(
                        "{} hook {:?} exited with {code}; proceeding (only exit {BLOCK_EXIT_CODE} blocks pre_* events)",
                        event.name(),
                        def.command
                    ));
                }
                HookRun::Failed(why) => {
                    ui.note(&format!(
                        "{} hook {:?} {why}; proceeding as allowed",
                        event.name(),
                        def.command
                    ));
                }
            }
        }
        HookDecision::Allow { context }
    }
}

/// cc's convention: the one exit code that means "deliberately blocked".
pub const BLOCK_EXIT_CODE: i32 = 2;

enum HookRun {
    Allow {
        stdout: String,
    },
    NonZero {
        code: i32,
        stdout: String,
        stderr: String,
    },
    /// Spawn failure or timeout — fail open.
    Failed(String),
}

/// The block reason the model (or user) sees: stderr is the designated
/// reason channel (stdout is for context injection), but a hook that spoke
/// only on stdout is still heard, and a silent one gets the bare status.
fn block_reason(stderr: &str, stdout: &str) -> String {
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let stdout = stdout.trim();
    if !stdout.is_empty() {
        return stdout.to_string();
    }
    format!("hook exited with status {BLOCK_EXIT_CODE}")
}

async fn run_hook(def: &HookDef, payload: &Value) -> HookRun {
    let run = async {
        let mut child = tokio::process::Command::new(&def.command[0])
            .args(&def.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("failed to spawn: {e}"))?;
        // The payload is tiny, so write-then-wait cannot deadlock on a full
        // stdout pipe. A hook that closed stdin early is not an error.
        if let Some(mut stdin) = child.stdin.take() {
            let mut line = payload.to_string();
            line.push('\n');
            let _ = stdin.write_all(line.as_bytes()).await;
        }
        child
            .wait_with_output()
            .await
            .map_err(|e| format!("failed to run: {e}"))
    };
    let output = match tokio::time::timeout(Duration::from_millis(def.timeout_ms), run).await {
        Err(_) => return HookRun::Failed(format!("timed out after {}ms", def.timeout_ms)),
        Ok(Err(why)) => return HookRun::Failed(why),
        Ok(Ok(output)) => output,
    };
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        return HookRun::Allow { stdout };
    }
    HookRun::NonZero {
        code: output.status.code().unwrap_or(-1),
        stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoteUi(std::sync::Mutex<Vec<String>>);
    impl Ui for NoteUi {
        fn text_delta(&self, _: &str) {}
        fn note(&self, s: &str) {
            self.0.lock().unwrap().push(s.to_string());
        }
    }

    fn note_ui() -> NoteUi {
        NoteUi(std::sync::Mutex::new(Vec::new()))
    }

    fn sh(event: HookEvent, script: &str) -> HookDef {
        HookDef {
            event,
            command: vec!["sh".into(), "-c".into(), script.into()],
            matcher: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    #[tokio::test]
    async fn allowing_hook_collects_stdout_as_context() {
        let hooks = Hooks {
            defs: vec![
                sh(HookEvent::PreTurn, "echo remember the style guide"),
                sh(HookEvent::PreTurn, "exit 0"), // silent: no context entry
            ],
        };
        let ui = note_ui();
        let decision = hooks.pre_turn("s-1", &ui).await;
        assert_eq!(
            decision,
            HookDecision::Allow {
                context: vec!["[pre_turn hook]\nremember the style guide".into()],
            }
        );
    }

    /// Exit 2 blocks; the reason channel is stderr, with stdout and the bare
    /// status as fallbacks.
    #[tokio::test]
    async fn exit_two_blocks_with_reason_from_stderr_then_stdout_then_status() {
        let ui = note_ui();
        let cases = [
            ("echo not now 1>&2; exit 2", "not now"),
            ("echo also-ctx; echo not now 1>&2; exit 2", "not now"),
            ("echo spoke on stdout; exit 2", "spoke on stdout"),
            ("exit 2", "hook exited with status 2"),
        ];
        for (script, want) in cases {
            let hooks = Hooks {
                defs: vec![sh(HookEvent::PreTool, script)],
            };
            let decision = hooks
                .pre_tool("s-1", "bash", &json!({"command": "rm -rf /"}), &ui)
                .await;
            assert_eq!(
                decision,
                HookDecision::Block {
                    reason: want.into()
                },
                "script: {script}"
            );
        }
    }

    /// Any non-zero exit other than 2 is a malfunction, not a block: warn
    /// and proceed, even on pre_* events.
    #[tokio::test]
    async fn other_nonzero_exits_fail_open_on_pre_events() {
        for script in ["echo broken 1>&2; exit 1", "exit 127"] {
            let hooks = Hooks {
                defs: vec![sh(HookEvent::PreTool, script)],
            };
            let ui = note_ui();
            let decision = hooks.pre_tool("s-1", "bash", &json!({}), &ui).await;
            assert_eq!(
                decision,
                HookDecision::Allow {
                    context: Vec::new()
                },
                "script: {script}"
            );
            let notes = ui.0.lock().unwrap();
            assert!(
                notes.iter().any(|n| n.contains("exited with")),
                "expected a malfunction warning, got {notes:?}"
            );
        }
    }

    /// A block short-circuits the chain: the later hook never runs.
    #[tokio::test]
    async fn block_short_circuits_later_hooks() {
        let marker =
            std::env::temp_dir().join(format!("kloop-hook-short-circuit-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let hooks = Hooks {
            defs: vec![
                sh(HookEvent::PreTool, "exit 2"),
                sh(HookEvent::PreTool, &format!("touch {}", marker.display())),
            ],
        };
        let ui = note_ui();
        let decision = hooks.pre_tool("s-1", "bash", &json!({}), &ui).await;
        assert!(matches!(decision, HookDecision::Block { .. }));
        assert!(!marker.exists(), "second hook must not have run");
    }

    /// The event JSON arrives on stdin with the documented fields.
    #[tokio::test]
    async fn event_json_reaches_the_hook_on_stdin() {
        let capture = std::env::temp_dir().join(format!("kloop-hook-stdin-{}", std::process::id()));
        let _ = std::fs::remove_file(&capture);
        let hooks = Hooks {
            defs: vec![sh(
                HookEvent::PostTool,
                &format!("cat > {}", capture.display()),
            )],
        };
        let ui = note_ui();
        hooks
            .post_tool(
                "s-9",
                "bash",
                &json!({"command": "ls"}),
                "file-a\nfile-b",
                false,
                &ui,
            )
            .await;
        let raw = std::fs::read_to_string(&capture).unwrap();
        let event: Value = serde_json::from_str(raw.trim()).unwrap();
        assert_eq!(
            event,
            json!({
                "event": "post_tool",
                "session_id": "s-9",
                "tool_name": "bash",
                "tool_input": {"command": "ls"},
                "tool_result": "file-a\nfile-b",
                "is_error": false,
            })
        );
        let _ = std::fs::remove_file(&capture);
    }

    #[tokio::test]
    async fn timeout_fails_open_with_a_warning() {
        let hooks = Hooks {
            defs: vec![HookDef {
                timeout_ms: 100,
                ..sh(HookEvent::PreTool, "sleep 30")
            }],
        };
        let ui = note_ui();
        let started = std::time::Instant::now();
        let decision = hooks.pre_tool("s-1", "bash", &json!({}), &ui).await;
        assert_eq!(
            decision,
            HookDecision::Allow {
                context: Vec::new()
            }
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        let notes = ui.0.lock().unwrap();
        assert!(
            notes.iter().any(|n| n.contains("timed out after 100ms")),
            "expected a timeout warning, got {notes:?}"
        );
    }

    #[tokio::test]
    async fn spawn_failure_fails_open_with_a_warning() {
        let hooks = Hooks {
            defs: vec![HookDef {
                event: HookEvent::PreTurn,
                command: vec!["/nonexistent/kloop-hook".into()],
                matcher: None,
                timeout_ms: DEFAULT_TIMEOUT_MS,
            }],
        };
        let ui = note_ui();
        let decision = hooks.pre_turn("s-1", &ui).await;
        assert_eq!(
            decision,
            HookDecision::Allow {
                context: Vec::new()
            }
        );
        let notes = ui.0.lock().unwrap();
        assert!(
            notes.iter().any(|n| n.contains("failed to spawn")),
            "expected a spawn warning, got {notes:?}"
        );
    }

    #[tokio::test]
    async fn matcher_filters_by_exact_tool_name() {
        let marker =
            std::env::temp_dir().join(format!("kloop-hook-matcher-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let hooks = Hooks {
            defs: vec![HookDef {
                matcher: Some("bash".into()),
                ..sh(
                    HookEvent::PreTool,
                    &format!("touch {}; exit 2", marker.display()),
                )
            }],
        };
        let ui = note_ui();

        // Non-matching tool: the hook does not even run.
        let decision = hooks
            .pre_tool("s-1", "read_file", &json!({"path": "x"}), &ui)
            .await;
        assert_eq!(
            decision,
            HookDecision::Allow {
                context: Vec::new()
            }
        );
        assert!(!marker.exists(), "hook must not run for read_file");

        // Matching tool: it runs and blocks.
        let decision = hooks.pre_tool("s-1", "bash", &json!({}), &ui).await;
        assert!(matches!(decision, HookDecision::Block { .. }));
        assert!(marker.exists());
        let _ = std::fs::remove_file(&marker);
    }

    /// Even the block exit code only warns on post events — blocking is a
    /// pre_* semantic.
    #[tokio::test]
    async fn post_event_nonzero_exit_warns_instead_of_blocking() {
        let hooks = Hooks {
            defs: vec![sh(HookEvent::PostTool, "echo ignored 1>&2; exit 2")],
        };
        let ui = note_ui();
        let context = hooks
            .post_tool("s-1", "bash", &json!({}), "out", false, &ui)
            .await;
        assert_eq!(context, Vec::<String>::new());
        let notes = ui.0.lock().unwrap();
        assert!(
            notes.iter().any(|n| n.contains("exited with 2")),
            "expected a non-zero warning, got {notes:?}"
        );
    }

    #[test]
    fn event_names_round_trip() {
        for event in [
            HookEvent::PreTurn,
            HookEvent::PostTurn,
            HookEvent::PreTool,
            HookEvent::PostTool,
        ] {
            assert_eq!(HookEvent::parse(event.name()), Some(event));
        }
        assert_eq!(HookEvent::parse("on_tool"), None);
    }
}
