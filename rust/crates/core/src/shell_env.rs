//! The environment a tool shell starts with, captured once from the user's own
//! login shell.
//!
//! A login POSIX shell re-runs `/etc/profile`, and on macOS that runs
//! `path_helper`, which rebuilds `PATH` with `/etc/paths` first and everything
//! else appended. A Homebrew prefix the user's login profile had put in front
//! therefore lands behind `/usr/bin`, and `python3` silently becomes Apple's.
//! kloop asks the user's shell once what its login environment is and then runs
//! commands in a **non-login** shell carrying it, so nothing rewrites `PATH`
//! behind its back. Without a capture the caller keeps the login `-lc` form —
//! exactly the old behavior.

use std::collections::BTreeMap;
use std::path::Path;

use crate::process_tree::ProcessSpec;

/// Provider/search credentials belong to the parent process, never to a
/// model-controlled shell. The capture must not bring them back: the user's own
/// profile is a plausible place for one of these to be exported.
pub(crate) const MODEL_SHELL_SECRET_ENV: &[&str] = &[
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "OPENAI_API_KEY",
    "TAVILY_API_KEY",
    "BRAVE_API_KEY",
];

/// Names the shell derives on every start; replaying the capture's value would
/// pin the child to the capture's own cwd instead of the tool call's.
const VOLATILE_ENV: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// The capture is one `env -0` of a login shell; anything this large is not it.
const CAPTURE_MAX_BYTES: usize = 1 << 20;
/// How long the capture may take before it is abandoned (a profile that waits
/// for input would otherwise hang startup).
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Delimits the `env -0` payload from anything the login profile prints.
const CAPTURE_MARKER: u8 = 0x01;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellLoginEnv {
    vars: BTreeMap<String, String>,
}

impl ShellLoginEnv {
    /// No capture: commands keep the login `-lc` form.
    pub fn none() -> Self {
        Self::default()
    }

    /// True when a login environment was captured, and so when commands run in
    /// a non-login shell carrying it.
    pub fn is_active(&self) -> bool {
        !self.vars.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// A capture to test against: `capture` is the only production constructor,
    /// and a stand-in map reads back the same way.
    #[cfg(test)]
    pub fn test_fixture(vars: &[(&str, &str)]) -> Self {
        Self {
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
        }
    }

    /// Overlay the captured variables on a process spec. Only the names the
    /// login shell set are written; everything else stays inherited — the same
    /// rule `[env]` follows, and it keeps Windows' environment block intact.
    pub(crate) fn apply(&self, spec: &mut ProcessSpec) {
        for (name, value) in &self.vars {
            spec.env(name.as_str(), value.as_str());
        }
    }

    /// Run `<shell> -lc` once and keep the environment it comes up with.
    ///
    /// `config_env_names` are the `[env]` names: the config file wins over what
    /// the shell exported (plan 172), so they are never taken from the capture.
    /// The returned warning is for the startup line; `none()` means "keep the
    /// login `-lc` behavior".
    #[cfg(unix)]
    pub fn capture(shell: &Path, config_env_names: &[String]) -> (Self, Option<String>) {
        Self::capture_within(shell, config_env_names, CAPTURE_TIMEOUT)
    }

    #[cfg(unix)]
    fn capture_within(
        shell: &Path,
        config_env_names: &[String],
        timeout: std::time::Duration,
    ) -> (Self, Option<String>) {
        // `command` keeps a shell function named `env` out of the way; the
        // markers separate `env -0` from anything the login profile prints.
        let script = "printf '\\001'; command env -0 2>/dev/null; printf '\\001'";
        let Some(bytes) = run_capture(shell, script, timeout) else {
            return (Self::none(), Some(unavailable(shell)));
        };
        let mut vars = BTreeMap::new();
        for (name, value) in parse_env0(&bytes) {
            if MODEL_SHELL_SECRET_ENV.contains(&name.as_str())
                || VOLATILE_ENV.contains(&name.as_str())
                || config_env_names.iter().any(|blocked| blocked == &name)
            {
                continue;
            }
            vars.insert(name, value);
        }
        if vars.is_empty() {
            return (Self::none(), Some(unavailable(shell)));
        }
        (Self { vars }, None)
    }

    #[cfg(not(unix))]
    pub fn capture(_shell: &Path, _config_env_names: &[String]) -> (Self, Option<String>) {
        (Self::none(), None)
    }
}

fn unavailable(shell: &Path) -> String {
    format!(
        "the login environment of {} could not be captured; shell tools will start a login \
         shell instead",
        shell.display()
    )
}

/// Run the capture, bounded by [`CAPTURE_TIMEOUT`]. The child is killed and its
/// reader thread left to finish on its own when the deadline passes; stdout is
/// capped so a hostile profile cannot balloon startup memory.
#[cfg(unix)]
fn run_capture(shell: &Path, script: &str, timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::process::Command;
    use std::process::Stdio;
    use std::sync::mpsc;
    use std::time::Instant;

    let mut child = Command::new(shell)
        .arg("-lc")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout
            .take((CAPTURE_MAX_BYTES + 1) as u64)
            .read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    return None;
                }
                // The reader may still be draining the last of the pipe; give
                // it a moment rather than dropping a complete capture.
                return rx.recv_timeout(std::time::Duration::from_millis(200)).ok();
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// The NUL-separated `NAME=VALUE` records between the capture markers. Entries
/// that are not UTF-8 or have no `=` are dropped rather than guessed at.
fn parse_env0(bytes: &[u8]) -> Vec<(String, String)> {
    let Some(start) = bytes.iter().position(|byte| *byte == CAPTURE_MARKER) else {
        return Vec::new();
    };
    let Some(end) = bytes.iter().rposition(|byte| *byte == CAPTURE_MARKER) else {
        return Vec::new();
    };
    if end <= start {
        return Vec::new();
    }
    let mut out = Vec::new();
    for entry in bytes[start + 1..end].split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let Ok(text) = std::str::from_utf8(entry) else {
            continue;
        };
        let Some((name, value)) = text.split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        out.push((name.to_string(), value.to_string()));
    }
    out
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// A stand-in for the user's login shell: it ignores `-lc <script>` and
    /// prints exactly the marked payload it was built with.
    fn fake_shell(tag: &str, body: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kloop-shell-env-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shell");
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    const MARKED: &str = "printf '\\001'; printf \
        'PATH=/login/bin\\000KLOOP_TEST_SENTINEL=1\\000ANTHROPIC_API_KEY=leak\\000PWD=/login\\000BLOCKED=cfg\\000'; \
        printf '\\001'";

    #[test]
    fn parse_env0_reads_only_the_marked_payload() {
        let bytes = b"profile banner\x01A=1\0B=2\0\x01trailing";
        assert_eq!(
            parse_env0(bytes),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ]
        );
    }

    #[test]
    fn parse_env0_ignores_entries_with_no_name_or_no_value() {
        assert_eq!(
            parse_env0(b"\x01\0=NOPE\0A\0\x01"),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn capture_keeps_the_login_environment_but_not_secrets_or_config_names() {
        let shell = fake_shell("keep", &format!("#!/bin/sh\n{MARKED}\n"));
        let (env, warning) = ShellLoginEnv::capture(&shell, &["BLOCKED".to_string()]);
        assert!(warning.is_none());
        assert!(env.is_active());
        assert_eq!(env.get("PATH"), Some("/login/bin"));
        assert_eq!(env.get("KLOOP_TEST_SENTINEL"), Some("1"));
        assert_eq!(
            env.get("ANTHROPIC_API_KEY"),
            None,
            "secrets never come back"
        );
        assert_eq!(env.get("PWD"), None, "the shell derives this per call");
        assert_eq!(env.get("BLOCKED"), None, "the config file wins");
    }

    #[test]
    fn a_failing_shell_capture_degrades_to_none() {
        let shell = fake_shell("fail", "#!/bin/sh\nexit 3\n");
        let (env, warning) = ShellLoginEnv::capture(&shell, &[]);
        assert!(!env.is_active());
        assert!(warning.unwrap().contains("login environment"));
    }

    #[test]
    fn a_wedged_shell_capture_times_out() {
        let shell = fake_shell("slow", "#!/bin/sh\nsleep 30\n");
        let (env, warning) =
            ShellLoginEnv::capture_within(&shell, &[], std::time::Duration::from_millis(200));
        assert!(!env.is_active());
        assert!(warning.is_some());
    }
}
