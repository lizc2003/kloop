//! Foreground and background shell execution. The background shape follows
//! cc: `run_in_background` returns immediately with an id and an output file
//! (stdout/stderr interleaved at the fd level — no reader tasks, no pipe
//! deadlock), `bash_output` blocks on completion by default, `kill_bash`
//! kills the whole owned process tree (Unix process group or Windows Job).
//! Interrupting a turn never touches
//! background shells; kill_bash, the size watchdog, and explicit session
//! shutdown reap them. State changes emit session-scoped background-task
//! events. cc's auto-backgrounding and model-visible Monitor tool are not ported.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::Weak;
use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::str_arg;
use super::ToolCtx;
use crate::agent::Ui;
use crate::event::BackgroundTask;
use crate::event::BackgroundTaskKind;
use crate::event::BackgroundTaskStatus;
use crate::event::Event;
use crate::inbox::Inbox;
use crate::inbox::InboxItem;
use crate::permissions::EscalationOutcome;
use crate::process_tree;
use crate::process_tree::ProcessExit;
use crate::process_tree::ProcessSpec;
use crate::process_tree::ProcessStdio;
use crate::process_tree::ProcessTreeChild;
use crate::process_tree::ProcessTreeKiller;
use crate::sandbox;
use crate::sandbox::SandboxPolicy;
use crate::shell_programs::ShellProgram;

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
/// Each foreground fd is drained to EOF while retaining at most this many
/// bytes. The final model-visible merge is tighter, matching CC's default
/// inline Bash budget without inheriting its unbounded child-pipe buffering.
const FOREGROUND_STREAM_CAP_BYTES: usize = 150_000;
const FOREGROUND_OUTPUT_CAP_CHARS: usize = 30_000;
const FOREGROUND_REAP_TIMEOUT: Duration = Duration::from_secs(2);

/// Process-global so parent/sub-agents and server threads sharing one
/// offload directory never collide on output file names (same lesson as the
/// offload counter).
static NEXT_BG_ID: AtomicUsize = AtomicUsize::new(1);

const MODEL_SHELL_SECRET_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "TAVILY_API_KEY",
    "BRAVE_API_KEY",
];

pub(super) fn scrub_model_shell_env(spec: &mut ProcessSpec) {
    for name in MODEL_SHELL_SECRET_ENV {
        spec.env_remove(*name);
    }
}

/// The process spec for the frozen Bash-family executable's `-lc <command>`,
/// wrapped in the OS sandbox when a policy applies. The env vars are hints only;
/// enforcement is the profile.
fn shell_spec(
    command: &str,
    cwd: &Path,
    sandbox: Option<&SandboxPolicy>,
    bash: &ShellProgram,
) -> ProcessSpec {
    let shell_args = vec!["-lc".into(), command.into()];
    let (program, args) = match sandbox {
        Some(policy) => sandbox::seatbelt_command(policy, bash.executable.as_os_str(), &shell_args),
        None => (bash.executable.clone(), shell_args),
    };
    let mut spec = ProcessSpec::new(program, cwd);
    #[cfg(windows)]
    spec.require_windows_descendant_debugging();
    spec.args = args;
    if let Some(policy) = sandbox {
        spec.env("KLOOP_SANDBOX", "seatbelt");
        if !policy.allow_network {
            spec.env("KLOOP_SANDBOX_NETWORK_DISABLED", "1");
        }
    }
    // Provider/search credentials belong to the parent process, never to a
    // model-controlled shell.
    scrub_model_shell_env(&mut spec);
    spec
}

/// The session sandbox policy for this call: disable_sandbox is the model's
/// per-call escape hatch (cc's dangerouslyDisableSandbox shape). The call
/// still went through the permission gate like any other — escaping changes
/// the execution wrapper, never the asking.
fn call_sandbox(input: &Value, ctx: &ToolCtx) -> Option<Arc<SandboxPolicy>> {
    if input["disable_sandbox"].as_bool().unwrap_or(false) {
        None
    } else {
        // effective_*: the active worktree's policy when the session entered
        // one (plan 35 slice 2), else the base policy.
        ctx.cfg.effective_sandbox()
    }
}

/// The per-call verdict dispatch feeds the permission gate's sandbox
/// auto-allow layer: bash, not escaped, and the active policy opts in.
/// Foreground and background take the same wrapper, so one verdict covers
/// both.
pub(super) fn sandbox_auto_allowed(name: &str, input: &Value, ctx: &ToolCtx) -> bool {
    name == "bash" && call_sandbox(input, ctx).is_some_and(|p| p.auto_allow)
}

pub(super) async fn bash_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let command = str_arg(input, "command", "bash")?;
    let bash = ctx
        .cfg
        .shell_programs
        .bash
        .as_ref()
        .context("bash: Git for Windows Bash is unavailable in this session")?;
    #[cfg(windows)]
    if input.get("disable_sandbox").is_some() {
        bail!("bash: disable_sandbox is unavailable on Windows because Windows shell sandboxing is not implemented");
    }
    let sandbox = call_sandbox(input, ctx);
    // effective cwd: the active worktree's when the session entered one.
    let cwd = ctx.cfg.effective_cwd();
    if input["run_in_background"].as_bool().unwrap_or(false) {
        // No timeout in background mode (cc clears the timer too); the
        // watchdog and kill_bash are the safety net.
        return ctx
            .cfg
            .background_shells
            .spawn_background(command, sandbox.as_deref(), bash, ctx);
    }
    if ctx.cancel.is_cancelled() {
        bail!("interrupted");
    }
    let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(60_000);
    let output = run_foreground(
        command,
        &cwd,
        sandbox.as_deref(),
        bash,
        timeout_ms,
        &ctx.cancel,
    )
    .await?;
    let mut text = format_output(&output);

    // Sandbox denial handling applies only to an actually-sandboxed run;
    // disable_sandbox / no policy leaves `sandbox` None and skips it.
    if let Some(policy) = &sandbox {
        if !output.status.success()
            && sandbox::is_likely_sandbox_denied(output.status.code(), &text, !policy.allow_network)
        {
            if policy.escalate {
                // The code-level escalation loop (codex's retry-on-denial):
                // ask once, and on approval re-run the command unsandboxed —
                // one fewer model round-trip than the disable_sandbox hint.
                match ctx
                    .cfg
                    .effective_permissions()
                    .escalate_sandbox(command, ctx.depth)
                    .await
                {
                    EscalationOutcome::Approved => {
                        let raw =
                            run_foreground(command, &cwd, None, bash, timeout_ms, &ctx.cancel)
                                .await?;
                        return Ok(format!(
                            "{}{}",
                            sandbox::ESCALATED_PREFIX,
                            format_output(&raw)
                        ));
                    }
                    EscalationOutcome::Declined => text.push_str(sandbox::ESCALATION_DECLINED),
                    EscalationOutcome::NotAttempted => text.push_str(sandbox::DENIAL_HINT),
                }
            } else {
                text.push_str(sandbox::DENIAL_HINT);
            }
        }
    }
    Ok(text)
}

/// One foreground shell run. The root is awaited independently from pipe EOF:
/// once it exits, residual tree members are killed before readers are joined.
async fn run_foreground(
    command: &str,
    cwd: &Path,
    sandbox: Option<&SandboxPolicy>,
    bash: &ShellProgram,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<ForegroundOutput> {
    let spec = shell_spec(command, cwd, sandbox, bash);
    run_process_foreground(spec, timeout_ms, cancel, "bash").await
}

pub(super) async fn run_process_foreground(
    mut spec: ProcessSpec,
    timeout_ms: u64,
    cancel: &CancellationToken,
    tool: &'static str,
) -> Result<ForegroundOutput> {
    spec.stdin = ProcessStdio::Null;
    spec.stdout = ProcessStdio::Pipe;
    spec.stderr = ProcessStdio::Pipe;
    let mut child =
        process_tree::spawn(spec).with_context(|| format!("{tool}: failed to spawn shell"))?;
    let stdout = child
        .take_stdout()
        .with_context(|| format!("{tool}: stdout pipe missing"))?;
    let stderr = child
        .take_stderr()
        .with_context(|| format!("{tool}: stderr pipe missing"))?;
    let stdout_task = tokio::spawn(read_bounded_stream(stdout));
    let stderr_task = tokio::spawn(read_bounded_stream(stderr));

    enum Completion {
        Finished(std::io::Result<ProcessExit>),
        TimedOut,
        Cancelled,
    }
    let completion = {
        let wait = child.wait();
        tokio::pin!(wait);
        let timeout = tokio::time::sleep(Duration::from_millis(timeout_ms));
        tokio::pin!(timeout);
        tokio::select! {
            biased;
            result = &mut wait => Completion::Finished(result),
            _ = cancel.cancelled() => Completion::Cancelled,
            _ = &mut timeout => Completion::TimedOut,
        }
    };

    match completion {
        Completion::Finished(status) => {
            let status = status.with_context(|| format!("{tool}: failed to wait for shell"))?;
            child
                .cleanup_after_exit(FOREGROUND_REAP_TIMEOUT)
                .await
                .with_context(|| format!("{tool}: failed to clean residual process tree"))?;
            collect_foreground_output(status, stdout_task, stderr_task, tool).await
        }
        Completion::TimedOut => {
            let cleanup = child
                .terminate_and_wait(FOREGROUND_REAP_TIMEOUT)
                .await
                .with_context(|| format!("{tool}: failed to terminate timed-out process tree"));
            let drain = drain_foreground_readers(stdout_task, stderr_task, tool).await;
            cleanup?;
            drain?;
            bail!("{tool}: command timed out after {timeout_ms}ms")
        }
        Completion::Cancelled => {
            let cleanup = child
                .terminate_and_wait(FOREGROUND_REAP_TIMEOUT)
                .await
                .with_context(|| format!("{tool}: failed to terminate interrupted process tree"));
            let drain = drain_foreground_readers(stdout_task, stderr_task, tool).await;
            cleanup?;
            drain?;
            bail!("interrupted")
        }
    }
}

#[derive(Debug)]
pub(super) struct ForegroundOutput {
    status: ProcessExit,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    omitted_bytes: usize,
}

struct BoundedStream {
    bytes: Vec<u8>,
    total_bytes: usize,
}

async fn collect_foreground_output(
    status: ProcessExit,
    stdout_task: tokio::task::JoinHandle<Result<BoundedStream>>,
    stderr_task: tokio::task::JoinHandle<Result<BoundedStream>>,
    tool: &'static str,
) -> Result<ForegroundOutput> {
    let (stdout, stderr) = finish_foreground_readers(stdout_task, stderr_task, tool).await?;
    let omitted_bytes = stdout
        .total_bytes
        .saturating_sub(stdout.bytes.len())
        .saturating_add(stderr.total_bytes.saturating_sub(stderr.bytes.len()));
    Ok(ForegroundOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        omitted_bytes,
    })
}

async fn finish_foreground_readers(
    stdout_task: tokio::task::JoinHandle<Result<BoundedStream>>,
    stderr_task: tokio::task::JoinHandle<Result<BoundedStream>>,
    tool: &'static str,
) -> Result<(BoundedStream, BoundedStream)> {
    let (stdout, stderr) = tokio::join!(
        finish_reader(stdout_task, tool),
        finish_reader(stderr_task, tool)
    );
    Ok((stdout?, stderr?))
}

async fn drain_foreground_readers(
    stdout_task: tokio::task::JoinHandle<Result<BoundedStream>>,
    stderr_task: tokio::task::JoinHandle<Result<BoundedStream>>,
    tool: &'static str,
) -> Result<()> {
    finish_foreground_readers(stdout_task, stderr_task, tool)
        .await
        .map(|_| ())
}

async fn finish_reader(
    mut task: tokio::task::JoinHandle<Result<BoundedStream>>,
    tool: &'static str,
) -> Result<BoundedStream> {
    match tokio::time::timeout(FOREGROUND_REAP_TIMEOUT, &mut task).await {
        Ok(result) => result
            .with_context(|| format!("{tool}: output reader task failed"))?
            .with_context(|| format!("{tool}: failed to drain output pipe")),
        Err(_) => {
            task.abort();
            let _ = task.await;
            bail!("{tool}: timed out draining output pipe after process-tree cleanup")
        }
    }
}

async fn read_bounded_stream(mut reader: impl AsyncRead + Unpin) -> Result<BoundedStream> {
    let mut bytes = Vec::with_capacity(FOREGROUND_STREAM_CAP_BYTES);
    let mut total_bytes = 0usize;
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            break;
        }
        total_bytes = total_bytes.saturating_add(count);
        let retained = FOREGROUND_STREAM_CAP_BYTES.saturating_sub(bytes.len());
        bytes.extend_from_slice(&chunk[..count.min(retained)]);
    }
    Ok(BoundedStream { bytes, total_bytes })
}

/// stdout+stderr merged, a trailing `[exit …]` when the run failed, and a
/// placeholder when empty — the model-facing text for one run (denial
/// annotation is the caller's job, so an escalated re-run reuses this).
pub(super) fn format_output(output: &ForegroundOutput) -> String {
    let mut merged = Vec::with_capacity(output.stdout.len() + output.stderr.len());
    merged.extend_from_slice(&output.stdout);
    merged.extend_from_slice(&output.stderr);
    let mut text = String::from_utf8_lossy(&merged).into_owned();
    let captured_chars = text.chars().count();
    let omitted_chars = captured_chars.saturating_sub(FOREGROUND_OUTPUT_CAP_CHARS);
    if omitted_chars > 0 {
        let boundary = text
            .char_indices()
            .nth(FOREGROUND_OUTPUT_CAP_CHARS)
            .map_or(text.len(), |(index, _)| index);
        text.truncate(boundary);
    }
    if omitted_chars > 0 || output.omitted_bytes > 0 {
        text.push_str(&format!(
            "\n[output truncated: {omitted_chars} characters and {} additional bytes omitted]",
            output.omitted_bytes
        ));
    }
    if !output.status.success() {
        let code = output
            .status
            .code()
            .map_or_else(|| "killed by signal".into(), |c| format!("exit status {c}"));
        text.push_str(&format!("\n[{code}]"));
    }
    if text.is_empty() {
        text = "(no output)".into();
    }
    text
}

pub(super) async fn bash_output_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
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
    let status_line = match &status {
        BgStatus::Running | BgStatus::Stopping if block => {
            format!("{id}: still {} after {timeout_ms}ms", status_text(&status))
        }
        _ => format!("{id}: {}", status_text(&status)),
    };
    let mut tail = read_tail(&path).await;
    if let (BgStatus::Exited(code), Some(sb)) = (&status, sandboxed) {
        if *code != Some(0) && sandbox::is_likely_sandbox_denied(*code, &tail, sb.network_disabled)
        {
            tail.push_str(sandbox::DENIAL_HINT);
        }
    }
    Ok(format!(
        "{status_line}\noutput file: {}\n--- output ---\n{tail}",
        path.display()
    ))
}

pub(super) async fn kill_bash_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = str_arg(input, "bash_id", "kill_bash")?;
    let shells = &ctx.cfg.background_shells;
    let command = shells
        .request_kill(id)
        .map_err(|e| anyhow!("kill_bash: {e}"))?;
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

struct BgShell {
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
    fn spawn_background(
        self: &Arc<Self>,
        command: &str,
        sandbox: Option<&SandboxPolicy>,
        bash: &ShellProgram,
        ctx: &ToolCtx,
    ) -> Result<String> {
        let cwd = ctx.cfg.effective_cwd();
        let offload_dir = &ctx.cfg.offload_dir;
        std::fs::create_dir_all(offload_dir)
            .with_context(|| format!("bash: cannot create {}", offload_dir.display()))?;
        let id = format!("bg-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let path = offload_dir.join(format!("{id}.out"));
        let stdout = std::fs::File::create(&path)
            .with_context(|| format!("bash: cannot create {}", path.display()))?;
        let stderr = stdout
            .try_clone()
            .context("bash: cannot clone output file")?;
        let mut spec = shell_spec(command, &cwd, sandbox, bash);
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
            kind: BackgroundTaskKind::Shell,
            description: command.to_string(),
            status: BackgroundTaskStatus::Running,
            output_path: Some(path.to_string_lossy().to_string()),
            detail: None,
        }));
        // A Weak, not an Arc: an owning ref would keep the registry alive as
        // long as any shell runs, so the synchronous Drop fallback could not fire.
        tokio::spawn(monitor(BackgroundMonitor {
            shells: Arc::downgrade(self),
            id: id.clone(),
            command: command.to_string(),
            child,
            killer,
            kill,
            output_path: path.clone(),
            ui: ctx.ui.clone(),
            inbox: ctx.cfg.inbox.clone(),
        }));
        Ok(format!(
            "Command running in background with ID: {id}. Output is being written to: {}. \
             You will be notified when it changes state. Check on it with bash_output; stop it \
             with kill_bash.",
            path.display()
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
    fn begin_finish(&self, id: &str, observed: BgStatus) -> Option<BgStatus> {
        debug_assert!(!observed.is_active());
        let mut registry = self.state.lock().unwrap();
        let shell = registry.shells.get_mut(id)?;
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

    fn complete_finish(&self, id: &str, status: BgStatus) -> bool {
        let changed = {
            let mut registry = self.state.lock().unwrap();
            let Some(shell) = registry.shells.get_mut(id) else {
                return false;
            };
            if !matches!(shell.status, BgStatus::Finishing) {
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
    id: String,
    command: String,
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
        id,
        command,
        mut child,
        killer,
        kill,
        output_path,
        ui,
        inbox,
    } = monitor;
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
    if let Some(shells) = shells.upgrade() {
        if let Some(status) = shells.begin_finish(&id, observed) {
            let (event_status, detail) = match &status {
                BgStatus::Exited(Some(0)) => {
                    (BackgroundTaskStatus::Completed, Some("exit 0".into()))
                }
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
            let summary = match detail.as_deref() {
                Some(detail) => format!("Background command {command:?} {status_label}: {detail}"),
                None => format!("Background command {command:?} {status_label}"),
            };
            ui.emit(&Event::BackgroundTaskUpdated(BackgroundTask {
                id: id.clone(),
                run_id: None,
                kind: BackgroundTaskKind::Shell,
                description: command,
                status: event_status,
                output_path: Some(output_path.clone()),
                detail: detail.clone(),
            }));
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
            debug_assert!(shells.complete_finish(&id, status));
        }
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
    use super::FOREGROUND_OUTPUT_CAP_CHARS;
    use crate::event::BackgroundTaskStatus;
    use crate::event::Event;
    use crate::inbox::InboxItem;
    use crate::tools::testutil::*;
    #[cfg(windows)]
    use base64::Engine as _;
    use serde_json::json;
    #[cfg(any(unix, windows))]
    use std::path::Path;
    #[cfg(any(unix, windows))]
    use std::path::PathBuf;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::AtomicUsize;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::Ordering;
    use std::sync::Arc;
    use std::sync::Mutex;

    #[cfg(any(unix, windows))]
    static FOREGROUND_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    #[derive(Default)]
    struct RecordingUi {
        events: Mutex<Vec<Event>>,
    }

    impl crate::agent::Ui for RecordingUi {
        fn emit(&self, event: &Event) {
            self.events.lock().unwrap().push(event.clone());
        }
    }

    fn recording_ctx(tag: &str) -> (crate::tools::ToolCtx, Arc<RecordingUi>) {
        let mut ctx = test_ctx(0, tag);
        let ui = Arc::new(RecordingUi::default());
        ctx.ui = ui.clone();
        (ctx, ui)
    }

    #[cfg(windows)]
    #[test]
    fn windows_bash_spec_requires_descendant_debugging() {
        let bash = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .unwrap();
        let spec = super::shell_spec("exit 0", &std::env::current_dir().unwrap(), None, &bash);
        assert!(spec.windows_debug_descendants());
    }

    #[cfg(unix)]
    struct ForegroundTree {
        root: PathBuf,
    }

    #[cfg(unix)]
    impl ForegroundTree {
        fn new(tag: &str) -> Self {
            let sequence = FOREGROUND_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "kloop-foreground-{tag}-{}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(
                root.join("runner.sh"),
                r#"trap '' TERM
root=$1
if [ "$2" = child ]; then
    printf '%s' $$ > "$root/grandchild.pid"
else
    printf '%s' $$ > "$root/child.pid"
    /bin/sh "$0" "$root" child &
fi
exec sleep 60
"#,
            )
            .unwrap();
            Self { root }
        }

        fn path(&self) -> &Path {
            &self.root
        }

        fn command(&self) -> String {
            format!(
                "/bin/sh '{}' '{}'",
                self.root.join("runner.sh").display(),
                self.root.display()
            )
        }
    }

    #[cfg(unix)]
    impl Drop for ForegroundTree {
        fn drop(&mut self) {
            for name in ["child.pid", "grandchild.pid"] {
                let Ok(text) = std::fs::read_to_string(self.root.join(name)) else {
                    continue;
                };
                let Ok(raw) = text.trim().parse::<i32>() else {
                    continue;
                };
                if let Some(pid) = rustix::process::Pid::from_raw(raw) {
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::Kill);
                }
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(windows)]
    struct WindowsBackgroundTree {
        root: PathBuf,
    }

    #[cfg(windows)]
    impl WindowsBackgroundTree {
        fn new(tag: &str) -> Self {
            let sequence = FOREGROUND_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "kloop-background-{tag}-{}-{sequence}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn command(&self) -> String {
            let (programs, _) = crate::shell_programs::resolve_shell_programs(Default::default())
                .expect("native Windows shell discovery succeeds");
            let powershell = programs
                .powershell
                .expect("Windows Bash lifecycle tests require PowerShell 7");
            assert_eq!(
                powershell.flavor,
                crate::shell_programs::ShellFlavor::PowerShell7,
                "Windows Bash lifecycle tests require PowerShell 7"
            );
            self.command_with_powershell(&powershell)
        }

        fn command_with_powershell(
            &self,
            powershell: &crate::shell_programs::ShellProgram,
        ) -> String {
            let root = self.root.to_string_lossy().replace('\'', "''");
            let script = format!(
                r#"$root = '{root}'
$grandchild = Start-Process -FilePath $env:ComSpec -ArgumentList @('/D', '/C', 'ping 127.0.0.1 -n 60 > NUL') -PassThru
[IO.File]::WriteAllText((Join-Path $root 'child.pid'), [string]$PID)
[IO.File]::WriteAllText((Join-Path $root 'grandchild.pid'), [string]$grandchild.Id)
Wait-Process -Id $grandchild.Id
"#
            );
            let bytes = script
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
            let executable = powershell
                .executable
                .to_string_lossy()
                .replace('\\', "/")
                .replace('\'', "'\\''");
            format!("'{executable}' -NoLogo -NoProfile -NonInteractive -EncodedCommand '{encoded}'")
        }

        fn path(&self) -> &Path {
            &self.root
        }
    }

    #[cfg(windows)]
    impl Drop for WindowsBackgroundTree {
        fn drop(&mut self) {
            for name in ["child.pid", "grandchild.pid"] {
                let Ok(text) = std::fs::read_to_string(self.root.join(name)) else {
                    continue;
                };
                let Ok(pid) = text.trim().parse::<u32>() else {
                    continue;
                };
                terminate_windows_process(pid);
            }
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[cfg(windows)]
    fn windows_process_alive(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if process == 0 {
            return match unsafe { GetLastError() } {
                ERROR_INVALID_PARAMETER => false,
                ERROR_ACCESS_DENIED => true,
                error => panic!("cannot probe pid {pid}: Windows error {error}"),
            };
        }
        let wait = unsafe { WaitForSingleObject(process, 0) };
        unsafe {
            CloseHandle(process);
        }
        match wait {
            WAIT_OBJECT_0 => false,
            WAIT_TIMEOUT => true,
            other => panic!("cannot probe pid {pid}: wait result {other}"),
        }
    }

    #[cfg(windows)]
    fn terminate_windows_process(pid: u32) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::TerminateProcess;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        use windows_sys::Win32::System::Threading::INFINITE;
        use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;

        let process = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        if process == 0 {
            return;
        }
        unsafe {
            TerminateProcess(process, 1);
            WaitForSingleObject(process, INFINITE);
            CloseHandle(process);
        }
    }

    #[cfg(windows)]
    async fn wait_for_windows_tree_pids(tree: &WindowsBackgroundTree) -> [u32; 2] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let parsed = ["child.pid", "grandchild.pid"].map(|name| {
                std::fs::read_to_string(tree.path().join(name))
                    .ok()
                    .and_then(|text| text.trim().parse::<u32>().ok())
            });
            if let [Some(child), Some(grandchild)] = parsed {
                return [child, grandchild];
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Windows runner did not publish both pids"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[cfg(windows)]
    async fn assert_windows_processes_dead(pids: [u32; 2]) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let alive = pids.map(windows_process_alive);
            if alive == [false, false] {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "Windows background descendants survived: pids={pids:?}, alive={alive:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    fn process_alive(raw: i32) -> bool {
        let Some(pid) = rustix::process::Pid::from_raw(raw) else {
            return false;
        };
        match rustix::process::test_kill_process(pid) {
            Ok(()) | Err(rustix::io::Errno::PERM) => true,
            Err(rustix::io::Errno::SRCH) => false,
            Err(error) => panic!("cannot probe pid {raw}: {error}"),
        }
    }

    #[cfg(unix)]
    async fn wait_for_tree_pids(tree: &ForegroundTree) -> [i32; 2] {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(3);
        loop {
            let parsed = ["child.pid", "grandchild.pid"].map(|name| {
                std::fs::read_to_string(tree.path().join(name))
                    .ok()
                    .and_then(|text| text.trim().parse::<i32>().ok())
            });
            if let [Some(child), Some(grandchild)] = parsed {
                return [child, grandchild];
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "runner did not publish both pids"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    #[cfg(unix)]
    async fn assert_processes_dead(pids: [i32; 2]) {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let alive = pids.map(process_alive);
            if alive == [false, false] {
                return;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "foreground descendants survived: pids={pids:?}, alive={alive:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }

    /// Pull the `bg-N` id out of the spawn message.
    fn bg_id(spawn_message: &str) -> String {
        spawn_message
            .split("ID: ")
            .nth(1)
            .and_then(|rest| rest.split('.').next())
            .unwrap_or_else(|| panic!("no id in: {spawn_message}"))
            .to_string()
    }

    #[tokio::test]
    async fn model_shell_environment_scrubs_provider_credentials() {
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .expect("test shell is available");
        let mut spec = super::ProcessSpec::new(shell.executable, std::env::current_dir().unwrap());
        spec.arg("-lc");
        spec.arg("env");
        spec.env("OPENAI_API_KEY", "SENTINEL-OPENAI");
        spec.env("ANTHROPIC_API_KEY", "SENTINEL-ANTHROPIC");
        spec.env("TAVILY_API_KEY", "SENTINEL-TAVILY");
        spec.env("KEEP_ME", "visible");
        super::scrub_model_shell_env(&mut spec);
        spec.stdout = super::ProcessStdio::Pipe;
        spec.stderr = super::ProcessStdio::Pipe;
        let mut child = crate::process_tree::spawn(spec).unwrap();
        let mut stdout = child.take_stdout().unwrap();
        let mut bytes = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut stdout, &mut bytes)
            .await
            .unwrap();
        let status = child.wait().await.unwrap();
        child
            .cleanup_after_exit(std::time::Duration::from_secs(2))
            .await
            .unwrap();
        assert!(status.success());
        let text = String::from_utf8(bytes).unwrap();
        assert!(!text.contains("SENTINEL"), "{text}");
        assert!(text.contains("KEEP_ME=visible"), "{text}");
    }

    #[tokio::test]
    async fn bash_merges_output_and_reports_exit_status() {
        let ctx = test_ctx(0, "bash");
        let (out, is_error) = run_tool(
            "bash",
            bash_input("echo to-stdout; echo to-stderr 1>&2; exit 3"),
            &ctx,
        )
        .await;
        assert!(
            !is_error,
            "non-zero exit is reported in content, not as an error result"
        );
        assert!(out.contains("to-stdout"));
        assert!(out.contains("to-stderr"));
        assert!(out.contains("[exit status 3]"));

        let (out, _) = run_tool("bash", bash_input("true"), &ctx).await;
        assert_eq!(out, "(no output)");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn git_bash_preserves_unicode_command_and_space_unicode_cwd() {
        let cwd = std::env::temp_dir().join(format!("kloop git bash 空格 {}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        let bash = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .expect("Git Bash fixture is available");
        let cancel = tokio_util::sync::CancellationToken::new();
        let output = super::run_foreground(
            "printf '你好' > marker.txt; printf stdout; printf stderr >&2",
            &cwd,
            None,
            &bash,
            10_000,
            &cancel,
        )
        .await
        .unwrap();
        let text = super::format_output(&output);
        assert!(text.contains("stdout"), "{text}");
        assert!(text.contains("stderr"), "{text}");
        assert_eq!(
            std::fs::read_to_string(cwd.join("marker.txt")).unwrap(),
            "你好"
        );
        let _ = std::fs::remove_dir_all(cwd);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn windows_bash_rejects_disable_sandbox_even_when_false() {
        let ctx = test_ctx(0, "windows-disable-sandbox");
        let (output, is_error) = run_tool(
            "bash",
            json!({"command": "true", "disable_sandbox": false}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(output.contains("unavailable on Windows"), "{output}");
    }

    #[tokio::test]
    async fn bash_times_out_and_kills_the_child() {
        let ctx = test_ctx(0, "bash-timeout");
        let started = std::time::Instant::now();
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "sleep 30", "timeout_ms": 100}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(out.contains("timed out"));
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    /// A foreground timeout must reap the whole process group, not just the
    /// `sh` leader: a backgrounded grandchild is orphaned and keeps running
    /// unless the group is killed.
    #[tokio::test]
    #[cfg(unix)]
    async fn foreground_timeout_reaps_the_whole_group() {
        let ctx = test_ctx(0, "bash-group-kill");
        let pidfile = std::env::temp_dir().join(format!("kloop-grp-{}.pid", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        // Background a long sleeper (the grandchild), record its pid, then
        // block on `wait`; the timeout has to take the sleeper down with sh.
        let cmd = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let (out, is_error) =
            run_tool("bash", json!({"command": cmd, "timeout_ms": 500}), &ctx).await;
        assert!(is_error && out.contains("timed out"), "{out}");

        // Let the group kill land, then check the sleeper is gone (`kill -0`
        // fails once the process no longer exists).
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let pid = std::fs::read_to_string(&pidfile)
            .expect("pidfile written before the timeout")
            .trim()
            .to_string();
        let alive = std::process::Command::new("kill")
            .arg("-0")
            .arg(&pid)
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(!alive, "grandchild {pid} outlived the group kill");
        let _ = std::fs::remove_file(&pidfile);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn foreground_timeout_reaps_term_ignoring_child_and_grandchild() {
        let tree = ForegroundTree::new("stubborn-timeout");
        let ctx = test_ctx(0, "stubborn-timeout");
        let command = tree.command();
        let execution = run_tool("bash", json!({"command": command, "timeout_ms": 500}), &ctx);
        let (result, pids) = tokio::join!(execution, wait_for_tree_pids(&tree));
        let (out, is_error) = result;
        assert!(is_error && out.contains("timed out"), "{out}");
        assert_processes_dead(pids).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn foreground_cancellation_reaps_term_ignoring_child_and_grandchild() {
        let tree = ForegroundTree::new("stubborn-cancel");
        let ctx = test_ctx(0, "stubborn-cancel");
        let command = tree.command();
        let execution = run_tool(
            "bash",
            json!({"command": command, "timeout_ms": 60_000}),
            &ctx,
        );
        let cancel = async {
            let pids = wait_for_tree_pids(&tree).await;
            ctx.cancel.cancel();
            pids
        };
        let (result, pids) = tokio::join!(execution, cancel);
        let (out, is_error) = result;
        assert!(is_error);
        assert_eq!(out, "interrupted");
        assert_processes_dead(pids).await;
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn foreground_success_does_not_orphan_closed_pipe_child() {
        let tree = ForegroundTree::new("residual-child");
        let pidfile = tree.path().join("child.pid");
        let command = format!(
            "sh -c 'trap \"\" TERM; exec sleep 60' >/dev/null 2>&1 & printf '%s' $! > '{}'",
            pidfile.display()
        );
        let ctx = test_ctx(0, "residual-child");
        let (out, is_error) = run_tool("bash", bash_input(&command), &ctx).await;
        assert!(!is_error, "{out}");
        let pid = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        assert_processes_dead([pid, pid]).await;
    }

    #[tokio::test]
    async fn foreground_large_stdout_and_stderr_are_drained_and_bounded() {
        let ctx = test_ctx(0, "bounded-output");
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            run_tool(
                "bash",
                bash_input("yes A | head -c 200000; yes B | head -c 200000 >&2"),
                &ctx,
            ),
        )
        .await
        .expect("large dual-pipe command deadlocked");
        let (out, is_error) = result;
        assert!(!is_error, "{out}");
        assert!(out.starts_with("A\nA\n"), "{out}");
        assert!(out.contains("[output truncated:"), "{out}");
        assert!(
            out.chars().count() <= FOREGROUND_OUTPUT_CAP_CHARS + 100,
            "bounded output grew to {} characters",
            out.chars().count()
        );
    }

    #[tokio::test]
    async fn background_spawn_then_blocking_output_sees_completion() {
        let ctx = test_ctx(0, "bg-complete");
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "echo bg-hello; echo bg-err 1>&2", "run_in_background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("Command running in background with ID: bg-"));
        assert!(out.contains("Output is being written to:"));
        let id = bg_id(&out);

        // block=true (default) waits for completion.
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
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
            json!({"command": "printf done", "run_in_background": true}),
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
        assert_eq!(updates[0].description, "printf done");
        assert_eq!(updates[0].status, BackgroundTaskStatus::Running);
        assert!(updates[0].output_path.is_some());
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
                assert!(summary.contains("printf done"), "{summary}");
                assert!(summary.contains("exit 0"), "{summary}");
            }
            other => panic!("expected ShellResult, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn background_failure_reports_exit_code() {
        let (ctx, ui) = recording_ctx("bg-fail");
        let (out, _) = run_tool(
            "bash",
            json!({"command": "echo pre; exit 7", "run_in_background": true}),
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
            json!({"command": "sleep 30", "run_in_background": true, "timeout_ms": 10}),
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
        let (out, is_error) = run_tool("kill_bash", json!({"bash_id": id}), &ctx2).await;
        assert!(!is_error, "{out}");
    }

    #[tokio::test]
    async fn kill_bash_stops_a_running_shell() {
        let ctx = test_ctx(0, "bg-kill");
        let (out, _) = run_tool(
            "bash",
            json!({"command": "sleep 30", "run_in_background": true}),
            &ctx,
        )
        .await;
        let id = bg_id(&out);
        let started = std::time::Instant::now();

        let (out, is_error) = run_tool("kill_bash", json!({"bash_id": id}), &ctx).await;
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
        let (out, is_error) = run_tool("kill_bash", json!({"bash_id": id}), &ctx).await;
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
            json!({"command": command, "run_in_background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);
        let pids = wait_for_tree_pids(&tree).await;

        assert_eq!(ctx.cfg.shutdown_background_work().await, 0);
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

        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "true", "run_in_background": true}),
            &ctx,
        )
        .await;
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
            json!({"command": command, "run_in_background": true}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        let id = bg_id(&out);
        let pids = wait_for_windows_tree_pids(&tree).await;

        assert_eq!(ctx.cfg.shutdown_background_work().await, 0);
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
    async fn official_msix_pwsh_breakaways_are_contained_for_every_bash_lifecycle() {
        let Some(powershell) = crate::shell_programs::official_msix_powershell_for_test() else {
            eprintln!("skipped: no official PowerShell MSIX package is installed");
            return;
        };
        assert_eq!(
            powershell.flavor,
            crate::shell_programs::ShellFlavor::PowerShell7
        );

        let tree = WindowsBackgroundTree::new("msix-timeout");
        let ctx = test_ctx(0, "windows-msix-timeout");
        let execution = run_tool(
            "bash",
            json!({
                "command": tree.command_with_powershell(&powershell),
                "timeout_ms": 10_000
            }),
            &ctx,
        );
        let (result, pids) = tokio::join!(execution, wait_for_windows_tree_pids(&tree));
        let (output, is_error) = result;
        assert!(is_error && output.contains("timed out"), "{output}");
        assert_windows_processes_dead(pids).await;

        let tree = WindowsBackgroundTree::new("msix-cancel");
        let ctx = test_ctx(0, "windows-msix-cancel");
        let execution = run_tool(
            "bash",
            json!({
                "command": tree.command_with_powershell(&powershell),
                "timeout_ms": 60_000
            }),
            &ctx,
        );
        let cancellation = async {
            let pids = wait_for_windows_tree_pids(&tree).await;
            ctx.cancel.cancel();
            pids
        };
        let (result, pids) = tokio::join!(execution, cancellation);
        assert_eq!(result, ("interrupted".into(), true));
        assert_windows_processes_dead(pids).await;

        let tree = WindowsBackgroundTree::new("msix-background-kill");
        let ctx = test_ctx(0, "windows-msix-background-kill");
        let (output, is_error) = run_tool(
            "bash",
            json!({
                "command": tree.command_with_powershell(&powershell),
                "run_in_background": true
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let id = bg_id(&output);
        let pids = wait_for_windows_tree_pids(&tree).await;
        let (output, is_error) = run_tool("kill_bash", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "{output}");
        assert_windows_processes_dead(pids).await;

        let tree = WindowsBackgroundTree::new("msix-session-shutdown");
        let ctx = test_ctx(0, "windows-msix-session-shutdown");
        let (output, is_error) = run_tool(
            "bash",
            json!({
                "command": tree.command_with_powershell(&powershell),
                "run_in_background": true
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let pids = wait_for_windows_tree_pids(&tree).await;
        assert_eq!(ctx.cfg.shutdown_background_work().await, 0);
        assert_windows_processes_dead(pids).await;
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
                json!({"command": command, "run_in_background": true}),
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
            json!({"command": "sleep 30", "run_in_background": true}),
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
        let _ = run_tool("kill_bash", json!({"bash_id": id}), &ctx).await;
    }

    #[tokio::test]
    async fn bash_output_returns_only_the_tail_of_large_output() {
        let ctx = test_ctx(0, "bg-tail");
        let (out, _) = run_tool(
            "bash",
            // ~100KB of x's then a marker; the tail must keep the marker and
            // drop the front.
            json!({"command": "yes x | head -c 100000; echo TAIL-MARKER", "run_in_background": true}),
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

    /// Real seatbelt integration: these run the actual /usr/bin/sandbox-exec,
    /// so they are macOS-only; Linux CI covers the sandbox-off path (every
    /// other test in this file) and the pure profile tests in sandbox.rs.
    #[cfg(target_os = "macos")]
    mod seatbelt {
        use super::*;
        use crate::sandbox::SandboxPolicy;
        use crate::sandbox::WritableRoot;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use std::sync::Arc;

        /// A ctx whose bash runs sandboxed with exactly one writable root
        /// (returned canonicalized, seatbelt matches resolved paths).
        fn sandbox_ctx(tag: &str) -> (crate::tools::ToolCtx, std::path::PathBuf) {
            let root = std::env::temp_dir().join(format!("kloop-sbx-{tag}"));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(&root).unwrap();
            let policy = SandboxPolicy {
                writable_roots: vec![WritableRoot {
                    root: root.clone(),
                    read_only_subpaths: vec![root.join(".kloop")],
                }],
                denied_read_paths: Vec::new(),
                allow_network: false,
                auto_allow: true,
                // No approver in test_ctx (allow_all), so escalation always
                // resolves NotAttempted → the model-driven hint; the tests
                // below that need a real escalation build their own ctx.
                escalate: true,
            };
            (with_sandbox(test_ctx(0, tag), policy), root)
        }

        /// An Approver that returns a fixed decision and counts asks — lets
        /// the escalation tests assert both the outcome and that the prompt
        /// fired exactly once.
        struct CountingApprover {
            decision: crate::permissions::Decision,
            asked: Arc<AtomicUsize>,
        }

        impl crate::permissions::Approver for CountingApprover {
            fn confirm(
                &self,
                _req: crate::permissions::ConfirmRequest,
            ) -> std::pin::Pin<
                Box<dyn std::future::Future<Output = crate::permissions::Decision> + Send + '_>,
            > {
                self.asked.fetch_add(1, Ordering::SeqCst);
                let decision = self.decision;
                Box::pin(async move { decision })
            }
        }

        /// A sandboxed ctx whose permission gate has a real (scripted)
        /// approver, so the escalation loop actually runs. auto_allow is on,
        /// so the initial contained call never prompts — only escalation does.
        fn escalating_ctx(
            tag: &str,
            decision: crate::permissions::Decision,
        ) -> (crate::tools::ToolCtx, Arc<AtomicUsize>) {
            let root = std::env::temp_dir().join(format!("kloop-sbx-{tag}"));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(&root).unwrap();
            let asked = Arc::new(AtomicUsize::new(0));
            let approver: Arc<dyn crate::permissions::Approver> = Arc::new(CountingApprover {
                decision,
                asked: asked.clone(),
            });
            let perms = crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &Default::default(),
                root.clone(),
                Some(approver),
                None,
            )
            .unwrap();
            let policy = SandboxPolicy {
                writable_roots: vec![WritableRoot {
                    root,
                    read_only_subpaths: vec![],
                }],
                denied_read_paths: Vec::new(),
                allow_network: false,
                auto_allow: true,
                escalate: true,
            };
            let base = test_ctx(0, tag);
            let mut cfg = (*base.cfg).clone();
            cfg.permissions = Arc::new(perms);
            cfg.sandbox = Some(Arc::new(policy));
            let ctx = crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..base
            };
            (ctx, asked)
        }

        /// A directory outside every writable root of `sandbox_ctx`.
        fn outside_dir(tag: &str) -> std::path::PathBuf {
            let dir = std::env::temp_dir().join(format!("kloop-sbx-out-{tag}"));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::canonicalize(&dir).unwrap()
        }

        #[tokio::test]
        async fn denied_read_path_cannot_be_read_through_a_symlink() {
            let (ctx, root) = sandbox_ctx("secret-read");
            let secret_dir = root.join(".kloop");
            std::fs::create_dir_all(&secret_dir).unwrap();
            let secret = secret_dir.join("config.toml");
            std::fs::write(&secret, "SENTINEL-CREDENTIAL").unwrap();
            let alias = root.join("innocent.txt");
            let _ = std::fs::remove_file(&alias);
            std::os::unix::fs::symlink(&secret, &alias).unwrap();

            let mut cfg = (*ctx.cfg).clone();
            let policy = cfg.sandbox.take().unwrap();
            cfg.sandbox = Some(Arc::new(policy.with_denied_read_path(&secret)));
            let ctx = crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..ctx
            };
            let (out, is_error) = run_tool(
                "bash",
                json!({"command": format!("cat {}", alias.display())}),
                &ctx,
            )
            .await;
            assert!(!is_error, "bash reports non-zero status in content");
            assert!(!out.contains("SENTINEL-CREDENTIAL"), "{out}");
            assert!(
                out.contains("Operation not permitted") || out.contains("Permission denied"),
                "{out}"
            );

            let _ = std::fs::remove_file(alias);
            let _ = std::fs::remove_dir_all(root);
        }

        #[tokio::test]
        async fn write_inside_root_succeeds_outside_gets_denial_hint() {
            let (ctx, root) = sandbox_ctx("inout");
            let inside = root.join("ok.txt");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo hi > {}", inside.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert_eq!(std::fs::read_to_string(&inside).unwrap(), "hi\n");

            let blocked = outside_dir("inout").join("no.txt");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo hi > {}", blocked.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "a denied write is content, not a tool error");
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(
                out.contains("disable_sandbox: true"),
                "hint teaches the escape: {out}"
            );
            assert!(!blocked.exists());
        }

        #[tokio::test]
        async fn read_only_subpath_stays_protected_inside_writable_root() {
            let (ctx, root) = sandbox_ctx("rosub");
            let (out, _) = run_tool(
                "bash",
                bash_input(&format!("mkdir -p {}", root.join(".kloop").display())),
                &ctx,
            )
            .await;
            assert!(out.contains("Operation not permitted"), "{out}");
        }

        #[tokio::test]
        async fn disable_sandbox_escapes_per_call() {
            let (ctx, _) = sandbox_ctx("escape");
            let target = outside_dir("escape").join("escaped.txt");
            let (out, is_error) = run_tool(
                "bash",
                json!({
                    "command": format!("echo freed > {}", target.display()),
                    "disable_sandbox": true
                }),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "freed\n");
        }

        #[tokio::test]
        async fn network_is_denied_where_the_bare_run_connects() {
            // A real local listener makes the pair discriminating: bare
            // connect succeeds, sandboxed connect is the one that fails.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let connect = format!("exec 3<>/dev/tcp/127.0.0.1/{port}");

            let (ctx, _) = sandbox_ctx("net");
            let (out, _) = run_tool("bash", bash_input(&connect), &ctx).await;
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(out.contains("disable_sandbox: true"), "{out}");

            let bare = test_ctx(0, "net-bare");
            let (out, is_error) = run_tool("bash", bash_input(&connect), &bare).await;
            assert!(!is_error, "control run must reach the listener: {out}");
            assert!(!out.contains("Operation not permitted"), "{out}");
        }

        /// Escalation loop, approved: a contained write outside the writable
        /// root is denied by the sandbox, the loop asks once, and on approval
        /// re-runs the command unsandboxed — the write lands and the result
        /// is flagged as escalated, with no denial hint left dangling.
        #[tokio::test]
        async fn escalation_reruns_unsandboxed_on_approval() {
            let (ctx, asked) = escalating_ctx("esc-yes", crate::permissions::Decision::Allow);
            let target = outside_dir("esc-yes").join("climbed.txt");
            let _ = std::fs::remove_file(&target);
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo climbed > {}", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert!(out.contains("Re-ran without the sandbox"), "{out}");
            assert!(
                !out.contains("looks like a sandbox"),
                "no dangling hint: {out}"
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "climbed\n");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "asked exactly once: {out}");
        }

        /// Escalation loop, declined: the sandboxed failure is kept and the
        /// result steers the model away from a disable_sandbox retry (not the
        /// hint, which would invite exactly that).
        #[tokio::test]
        async fn escalation_declined_keeps_denial_and_warns_off_retry() {
            let (ctx, asked) = escalating_ctx("esc-no", crate::permissions::Decision::Deny);
            let target = outside_dir("esc-no").join("nope.txt");
            let _ = std::fs::remove_file(&target);
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo climbed > {}", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(out.contains("declined to run this outside"), "{out}");
            assert!(
                !out.contains("looks like a sandbox"),
                "declined must not double up with the hint: {out}"
            );
            assert!(!target.exists());
            assert_eq!(asked.load(Ordering::SeqCst), 1);
        }

        /// End-to-end through the real dispatch gate: with auto_allow, a
        /// contained bash call (opaque redirect, previously always asked)
        /// runs with NOBODY available to approve; the escaped form and the
        /// auto_allow=false policy both still reach the ask layer and fail.
        #[tokio::test]
        async fn auto_allow_runs_contained_bash_without_an_approver() {
            let (ctx, root) = sandbox_ctx("autoallow");
            let no_approver = || {
                Arc::new(
                    crate::permissions::Permissions::new(
                        crate::permissions::Mode::Manual,
                        &Default::default(),
                        root.clone(),
                        None,
                        None,
                    )
                    .unwrap(),
                )
            };
            let mut cfg = (*ctx.cfg).clone();
            cfg.permissions = no_approver();
            let ctx = crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..ctx
            };

            let target = root.join("auto.txt");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo hi > {}", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "hi\n");

            // Escaping the sandbox forfeits the auto-allow.
            let (out, is_error) = run_tool(
                "bash",
                json!({"command": "touch escaped.txt", "disable_sandbox": true}),
                &ctx,
            )
            .await;
            assert!(is_error);
            assert!(out.contains("approval required"), "{out}");

            // auto_allow = false reverts to slice-1: contained or not, the
            // call asks.
            let mut cfg = (*ctx.cfg).clone();
            let mut policy = (*cfg.sandbox.take().unwrap()).clone();
            policy.auto_allow = false;
            cfg.sandbox = Some(Arc::new(policy));
            cfg.permissions = no_approver();
            let ctx = crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..ctx
            };
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo hi > {}", root.join("b.txt").display())),
                &ctx,
            )
            .await;
            assert!(is_error);
            assert!(out.contains("approval required"), "{out}");
        }

        /// The bg output file lives outside the writable roots; the child
        /// writes it through the inherited fd, which seatbelt permits (checks
        /// happen at open time). This test is the regression lock on that.
        #[tokio::test]
        async fn background_shell_runs_sandboxed_and_reports_denials() {
            let (ctx, _) = sandbox_ctx("bg");
            let (out, is_error) = run_tool(
                "bash",
                json!({"command": "echo bg-sandboxed", "run_in_background": true}),
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
                    "run_in_background": true
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
    async fn unknown_background_ids_error() {
        let ctx = test_ctx(0, "bg-unknown");
        let (out, is_error) = run_tool("bash_output", json!({"bash_id": "bg-99999"}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("no background command"), "{out}");

        let (out, is_error) = run_tool("kill_bash", json!({"bash_id": "bg-99999"}), &ctx).await;
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
