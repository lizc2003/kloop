//! Foreground and background shell execution. The background shape follows
//! cc: `run_in_background` returns immediately with an id and an output file
//! (stdout/stderr interleaved at the fd level — no reader tasks, no pipe
//! deadlock), `bash_output` blocks on completion by default, `kill_bash`
//! kills the whole process group. Interrupting a turn never touches
//! background shells; only kill_bash, the size watchdog, and process exit
//! reap them. cc's auto-backgrounding, completion notifications and Monitor
//! tool are not ported.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
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
use tokio_util::sync::CancellationToken;

use super::str_arg;
use super::ToolCtx;
use crate::permissions::EscalationOutcome;
use crate::sandbox;
use crate::sandbox::SandboxPolicy;

/// Tail returned inline by bash_output; the rest stays in the output file
/// (cc's BASH_MAX_OUTPUT_DEFAULT is 30k chars).
const OUTPUT_TAIL_BYTES: u64 = 30_000;
/// Watchdog cap on the output file — a runaway `yes`-style command gets its
/// group killed instead of filling the disk (cc caps at 5GB; kloop is
/// stingier).
const OUTPUT_FILE_CAP: u64 = 1 << 30;
const WATCHDOG_INTERVAL: Duration = Duration::from_secs(5);
/// bash_output blocking defaults, straight from cc's TaskOutput.
const BLOCK_TIMEOUT_DEFAULT_MS: u64 = 30_000;
const BLOCK_TIMEOUT_MAX_MS: u64 = 600_000;
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Process-global so parent/sub-agents and server threads sharing one
/// offload directory never collide on output file names (same lesson as the
/// offload counter).
static NEXT_BG_ID: AtomicUsize = AtomicUsize::new(1);

/// The process for `sh -lc <command>`, wrapped in the OS sandbox when a
/// policy applies. The env vars are hints only (codex's CODEX_SANDBOX
/// shape): scripts get a way to detect the sandbox instead of failing
/// mysteriously; enforcement is the profile.
fn shell_command(command: &str, sandbox: Option<&SandboxPolicy>) -> tokio::process::Command {
    match sandbox {
        Some(policy) => {
            let (program, args) = sandbox::seatbelt_command(policy, command);
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args);
            cmd.env("KLOOP_SANDBOX", "seatbelt");
            if !policy.allow_network {
                cmd.env("KLOOP_SANDBOX_NETWORK_DISABLED", "1");
            }
            cmd
        }
        None => {
            let mut cmd = tokio::process::Command::new("sh");
            cmd.arg("-lc").arg(command);
            cmd
        }
    }
}

/// The session sandbox policy for this call: disable_sandbox is the model's
/// per-call escape hatch (cc's dangerouslyDisableSandbox shape). The call
/// still went through the permission gate like any other — escaping changes
/// the execution wrapper, never the asking.
fn call_sandbox<'a>(input: &Value, ctx: &'a ToolCtx) -> Option<&'a SandboxPolicy> {
    if input["disable_sandbox"].as_bool().unwrap_or(false) {
        None
    } else {
        ctx.cfg.sandbox.as_deref()
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
    let sandbox = call_sandbox(input, ctx);
    if input["run_in_background"].as_bool().unwrap_or(false) {
        // No timeout in background mode (cc clears the timer too); the
        // watchdog and kill_bash are the safety net.
        return ctx
            .cfg
            .background_shells
            .spawn_background(command, &ctx.cfg.offload_dir, sandbox);
    }
    let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(60_000);
    let output = run_foreground(command, sandbox, timeout_ms).await?;
    let mut text = format_output(&output);

    // Sandbox denial handling applies only to an actually-sandboxed run;
    // disable_sandbox / no policy leaves `sandbox` None and skips it.
    if let Some(policy) = sandbox {
        if !output.status.success()
            && sandbox::is_likely_sandbox_denied(output.status.code(), &text, !policy.allow_network)
        {
            if policy.escalate {
                // The code-level escalation loop (codex's retry-on-denial):
                // ask once, and on approval re-run the command unsandboxed —
                // one fewer model round-trip than the disable_sandbox hint.
                match ctx
                    .cfg
                    .permissions
                    .escalate_sandbox(command, ctx.depth)
                    .await
                {
                    EscalationOutcome::Approved => {
                        let raw = run_foreground(command, None, timeout_ms).await?;
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

/// One foreground run of `sh -lc <command>`, wrapped in the OS sandbox per
/// `sandbox`. The caller turns the raw output into model-facing text.
async fn run_foreground(
    command: &str,
    sandbox: Option<&SandboxPolicy>,
    timeout_ms: u64,
) -> Result<std::process::Output> {
    let mut cmd = shell_command(command, sandbox);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Its own process group so a timeout or a cancelled turn can SIGKILL the
    // whole tree, not just the `sh` leader: kill_on_drop reaps only the direct
    // child, leaving `make`/`npm` grandchildren orphaned and still running.
    #[cfg(unix)]
    cmd.process_group(0);
    let child = cmd.spawn().context("bash: failed to spawn sh")?;
    // Group-kills the tree if this future is dropped mid-run (turn cancel) or
    // times out; disarmed once the child has exited cleanly on its own.
    let mut guard = GroupKillGuard { pid: child.id() };
    match tokio::time::timeout(Duration::from_millis(timeout_ms), child.wait_with_output()).await {
        Ok(result) => {
            guard.disarm();
            result.context("bash: failed to run sh")
        }
        Err(_) => bail!("bash: command timed out after {timeout_ms}ms"),
    }
}

/// Group-kills a foreground shell's process tree if dropped before its child
/// exits on its own (timeout, or the owning turn being cancelled). Disarmed on
/// a clean exit so a reused group id is never signalled.
struct GroupKillGuard {
    pid: Option<u32>,
}

impl GroupKillGuard {
    fn disarm(&mut self) {
        self.pid = None;
    }
}

impl Drop for GroupKillGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            kill_group(pid);
        }
    }
}

/// stdout+stderr merged, a trailing `[exit …]` when the run failed, and a
/// placeholder when empty — the model-facing text for one run (denial
/// annotation is the caller's job, so an escalated re-run reuses this).
fn format_output(output: &std::process::Output) -> String {
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
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
        let done = !matches!(status, BgStatus::Running);
        if done || !block || started.elapsed() >= Duration::from_millis(timeout_ms) {
            break (status, path, sandboxed);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    let status_line = match &status {
        BgStatus::Running if block => format!("{id}: still running after {timeout_ms}ms"),
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
            Some((BgStatus::Running, _, _)) => tokio::time::sleep(POLL_INTERVAL).await,
            _ => break,
        }
    }
    Ok(format!("Stopped {id} ({command})"))
}

#[derive(Clone, Debug)]
enum BgStatus {
    Running,
    /// Normal termination; None = killed by an external signal.
    Exited(Option<i32>),
    /// Killed through this registry, with the reason.
    Killed(String),
}

fn status_text(status: &BgStatus) -> String {
    match status {
        BgStatus::Running => "running".into(),
        BgStatus::Exited(Some(0)) => "completed (exit 0)".into(),
        BgStatus::Exited(Some(code)) => format!("failed (exit {code})"),
        BgStatus::Exited(None) => "killed by signal".into(),
        BgStatus::Killed(reason) => format!("killed ({reason})"),
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
    pid: Option<u32>,
    /// Some = ran inside the OS sandbox.
    sandbox: Option<BgSandbox>,
}

/// Session-scoped registry of background shells (one per Config; sub-agents
/// share the parent's through the Config clone).
#[derive(Default)]
pub struct BackgroundShells {
    shells: Mutex<HashMap<String, BgShell>>,
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
        offload_dir: &Path,
        sandbox: Option<&SandboxPolicy>,
    ) -> Result<String> {
        std::fs::create_dir_all(offload_dir)
            .with_context(|| format!("bash: cannot create {}", offload_dir.display()))?;
        let id = format!("bg-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let path = offload_dir.join(format!("{id}.out"));
        let stdout = std::fs::File::create(&path)
            .with_context(|| format!("bash: cannot create {}", path.display()))?;
        let stderr = stdout
            .try_clone()
            .context("bash: cannot clone output file")?;
        let mut cmd = shell_command(command, sandbox);
        cmd.stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true);
        // Its own process group: turn interrupts (terminal signals) never
        // reach it, and kill_bash can take down the whole tree at once.
        #[cfg(unix)]
        cmd.process_group(0);
        let child = cmd.spawn().context("bash: failed to spawn sh")?;
        let pid = child.id();
        self.shells.lock().unwrap().insert(
            id.clone(),
            BgShell {
                command: command.to_string(),
                output_path: path.clone(),
                status: BgStatus::Running,
                kill: CancellationToken::new(),
                pid,
                sandbox: sandbox.map(|p| BgSandbox {
                    network_disabled: !p.allow_network,
                }),
            },
        );
        let kill = self.shells.lock().unwrap()[&id].kill.clone();
        // A Weak, not an Arc: an owning ref would keep the registry alive as
        // long as any shell runs, so `Drop for BackgroundShells` (the session
        // teardown that group-kills leftover shells) could never fire while it
        // still had work to reap. The monitor only needs the registry to write
        // back a final status, which it skips if the session is already gone.
        tokio::spawn(monitor(
            Arc::downgrade(self),
            id.clone(),
            child,
            kill,
            path.clone(),
        ));
        Ok(format!(
            "Command running in background with ID: {id}. Output is being written to: {}. \
             Check on it with bash_output; stop it with kill_bash.",
            path.display()
        ))
    }

    fn snapshot(&self, id: &str) -> Option<(BgStatus, PathBuf, Option<BgSandbox>)> {
        let shells = self.shells.lock().unwrap();
        let shell = shells.get(id)?;
        Some((
            shell.status.clone(),
            shell.output_path.clone(),
            shell.sandbox,
        ))
    }

    /// Flags the shell for its monitor task to kill; Err carries the
    /// model-facing reason.
    fn request_kill(&self, id: &str) -> Result<String, String> {
        let shells = self.shells.lock().unwrap();
        let Some(shell) = shells.get(id) else {
            return Err(format!("no background command with id {id}"));
        };
        if !matches!(shell.status, BgStatus::Running) {
            return Err(format!(
                "{id} is not running (status: {})",
                status_text(&shell.status)
            ));
        }
        shell.kill.cancel();
        Ok(shell.command.clone())
    }

    fn set_status(&self, id: &str, status: BgStatus) {
        if let Some(shell) = self.shells.lock().unwrap().get_mut(id) {
            shell.status = status;
        }
    }
}

impl Drop for BackgroundShells {
    /// Best-effort reaping at session end: kill_on_drop takes the `sh`
    /// itself, the group kill takes its descendants.
    fn drop(&mut self) {
        for shell in self.shells.lock().unwrap().values() {
            if matches!(shell.status, BgStatus::Running) {
                if let Some(pid) = shell.pid {
                    kill_group(pid);
                }
            }
        }
    }
}

/// SIGKILL the whole process group (the child is its own group leader).
fn kill_group(pid: u32) {
    #[cfg(unix)]
    {
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(format!("-{pid}"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
    #[cfg(not(unix))]
    let _ = pid;
}

/// Owns the child: waits for exit, executes kill requests, and enforces the
/// output-file cap. Deliberately not tied to any turn's cancel token — that
/// is what makes the shell "background".
async fn monitor(
    shells: Weak<BackgroundShells>,
    id: String,
    mut child: tokio::process::Child,
    kill: CancellationToken,
    output_path: PathBuf,
) {
    let pid = child.id();
    let mut watchdog = tokio::time::interval(WATCHDOG_INTERVAL);
    watchdog.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut kill_reason: Option<String> = None;
    let exit = loop {
        tokio::select! {
            status = child.wait() => break status.ok(),
            _ = kill.cancelled(), if kill_reason.is_none() => {
                if let Some(pid) = pid {
                    kill_group(pid);
                }
                let _ = child.start_kill();
                kill_reason = Some("stopped".into());
            }
            _ = watchdog.tick() => {
                let size = std::fs::metadata(&output_path).map_or(0, |m| m.len());
                if size > OUTPUT_FILE_CAP && kill_reason.is_none() {
                    if let Some(pid) = pid {
                        kill_group(pid);
                    }
                    let _ = child.start_kill();
                    kill_reason = Some(format!("output file exceeded {OUTPUT_FILE_CAP} bytes"));
                }
            }
        }
    };
    let status = match kill_reason {
        Some(reason) => BgStatus::Killed(reason),
        None => BgStatus::Exited(exit.and_then(|s| s.code())),
    };
    // The session may have ended while this shell ran; if so there is no
    // registry left to update (Drop already group-killed it).
    if let Some(shells) = shells.upgrade() {
        shells.set_status(&id, status);
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
    use crate::tools::testutil::*;
    use serde_json::json;

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
    async fn background_failure_reports_exit_code() {
        let ctx = test_ctx(0, "bg-fail");
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

        // A second kill is an error: the shell is no longer running.
        let (out, is_error) = run_tool("kill_bash", json!({"bash_id": id}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("not running"), "{out}");
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
                crate::permissions::Mode::Default,
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
                        crate::permissions::Mode::Default,
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
