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

use crate::agent::Ui;
use crate::config::Config;
use crate::history::History;
use crate::tools::TaskGraphSnapshot;

mod clear;
mod compact;
mod cost;
mod effort;
mod exit;
mod help;
#[path = "loop.rs"]
mod loop_command;
mod model;
mod provider;
mod skills;

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
    /// The session route changed in a way sampling must see — a provider or
    /// model switch, or a new reasoning effort. The front-ends answer it by
    /// re-freezing `cfg` from the session provider state.
    pub route_changed: bool,
    /// The front-end should open the route picker at this stage. `/provider`,
    /// `/model` and `/effort` are three entry points into one wizard, and the
    /// stage is what distinguishes them; a front-end without a picker ignores it
    /// and shows `output`, which is why each of the three also prints its list.
    pub open_picker: Option<kloop_protocol::RoutePickerStage>,
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
            route_changed: false,
            open_picker: None,
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
            route_changed: false,
            open_picker: None,
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
            route_changed: false,
            open_picker: None,
        }
    }

    fn route(
        output: impl Into<String>,
        changed: bool,
        open_picker: Option<kloop_protocol::RoutePickerStage>,
    ) -> Self {
        Self {
            output: output.into(),
            cleared: false,
            run_turn: None,
            task_graph: None,
            quit: false,
            route_changed: changed,
            open_picker,
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
            route_changed: false,
            open_picker: None,
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
        name: "provider",
        summary: provider::SUMMARY,
    },
    Builtin {
        name: "model",
        summary: model::SUMMARY,
    },
    Builtin {
        name: "effort",
        summary: effort::SUMMARY,
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
        name: "skills",
        summary: skills::SUMMARY,
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
///
/// `ui` is the live event seam. Most commands never touch it: they return text
/// and the front-end shows it. `/compact` does — a [`SlashResult`] only exists
/// once the work is over, and for a compaction that is a minute in which the
/// screen has nothing to say.
pub async fn run(
    line: &str,
    history: &mut History,
    cfg: &Arc<Config>,
    ui: &dyn Ui,
    cancel: &CancellationToken,
) -> SlashResult {
    let state = crate::provider_route::SessionProviderState::from_route(
        Arc::clone(&cfg.provider_catalog),
        cfg.provider_route.clone(),
    );
    run_with_provider_state(line, history, cfg, &state, ui, cancel).await
}

pub async fn run_with_provider_state(
    line: &str,
    history: &mut History,
    cfg: &Arc<Config>,
    provider_state: &crate::provider_route::SessionProviderState,
    ui: &dyn Ui,
    cancel: &CancellationToken,
) -> SlashResult {
    let rest = line.strip_prefix('/').unwrap_or(line);
    let (name, args) = match rest.split_once(char::is_whitespace) {
        Some((name, args)) => (name, args.trim()),
        None => (rest, ""),
    };
    match name {
        "help" => help::run(cfg),
        "provider" => provider::run(args, history, cfg, provider_state),
        "model" => model::run(args, history, cfg, provider_state),
        "effort" => effort::run(args, history, cfg, provider_state),
        "cost" => cost::run(history, cfg),
        "compact" => compact::run(history, cfg, ui, cancel).await,
        "clear" => clear::run(history, cfg),
        "loop" => loop_command::run(args),
        "skills" => skills::run(args, cfg),
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
    use crate::event::Event;
    use crate::tools::testutil::SilentUi;
    use kloop_protocol::ContentBlock;
    use kloop_protocol::Message;

    /// Collects what a command puts on the live event seam while it runs, as
    /// opposed to the `SlashResult` it returns when it is over.
    #[derive(Default)]
    struct NoteUi(std::sync::Mutex<Vec<String>>);

    impl Ui for NoteUi {
        fn emit(&self, ev: &Event) {
            if let Event::Note(note) = ev {
                self.0.lock().unwrap().push(note.clone());
            }
        }
    }

    impl NoteUi {
        fn notes(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    /// A Config wired to the given provider; only the fields the commands read
    /// (provider route/context_window/tasks/inbox) matter here.
    fn test_cfg(provider: kloop_provider::Provider, window: Option<u64>) -> Arc<Config> {
        crate::tools::testutil::TestConfig::new("cmd-test")
            .provider(provider)
            .models("test-model", &["test-model"])
            .context_window(window)
            .build()
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
        let result = run(
            "/help",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
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
        let result = run(
            "/cost",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            result,
            SlashResult::message(
                "provider: test\nmodel: test-model\nroute revision: 1\ncontext: ~20000 / 200000 tokens (10%)\nprovider-reported usage by provider/model: unavailable"
            )
        );
    }

    #[tokio::test]
    async fn cost_notes_when_the_window_is_off() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), None);
        let mut history = History::new(cfg.offload_dir.clone());
        let result = run(
            "/cost",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert!(result.output.contains("window limit off"));
    }

    #[tokio::test]
    async fn cost_distinguishes_unavailable_reported_zero_and_multiple_models() {
        use crate::usage::{ProviderUsageRecord, UsageOperation};
        use kloop_protocol::Usage;

        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        assert!(
            run(
                "/cost",
                &mut history,
                &cfg,
                &SilentUi,
                &CancellationToken::new()
            )
            .await
            .output
            .ends_with("provider-reported usage by provider/model: unavailable")
        );

        history.record_provider_usage(ProviderUsageRecord {
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            route_revision: 1,
            model: "primary".into(),
            operation: UsageOperation::Sampling,
            usage: Usage::default(),
        });
        assert_eq!(
            run(
                "/cost",
                &mut history,
                &cfg,
                &SilentUi,
                &CancellationToken::new()
            )
            .await
            .output,
            "provider: test\nmodel: test-model\nroute revision: 1\ncontext: ~0 / 200000 tokens (0%)\nprovider-reported usage by provider/model: \n  test/primary [Mock]: input=0 output=0 cache-read=0 cache-create=0 responses=1\ncache hit: 0 of 0 prompt tokens (n/a)"
        );
        history.record_provider_usage(ProviderUsageRecord {
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            route_revision: 1,
            model: "fallback".into(),
            operation: UsageOperation::Compaction,
            usage: Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_input_tokens: 30,
                cache_creation_input_tokens: 40,
            },
        });
        let output = run(
            "/cost",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await
        .output;
        assert_eq!(
            output,
            "provider: test\nmodel: test-model\nroute revision: 1\ncontext: ~0 / 200000 tokens (0%)\nprovider-reported usage by provider/model: \n  test/primary [Mock]: input=0 output=0 cache-read=0 cache-create=0 responses=1\n  test/fallback [Mock]: input=10 output=20 cache-read=30 cache-create=40 responses=1\ncache hit: 30 of 80 prompt tokens (38%)"
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
    async fn cost_cache_hit_counts_cache_creation_as_a_miss() {
        use crate::usage::{ProviderUsageRecord, UsageOperation};
        use kloop_protocol::Usage;

        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        fn record(history: &mut History, usage: Usage) {
            history.record_provider_usage(ProviderUsageRecord {
                provider_id: "test".into(),
                api_family: kloop_protocol::ProviderApiFamily::Mock,
                route_revision: 1,
                model: "primary".into(),
                operation: UsageOperation::Sampling,
                usage,
            });
        }
        record(
            &mut history,
            Usage {
                input_tokens: 25,
                output_tokens: 1_000,
                cache_read_input_tokens: 75,
                cache_creation_input_tokens: 0,
            },
        );
        let hit = |output: &str| output.lines().last().unwrap().to_string();

        assert_eq!(
            hit(&run(
                "/cost",
                &mut history,
                &cfg,
                &SilentUi,
                &CancellationToken::new()
            )
            .await
            .output),
            "cache hit: 75 of 100 prompt tokens (75%)"
        );

        record(
            &mut history,
            Usage {
                cache_creation_input_tokens: 100,
                ..Usage::default()
            },
        );
        assert_eq!(
            hit(&run(
                "/cost",
                &mut history,
                &cfg,
                &SilentUi,
                &CancellationToken::new()
            )
            .await
            .output),
            "cache hit: 75 of 200 prompt tokens (38%)"
        );
    }

    #[tokio::test]
    async fn clear_preserves_provider_usage_for_the_same_transcript() {
        use crate::usage::{ProviderUsageRecord, UsageOperation};
        use kloop_protocol::Usage;

        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("some earlier work"));
        history.record_provider_usage(ProviderUsageRecord {
            provider_id: "test".into(),
            api_family: kloop_protocol::ProviderApiFamily::Mock,
            route_revision: 1,
            model: "model".into(),
            operation: UsageOperation::Sampling,
            usage: Usage {
                input_tokens: 1,
                ..Usage::default()
            },
        });

        let _ = run(
            "/clear",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        let cost = run(
            "/cost",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;

        assert!(history.messages().is_empty());
        assert!(cost.output.contains("input=1"), "{}", cost.output);
        assert!(cost.output.contains("responses=1"), "{}", cost.output);
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
            text: "old work ".repeat(crate::compact::keep_recent_tokens() as usize),
        }]));
        history.record(Message::user_text("current request"));

        let ui = NoteUi::default();
        let result = run(
            "/compact",
            &mut history,
            &cfg,
            &ui,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            result,
            SlashResult::message("history compacted: 2 summarized, 1 kept verbatim")
        );
        assert!(history.messages().len() < 3, "history shrank");
        // The receipt above is the `SlashResult`; this is what the screen said
        // during the summary request, which is where the whole wait happens.
        assert_eq!(ui.notes(), ["compacting history"]);
    }

    #[tokio::test]
    async fn compact_reports_noop_without_sampling() {
        let (provider, seen) = kloop_provider::Provider::mock_recording(Vec::new());
        let cfg = test_cfg(provider, Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        history.record(Message::user_text("only message"));

        let ui = NoteUi::default();
        let result = run(
            "/compact",
            &mut history,
            &cfg,
            &ui,
            &CancellationToken::new(),
        )
        .await;

        assert_eq!(
            result,
            SlashResult::message("history already compacted: nothing new to summarize")
        );
        // Announced before the outcome is known: the provider was never called
        // here, so a note that exists at all was emitted ahead of the work.
        assert_eq!(ui.notes(), ["compacting history"]);
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

        let ui = NoteUi::default();
        let result = run(
            "/compact",
            &mut history,
            &cfg,
            &ui,
            &CancellationToken::new(),
        )
        .await;

        assert!(result.output.starts_with("compaction failed: "));
        assert_eq!(ui.notes(), ["compacting history"]);
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
            "task_write",
            serde_json::json!({"tasks":[{"subject":"leftover","status":"pending"}]}),
            &task_ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        cfg.inbox
            .push(crate::inbox::InboxItem::Steer("stale steer".into()));

        let result = run(
            "/clear",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
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
        assert!(task_ctx.cfg.tasks.snapshot().tasks.is_empty());
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
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            result,
            SlashResult::message(
                "unknown command '/frobnicate' (available: /help, /provider, /model, /effort, /cost, /compact, /clear, /loop, /skills, /exit)"
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
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, SlashResult::turn("Say hi to world.".into()));
        assert!(history.messages().is_empty());

        let unknown = run(
            "/nope",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            unknown,
            SlashResult::message(
                "unknown command '/nope' (available: /help, /provider, /model, /effort, /cost, /compact, /clear, /loop, /skills, /exit, /greet)"
            )
        );
    }

    /// `/help` names the skills too (plan 119): they are `/name`-invocable like
    /// a built-in, and leaving them out hid the builtins from everyone — the
    /// only listing that named them was the unknown-command error.
    #[tokio::test]
    async fn help_lists_skills_after_the_builtins() {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let cfg = Arc::new(Config {
            skills: Arc::new(crate::skills::builtin()),
            ..base.test_clone()
        });
        let mut history = History::new(cfg.offload_dir.clone());
        let result = run(
            "/help",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert!(result.output.starts_with("commands:"));
        let (commands, skills) = result
            .output
            .split_once("\n\nskills:")
            .expect("a skills section follows the commands");
        assert!(commands.contains("/help"), "{commands}");
        assert!(
            skills.contains("/code-review") && skills.contains("(builtin)"),
            "{skills}"
        );
        // The listing is one line per skill: a description carries both what the
        // skill does and when to use it, so printed whole it wraps to four lines
        // and buries the next entry. Truncated here, full text via /skills.
        let entry = skills
            .lines()
            .find(|l| l.contains("/code-review"))
            .expect("the code-review line");
        assert!(
            entry.chars().count() < 130,
            "listing line too long: {entry}"
        );
        assert!(entry.contains("..."), "truncated with an ellipsis: {entry}");
        assert!(skills.contains("/skills <name>"), "{skills}");
    }

    /// `/skills` lists what is loaded and where each entry came from; `/skills
    /// <name>` prints that skill's body. For a builtin there is no file to
    /// open, so this is the only way to read what you would be replacing.
    #[tokio::test]
    async fn skills_lists_entries_and_prints_one_body() {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut loaded = crate::skills::builtin();
        loaded.push(crate::skills::Skill {
            name: "deploy".into(),
            description: "Ship it.".into(),
            body: "Deploy now.".into(),
            dir: "/repo/.kloop/commands".into(),
            source: crate::skills::SkillSource::Command,
            ..Default::default()
        });
        let cfg = Arc::new(Config {
            skills: Arc::new(loaded),
            ..base.test_clone()
        });
        let mut history = History::new(cfg.offload_dir.clone());

        let list = run(
            "/skills",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert!(list.output.contains("/code-review"), "{}", list.output);
        assert!(list.output.contains("builtin"), "{}", list.output);
        assert!(
            list.output.contains("user command · /repo/.kloop/commands"),
            "{}",
            list.output
        );

        let one = run(
            "/skills code-review",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert!(one.output.contains("source: builtin"), "{}", one.output);
        // Naming one skill gives the untruncated description, unlike the listing.
        assert!(
            one.output
                .contains("not for re-reading edits you just made yourself"),
            "the full description: {}",
            one.output
        );
        assert!(
            one.output.contains("failure scenario"),
            "the body is printed in full: {}",
            one.output
        );
        assert_eq!(one.run_turn, None, "printing a skill must not start a turn");

        let missing = run(
            "/skills nope",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert!(missing.output.starts_with("unknown skill 'nope'"));
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
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(result, SlashResult::turn("Deploy prod now.".into()));

        let unknown = run(
            "/nope",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
        assert_eq!(
            unknown,
            SlashResult::message(
                "unknown command '/nope' (available: /help, /provider, /model, /effort, /cost, /compact, /clear, /loop, /skills, /exit, /deploy)"
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
            &SilentUi,
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
        let prompt = run(
            "/ctx",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
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
        let result = run(
            "/danger",
            &mut history,
            &cfg,
            &SilentUi,
            &CancellationToken::new(),
        )
        .await;
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

    /// Effort rides the route timeline, so a transcript says which effort each
    /// stretch of the session ran at. Recording only the opening value would be
    /// worse than recording none: it states an answer with confidence and is
    /// wrong the moment `/effort` is used.
    /// A model that declares its levels is taken at its word: `/effort` refuses
    /// an undeclared level on the spot instead of letting the turn find out from
    /// a 400. `none` is in the list or it is not — it gets no exemption, while
    /// `unset` never reaches the check at all because it sends no field.
    #[tokio::test]
    async fn effort_refuses_a_level_the_active_model_does_not_declare() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        let catalog = Arc::new(
            crate::provider_route::ProviderCatalog::new(vec![
                crate::provider_route::ProviderCatalogEntry {
                    id: "responses".into(),
                    api_family: kloop_protocol::ProviderApiFamily::OpenAiResponses,
                    endpoint_fingerprint: "responses:test".into(),
                    default_model: "m1".into(),
                    models: vec!["m1".into()],
                    context_window: None,
                    availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    factory: Arc::new(|| Ok(kloop_provider::Provider::mock(Vec::new()))),
                },
            ])
            .unwrap()
            .with_model_knowledge(std::collections::BTreeMap::from([(
                "m1".to_string(),
                crate::provider_route::ModelKnowledge {
                    context_window: None,
                    efforts: Some(vec![
                        kloop_protocol::ReasoningEffort::Low,
                        kloop_protocol::ReasoningEffort::High,
                    ]),
                    thinking_budgets: None,
                },
            )])),
        );
        let state =
            crate::provider_route::SessionProviderState::new(catalog, "responses", None).unwrap();
        let cancel = CancellationToken::new();
        let refused = run_with_provider_state(
            "/effort max",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        assert!(!refused.route_changed);
        assert_eq!(
            refused.output,
            "effort not changed: 'max' is not among the efforts declared for model 'm1' (low, high)"
        );
        assert_eq!(state.effort(), None);

        let accepted = run_with_provider_state(
            "/effort high",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        assert!(accepted.route_changed);
        // `none` is an ordinary level here: the list left it out, so it is out.
        let refused_none = run_with_provider_state(
            "/effort none",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        assert!(!refused_none.route_changed);
        // `unset` is not a level — it sends no field, so no list can gate it.
        let unset = run_with_provider_state(
            "/effort unset",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        assert!(unset.route_changed);
        assert_eq!(state.effort(), None);
    }

    #[tokio::test]
    async fn effort_changes_land_on_the_route_timeline() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        let catalog = Arc::new(
            crate::provider_route::ProviderCatalog::new(vec![
                crate::provider_route::ProviderCatalogEntry {
                    id: "responses".into(),
                    api_family: kloop_protocol::ProviderApiFamily::OpenAiResponses,
                    endpoint_fingerprint: "responses:test".into(),
                    default_model: "m1".into(),
                    models: vec!["m1".into()],
                    context_window: None,
                    availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    factory: Arc::new(|| Ok(kloop_provider::Provider::mock(Vec::new()))),
                },
            ])
            .unwrap(),
        );
        let state =
            crate::provider_route::SessionProviderState::new(catalog, "responses", None).unwrap();
        let cancel = CancellationToken::new();

        // Before any turn: setting the effort opens the timeline itself.
        assert!(!history.has_provider_route());
        run_with_provider_state(
            "/effort high",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        run_with_provider_state(
            "/effort low",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        // Re-setting the same value is not a revision.
        run_with_provider_state(
            "/effort low",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;
        run_with_provider_state(
            "/effort unset",
            &mut history,
            &cfg,
            &state,
            &SilentUi,
            &cancel,
        )
        .await;

        assert_eq!(
            history
                .provider_routes()
                .iter()
                .map(|receipt| (receipt.revision, receipt.effort))
                .collect::<Vec<_>>(),
            vec![
                (1, None),
                (2, Some(kloop_protocol::ReasoningEffort::High)),
                (3, Some(kloop_protocol::ReasoningEffort::Low)),
                (4, None),
            ]
        );
    }

    /// Regression probe kept because it is the bug the picker would have walked
    /// into: `append_provider_route_changed` refuses an empty timeline, so a
    /// route change before the first turn only works because the command opens
    /// the timeline first. `/effort` always did; `/provider` never did.
    #[tokio::test]
    async fn a_route_change_before_the_first_turn_needs_the_timeline_opened() {
        let state =
            crate::provider_route::SessionProviderState::new(route_catalog(), "a", None).unwrap();
        let mut history = History::new(route_cfg(&state).offload_dir.clone());
        let error = history
            .switch_provider(
                &state,
                1,
                "b",
                None,
                crate::provider_route::EffortRequest::Inherit,
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "provider route persistence failed: history provider route timeline is missing"
        );
    }

    /// Two providers, `b2` declaring a narrow effort list, for the entry points
    /// that have to cross between them.
    fn route_catalog() -> Arc<crate::provider_route::ProviderCatalog> {
        let entry = |id: &str, models: &[&str]| crate::provider_route::ProviderCatalogEntry {
            id: id.into(),
            api_family: kloop_protocol::ProviderApiFamily::OpenAiResponses,
            endpoint_fingerprint: format!("responses:{id}"),
            default_model: models[0].into(),
            models: models.iter().map(|model| (*model).to_string()).collect(),
            context_window: None,
            availability: kloop_protocol::ProviderAvailabilityCode::Ready,
            default_effort: None,
            factory: Arc::new(|| Ok(kloop_provider::Provider::mock(Vec::new()))),
        };
        Arc::new(
            crate::provider_route::ProviderCatalog::new(vec![
                entry("a", &["a1", "a2"]),
                entry("b", &["b1", "b2"]),
            ])
            .unwrap()
            .with_model_knowledge(std::collections::BTreeMap::from([(
                "b2".to_string(),
                crate::provider_route::ModelKnowledge {
                    context_window: None,
                    efforts: Some(vec![
                        kloop_protocol::ReasoningEffort::Low,
                        kloop_protocol::ReasoningEffort::High,
                    ]),
                    thinking_budgets: None,
                },
            )])),
        )
    }

    /// A cfg whose frozen route is the one this session actually opens on:
    /// production hands both the same catalog, and the timeline's first receipt
    /// comes off `cfg`.
    fn route_cfg(state: &crate::provider_route::SessionProviderState) -> Arc<Config> {
        let base = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        Arc::new(base.clone_with_provider_route(state.freeze()))
    }

    fn route_timeline(
        history: &History,
    ) -> Vec<(u64, String, String, Option<kloop_protocol::ReasoningEffort>)> {
        history
            .provider_routes()
            .iter()
            .map(|receipt| {
                (
                    receipt.revision,
                    receipt.provider_id.clone(),
                    receipt.primary_model.clone(),
                    receipt.effort,
                )
            })
            .collect()
    }

    /// The picker walks three stages and sends **one** line, so provider, model
    /// and effort have to land as one revision. Splitting them would leave a
    /// timeline that reads, months later, as a user who changed their mind
    /// twice.
    #[tokio::test]
    async fn one_command_moves_provider_model_and_effort_in_a_single_revision() {
        let state =
            crate::provider_route::SessionProviderState::new(route_catalog(), "a", None).unwrap();
        let cfg = route_cfg(&state);
        let mut history = History::new(cfg.offload_dir.clone());
        let cancel = CancellationToken::new();
        let run = async |line: &str, history: &mut History| {
            run_with_provider_state(line, history, &cfg, &state, &SilentUi, &cancel).await
        };

        // Before the first turn there is no timeline yet; choosing a route opens
        // it, which is the normal way a session starts.
        assert!(!history.has_provider_route());
        let switched = run("/provider b b2 high", &mut history).await;
        assert!(switched.route_changed);
        assert_eq!(
            switched.output,
            "provider switched: b b2 (revision 2, reasoning continuity: Preserved, effort: high)"
        );

        // Same route, new level: still one revision, and the timeline says which
        // stretch ran at which effort.
        assert!(run("/provider b b2 low", &mut history).await.route_changed);
        // Same route, same level: nothing to record.
        assert!(!run("/provider b b2 low", &mut history).await.route_changed);
        // `/model` and `/effort` are the same road from further in.
        assert!(run("/model b1 unset", &mut history).await.route_changed);
        assert!(run("/effort max", &mut history).await.route_changed);

        assert_eq!(
            route_timeline(&history),
            vec![
                (1, "a".into(), "a1".into(), None),
                (
                    2,
                    "b".into(),
                    "b2".into(),
                    Some(kloop_protocol::ReasoningEffort::High)
                ),
                (
                    3,
                    "b".into(),
                    "b2".into(),
                    Some(kloop_protocol::ReasoningEffort::Low)
                ),
                (4, "b".into(), "b1".into(), None),
                (
                    5,
                    "b".into(),
                    "b1".into(),
                    Some(kloop_protocol::ReasoningEffort::Max)
                ),
            ]
        );
    }

    /// The declared list governs the model being switched *to*, not the one
    /// being left: a switch can land on a narrower list than it started from,
    /// and finding that out from a 400 mid-turn is the cost this check avoids.
    #[tokio::test]
    async fn a_named_effort_is_checked_against_the_model_the_command_lands_on() {
        let state =
            crate::provider_route::SessionProviderState::new(route_catalog(), "a", None).unwrap();
        let cfg = route_cfg(&state);
        let mut history = History::new(cfg.offload_dir.clone());
        let cancel = CancellationToken::new();
        let run = async |line: &str, history: &mut History| {
            run_with_provider_state(line, history, &cfg, &state, &SilentUi, &cancel).await
        };

        // `a1` declares nothing, so `max` passes there.
        assert!(run("/effort max", &mut history).await.route_changed);
        let refused = run("/provider b b2 max", &mut history).await;
        assert_eq!(
            refused,
            SlashResult::message(
                "provider switch failed: 'max' is not among the efforts declared for model 'b2' \
                 (low, high)"
            )
        );
        assert_eq!(state.active_route().provider_id, "a");
        let refused = run("/model b2 max", &mut history).await;
        assert_eq!(
            refused.output,
            "model switch failed: 'max' is not among the efforts declared for model 'b2' (low, high)"
        );
        // The model's own list is the whole list — `/effort` says so too.
        assert!(run("/provider b b2 high", &mut history).await.route_changed);
        assert_eq!(
            run("/effort", &mut history).await.output,
            "effort: high (provider b)\n\
             levels: unset, low, high — declared by model 'b2'\n\
             usage: /effort <level> | /effort unset (send no effort field at all)"
        );
    }

    /// Each of the three commands opens the wizard at its own stage, and each
    /// prints its own list: a front-end without a picker ignores the stage, so
    /// "pick one in the TUI" would leave it with nothing.
    #[tokio::test]
    async fn the_three_entry_points_each_open_a_stage_and_print_their_list() {
        let state =
            crate::provider_route::SessionProviderState::new(route_catalog(), "a", None).unwrap();
        let cfg = route_cfg(&state);
        let mut history = History::new(cfg.offload_dir.clone());
        let cancel = CancellationToken::new();
        let run = async |line: &str, history: &mut History| {
            run_with_provider_state(line, history, &cfg, &state, &SilentUi, &cancel).await
        };

        let providers = run("/provider", &mut history).await;
        assert_eq!(
            providers,
            SlashResult::route(
                "active: a a1 (revision 1)\n\
                 providers:\n  \
                 a [responses] default=a1 models=a1,a2 availability=Ready\n  \
                 b [responses] default=b1 models=b1,b2 availability=Ready\n\
                 usage: /provider <provider> [model] [effort]",
                /*changed*/ false,
                Some(kloop_protocol::RoutePickerStage::Provider),
            )
        );

        let models = run("/model", &mut history).await;
        assert_eq!(
            models,
            SlashResult::route(
                "active: a a1 (revision 1)\n\
                 models on provider 'a':\n  \
                 a1 (default)\n  \
                 a2\n\
                 usage: /model <model> [effort]",
                /*changed*/ false,
                Some(kloop_protocol::RoutePickerStage::Model),
            )
        );

        let switched = run("/model a2", &mut history).await;
        assert!(switched.route_changed);
        assert_eq!(
            switched.output,
            "model switched: a2 (provider a, revision 2, reasoning continuity: Preserved, \
             effort: unset)"
        );
        assert_eq!(
            run("/model a2", &mut history).await.output,
            "model unchanged: a2 (provider a, revision 2)"
        );
        assert_eq!(
            run("/model nope", &mut history).await.output,
            "model switch failed: unknown model 'nope' for provider 'a'"
        );
        assert_eq!(
            run("/model a2 hgih", &mut history).await.output,
            "unknown effort 'hgih' (known: none, low, medium, high, xhigh, max)\n\
             usage: /model <model> [effort]"
        );
    }

    /// `/effort` reads and writes the session knob, enforcing only kloop's own
    /// spelling — which levels a model takes is the model's contract.
    #[tokio::test]
    async fn effort_shows_sets_and_clears_the_session_knob() {
        let cfg = test_cfg(kloop_provider::Provider::mock(vec![]), Some(200_000));
        let mut history = History::new(cfg.offload_dir.clone());
        let catalog = Arc::new(
            crate::provider_route::ProviderCatalog::new(vec![
                crate::provider_route::ProviderCatalogEntry {
                    id: "responses".into(),
                    api_family: kloop_protocol::ProviderApiFamily::OpenAiResponses,
                    endpoint_fingerprint: "responses:test".into(),
                    default_model: "m1".into(),
                    models: vec!["m1".into()],
                    context_window: None,
                    availability: kloop_protocol::ProviderAvailabilityCode::Ready,
                    default_effort: None,
                    factory: Arc::new(|| Ok(kloop_provider::Provider::mock(Vec::new()))),
                },
            ])
            .unwrap(),
        );
        let state =
            crate::provider_route::SessionProviderState::new(catalog, "responses", None).unwrap();
        let cancel = CancellationToken::new();
        let run = async |line: &str, history: &mut History| {
            run_with_provider_state(line, history, &cfg, &state, &SilentUi, &cancel).await
        };

        // Bare: the status, the list this model would offer, the usage — and
        // the picker stage for a front-end that has one.
        let shown = run("/effort", &mut history).await;
        assert_eq!(
            shown,
            SlashResult::route(
                "effort: unset — no effort field is sent, the provider's own default applies \
                 (provider responses)\n\
                 levels: unset, none, low, medium, high, xhigh, max — 'm1' declares none, so a \
                 model accepts its own subset and names it if you miss\n\
                 usage: /effort <level> | /effort unset (send no effort field at all)",
                /*changed*/ false,
                Some(kloop_protocol::RoutePickerStage::Effort),
            )
        );

        let set = run("/effort Medium", &mut history).await;
        assert_eq!(
            set,
            SlashResult::route(
                "effort: medium (provider responses)",
                /*changed*/ true,
                /*open_picker*/ None,
            )
        );
        assert_eq!(
            state.effort(),
            Some(kloop_protocol::ReasoningEffort::Medium)
        );

        // Setting the level it already has is a read, not a route change.
        assert!(!run("/effort medium", &mut history).await.route_changed);

        // A level some models on this rail refuse is still accepted here: that
        // contract belongs to the model, which states it in its own error.
        assert!(run("/effort xhigh", &mut history).await.route_changed);
        assert_eq!(state.effort(), Some(kloop_protocol::ReasoningEffort::XHigh));

        // Only kloop's own spelling is enforced.
        let unknown = run("/effort hgih", &mut history).await;
        assert_eq!(
            unknown,
            SlashResult::message(
                "unknown effort 'hgih' (known: none, low, medium, high, xhigh, max)\n\
                 usage: /effort <level> | /effort unset (send no effort field at all)"
            )
        );
        assert_eq!(state.effort(), Some(kloop_protocol::ReasoningEffort::XHigh));

        // `unset` (send no field) is a different thing from the `none` level,
        // which asks the model for no reasoning — hence not calling it `off`.
        assert!(run("/effort none", &mut history).await.route_changed);
        assert_eq!(state.effort(), Some(kloop_protocol::ReasoningEffort::None));
        assert!(run("/effort unset", &mut history).await.route_changed);
        assert_eq!(state.effort(), None);
    }
}
