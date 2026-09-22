//! kloop CLI entry point: parse argv, wire the process-stable state (MCP/web
//! tool sources, project context, sandbox, agent types, skills), then dispatch
//! to the server, the TUI, or the plain REPL. Config assembly lives in
//! [`startup`], argument/session handling in [`args`], the plain frontend in
//! [`ui`].

mod args;
mod context;
mod headless;
mod image;
mod mcp;
mod mcp_auth;
mod private_store;
mod project_store;
mod provider_config;
mod startup;
mod ui;
mod user_config;
mod web;

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use anyhow::Result;
#[cfg(test)]
use tokio::io::AsyncBufReadExt;
#[cfg(test)]
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agent::run_turn;
use kloop_core::agent::run_turn_with_input;
use kloop_core::history::History;
use kloop_core::skills::Skill;
use kloop_core::tools::ToolSource;
use kloop_core::tools::tool_merge_warnings;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_server::SkillsReader;
use kloop_server::SkillsSnapshot;

use crate::args::CliArgs;
use crate::args::list_sessions;
use crate::args::open_history;
use crate::args::parse_args;
use crate::args::version_string;
use crate::provider_config::ResolvedProviderSettings;
use crate::startup::RuntimeSettings;
use crate::startup::build_sandbox;
use crate::startup::config_from_settings;
use crate::startup::load_skills;
use crate::startup::server_config_snapshot;
use crate::startup::server_skills_snapshot;
use crate::ui::CliApprover;
use crate::ui::StdoutUi;
use kloop_core::session_store::SessionDirs;
use kloop_core::session_store::SessionStore;

/// `kloop mcp <subcommand>`. Only `login <server-name>` exists today (interactive
/// OAuth for a remote server); logout/list are noted as future work in the plan.
async fn mcp_subcommand(args: &[String]) -> Result<ExitCode> {
    const USAGE: &str = "usage: kloop mcp login <server-name>";
    match args.first().map(String::as_str) {
        Some("login") => match args.get(1) {
            Some(name) if !name.starts_with('-') && args.len() == 2 => {
                mcp_auth::run_login(name).await?;
                Ok(ExitCode::SUCCESS)
            }
            _ => anyhow::bail!("{USAGE}"),
        },
        Some(other) => {
            anyhow::bail!("unknown `kloop mcp` subcommand '{other}' ({USAGE})")
        }
        None => anyhow::bail!("{USAGE}"),
    }
}

fn server_skills_reader(args: &CliArgs) -> SkillsReader {
    if args.mock {
        Arc::new(|cwd| {
            Ok(SkillsSnapshot {
                cwd: cwd.to_string_lossy().to_string(),
                skills: Vec::new(),
                warnings: Vec::new(),
            })
        })
    } else {
        Arc::new(|cwd| Ok(server_skills_snapshot(cwd)))
    }
}

/// Process-global state: resolved once, before any front-end owns the terminal,
/// and shared by every session — server mode's threads included. The MCP
/// lifecycle owner rides alongside rather than inside, because it outlives the
/// front-end these are handed to and must bring the transports down last.
struct ProcessState {
    provider: Arc<ResolvedProviderSettings>,
    runtime: Arc<RuntimeSettings>,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    mcp_statuses: Vec<kloop_server::McpServerStatus>,
}

impl ProcessState {
    async fn load(args: &CliArgs) -> Result<(Self, mcp::McpLifecycleOwner)> {
        // Parse the one user config once, then resolve every process-global piece
        // from that snapshot. --mock stays hermetic: UserConfig neither resolves
        // HOME nor reads config/provider environment variables.
        let user_config = user_config::UserConfig::load(args.mock)?;
        let provider = Arc::new(provider_config::load(args.mock, user_config.table())?);
        let runtime = Arc::new(RuntimeSettings::load(&user_config, args.mock)?);
        for warning in runtime.shell_warnings() {
            eprintln!("\x1b[2m[{warning}]\x1b[0m");
        }
        // MCP servers connect once per process (before any UI owns the terminal)
        // and are shared into every Config — including all server-mode threads.
        // --mock stays hermetic: no child processes, no network-backed tools.
        let (tool_sources, mcp_statuses, mcp_lifecycle) = if args.mock {
            (Vec::new(), Vec::new(), mcp::McpLifecycleOwner::default())
        } else {
            let warn = |s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m");
            // Web tools ride the same ToolSource seam, registered before MCP so
            // a colliding MCP tool name loses (and is warned about).
            let web_cfg = web::load_web_config(user_config.table())?;
            let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
            sources.extend(web::build_web_source(&web_cfg, &warn));
            let servers = mcp::load_mcp_servers(user_config.table())?;
            let mcp::McpConnections {
                sources: mcp_sources,
                statuses,
                lifecycle,
            } = mcp::connect_servers(servers, &warn).await?;
            sources.extend(mcp_sources);
            for warning in tool_merge_warnings(
                &sources,
                runtime.defer_threshold(),
                runtime.shell_programs(),
            ) {
                warn(&warning);
            }
            (sources, statuses, lifecycle)
        };
        Ok((
            Self {
                provider,
                runtime,
                tool_sources,
                mcp_statuses,
            },
            mcp_lifecycle,
        ))
    }
}

/// The cwd-bound state a single-session front-end needs, built once. Server mode
/// never reaches here: it does the same work per thread, against that thread's
/// own cwd, inside [`serve_config_factory`].
struct SessionState {
    project: context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    skills: Arc<Vec<Skill>>,
    history: History,
    session_id: String,
    session_route: kloop_core::provider_route::FrozenProviderRoute,
    session_dirs: SessionDirs,
    pending_images: Vec<ContentBlock>,
}

impl SessionState {
    /// `Ok(None)`: the resume picker was cancelled, so there is no session to
    /// open and nothing to run.
    fn open(
        args: &CliArgs,
        cwd: &std::path::Path,
        process: &ProcessState,
        session_store: &SessionStore,
    ) -> Result<Option<Self>> {
        let project = if args.mock {
            context::mock(cwd)
        } else {
            context::gather(cwd)
        };
        for warning in &project.warnings {
            eprintln!("\x1b[2m[{warning}]\x1b[0m");
        }
        // Ahead of the sandbox: the policy carves this project's offload directory
        // back out of the otherwise denied private state root.
        let session_dirs = session_store
            .ensure(cwd)
            .context("cannot create the session directory")?;
        let sandbox = build_sandbox(
            args,
            cwd,
            &process.runtime,
            &session_dirs.offload,
            |warning| eprintln!("\x1b[2m[{warning}]\x1b[0m"),
        )?;
        let skills = Arc::new(if args.mock {
            kloop_core::skills::builtin()
        } else {
            let (skills, warnings) = load_skills(cwd);
            for warning in &warnings {
                eprintln!("\x1b[2m[{warning}]\x1b[0m");
            }
            skills
        });
        let Some((mut history, session_id)) = open_history(
            session_store,
            &session_dirs,
            &args.session,
            &process.provider.initial_route(),
        )?
        else {
            // The resume picker was cancelled: nothing was chosen, so there is
            // nothing to run.
            return Ok(None);
        };
        // Every session — fresh, resumed or forked — opens on the route a new
        // session would open on: `model_provider`/`model` (and their env overrides)
        // as they read right now. A `/provider` switch belongs to the conversation
        // that ran it, not to the file it left behind, so reopening re-asserts the
        // configured default and records the hop when it differs.
        let (session_route, reopened) = history
            .adopt_provider_route(&process.provider.initial_route())
            .map_err(anyhow::Error::new)?;
        if let Some(reopened) = &reopened {
            eprintln!("\x1b[2m[{reopened}]\x1b[0m");
        }
        // `--image` files are read + validated once, up front, so a bad path fails
        // fast before any UI owns the terminal. They attach to the first user turn.
        let pending_images = if args.mock {
            if !args.images.is_empty() {
                eprintln!("\x1b[2m[--image ignored with --mock]\x1b[0m");
            }
            Vec::new()
        } else {
            image::load_images(&args.images)?
        };
        Ok(Some(Self {
            project,
            sandbox,
            skills,
            history,
            session_id,
            session_route,
            session_dirs,
            pending_images,
        }))
    }
}

/// `[env]` is applied here and nowhere else, because here is the only place
/// it can be: `set_var` is sound while the process is single-threaded, and the
/// runtime built on the next line is what ends that. The file wins over what
/// the shell exported — see `load_env_overrides`.
fn main() -> Result<ExitCode> {
    // A conservative scan rather than the real parser, which needs the
    // subcommand normalization that lives inside `run`. Erring towards "this
    // is --mock" only means declining to read the config, which --mock never
    // does anyway.
    if !std::env::args().any(|arg| arg == "--mock") {
        for (name, value) in user_config::config_env() {
            // SAFETY: no second thread exists yet — the runtime below has not
            // been built, and nothing above this point spawns one.
            unsafe { std::env::set_var(name, value) };
        }
    }
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("cannot start the async runtime")?
        .block_on(run())
}

async fn run() -> Result<ExitCode> {
    let mut raw: Vec<String> = std::env::args().skip(1).collect();
    // `kloop mcp …` is the one subcommand (OAuth login); everything else is
    // flag-shaped and goes through the flag parser.
    if raw.first().map(String::as_str) == Some("mcp") {
        return mcp_subcommand(&raw[1..]).await;
    }
    // `kloop app-server` is the positional entry the Tauri app launches for the
    // native agent protocol (plan 39): it is exactly `--serve`, so normalize it
    // and let the flag parser handle the rest (`app-server --mock`, etc.).
    if raw.first().map(String::as_str) == Some("app-server") {
        raw[0] = "--serve".into();
    }
    let args = parse_args(&raw)?;
    if args.help {
        print!("{}", args::help_text());
        return Ok(ExitCode::SUCCESS);
    }
    // Same one line the banner shows, so "which build is this" has an answer
    // that does not require starting a session (plan 161).
    if args.version {
        println!("kloop {}", version_string());
        return Ok(ExitCode::SUCCESS);
    }
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    // Session storage resolves before the user config is parsed: `--list-sessions`
    // must keep working when `~/.kloop/config.toml` is broken, so the store needs
    // the private-state root but never its contents.
    let session_store = if args.mock {
        SessionStore::hermetic(&cwd)
    } else {
        SessionStore::global(user_config::private_state_root()?)
    };
    if args.list_sessions {
        list_sessions(&session_store, &cwd, args.all);
        return Ok(ExitCode::SUCCESS);
    }
    if args.all {
        anyhow::bail!("--all only applies to --list-sessions");
    }
    // Worktree mode is the single-session feature (plan 35 slice 2): --serve has
    // per-thread configs and no client cwd-switch protocol, --mock has no git.
    if args.worktree.is_some() && (args.serve || args.mock) {
        anyhow::bail!("--worktree is not supported with --serve or --mock");
    }
    let (process, mcp_lifecycle) = ProcessState::load(&args).await?;
    // One dispatch, one teardown: whichever front-end ran, the MCP transports
    // come down after it and before its result is propagated.
    let result = run_front_end(args, process, session_store, cwd).await;
    mcp_lifecycle.shutdown().await;
    result
}

/// Pick the front-end the flags asked for and run it to completion.
async fn run_front_end(
    args: CliArgs,
    process: ProcessState,
    session_store: SessionStore,
    cwd: std::path::PathBuf,
) -> Result<ExitCode> {
    if args.serve {
        return run_serve(args, process, session_store).await;
    }
    let Some(session) = SessionState::open(&args, &cwd, &process, &session_store)? else {
        return Ok(ExitCode::SUCCESS);
    };
    // Headless (`--headless`) takes precedence over the interactive
    // front-ends — including --mock, so `--mock --headless` is a hermetic
    // end-to-end run for CI. One turn, print the result, exit by outcome.
    if args.headless {
        return run_headless_turn(args, process, session, &cwd).await;
    }
    // The TUI is the default entry point; --plain keeps the line-based REPL,
    // and --mock's scripted demo stays on plain output where it is readable.
    if args.mock || args.plain {
        return run_plain(args, process, session).await;
    }
    run_tui(args, process, session, cwd).await
}

/// Server mode's per-thread `Config` builder. Process-wide transports and policy
/// stay shared, while each thread rebuilds cwd-bound context, skills, permission
/// anchors, and its sandbox workspace root.
fn serve_config_factory(
    args: CliArgs,
    process: &ProcessState,
    session_store: SessionStore,
) -> kloop_server::ConfigFactory {
    let provider = process.provider.clone();
    let runtime = process.runtime.clone();
    let tool_sources = process.tool_sources.clone();
    Arc::new(move |options, catalog, approver, questioner, notify| {
        let project = if args.mock {
            context::mock(&options.cwd)
        } else {
            context::gather(&options.cwd)
        };
        for warning in &project.warnings {
            notify(warning);
        }
        // Each thread pins its own cwd, so each resolves its own
        // project partition rather than inheriting the process one.
        // Resolved before the sandbox because the policy carves the
        // partition's offload directory back out of the denied store.
        let session_dirs = session_store.ensure(&options.cwd)?;
        let sandbox = build_sandbox(
            &args,
            &options.cwd,
            &runtime,
            &session_dirs.offload,
            |warning| notify(warning),
        )?;
        // `--mock` skips disk discovery (hermetic) but keeps the
        // builtins, which are compiled in and touch no filesystem.
        let skills = Arc::new(if args.mock {
            kloop_core::skills::builtin()
        } else {
            let (skills, warnings) = load_skills(&options.cwd);
            for warning in &warnings {
                notify(warning);
            }
            skills
        });
        let mut cfg = config_from_settings(
            &args,
            &provider,
            &runtime,
            approver,
            questioner,
            notify,
            &tool_sources,
            &project,
            sandbox,
            skills,
            &options.cwd,
            &session_dirs,
        )?;
        let current_provider = cfg.provider_route.provider_id().to_string();
        let current_model = cfg.provider_route.primary_model().to_string();
        cfg.provider_catalog = Arc::clone(&catalog);
        if options.provider_id.is_some() || options.model.is_some() {
            let provider_id = options
                .provider_id
                .as_deref()
                .unwrap_or_else(|| provider.initial_provider());
            cfg.provider_route = catalog
                .initial_route(provider_id, options.model.as_deref())
                .map_err(anyhow::Error::new)?;
        } else {
            cfg.provider_route = catalog
                .initial_route(&current_provider, Some(&current_model))
                .map_err(anyhow::Error::new)?;
        }
        Ok(cfg)
    })
}

/// `--serve`: the multi-session JSON-RPC server on stdio.
async fn run_serve(
    args: CliArgs,
    process: ProcessState,
    session_store: SessionStore,
) -> Result<ExitCode> {
    if !args.images.is_empty() {
        eprintln!("\x1b[2m[--image ignored with --serve; send images via the RPC client]\x1b[0m");
    }
    let factory = serve_config_factory(args.clone(), &process, session_store.clone());
    let read_args = args.clone();
    let mut server = kloop_server::ServerConfig::new(
        factory,
        kloop_server::ServerPaths {
            store: session_store,
        },
    );
    server.provider_catalog = process.provider.catalog();
    server.mcp_servers = process.mcp_statuses;
    let config_provider = process.provider.clone();
    let config_runtime = process.runtime.clone();
    server.config_reader = Arc::new(move |cwd| {
        server_config_snapshot(&read_args, cwd, &config_provider, &config_runtime)
    });
    server.skills_reader = server_skills_reader(&args);
    kloop_server::serve_stdio(server).await?;
    Ok(ExitCode::SUCCESS)
}

/// `--headless`: one turn, print the result, exit by outcome.
async fn run_headless_turn(
    args: CliArgs,
    process: ProcessState,
    session: SessionState,
    cwd: &std::path::Path,
) -> Result<ExitCode> {
    let prompt = if args.mock {
        // The scripted demo needs no real prompt; the trigger is fixed.
        "run the demo".to_string()
    } else {
        let piped = read_stdin_if_piped().await?;
        headless::assemble_prompt(args.prompt.as_deref(), piped.as_deref())?
    };
    let notify: kloop_tui::NoteFn = Arc::new(|s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m"));
    let mut cfg = config_from_settings(
        &args,
        &process.provider,
        &process.runtime,
        Arc::new(headless::DenyApprover),
        None,
        notify,
        &process.tool_sources,
        &session.project,
        session.sandbox,
        session.skills,
        cwd,
        &session.session_dirs,
    )?;
    cfg.provider_route = if args.mock {
        cfg.provider_route
            .at_revision(session.session_route.revision())
            .map_err(anyhow::Error::new)?
    } else {
        session.session_route.clone()
    };
    cfg.bind_session(session.session_id.clone())?;
    // An explicit headless runaway guardrail enables the otherwise-absent cap.
    if let Some(max_rounds) = args.max_rounds {
        cfg.set_max_rounds(Some(max_rounds));
    }
    let cfg = Arc::new(cfg);
    // `--worktree`: run this headless turn inside an isolated tree (plan 35
    // slice 2). Fail-closed — abort if it can't be created.
    if let Some(name) = &args.worktree
        && let Err(e) = kloop_core::worktree::enter(&cfg, name).await
    {
        eprintln!("worktree: {e:#}");
        return Ok(ExitCode::FAILURE);
    }
    let cancel = CancellationToken::new();
    let watcher = spawn_ctrl_c(cancel.clone());
    let result = headless::run_headless(
        cfg.clone(),
        session.history,
        session.session_id,
        prompt,
        session.pending_images,
        args.json,
        Arc::new(Mutex::new(std::io::stdout())),
        cancel,
    )
    .await;
    watcher.abort();
    let remaining = cfg.shutdown_background_work(&result.ui).await;
    if remaining > 0 {
        eprintln!("warning: {remaining} background task(s) missed the shutdown deadline");
    }
    // Tear down the worktree (dirty kept on its branch, clean removed).
    if let Some(note) = kloop_core::worktree::finish_active(&cfg).await {
        eprintln!("{}", note.trim());
    }
    Ok(ExitCode::from(result.code as u8))
}

/// `--plain` (and `--mock`): the line-based REPL.
async fn run_plain(
    args: CliArgs,
    process: ProcessState,
    session: SessionState,
) -> Result<ExitCode> {
    plain_main(
        args,
        process.provider,
        session.session_route,
        process.runtime,
        session.history,
        session.session_id,
        process.tool_sources,
        session.project,
        session.sandbox,
        session.skills,
        session.pending_images,
        session.session_dirs,
    )
    .await?;
    Ok(ExitCode::SUCCESS)
}

/// The default front-end.
async fn run_tui(
    args: CliArgs,
    process: ProcessState,
    session: SessionState,
    cwd: std::path::PathBuf,
) -> Result<ExitCode> {
    let SessionState {
        project,
        sandbox,
        skills,
        history,
        session_id,
        session_route,
        session_dirs,
        pending_images,
    } = session;
    let factory_session_id = session_id.clone();
    let worktree = args.worktree.clone();
    kloop_tui::run(
        move |approver, questioner, notify| {
            let mut cfg = config_from_settings(
                &args,
                &process.provider,
                &process.runtime,
                approver,
                Some(questioner),
                notify,
                &process.tool_sources,
                &project,
                sandbox.clone(),
                skills.clone(),
                &cwd,
                &session_dirs,
            )?;
            cfg.provider_route = session_route.clone();
            cfg.bind_session(factory_session_id.clone())?;
            Ok(cfg)
        },
        history,
        session_id,
        pending_images,
        worktree,
        &version_string(),
    )
    .await?;
    Ok(ExitCode::SUCCESS)
}

/// Read all of stdin when it is a pipe/redirect, or None when it is an
/// interactive terminal (reading would block waiting for the user). The
/// blocking read runs off the async runtime.
async fn read_stdin_if_piped() -> Result<Option<String>> {
    use std::io::IsTerminal as _;
    use std::io::Read as _;
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let text = tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf).map(|_| buf)
    })
    .await
    .context("stdin reader panicked")?
    .context("cannot read stdin")?;
    Ok(Some(text))
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

enum PlainInput {
    Line(std::io::Result<Option<String>>),
    Inbox,
    InboxClosed,
    CtrlC,
}

struct PlainCtrlC {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
    #[cfg(windows)]
    receiver: tokio::sync::mpsc::UnboundedReceiver<()>,
    #[cfg(windows)]
    task: tokio::task::JoinHandle<()>,
}

impl PlainCtrlC {
    fn install() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            let signal = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
            Ok(Self { signal })
        }
        #[cfg(windows)]
        {
            let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
            let task = tokio::spawn(async move {
                loop {
                    if tokio::signal::ctrl_c().await.is_err() || sender.send(()).is_err() {
                        break;
                    }
                }
            });
            Ok(Self { receiver, task })
        }
    }

    async fn recv(&mut self) {
        #[cfg(unix)]
        let _ = self.signal.recv().await;
        #[cfg(windows)]
        let _ = self.receiver.recv().await;
    }
}

#[cfg(windows)]
impl Drop for PlainCtrlC {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct PlainInputThread {
    requests: std::sync::mpsc::Sender<()>,
    receiver: tokio::sync::mpsc::UnboundedReceiver<std::io::Result<Option<String>>>,
}

impl PlainInputThread {
    fn spawn() -> std::io::Result<Self> {
        let (requests, requested) = std::sync::mpsc::channel();
        let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
        std::thread::Builder::new()
            .name("kloop-plain-stdin".into())
            .spawn(move || {
                while requested.recv().is_ok() {
                    let mut line = String::new();
                    let read = std::io::stdin()
                        .read_line(&mut line)
                        .map(|bytes| (bytes != 0).then_some(line));
                    let done = !matches!(read, Ok(Some(_)));
                    if sender.send(read).is_err() || done {
                        break;
                    }
                }
            })?;
        Ok(Self { requests, receiver })
    }

    async fn next_line(&mut self) -> std::io::Result<Option<String>> {
        self.requests.send(()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::BrokenPipe, "plain stdin reader stopped")
        })?;
        self.receiver.recv().await.unwrap_or(Ok(None))
    }
}

#[cfg(test)]
async fn next_plain_input_with_lines<R: tokio::io::AsyncBufRead + Unpin>(
    lines: &mut tokio::io::Lines<R>,
    inbox_activity: &mut tokio::sync::watch::Receiver<u64>,
    ctrl_c: &mut PlainCtrlC,
) -> PlainInput {
    tokio::select! {
        biased;
        () = ctrl_c.recv() => PlainInput::CtrlC,
        line = lines.next_line() => PlainInput::Line(line),
        activity = inbox_activity.changed() => match activity {
            Ok(()) => PlainInput::Inbox,
            Err(_) => PlainInput::InboxClosed,
        },
    }
}

async fn next_plain_input(
    input: &mut PlainInputThread,
    inbox_activity: &mut tokio::sync::watch::Receiver<u64>,
    ctrl_c: &mut PlainCtrlC,
) -> PlainInput {
    tokio::select! {
        biased;
        () = ctrl_c.recv() => PlainInput::CtrlC,
        line = input.next_line() => PlainInput::Line(line),
        activity = inbox_activity.changed() => match activity {
            Ok(()) => PlainInput::Inbox,
            Err(_) => PlainInput::InboxClosed,
        },
    }
}

/// Run one plain-REPL operation while Ctrl+C remains an application-level exit
/// signal. If it arrives, cancel first so `run_turn` can patch History, then wait
/// for the operation to finish before the caller tears the session down.
async fn run_plain_operation<F, T>(
    operation: F,
    cancel: &CancellationToken,
    ctrl_c: &mut PlainCtrlC,
) -> (T, bool)
where
    F: std::future::Future<Output = T>,
{
    tokio::pin!(operation);
    tokio::select! {
        biased;
        () = ctrl_c.recv() => {
            cancel.cancel();
            (operation.await, true)
        }
        result = &mut operation => (result, false),
    }
}

#[allow(clippy::too_many_arguments)]
async fn plain_main(
    args: CliArgs,
    provider: Arc<ResolvedProviderSettings>,
    session_route: kloop_core::provider_route::FrozenProviderRoute,
    runtime: Arc<RuntimeSettings>,
    mut history: History,
    session_id: String,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    project: context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    skills: Arc<Vec<Skill>>,
    pending_images: Vec<ContentBlock>,
    session_dirs: SessionDirs,
) -> Result<()> {
    let notify: kloop_tui::NoteFn = Arc::new(|s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m"));
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    let interaction = Arc::new(CliApprover::default());
    let mut cfg = config_from_settings(
        &args,
        &provider,
        &runtime,
        interaction.clone(),
        Some(interaction),
        notify,
        &tool_sources,
        &project,
        sandbox,
        skills,
        &cwd,
        &session_dirs,
    )?;
    cfg.provider_route = if args.mock {
        cfg.provider_route
            .at_revision(session_route.revision())
            .map_err(anyhow::Error::new)?
    } else {
        session_route
    };
    cfg.bind_session(session_id.clone())?;
    let mut cfg = Arc::new(cfg);
    let provider_state = kloop_core::provider_route::SessionProviderState::from_timeline(
        Arc::clone(&cfg.provider_catalog),
        history.provider_routes(),
    )
    .map_err(anyhow::Error::new)?;
    let ui: Arc<dyn Ui> = Arc::new(StdoutUi::default());

    if args.mock {
        history.record(Message::user_text("run the demo"));
        let cancel = CancellationToken::new();
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        println!(
            "\n--- mock run: {:?} after {} round(s) ---",
            outcome.reason, outcome.rounds
        );
        let _ = cfg.shutdown_background_work(&ui).await;
        return Ok(());
    }

    // `--worktree`: enter an isolated tree for the session (plan 35 slice 2).
    // Fail-closed like the TUI path — abort if the tree can't be created.
    if let Some(name) = &args.worktree {
        match kloop_core::worktree::enter(&cfg, name).await {
            Ok(msg) => println!("{msg}"),
            Err(e) => return Err(e),
        }
    }

    let mut ctrl_c = PlainCtrlC::install().context("cannot install plain Ctrl+C handler")?;
    println!(
        "kloop — session {session_id}; type a task, /help for commands, \
         'exit' or Ctrl+C to quit"
    );
    // `--image` blocks ride the first user turn; taken once, then empty.
    let mut pending_images = pending_images;
    let mut input = PlainInputThread::spawn().context("cannot start plain stdin reader")?;
    let mut inbox_activity = cfg.inbox.subscribe_activity();
    let mut input_error = None;
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let input = next_plain_input(&mut input, &mut inbox_activity, &mut ctrl_c).await;
        if matches!(input, PlainInput::InboxClosed) {
            break;
        }
        if matches!(input, PlainInput::CtrlC) {
            println!();
            break;
        }
        if matches!(input, PlainInput::Inbox) {
            if cfg.inbox.is_empty() {
                continue;
            }
            let cancel = CancellationToken::new();
            let (outcome, exit_requested) = run_plain_operation(
                run_turn(&cfg, &mut history, &ui, &cancel, 0),
                &cancel,
                &mut ctrl_c,
            )
            .await;
            println!();
            match outcome.reason {
                EndReason::Completed => {}
                EndReason::MaxRounds => println!("[scheduled delivery stopped: max rounds]"),
                EndReason::Aborted => println!("[scheduled delivery interrupted]"),
                EndReason::Error(error) => println!("[scheduled delivery error: {error}]"),
            }
            if exit_requested {
                break;
            }
            continue;
        }
        let PlainInput::Line(line) = input else {
            unreachable!("non-line plain input handled above")
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => break,
            Err(error) => {
                input_error = Some(error);
                break;
            }
        };
        let mut line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" {
            break;
        }
        // Slash commands run inline: the REPL owns History directly, so no
        // routing is needed (unlike the TUI). Ctrl+C cancels a slow one (e.g.
        // /compact), waits for it to settle, then exits the plain REPL.
        if kloop_core::commands::is_command(&line) {
            let cancel = CancellationToken::new();
            let (result, exit_requested) = run_plain_operation(
                kloop_core::commands::run_with_provider_state(
                    &line,
                    &mut history,
                    &cfg,
                    &provider_state,
                    ui.as_ref(),
                    &cancel,
                ),
                &cancel,
                &mut ctrl_c,
            )
            .await;
            if !result.output.is_empty() {
                println!("{}", result.output);
            }
            if result.route_changed {
                cfg = Arc::new(cfg.clone_with_provider_route(provider_state.freeze()));
            }
            // `/exit` quits the REPL, like the bare `exit` word above.
            if result.quit || exit_requested {
                break;
            }
            // A skill invoked as `/name` expands to a prompt; run it as a turn
            // just like a typed message, falling through to the turn path below.
            match result.run_turn {
                Some(prompt) => line = prompt,
                None => continue,
            }
        }

        let msg = if pending_images.is_empty() {
            Message::user_text(line)
        } else {
            Message::user_with_blocks(line, std::mem::take(&mut pending_images))
        };
        let cancel = CancellationToken::new();
        // Staged, not recorded: an interrupt before the model produces anything
        // leaves the session exactly as it was. There is no composer to hand
        // the text back to here, so the line is simply gone — which is why the
        // `Aborted` arm below says so.
        let ((outcome, returned), exit_requested) = run_plain_operation(
            run_turn_with_input(&cfg, &mut history, &ui, &cancel, 0, msg),
            &cancel,
            &mut ctrl_c,
        )
        .await;
        println!();
        match outcome.reason {
            EndReason::Completed => {}
            EndReason::MaxRounds => {
                println!(
                    "[stopped: hit max rounds ({})]",
                    cfg.max_rounds
                        .expect("MaxRounds requires a configured limit")
                )
            }
            EndReason::Aborted if returned.is_some() => {
                println!("[interrupted before the model replied — that message was not recorded]")
            }
            EndReason::Aborted => println!("[interrupted — history patched; exiting]"),
            EndReason::Error(e) => println!("[error: {e}]"),
        }
        if exit_requested {
            break;
        }
    }
    let remaining = cfg.shutdown_background_work(&ui).await;
    if remaining > 0 {
        eprintln!("warning: {remaining} background task(s) missed the shutdown deadline");
    }
    // Tear down the session worktree on exit (dirty kept on its branch, clean
    // removed); the kept-tree note tells the user where its changes live.
    if let Some(note) = kloop_core::worktree::finish_active(&cfg).await {
        println!("{}", note.trim());
    }
    match input_error {
        Some(error) => Err(error.into()),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scheduled_due_interrupts_an_idle_plain_input_read() {
        let inbox = Arc::new(kloop_core::inbox::Inbox::default());
        let clock = kloop_core::scheduler::ManualClock::new(0);
        let scheduler = kloop_core::scheduler::Scheduler::with_clock(
            Arc::clone(&inbox),
            None,
            clock.clone(),
            kloop_core::scheduler::SchedulerTimeZone::named("UTC").unwrap(),
        );
        scheduler.bind_owner("plain-owner").unwrap();
        let wakeup = scheduler
            .schedule_wakeup(60.0, "test delivery", "timer work")
            .unwrap();
        let mut inbox_activity = inbox.subscribe_activity();
        let (_writer, reader) = tokio::io::duplex(128);
        let mut lines = BufReader::new(reader).lines();

        clock.set(wakeup.scheduled_for_ms);
        let mut ctrl_c = PlainCtrlC::install().unwrap();
        let input = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_plain_input_with_lines(&mut lines, &mut inbox_activity, &mut ctrl_c),
        )
        .await
        .unwrap();
        assert!(matches!(input, PlainInput::Inbox));
        assert!(!inbox.is_empty());

        scheduler.shutdown().await;
    }

    #[test]
    fn mock_server_skills_reader_is_hermetic() {
        let root =
            std::env::temp_dir().join(format!("kloop-mock-server-skills-{}", std::process::id()));
        let skill_dir = root.join(".kloop/skills/private");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: PRIVATE DESCRIPTION\n---\nPRIVATE BODY",
        )
        .unwrap();
        let args = parse_args(&["--mock".to_string()]).unwrap();

        let snapshot = server_skills_reader(&args)(&root).unwrap();

        assert_eq!(
            snapshot,
            SkillsSnapshot {
                cwd: root.to_string_lossy().to_string(),
                skills: Vec::new(),
                warnings: Vec::new(),
            }
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
