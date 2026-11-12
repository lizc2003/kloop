//! Bash 工具入口、前台执行与共用的 shell 启动配置。

mod background;
#[cfg(test)]
mod testutil;

pub use background::BackgroundShells;
pub(super) use background::bash_output_tool;
pub(super) use background::stop_bash_tool;

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncRead;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

use super::ToolCtx;
use crate::agent::Ui;
use crate::config::EffectiveWorkspace;
use crate::event::Event;
use crate::permissions::Escalation;
use crate::permissions::EscalationOutcome;
use crate::process_tree;
use crate::process_tree::ProcessExit;
use crate::process_tree::ProcessSpec;
use crate::process_tree::ProcessStdio;
use crate::sandbox;
use crate::sandbox::SandboxPolicy;
use crate::shell_env::MODEL_SHELL_SECRET_ENV;
use crate::shell_env::ShellLoginEnv;
use crate::shell_programs::ShellFlavor;
use crate::shell_programs::ShellProgram;

/// Each foreground fd is drained to EOF while retaining at most this many
/// bytes. The final model-visible merge is tighter, matching CC's default
/// inline Bash budget without inheriting its unbounded child-pipe buffering.
/// 截断时落盘给模型回读的也是这部分（plan 272），所以它同时是那个文件的上限：
/// 大 diff 应当一次落全，而不是让模型分段重跑。
const FOREGROUND_STREAM_CAP_BYTES: usize = 1 << 20;
const FOREGROUND_OUTPUT_CAP_CHARS: usize = 30_000;
/// When the merged capture exceeds the inline budget, at least this much of
/// stderr's tail is kept. stdout fills the budget first, so without a reserve a
/// failure reason written to stderr never reached the model at all.
const FOREGROUND_STDERR_TAIL_CHARS: usize = 8_000;
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

/// The longest foreground budget any tool accepts. Ten minutes is long enough
/// for a slow build and short enough that a stuck command still gives the model
/// an answer; anything longer belongs in `background`.
pub(super) const MAX_TIMEOUT_MS: u64 = 600_000;

/// The one bound both `bash` and `powershell` apply to `timeout_ms`. A zero
/// budget kills the command before its first poll, and an unbounded one lets a
/// single call outlive the turn it belongs to.
pub(super) fn bounded_timeout_ms(timeout_ms: u64, tool: &str) -> Result<u64> {
    if (1..=MAX_TIMEOUT_MS).contains(&timeout_ms) {
        Ok(timeout_ms)
    } else {
        bail!("{tool}: timeout_ms must be between 1 and {MAX_TIMEOUT_MS}")
    }
}

/// Reject a malformed call before it reaches the permission gate.
pub(super) fn validate_input(input: &Value) -> Result<()> {
    parse_bash_input(input).map(drop)
}

fn parse_bash_input(input: &Value) -> Result<BashInput> {
    super::optional_display_description(input, "bash")?;
    let mut parsed: BashInput =
        serde_json::from_value(input.clone()).context("bash: invalid input")?;
    // A background call ignores the field, so its value cannot mislead anyone;
    // a foreground call is the one the number actually kills.
    if !parsed.background
        && let Some(timeout_ms) = parsed.timeout_ms
    {
        parsed.timeout_ms = Some(bounded_timeout_ms(timeout_ms, "bash")?);
    }
    Ok(parsed)
}

pub(super) fn scrub_model_shell_env(spec: &mut ProcessSpec) {
    for name in MODEL_SHELL_SECRET_ENV {
        spec.env_remove(*name);
    }
}

const REPLAY_PRELUDE: &str = ". \"$2\" || exit $?; eval 'set --; '\"$1\"";

/// The process spec for the user's shell command, wrapped in the OS sandbox
/// when a policy applies. The env vars are hints only; enforcement is the
/// profile. The shell arguments are [`shell_args`]'s business.
fn shell_spec(
    command: &str,
    cwd: &Path,
    sandbox: Option<&SandboxPolicy>,
    bash: &ShellProgram,
    login_env: &ShellLoginEnv,
    ui: &dyn Ui,
) -> ProcessSpec {
    let shell_args = shell_args(bash, command, login_env, ui);
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
/// A capture is replayed by the shell itself. `-f` suppresses every startup file
/// **except** `/etc/zshenv`, which zsh documents as unavoidable ("commands are
/// first read from /etc/zshenv; this cannot be overridden"), so a fixed prelude
/// re-applies the capture after it and only then runs the command. The command
/// arrives as `$1`, so it never has to be quoted into a script. Without a
/// capture the shell stays `-lc`, which is the old behavior.
fn shell_args(
    bash: &ShellProgram,
    command: &str,
    login_env: &ShellLoginEnv,
    ui: &dyn Ui,
) -> Vec<OsString> {
    let replay_path = match login_env.replay_path_for_use() {
        Ok(Some(path)) => path,
        Ok(None) => return vec!["-lc".into(), command.into()],
        Err(error) => {
            ui.emit(&Event::Note(format!(
                "warning: could not prepare the login environment replay file ({error}); \
                 this command will use a login shell (-lc); later commands will retry"
            )));
            return vec!["-lc".into(), command.into()];
        }
    };
    let mut args: Vec<OsString> = Vec::with_capacity(6);
    if bash.flavor == ShellFlavor::Zsh {
        args.push("-f".into());
    }
    args.push("-c".into());
    args.push(REPLAY_PRELUDE.into());
    // `$0`, so the command sees what `-c` would have given it.
    args.push(bash.executable.clone().into_os_string());
    args.push(command.into());
    args.push(replay_path.into_os_string());
    args
}

pub(super) fn execution_context<'a>(
    name: &str,
    workspace: &'a EffectiveWorkspace,
    login_env: &'a ShellLoginEnv,
) -> sandbox::ExecutionContext<'a> {
    let policy = (name == "bash")
        .then_some(workspace.sandbox.as_deref())
        .flatten();
    sandbox::ExecutionContext::sandboxed(policy)
        .with_shell_home((name == "bash").then(|| replayed_home(login_env)).flatten())
}

pub(super) fn replayed_home(login_env: &ShellLoginEnv) -> Option<&str> {
    // 回放失败会退回登录 shell，其启动文件可以改变环境，不能拿父进程的 HOME 猜。
    login_env.replay_path_for_use().ok().flatten()?;
    if login_env.get("IFS").is_some_and(|ifs| ifs != " \t\n") {
        return None;
    }
    login_env.get("HOME")
}

pub(super) async fn bash_tool(
    input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    verdict: &crate::permissions::Verdict,
    facts: Option<&crate::shell::BashFacts>,
) -> Result<String> {
    let parsed = parse_bash_input(input)?;
    let command = parsed.command.as_str();
    let bash = ctx
        .cfg
        .shell_programs
        .bash
        .as_ref()
        .context("bash: Git for Windows Bash is unavailable in this session")?;
    // The gate already decided containment from the same facts; execution
    // takes it as written. `Escalated` is remembered consent to stay uncontained.
    let sandbox = sandbox_for(verdict, workspace);
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
            verdict,
            facts,
        );
    }
    run_foreground_bash(
        command,
        parsed.timeout_ms,
        sandbox,
        ctx,
        workspace,
        verdict,
        facts,
    )
    .await
    .map(|run| run.text)
}

/// The executor's whole sandbox decision: the gate's plan, read verbatim.
fn sandbox_for(
    verdict: &crate::permissions::Verdict,
    workspace: &EffectiveWorkspace,
) -> Option<Arc<SandboxPolicy>> {
    match verdict.sandbox {
        crate::permissions::SandboxPlan::Contained => workspace.sandbox.clone(),
        crate::permissions::SandboxPlan::None | crate::permissions::SandboxPlan::Escalated => None,
    }
}

/// Whether a zero exit status may hide the failure the sandbox caused. bash
/// reports only the last command's status, so in `make test 2>&1 | tail -20`
/// or `make install; echo done` the part that did the work can be denied while
/// the call still exits 0 — and, with no way out of the sandbox but the
/// escalation (plan 283), that run would end there. Only when the last command
/// is read-only (it is not what the sandbox stopped) and something before it
/// had an effect or a redirect writes.
pub(super) fn status_may_be_masked(command: &str, facts: Option<&crate::shell::BashFacts>) -> bool {
    let computed;
    let facts = match facts {
        Some(facts) => facts,
        None => {
            computed = crate::shell::analyze_bash_facts(command);
            &computed
        }
    };
    let read_only = |command: &crate::shell::CommandFact| {
        command
            .argv()
            .is_some_and(|argv| crate::shell::argv_is_readonly(&argv))
    };
    let Some((status, wrappers)) = status_command(&facts.commands) else {
        return false;
    };
    read_only(&facts.commands[status])
        && (facts.commands.iter().enumerate().any(|(index, command)| {
            index != status && !wrappers.contains(&index) && !read_only(command)
        }) || facts
            .redirects
            .iter()
            .any(|redirect| redirect.direction == crate::shell::Direction::Write))
}

/// The command whose exit status the shell reports, as an index into the
/// analysis' command list, plus the `-c` wrappers it passes through. The list
/// is not in execution order: a wrapper's own script is analysed first, so
/// `sh -c 'touch x | tail -5'` lists `touch`, `tail`, then `sh`, with the inner
/// two one `script_depth` deeper. A last entry preceded by deeper commands is
/// such a wrapper, and its status is its script's.
fn status_command(commands: &[crate::shell::CommandFact]) -> Option<(usize, Vec<usize>)> {
    let mut wrappers = Vec::new();
    let (mut start, mut end) = (0, commands.len());
    loop {
        let last = end.checked_sub(1).filter(|last| *last >= start)?;
        let depth = commands[last].context.script_depth;
        let inner_start = commands[start..last]
            .iter()
            .rposition(|command| command.context.script_depth <= depth)
            .map_or(start, |offset| start + offset + 1);
        if inner_start == last {
            return Some((last, wrappers));
        }
        wrappers.push(last);
        (start, end) = (inner_start, last);
    }
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
    verdict: &crate::permissions::Verdict,
    facts: Option<&crate::shell::BashFacts>,
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
    // the contained attempt is skipped rather than run and thrown away. The
    // gate decided that once; the contained run is the expensive half — the
    // run that compiles the test binary, binds the port, and only then
    // discovers it may not.
    let remembered_escalation = verdict.sandbox == crate::permissions::SandboxPlan::Escalated;
    if verdict.protection == crate::permissions::Protection::Required && sandbox.is_none() {
        bail!(sandbox::PROTECTION_REQUIRED);
    }
    let output = run_foreground(
        command,
        &cwd,
        sandbox.as_deref(),
        bash,
        timeout_ms,
        ctx,
        verdict.protection,
    )
    .await?;
    fail_if_timed_out(&output, Some(&ctx.cfg.offload_dir))?;
    let success = output.status.success();
    if remembered_escalation {
        return Ok(ForegroundRun {
            text: format!(
                "{}{}",
                sandbox::REMEMBERED_ESCALATION_PREFIX,
                format_output(&output, Some(&ctx.cfg.offload_dir))
            ),
            success,
        });
    }

    // 沙箱内这次的结果确实要交给模型时才格式化并落盘：升级获批会换成重跑的结果，
    // 先落盘的话第一次的全文就成了无人引用的文件。
    let mut hint = None;
    // Sandbox denial handling applies only to an actually-sandboxed run; no
    // policy, or a remembered escalation, leaves `sandbox` None and skips it.
    // 判定看完整捕获的输出而不是内联文本：合并顺序是先 stdout 后 stderr，stdout 超过
    // 内联上限时，stderr 里的拒绝行会被截掉，升级和提示就都不会出现。
    // A zero a pipe or `;` may have masked is judged on strong evidence only.
    if let Some(policy) = &sandbox
        && let Some(denial) = if !output.status.success() {
            sandbox::classify_sandbox_denial(
                output.status.code(),
                &captured_text(&output),
                !policy.allow_network,
            )
        } else if status_may_be_masked(command, facts) {
            sandbox::classify_masked_denial(&captured_text(&output), !policy.allow_network)
        } else {
            None
        }
    {
        if verdict.protection == crate::permissions::Protection::Required {
            hint = Some(sandbox::PROTECTION_REQUIRED);
        } else {
            // The code-level escalation loop (codex's retry-on-denial):
            // ask once, and on approval re-run the command unsandboxed. The
            // model has no way out of the sandbox of its own (plan 283).
            let asking = workspace.permissions.escalate_sandbox_with_facts(
                command,
                Some(&denial),
                ctx.depth,
                facts,
            );
            // A prompt withdrawn by another call's stop leaves the sandboxed
            // failure standing, exactly as a decline would; the turn is ending.
            // Cancelling the turn itself withdraws it too, but must end the
            // call as interrupted rather than let the stale failure read as an
            // answer.
            let escalation = tokio::select! {
                biased;
                _ = ctx.stop.cancelled() => Escalation {
                    outcome: EscalationOutcome::Declined,
                    notice: None,
                    approval: None,
                },
                _ = ctx.cancel.cancelled() => bail!("interrupted"),
                escalation = asking => escalation,
            };
            ctx.journal_approval(escalation.approval.clone()).await;
            if let Some(notice) = &escalation.notice {
                ctx.ui.emit(&Event::Note(notice.message.clone()));
            }
            match escalation.outcome {
                EscalationOutcome::Approved(approval) => {
                    let raw = run_foreground(
                        command,
                        &cwd,
                        None,
                        bash,
                        timeout_ms,
                        ctx,
                        verdict.protection,
                    )
                    .await?;
                    fail_if_timed_out(&raw, Some(&ctx.cfg.offload_dir))?;
                    // What the sandbox refused is the only record of why this
                    // ran uncontained, and the sandboxed output is about to be
                    // dropped for the unsandboxed one. Carrying the line keeps
                    // the escalation attributable after the fact.
                    return Ok(ForegroundRun {
                        text: format!(
                            "{}{}",
                            sandbox::escalated_prefix(&denial, approval),
                            format_output(&raw, Some(&ctx.cfg.offload_dir))
                        ),
                        success: raw.status.success(),
                    });
                }
                EscalationOutcome::Declined => hint = Some(sandbox::ESCALATION_DECLINED),
                EscalationOutcome::Stopped => {
                    ctx.stop.cancel();
                    hint = Some(sandbox::ESCALATION_STOPPED);
                }
                EscalationOutcome::NotAttempted => hint = Some(sandbox::DENIAL_HINT),
            }
        }
    }
    let mut text = format_output(&output, Some(&ctx.cfg.offload_dir));
    if let Some(hint) = hint {
        text.push_str(hint);
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
    timeout_ms: u64,
    ctx: &ToolCtx,
    protection: crate::permissions::Protection,
) -> Result<ForegroundOutput> {
    // Per-launch invariant: the one protection judgment came from the gate, and
    // an uncontained launch may not be the way around it.
    if protection == crate::permissions::Protection::Required && sandbox.is_none() {
        bail!(sandbox::PROTECTION_REQUIRED);
    }
    let spec = shell_spec(
        command,
        cwd,
        sandbox,
        bash,
        &ctx.cfg.shell_login_env,
        ctx.ui.as_ref(),
    );
    run_process_foreground(spec, timeout_ms, &ctx.cancel, "bash").await
}

pub(super) async fn run_process_foreground(
    mut spec: ProcessSpec,
    timeout_ms: u64,
    cancel: &CancellationToken,
    tool: &'static str,
) -> Result<ForegroundOutput> {
    // 审批与 spawn 之间也可能收到取消；此时进程还没出现，不能再 spawn 后再杀。
    if cancel.is_cancelled() {
        bail!("interrupted");
    }
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
    // The readers own only the read; the bytes land in shared buffers. A pipe
    // that outlives the process tree therefore still leaves everything that had
    // arrived available to the caller instead of being lost with the aborted
    // task.
    let stdout_capture = Arc::new(Mutex::new(BoundedStream::default()));
    let stderr_capture = Arc::new(Mutex::new(BoundedStream::default()));
    let stdout_task = tokio::spawn(read_bounded_stream(stdout, Arc::clone(&stdout_capture)));
    let stderr_task = tokio::spawn(read_bounded_stream(stderr, Arc::clone(&stderr_capture)));

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
            let pipe_stalled = finish_foreground_readers(stdout_task, stderr_task, tool).await?;
            Ok(assemble_output(
                status,
                None,
                &stdout_capture,
                &stderr_capture,
                pipe_stalled,
            ))
        }
        Completion::TimedOut => {
            let cleanup = child
                .terminate_and_wait(FOREGROUND_REAP_TIMEOUT)
                .await
                .with_context(|| format!("{tool}: failed to terminate timed-out process tree"));
            let drained = finish_foreground_readers(stdout_task, stderr_task, tool).await;
            let status = cleanup?;
            // What the command printed before it was killed is the only clue to
            // where it got stuck; the caller renders it next to the timeout.
            Ok(assemble_output(
                status,
                Some(timeout_ms),
                &stdout_capture,
                &stderr_capture,
                drained?,
            ))
        }
        Completion::Cancelled => {
            let cleanup = child
                .terminate_and_wait(FOREGROUND_REAP_TIMEOUT)
                .await
                .with_context(|| format!("{tool}: failed to terminate interrupted process tree"));
            let drained = finish_foreground_readers(stdout_task, stderr_task, tool).await;
            cleanup?;
            drained?;
            // Deliberately drops the partial output: an interrupted call's
            // contract with the dispatcher is exactly this text.
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
    /// The command was killed at its budget; `status` is the kill, not an exit.
    timed_out: Option<u64>,
    /// The tree was cleaned up but a pipe writer outside it kept the readers
    /// from reaching EOF inside the reap bound.
    pipe_stalled: bool,
}

impl ForegroundOutput {
    pub(super) fn timed_out(&self) -> bool {
        self.timed_out.is_some()
    }
}

#[derive(Default)]
struct BoundedStream {
    bytes: Vec<u8>,
    total_bytes: usize,
    /// The cap was reached. Trimming an incomplete character opens a couple of
    /// bytes of room again; without this flag the next chunks would refill them
    /// and the capture would no longer be the stream's prefix.
    capped: bool,
}

impl BoundedStream {
    fn push(&mut self, chunk: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(chunk.len());
        if self.capped {
            return;
        }
        let room = FOREGROUND_STREAM_CAP_BYTES.saturating_sub(self.bytes.len());
        if chunk.len() < room {
            self.bytes.extend_from_slice(chunk);
            return;
        }
        self.bytes.extend_from_slice(&chunk[..room]);
        // The cap can land inside a multi-byte character. Keeping that
        // partial sequence would decode as U+FFFD and, worse, absorb the
        // first bytes of the other stream when the two are merged.
        truncate_incomplete_utf8(&mut self.bytes);
        self.capped = true;
    }
}

fn take_capture(capture: &Arc<Mutex<BoundedStream>>) -> BoundedStream {
    std::mem::take(&mut *capture.lock().unwrap())
}

fn assemble_output(
    status: ProcessExit,
    timed_out: Option<u64>,
    stdout: &Arc<Mutex<BoundedStream>>,
    stderr: &Arc<Mutex<BoundedStream>>,
    pipe_stalled: bool,
) -> ForegroundOutput {
    let stdout = take_capture(stdout);
    let stderr = take_capture(stderr);
    let omitted_bytes = stdout
        .total_bytes
        .saturating_sub(stdout.bytes.len())
        .saturating_add(stderr.total_bytes.saturating_sub(stderr.bytes.len()));
    ForegroundOutput {
        status,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        omitted_bytes,
        timed_out,
        pipe_stalled,
    }
}

/// Join both readers under one short bound. False = both reached EOF; true =
/// the bound expired, the readers were aborted, and whatever they had captured
/// is all there is.
async fn finish_foreground_readers(
    stdout_task: tokio::task::JoinHandle<Result<()>>,
    stderr_task: tokio::task::JoinHandle<Result<()>>,
    tool: &'static str,
) -> Result<bool> {
    let (stdout, stderr) = tokio::join!(
        finish_reader(stdout_task, tool),
        finish_reader(stderr_task, tool)
    );
    merge_drains(stdout, stderr)
}

/// Both drains are always observed. `stdout? || stderr?` short-circuits: a
/// stalled stdout reports `true` and the stderr read error is never looked at.
fn merge_drains(stdout: Result<bool>, stderr: Result<bool>) -> Result<bool> {
    match (stdout, stderr) {
        (Ok(stdout), Ok(stderr)) => Ok(stdout || stderr),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

async fn finish_reader(
    mut task: tokio::task::JoinHandle<Result<()>>,
    tool: &'static str,
) -> Result<bool> {
    match tokio::time::timeout(FOREGROUND_REAP_TIMEOUT, &mut task).await {
        Ok(result) => {
            result
                .with_context(|| format!("{tool}: output reader task failed"))?
                .with_context(|| format!("{tool}: failed to drain output pipe"))?;
            Ok(false)
        }
        Err(_) => {
            task.abort();
            let _ = task.await;
            Ok(true)
        }
    }
}

async fn read_bounded_stream(
    mut reader: impl AsyncRead + Unpin,
    capture: Arc<Mutex<BoundedStream>>,
) -> Result<()> {
    let mut chunk = [0u8; 8192];
    loop {
        let count = reader.read(&mut chunk).await?;
        if count == 0 {
            return Ok(());
        }
        capture.lock().unwrap().push(&chunk[..count]);
    }
}

/// Cut a byte buffer whose end fell inside a multi-byte UTF-8 character.
/// A sequence is at most four bytes, so only the last three can hide a lead.
pub(crate) fn truncate_incomplete_utf8(bytes: &mut Vec<u8>) {
    let len = bytes.len();
    let start = len.saturating_sub(3);
    let Some(lead) = (start..len)
        .rev()
        .find(|&index| bytes[index] & 0xC0 != 0x80)
    else {
        return;
    };
    let width = match bytes[lead] {
        0x00..=0x7F => 1,
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        _ => 4,
    };
    if len - lead < width {
        bytes.truncate(lead);
    }
}

/// stdout+stderr merged, a trailing `[exit …]` when the run failed, and a
/// placeholder when empty — the model-facing text for one run (denial
/// annotation is the caller's job, so an escalated re-run reuses this).
///
/// 合并结果超出内联预算且给了 `spill_dir` 时，先把捕获到的全文写进去，截断说明里
/// 给出文件路径：否则大 diff 被截掉的后半段只能重跑命令才拿得到（plan 272 实测
/// 11 次审查里 6 次如此）。内联部分大小不变，路径不额外占上下文。
/// 捕获到的 stdout 在前、stderr 在后，两个流各自做 UTF-8 有损解码；内联截断之前的全文。
fn captured_text(output: &ForegroundOutput) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

/// Inline excerpt of an over-budget capture: stdout's head, then stderr's tail.
/// stdout filling the whole budget used to push a failure reason on stderr out
/// of the result entirely; the head of stdout is still what explains most
/// successful output, so it keeps the larger share.
fn inline_excerpt(stdout: &str, stderr: &str) -> String {
    let stdout_chars = stdout.chars().count();
    let stderr_chars = stderr.chars().count();
    let stderr_reserved = stderr_chars.min(FOREGROUND_STDERR_TAIL_CHARS);
    let stdout_keep = stdout_chars.min(FOREGROUND_OUTPUT_CAP_CHARS.saturating_sub(stderr_reserved));
    let stderr_keep = (FOREGROUND_OUTPUT_CAP_CHARS - stdout_keep).min(stderr_chars);
    let mut text = take_head(stdout, stdout_keep).to_string();
    if stderr_keep > 0 {
        if stdout_keep < stdout_chars {
            text.push_str("\n[... output omitted ...]\n");
        }
        text.push_str(take_tail(stderr, stderr_keep));
    }
    text
}

fn take_head(text: &str, chars: usize) -> &str {
    match text.char_indices().nth(chars) {
        Some((index, _)) => &text[..index],
        None => text,
    }
}

fn take_tail(text: &str, chars: usize) -> &str {
    let skip = text.chars().count().saturating_sub(chars);
    match text.char_indices().nth(skip) {
        Some((index, _)) => &text[index..],
        None => text,
    }
}

/// Render a finished run. A timeout is the caller's to turn into an error; this
/// only makes sure the text the model sees includes what the command printed
/// before it was killed, and says why the output may be short.
pub(super) fn format_output(output: &ForegroundOutput, spill_dir: Option<&Path>) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let captured_chars = stdout.chars().count() + stderr.chars().count();
    let omitted_chars = captured_chars.saturating_sub(FOREGROUND_OUTPUT_CAP_CHARS);
    let saved = match spill_dir {
        Some(dir) if omitted_chars > 0 => {
            let full = format!("{stdout}{stderr}");
            Some(
                crate::history::write_offload_file(dir, &full)
                    .map(|path| (path, full.lines().count())),
            )
        }
        _ => None,
    };
    let mut text = if omitted_chars > 0 {
        inline_excerpt(&stdout, &stderr)
    } else {
        format!("{stdout}{stderr}")
    };
    if omitted_chars > 0 || output.omitted_bytes > 0 {
        let omitted_bytes = output.omitted_bytes;
        let saved = match saved {
            Some(Ok((path, lines))) => format!(
                "; all {captured_chars} captured characters ({lines} lines) are saved to {} \
                 — read the rest from that file instead of re-running the command",
                path.display()
            ),
            Some(Err(error)) => format!("; saving the captured output failed ({error})"),
            None => String::new(),
        };
        text.push_str(&format!(
            "\n[output truncated: {omitted_chars} characters and {omitted_bytes} additional \
             bytes omitted{saved}]"
        ));
    }
    if output.pipe_stalled {
        text.push_str(
            "\n[the output pipe stayed open after the command exited: a descendant that left \
             the process group still holds it, so the output above is what had arrived]",
        );
    }
    if let Some(timeout_ms) = output.timed_out {
        text.push_str(&format!("\n[timed out after {timeout_ms}ms]"));
    } else if !output.status.success() {
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

/// A timed-out run is still a failed tool call — the caller reports the
/// rendered text as the error, so the model sees both why it stopped and what
/// it printed first.
pub(super) fn fail_if_timed_out(output: &ForegroundOutput, spill_dir: Option<&Path>) -> Result<()> {
    if output.timed_out() {
        bail!("{}", format_output(output, spill_dir));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::FOREGROUND_OUTPUT_CAP_CHARS;
    use super::testutil::*;
    #[cfg(unix)]
    use crate::event::Event;
    use crate::shell_env::ShellLoginEnv;
    #[cfg(unix)]
    use crate::shell_programs::ShellFlavor;
    use crate::tools::testutil::*;
    use serde_json::json;
    #[cfg(unix)]
    use std::path::Path;
    #[cfg(unix)]
    use std::sync::Arc;

    /// Plan 266: the executor's sandbox is the gate's plan, read verbatim. A
    /// `Contained` verdict always means the workspace policy; every other plan
    /// means no containment.
    #[test]
    fn plan266_execution_follows_the_verdicts_containment() {
        use crate::config::EffectiveWorkspace;
        use crate::permissions::Permissions;
        use crate::permissions::SandboxPlan;
        use crate::permissions::Verdict;
        use crate::sandbox::SandboxPolicy;

        let policy = std::sync::Arc::new(SandboxPolicy::workspace(
            std::path::Path::new("/w"),
            &[],
            false,
        ));
        let workspace = EffectiveWorkspace {
            identity: crate::project::WorkspaceIdentity::ephemeral("/w".into()),
            workspace_epoch: 0,
            cwd: "/w".into(),
            permissions: std::sync::Arc::new(Permissions::allow_all()),
            file_state: Default::default(),
            sandbox: Some(policy.clone()),
            system: String::new(),
            branch: None,
        };
        let plan = |sandbox| Verdict {
            sandbox,
            ..Default::default()
        };
        let contained = super::sandbox_for(&plan(SandboxPlan::Contained), &workspace)
            .expect("contained means the workspace policy");
        assert!(std::sync::Arc::ptr_eq(&contained, &policy));
        assert!(super::sandbox_for(&plan(SandboxPlan::None), &workspace).is_none());
        assert!(super::sandbox_for(&plan(SandboxPlan::Escalated), &workspace).is_none());

        let no_policy = EffectiveWorkspace {
            sandbox: None,
            ..workspace
        };
        assert!(super::sandbox_for(&plan(SandboxPlan::Contained), &no_policy).is_none());
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
            &RecordingUi::default(),
        );
        assert!(spec.windows_debug_descendants());
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

    /// 取消发生在审批与 spawn 之间时，进程树根本不能出现。故意用一个不存在的
    /// 可执行文件：旧代码会先尝试 spawn 并报 “failed to spawn shell”，新代码
    /// 必须在 spawn 前返回 interrupted，结果不依赖子进程调度时机。
    #[tokio::test]
    async fn cancelled_foreground_process_never_spawns() {
        let spec = super::ProcessSpec::new(
            std::path::PathBuf::from("/nonexistent/kloop-cancel-spawn-probe"),
            std::env::current_dir().unwrap(),
        );
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        let error = super::run_process_foreground(spec, 10_000, &cancel, "bash")
            .await
            .expect_err("a cancelled call must not spawn")
            .to_string();
        assert!(error.contains("interrupted"), "{error}");
        assert!(!error.contains("failed to spawn shell"), "{error}");
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
            let spec = super::shell_spec(
                "true",
                &cwd,
                Some(&policy),
                &shell,
                &ShellLoginEnv::none(),
                &RecordingUi::default(),
            );
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

    /// A captured login environment is replayed by the shell itself. `/etc/
    /// profile`'s `path_helper` would otherwise put `/usr/bin` back in front of
    /// the captured `PATH`, and zsh reads `/etc/zshenv` no matter how it is
    /// invoked; `-f` covers the rest. Without a capture the shell stays login
    /// `-lc`.
    #[test]
    #[cfg(unix)]
    fn an_active_login_environment_runs_the_shell_non_login() {
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .expect("test shell is available");
        let cwd = std::env::current_dir().unwrap();
        let login = ShellLoginEnv::test_fixture(&[("PATH", "/login/bin")]);
        let prelude = std::ffi::OsString::from(super::REPLAY_PRELUDE);

        let active = super::shell_spec("true", &cwd, None, &shell, &login, &RecordingUi::default());
        assert_eq!(
            active.args,
            vec![
                std::ffi::OsString::from("-c"),
                prelude.clone(),
                shell.executable.clone().into_os_string(),
                std::ffi::OsString::from("true"),
                login
                    .replay_path_for_use()
                    .unwrap()
                    .unwrap()
                    .into_os_string(),
            ],
            "the command is an argument, so nothing is quoted into the script"
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
            super::shell_spec("true", &cwd, None, &zsh, &login, &RecordingUi::default()).args[0],
            std::ffi::OsString::from("-f"),
            "zsh suppresses the startup files it would otherwise read"
        );

        let inactive = super::shell_spec(
            "true",
            &cwd,
            None,
            &shell,
            &ShellLoginEnv::none(),
            &RecordingUi::default(),
        );
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

    /// The replay runs *inside* the shell, after whatever startup file it could
    /// not be stopped from reading: what the capture dropped is gone even though
    /// the shell inherited it, and what it kept is there.
    #[tokio::test]
    #[cfg(unix)]
    async fn the_replay_prelude_beats_the_inherited_environment() {
        let (ctx, _ui) = recording_ctx("bash-replay-prelude");
        let shell = ctx
            .cfg
            .shell_programs
            .bash
            .clone()
            .expect("test shell is available");
        let login = ShellLoginEnv::test_fixture_with_removals(
            &[("KLOOP_TEST_SENTINEL", "from-login")],
            &["PATH"],
        );
        let mut spec = super::shell_spec(
            "printf '%s|%s' \"${KLOOP_TEST_SENTINEL-unset}\" \"${PATH+set}\"",
            &ctx.cfg.cwd,
            None,
            &shell,
            &login,
            &RecordingUi::default(),
        );
        // Leave the inherited PATH for the prelude to remove: the process spec
        // alone would have hidden it.
        spec.env_remove.retain(|name| name != "PATH");

        let output = super::run_process_foreground(
            spec,
            10_000,
            &tokio_util::sync::CancellationToken::new(),
            "bash",
        )
        .await
        .unwrap();
        let text = super::format_output(&output, None);
        assert!(text.contains("from-login|"), "{text}");
        assert!(
            !text.contains("from-login|set"),
            "the prelude, not the inherited environment, has the last word: {text}"
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn replay_cannot_replace_the_command_or_restore_startup_hooks() {
        if !Path::new("/bin/bash").is_file() {
            return;
        }
        for (executable, flavor) in [
            ("/bin/sh", ShellFlavor::PosixSh),
            ("/bin/zsh", ShellFlavor::Zsh),
        ] {
            if !Path::new(executable).is_file() {
                continue;
            }
            let shell = crate::shell_programs::ShellProgram {
                executable: executable.into(),
                flavor,
            };
            for login in [
                ShellLoginEnv::test_fixture(&[
                    ("__kloop_cmd", "printf replaced"),
                    ("BASH_ENV", "/dev/null"),
                    ("ENV", "/dev/null"),
                ]),
                ShellLoginEnv::test_fixture_with_removals(
                    &[("BASH_ENV", "/dev/null"), ("ENV", "/dev/null")],
                    &["__kloop_cmd"],
                ),
            ] {
                let spec = super::shell_spec(
                    "printf 'checked:%s|%s|' \"$#\" \"${1-unset}\"; /bin/bash -c 'printf \"%s|%s\" \"${BASH_ENV+set}\" \"${ENV+set}\"'",
                    &std::env::current_dir().unwrap(),
                    None,
                    &shell,
                    &login,
                    &RecordingUi::default(),
                );
                let output = super::run_process_foreground(
                    spec,
                    10_000,
                    &tokio_util::sync::CancellationToken::new(),
                    "bash",
                )
                .await
                .unwrap();
                assert!(output.status.success(), "{output:?}");
                assert_eq!(output.stdout, b"checked:0|unset||", "{output:?}");
            }
        }
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn replay_preserves_inherited_bash_options_without_assigning_readonly_variables() {
        if !Path::new("/bin/bash").is_file() {
            return;
        }
        let shell = crate::shell_programs::ShellProgram {
            executable: "/bin/bash".into(),
            flavor: ShellFlavor::PosixSh,
        };
        let login = ShellLoginEnv::test_fixture(&[
            ("SHELLOPTS", "errexit:nounset:pipefail"),
            ("BASHOPTS", "checkwinsize:extquote"),
            ("UID", "1"),
            ("EUID", "1"),
            ("PPID", "1"),
        ]);
        let spec = super::shell_spec(
            "case $- in *e*) printf errexit;; esac; printf '|ok'",
            &std::env::current_dir().unwrap(),
            None,
            &shell,
            &login,
            &RecordingUi::default(),
        );
        let output = super::run_process_foreground(
            spec,
            10_000,
            &tokio_util::sync::CancellationToken::new(),
            "bash",
        )
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"errexit|ok", "{output:?}");
        assert!(output.stderr.is_empty(), "{output:?}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_large_capture_does_not_duplicate_the_environment_in_exec_arguments() {
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .unwrap();
        let value = "x".repeat(50_000);
        let login = ShellLoginEnv::test_fixture(&[
            ("KLOOP_LARGE_A", &value),
            ("KLOOP_LARGE_B", &value),
            ("KLOOP_LARGE_C", &value),
        ]);
        let spec = super::shell_spec(
            "printf '%s|%s|%s' \"${#KLOOP_LARGE_A}\" \"${#KLOOP_LARGE_B}\" \"${#KLOOP_LARGE_C}\"",
            &std::env::current_dir().unwrap(),
            None,
            &shell,
            &login,
            &RecordingUi::default(),
        );
        let output = super::run_process_foreground(
            spec,
            10_000,
            &tokio_util::sync::CancellationToken::new(),
            "bash",
        )
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(output.stdout, b"50000|50000|50000", "{output:?}");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn replay_restores_config_values_after_shell_startup() {
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .unwrap();
        let login = ShellLoginEnv::test_fixture(&[
            ("PATH", "/configured/bin"),
            ("HTTP_PROXY", "http://configured"),
        ]);
        let mut spec = super::shell_spec(
            "printf '%s|%s' \"$PATH\" \"$HTTP_PROXY\"",
            &std::env::current_dir().unwrap(),
            None,
            &shell,
            &login,
            &RecordingUi::default(),
        );
        spec.args[1] = format!(
            "PATH=/startup/bin; HTTP_PROXY=http://startup; {}",
            super::REPLAY_PRELUDE
        )
        .into();
        let output = super::run_process_foreground(
            spec,
            10_000,
            &tokio_util::sync::CancellationToken::new(),
            "bash",
        )
        .await
        .unwrap();
        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            output.stdout, b"/configured/bin|http://configured",
            "{output:?}"
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
    #[cfg(unix)]
    async fn write_rules_use_the_home_replayed_to_the_actual_shell() {
        use crate::permissions::{Mode, PermissionRules, Permissions};

        let root = std::env::temp_dir().join(crate::resource_id::fresh("kloop-word-env-").unwrap());
        std::fs::create_dir_all(&root).unwrap();
        let home = root.to_str().unwrap();
        let wrapped_home = root.join("wrapped");
        std::fs::create_dir_all(&wrapped_home).unwrap();
        let ctx = test_ctx_with_cfg(
            0,
            TestConfig::new("word-env")
                .login_env(ShellLoginEnv::test_fixture(&[("HOME", home)]))
                .build(),
        );
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(
            Permissions::new(
                Mode::Bypass,
                &PermissionRules {
                    allow: Vec::new(),
                    deny: vec![
                        format!("bash_write({home}/locked.txt)"),
                        format!("bash_write({}/locked.txt)", wrapped_home.display()),
                    ],
                    ask: Vec::new(),
                },
                root.clone(),
                None,
            )
            .unwrap(),
        );
        let ctx = crate::tools::ToolCtx {
            cfg: Arc::new(cfg),
            ..ctx
        };
        let (out, is_error) =
            run_tool("bash", bash_input("printf x > \"$HOME/locked.txt\""), &ctx).await;
        assert!(is_error && out.contains("deny permission rule"), "{out}");
        assert!(!root.join("locked.txt").exists());

        for options in ["", "-u HOME "] {
            let command = format!(
                "/usr/bin/env {options}HOME={} /bin/bash -c 'printf x > \"$HOME/locked.txt\"'",
                wrapped_home.display()
            );
            let (out, is_error) = run_tool("bash", bash_input(&command), &ctx).await;
            assert!(
                is_error && out.contains("deny permission rule"),
                "{command}: {out}"
            );
            assert!(!wrapped_home.join("locked.txt").exists());
        }

        let (out, is_error) =
            run_tool("bash", bash_input("printf x > \"$HOME/other.txt\""), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(std::fs::read(root.join("other.txt")).unwrap(), b"x");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn unavailable_login_home_stays_unknown() {
        assert_eq!(super::replayed_home(&ShellLoginEnv::none()), None);
        let login = ShellLoginEnv::test_fixture(&[("PATH", "/bin")]);
        assert_eq!(super::replayed_home(&login), None);
        let login = ShellLoginEnv::test_fixture(&[("HOME", "/tmp/home"), ("IFS", "/")]);
        assert_eq!(super::replayed_home(&login), None);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn clearing_replay_files_does_not_break_the_next_bash_call() {
        let directory =
            std::env::temp_dir().join(crate::resource_id::fresh("kloop-replay-cleanup-").unwrap());
        let login = ShellLoginEnv::test_fixture_in(
            &[
                ("KLOOP_TEST_SENTINEL", "from-login"),
                ("KLOOP_REPLAY_DIR", directory.to_str().unwrap()),
            ],
            &[],
            &directory,
        );
        let original = login.replay_path_for_use().unwrap().unwrap();
        let mut ctx = test_ctx_with_cfg(
            0,
            TestConfig::new("bash-replay-cleanup")
                .login_env(login)
                .build(),
        );
        let ui = Arc::new(RecordingUi::default());
        ctx.ui = ui.clone();
        for command in [
            "rm -f -- \"$KLOOP_REPLAY_DIR\"/*.sh; printf %s \"$KLOOP_TEST_SENTINEL\"",
            "printf %s \"$KLOOP_TEST_SENTINEL\"",
        ] {
            let (out, is_error) = run_tool("bash", bash_input(command), &ctx).await;
            assert!(!is_error, "{out}");
            assert!(out.contains("from-login"), "{out}");
            assert!(!original.exists());
        }
        assert!(
            !ui.events
                .lock()
                .unwrap()
                .iter()
                .any(|event| matches!(event, Event::Note(_)))
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        drop(ctx);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn replay_write_failure_uses_a_login_shell_and_retries_later() {
        let directory =
            std::env::temp_dir().join(crate::resource_id::fresh("kloop-replay-retry-").unwrap());
        let login = ShellLoginEnv::test_fixture_in(
            &[("KLOOP_TEST_SENTINEL", "from-login")],
            &[],
            &directory,
        );
        std::fs::remove_dir_all(&directory).unwrap();
        std::fs::write(&directory, b"not a directory").unwrap();
        let shell = crate::shell_programs::ShellPrograms::test_fixture()
            .bash
            .unwrap();
        let command = "printf '%s|%s' \"$KLOOP_TEST_SENTINEL\" \"$#\"";
        for expected_flag in ["-lc", "-c"] {
            let spec = super::shell_spec(
                command,
                &std::env::current_dir().unwrap(),
                None,
                &shell,
                &login,
                &RecordingUi::default(),
            );
            assert_eq!(spec.args[0], std::ffi::OsString::from(expected_flag));
            let output = super::run_process_foreground(
                spec,
                10_000,
                &tokio_util::sync::CancellationToken::new(),
                "bash",
            )
            .await
            .unwrap();
            assert!(output.status.success(), "{output:?}");
            assert_eq!(output.stdout, b"from-login|0", "{output:?}");
            if expected_flag == "-lc" {
                std::fs::remove_file(&directory).unwrap();
            }
        }
        drop(login);
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0);
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn replay_write_failures_warn_for_foreground_and_background_commands() {
        let directory =
            std::env::temp_dir().join(crate::resource_id::fresh("kloop-replay-warnings-").unwrap());
        let login = ShellLoginEnv::test_fixture_in(
            &[("KLOOP_TEST_SENTINEL", "from-login")],
            &[],
            &directory,
        );
        std::fs::remove_dir_all(&directory).unwrap();
        std::fs::write(&directory, b"not a directory").unwrap();
        let error = login.replay_path_for_use().unwrap_err();
        let warning = format!(
            "warning: could not prepare the login environment replay file ({error}); \
             this command will use a login shell (-lc); later commands will retry"
        );
        let mut ctx = test_ctx_with_cfg(
            0,
            TestConfig::new("bash-replay-warnings")
                .login_env(login)
                .build(),
        );
        let ui = Arc::new(RecordingUi::default());
        ctx.ui = ui.clone();
        for blocked in [true, false] {
            if !blocked {
                std::fs::remove_file(&directory).unwrap();
            }
            for background in [false, true] {
                let (mut out, is_error) = run_tool(
                    "bash",
                    json!({
                        "command": "printf %s \"$KLOOP_TEST_SENTINEL\"",
                        "background": background,
                    }),
                    &ctx,
                )
                .await;
                assert!(!is_error, "{out}");
                if background {
                    let id = bg_id(&out);
                    let (completed, is_error) =
                        run_tool("bash_output", json!({"bash_id": id}), &ctx).await;
                    assert!(!is_error, "{completed}");
                    out = completed;
                }
                assert!(out.contains("from-login"), "{out}");
            }
            let notes: Vec<_> = ui
                .events
                .lock()
                .unwrap()
                .iter()
                .filter(|event| matches!(event, Event::Note(_)))
                .cloned()
                .collect();
            assert_eq!(
                notes,
                vec![Event::Note(warning.clone()), Event::Note(warning.clone())]
            );
        }
        drop(ctx);
        std::fs::remove_dir_all(directory).unwrap();
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
        let ctx = test_ctx(0, "git-bash-unicode");
        let output = super::run_foreground(
            "printf '你好' > marker.txt; printf stdout; printf stderr >&2",
            &cwd,
            None,
            &bash,
            10_000,
            &ctx,
            crate::permissions::Protection::Unchecked,
        )
        .await
        .unwrap();
        let text = super::format_output(&output, None);
        assert!(text.contains("stdout"), "{text}");
        assert!(text.contains("stderr"), "{text}");
        assert_eq!(
            std::fs::read_to_string(cwd.join("marker.txt")).unwrap(),
            "你好"
        );
        let _ = std::fs::remove_dir_all(cwd);
    }

    /// A zero exit can hide the denied command only when the last one is
    /// read-only and something before it had an effect (plan 283 review).
    #[test]
    fn a_read_only_tail_can_mask_the_status_of_the_work_before_it() {
        for (command, masked) in [
            ("make test 2>&1 | tail -20", true),
            ("cd /x && make check 2>&1 | tail -80", true),
            ("make install; echo done", true),
            ("cargo test | grep -E 'test result'", true),
            ("echo hi > /elsewhere/out.txt; echo done", true),
            // A `-c` wrapper's status is its script's: the analysis lists the
            // script's commands first and the wrapper last (plan 283 review).
            ("sh -c 'touch /outside/x 2>&1 | tail -5'", true),
            ("bash -c 'make test | tail -3'; echo done", true),
            ("sh -c 'make test'", false),
            ("sh -c 'echo a | tail -1'", false),
            ("make test", false),
            ("cat build.log | tail -5", false),
            ("make test | tee test.log", false),
            ("ls && pwd", false),
        ] {
            assert_eq!(
                super::status_may_be_masked(command, None),
                masked,
                "{command}"
            );
        }
    }

    /// Plan 283 removed `disable_sandbox`: it is an unknown field like any
    /// other, on every platform and whatever its value.
    #[tokio::test]
    async fn disable_sandbox_is_an_unknown_field() {
        let ctx = test_ctx(0, "removed-disable-sandbox");
        for value in [true, false] {
            let (output, is_error) = run_tool(
                "bash",
                json!({"command": "true", "disable_sandbox": value}),
                &ctx,
            )
            .await;
            assert!(is_error, "{output}");
            assert!(
                output.contains("unknown field `disable_sandbox`"),
                "{output}"
            );
        }
        let definition = crate::tools::tool_defs(ctx.depth, &ctx.cfg.shell_programs)
            .into_iter()
            .find(|definition| definition.name == "bash")
            .unwrap();
        assert_eq!(definition.schema["properties"].get("disable_sandbox"), None);
        #[cfg(not(windows))]
        assert!(
            definition
                .description
                .contains("You cannot turn the sandbox off"),
            "{}",
            definition.description
        );
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

    /// What the command printed before the timeout is the only clue to where it
    /// got stuck; the error keeps it instead of discarding the capture.
    #[tokio::test]
    async fn foreground_timeout_keeps_what_the_command_printed() {
        let ctx = test_ctx(0, "bash-timeout-output");
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "echo stuck-here; sleep 30", "timeout_ms": 500}),
            &ctx,
        )
        .await;
        assert!(is_error, "{out}");
        assert!(out.contains("stuck-here"), "{out}");
        assert!(out.contains("timed out after 500ms"), "{out}");
    }

    #[tokio::test]
    async fn foreground_timeout_bounds_are_rejected_before_spawning() {
        let ctx = test_ctx(0, "bash-timeout-bounds");
        for value in [0, 600_001] {
            let (out, is_error) = run_tool(
                "bash",
                json!({"command": "echo spawned-anyway", "timeout_ms": value}),
                &ctx,
            )
            .await;
            assert!(is_error, "{out}");
            assert!(
                out.contains("timeout_ms must be between 1 and 600000"),
                "{out}"
            );
            assert!(!out.contains("spawned-anyway"), "{out}");
        }
        // Background execution ignores the field, so an out-of-range value is
        // not a reason to refuse a call that can never use it.
        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "true", "background": true, "timeout_ms": 0}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
    }

    /// A descendant that leaves the process group keeps its inherited stdio
    /// after the tree is reaped. That must not turn a successful command into
    /// an error: the already-read output and the exit status are the result.
    #[tokio::test]
    #[cfg(unix)]
    async fn a_detached_descendant_holding_the_pipe_keeps_the_result() {
        let ctx = test_ctx(0, "bash-setsid-pipe");
        // macOS ships no `setsid` command; python3 is the portable way to put
        // the descendant in a session of its own while it keeps our stdout.
        let command =
            "python3 -c 'import os, time; os.setsid(); time.sleep(5)' & sleep 0.2; echo ok; exit 3";
        let started = std::time::Instant::now();
        let (out, is_error) = run_tool("bash", bash_input(command), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains("ok"), "{out}");
        assert!(out.contains("output pipe stayed open"), "{out}");
        assert!(out.contains("[exit status 3]"), "{out}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(4),
            "waited for the detached descendant to close the pipe"
        );
    }

    /// A failure reason on stderr must survive a stdout that alone fills the
    /// inline budget — the merged result used to keep only the head.
    #[tokio::test]
    async fn stderr_failure_reason_survives_a_budget_filling_stdout() {
        let ctx = test_ctx(0, "bash-stderr-tail");
        let (out, is_error) = run_tool(
            "bash",
            bash_input("yes A | head -c 200000; echo 'BUILD FAILED: reason' >&2; exit 1"),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("BUILD FAILED: reason"), "{out}");
        assert!(out.contains("output truncated"), "{out}");
        assert!(out.contains("[exit status 1]"), "{out}");
    }

    #[test]
    fn a_cut_inside_a_character_is_trimmed_to_a_boundary() {
        let mut three_byte = "中".as_bytes().to_vec();
        three_byte.truncate(2);
        super::truncate_incomplete_utf8(&mut three_byte);
        assert!(three_byte.is_empty(), "{three_byte:?}");

        let mut four_byte = "😀".as_bytes().to_vec();
        let keep = four_byte.len() - 2;
        four_byte.truncate(keep);
        super::truncate_incomplete_utf8(&mut four_byte);
        assert!(four_byte.is_empty(), "{four_byte:?}");

        let mut complete = "ok中".as_bytes().to_vec();
        super::truncate_incomplete_utf8(&mut complete);
        assert_eq!(complete, "ok中".as_bytes());
    }

    /// The 1 MiB per-stream cap can land inside a character. If it does, the
    /// stream must end on a boundary: otherwise the spilled file's stdout tail
    /// decodes as U+FFFD, and the first bytes of stderr get absorbed into it.
    /// Trimming the incomplete character at the cap frees a byte or two. Later
    /// chunks must not refill that room: the capture is a prefix, and splicing
    /// bytes from beyond the cap onto it would end it somewhere it never was.
    #[test]
    fn a_capped_stream_never_resumes_filling() {
        let cap = super::FOREGROUND_STREAM_CAP_BYTES;
        let mut stream = super::BoundedStream::default();
        stream.push(&vec![b'a'; cap - 1]);
        // Two bytes of 中, so the cap lands inside the character.
        stream.push(&"中".as_bytes()[..2]);
        assert_eq!(stream.bytes.len(), cap - 1);
        stream.push(b"STRAY");
        assert_eq!(stream.bytes, vec![b'a'; cap - 1]);
        assert_eq!(stream.total_bytes, cap - 1 + 2 + 5);
    }

    #[test]
    fn a_stalled_stream_does_not_hide_the_other_streams_read_error() {
        fn failed() -> anyhow::Result<bool> {
            Err(anyhow::anyhow!("reader failed"))
        }
        assert!(super::merge_drains(Ok(true), failed()).is_err());
        assert!(super::merge_drains(failed(), Ok(true)).is_err());
        assert!(super::merge_drains(Ok(false), Ok(true)).unwrap());
        assert!(!super::merge_drains(Ok(false), Ok(false)).unwrap());
    }

    #[tokio::test]
    async fn a_capped_stream_ends_on_a_character_boundary() {
        let ctx = test_ctx(0, "bash-utf8-seam");
        // 1 MiB lands two bytes into a 3-byte character after the `xy` prefix.
        let (out, is_error) = run_tool(
            "bash",
            bash_input("printf xy; yes 中 | head -c 1200000; printf 'ERROR-TAIL' >&2; exit 1"),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("ERROR-TAIL"), "{out}");
        let notice = out.rsplit_once("\n[output truncated: ").expect("notice").1;
        let path = notice
            .split_once(" are saved to ")
            .and_then(|(_, rest)| rest.split_once(" — "))
            .map(|(path, _)| path)
            .expect("saved path");
        let spilled = std::fs::read_to_string(path).unwrap();
        assert!(
            spilled.ends_with("ERROR-TAIL"),
            "{:?}",
            &spilled[spilled.len() - 40..]
        );
        assert!(!spilled.contains('\u{FFFD}'), "seam decoded as U+FFFD");
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
        // 内联预算不变，只多一行路径。
        assert!(
            out.chars().count() <= FOREGROUND_OUTPUT_CAP_CHARS + 400,
            "bounded output grew to {} characters",
            out.chars().count()
        );
        // 被截掉的内容都在说明里的文件中（stdout 在前、stderr 在后），模型可以接着读，
        // 不必重跑命令。
        let notice = out.rsplit_once("\n[output truncated: ").expect("notice").1;
        assert!(
            notice.starts_with("370000 characters and 0 additional bytes omitted; all 400000 captured characters (200000 lines) are saved to "),
            "{notice}"
        );
        let path = notice
            .split_once(" are saved to ")
            .and_then(|(_, rest)| rest.split_once(" — "))
            .map(|(path, _)| path)
            .expect("saved path");
        assert!(
            path.starts_with(&ctx.cfg.offload_dir.display().to_string()),
            "{path}"
        );
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            format!("{}{}", "A\n".repeat(100_000), "B\n".repeat(100_000))
        );
    }

    #[tokio::test]
    async fn output_within_the_inline_budget_saves_nothing() {
        let ctx = test_ctx(0, "unspilled-output");
        let (out, is_error) = run_tool("bash", bash_input("printf short"), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "short");
        let saved = std::fs::read_dir(&ctx.cfg.offload_dir)
            .map(|entries| entries.count())
            .unwrap_or(0);
        assert_eq!(saved, 0, "a result that fits must not write a file");
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

    /// Real seatbelt integration: these run the actual /usr/bin/sandbox-exec,
    /// so they are macOS-only; Linux CI covers the sandbox-off path (every
    /// other test in this file) and the pure profile tests in sandbox.rs.
    #[cfg(target_os = "macos")]
    mod seatbelt {
        include!("bash/protection_tests.rs");
        use super::*;
        use crate::sandbox::SandboxPolicy;
        use crate::sandbox::WritableRoot;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;

        use super::super::testutil::seatbelt::*;

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
                credential_home: None,
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

        #[tokio::test]
        async fn denied_read_path_cannot_be_read_through_a_symlink() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, root) = sandbox_ctx("inout");
            let inside = root.join("ok.txt");
            // The root is a fixed temp path; a file left by an earlier run would
            // let the read below pass without this run's write.
            let _ = std::fs::remove_file(&inside);
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
                out.ends_with(crate::sandbox::DENIAL_HINT),
                "nobody to ask: the hint points at the user, not a retry: {out}"
            );
            assert!(!out.contains("disable_sandbox"), "{out}");
            assert!(!blocked.exists());
        }

        #[tokio::test]
        async fn private_state_stays_unwritable_when_it_is_the_workspace_root() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, root) = sandbox_ctx("rosub");
            let (out, _) = run_tool(
                "bash",
                bash_input(&format!("mkdir -p {}", root.join(".kloop").display())),
                &ctx,
            )
            .await;
            assert!(out.contains("Operation not permitted"), "{out}");
        }

        /// The denied network is a boundary around the machine, not around the
        /// socket API: a command must still reach a listener of its own (plan
        /// 170 — Go's httptest is this shape, and denying it is what drove a
        /// whole session out of the sandbox), while anything off the host stays
        /// refused.
        #[tokio::test]
        async fn loopback_stays_reachable_while_the_machine_boundary_holds() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
            assert!(out.ends_with(crate::sandbox::DENIAL_HINT), "{out}");
        }

        /// Escalation loop, approved: a contained write outside the writable
        /// root is denied by the sandbox, the loop asks once, and on approval
        /// re-runs the command unsandboxed — the write lands and the result
        /// is flagged as escalated, with no denial hint left dangling.
        #[tokio::test]
        async fn escalation_reruns_unsandboxed_on_approval() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
            assert!(
                out.contains("Re-ran without the sandbox after user approval"),
                "{out}"
            );
            assert!(
                !out.contains("looks like a sandbox"),
                "no dangling hint: {out}"
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "climbed\n");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "asked exactly once: {out}");
        }

        /// A pipe ending in a read-only command exits 0 even when the command
        /// before it was denied (`make test 2>&1 | tail -20`). With no way out
        /// of the sandbox but the escalation, that zero must not end the run
        /// there (plan 283 review).
        #[tokio::test]
        async fn a_denial_masked_by_a_read_only_pipe_tail_still_escalates() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, asked) = escalating_ctx(
                "esc-pipe",
                crate::permissions::Decision::Allow(crate::permissions::ApprovalScope::Once),
            );
            let target = outside_dir("esc-pipe").join("piped.txt");
            let _ = std::fs::remove_file(&target);
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("touch {} 2>&1 | tail -5", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert!(
                out.contains("Re-ran without the sandbox after user approval"),
                "{out}"
            );
            assert!(target.exists(), "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{out}");

            // A read-only pipeline has nothing the sandbox could have stopped.
            let (out, _) = run_tool(
                "bash",
                bash_input("echo 'touch: x: Operation not permitted' | tail -1"),
                &ctx,
            )
            .await;
            assert!(!out.contains("Re-ran without the sandbox"), "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{out}");

            // A masked zero needs the OS's own refusal text: a passing run whose
            // output merely names the sandbox (test names do) is not a denial.
            let (out, _) = run_tool(
                "bash",
                bash_input(&format!(
                    "touch {} && echo 'test tools::sandbox::denial_hint ... ok' | tail -1",
                    std::fs::canonicalize(std::env::temp_dir().join("kloop-sbx-esc-pipe"))
                        .unwrap()
                        .join("inside.txt")
                        .display()
                )),
                &ctx,
            )
            .await;
            assert!(!out.contains("Re-ran without the sandbox"), "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{out}");

            // Nor does a resolver function's name, behind a `-c` wrapper too.
            let (out, _) = run_tool(
                "bash",
                bash_input(&format!(
                    "sh -c \"touch {} && echo 'test net::getaddrinfo_fallback ... ok' | tail -1\"",
                    std::fs::canonicalize(std::env::temp_dir().join("kloop-sbx-esc-pipe"))
                        .unwrap()
                        .join("inside-wrapped.txt")
                        .display()
                )),
                &ctx,
            )
            .await;
            assert!(!out.contains("Re-ran without the sandbox"), "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{out}");
            let _ = std::fs::remove_file(target);
        }

        /// 升级获批后返回的是重跑的结果：沙箱内那次的超长输出不能先落盘，否则就是
        /// 无人引用的孤儿文件。这里重跑没有大输出，offload 目录应当保持为空。
        #[tokio::test]
        async fn approved_escalation_leaves_no_spill_of_the_sandboxed_run() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, asked) = escalating_ctx(
                "esc-no-orphan",
                crate::permissions::Decision::Allow(crate::permissions::ApprovalScope::Once),
            );
            let target = outside_dir("esc-no-orphan").join("climbed.txt");
            let _ = std::fs::remove_file(&target);
            let _ = std::fs::remove_dir_all(&ctx.cfg.offload_dir);
            let command = format!(
                "echo climbed > {} || {{ yes B | head -c 40000 >&2; exit 1; }}",
                target.display()
            );
            let (out, is_error) = run_tool("bash", bash_input(&command), &ctx).await;
            assert!(!is_error, "{out}");
            assert_eq!(asked.load(Ordering::SeqCst), 1, "{out}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "climbed\n");
            let spilled = std::fs::read_dir(&ctx.cfg.offload_dir)
                .map(|entries| entries.count())
                .unwrap_or(0);
            assert_eq!(
                spilled, 0,
                "the sandboxed run's output must not be saved: {out}"
            );
        }

        /// 拒绝行在 stderr，而 stdout 已超过内联上限：判定必须看完整捕获的输出，
        /// 否则截断后的文本里没有拒绝行，既不问升级也不给沙箱提示。
        #[tokio::test]
        async fn denial_after_long_stdout_is_still_recognized() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, asked) = escalating_ctx(
                "esc-long-stdout",
                crate::permissions::Decision::Allow(crate::permissions::ApprovalScope::Once),
            );
            let target = outside_dir("esc-long-stdout").join("climbed.txt");
            let _ = std::fs::remove_file(&target);
            let command = format!("yes A | head -c 40000; echo climbed > {}", target.display());
            let (out, is_error) = run_tool("bash", bash_input(&command), &ctx).await;
            assert!(!is_error, "{out}");
            assert_eq!(
                asked.load(Ordering::SeqCst),
                1,
                "denial was not recognized: {out}"
            );
            assert!(
                out.contains("Re-ran without the sandbox after user approval"),
                "{out}"
            );
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "climbed\n");
        }

        #[tokio::test]
        async fn escalation_in_bypass_attributes_the_retry_without_an_approver() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (mut ctx, _) = sandbox_ctx("esc-bypass");
            let mut cfg = ctx.cfg.test_clone();
            cfg.permissions = Arc::new(
                crate::permissions::Permissions::new(
                    crate::permissions::Mode::Bypass,
                    &Default::default(),
                    cfg.cwd.clone(),
                    None,
                )
                .unwrap(),
            );
            ctx.cfg = Arc::new(cfg);
            let target = outside_dir("esc-bypass").join("climbed.txt");
            let _ = std::fs::remove_file(&target);
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("echo climbed > {}", target.display())),
                &ctx,
            )
            .await;
            assert!(!is_error, "{out}");
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "climbed\n");
            let _ = std::fs::remove_file(target);
            assert!(out.contains("automatically in bypass mode"), "{out}");
            assert!(out.contains("Operation not permitted"), "{out}");
            assert!(!out.contains("after user approval"), "{out}");
            assert!(!out.contains("looks like a sandbox"), "{out}");
        }

        /// A remembered escalation skips the contained attempt entirely: the
        /// command runs uncontained on the first try, nobody is asked, and the
        /// result says so. Remembering the answer without this only saves the
        /// click — the expensive half is the run that was always going to be
        /// thrown away.
        #[tokio::test]
        async fn a_remembered_escalation_skips_the_sandboxed_attempt() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, asked) = escalating_ctx(
                "esc-remembered",
                crate::permissions::Decision::Allow(
                    crate::permissions::ApprovalScope::WorkspaceSession,
                ),
            );
            // `git init` so the remembered two-word prefix is the subcommand,
            // not the path — the second call must be a *different* argument
            // covered by the same rule, which is the point of the prefix.
            let dir = outside_dir("esc-remembered");
            let first = dir.join("one");
            let second = dir.join("two");
            let _ = std::fs::remove_dir_all(&first);
            let _ = std::fs::remove_dir_all(&second);
            std::fs::create_dir_all(&first).unwrap();
            std::fs::create_dir_all(&second).unwrap();

            // First call: denied inside the sandbox, asked once, approved with
            // the workspace scope.
            let (out, is_error) = run_tool(
                "bash",
                bash_input(&format!("git init {}", first.display())),
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
                bash_input(&format!("git init {}", second.display())),
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
            assert!(
                second.join(".git").is_dir(),
                "the uncontained run actually did the work"
            );
        }

        #[tokio::test]
        async fn remembered_log_writes_skip_the_sandboxed_attempt() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
            let (ctx, asked) = escalating_ctx(
                "log-remembered",
                crate::permissions::Decision::Allow(
                    crate::permissions::ApprovalScope::WorkspaceSession,
                ),
            );
            let outside = outside_dir("log-remembered");
            let counter = ctx.cfg.sandbox.as_ref().unwrap().writable_roots[0]
                .root
                .join("attempts.log");
            let _ = std::fs::remove_file(&counter);
            for (index, name) in ["first.log", "next.log"].into_iter().enumerate() {
                let command = format!(
                    "echo attempt >> {}; echo \"EXIT:$?\" >> {}; echo complete > {}",
                    counter.display(),
                    counter.display(),
                    outside.join(name).display()
                );
                let (out, is_error) = run_tool("bash", bash_input(&command), &ctx).await;
                assert!(!is_error, "{out}");
                assert_eq!(asked.load(Ordering::SeqCst), 1, "{out}");
                assert_eq!(
                    std::fs::read_to_string(outside.join(name)).unwrap(),
                    "complete\n"
                );
                if index == 0 {
                    assert!(out.contains("Re-ran without the sandbox"), "{out}");
                    assert_eq!(
                        std::fs::read_to_string(&counter).unwrap(),
                        "attempt\nEXIT:0\nattempt\nEXIT:0\n"
                    );
                } else {
                    assert!(
                        out.contains("a remembered sandbox_escalate rule already covers this"),
                        "{out}"
                    );
                    assert!(!out.contains("Re-ran without the sandbox"), "{out}");
                    assert_eq!(
                        std::fs::read_to_string(&counter).unwrap(),
                        "attempt\nEXIT:0\nattempt\nEXIT:0\nattempt\nEXIT:0\n"
                    );
                }
            }
            std::fs::remove_dir_all(outside).unwrap();
            std::fs::remove_file(counter).unwrap();
        }

        /// Escalation loop, declined: the sandboxed failure is kept and the
        /// result tells the model not to retry it (not the hint, which is for
        /// a denial nobody could be asked about).
        #[tokio::test]
        async fn escalation_declined_keeps_denial_and_warns_off_retry() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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
        /// runs with NOBODY available to approve; the auto_allow=false policy
        /// still reaches the ask layer and fails. A call carrying the removed
        /// `disable_sandbox` fails as malformed before the gate is consulted.
        #[tokio::test]
        async fn auto_allow_runs_contained_bash_without_an_approver() {
            if crate::sandbox::skip_nested_seatbelt_test() {
                return;
            }
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

            // The same call with the removed field never reaches that layer.
            let (out, is_error) = run_tool(
                "bash",
                json!({
                    "command": format!("echo hi > {}", root.join("c.txt").display()),
                    "disable_sandbox": true
                }),
                &ctx,
            )
            .await;
            assert!(is_error);
            assert!(out.contains("unknown field `disable_sandbox`"), "{out}");
            assert!(!out.contains("approval required"), "{out}");
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
}
