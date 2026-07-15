//! kloop CLI entry point: parse argv, wire the process-stable state (MCP/web
//! tool sources, project context, sandbox, agent types, skills), then dispatch
//! to the server, the TUI, or the plain REPL. Config assembly lives in
//! [`startup`], argument/session handling in [`args`], the plain frontend in
//! [`ui`].

mod args;
mod context;
mod mcp;
mod startup;
mod ui;
mod web;

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent_type::AgentType;
use kloop_core::history::History;
use kloop_core::skills::Skill;
use kloop_core::tools::tool_merge_warnings;
use kloop_core::tools::ToolSource;
use kloop_protocol::Message;

use crate::args::list_sessions;
use crate::args::open_history;
use crate::args::parse_args;
use crate::args::CliArgs;
use crate::startup::build_sandbox;
use crate::startup::config_from_env;
use crate::startup::defer_threshold_from_env;
use crate::startup::load_agent_types;
use crate::startup::load_skills;
use crate::startup::PERMISSIONS_CONFIG;
use crate::ui::CliApprover;
use crate::ui::StdoutUi;

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&args)?;
    let sessions_dir = PathBuf::from(".kloop/sessions");
    if args.list_sessions {
        list_sessions(&sessions_dir);
        return Ok(());
    }
    // MCP servers connect once per process (before any UI owns the terminal)
    // and are shared into every Config — including all server-mode threads.
    // --mock stays hermetic: no child processes, no config reads.
    let tool_sources = if args.mock {
        Vec::new()
    } else {
        let warn = |s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m");
        // Web tools ride the same ToolSource seam, registered before MCP so
        // a colliding MCP tool name loses (and is warned about).
        let web_cfg = web::load_web_config(Path::new(PERMISSIONS_CONFIG))?;
        let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
        sources.extend(web::build_web_source(&web_cfg, &warn));
        let servers = mcp::load_mcp_servers(Path::new(PERMISSIONS_CONFIG))?;
        sources.extend(mcp::connect_servers(servers, &warn).await);
        for warning in tool_merge_warnings(&sources, defer_threshold_from_env()?) {
            warn(&warning);
        }
        sources
    };
    // Project context (instruction files, env block, git snapshot) is
    // gathered once per process and shared into every Config the same way.
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    let project = if args.mock {
        context::mock(&cwd)
    } else {
        context::gather(&cwd)
    };
    for warning in &project.warnings {
        eprintln!("\x1b[2m[{warning}]\x1b[0m");
    }
    // The sandbox policy is process-stable (cwd + config), so it is built
    // once and shared into every Config — server threads included.
    let sandbox = build_sandbox(&args, &cwd, |s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m"))?;
    // Agent types are likewise config-derived and process-stable; --mock stays
    // hermetic (no config reads).
    let agent_types = Arc::new(if args.mock {
        Vec::new()
    } else {
        load_agent_types(Path::new(PERMISSIONS_CONFIG))?
    });
    // Skills are process-stable (discovered from disk once); --mock stays
    // hermetic. Parse-skip warnings surface at startup like the others.
    let skills = Arc::new(if args.mock {
        Vec::new()
    } else {
        let (skills, warnings) = load_skills(&cwd);
        for warning in &warnings {
            eprintln!("\x1b[2m[{warning}]\x1b[0m");
        }
        skills
    });
    if args.serve {
        // Multi-session JSON-RPC server on stdio; each thread gets its own
        // Config (and thus its own permission gate + session cache).
        let factory: kloop_server::ConfigFactory = {
            let args = args.clone();
            Arc::new(move |approver, notify| {
                config_from_env(
                    &args,
                    approver,
                    notify,
                    &tool_sources,
                    &project,
                    sandbox.clone(),
                    agent_types.clone(),
                    skills.clone(),
                )
            })
        };
        return kloop_server::serve_stdio(
            factory,
            kloop_server::ServerPaths {
                sessions_dir,
                offload_dir: PathBuf::from(".kloop/offload"),
            },
        )
        .await;
    }
    let (history, session_id) = open_history(
        PathBuf::from(".kloop/offload"),
        &args.session,
        &sessions_dir,
    )?;

    // The TUI is the default entry point; --plain keeps the line-based REPL,
    // and --mock's scripted demo stays on plain output where it is readable.
    if args.mock || args.plain {
        return plain_main(
            args,
            history,
            session_id,
            tool_sources,
            project,
            sandbox,
            agent_types,
            skills,
        )
        .await;
    }
    let factory_session_id = session_id.clone();
    kloop_tui::run(
        move |approver, notify| {
            let mut cfg = config_from_env(
                &args,
                approver,
                notify,
                &tool_sources,
                &project,
                sandbox.clone(),
                agent_types.clone(),
                skills.clone(),
            )?;
            cfg.session_id = factory_session_id.clone();
            Ok(cfg)
        },
        history,
        session_id,
    )
    .await
}

/// A task that cancels `cancel` on the first Ctrl+C; the caller aborts it once
/// the turn or command finishes. The REPL's own stdin reader is idle meanwhile.
fn spawn_ctrl_c(cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn plain_main(
    args: CliArgs,
    mut history: History,
    session_id: String,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    project: context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    agent_types: Arc<Vec<AgentType>>,
    skills: Arc<Vec<Skill>>,
) -> Result<()> {
    let notify: kloop_tui::NoteFn = Arc::new(|s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m"));
    let mut cfg = config_from_env(
        &args,
        Arc::new(CliApprover::default()),
        notify,
        &tool_sources,
        &project,
        sandbox,
        agent_types,
        skills,
    )?;
    cfg.session_id = session_id.clone();
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(StdoutUi);

    if args.mock {
        history.record(Message::user_text("run the demo"));
        let cancel = CancellationToken::new();
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        println!(
            "\n--- mock run: {:?} after {} round(s) ---",
            outcome.reason, outcome.rounds
        );
        return Ok(());
    }

    println!(
        "kloop — session {session_id}; type a task, /help for commands, \
         'exit' or Ctrl+D to quit, Ctrl+C to interrupt a running turn"
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let mut line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" {
            break;
        }
        // Slash commands run inline: the REPL owns History directly, so no
        // routing is needed (unlike the TUI). Ctrl+C interrupts a slow one
        // (e.g. /compact) the same way it interrupts a turn.
        if kloop_core::commands::is_command(&line) {
            let cancel = CancellationToken::new();
            let watcher = spawn_ctrl_c(cancel.clone());
            let result = kloop_core::commands::run(&line, &mut history, &cfg, &cancel).await;
            watcher.abort();
            if !result.output.is_empty() {
                println!("{}", result.output);
            }
            // A skill invoked as `/name` expands to a prompt; run it as a turn
            // just like a typed message, falling through to the turn path below.
            match result.run_turn {
                Some(prompt) => line = prompt,
                None => continue,
            }
        }

        history.record(Message::user_text(line));
        let cancel = CancellationToken::new();
        let watcher = spawn_ctrl_c(cancel.clone());
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        watcher.abort();
        println!();
        match outcome.reason {
            EndReason::Completed => {}
            EndReason::MaxRounds => println!("[stopped: hit max rounds ({})]", cfg.max_rounds),
            EndReason::Aborted => {
                println!("[interrupted — history patched; Ctrl+D or 'exit' to quit]")
            }
            EndReason::Error(e) => println!("[error: {e}]"),
        }
    }
    Ok(())
}
