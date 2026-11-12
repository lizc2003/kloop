//! 沙箱拒绝的系统日志证据（plan 283 阶段 2a）。
//!
//! macOS 内核把每次沙箱拒绝写进统一日志：`Sandbox: touch(123) deny(1) file-write-create /path`。
//! profile 里每条 deny 都带上本次调用的标记（`(with message "kloop-deny-…")`），标记出现在事件
//! 正文第二行，`log stream` 按它过滤，就只收到这一次调用的拒绝——不必跟踪进程树，并发调用也
//! 不会串。这些事件不落盘（`log show` 查不到，带 `--info --debug` 也一样），只能在命令运行时接；
//! `log stream` 约 6 ms 输出第一行，以它为就绪信号，就绪后才启动命令。
//!
//! 输出里的关键字只能猜：测试名里的 `sandbox`、`getaddrinfo` 会被当成拒绝，`touch` 的拒绝
//! 认不出是写。事件给的是操作和确切路径，阶段 2b 的“可写某目录”授权要的正是这个路径。

use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncBufReadExt;

use super::DenialKind;
use super::SandboxDenial;

/// How long `log stream` gets to print its first line before the call goes
/// ahead without it (and falls back to reading its output).
const READY_TIMEOUT: Duration = Duration::from_millis(500);

/// Measured delivery is under a millisecond; this only covers a busy machine.
const DELIVERY_GRACE: Duration = Duration::from_millis(50);

/// A fresh per-call marker for the profile's deny rules. Base58, so it needs no
/// quoting inside an SBPL string or a log predicate.
pub(crate) fn fresh_tag() -> Option<String> {
    crate::resource_id::fresh("kloop-deny-").ok()
}

/// One sandbox refusal the kernel logged for this call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoggedDenial {
    /// The sandbox operation, e.g. `file-write-create`, `network-outbound`.
    pub(crate) operation: String,
    /// What it was refused on: a path, a socket, a sysctl name. May be empty.
    pub(crate) target: String,
}

impl LoggedDenial {
    /// `Sandbox: touch(123) deny(1) file-write-create /path` — the first line
    /// of the event; the second is the call's marker.
    pub(crate) fn parse(message: &str) -> Option<Self> {
        let line = message.lines().next()?;
        let rest = line.strip_prefix("Sandbox: ")?;
        let (_, after) = rest.split_once(") deny(")?;
        let (_, refusal) = after.split_once(") ")?;
        let (operation, target) = refusal.split_once(' ').unwrap_or((refusal, ""));
        Some(Self {
            operation: operation.to_string(),
            target: target.to_string(),
        })
    }

    fn from_ndjson(line: &str) -> Option<Self> {
        let event: serde_json::Value = serde_json::from_str(line).ok()?;
        Self::parse(event["eventMessage"].as_str()?)
    }

    fn evidence(&self) -> String {
        if self.target.is_empty() {
            self.operation.clone()
        } else {
            format!("{} {}", self.operation, self.target)
        }
    }
}

/// What the logged refusals say the command was stopped from doing. Every
/// process start under the profile also logs harmless refusals — `sysctl-read`,
/// `mach-lookup` of the log and notification daemons, writes to
/// `/dev/dtracehelper` and `/dev/tty` — so only writes outside `/dev`, network
/// operations and reads count. A write is the actionable one (2b grants a
/// directory), so it wins; then the network; then a read, which is always a
/// protected path (the profile reads the whole disk otherwise).
pub(crate) fn classify_logged(denials: &[LoggedDenial]) -> Option<SandboxDenial> {
    let writes: Vec<&LoggedDenial> = denials
        .iter()
        .filter(|denial| {
            denial.operation.starts_with("file-write") && !denial.target.starts_with("/dev/")
        })
        .collect();
    if let Some(first) = writes.first() {
        let mut paths: Vec<PathBuf> = Vec::new();
        for denial in &writes {
            let path = PathBuf::from(&denial.target);
            if !denial.target.is_empty() && !paths.contains(&path) {
                paths.push(path);
            }
        }
        return Some(SandboxDenial {
            kind: DenialKind::Write,
            evidence: super::clip_evidence(&first.evidence()),
            paths,
        });
    }
    let first_of = |prefix: &str| {
        denials
            .iter()
            .find(|denial| denial.operation.starts_with(prefix))
    };
    if let Some(network) = first_of("network-") {
        return Some(SandboxDenial {
            kind: DenialKind::Network,
            evidence: super::clip_evidence(&network.evidence()),
            paths: Vec::new(),
        });
    }
    first_of("file-read").map(|read| SandboxDenial {
        kind: DenialKind::Read,
        evidence: super::clip_evidence(&read.evidence()),
        paths: Vec::new(),
    })
}

/// A `log stream` subscribed to one call's marker.
pub(crate) struct DenialWatch {
    child: tokio::process::Child,
    lines: tokio::task::JoinHandle<Vec<String>>,
}

impl DenialWatch {
    /// `None` when there is no log to watch (another platform, a missing or
    /// refused `log`, no first line in time); the caller then judges the output.
    pub(crate) async fn start(tag: &str) -> Option<Self> {
        if !cfg!(target_os = "macos") {
            return None;
        }
        // Absolute, like `sandbox-exec`: PATH must not choose the witness.
        let mut child = tokio::process::Command::new("/usr/bin/log")
            .args([
                "stream",
                "--style",
                "ndjson",
                "--predicate",
                &format!("eventMessage CONTAINS \"{tag}\""),
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut reader = tokio::io::BufReader::new(child.stdout.take()?).lines();
        // The "Filtering the log data using …" line: subscribed from here on.
        tokio::time::timeout(READY_TIMEOUT, reader.next_line())
            .await
            .ok()?
            .ok()??;
        let lines = tokio::spawn(async move {
            let mut lines = Vec::new();
            while let Ok(Some(line)) = reader.next_line().await {
                lines.push(line);
            }
            lines
        });
        Some(Self { child, lines })
    }

    /// Everything logged for the call, once the command has exited.
    pub(crate) async fn finish(mut self) -> Vec<LoggedDenial> {
        tokio::time::sleep(DELIVERY_GRACE).await;
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
        self.lines
            .await
            .unwrap_or_default()
            .iter()
            .filter_map(|line| LoggedDenial::from_ndjson(line))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logged(message: &str) -> LoggedDenial {
        LoggedDenial::parse(message).expect(message)
    }

    /// The event's own shape, marker line included (measured on Darwin 25.4).
    #[test]
    fn parses_the_kernel_refusal_line() {
        assert_eq!(
            logged(
                "Sandbox: touch(21032) deny(1) file-write-create /Users/me/a b.txt\nkloop-deny-x"
            ),
            LoggedDenial {
                operation: "file-write-create".into(),
                target: "/Users/me/a b.txt".into(),
            }
        );
        assert_eq!(
            logged("Sandbox: sandbox-exec(65761) deny(1) forbidden-sandbox-reinit"),
            LoggedDenial {
                operation: "forbidden-sandbox-reinit".into(),
                target: String::new(),
            }
        );
        assert_eq!(LoggedDenial::parse("kloop-deny-x"), None);
    }

    /// Startup noise every process logs is no denial; a write outside `/dev`
    /// wins over the network, which wins over a read, and every written path is
    /// kept for the directory grant.
    #[test]
    fn classifies_what_the_command_was_stopped_from_doing() {
        let noise = [
            "Sandbox: zsh(1) deny(1) sysctl-read kern.bootargs",
            "Sandbox: zsh(1) deny(1) file-write-data /dev/dtracehelper",
            "Sandbox: zsh(1) deny(1) file-write-data /dev/tty",
            "Sandbox: zsh(1) deny(1) mach-lookup com.apple.logd",
        ]
        .map(logged);
        assert_eq!(classify_logged(&noise), None);

        let mut events = noise.to_vec();
        events.push(logged(
            "Sandbox: curl(2) deny(1) network-outbound /private/var/run/mDNSResponder",
        ));
        assert_eq!(
            classify_logged(&events),
            Some(SandboxDenial {
                kind: DenialKind::Network,
                evidence: "network-outbound /private/var/run/mDNSResponder".into(),
                paths: Vec::new(),
            })
        );
        events.push(logged(
            "Sandbox: cp(3) deny(1) file-write-create /opt/out/a",
        ));
        events.push(logged("Sandbox: cp(3) deny(1) file-write-data /opt/out/b"));
        events.push(logged(
            "Sandbox: cp(3) deny(1) file-write-create /opt/out/a",
        ));
        assert_eq!(
            classify_logged(&events),
            Some(SandboxDenial {
                kind: DenialKind::Write,
                evidence: "file-write-create /opt/out/a".into(),
                paths: vec![PathBuf::from("/opt/out/a"), PathBuf::from("/opt/out/b")],
            })
        );
        assert_eq!(
            classify_logged(&[logged(
                "Sandbox: cat(4) deny(1) file-read-data /Users/me/.ssh/id_ed25519"
            )])
            .map(|denial| denial.kind),
            Some(DenialKind::Read)
        );
    }
}
