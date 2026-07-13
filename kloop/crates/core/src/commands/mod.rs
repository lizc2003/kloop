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
    let (name, _args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    match name {
        "help" => help::run(),
        "cost" => cost::run(history, cfg),
        "compact" => compact::run(history, cfg, cancel).await,
        "clear" => clear::run(history, cfg),
        _ => unknown(name),
    }
}

fn unknown(name: &str) -> SlashResult {
    let available = BUILTINS
        .iter()
        .map(|b| format!("/{}", b.name))
        .collect::<Vec<_>>()
        .join(", ");
    SlashResult {
        output: format!("unknown command '/{name}' (available: {available})"),
        cleared: false,
    }
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
            SlashResult {
                output: "model: test-model\ncontext: ~20000 / 200000 tokens (10%)".into(),
                cleared: false,
            }
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
            SlashResult {
                output: "history compacted: 2 summarized, 1 kept verbatim".into(),
                cleared: false,
            }
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
        cfg.inbox.lock().unwrap().push("stale steer".into());

        let result = run("/clear", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(
            result,
            SlashResult {
                output: "conversation cleared".into(),
                cleared: true,
            }
        );
        assert!(history.messages().is_empty());
        assert!(cfg.todos.lock().unwrap().is_empty());
        assert!(cfg.inbox.lock().unwrap().is_empty());
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
            SlashResult {
                output: "unknown command '/frobnicate' (available: /help, /cost, /compact, /clear)"
                    .into(),
                cleared: false,
            }
        );
    }
}
