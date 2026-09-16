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

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::agent::Ui;

pub const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// The `agent_type` a sub-agent reports when it was started without one
/// (`run_agent` omitted `agent_type`, or a `fork` skill spawned it). Codex
/// (`DEFAULT_ROLE_NAME` = "default") and deepseek-harness (`SUBAGENT_TYPE` =
/// "general-purpose") both substitute a real name here instead of letting
/// "no type" reach the matcher; cc and grok instead treat it as "no value"
/// and fire every hook, matcher or not. The name is deliberate: a matcher
/// never silently stops applying, and `matcher = "default"` selects exactly
/// the untyped sub-agents — which the fail-open shape cannot express. It is
/// also in the payload, so a hook reads the name instead of learning it from
/// documentation.
pub const DEFAULT_AGENT_TYPE: &str = "default";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookEvent {
    PreTurn,
    PostTurn,
    PreTool,
    PostTool,
    /// A sub-agent's turn start/end. A sub-agent fires these INSTEAD of
    /// pre_turn/post_turn (its main-agent counterparts) — the shape both cc
    /// (Stop→SubagentStop) and codex ("child turns run SubagentStop") agree
    /// on. subagent_stop carries the sub-agent's own transcript path (plan 17
    /// slice 3) and its final message, so an audit/notification hook gets "this
    /// sub-agent finished, here is its record and result".
    SubagentStart,
    SubagentStop,
}

impl HookEvent {
    pub fn name(self) -> &'static str {
        match self {
            HookEvent::PreTurn => "pre_turn",
            HookEvent::PostTurn => "post_turn",
            HookEvent::PreTool => "pre_tool",
            HookEvent::PostTool => "post_tool",
            HookEvent::SubagentStart => "subagent_start",
            HookEvent::SubagentStop => "subagent_stop",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pre_turn" => Some(HookEvent::PreTurn),
            "post_turn" => Some(HookEvent::PostTurn),
            "pre_tool" => Some(HookEvent::PreTool),
            "post_tool" => Some(HookEvent::PostTool),
            "subagent_start" => Some(HookEvent::SubagentStart),
            "subagent_stop" => Some(HookEvent::SubagentStop),
            _ => None,
        }
    }

    /// What a `matcher` on this event is compared against, as it reads in a
    /// config error; None on the events that have no such subject, where a
    /// matcher is a config error rather than a filter that quietly never
    /// applies. Tool events match the tool name and sub-agent events match the
    /// agent type — the split cc (`hooks.ts:1779`), codex
    /// (`HOOK_EVENT_NAMES_WITH_MATCHERS`) and grok (`match_value`) all make.
    pub fn matcher_subject(self) -> Option<&'static str> {
        match self {
            HookEvent::PreTool | HookEvent::PostTool => Some("tool name"),
            HookEvent::SubagentStart | HookEvent::SubagentStop => Some("agent type"),
            HookEvent::PreTurn | HookEvent::PostTurn => None,
        }
    }

    /// Events on which exit code 2 is an explicit block (the rest fail open).
    /// The "start"/"pre" events, where blocking still means something.
    fn can_block(self) -> bool {
        matches!(
            self,
            HookEvent::PreTurn | HookEvent::PreTool | HookEvent::SubagentStart
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookDef {
    pub event: HookEvent,
    /// argv; not passed through a shell.
    pub command: Vec<String>,
    /// Exact filter on the event's matcher subject — the tool name on
    /// pre_tool/post_tool, the agent type on subagent_start/subagent_stop
    /// (see [`HookEvent::matcher_subject`]). None matches every subject;
    /// config loading rejects a matcher on the events that have none.
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

/// Add the `agent` field to a tool-event payload only when it names a
/// sub-agent, so main-agent payloads stay byte-identical (cc/codex both omit
/// the sub-agent id on main-thread tool events).
fn with_agent(mut event: Value, agent: &str) -> Value {
    if !agent.is_empty() {
        event["agent"] = Value::String(agent.to_string());
    }
    event
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

    /// `agent` is the sub-agent label ("agent-N") when a sub-agent's tool call
    /// triggered this, or "" for the main agent. Both cc and codex carry an
    /// `agent_id` present only on sub-agent tool events; kloop mirrors that by
    /// adding the `agent` field only when non-empty, so main-agent payloads
    /// stay byte-identical.
    pub async fn pre_tool(
        &self,
        session_id: &str,
        agent: &str,
        tool_name: &str,
        tool_input: &Value,
        ui: &dyn Ui,
    ) -> HookDecision {
        let event = with_agent(
            json!({
                "event": "pre_tool",
                "session_id": session_id,
                "tool_name": tool_name,
                "tool_input": tool_input,
            }),
            agent,
        );
        self.run_event(HookEvent::PreTool, Some(tool_name), &event, ui)
            .await
    }

    // The event fields plus the agent label and ui sink add up past the lint's
    // threshold; they are all distinct primitives, so a params struct would
    // only add ceremony.
    #[allow(clippy::too_many_arguments)]
    pub async fn post_tool(
        &self,
        session_id: &str,
        agent: &str,
        tool_name: &str,
        tool_input: &Value,
        tool_result: &str,
        is_error: bool,
        ui: &dyn Ui,
    ) -> Vec<String> {
        let event = with_agent(
            json!({
                "event": "post_tool",
                "session_id": session_id,
                "tool_name": tool_name,
                "tool_input": tool_input,
                "tool_result": tool_result,
                "is_error": is_error,
            }),
            agent,
        );
        match self
            .run_event(HookEvent::PostTool, Some(tool_name), &event, ui)
            .await
        {
            HookDecision::Allow { context } => context,
            HookDecision::Block { .. } => unreachable!("post events never block"),
        }
    }

    /// A sub-agent's turn is starting (its counterpart to pre_turn). Fires only
    /// for sub-agents; `agent` is the label ("agent-N"), `agent_type` the type
    /// `run_agent` dispatched to (None becomes [`DEFAULT_AGENT_TYPE`], which is
    /// what a matcher sees). Can block like pre_turn — a policy hook may refuse
    /// to let a sub-agent run.
    pub async fn subagent_start(
        &self,
        session_id: &str,
        agent: &str,
        agent_type: Option<&str>,
        ui: &dyn Ui,
    ) -> HookDecision {
        let agent_type = agent_type.unwrap_or(DEFAULT_AGENT_TYPE);
        let event = json!({
            "event": "subagent_start",
            "session_id": session_id,
            "agent": agent,
            "agent_type": agent_type,
        });
        self.run_event(HookEvent::SubagentStart, Some(agent_type), &event, ui)
            .await
    }

    /// A sub-agent's turn has ended (its counterpart to post_turn). Carries the
    /// agent type a matcher filters on (see [`Hooks::subagent_start`]), the
    /// sub-agent's own transcript path (its session file, plan 17 slice 3; None
    /// when the sub-agent ran in-memory) and its final assistant message, so an
    /// audit/notification hook gets the record and the result. Context-only,
    /// never blocks.
    pub async fn subagent_stop(
        &self,
        session_id: &str,
        agent: &str,
        agent_type: Option<&str>,
        agent_transcript_path: Option<&Path>,
        last_assistant_message: &str,
        ui: &dyn Ui,
    ) -> Vec<String> {
        let agent_type = agent_type.unwrap_or(DEFAULT_AGENT_TYPE);
        let mut event = json!({
            "event": "subagent_stop",
            "session_id": session_id,
            "agent": agent,
            "agent_type": agent_type,
            "last_assistant_message": last_assistant_message,
        });
        if let Some(path) = agent_transcript_path {
            event["agent_transcript_path"] = json!(path.to_string_lossy());
        }
        match self
            .run_event(HookEvent::SubagentStop, Some(agent_type), &event, ui)
            .await
        {
            HookDecision::Allow { context } => context,
            HookDecision::Block { .. } => unreachable!("subagent_stop never blocks"),
        }
    }

    /// `subject` is what a `matcher` is compared against on this event (see
    /// [`HookEvent::matcher_subject`]); it is Some on every event that accepts
    /// one, so a configured matcher always decides. The None arm therefore
    /// only covers pre_turn/post_turn, where config loading rejects matchers
    /// outright — a `HookDef` built directly in code can still reach it, and
    /// there an inapplicable matcher fails open.
    async fn run_event(
        &self,
        event: HookEvent,
        subject: Option<&str>,
        payload: &Value,
        ui: &dyn Ui,
    ) -> HookDecision {
        let mut context = Vec::new();
        for def in &self.defs {
            if def.event != event {
                continue;
            }
            if let (Some(matcher), Some(subject)) = (&def.matcher, subject)
                && matcher != subject
            {
                continue;
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
                    // Only the explicit block signal (exit 2 on a blocking
                    // event) blocks; any other non-zero exit is a hook
                    // malfunction and fails open.
                    if code == BLOCK_EXIT_CODE && event.can_block() {
                        return HookDecision::Block {
                            reason: block_reason(&stderr, &stdout),
                        };
                    }
                    ui.emit(&crate::event::Event::Note(format!(
                        "{} hook {:?} exited with {code}; proceeding (only exit {BLOCK_EXIT_CODE} blocks pre_*/subagent_start events)",
                        event.name(),
                        def.command
                    )));
                }
                HookRun::Failed(why) => {
                    ui.emit(&crate::event::Event::Note(format!(
                        "{} hook {:?} {why}; proceeding as allowed",
                        event.name(),
                        def.command
                    )));
                }
            }
        }
        HookDecision::Allow { context }
    }
}

/// cc's convention: the one exit code that means "deliberately blocked".
pub const BLOCK_EXIT_CODE: i32 = 2;

#[cfg(test)]
pub(crate) fn test_shell_command(script: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        let (programs, _) = crate::shell_programs::resolve_shell_programs(Default::default())
            .expect("native Windows shell discovery succeeds");
        let bash = programs
            .bash
            .expect("hook tests require a validated Git for Windows Bash");
        vec![
            bash.executable.to_string_lossy().into_owned(),
            "-lc".into(),
            script.replace('\\', "/"),
        ]
    }
    #[cfg(not(windows))]
    {
        vec!["sh".into(), "-c".into(), script.into()]
    }
}

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
        let mut command = tokio::process::Command::new(&def.command[0]);
        command
            .args(&def.command[1..])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = kloop_process_spawn::spawn(&mut command)
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
        fn emit(&self, ev: &crate::event::Event) {
            if let crate::event::Event::Note(s) = ev {
                self.0.lock().unwrap().push(s.to_string());
            }
        }
    }

    fn note_ui() -> NoteUi {
        NoteUi(std::sync::Mutex::new(Vec::new()))
    }

    fn sh(event: HookEvent, script: &str) -> HookDef {
        HookDef {
            event,
            command: test_shell_command(script),
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
                .pre_tool("s-1", "", "bash", &json!({"command": "rm -rf /"}), &ui)
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
            let decision = hooks.pre_tool("s-1", "", "bash", &json!({}), &ui).await;
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
        let decision = hooks.pre_tool("s-1", "", "bash", &json!({}), &ui).await;
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
                "",
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
        let decision = hooks.pre_tool("s-1", "", "bash", &json!({}), &ui).await;
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
            .pre_tool("s-1", "", "read_file", &json!({"path": "x"}), &ui)
            .await;
        assert_eq!(
            decision,
            HookDecision::Allow {
                context: Vec::new()
            }
        );
        assert!(!marker.exists(), "hook must not run for read_file");

        // Matching tool: it runs and blocks.
        let decision = hooks.pre_tool("s-1", "", "bash", &json!({}), &ui).await;
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
            .post_tool("s-1", "", "bash", &json!({}), "out", false, &ui)
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
            HookEvent::SubagentStart,
            HookEvent::SubagentStop,
        ] {
            assert_eq!(HookEvent::parse(event.name()), Some(event));
        }
        assert_eq!(HookEvent::parse("on_tool"), None);
    }

    /// A sub-agent's tool call carries the `agent` field; the main agent's does
    /// not (byte-identical to before).
    #[tokio::test]
    async fn tool_event_carries_agent_only_for_subagents() {
        let capture = std::env::temp_dir().join(format!("kloop-hook-agent-{}", std::process::id()));
        let read_event = |path: &Path| -> Value {
            serde_json::from_str(std::fs::read_to_string(path).unwrap().trim()).unwrap()
        };
        let ui = note_ui();

        // Sub-agent: `agent` present.
        let _ = std::fs::remove_file(&capture);
        let hooks = Hooks {
            defs: vec![sh(
                HookEvent::PreTool,
                &format!("cat > {}", capture.display()),
            )],
        };
        hooks
            .pre_tool("s-1", "agent-2", "bash", &json!({"command": "ls"}), &ui)
            .await;
        assert_eq!(read_event(&capture)["agent"], "agent-2");

        // Main agent: no `agent` key at all.
        let _ = std::fs::remove_file(&capture);
        hooks
            .pre_tool("s-1", "", "bash", &json!({"command": "ls"}), &ui)
            .await;
        assert_eq!(read_event(&capture).get("agent"), None);
        let _ = std::fs::remove_file(&capture);
    }

    /// subagent_start can block (exit 2), just like pre_turn — a policy hook
    /// may refuse to let a sub-agent run.
    #[tokio::test]
    async fn subagent_start_can_block() {
        let hooks = Hooks {
            defs: vec![sh(
                HookEvent::SubagentStart,
                "echo no subagents 1>&2; exit 2",
            )],
        };
        let ui = note_ui();
        let decision = hooks
            .subagent_start("s-1", "agent-1", Some("reviewer"), &ui)
            .await;
        assert_eq!(
            decision,
            HookDecision::Block {
                reason: "no subagents".into()
            }
        );
    }

    /// subagent_stop delivers the agent label, its own transcript path, and the
    /// final assistant message; it never blocks (exit 2 only warns).
    #[tokio::test]
    async fn subagent_stop_payload_and_never_blocks() {
        let capture =
            std::env::temp_dir().join(format!("kloop-hook-substop-{}", std::process::id()));
        let _ = std::fs::remove_file(&capture);
        let hooks = Hooks {
            defs: vec![sh(
                HookEvent::SubagentStop,
                &format!("cat > {}; exit 2", capture.display()),
            )],
        };
        let ui = note_ui();
        let transcript = std::path::PathBuf::from("/tmp/sessions/parent-agent-1.jsonl");
        let context = hooks
            .subagent_stop(
                "parent",
                "agent-1",
                Some("reviewer"),
                Some(&transcript),
                "the sub-agent's answer",
                &ui,
            )
            .await;
        // exit 2 on a stop event is a malfunction, not a block: context is empty.
        assert_eq!(context, Vec::<String>::new());
        let event: Value =
            serde_json::from_str(std::fs::read_to_string(&capture).unwrap().trim()).unwrap();
        assert_eq!(
            event,
            json!({
                "event": "subagent_stop",
                "session_id": "parent",
                "agent": "agent-1",
                "agent_type": "reviewer",
                "agent_transcript_path": "/tmp/sessions/parent-agent-1.jsonl",
                "last_assistant_message": "the sub-agent's answer",
            })
        );
        let _ = std::fs::remove_file(&capture);
    }

    /// An in-memory sub-agent (no session file) omits the transcript path.
    #[tokio::test]
    async fn subagent_stop_omits_transcript_when_in_memory() {
        let capture =
            std::env::temp_dir().join(format!("kloop-hook-substop-mem-{}", std::process::id()));
        let _ = std::fs::remove_file(&capture);
        let hooks = Hooks {
            defs: vec![sh(
                HookEvent::SubagentStop,
                &format!("cat > {}", capture.display()),
            )],
        };
        let ui = note_ui();
        hooks
            .subagent_stop("parent", "agent-1", Some("reviewer"), None, "answer", &ui)
            .await;
        let event: Value =
            serde_json::from_str(std::fs::read_to_string(&capture).unwrap().trim()).unwrap();
        assert_eq!(event.get("agent_transcript_path"), None);
        let _ = std::fs::remove_file(&capture);
    }

    /// Every event's stdin payload, pinned whole. The four main-agent events
    /// must stay byte-identical to what cc/codex send — that is what
    /// `with_agent` exists for — so any added field has to fail here. The two
    /// sub-agent events are kloop's own shape and carry `agent_type`, the
    /// subject their matcher filters on.
    #[tokio::test]
    async fn every_event_payload_is_pinned() {
        let capture =
            std::env::temp_dir().join(format!("kloop-hook-pinned-{}", std::process::id()));
        let _ = std::fs::remove_file(&capture);
        let hook = |event: HookEvent| sh(event, &format!("cat > {}", capture.display()));
        let taken = || -> Value {
            let raw = std::fs::read_to_string(&capture).expect("the hook captured a payload");
            std::fs::remove_file(&capture).unwrap();
            serde_json::from_str(raw.trim()).unwrap()
        };
        let ui = note_ui();
        let hooks = Hooks {
            defs: vec![
                hook(HookEvent::PreTurn),
                hook(HookEvent::PostTurn),
                hook(HookEvent::PreTool),
                hook(HookEvent::PostTool),
                hook(HookEvent::SubagentStart),
                hook(HookEvent::SubagentStop),
            ],
        };
        let input = json!({"command": "ls"});

        hooks.pre_turn("s-1", &ui).await;
        assert_eq!(taken(), json!({"event": "pre_turn", "session_id": "s-1"}));

        hooks.post_turn("s-1", &ui).await;
        assert_eq!(taken(), json!({"event": "post_turn", "session_id": "s-1"}));

        hooks.pre_tool("s-1", "", "bash", &input, &ui).await;
        assert_eq!(
            taken(),
            json!({
                "event": "pre_tool",
                "session_id": "s-1",
                "tool_name": "bash",
                "tool_input": {"command": "ls"},
            })
        );

        hooks.pre_tool("s-1", "agent-3", "bash", &input, &ui).await;
        assert_eq!(
            taken(),
            json!({
                "event": "pre_tool",
                "session_id": "s-1",
                "tool_name": "bash",
                "tool_input": {"command": "ls"},
                "agent": "agent-3",
            })
        );

        hooks
            .post_tool("s-1", "", "bash", &input, "out", false, &ui)
            .await;
        assert_eq!(
            taken(),
            json!({
                "event": "post_tool",
                "session_id": "s-1",
                "tool_name": "bash",
                "tool_input": {"command": "ls"},
                "tool_result": "out",
                "is_error": false,
            })
        );

        hooks
            .post_tool("s-1", "agent-3", "bash", &input, "out", true, &ui)
            .await;
        assert_eq!(
            taken(),
            json!({
                "event": "post_tool",
                "session_id": "s-1",
                "tool_name": "bash",
                "tool_input": {"command": "ls"},
                "tool_result": "out",
                "is_error": true,
                "agent": "agent-3",
            })
        );

        hooks
            .subagent_start("s-1", "agent-3", Some("explorer"), &ui)
            .await;
        assert_eq!(
            taken(),
            json!({
                "event": "subagent_start",
                "session_id": "s-1",
                "agent": "agent-3",
                "agent_type": "explorer",
            })
        );

        // No type is not "no value": it is the DEFAULT_AGENT_TYPE name, and the
        // hook reads it from the payload rather than from documentation.
        hooks.subagent_start("s-1", "agent-4", None, &ui).await;
        assert_eq!(
            taken(),
            json!({
                "event": "subagent_start",
                "session_id": "s-1",
                "agent": "agent-4",
                "agent_type": "default",
            })
        );

        hooks
            .subagent_stop("s-1", "agent-4", None, None, "done", &ui)
            .await;
        assert_eq!(
            taken(),
            json!({
                "event": "subagent_stop",
                "session_id": "s-1",
                "agent": "agent-4",
                "agent_type": "default",
                "last_assistant_message": "done",
            })
        );
    }

    /// The plan 17 debt: a matcher on a sub-agent event filters by agent TYPE.
    /// The type never used to reach `run_event`, so a configured matcher fired
    /// for every type — a filter that was neither an error nor a filter.
    #[tokio::test]
    async fn subagent_matcher_filters_by_agent_type() {
        let marker =
            std::env::temp_dir().join(format!("kloop-hook-subtype-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let fired = || {
            let hit = marker.exists();
            let _ = std::fs::remove_file(&marker);
            hit
        };
        let touch = format!("touch {}", marker.display());
        let ui = note_ui();
        let hooks = Hooks {
            defs: vec![HookDef {
                matcher: Some("reviewer".into()),
                ..sh(HookEvent::SubagentStart, &touch)
            }],
        };

        hooks
            .subagent_start("s-1", "agent-1", Some("reviewer"), &ui)
            .await;
        assert!(fired(), "the named type fires");

        hooks
            .subagent_start("s-1", "agent-2", Some("explorer"), &ui)
            .await;
        assert!(!fired(), "another type must not fire");

        // The case the old code got wrong: no type at all used to fire every
        // matcher. It is DEFAULT_AGENT_TYPE now, which "reviewer" does not select.
        hooks.subagent_start("s-1", "agent-3", None, &ui).await;
        assert!(
            !fired(),
            "an untyped sub-agent must not fire a typed matcher"
        );

        // ...and that name is selectable, which is the reason for naming it.
        let hooks = Hooks {
            defs: vec![HookDef {
                matcher: Some(DEFAULT_AGENT_TYPE.into()),
                ..sh(HookEvent::SubagentStop, &touch)
            }],
        };
        hooks
            .subagent_stop("s-1", "agent-3", None, None, "done", &ui)
            .await;
        assert!(fired(), "matcher = default selects the untyped sub-agents");

        hooks
            .subagent_stop("s-1", "agent-1", Some("reviewer"), None, "done", &ui)
            .await;
        assert!(
            !fired(),
            "a typed sub-agent must not fire matcher = default"
        );
    }

    #[test]
    fn only_tool_and_subagent_events_take_a_matcher() {
        assert_eq!(HookEvent::PreTool.matcher_subject(), Some("tool name"));
        assert_eq!(HookEvent::PostTool.matcher_subject(), Some("tool name"));
        assert_eq!(
            HookEvent::SubagentStart.matcher_subject(),
            Some("agent type")
        );
        assert_eq!(
            HookEvent::SubagentStop.matcher_subject(),
            Some("agent type")
        );
        assert_eq!(HookEvent::PreTurn.matcher_subject(), None);
        assert_eq!(HookEvent::PostTurn.matcher_subject(), None);
    }
}
