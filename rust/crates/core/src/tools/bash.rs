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
    let output = run_foreground(command, &cwd, effective_sandbox, bash, timeout_ms, ctx).await?;
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
                    let raw = run_foreground(command, &cwd, None, bash, timeout_ms, ctx).await?;
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
    timeout_ms: u64,
    ctx: &ToolCtx,
) -> Result<ForegroundOutput> {
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
        let text = super::format_output(&output);
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
