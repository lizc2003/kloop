//! Built-in slash commands for the REPL front-ends (TUI + plain). One file per
//! command so the directory listing *is* the command catalog; this module owns
//! the shared result type, the registry (for `/help` and the unknown-command
//! error), and the dispatch. Commands run only when no turn is in flight — they
//! read or rewrite History directly, which the turn loop is otherwise using.
//!
//! User-defined `.kloop/commands/*.md` templates (plan 36) are not a separate
//! system: the CLI loads them as `SkillSource::Command` entries in the same
//! skill registry, so they resolve through the [`run`] fall-through below,
//! reusing the skills' argument expansion (which is why parsing already splits
//! off an argument string). After substitution, [`run`] runs any `` !`cmd` `` /
//! `@file` injections ([`crate::tools::expand_slash_injections`], slice 2) —
//! each through the same permission gate a real bash/read call faces — before
//! handing back the prompt.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::history::History;
use crate::tools::TaskGraphSnapshot;

mod clear;
mod compact;
mod cost;
mod exit;
mod help;
#[path = "loop.rs"]
mod loop_command;

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
    pub task_graph: Option<TaskGraphSnapshot>,
    /// `/exit`: the interactive front-ends (TUI, plain REPL) quit. The server
    /// ignores it — one client leaving must not stop a multi-session process.
    pub quit: bool,
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
            task_graph: None,
            quit: false,
        }
    }

    /// `/clear`: text plus a transcript reset.
    fn cleared_message(output: impl Into<String>, task_graph: TaskGraphSnapshot) -> Self {
        Self {
            output: output.into(),
            cleared: true,
            run_turn: None,
            task_graph: Some(task_graph),
            quit: false,
        }
    }

    /// A skill invoked as `/name`: the expanded prompt runs as a turn.
    fn turn(prompt: String) -> Self {
        Self {
            output: String::new(),
            cleared: false,
            run_turn: Some(prompt),
            task_graph: None,
            quit: false,
        }
    }

    /// `/exit`: interactive front-ends quit after showing `output`.
    fn quit(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            cleared: false,
            run_turn: None,
            task_graph: None,
            quit: true,
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
    Builtin {
        name: "loop",
        summary: loop_command::SUMMARY,
    },
    Builtin {
        name: "exit",
        summary: exit::SUMMARY,
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
        "loop" => loop_command::run(args),
        "exit" => exit::run(),
        // A user-invoked skill or command: expand its body (same seam the
        // model's `skill` tool uses) and hand it back as a turn to run. Searches
        // every loaded entry — both `SKILL.md` skills and `.kloop/commands/*.md`
        // user commands are `/name`-invocable. Falls through to the
        // unknown-command reply — which lists them too — when the name is
        // neither a built-in nor a loaded entry.
        _ => match crate::skills::Skill::lookup(cfg.skills.iter(), name) {
            Ok(skill) => {
                let body = crate::skills::expand_body(&skill.body, &skill.dir, args);
                // Then run any `!cmd` / `@file` injections (plan 36 slice 2),
                // gated exactly like a bash/read call. A blocked or failed
                // `!cmd` aborts: show the error, don't start a turn.
                match crate::tools::expand_slash_injections(&body, cfg, cancel).await {
                    Ok(prompt) => SlashResult::turn(prompt),
                    Err(e) => SlashResult::message(format!("/{name}: {e:#}")),
                }
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
    /// (provider/model/context_window/tasks/inbox) matter here.
    fn test_cfg(provider: kloop_provider::Provider, window: Option<u64>) -> Arc<Config> {
        let inbox = Arc::new(crate::inbox::Inbox::default());
        Arc::new(Config {
            provider: Arc::new(provider),
            model: "test-model".into(),
            system: "test".into(),
            project_instructions: None,
            max_rounds: Some(5),
            cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            offload_dir: std::env::temp_dir().join("kloop-cmd-test"),
            sessions_dir: std::env::temp_dir().join("kloop-cmd-test-sessions"),
            context_window: window,
            fallback_model: None,
            permissions: Arc::new(crate::permissions::Permissions::allow_all()),
            questioner: None,
            file_state: Default::default(),
            tool_sources: Vec::new(),
            session_id: String::new(),
            local_agent: crate::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
            hooks: Arc::new(crate::hooks::Hooks::none()),
            background_shells: crate::tools::BackgroundShells::new(),
            shell_programs: std::sync::Arc::new(
                crate::shell_programs::ShellPrograms::test_fixture(),
            ),
            powershell_execution_gate: Default::default(),
            sandbox: None,
            agent_types: Arc::new(Vec::new()),
            tool_allowlist: None,
            defer_threshold: 30,
            unlocked_tools: Default::default(),
            tasks: Default::default(),
            inbox: Arc::clone(&inbox),
            scheduler: crate::scheduler::Scheduler::in_memory(inbox),
            background_executions: Default::default(),
            program_limits: Default::default(),
            skills: Default::default(),
            active_worktree: std::sync::Arc::new(crate::worktree::ActiveWorktreeState::default()),
            surface: Default::default(),
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
            SlashResult::message(
                "model: test-model\ncontext: ~20000 / 200000 tokens (10%)\nprovider-reported usage across all models: unavailable"
            )
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
    async fn cost_distinguishes_unavailable_reported_zero_and_multiple_models() {
        use crate::usage::{ProviderUsageRecord, UsageOperation};
        use kloop_protocol::Usage;

        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        assert!(
            run("/cost", &mut history, &cfg, &CancellationToken::new())
                .await
                .output
                .ends_with("provider-reported usage across all models: unavailable")
        );

        history.record_provider_usage(ProviderUsageRecord {
            model: "primary".into(),
            operation: UsageOperation::Sampling,
            usage: Usage::default(),
        });
        assert_eq!(
            run("/cost", &mut history, &cfg, &CancellationToken::new())
                .await
                .output,
            "model: test-model\ncontext: ~0 / 200000 tokens (0%)\nprovider-reported usage across all models: \n  input tokens: 0\n  output tokens: 0\n  cache read input tokens: 0\n  cache creation input tokens: 0\n  1 reported responses"
        );
        history.record_provider_usage(ProviderUsageRecord {
            model: "fallback".into(),
            operation: UsageOperation::Compaction,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: 30,
                cache_creation_input_tokens: 40,
            },
        });
        let output = run("/cost", &mut history, &cfg, &CancellationToken::new())
            .await
            .output;
        assert_eq!(
            output,
            "model: test-model\ncontext: ~0 / 200000 tokens (0%)\nprovider-reported usage across all models: \n  input tokens: 10\n  output tokens: 20\n  cache read input tokens: 30\n  cache creation input tokens: 40\n  2 reported responses"
        );
        for forbidden in [
            "$", "currency", "price", "quota", "budget", "coverage", "total",
        ] {
            assert!(
                !output.contains(forbidden),
                "unexpected {forbidden}: {output}"
            );
        }
    }

    #[tokio::test]
    async fn clear_preserves_provider_usage_for_the_same_transcript() {
        use crate::usage::{ProviderUsageRecord, UsageOperation};
        use kloop_protocol::Usage;

        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("some earlier work"));
        history.record_provider_usage(ProviderUsageRecord {
            model: "model".into(),
            operation: UsageOperation::Sampling,
            usage: Usage {
                input_tokens: 1,
                ..Usage::default()
            },
        });

        let _ = run("/clear", &mut history, &cfg, &CancellationToken::new()).await;
        let cost = run("/cost", &mut history, &cfg, &CancellationToken::new()).await;

        assert!(history.messages().is_empty());
        assert!(cost.output.contains("input tokens: 1"), "{}", cost.output);
        assert!(
            cost.output.contains("1 reported responses"),
            "{}",
            cost.output
        );
    }

    #[tokio::test]
    async fn compact_summarizes_and_reports_counts() {
        let provider =
            kloop_provider::Provider::mock(vec![vec![kloop_protocol::AssistantBlock::Text {
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
    async fn compact_reports_noop_without_sampling() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(Vec::new());
        let cfg = test_cfg(provider, Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("only message"));

        let result = run("/compact", &mut history, &cfg, &CancellationToken::new()).await;

        assert_eq!(
            result,
            SlashResult::message("history already compacted: nothing new to summarize")
        );
        assert!(seen.lock().unwrap().is_empty());
        assert_eq!(history.messages(), &[Message::user_text("only message")]);
    }

    #[tokio::test]
    async fn compact_reports_provider_error_without_mutating_history() {
        let provider =
            kloop_provider::Provider::mock_scripted(vec![kloop_provider::MockTurn::Error(
                "summarizer unavailable".into(),
            )]);
        let cfg = test_cfg(provider, Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("old request"));
        history.record(Message::user_text("current request"));
        let before = history.messages().to_vec();

        let result = run("/compact", &mut history, &cfg, &CancellationToken::new()).await;

        assert!(result.output.starts_with("compaction failed: "));
        assert_eq!(history.messages(), before.as_slice());
        assert!(history.provider_usage().records().is_empty());
    }

    #[tokio::test]
    async fn clear_empties_history_and_process_state() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("some earlier work"));
        let mut task_ctx = crate::tools::testutil::test_ctx(0, "command-clear");
        task_ctx.cfg = Arc::clone(&cfg);
        let (output, is_error) = crate::tools::testutil::run_tool(
            "task_create",
            serde_json::json!({"subject":"leftover","description":"clear me"}),
            &task_ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        cfg.inbox
            .push(crate::inbox::InboxItem::Steer("stale steer".into()));

        let result = run("/clear", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(
            result,
            SlashResult::cleared_message(
                "conversation cleared",
                TaskGraphSnapshot {
                    revision: 2,
                    tasks: Vec::new(),
                },
            )
        );
        assert!(history.messages().is_empty());
        let (tasks, is_error) =
            crate::tools::testutil::run_tool("task_list", serde_json::json!({}), &task_ctx).await;
        assert!(!is_error, "{tasks}");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&tasks).unwrap(),
            serde_json::json!({"tasks": []})
        );
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
                "unknown command '/frobnicate' (available: /help, /cost, /compact, /clear, /loop, /exit)"
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
                ..Default::default()
            }]),
            ..base.test_clone()
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
                "unknown command '/nope' (available: /help, /cost, /compact, /clear, /loop, /exit, /greet)"
            )
        );
    }

    /// A user command (`SkillSource::Command`) is `/name`-invocable through the
    /// same fall-through — the slash path doesn't distinguish source — and is
    /// listed in the unknown-command reply like any other entry.
    #[tokio::test]
    async fn command_slash_expands_and_appears_in_unknown_list() {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let cfg = Arc::new(Config {
            skills: Arc::new(vec![crate::skills::Skill {
                name: "deploy".into(),
                description: "Ship it.".into(),
                body: "Deploy $0 now.".into(),
                dir: "/repo/.kloop/commands".into(),
                source: crate::skills::SkillSource::Command,
                ..Default::default()
            }]),
            ..base.test_clone()
        });
        let mut history = History::new(cfg.offload_dir.clone());

        let result = run(
            "/deploy prod",
            &mut history,
            &cfg,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, SlashResult::turn("Deploy prod now.".into()));

        let unknown = run("/nope", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(
            unknown,
            SlashResult::message(
                "unknown command '/nope' (available: /help, /cost, /compact, /clear, /loop, /exit, /deploy)"
            )
        );
    }

    /// A command body's `` !`cmd` `` runs through the (here allow-all) gate at
    /// expansion time, its output inlined into the prompt — after argument
    /// substitution, so `$0` is already resolved.
    #[tokio::test]
    async fn injection_runs_embedded_bash_and_inlines_output() {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let cfg = Arc::new(Config {
            skills: Arc::new(vec![crate::skills::Skill {
                name: "greet".into(),
                description: "d".into(),
                body: "Say !`echo hi` to $0.".into(),
                source: crate::skills::SkillSource::Command,
                ..Default::default()
            }]),
            ..base.test_clone()
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
    }

    /// A `@file` mention that resolves to a readable file has its contents
    /// appended to the prompt (the mention itself stays in place).
    #[tokio::test]
    async fn injection_appends_atfile_contents() {
        let dir = std::env::temp_dir().join(format!("kloop-inject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "FILE-BODY-XYZ").unwrap();
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let cfg = Arc::new(Config {
            cwd: dir.clone(),
            skills: Arc::new(vec![crate::skills::Skill {
                name: "ctx".into(),
                description: "d".into(),
                body: "Review @notes.txt now.".into(),
                source: crate::skills::SkillSource::Command,
                ..Default::default()
            }]),
            ..base.test_clone()
        });
        let mut history = History::new(cfg.offload_dir.clone());
        let prompt = run("/ctx", &mut history, &cfg, &CancellationToken::new())
            .await
            .run_turn
            .expect("a turn");
        assert!(prompt.starts_with("Review @notes.txt now."));
        assert!(
            prompt.contains("\n\n@notes.txt:\nFILE-BODY-XYZ"),
            "got: {prompt}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A `!cmd` the permission gate denies aborts the expansion: no turn runs,
    /// and the reason is shown to the user.
    #[tokio::test]
    async fn injection_denied_bash_aborts_with_message() {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let perms = crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules {
                allow: Vec::new(),
                deny: vec!["bash(rm *)".into()],
                ask: Vec::new(),
            },
            std::env::current_dir().unwrap(),
            None,
        )
        .unwrap();
        let cfg = Arc::new(Config {
            permissions: Arc::new(perms),
            skills: Arc::new(vec![crate::skills::Skill {
                name: "danger".into(),
                description: "d".into(),
                body: "cleanup: !`rm nope`".into(),
                source: crate::skills::SkillSource::Command,
                ..Default::default()
            }]),
            ..base.test_clone()
        });
        let mut history = History::new(cfg.offload_dir.clone());
        let result = run("/danger", &mut history, &cfg, &CancellationToken::new()).await;
        assert_eq!(result.run_turn, None, "blocked: no turn runs");
        assert!(
            result.output.starts_with("/danger:"),
            "got: {}",
            result.output
        );
        assert!(
            result.output.contains("blocked by a deny permission rule"),
            "got: {}",
            result.output
        );
    }
}
