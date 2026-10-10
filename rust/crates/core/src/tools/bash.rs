//! Foreground and background shell execution. The background shape follows
//! Native `background: true` returns immediately with an id and an output file
//! (stdout/stderr interleaved at the fd level — no reader tasks, no pipe
//! deadlock), `bash_output` blocks on completion by default, `stop_bash`
//! kills the whole owned process tree (Unix process group or Windows Job).
//! Interrupting a turn never touches
//! background shells; stop_bash, the size watchdog, and explicit session
//! shutdown reap them. State changes emit session-scoped background-task
//! events. cc's auto-backgrounding and model-visible Monitor tool are not ported.

use std::collections::HashMap;
use std::ffi::OsString;
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
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use super::ToolCtx;
use super::str_arg;
use super::strict_str_arg;
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
use crate::permissions::EscalationOutcome;
use crate::process_tree;
use crate::process_tree::ProcessExit;
use crate::process_tree::ProcessSpec;
use crate::process_tree::ProcessStdio;
use crate::process_tree::ProcessTreeChild;
use crate::process_tree::ProcessTreeKiller;
use crate::sandbox;
use crate::sandbox::SandboxPolicy;
use crate::shell_env::MODEL_SHELL_SECRET_ENV;
use crate::shell_env::ShellLoginEnv;
use crate::shell_programs::ShellFlavor;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BashInput {
    command: String,
    /// Display-only, same contract as run_agent's: it never reaches the shell.
    /// Declared so `deny_unknown_fields` accepts it; its constraints are checked
    /// in `parse_bash_input`, and a background call carries it into the task's
    /// label so the lifecycle row reads as the model's summary.
    #[serde(default, rename = "description")]
    description: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    disable_sandbox: bool,
}

/// The foreground budget this call runs under: the model's `timeout_ms` when it
/// gave one, else 60s. The one place that number is decided, because the outer
/// per-call deadline has to sit strictly above it — a bound derived from a
/// second reading of the same field would drift away from the one that actually
/// kills the process tree.
///
/// A malformed input parses as the default; the executor rejects it a moment
/// later with a real message, and a deadline is not where that gets reported.
pub(super) fn foreground_timeout(input: &Value) -> Duration {
    let timeout_ms = parse_bash_input(input)
        .ok()
        .and_then(|parsed| parsed.timeout_ms)
        .unwrap_or(DEFAULT_FOREGROUND_TIMEOUT_MS);
    Duration::from_millis(timeout_ms)
}

const DEFAULT_FOREGROUND_TIMEOUT_MS: u64 = 60_000;

fn parse_bash_input(input: &Value) -> Result<BashInput> {
    super::optional_display_description(input, "bash")?;
    serde_json::from_value(input.clone()).context("bash: invalid input")
}

pub(super) fn scrub_model_shell_env(spec: &mut ProcessSpec) {
    for name in MODEL_SHELL_SECRET_ENV {
        spec.env_remove(*name);
    }
}

/// The process spec for the user's shell command, wrapped in the OS sandbox
/// when a policy applies. The env vars are hints only; enforcement is the
/// profile. The shell arguments are [`shell_args`]'s business.
fn shell_spec(
    command: &str,
    cwd: &Path,
    sandbox: Option<&SandboxPolicy>,
    bash: &ShellProgram,
    login_env: &ShellLoginEnv,
) -> ProcessSpec {
    let shell_args = shell_args(bash, command, login_env);
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
    // model-controlled shell — the capture is filtered the same way, so this
    // stays the single place the removal is stated.
    scrub_model_shell_env(&mut spec);
    login_env.apply(&mut spec);
    spec
}

/// The arguments one command is run with.
///
/// A capture is replayed into a **non-login** shell: a login `sh`/`bash` re-runs
/// `/etc/profile`, whose `path_helper` would put `/etc/paths` — and `/usr/bin` —
/// back in front of the captured `PATH`. zsh is the one shell that also re-reads
/// a startup file (`~/.zshenv`) when it is neither login nor interactive, so it
/// gets `-f` to suppress every one of them; `bash` and POSIX `sh` read none in
/// this shape. Without a capture the shell stays `-lc`, which is the old
/// behavior.
fn shell_args(bash: &ShellProgram, command: &str, login_env: &ShellLoginEnv) -> Vec<OsString> {
    let mut args = Vec::with_capacity(3);
    if login_env.is_active() {
        if bash.flavor == ShellFlavor::Zsh {
            args.push("-f".into());
        }
        args.push("-c".into());
    } else {
        args.push("-lc".into());
    }
    args.push(command.into());
    args
}

/// The session sandbox policy for this call: disable_sandbox is the model's
/// per-call escape hatch (cc's dangerouslyDisableSandbox shape). The call
/// still went through the permission gate like any other — escaping changes
/// the execution wrapper, never the asking.
fn call_sandbox(input: &Value, workspace: &EffectiveWorkspace) -> Option<Arc<SandboxPolicy>> {
    if input["disable_sandbox"].as_bool().unwrap_or(false) {
        None
    } else {
        workspace.sandbox.clone()
    }
}

/// The per-call verdict dispatch feeds the permission gate's sandbox
/// auto-allow layer: bash, not escaped, and the active policy opts in.
/// Foreground and background take the same wrapper, so one verdict covers
/// both.
pub(super) fn sandbox_auto_allowed(
    name: &str,
    input: &Value,
    workspace: &EffectiveWorkspace,
) -> bool {
    name == "bash" && call_sandbox(input, workspace).is_some_and(|policy| policy.auto_allow)
}

pub(super) async fn bash_tool(
    input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    #[cfg(windows)]
    if input.get("disable_sandbox").is_some() {
        bail!(
            "bash: disable_sandbox is unavailable on Windows because Windows shell sandboxing is not implemented"
        );
    }
    let parsed = parse_bash_input(input)?;
    let command = parsed.command.as_str();
    let bash = ctx
        .cfg
        .shell_programs
        .bash
        .as_ref()
        .context("bash: Git for Windows Bash is unavailable in this session")?;
    let sandbox = if parsed.disable_sandbox {
        None
    } else {
        workspace.sandbox.clone()
    };
    let cwd = workspace.cwd.clone();
    if parsed.background {
        // No timeout in background mode; the watchdog and stop_bash are the
        // safety net.
        return ctx.cfg.background_shells.spawn_background(
            &parsed,
            &cwd,
            sandbox.as_deref(),
            bash,
            ctx,
            workspace,
        );
    }
    run_foreground_bash(command, parsed.timeout_ms, sandbox, ctx, workspace)
        .await
        .map(|run| run.text)
}

/// What one foreground run left behind: the model-facing text, and whether the
/// command that produced it (the escalated re-run, when there was one)
/// succeeded. A scheduled check needs the second without parsing the first.
pub(crate) struct ForegroundRun {
    pub(crate) text: String,
    pub(crate) success: bool,
}

/// The foreground half of [`bash_tool`], after input parsing and the sandbox
/// choice: run, then handle a sandbox denial (escalate or annotate).
pub(crate) async fn run_foreground_bash(
    command: &str,
    timeout_ms: Option<u64>,
    sandbox: Option<Arc<SandboxPolicy>>,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<ForegroundRun> {
    let cwd = workspace.cwd.clone();
    let bash = ctx
        .cfg
        .shell_programs
        .bash
        .as_ref()
        .context("bash: Git for Windows Bash is unavailable in this session")?;
    if ctx.cancel.is_cancelled() {
        bail!("interrupted");
    }
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_FOREGROUND_TIMEOUT_MS);
    // A remembered escalation is consent to run this command uncontained, so
    // the contained attempt is skipped rather than run and thrown away. It is
    // the expensive half — the run that compiles the test binary, binds the
    // port, and only then discovers it may not — and re-running it every time
    // is what made "remember this" feel like it had not been remembered.
    let remembered_escalation = sandbox.as_ref().is_some_and(|policy| policy.escalate)
        && workspace.permissions.sandbox_escalation_remembered(command);
    let effective_sandbox = if remembered_escalation {
        None
    } else {
        sandbox.as_deref()
    };
    let output = run_foreground(
        command,
        &cwd,
        effective_sandbox,
        bash,
        &ctx.cfg.shell_login_env,
        timeout_ms,
        &ctx.cancel,
    )
    .await?;
    let mut text = format_output(&output);
    let success = output.status.success();
    if remembered_escalation {
        return Ok(ForegroundRun {
            text: format!("{}{text}", sandbox::REMEMBERED_ESCALATION_PREFIX),
            success,
        });
    }

    // Sandbox denial handling applies only to an actually-sandboxed run;
    // disable_sandbox / no policy leaves `sandbox` None and skips it.
    if let Some(policy) = &sandbox
        && !output.status.success()
        && let Some(denial) =
            sandbox::classify_sandbox_denial(output.status.code(), &text, !policy.allow_network)
    {
        if policy.escalate {
            // The code-level escalation loop (codex's retry-on-denial):
            // ask once, and on approval re-run the command unsandboxed —
            // one fewer model round-trip than the disable_sandbox hint.
            let asking = workspace
                .permissions
                .escalate_sandbox(command, Some(&denial), ctx.depth);
            // A prompt withdrawn by another call's stop leaves the sandboxed
            // failure standing, exactly as a decline would; the turn is ending.
            let outcome = super::unless_stopped(&ctx.stop, asking)
                .await
                .unwrap_or(EscalationOutcome::Declined);
            match outcome {
                EscalationOutcome::Approved => {
                    let raw = run_foreground(
                        command,
                        &cwd,
                        None,
                        bash,
                        &ctx.cfg.shell_login_env,
                        timeout_ms,
                        &ctx.cancel,
                    )
                    .await?;
                    // What the sandbox refused is the only record of why this
                    // ran uncontained, and the sandboxed output is about to be
                    // dropped for the unsandboxed one. Carrying the line keeps
                    // the escalation attributable after the fact.
                    return Ok(ForegroundRun {
                        text: format!(
                            "{}{}",
                            sandbox::escalated_prefix(&denial),
                            format_output(&raw)
                        ),
                        success: raw.status.success(),
                    });
                }
                EscalationOutcome::Declined => text.push_str(sandbox::ESCALATION_DECLINED),
                EscalationOutcome::Stopped => {
                    ctx.stop.cancel();
                    text.push_str(sandbox::ESCALATION_STOPPED);
                }
                EscalationOutcome::NotAttempted => text.push_str(sandbox::DENIAL_HINT),
            }
        } else {
            text.push_str(sandbox::DENIAL_HINT);
        }
    }
    Ok(ForegroundRun { text, success })
}

/// One foreground shell run. The root is awaited independently from pipe EOF:
/// once it exits, residual tree members are killed before readers are joined.
async fn run_foreground(
    command: &str,
    cwd: &Path,
    sandbox: Option<&SandboxPolicy>,
    bash: &ShellProgram,
    login_env: &ShellLoginEnv,
    timeout_ms: u64,
    cancel: &CancellationToken,
) -> Result<ForegroundOutput> {
    let spec = shell_spec(command, cwd, sandbox, bash, login_env);
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

pub(super) async fn stop_bash_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let id = strict_str_arg(input, "bash_id", "stop_bash")?;
    if let Some(hint) = super::background_executions::execution_stop_hint(id) {
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
    fn spawn_background(
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
        let mut spec = shell_spec(command, cwd, sandbox, bash, &ctx.cfg.shell_login_env);
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
    use super::BLOCK_TIMEOUT_MAX_MS;
    use super::BgStatus;
    use super::FOREGROUND_OUTPUT_CAP_CHARS;
    use super::status_text;
    use super::wait_status_line;
    use crate::event::BackgroundTaskStatus;
    use crate::event::Event;
    use crate::execution_provenance::DeliveryRoute;
    use crate::execution_provenance::MailboxRoute;
    use crate::execution_provenance::TerminalOwner;
    use crate::inbox::InboxItem;
    use crate::shell_env::ShellLoginEnv;
    use crate::shell_programs::ShellFlavor;
    use crate::tools::testutil::*;
    #[cfg(windows)]
    use base64::Engine as _;
    use serde_json::json;
    #[cfg(any(unix, windows))]
    use std::path::Path;
    #[cfg(any(unix, windows))]
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::Mutex;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::AtomicUsize;
    #[cfg(any(unix, windows))]
    use std::sync::atomic::Ordering;

    #[cfg(any(unix, windows))]
    static FOREGROUND_TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

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
        let spec = super::shell_spec(
            "exit 0",
            &std::env::current_dir().unwrap(),
            None,
            &bash,
            &ShellLoginEnv::none(),
        );
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
                    let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
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
        use windows_sys::Win32::Foundation::ERROR_ACCESS_DENIED;
        use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
        use windows_sys::Win32::Foundation::GetLastError;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::Foundation::WAIT_TIMEOUT;
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let process = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if process.is_null() {
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
        use windows_sys::Win32::System::Threading::INFINITE;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::PROCESS_TERMINATE;
        use windows_sys::Win32::System::Threading::TerminateProcess;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;

        let process = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
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

    /// Pull the `bg-*` id out of the spawn message.
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

    /// The shell's environment is the user's, minus this process's own
    /// credentials — and nothing else, whether or not the network is denied.
    /// kloop used to also strip the proxy variables under a denied network
    /// (plan 170); it no longer does (plan 173), because a command has to
    /// behave the same run by hand as run here.
    #[test]
    fn only_this_process_credentials_leave_the_shell_environment() {
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .expect("test shell is available");
        let cwd = std::env::current_dir().unwrap();
        let scrubbed = |allow_network: bool| {
            let policy = crate::sandbox::SandboxPolicy::workspace(&cwd, &[], allow_network);
            let spec =
                super::shell_spec("true", &cwd, Some(&policy), &shell, &ShellLoginEnv::none());
            let mut names: Vec<String> = spec
                .env_remove
                .iter()
                .map(|name| name.to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let sorted = |names: &[&str]| {
            let mut owned: Vec<String> = names.iter().map(|n| (*n).to_string()).collect();
            owned.sort();
            owned
        };
        let secrets = [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "OPENAI_API_KEY",
            "TAVILY_API_KEY",
            "BRAVE_API_KEY",
        ];
        assert_eq!(scrubbed(/*allow_network*/ false), sorted(&secrets));
        assert_eq!(scrubbed(/*allow_network*/ true), sorted(&secrets));
    }

    /// A captured login environment only survives in a non-login shell: `/etc/
    /// profile`'s `path_helper` would otherwise put `/usr/bin` back in front of
    /// the captured `PATH`. zsh additionally gets `-f`, because it re-reads
    /// `~/.zshenv` even when it is neither login nor interactive — a file that
    /// can export a credential the capture filtered out. Without a capture the
    /// shell stays login `-lc`.
    #[test]
    #[cfg(unix)]
    fn an_active_login_environment_runs_the_shell_non_login() {
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .expect("test shell is available");
        let cwd = std::env::current_dir().unwrap();
        let login = ShellLoginEnv::test_fixture(&[("PATH", "/login/bin")]);

        let active = super::shell_spec("true", &cwd, None, &shell, &login);
        assert_eq!(
            active.args,
            vec![
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("true")
            ]
        );
        assert!(active.env_add.contains(&(
            std::ffi::OsString::from("PATH"),
            std::ffi::OsString::from("/login/bin")
        )));

        let zsh = crate::shell_programs::ShellProgram {
            flavor: ShellFlavor::Zsh,
            ..shell.clone()
        };
        assert_eq!(
            super::shell_spec("true", &cwd, None, &zsh, &login).args,
            vec![
                std::ffi::OsString::from("-f"),
                std::ffi::OsString::from("-c"),
                std::ffi::OsString::from("true")
            ],
            "zsh suppresses its startup files when a capture is replayed"
        );

        let inactive = super::shell_spec("true", &cwd, None, &shell, &ShellLoginEnv::none());
        assert_eq!(
            inactive.args,
            vec![
                std::ffi::OsString::from("-lc"),
                std::ffi::OsString::from("true")
            ]
        );
        assert!(
            !inactive
                .env_add
                .iter()
                .any(|(name, _)| name == std::ffi::OsStr::new("PATH"))
        );
    }

    /// The whole point of the capture: a variable the login shell exports is
    /// visible to the command kloop runs.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_captured_login_environment_reaches_the_command() {
        let ctx = test_ctx_with_cfg(
            0,
            TestConfig::new("bash-login-env")
                .login_env(ShellLoginEnv::test_fixture(&[(
                    "KLOOP_TEST_SENTINEL",
                    "from-login",
                )]))
                .build(),
        );
        let (out, is_error) = run_tool(
            "bash",
            bash_input("printf %s \"$KLOOP_TEST_SENTINEL\""),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("from-login"), "{out}");
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
            &ShellLoginEnv::none(),
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
    #[ignore = "requires a registered official Microsoft PowerShell MSIX fixture"]
    async fn official_msix_pwsh_breakaways_are_contained_for_every_bash_lifecycle() {
        let powershell = crate::shell_programs::official_msix_powershell_for_test().expect(
            "the provisioned acceptance host must expose a valid Microsoft.PowerShell_8wekyb3d8bbwe or Microsoft.PowerShell-LTS_8wekyb3d8bbwe MSIX package",
        );
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
                "background": true
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let id = bg_id(&output);
        let pids = wait_for_windows_tree_pids(&tree).await;
        let (output, is_error) = run_tool("stop_bash", json!({"bash_id": id}), &ctx).await;
        assert!(!is_error, "{output}");
        assert_windows_processes_dead(pids).await;

        let tree = WindowsBackgroundTree::new("msix-session-shutdown");
        let ctx = test_ctx(0, "windows-msix-session-shutdown");
        let (output, is_error) = run_tool(
            "bash",
            json!({
                "command": tree.command_with_powershell(&powershell),
                "background": true
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        let pids = wait_for_windows_tree_pids(&tree).await;
        assert_eq!(ctx.cfg.shutdown_background_work(&ctx.ui).await, 0);
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

    /// Real seatbelt integration: these run the actual /usr/bin/sandbox-exec,
    /// so they are macOS-only; Linux CI covers the sandbox-off path (every
    /// other test in this file) and the pure profile tests in sandbox.rs.
    #[cfg(target_os = "macos")]
    mod seatbelt {
        use super::*;
        use crate::sandbox::SandboxPolicy;
        use crate::sandbox::WritableRoot;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        /// A ctx whose bash runs sandboxed with exactly one writable root
        /// (returned canonicalized, seatbelt matches resolved paths).
        fn sandbox_ctx(tag: &str) -> (crate::tools::ToolCtx, std::path::PathBuf) {
            let root = std::env::temp_dir().join(format!("kloop-sbx-{tag}"));
            std::fs::create_dir_all(&root).unwrap();
            let root = std::fs::canonicalize(&root).unwrap();
            let policy = SandboxPolicy {
                writable_roots: vec![WritableRoot {
                    root: root.clone(),
                    origins: vec![crate::sandbox::WritableRootOrigin::Workspace],
                    read_only_subpaths: vec![root.join(".kloop")],
                }],
                denied_read_paths: Vec::new(),
                allowed_read_paths: Vec::new(),
                denied_write_paths: Vec::new(),
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
            )
            .unwrap();
            let policy = SandboxPolicy {
                writable_roots: vec![WritableRoot {
                    root,
                    origins: vec![crate::sandbox::WritableRootOrigin::Workspace],
                    read_only_subpaths: vec![],
                }],
                denied_read_paths: Vec::new(),
                allowed_read_paths: Vec::new(),
                denied_write_paths: Vec::new(),
                allow_network: false,
                auto_allow: true,
                escalate: true,
            };
            let base = test_ctx(0, tag);
            let mut cfg = base.cfg.test_clone();
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

            let mut cfg = ctx.cfg.test_clone();
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
        async fn private_state_stays_unwritable_when_it_is_the_workspace_root() {
            let (ctx, root) = sandbox_ctx("private-state-overlap");
            let mut cfg = ctx.cfg.test_clone();
            let policy = cfg.sandbox.take().unwrap();
            cfg.sandbox = Some(Arc::new(policy.with_denied_write_path(&root)));
            let ctx = crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..ctx
            };
            let target = root.join("permissions.json");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo forged > {}", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "a denied write is content, not a tool error");
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(!target.exists());
            let _ = std::fs::remove_dir_all(root);
        }

        #[tokio::test]
        async fn worktree_policy_cannot_write_the_previous_workspace() {
            let (ctx, previous) = sandbox_ctx("workspace-replace");
            let worktree = outside_dir("workspace-replace-tree");
            let mut cfg = ctx.cfg.test_clone();
            let policy = cfg.sandbox.take().unwrap();
            cfg.sandbox = Some(Arc::new(policy.for_workspace(&worktree)));
            let ctx = crate::tools::ToolCtx {
                cfg: Arc::new(cfg),
                ..ctx
            };

            let worktree_file = worktree.join("allowed.txt");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo tree > {}", worktree_file.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert_eq!(std::fs::read_to_string(&worktree_file).unwrap(), "tree\n");

            let previous_file = previous.join("blocked.txt");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo base > {}", previous_file.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "a denied write is content, not a tool error");
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(!previous_file.exists());
            let _ = std::fs::remove_dir_all(previous);
            let _ = std::fs::remove_dir_all(worktree);
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

        /// The denied network is a boundary around the machine, not around the
        /// socket API: a command must still reach a listener of its own (plan
        /// 170 — Go's httptest is this shape, and denying it is what drove a
        /// whole session out of the sandbox), while anything off the host stays
        /// refused.
        #[tokio::test]
        async fn loopback_stays_reachable_while_the_machine_boundary_holds() {
            // A real local listener makes the first half discriminating: the
            // connect can only succeed if something is actually listening.
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();

            let (ctx, _) = sandbox_ctx("net");
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("exec 3<>/dev/tcp/127.0.0.1/{port}")),
                &ctx,
            )
            .await;
            assert!(!is_error, "loopback must stay reachable sandboxed: {out}");
            assert!(!out.contains("Operation not permitted"), "{out}");

            // TEST-NET-1 (RFC 5737), never routable. Seatbelt refuses at the
            // connect syscall, so this needs no network and cannot hang on one.
            let (out, _) =
                run_tool("bash", bash_input("exec 3<>/dev/tcp/192.0.2.1/80"), &ctx).await;
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(out.contains("disable_sandbox: true"), "{out}");
        }

        /// Escalation loop, approved: a contained write outside the writable
        /// root is denied by the sandbox, the loop asks once, and on approval
        /// re-runs the command unsandboxed — the write lands and the result
        /// is flagged as escalated, with no denial hint left dangling.
        #[tokio::test]
        async fn escalation_reruns_unsandboxed_on_approval() {
            let (ctx, asked) = escalating_ctx(
                "esc-yes",
                crate::permissions::Decision::Allow(crate::permissions::ApprovalScope::Once),
            );
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

        /// A remembered escalation skips the contained attempt entirely: the
        /// command runs uncontained on the first try, nobody is asked, and the
        /// result says so. Remembering the answer without this only saves the
        /// click — the expensive half is the run that was always going to be
        /// thrown away.
        #[tokio::test]
        async fn a_remembered_escalation_skips_the_sandboxed_attempt() {
            let (ctx, asked) = escalating_ctx(
                "esc-remembered",
                crate::permissions::Decision::Allow(
                    crate::permissions::ApprovalScope::WorkspaceSession,
                ),
            );
            // `mkdir -p` so the remembered two-word prefix is the subcommand,
            // not the path — the second call must be a *different* argument
            // covered by the same rule, which is the point of the prefix.
            let dir = outside_dir("esc-remembered");
            let first = dir.join("one");
            let second = dir.join("two");
            let _ = std::fs::remove_dir_all(&first);
            let _ = std::fs::remove_dir_all(&second);

            // First call: denied inside the sandbox, asked once, approved with
            // the workspace scope.
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("mkdir -p {}", first.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert!(out.contains("Re-ran without the sandbox"), "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1);

            // Second call, same two-word prefix: no ask, and no contained
            // attempt — the marker says the rule covered it up front rather
            // than that a denial was escalated.
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("mkdir -p {}", second.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "not asked again: {out}");
            assert!(
                out.contains("a remembered sandbox_escalate rule already covers this"),
                "{out}"
            );
            assert!(
                !out.contains("Re-ran without the sandbox"),
                "the contained attempt must not have run: {out}"
            );
            assert!(
                !out.contains("Operation not permitted"),
                "no denial output from a skipped attempt: {out}"
            );
            assert!(second.is_dir(), "the uncontained run actually did the work");
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

        /// Escalation loop, answered No (plan 216): the sandboxed failure is
        /// kept as on a decline, the turn is stopped, and the model is told to
        /// wait for the user rather than find another way.
        #[tokio::test]
        async fn escalation_no_keeps_denial_and_stops_the_turn() {
            let (ctx, asked) = escalating_ctx("esc-stop", crate::permissions::Decision::Stop);
            let target = outside_dir("esc-stop").join("nope.txt");
            let _ = std::fs::remove_file(&target);
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo climbed > {}", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(out.ends_with(crate::sandbox::ESCALATION_STOPPED), "{out}");
            assert!(ctx.stop.is_cancelled());
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
                    )
                    .unwrap(),
                )
            };
            let mut cfg = ctx.cfg.test_clone();
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
            let mut cfg = ctx.cfg.test_clone();
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
    async fn background_field_is_strict_and_unknown_fields_fail_closed() {
        let ctx = test_ctx(0, "bash-background-field");
        // Nothing here is spelled the way another product spells it — cc's
        // `run_in_background` and its `timeout` both have to fail rather than
        // be ignored, and the strict deserializer is what says so. There is no
        // hand-written alias for either: a name kloop does not answer to is a
        // name kloop does not answer to.
        for field in [json!({"run_in_background": true}), json!({"timeout": 5000})] {
            let (name, value) = field.as_object().unwrap().iter().next().unwrap();
            let mut input = json!({"command": "printf should-not-run"});
            input[name] = value.clone();
            let (output, is_error) = run_tool("bash", input, &ctx).await;
            assert!(is_error, "{output}");
            assert!(
                output.contains(&format!("unknown field `{name}`")),
                "{output}"
            );
        }
        let (wrong_type, wrong_type_error) = run_tool(
            "bash",
            json!({"command": "printf should-not-run", "background": "yes"}),
            &ctx,
        )
        .await;
        assert!(wrong_type_error);
        assert!(wrong_type.contains("invalid type"), "{wrong_type}");
        assert_eq!(ctx.cfg.background_shells.running_count(), 0);
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

        // Keep the test-module evidence range stable for the pinned parity corpus.
        let (out, is_error) = run_tool("bash_output", json!({}), &ctx).await;
        assert!(is_error);
        assert!(
            out.contains("missing required string argument 'bash_id'"),
            "{out}"
        );
    }
}
