//! plain 前端：行输入、会话级 Ctrl+C、REPL 操作与退出清理。

use std::io::Write as _;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use tokio_util::sync::CancellationToken;

use kloop_core::Config;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::driver::Input;
use kloop_core::driver::Report;
use kloop_core::driver::SessionDriver;
use kloop_core::history::History;
use kloop_core::session_store::SessionDirs;
use kloop_core::skills::Skill;
use kloop_core::tools::ToolSource;
use kloop_protocol::ContentBlock;

use crate::ProcessState;
use crate::SessionState;
use crate::args::CliArgs;
use crate::context;
use crate::provider_config::ResolvedProviderSettings;
use crate::startup::RuntimeSettings;
use crate::startup::config_from_settings;
use crate::startup::finalize_session_config;
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
        session.identity,
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

/// The one stdin reader for the plain front-end. The REPL prompt and the
/// approval / question prompts all take lines from this handle, so a line typed
/// for one of them can never be swallowed by the other. The reader thread reads
/// one line ahead (the channel's bound) — enough to see EOF, no further.
#[derive(Clone)]
pub(crate) struct PlainStdin {
    lines: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<std::io::Result<Option<String>>>>>,
}

impl PlainStdin {
    fn spawn_stdin() -> std::io::Result<Self> {
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        std::thread::Builder::new()
            .name("kloop-plain-stdin".into())
            .spawn(move || {
                loop {
                    let mut line = String::new();
                    let read = std::io::stdin()
                        .read_line(&mut line)
                        .map(|bytes| (bytes != 0).then_some(line));
                    let done = !matches!(read, Ok(Some(_)));
                    if sender.blocking_send(read).is_err() || done {
                        break;
                    }
                }
            })?;
        Ok(Self::from_receiver(receiver))
    }

    fn from_receiver(
        receiver: tokio::sync::mpsc::Receiver<std::io::Result<Option<String>>>,
    ) -> Self {
        Self {
            lines: Arc::new(tokio::sync::Mutex::new(receiver)),
        }
    }

    /// The next line typed (or EOF / a read error). Callers share one queue, so
    /// whoever is prompting at that moment gets the line.
    pub(crate) async fn line(&self) -> std::io::Result<Option<String>> {
        self.lines.lock().await.recv().await.unwrap_or(Ok(None))
    }
}

async fn next_plain_input(
    input: &PlainStdin,
    inbox_activity: &mut tokio::sync::watch::Receiver<u64>,
    ctrl_c: &mut PlainCtrlC,
) -> PlainInput {
    tokio::select! {
        biased;
        () = ctrl_c.recv() => PlainInput::CtrlC,
        line = input.line() => PlainInput::Line(line),
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

/// The human-facing ending of a turn, in the plain REPL's voice. `returned` is
/// the turn's own input when an interrupt before the first reply took it back;
/// there is no composer here, so the line is gone rather than handed back.
fn report_turn_reason(reason: &EndReason, returned: bool, cfg: &Config) {
    match reason {
        EndReason::Completed => {}
        EndReason::MaxRounds => {
            println!(
                "[stopped: hit max rounds ({})]",
                cfg.max_rounds
                    .expect("MaxRounds requires a configured limit")
            )
        }
        EndReason::Aborted if returned => {
            println!("[interrupted before the model replied — that message was not recorded]")
        }
        EndReason::Aborted => println!("[interrupted — history patched; exiting]"),
        EndReason::Stopped => println!("[stopped — tell kloop what to do differently]"),
        EndReason::Error(e) => println!("[error: {e}]"),
    }
}

#[allow(clippy::too_many_arguments)]
async fn plain_main(
    args: CliArgs,
    provider: Arc<ResolvedProviderSettings>,
    session_route: kloop_core::provider_route::FrozenProviderRoute,
    runtime: Arc<RuntimeSettings>,
    history: History,
    session_id: String,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    identity: kloop_core::project::WorkspaceIdentity,
    project: context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    skills: Arc<Vec<Skill>>,
    pending_images: Vec<ContentBlock>,
    session_dirs: SessionDirs,
) -> Result<()> {
    let notify: kloop_tui::NoteFn = Arc::new(|s: &str| crate::eprint_note(s));
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    // Spawned before the config because the same handle answers approvals and
    // questions during a turn; a second reader would race the REPL for stdin.
    let input = PlainStdin::spawn_stdin().context("cannot start plain stdin reader")?;
    let interaction = Arc::new(CliApprover::new(input.clone()));
    let mut cfg = config_from_settings(
        &args,
        &provider,
        &runtime,
        interaction.clone(),
        Some(interaction),
        notify,
        &tool_sources,
        &project,
        &identity,
        sandbox,
        skills,
        &cwd,
        &session_dirs,
    )?;
    finalize_session_config(&args, &mut cfg, &session_route, &session_id)?;
    let cfg = Arc::new(cfg);
    let mut driver = SessionDriver::new(Arc::clone(&cfg), history, pending_images)
        .map_err(anyhow::Error::new)?;
    let ui: Arc<dyn Ui> = Arc::new(StdoutUi::default());

    if args.mock {
        let cancel = CancellationToken::new();
        let report = driver
            .dispatch(
                Input::Message {
                    text: "run the demo".into(),
                    images: Vec::new(),
                },
                &ui,
                &cancel,
            )
            .await;
        let Report::Turn(Some(turn)) = report else {
            unreachable!("a message input always runs a turn");
        };
        println!(
            "\n--- mock run: {:?} after {} round(s) ---",
            turn.outcome.reason, turn.outcome.rounds
        );
        let _ = driver.shutdown(&ui).await;
        return Ok(());
    }

    // `--worktree`: enter an isolated tree for the session (plan 35 slice 2).
    // Fail-closed like the TUI path — abort if the tree can't be created.
    if let Some(name) = &args.worktree {
        match kloop_core::worktree::enter(driver.cfg(), name).await {
            Ok(msg) => println!("{msg}"),
            Err(e) => return Err(e),
        }
    }

    let mut ctrl_c = PlainCtrlC::install().context("cannot install plain Ctrl+C handler")?;
    println!(
        "kloop — session {}; type a task, /help for commands, \
         'exit' or Ctrl+C to quit",
        driver.cfg().session_id
    );
    let mut inbox_activity = driver.cfg().inbox.subscribe_activity();
    let mut input_error = None;
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let input = next_plain_input(&input, &mut inbox_activity, &mut ctrl_c).await;
        if matches!(input, PlainInput::InboxClosed) {
            break;
        }
        if matches!(input, PlainInput::CtrlC) {
            println!();
            break;
        }
        if matches!(input, PlainInput::Inbox) {
            if driver.cfg().inbox.is_empty() {
                continue;
            }
            let cancel = CancellationToken::new();
            let (report, exit_requested) = run_plain_operation(
                driver.dispatch(Input::Wake, &ui, &cancel),
                &cancel,
                &mut ctrl_c,
            )
            .await;
            driver.reannounce_if_owed(report.drained_inbox());
            println!();
            match report.reason() {
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
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" {
            break;
        }
        // Slash commands run inline: the REPL owns the driver, so a slow one
        // (e.g. /compact) is cancelled by Ctrl+C, waited for, then the REPL
        // exits.
        if kloop_core::commands::is_command_or_skill(&line, &driver.cfg().skills) {
            let cancel = CancellationToken::new();
            let (report, exit_requested) = run_plain_operation(
                driver.dispatch(Input::Command { line }, &ui, &cancel),
                &cancel,
                &mut ctrl_c,
            )
            .await;
            driver.reannounce_if_owed(report.drained_inbox());
            let Report::Command(command) = report else {
                unreachable!("a command input always runs a command");
            };
            if !command.output.is_empty() {
                println!("{}", command.output);
            }
            if command.new_session {
                match driver.start_fresh_session(&ui).await {
                    Ok(fresh) => {
                        inbox_activity = driver.cfg().inbox.subscribe_activity();
                        println!("conversation cleared — new session {}", fresh.session_id);
                        for line in fresh.report {
                            println!("{line}");
                        }
                    }
                    Err(error) => println!("clear failed: {error:#}"),
                }
            }
            // A skill invoked as `/name` expands to a prompt, which the driver
            // runs as a turn; report it like a typed message. Ctrl+C during that
            // turn still reports how it ended before the REPL exits.
            if let Some(turn) = command.turn {
                println!();
                report_turn_reason(&turn.outcome.reason, turn.returned.is_some(), driver.cfg());
            }
            // `/exit` quits the REPL, like the bare `exit` word above; so does
            // a Ctrl+C that arrived while the command ran.
            if command.quit || exit_requested {
                break;
            }
            continue;
        }

        let cancel = CancellationToken::new();
        // Staged, not recorded: an interrupt before the model produces anything
        // leaves the session exactly as it was. There is no composer to hand
        // the text back to here, so the line is simply gone — which is why the
        // `Aborted` arm below says so.
        let (report, exit_requested) = run_plain_operation(
            driver.dispatch(
                Input::Message {
                    text: line,
                    images: Vec::new(),
                },
                &ui,
                &cancel,
            ),
            &cancel,
            &mut ctrl_c,
        )
        .await;
        driver.reannounce_if_owed(report.drained_inbox());
        println!();
        let Report::Turn(Some(turn)) = report else {
            unreachable!("a message input always runs a turn");
        };
        report_turn_reason(&turn.outcome.reason, turn.returned.is_some(), driver.cfg());
        if exit_requested {
            break;
        }
    }
    let shutdown = driver.shutdown(&ui).await;
    let missed = shutdown.missed;
    if missed > 0 {
        eprintln!("warning: {missed} background task(s) missed the shutdown deadline");
    }
    // Tear down the session worktree on exit (dirty kept on its branch, clean
    // removed); the kept-tree note tells the user where its changes live.
    if let Some(note) = shutdown.worktree {
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
        // Nothing is typed: the send half stays open, so the read is idle.
        let (_lines, receiver) = tokio::sync::mpsc::channel(1);
        let input = PlainStdin::from_receiver(receiver);

        clock.set(wakeup.scheduled_for_ms);
        let mut ctrl_c = PlainCtrlC::install().unwrap();
        let input = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            next_plain_input(&input, &mut inbox_activity, &mut ctrl_c),
        )
        .await
        .unwrap();
        assert!(matches!(input, PlainInput::Inbox));
        assert!(!inbox.is_empty());

        scheduler.shutdown().await;
    }

    /// The bug this pins: the REPL's select used to ask for a line and then
    /// drop the request when the inbox won, so the read stayed pending on a
    /// second reader and the approval prompt's `y` landed in the next turn.
    /// One shared queue means the turn (approval) takes the line, and the REPL
    /// has nothing left to send.
    #[tokio::test]
    async fn an_inbox_turn_takes_the_approval_answer_not_the_next_message() {
        use kloop_core::permissions::ApprovalScope;
        use kloop_core::permissions::Approver;
        use kloop_core::permissions::ConfirmRequest;
        use kloop_core::permissions::Decision;

        let (activity, mut activity_rx) = tokio::sync::watch::channel(0u64);
        let (lines, receiver) = tokio::sync::mpsc::channel(1);
        let input = PlainStdin::from_receiver(receiver);
        let mut ctrl_c = PlainCtrlC::install().unwrap();

        activity.send(1).unwrap();
        let event = next_plain_input(&input, &mut activity_rx, &mut ctrl_c).await;
        assert!(matches!(event, PlainInput::Inbox));

        let approver = CliApprover::new(input.clone());
        let request = ConfirmRequest {
            description: "run a command".into(),
            approval_scopes: vec![ApprovalScope::Once],
            ..Default::default()
        };
        let approval = tokio::spawn(async move { approver.confirm(request).await });
        lines.send(Ok(Some("y\n".into()))).await.unwrap();
        assert_eq!(
            approval.await.unwrap(),
            Decision::Allow(ApprovalScope::Once)
        );

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), input.line())
                .await
                .is_err(),
            "the approval answer must not remain queued for the next user turn"
        );
    }
}
