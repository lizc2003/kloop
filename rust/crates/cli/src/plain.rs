//! plain 前端：行输入、会话级 Ctrl+C、REPL 操作与退出清理。

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

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
use kloop_core::session_store::SessionDirs;
use kloop_core::skills::Skill;
use kloop_core::tools::ToolSource;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;

use crate::ProcessState;
use crate::SessionState;
use crate::args::CliArgs;
use crate::context;
use crate::provider_config::ResolvedProviderSettings;
use crate::startup::RuntimeSettings;
use crate::startup::config_from_settings;
use crate::ui::CliApprover;
use crate::ui::StdoutUi;

/// `--plain` (and `--mock`): the line-based REPL.
pub(super) async fn run_plain(
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
    let mut provider_state = kloop_core::provider_route::SessionProviderState::from_timeline(
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
                EndReason::Stopped => {
                    println!("[scheduled delivery stopped — tell kloop what to do differently]")
                }
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
        if kloop_core::commands::is_command_or_skill(&line, &cfg.skills) {
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
            if result.new_session {
                match kloop_core::commands::start_fresh_session(&cfg, &ui).await {
                    Ok(fresh) => {
                        history = fresh.history;
                        cfg = Arc::new(fresh.cfg);
                        provider_state =
                            kloop_core::provider_route::SessionProviderState::from_timeline(
                                Arc::clone(&cfg.provider_catalog),
                                history.provider_routes(),
                            )
                            .map_err(anyhow::Error::new)?;
                        inbox_activity = cfg.inbox.subscribe_activity();
                        println!("conversation cleared — new session {}", cfg.session_id);
                        for line in fresh.report {
                            println!("{line}");
                        }
                    }
                    Err(error) => println!("clear failed: {error:#}"),
                }
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
            EndReason::Stopped => println!("[stopped — tell kloop what to do differently]"),
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
}
