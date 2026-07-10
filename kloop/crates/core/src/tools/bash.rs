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
use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::str_arg;
use super::ToolCtx;

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

pub(super) async fn bash_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let command = str_arg(input, "command", "bash")?;
    if input["run_in_background"].as_bool().unwrap_or(false) {
        // No timeout in background mode (cc clears the timer too); the
        // watchdog and kill_bash are the safety net.
        return ctx
            .cfg
            .background_shells
            .spawn_background(command, &ctx.cfg.offload_dir);
    }
    let timeout_ms = input["timeout_ms"].as_u64().unwrap_or(60_000);
    let output = tokio::time::timeout(
        Duration::from_millis(timeout_ms),
        tokio::process::Command::new("sh")
            .arg("-lc")
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| anyhow!("bash: command timed out after {timeout_ms}ms"))?
    .context("bash: failed to spawn sh")?;

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
    Ok(text)
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
    let (status, path) = loop {
        let Some((status, path)) = shells.snapshot(id) else {
            bail!("bash_output: no background command with id {id}");
        };
        let done = !matches!(status, BgStatus::Running);
        if done || !block || started.elapsed() >= Duration::from_millis(timeout_ms) {
            break (status, path);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    };
    let status_line = match &status {
        BgStatus::Running if block => format!("{id}: still running after {timeout_ms}ms"),
        _ => format!("{id}: {}", status_text(&status)),
    };
    let tail = read_tail(&path).await;
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
            Some((BgStatus::Running, _)) => tokio::time::sleep(POLL_INTERVAL).await,
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

struct BgShell {
    command: String,
    output_path: PathBuf,
    status: BgStatus,
    kill: CancellationToken,
    pid: Option<u32>,
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

    fn spawn_background(self: &Arc<Self>, command: &str, offload_dir: &Path) -> Result<String> {
        std::fs::create_dir_all(offload_dir)
            .with_context(|| format!("bash: cannot create {}", offload_dir.display()))?;
        let id = format!("bg-{}", NEXT_BG_ID.fetch_add(1, Ordering::Relaxed));
        let path = offload_dir.join(format!("{id}.out"));
        let stdout = std::fs::File::create(&path)
            .with_context(|| format!("bash: cannot create {}", path.display()))?;
        let stderr = stdout
            .try_clone()
            .context("bash: cannot clone output file")?;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-lc")
            .arg(command)
            .stdin(Stdio::null())
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
            },
        );
        let kill = self.shells.lock().unwrap()[&id].kill.clone();
        tokio::spawn(monitor(self.clone(), id.clone(), child, kill, path.clone()));
        Ok(format!(
            "Command running in background with ID: {id}. Output is being written to: {}. \
             Check on it with bash_output; stop it with kill_bash.",
            path.display()
        ))
    }

    fn snapshot(&self, id: &str) -> Option<(BgStatus, PathBuf)> {
        let shells = self.shells.lock().unwrap();
        let shell = shells.get(id)?;
        Some((shell.status.clone(), shell.output_path.clone()))
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
    shells: Arc<BackgroundShells>,
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
    shells.set_status(&id, status);
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
