//! 会话级后台 Bash 注册表、进程监控与输出、停止工具。
//! Native `background: true` returns immediately with an id and an output file
//! (stdout/stderr interleaved at the fd level — no reader tasks, no pipe
//! deadlock), `bash_output` blocks on completion by default, `stop_bash`
//! kills the whole owned process tree (Unix process group or Windows Job).
//! Interrupting a turn never touches
//! background shells; stop_bash, the size watchdog, and explicit session
//! shutdown reap them. State changes emit session-scoped background-task
//! events. cc's auto-backgrounding and model-visible Monitor tool are not ported.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Value;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::BashInput;
use super::FOREGROUND_REAP_TIMEOUT;
use super::shell_spec;
use crate::agent::Ui;
use crate::config::EffectiveWorkspace;
use crate::event::BackgroundTask;
use crate::event::BackgroundTaskKind;
use crate::event::BackgroundTaskStatus;
use crate::event::Event;
use crate::execution_provenance::AdmissionAuthority;
use crate::execution_provenance::AdmissionOrigin;
use crate::execution_provenance::BackgroundShellId;
use crate::execution_provenance::DeliveryRoute;
use crate::execution_provenance::ExecutionProvenanceReceipt;
use crate::execution_provenance::ExecutionRegistration;
use crate::execution_provenance::MailboxRoute;
use crate::execution_provenance::ResolvedExecutionAdmission;
use crate::execution_provenance::TerminalOwner;
use crate::execution_provenance::TerminalRoute;
use crate::execution_provenance::TransientExecutionId;
use crate::execution_provenance::WorkspaceProvenance;
use crate::inbox::Inbox;
use crate::inbox::InboxItem;
use crate::process_tree;
use crate::process_tree::ProcessStdio;
use crate::process_tree::ProcessTreeChild;
use crate::process_tree::ProcessTreeKiller;
use crate::sandbox;
use crate::sandbox::SandboxPolicy;
use crate::shell_programs::ShellProgram;
use crate::tools::ToolCtx;
use crate::tools::str_arg;
use crate::tools::strict_str_arg;

/// Tail returned inline by bash_output; the rest stays in the output file
/// (cc's BASH_MAX_OUTPUT_DEFAULT is 30k chars).
const OUTPUT_TAIL_BYTES: u64 = 30_000;
/// Watchdog cap on the output file — a runaway `yes`-style command gets its
/// tree terminated instead of filling the disk (cc caps at 5GB; kloop is
/// stingier).
const OUTPUT_FILE_CAP: u64 = 1 << 30;
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// bash_output blocking defaults, straight from cc's TaskOutput.
const BLOCK_TIMEOUT_DEFAULT_MS: u64 = 30_000;
const BLOCK_TIMEOUT_MAX_MS: u64 = 600_000;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(in crate::tools) async fn bash_output_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = str_arg(input, "bash_id", "bash_output")?;
    let block = input["block"].as_bool().unwrap_or(true);
    let timeout_ms = input["timeout_ms"]
        .as_u64()
        .unwrap_or(BLOCK_TIMEOUT_DEFAULT_MS)
        .min(BLOCK_TIMEOUT_MAX_MS);
    let shells = &ctx.cfg.background_shells;
    let started = std::time::Instant::now();
    let (status, path, sandboxed) = loop {
        let Some((status, path, sandboxed)) = shells.snapshot(id) else {
            bail!("bash_output: no background command with id {id}");
        };
        let done = !status.is_active();
        if done || !block || started.elapsed() >= Duration::from_millis(timeout_ms) {
            break (status, path, sandboxed);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    let status_line = wait_status_line(id, &status, block, timeout_ms);
    let mut tail = read_tail(&path).await;
    if let (BgStatus::Exited(code), Some(sb)) = (&status, sandboxed)
        && *code != Some(0)
        && sandbox::is_likely_sandbox_denied(*code, &tail, sb.network_disabled)
    {
        tail.push_str(sandbox::DENIAL_HINT);
    }
    Ok(format!(
        "{status_line}\noutput file: {}\n--- output ---\n{tail}",
        path.display()
    ))
}

pub(in crate::tools) async fn stop_bash_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = strict_str_arg(input, "bash_id", "stop_bash")?;
    if let Some(hint) = crate::tools::background_executions::execution_stop_hint(id) {
        bail!(hint);
    }
    let shells = &ctx.cfg.background_shells;
    let command = shells
        .request_kill(id)
        .map_err(|e| anyhow!("stop_bash: {e}"))?;
    // The monitor task does the killing; wait for it to confirm so the
    // reported status is final rather than racy.
    let started = std::time::Instant::now();
    while started.elapsed() < Duration::from_secs(5) {
        match shells.snapshot(id) {
            Some((status, _, _)) if status.is_active() => tokio::time::sleep(POLL_INTERVAL).await,
            _ => break,
        }
    }
    Ok(format!("Stopped {id} ({command})"))
}

#[derive(Clone, Debug)]
enum BgStatus {
    Running,
    /// A user/session stop won, but the monitor has not reaped the child yet.
    Stopping,
    /// A terminal publisher won, but has not completed frontend/inbox delivery.
    Finishing,
    /// Normal termination; None = killed by an external signal.
    Exited(Option<i32>),
    /// Cancelled through this registry, with the reason.
    Killed(String),
    /// The registry killed it because a lifecycle guard failed.
    Failed(String),
}

impl BgStatus {
    fn is_active(&self) -> bool {
        matches!(self, Self::Running | Self::Stopping | Self::Finishing)
    }
}

/// The one line `bash_output` answers with. A wait that ran out of time says so
/// and names how long it waited — for *every* active state, `Finishing`
/// included: that one is not a finished job either, and reading it as a terminal
/// report is exactly how a timed-out wait gets mistaken for a done one.
fn wait_status_line(id: &str, status: &BgStatus, block: bool, timeout_ms: u64) -> String {
    if block && status.is_active() {
        format!("{id}: still {} after {timeout_ms}ms", status_text(status))
    } else {
        format!("{id}: {}", status_text(status))
    }
}

fn status_text(status: &BgStatus) -> String {
    match status {
        BgStatus::Running => "running".into(),
        BgStatus::Stopping => "stopping".into(),
        BgStatus::Finishing => "finishing".into(),
        BgStatus::Exited(Some(0)) => "completed (exit 0)".into(),
        BgStatus::Exited(Some(code)) => format!("failed (exit {code})"),
        BgStatus::Exited(None) => "failed (killed by signal)".into(),
        BgStatus::Killed(reason) => format!("killed ({reason})"),
        BgStatus::Failed(reason) => format!("failed ({reason})"),
    }
}

/// What bash_output needs to know about the sandbox a background shell ran
/// in, captured at spawn time for denial annotation.
#[derive(Clone, Copy)]
struct BgSandbox {
    network_disabled: bool,
}

type BackgroundShellRegistration = ExecutionRegistration;

struct BgShell {
    receipt: Arc<ExecutionProvenanceReceipt>,
    command: String,
    output_path: PathBuf,
    status: BgStatus,
    kill: CancellationToken,
    stop_reason: Arc<Mutex<Option<String>>>,
    killer: Option<ProcessTreeKiller>,
    /// Some = ran inside the OS sandbox.
    sandbox: Option<BgSandbox>,
}

#[derive(Default)]
struct ShellRegistry {
    closed: bool,
    shells: HashMap<String, BgShell>,
}

/// Session-scoped registry of background shells (one per Config; sub-agents
/// share the parent's through the Config clone). Normal teardown calls
/// [`BackgroundShells::shutdown`]; `Drop` is only the synchronous fallback.
pub struct BackgroundShells {
    state: Mutex<ShellRegistry>,
    activity: watch::Sender<u64>,
}

impl Default for BackgroundShells {
    fn default() -> Self {
        let (activity, _) = watch::channel(0);
        Self {
            state: Mutex::new(ShellRegistry::default()),
            activity,
        }
    }
}

impl BackgroundShells {
    pub fn new() -> Arc<Self> {
        Arc::default()
    }

    /// The output file lives outside the sandbox's writable roots, but the
    /// child writes it through an inherited fd — seatbelt checks at open
    /// time, not per write (verified against the real sandbox-exec).
    pub(super) fn spawn_background(
        self: &Arc<Self>,
        input: &BashInput,
        cwd: &Path,
        sandbox: Option<&SandboxPolicy>,
        bash: &ShellProgram,
        ctx: &ToolCtx,
        workspace: &EffectiveWorkspace,
    ) -> Result<String> {
        let command = input.command.as_str();
        let description = input.description.as_deref().unwrap_or("");
        let offload_dir = &ctx.cfg.offload_dir;
        std::fs::create_dir_all(offload_dir)
            .with_context(|| format!("bash: cannot create {}", offload_dir.display()))?;
        let (id, path, stdout) = crate::resource_id::create_file(offload_dir, "bg-", ".out")
            .context("bash: cannot allocate output file")?;
        let receipt = ExecutionProvenanceReceipt::mint(ResolvedExecutionAdmission {
            session_id: &ctx.cfg.session_id,
            parent: ctx.enclosing_execution.clone(),
            execution: TransientExecutionId::Shell(BackgroundShellId::parse(&id)?),
            durable: None,
            mailbox: MailboxRoute::NotMailboxPeer,
            authority: AdmissionAuthority::new(
                ctx.cfg.local_agent.context_id(),
                ctx.cfg.agent_id().clone(),
                ctx.depth,
            ),
            parent_rollout_id: ctx.parent_rollout_id.as_deref(),
            workspace: WorkspaceProvenance::capture_current(workspace),
            origin: AdmissionOrigin::BackgroundShell,
            terminal: TerminalRoute::new(
                TerminalOwner::BackgroundShells,
                DeliveryRoute::ShellOutputPointer,
            ),
        })?;
        let registration = ExecutionRegistration::new(Arc::clone(&receipt));
        let stderr = stdout
            .try_clone()
            .context("bash: cannot clone output file")?;
        let mut spec = shell_spec(
            command,
            cwd,
            sandbox,
            bash,
            &ctx.cfg.shell_login_env,
            ctx.ui.as_ref(),
        );
        spec.stdin = ProcessStdio::Null;
        spec.stdout = ProcessStdio::File(stdout);
        spec.stderr = ProcessStdio::File(stderr);
        let (child, killer, kill) = {
            // Hold the registry lock across spawn+insert so shutdown cannot close
            // the table between the process becoming real and its controller
            // being tracked.
            let mut registry = self.state.lock().unwrap();
            if registry.closed {
                bail!("bash: the session is closing; no new background commands may start");
            }
            let child = process_tree::spawn(spec).context("bash: failed to spawn sh")?;
            let killer = child.killer();
            let kill = CancellationToken::new();
            let stop_reason = Arc::new(Mutex::new(None));
            registry.shells.insert(
                id.clone(),
                BgShell {
                    receipt,
                    command: command.to_string(),
                    output_path: path.clone(),
                    status: BgStatus::Running,
                    kill: kill.clone(),
                    stop_reason,
                    killer: Some(killer.clone()),
                    sandbox: sandbox.map(|policy| BgSandbox {
                        network_disabled: !policy.allow_network,
                    }),
                },
            );
            (child, killer, kill)
        };
        ctx.ui.emit(&Event::BackgroundTaskUpdated(BackgroundTask {
            id: id.clone(),
            run_id: None,
            kind: BackgroundTaskKind::Bash,
            description: description.to_string(),
            command: Some(command.to_string()),
            status: BackgroundTaskStatus::Running,
            output_path: Some(path.to_string_lossy().to_string()),
            detail: None,
        }));
        // A Weak, not an Arc: an owning ref would keep the registry alive as
        // long as any shell runs, so the synchronous Drop fallback could not fire.
        tokio::spawn(monitor(BackgroundMonitor {
            shells: Arc::downgrade(self),
            registration,
            command: command.to_string(),
            description: description.to_string(),
            child,
            killer,
            kill,
            output_path: path.clone(),
            ui: ctx.ui.clone(),
            inbox: ctx.cfg.inbox.clone(),
        }));
        Ok(format!(
            "Command running in background with ID: {id}. Output is being written to: {}. \
             You will be notified when it changes state. If your next step depends on this \
             command, wait with bash_output {{\"bash_id\":\"{id}\",\"block\":true,\"timeout_ms\":{BLOCK_TIMEOUT_MAX_MS}}}. \
             It returns as soon as the command finishes; timeout_ms is only a maximum wait. \
             Do not use sleep or shell polling to wait for it. Use block=false to peek \
             without waiting; stop it with stop_bash.",
            path.display()
        ))
    }

    pub fn running_count(&self) -> usize {
        self.state
            .lock()
            .unwrap()
            .shells
            .values()
            .filter(|shell| shell.status.is_active())
            .count()
    }

    pub(crate) fn wait_reminder(&self) -> Option<String> {
        let ids: Vec<String> = self
            .state
            .lock()
            .unwrap()
            .shells
            .iter()
            .filter(|(_, shell)| shell.status.is_active())
            .map(|(id, _)| id.clone())
            .collect();
        Self::wait_reminder_for_ids(ids)
    }

    pub(crate) fn wait_reminder_for_ids(mut ids: Vec<String>) -> Option<String> {
        ids.sort();
        let id = ids.first()?;
        let commands = ids
            .iter()
            .map(|id| format!("- {id}"))
            .collect::<Vec<_>>()
            .join("\n");
        Some(format!(
            "<system-reminder>\nBackground bash commands are still running:\n{commands}\n\
             If your next step needs a command's result, wait for its ID with bash_output \
             {{\"bash_id\":\"{id}\",\"block\":true,\"timeout_ms\":{BLOCK_TIMEOUT_MAX_MS}}}. \
             timeout_ms is only an upper bound; completion returns immediately. Do not use \
             bash sleep or shell polling to wait for these commands, even if their actual \
             results are written to a separate log file. After bash_output finishes, read \
             that log as needed. Independent work can continue without waiting.\n\
             </system-reminder>"
        ))
    }

    fn snapshot(&self, id: &str) -> Option<(BgStatus, PathBuf, Option<BgSandbox>)> {
        let registry = self.state.lock().unwrap();
        let shell = registry.shells.get(id)?;
        Some((
            shell.status.clone(),
            shell.output_path.clone(),
            shell.sandbox,
        ))
    }

    /// Atomically mark a running shell as stopping before firing its monitor
    /// token. A duplicate kill therefore reports the stable in-progress state
    /// instead of racing another successful request.
    fn request_kill(&self, id: &str) -> Result<String, String> {
        let (command, kill) = {
            let mut registry = self.state.lock().unwrap();
            let Some(shell) = registry.shells.get_mut(id) else {
                return Err(format!("no background command with id {id}"));
            };
            match shell.status {
                BgStatus::Running => {
                    shell.status = BgStatus::Stopping;
                    *shell.stop_reason.lock().unwrap() = Some("stopped".into());
                    (shell.command.clone(), shell.kill.clone())
                }
                BgStatus::Stopping => return Err(format!("stop already requested for {id}")),
                _ => {
                    return Err(format!(
                        "{id} is not running (status: {})",
                        status_text(&shell.status)
                    ));
                }
            }
        };
        kill.cancel();
        Ok(command)
    }

    /// Claim the one terminal transition. A stop that linearized first overrides
    /// a concurrently observed natural exit. `Finishing` stays active until the
    /// frontend event and inbox delivery have both been published.
    fn begin_finish(
        &self,
        registration: &BackgroundShellRegistration,
        observed: BgStatus,
    ) -> Option<BgStatus> {
        debug_assert!(!observed.is_active());
        let mut registry = self.state.lock().unwrap();
        let shell = registry.shells.get_mut(registration.id())?;
        if !registration.shares_receipt(&shell.receipt) {
            return None;
        }
        let terminal = match &shell.status {
            BgStatus::Running => observed,
            BgStatus::Stopping => BgStatus::Killed(
                shell
                    .stop_reason
                    .lock()
                    .unwrap()
                    .clone()
                    .unwrap_or_else(|| "stopped".into()),
            ),
            BgStatus::Finishing
            | BgStatus::Exited(_)
            | BgStatus::Killed(_)
            | BgStatus::Failed(_) => return None,
        };
        shell.status = BgStatus::Finishing;
        shell.killer = None;
        Some(terminal)
    }

    fn complete_finish(
        &self,
        registration: &BackgroundShellRegistration,
        status: BgStatus,
    ) -> bool {
        let changed = {
            let mut registry = self.state.lock().unwrap();
            let Some(shell) = registry.shells.get_mut(registration.id()) else {
                return false;
            };
            if !registration.shares_receipt(&shell.receipt)
                || !matches!(shell.status, BgStatus::Finishing)
            {
                return false;
            }
            shell.status = status;
            true
        };
        let next = (*self.activity.borrow()).wrapping_add(1);
        self.activity.send_replace(next);
        changed
    }

    /// Close the registry, cancel every active process tree, and wait for the
    /// monitors to reap their direct children. Returns the number still active
    /// after `timeout` (normally zero).
    pub(crate) async fn shutdown(&self, timeout: Duration) -> usize {
        let kills = {
            let mut registry = self.state.lock().unwrap();
            registry.closed = true;
            registry
                .shells
                .values_mut()
                .filter_map(|shell| {
                    if matches!(&shell.status, BgStatus::Running) {
                        shell.status = BgStatus::Stopping;
                        *shell.stop_reason.lock().unwrap() = Some("session shutdown".into());
                        Some(shell.kill.clone())
                    } else if matches!(&shell.status, BgStatus::Stopping) {
                        Some(shell.kill.clone())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
        };
        for kill in kills {
            kill.cancel();
        }

        let deadline = tokio::time::Instant::now() + timeout;
        let mut activity = self.activity.subscribe();
        loop {
            let active = self
                .state
                .lock()
                .unwrap()
                .shells
                .values()
                .filter(|shell| shell.status.is_active())
                .count();
            if active == 0 {
                return 0;
            }
            if tokio::time::timeout_at(deadline, activity.changed())
                .await
                .is_err()
            {
                break;
            }
        }

        let killers = self
            .state
            .lock()
            .unwrap()
            .shells
            .values()
            .filter(|shell| shell.status.is_active())
            .filter_map(|shell| shell.killer.clone())
            .collect::<Vec<_>>();
        for killer in killers {
            let _ = killer.terminate();
        }

        let hard_deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        loop {
            let active = self
                .state
                .lock()
                .unwrap()
                .shells
                .values()
                .filter(|shell| shell.status.is_active())
                .count();
            if active == 0 {
                return 0;
            }
            if tokio::time::timeout_at(hard_deadline, activity.changed())
                .await
                .is_err()
            {
                return active;
            }
        }
    }
}

impl Drop for BackgroundShells {
    /// Synchronous fallback when the session cannot await `shutdown`.
    fn drop(&mut self) {
        let registry = self.state.get_mut().unwrap();
        registry.closed = true;
        for shell in registry.shells.values_mut() {
            if shell.status.is_active() {
                *shell.stop_reason.lock().unwrap() = Some("session dropped".into());
                shell.kill.cancel();
                if let Some(killer) = &shell.killer {
                    let _ = killer.terminate();
                }
            }
        }
    }
}

struct BackgroundMonitor {
    shells: Weak<BackgroundShells>,
    registration: BackgroundShellRegistration,
    command: String,
    /// The model's one-line label, empty when it gave none; the terminal
    /// notification uses it (via [`BackgroundTask::label`]) instead of re-quoting
    /// the command.
    description: String,
    child: ProcessTreeChild,
    killer: ProcessTreeKiller,
    kill: CancellationToken,
    output_path: PathBuf,
    ui: Arc<dyn Ui>,
    inbox: Arc<Inbox>,
}

/// Owns the child: waits for exit, executes kill requests, and enforces the
/// output-file cap. Deliberately not tied to any turn's cancel token — that
/// is what makes the shell "background".
async fn monitor(monitor: BackgroundMonitor) {
    let BackgroundMonitor {
        shells,
        registration,
        command,
        description,
        mut child,
        killer,
        kill,
        output_path,
        ui,
        inbox,
    } = monitor;
    let id = registration.id().to_string();
    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failure_reason: Option<String> = None;
    let mut signal_sent = false;
    let exit = loop {
        tokio::select! {
            status = child.wait() => break status,
            _ = kill.cancelled(), if !signal_sent => {
                if let Err(error) = killer.terminate() {
                    failure_reason = Some(format!("failed to terminate process tree: {error}"));
                }
                signal_sent = true;
            }
            _ = watchdog.tick() => {
                let size = std::fs::metadata(&output_path).map_or(0, |metadata| metadata.len());
                if size > OUTPUT_FILE_CAP && !signal_sent {
                    let reason = format!("output file exceeded {OUTPUT_FILE_CAP} bytes");
                    if let Err(error) = killer.terminate() {
                        failure_reason = Some(format!("{reason}; failed to terminate process tree: {error}"));
                    } else {
                        failure_reason = Some(reason);
                    }
                    signal_sent = true;
                }
            }
        }
    };
    let cleanup = child.cleanup_after_exit(FOREGROUND_REAP_TIMEOUT).await;
    let observed = if let Some(reason) = failure_reason {
        BgStatus::Failed(reason)
    } else if let Err(error) = cleanup {
        BgStatus::Failed(format!("process-tree cleanup failed: {error}"))
    } else {
        match exit {
            Ok(status) => BgStatus::Exited(status.code()),
            Err(error) => BgStatus::Failed(format!("wait failed: {error}")),
        }
    };
    // The session may have ended while this shell ran; if so Drop already
    // group-killed it and there is no live frontend to notify.
    let live_shells = shells.upgrade();
    if let Some(shells) = live_shells
        && let Some(status) = shells.begin_finish(&registration, observed)
    {
        let (event_status, detail) = match &status {
            BgStatus::Exited(Some(0)) => (BackgroundTaskStatus::Completed, Some("exit 0".into())),
            BgStatus::Exited(Some(code)) => {
                (BackgroundTaskStatus::Failed, Some(format!("exit {code}")))
            }
            BgStatus::Exited(None) => (
                BackgroundTaskStatus::Failed,
                Some("killed by signal".into()),
            ),
            BgStatus::Killed(reason) => (BackgroundTaskStatus::Cancelled, Some(reason.clone())),
            BgStatus::Failed(reason) => (BackgroundTaskStatus::Failed, Some(reason.clone())),
            BgStatus::Running | BgStatus::Stopping | BgStatus::Finishing => {
                unreachable!("monitor produced active status")
            }
        };
        let status_label = match event_status {
            BackgroundTaskStatus::Running => unreachable!("monitor emitted running"),
            BackgroundTaskStatus::Completed => "completed",
            BackgroundTaskStatus::Failed => "failed",
            BackgroundTaskStatus::Cancelled => "cancelled",
        };
        let output_path = output_path.to_string_lossy().to_string();
        let task = BackgroundTask {
            id: id.clone(),
            run_id: None,
            kind: BackgroundTaskKind::Bash,
            description,
            command: Some(command),
            status: event_status,
            output_path: Some(output_path.clone()),
            detail: detail.clone(),
        };
        // Name the job by the model's own label, falling back to the command for
        // a shell it never described: the model already has its call, so
        // re-quoting the command only made a long history line, and the status
        // already rides the `[{id}] {status}` frame above.
        let summary = match detail.as_deref() {
            Some(detail) => format!("{} · {detail}", task.label()),
            None => task.label().to_string(),
        };
        ui.emit(&Event::BackgroundTaskUpdated(task));
        let closing = event_status == BackgroundTaskStatus::Cancelled
            && matches!(
                detail.as_deref(),
                Some("session shutdown" | "session dropped")
            );
        if !closing {
            inbox.push(InboxItem::ShellResult {
                id: id.clone(),
                status: status_label.into(),
                output_path,
                summary,
            });
        }
        // The call itself cannot live inside `debug_assert!`: the macro does
        // not evaluate its argument without `debug_assertions`, so a release
        // build would leave every shell `Finishing` — active forever, and a
        // blocking `bash_output` on it could only ever run out its clock.
        let recorded = shells.complete_finish(&registration, status);
        debug_assert!(recorded);
    }
}

/// Last `OUTPUT_TAIL_BYTES` of the output file; the model reads further back
/// with read_file on the reported path.
async fn read_tail(path: &Path) -> String {
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncSeekExt;
    let content = async {
        let mut file = tokio::fs::File::open(path).await?;
        let len = file.metadata().await?.len();
        let skip = len.saturating_sub(OUTPUT_TAIL_BYTES);
        file.seek(std::io::SeekFrom::Start(skip)).await?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).await?;
        let mut text = String::from_utf8_lossy(&buf).into_owned();
        if skip > 0 {
            text = format!("[{skip} bytes of earlier output omitted]\n{text}");
        }
        Ok::<String, std::io::Error>(text)
    }
    .await;
    match content {
        Ok(text) if text.is_empty() => "(no output yet)".into(),
        Ok(text) => text,
        Err(e) => format!("(cannot read output file: {e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::super::testutil::*;
    use super::BLOCK_TIMEOUT_MAX_MS;
    use super::BgStatus;
    use super::status_text;
    use super::wait_status_line;
    use crate::event::BackgroundTaskStatus;
    use crate::event::Event;
    use crate::execution_provenance::DeliveryRoute;
    use crate::execution_provenance::MailboxRoute;
    use crate::execution_provenance::TerminalOwner;
    use crate::inbox::InboxItem;
    use crate::tools::testutil::*;
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn background_wait_reminder_matches_the_measured_wording() {
        assert_eq!(
            super::BackgroundShells::wait_reminder_for_ids(Vec::new()),
            None
        );
        for (ids, commands, example_id) in [
            (vec!["bg-12".into()], "- bg-12", "bg-12"),
            (
                vec!["bg-10".into(), "bg-11".into(), "bg-12".into()],
                "- bg-10\n- bg-11\n- bg-12",
                "bg-10",
            ),
            (
                vec!["bg-12".into(), "bg-10".into(), "bg-11".into()],
                "- bg-10\n- bg-11\n- bg-12",
                "bg-10",
            ),
            (
                vec!["bg-2".into(), "bg-1".into(), "bg-10".into()],
                "- bg-1\n- bg-10\n- bg-2",
                "bg-1",
            ),
        ] {
            let expected = format!(
                "<system-reminder>\nBackground bash commands are still running:\n{commands}\n\
                 If your next step needs a command's result, wait for its ID with bash_output \
                 {{\"bash_id\":\"{example_id}\",\"block\":true,\"timeout_ms\":{BLOCK_TIMEOUT_MAX_MS}}}. \
                 timeout_ms is only an upper bound; completion returns immediately. Do not use \
                 bash sleep or shell polling to wait for these commands, even if their actual \
                 results are written to a separate log file. After bash_output finishes, read \
                 that log as needed. Independent work can continue without waiting.\n\
                 </system-reminder>"
            );
            assert_eq!(
                super::BackgroundShells::wait_reminder_for_ids(ids),
                Some(expected)
            );
        }
    }

    /// A timed-out wait says so for every active state. `Finishing` is the one
    /// that used to fall through to the terminal shape, which made a wait that
    /// gave up read like a job that finished — hit for real on a 600s wait.
    #[test]
    fn a_timed_out_wait_is_reported_as_waiting_for_every_active_state() {
        for (status, line) in [
            (BgStatus::Running, "bg-1: still running after 30000ms"),
            (BgStatus::Stopping, "bg-1: still stopping after 30000ms"),
            (BgStatus::Finishing, "bg-1: still finishing after 30000ms"),
            (BgStatus::Exited(Some(0)), "bg-1: completed (exit 0)"),
            (BgStatus::Exited(Some(2)), "bg-1: failed (exit 2)"),
            (BgStatus::Exited(None), "bg-1: failed (killed by signal)"),
            (
                BgStatus::Killed("session shutdown".into()),
                "bg-1: killed (session shutdown)",
            ),
            (
                BgStatus::Failed("wait failed".into()),
                "bg-1: failed (wait failed)",
            ),
        ] {
            assert_eq!(wait_status_line("bg-1", &status, true, 30_000), line);
        }
        // A peek never claims to be waiting; it reports the state itself.
        for status in [BgStatus::Running, BgStatus::Stopping, BgStatus::Finishing] {
            assert_eq!(
                wait_status_line("bg-1", &status, false, 30_000),
                format!("bg-1: {}", status_text(&status))
            );
        }
    }

    #[tokio::test]
    async fn background_spawn_then_blocking_output_sees_completion() {
        let ctx = test_ctx(0, "bg-complete");
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "echo bg-hello; echo bg-err 1>&2", "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Command running in background with ID: bg-"));
        assert!(out.contains("Output is being written to:"));
        let id = bg_id(&out);
        let wait_input: serde_json::Value = serde_json::from_str(
            out.split("wait with bash_output ")
                .nth(1)
                .unwrap()
                .split(". It returns")
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(
            wait_input,
            json!({"bash_id": id, "block": true, "timeout_ms": BLOCK_TIMEOUT_MAX_MS})
        );
        assert!(out.contains("Do not use sleep or shell polling to wait for it."));
        let receipt = {
            let registry = ctx.cfg.background_shells.state.lock().unwrap();
            Arc::clone(&registry.shells.get(&id).unwrap().receipt)
        };
        assert_eq!(
            receipt.execution().kind(),
            crate::execution_provenance::ExecutionKind::Shell
        );
        assert!(matches!(receipt.mailbox(), MailboxRoute::NotMailboxPeer));
        assert_eq!(receipt.terminal().owner(), TerminalOwner::BackgroundShells);
        assert_eq!(
            receipt.terminal().delivery(),
            DeliveryRoute::ShellOutputPointer
        );

        let (out, is_error) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_tool("bash_output", wait_input, &ctx),
        )
        .await
        .expect("bash_output waited for the full timeout instead of returning on completion");
        assert!(!is_error, "{out}");
        assert!(out.contains(&format!("{id}: completed (exit 0)")), "{out}");
        assert!(out.contains("bg-hello"), "stdout captured: {out}");
        assert!(out.contains("bg-err"), "stderr interleaved: {out}");
    }

    #[tokio::test]
    async fn background_shell_emits_one_start_and_one_terminal_update() {
        let (ctx, ui) = recording_ctx("bg-events");
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "printf done", "background": true, "description": "Print done"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "{out}");

        let updates = ui
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::BackgroundTaskUpdated(task) if task.id == id => Some(task.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 2, "{updates:?}");
        // The label is the model's description; the command rides its own field
        // so the lifecycle row can show it whole on a line of its own.
        assert_eq!(updates[0].description, "Print done");
        assert_eq!(updates[0].command.as_deref(), Some("printf done"));
        assert_eq!(updates[0].status, BackgroundTaskStatus::Running);
        assert!(updates[0].output_path.is_some());
        assert_eq!(updates[1].description, "Print done");
        assert_eq!(updates[1].command.as_deref(), Some("printf done"));
        assert_eq!(updates[1].status, BackgroundTaskStatus::Completed);
        assert_eq!(updates[1].detail.as_deref(), Some("exit 0"));
        assert_eq!(updates[1].output_path, updates[0].output_path);
        let items = ctx.cfg.inbox.drain();
        assert_eq!(items.len(), 1);
        match &items[0] {
            InboxItem::ShellResult {
                id: notified_id,
                status,
                output_path,
                summary,
            } => {
                assert_eq!(notified_id, &id);
                assert_eq!(status, "completed");
                assert_eq!(Some(output_path), updates[1].output_path.as_ref());
                // The label names the job; the raw command is not re-quoted.
                assert_eq!(summary, "Print done · exit 0");
            }
            other => panic!("expected ShellResult, got {other:?}"),
        }
    }

    /// A shell the model never described still reads as the command it ran.
    #[tokio::test]
    async fn background_shell_without_a_description_falls_back_to_the_command() {
        let (ctx, _ui) = recording_ctx("bg-nodesc");
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "printf done", "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "{out}");

        let items = ctx.cfg.inbox.drain();
        assert_eq!(items.len(), 1, "{items:?}");
        match &items[0] {
            InboxItem::ShellResult { summary, .. } => assert_eq!(summary, "printf done · exit 0"),
            other => panic!("expected ShellResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn background_failure_reports_exit_code() {
        let (ctx, ui) = recording_ctx("bg-fail");
        let (out, _) = run_tool(
            "bash",
            json!({"command": "echo pre; exit 7", "background": true}),
            &ctx,
        )
        .await;
        let id = bg_id(&out);
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "a failed command is still a successful query");
        assert!(out.contains(&format!("{id}: failed (exit 7)")), "{out}");
        assert!(out.contains("pre"));
        let updates = ui
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::BackgroundTaskUpdated(task) if task.id == id => Some(task.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert_eq!(updates[0].status, BackgroundTaskStatus::Running);
        assert_eq!(updates[1].status, BackgroundTaskStatus::Failed);
        assert_eq!(updates[1].detail.as_deref(), Some("exit 7"));
        let items = ctx.cfg.inbox.drain();
        assert!(matches!(
            items.as_slice(),
            [InboxItem::ShellResult { status, summary, .. }]
                if status == "failed" && summary.contains("exit 7")
        ));
    }

    #[tokio::test]
    async fn background_ignores_timeout_and_survives_turn_interrupt() {
        let ctx = test_ctx(0, "bg-survive");
        // timeout_ms would kill a foreground sleep instantly; background
        // ignores it.
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "sleep 30", "background": true, "timeout_ms": 10}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);

        // Interrupt the turn; the background shell must not care.
        ctx.cancel.cancel();
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        let ctx2 = crate::tools::ToolCtx {
            cancel: tokio_util::sync::CancellationToken::new(),
            ..ctx.clone()
        };
        let (out, is_error) =
            run_tool("bash_output", json!({"bash_id": id, "block": false}), &ctx2).await;
        assert!(!is_error, "{out}");
        assert!(out.contains(&format!("{id}: running")), "{out}");

        // Clean up.
        let (out, is_error) = run_tool("stop_bash", json!({"bash_id": id}), &ctx2).await;
        assert!(!is_error, "{out}");
    }

    #[tokio::test]
    async fn stop_bash_stops_a_running_shell() {
        let ctx = test_ctx(0, "bg-kill");
        let (out, _) = run_tool(
            "bash",
            json!({"command": "sleep 30", "background": true}),
            &ctx,
        )
        .await;
        let id = bg_id(&out);
        let started = std::time::Instant::now();

        let (out, is_error) = run_tool("stop_bash", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains(&format!("Stopped {id} (sleep 30)")), "{out}");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));

        let (out, _) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
        assert!(out.contains(&format!("{id}: killed (stopped)")), "{out}");
        let items = ctx.cfg.inbox.drain();
        assert!(matches!(
            items.as_slice(),
            [InboxItem::ShellResult { status, summary, .. }]
                if status == "cancelled" && summary.contains("stopped")
        ));

        // A second kill is an error: the shell is no longer running.
        let (out, is_error) = run_tool("stop_bash", json!({"bash_id": id}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("not running"), "{out}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn session_shutdown_reaps_background_tree_and_closes_registry() {
        let tree = ForegroundTree::new("background-shutdown");
        let (ctx, ui) = recording_ctx("background-shutdown");
        let command = tree.command();
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": command, "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);
        let pids = wait_for_tree_pids(&tree).await;

        assert_eq!(ctx.cfg.shutdown_background_work(&ctx.ui).await, 0);
        assert_processes_dead(pids).await;
        let (out, is_error) =
            run_tool("bash_output", json!({"bash_id": id, "block": false}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("killed (session shutdown)"), "{out}");

        let updates = ui
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::BackgroundTaskUpdated(task) if task.id == id => Some(task.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert_eq!(updates[0].status, BackgroundTaskStatus::Running);
        assert_eq!(updates[1].status, BackgroundTaskStatus::Cancelled);
        assert_eq!(updates[1].detail.as_deref(), Some("session shutdown"));
        assert!(
            ctx.cfg.inbox.is_empty(),
            "session teardown must not enqueue a turn that cannot run"
        );

        let (out, is_error) =
            run_tool("bash", json!({"command": "true", "background": true}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("session is closing"), "{out}");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_session_shutdown_reaps_background_job_and_closes_registry() {
        let tree = WindowsBackgroundTree::new("session-shutdown");
        let (ctx, ui) = recording_ctx("windows-background-shutdown");
        let command = tree.command();
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": command, "background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);
        let pids = wait_for_windows_tree_pids(&tree).await;

        assert_eq!(ctx.cfg.shutdown_background_work(&ctx.ui).await, 0);
        assert_windows_processes_dead(pids).await;
        let (out, is_error) =
            run_tool("bash_output", json!({"bash_id": id, "block": false}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("killed (session shutdown)"), "{out}");

        let updates = ui
            .events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                Event::BackgroundTaskUpdated(task) if task.id == id => Some(task.clone()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(updates.len(), 2, "{updates:?}");
        assert_eq!(updates[0].status, BackgroundTaskStatus::Running);
        assert_eq!(updates[1].status, BackgroundTaskStatus::Cancelled);
        assert_eq!(updates[1].detail.as_deref(), Some("session shutdown"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_registry_drop_kills_monitor_owned_background_job() {
        let tree = WindowsBackgroundTree::new("registry-drop");
        let pids = {
            let ctx = test_ctx(0, "windows-registry-drop");
            let command = tree.command();
            let (out, is_error) = run_tool(
                "bash",
                json!({"command": command, "background": true}),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            wait_for_windows_tree_pids(&tree).await
        };
        assert_windows_processes_dead(pids).await;
    }

    #[tokio::test]
    async fn blocking_output_times_out_on_a_long_runner() {
        let ctx = test_ctx(0, "bg-block-timeout");
        let (out, _) = run_tool(
            "bash",
            json!({"command": "sleep 30", "background": true}),
            &ctx,
        )
        .await;
        let id = bg_id(&out);
        let (out, is_error) = run_tool(
            "bash_output",
            json!({"bash_id": id, "timeout_ms": 200}),
            &ctx,
        )
        .await;
        assert!(!is_error);
        assert!(
            out.contains(&format!("{id}: still running after 200ms")),
            "{out}"
        );
        let _ = run_tool("stop_bash", json!({"bash_id": id}), &ctx).await;
    }

    #[tokio::test]
    async fn bash_output_returns_only_the_tail_of_large_output() {
        let ctx = test_ctx(0, "bg-tail");
        let (out, _) = run_tool(
            "bash",
            // ~100KB of x's then a marker; the tail must keep the marker and
            // drop the front.
            json!({"command": "yes x | head -c 100000; echo TAIL-MARKER", "background": true}),
            &ctx,
        )
        .await;
        let id = bg_id(&out);
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("TAIL-MARKER"), "{out}");
        assert!(out.contains("bytes of earlier output omitted"), "{out}");
        assert!(
            out.len() < 40_000,
            "inline output stays bounded: {}",
            out.len()
        );
    }

    #[cfg(target_os = "macos")]
    mod seatbelt {
        use super::super::super::testutil::seatbelt::*;
        use super::*;

        /// The bg output file lives outside the writable roots; the child
        /// writes it through the inherited fd, which seatbelt permits (checks
        /// happen at open time). This test is the regression lock on that.
        #[tokio::test]
        async fn background_shell_runs_sandboxed_and_reports_denials() {
            let (ctx, _) = sandbox_ctx("bg");
            let (out, is_error) = run_tool(
                "bash",
                json!({"command": "echo bg-sandboxed", "background": true}),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            let id = bg_id(&out);
            let (out, _) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
            assert!(out.contains("completed (exit 0)"), "{out}");
            assert!(out.contains("bg-sandboxed"), "{out}");

            let blocked = outside_dir("bg").join("no.txt");
            let (out, _) = run_tool(
                "bash",
                json!({
                    "command": format!("echo hi > {}", blocked.display()),
                    "background": true
                }),
                &ctx,
            )
            .await;
            let id = bg_id(&out);
            let (out, _) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
            assert!(out.contains("failed"), "{out}");
            assert!(out.contains("disable_sandbox: true"), "{out}");
        }
    }

    #[tokio::test]
    async fn stop_bash_rejects_every_execution_id_with_a_directed_hint() {
        let ctx = test_ctx(0, "stop-bash-typed-id");
        for (id, hint) in [
            ("agent-1", "use stop_agent {agent_id: \"agent-1\"}"),
            ("program-1", "use stop_program {program_id: \"program-1\"}"),
            (
                "workflow-1",
                "use stop_workflow {workflow_id: \"workflow-1\"}",
            ),
        ] {
            let (output, is_error) = run_tool("stop_bash", json!({"bash_id": id}), &ctx).await;
            assert!(is_error, "{id}: {output}");
            assert!(output.contains(hint), "{id}: {output}");
        }

        let (unknown, unknown_error) = run_tool(
            "stop_bash",
            json!({"bash_id": "bg-1", "agent_id": "agent-1"}),
            &ctx,
        )
        .await;
        assert!(unknown_error);
        assert!(unknown.contains("unknown field `agent_id`"), "{unknown}");
    }

    #[tokio::test]
    async fn unknown_background_ids_error() {
        let ctx = test_ctx(0, "bg-unknown");
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": "bg-99999"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("no background command"), "{out}");

        let (out, is_error) = run_tool("stop_bash", json!({"bash_id": "bg-99999"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("no background command"), "{out}");

        let (out, is_error) = run_tool("bash_output", json!({}), &ctx).await;
        assert!(is_error);
        assert!(
            out.contains("missing required string argument 'bash_id'"),
            "{out}"
        );
    }
}
