//! Built-in slash commands for the REPL front-ends (TUI + plain). One file per
//! command so the directory listing *is* the command catalog; this module owns
//! the shared result type, the registry (for `/help` and the unknown-command
//! error), and the dispatch. Commands run only when no turn is in flight — they
//! read or rewrite History directly, which the turn loop is otherwise using.
//!
//! User-defined `.kloop/commands/*.md` templates are a future plan; they plug
//! into this same seam (a `custom.rs` sibling + a lookup ahead of [`run`]'s
//! match), which is why parsing already splits off an argument string.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::history::History;

mod clear;
mod compact;
mod cost;
mod help;

/// What a command produced. `output` is shown to the user as-is; `cleared`
/// tells the front-end to reset its own transcript view — only `/clear` sets
/// it (it empties History here, but each front-end owns its own display).
#[derive(Debug, PartialEq, Eq)]
pub struct SlashResult {
    pub output: String,
    pub cleared: bool,
    /// When set, the front-end records this as a user message and runs a turn:
    /// a skill invoked as `/name args` expands to a prompt to act on, unlike
    /// the built-in commands which only produce `output` (`output` is empty).
    pub run_turn: Option<String>,
}

impl SlashResult {
    /// A plain command result: text shown to the user, no transcript reset.
    /// (Private, but visible to the sibling command modules and tests, which
    /// are descendants of `commands`.)
    fn message(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            cleared: false,
            run_turn: None,
        }
    }

    /// `/clear`: text plus a transcript reset.
    fn cleared_message(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            cleared: true,
            run_turn: None,
        }
    }

    /// A skill invoked as `/name`: the expanded prompt runs as a turn.
    fn turn(prompt: String) -> Self {
        Self {
            output: String::new(),
            cleared: false,
            run_turn: Some(prompt),
        }
    }
}

/// One registry row: the name (without the leading `/`) and a one-line summary.
/// Drives `/help` and the unknown-command error; [`run`]'s match must carry the
/// same names.
pub struct Builtin {
    pub name: &'static str,
    pub summary: &'static str,
}

pub const BUILTINS: &[Builtin] = &[
    Builtin {
        name: "help",
        summary: help::SUMMARY,
    },
    Builtin {
        name: "cost",
        summary: cost::SUMMARY,
    },
    Builtin {
        name: "compact",
        summary: compact::SUMMARY,
    },
    Builtin {
        name: "clear",
        summary: clear::SUMMARY,
    },
];

/// Whether an input line is a slash-command invocation: a leading `/` followed
/// by a non-space character. Everything else is a normal message to the model.
pub fn is_command(line: &str) -> bool {
    matches!(line.strip_prefix('/'), Some(rest) if rest.starts_with(|c: char| !c.is_whitespace()))
}

/// Parse and run a slash-command line. Assumes [`is_command`] already held; an
/// unrecognized name is handled (not an error) so the reply can list what
/// exists — the same discoverable shape as an unknown agent_type.
pub async fn run(
    line: &str,
    history: &mut History,
    cfg: &Arc<Config>,
    cancel: &CancellationToken,
) -> SlashResult {
    let rest = line.strip_prefix('/').unwrap_or(line);
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    match name {
        "help" => help::run(),
        "cost" => cost::run(history, cfg),
        "compact" => compact::run(history, cfg, cancel).await,
        "clear" => clear::run(history, cfg),
        // A user-invoked skill: expand its body (same seam the model's `skill`
        // tool uses) and hand it back as a turn to run. Falls through to the
        // unknown-command reply — which lists skills too — when the name is
        // neither a built-in nor a skill.
        _ => match crate::skills::Skill::lookup(&cfg.skills, name) {
            Ok(skill) => {
                SlashResult::turn(crate::skills::expand_body(&skill.body, &skill.dir, args))
            }
            Err(_) => unknown(name, cfg),
        },
    }
}

fn unknown(name: &str, cfg: &Config) -> SlashResult {
    let mut available: Vec<String> = BUILTINS.iter().map(|b| format!("/{}", b.name)).collect();
    available.extend(cfg.skills.iter().map(|s| format!("/{}", s.name)));
    SlashResult::message(format!(
        "unknown command '/{name}' (available: {})",
        available.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kloop_protocol::ContentBlock;
    use kloop_protocol::Message;

    /// A Config wired to the given provider; only the fields the commands read
    /// (provider/model/context_window/todos/inbox) matter here.
    fn test_cfg(provider: kloop_provider::Provider, window: Option<u64>) -> Arc<Config> {
        Arc::new(Config {
            provider: Arc::new(provider),
            model: "test-model".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: 5,
            offload_dir: std::env::temp_dir().join("kloop-cmd-test"),
            sessions_dir: std::env::temp_dir().join("kloop-cmd-test-sessions"),
            context_window: window,
            fallback_model: None,
            permissions: Arc::new(crate::permissions::Permissions::allow_all()),
            tool_sources: Vec::new(),
            session_id: String::new(),
            agent_label: String::new(),
            hooks: Arc::new(crate::hooks::Hooks::none()),
            background_shells: crate::tools::BackgroundShells::new(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            todos: Default::default(),
            inbox: Default::default(),
            background_tasks: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
        })
    }

    #[test]
    fn is_command_detects_slash_lines_only() {
        assert!(is_command("/help"));
        assert!(is_command("/help me"));
        assert!(is_command("/usr/bin")); // a command line (unknown name)
        assert!(!is_command("/")); // bare slash: not a command
        assert!(!is_command("/ spaced"));
        assert!(!is_command("hello"));
        assert!(!is_command("  /help")); // leading space: a normal message
    }

    #[tokio::test]
    async fn help_lists_every_builtin() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        let result = run("/help", &mut history, &cfg, &CancellationToken::new()).await;
        assert!(!result.cleared);
        assert!(result.output.starts_with("commands:"));
        for b in BUILTINS {
            assert!(
                result.output.contains(&format!("/{}", b.name))
                    && result.output.contains(b.summary),
                "help missing /{}",
                b.name
            );
        }
    }

    #[tokio::test]
    async fn cost_reports_context_against_the_window() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        // Anchor the estimate with no tail after it, for a deterministic count.
        history.note_usage(20_000);
        let result = run("/cost", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(
            result,
            SlashResult::message("model: test-model\ncontext: ~20000 / 200000 tokens (10%)")
        );
    }

    #[tokio::test]
    async fn cost_notes_when_the_window_is_off() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), None);
        let mut history = History::new(cfg.offload_dir.clone());
        let result = run("/cost", &mut history, &cfg, &CancellationToken::new()).await;
        assert!(result.output.contains("window limit off"));
    }

    #[tokio::test]
    async fn compact_summarizes_and_reports_counts() {
        let provider = kloop_provider::Provider::mock(vec![vec![ContentBlock::Text {
            text: "what happened so far".into(),
        }]]);
        let cfg = test_cfg(provider, Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("old request"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "old work ".repeat(1_500), // exceeds the keep budget
        }]));
        history.record(Message::user_text("current request"));

        let result = run("/compact", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(
            result,
            SlashResult::message("history compacted: 2 summarized, 1 kept verbatim")
        );
        assert!(history.messages().len() < 3, "history shrank");
    }

    #[tokio::test]
    async fn clear_empties_history_and_process_state() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("some earlier work"));
        cfg.todos.lock().unwrap().push(crate::tools::TodoItem {
            content: "leftover".into(),
            active_form: "doing".into(),
            status: crate::tools::TodoStatus::InProgress,
        });
        cfg.inbox
            .push(crate::inbox::InboxItem::Steer("stale steer".into()));

        let result = run("/clear", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(result, SlashResult::cleared_message("conversation cleared"));
        assert!(history.messages().is_empty());
        assert!(cfg.todos.lock().unwrap().is_empty());
        assert!(cfg.inbox.is_empty());
    }

    #[tokio::test]
    async fn unknown_command_lists_the_available_ones() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        let result = run(
            "/frobnicate now",
            &mut history,
            &cfg,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            result,
            SlashResult::message(
                "unknown command '/frobnicate' (available: /help, /cost, /compact, /clear)"
            )
        );
    }

    /// A `/name` matching a loaded skill expands its body (same substitution the
    /// model's `skill` tool uses) into a turn to run — it produces no `output`
    /// and records nothing itself (the front-end runs the returned prompt). An
    /// unknown `/name` lists the skills alongside the built-ins.
    #[tokio::test]
    async fn skill_slash_expands_body_as_a_turn_and_appears_in_unknown_list() {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let cfg = Arc::new(Config {
            skills: Arc::new(vec![crate::skills::Skill {
                name: "greet".into(),
                description: "Greet someone.".into(),
                body: "Say hi to $0.".into(),
                dir: "/skills/greet".into(),
            }]),
            ..(*base).clone()
        });
        let mut history = History::new(cfg.offload_dir.clone());

        let result = run(
            "/greet world",
            &mut history,
            &cfg,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, SlashResult::turn("Say hi to world.".into()));
        assert!(history.messages().is_empty());

        let unknown = run("/nope", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(
            unknown,
            SlashResult::message(
                "unknown command '/nope' (available: /help, /cost, /compact, /clear, /greet)"
            )
        );
    }
}
