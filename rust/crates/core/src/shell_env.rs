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
//!
//! The capture is a full picture, not a set of additions: a name the profile
//! dropped is dropped from the command too, and the shell re-applies the
//! capture itself just before the command runs — zsh reads `/etc/zshenv` no
//! matter how it is invoked (the manual: "this cannot be overridden"), so a
//! process environment alone can be rewritten before the command starts.

use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

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

/// Startup hooks a non-interactive shell still expands on its own: bash reads
/// `BASH_ENV`, a POSIX `sh` historically reads `ENV` when interactive. Letting
/// one run would let the captured environment be rewritten from under kloop —
/// including a credential the capture filtered out — so they are removed
/// whenever a capture is replayed.
const SHELL_STARTUP_HOOKS: &[&str] = &["BASH_ENV", "ENV"];

/// Names the shell derives on every start; replaying the capture's value would
/// pin the child to the capture's own cwd instead of the tool call's.
#[cfg(unix)]
const VOLATILE_ENV: &[&str] = &["PWD", "OLDPWD", "SHLVL", "_"];

/// Variables kloop itself adds to a shell command. The capture never saw them,
/// so they are not the login shell's to drop.
#[cfg(unix)]
const KLOOP_SHELL_ENV: &[&str] = &["KLOOP_SANDBOX", "KLOOP_SANDBOX_NETWORK_DISABLED"];

#[cfg(any(unix, test))]
const SHELL_READONLY_ENV: &[&str] = &[
    "BASHOPTS",
    "BASH_VERSINFO",
    "EUID",
    "PPID",
    "SHELLOPTS",
    "UID",
    "ZSH_EVAL_CONTEXT",
    "ZSH_PATCHLEVEL",
    "ZSH_SUBSHELL",
    "ZSH_VERSION",
];

/// The capture is one `env -0` of a login shell; anything this large is not it.
#[cfg(unix)]
const CAPTURE_MAX_BYTES: usize = 1 << 20;
/// How long the capture may take before it is abandoned (a profile that waits
/// for input would otherwise hang startup).
#[cfg(unix)]
const CAPTURE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// Delimits the `env -0` payload from anything the login profile prints.
#[cfg(unix)]
const CAPTURE_MARKER: u8 = 0x01;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ShellLoginEnv {
    vars: BTreeMap<String, String>,
    removed: BTreeSet<String>,
    replay_file: Option<Arc<ReplayFile>>,
}

#[derive(Debug, PartialEq, Eq)]
struct ReplayFile {
    path: PathBuf,
}

impl Drop for ReplayFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
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
        Self::test_fixture_with_removals(vars, &[])
    }

    #[cfg(test)]
    pub fn test_fixture_with_removals(vars: &[(&str, &str)], removed: &[&str]) -> Self {
        Self {
            vars: vars
                .iter()
                .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
                .collect(),
            removed: removed.iter().map(|name| (*name).to_string()).collect(),
            replay_file: None,
        }
        .prepare_replay()
        .expect("create the test replay file")
    }

    pub(crate) fn replay_path(&self) -> Option<&Path> {
        self.replay_file.as_ref().map(|file| file.path.as_path())
    }

    #[cfg(any(unix, test))]
    fn prepare_replay(mut self) -> std::io::Result<Self> {
        use std::io::Write as _;

        if !self.is_active() {
            return Ok(self);
        }
        let (_, path, mut file) =
            crate::resource_id::create_file(&std::env::temp_dir(), "shell-env-", ".sh")?;
        let replay_file = Arc::new(ReplayFile { path });
        file.write_all(self.replay().as_bytes())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            file.set_permissions(std::fs::Permissions::from_mode(0o400))?;
        }
        self.replay_file = Some(replay_file);
        Ok(self)
    }

    /// Replay the capture on a process spec: the environment the login shell
    /// came up with, minus a named startup hook, plus everything kloop keeps
    /// out of it.
    ///
    /// Removals are written before the additions, and the process spec applies
    /// `env_remove` last, so a name cannot survive by being inherited.
    pub(crate) fn apply(&self, spec: &mut ProcessSpec) {
        if !self.is_active() {
            // No capture: the login `-lc` form keeps its own startup behavior,
            // so nothing beyond the credential scrub is touched.
            return;
        }
        for name in SHELL_STARTUP_HOOKS {
            spec.env_remove(*name);
        }
        for name in &self.removed {
            spec.env_remove(name.as_str());
        }
        for (name, value) in &self.vars {
            if !is_blocked(name) {
                spec.env(name.as_str(), value.as_str());
            }
        }
    }

    /// The script the shell runs before the command: drop the names the login
    /// shell dropped (and the credential names it must never carry), then put
    /// the captured values back.
    ///
    /// Values are single-quoted, and a name that is not a shell identifier is
    /// left out — an environment name may contain anything, and one that is not
    /// an identifier would turn this script into a syntax error. Those names are
    /// still covered by the process spec's own additions and removals.
    #[cfg(any(unix, test))]
    fn replay(&self) -> String {
        let unset: BTreeSet<&str> = MODEL_SHELL_SECRET_ENV
            .iter()
            .copied()
            .chain(SHELL_STARTUP_HOOKS.iter().copied())
            .chain(self.removed.iter().map(String::as_str))
            .filter(|name| is_identifier(name) && !SHELL_READONLY_ENV.contains(name))
            .collect();
        let mut script = String::new();
        if !unset.is_empty() {
            script.push_str("unset");
            for name in unset {
                script.push(' ');
                script.push_str(name);
            }
            script.push_str(" || exit $?");
        }
        for (name, value) in &self.vars {
            if !is_identifier(name)
                || is_blocked(name)
                || SHELL_READONLY_ENV.contains(&name.as_str())
            {
                continue;
            }
            if !script.is_empty() {
                script.push('\n');
            }
            script.push_str("export ");
            script.push_str(name);
            script.push('=');
            script.push_str(&quote_sh(value));
            script.push_str(" || exit $?");
        }
        script
    }

    /// Run `<shell> -lc` once and keep the environment it comes up with.
    ///
    /// `config_env` 的实际值覆盖捕获，并在 shell 启动之后恢复。
    /// 失败返回启动警告与 `none()`，继续沿用登录 `-lc`。
    #[cfg(unix)]
    pub fn capture(shell: &Path, config_env: &[(String, String)]) -> (Self, Option<String>) {
        Self::capture_within(shell, config_env, CAPTURE_TIMEOUT)
    }

    #[cfg(unix)]
    fn capture_within(
        shell: &Path,
        config_env: &[(String, String)],
        timeout: std::time::Duration,
    ) -> (Self, Option<String>) {
        // `command` keeps a shell function named `env` out of the way; the
        // markers separate `env -0` from anything the login profile prints.
        let script = "printf '\\001'; command env -0 2>/dev/null; printf '\\001'";
        let Some(bytes) = run_capture(shell, script, timeout) else {
            return (Self::none(), Some(unavailable(shell)));
        };
        let parsed = parse_env0(&bytes);
        if parsed.pairs.is_empty() {
            return (Self::none(), Some(unavailable(shell)));
        }
        let mut vars = BTreeMap::new();
        for (name, value) in parsed.pairs.into_iter().chain(config_env.iter().cloned()) {
            if is_blocked(&name) || VOLATILE_ENV.contains(&name.as_str()) {
                continue;
            }
            vars.insert(name, value);
        }
        if vars.is_empty() {
            return (Self::none(), Some(unavailable(shell)));
        }
        let removed = removals(
            std::env::vars_os().map(|(name, _)| name.to_string_lossy().into_owned()),
            &parsed.names,
            &config_env
                .iter()
                .map(|(name, _)| name.clone())
                .collect::<Vec<_>>(),
        );
        match (Self {
            vars,
            removed,
            replay_file: None,
        })
        .prepare_replay()
        {
            Ok(env) => (env, None),
            Err(_) => (Self::none(), Some(unavailable(shell))),
        }
    }

    #[cfg(not(unix))]
    pub fn capture(_shell: &Path, _config_env: &[(String, String)]) -> (Self, Option<String>) {
        (Self::none(), None)
    }
}

fn is_blocked(name: &str) -> bool {
    MODEL_SHELL_SECRET_ENV.contains(&name) || SHELL_STARTUP_HOOKS.contains(&name)
}

/// Which names the login shell dropped: present in kloop's own environment but
/// absent from the capture. Replaying the capture as additions alone would let a
/// value the profile deliberately unset (`PYTHONHOME` is the classic) walk back
/// in through inheritance.
///
/// `kept` is every name the capture *reported*, not just the ones whose value
/// kloop could decode: a name whose value is not UTF-8 is still a name the login
/// shell kept, and dropping it would take a variable like `PATH` away from a
/// user who never unset it.
#[cfg(unix)]
fn removals(
    process: impl IntoIterator<Item = String>,
    kept: &BTreeSet<String>,
    config_env_names: &[String],
) -> BTreeSet<String> {
    let mut removed = BTreeSet::new();
    for name in process {
        if kept.contains(&name)
            || config_env_names.iter().any(|blocked| blocked == &name)
            || KLOOP_SHELL_ENV.contains(&name.as_str())
        {
            continue;
        }
        removed.insert(name);
    }
    removed
}

/// Whether a name may appear in the replay script at all.
#[cfg(any(unix, test))]
fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// Single-quote a value for the replay script: everything inside is literal, and
/// an embedded quote closes, escapes and reopens it.
#[cfg(any(unix, test))]
fn quote_sh(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(unix)]
fn unavailable(shell: &Path) -> String {
    format!(
        "the login environment of {} could not be captured; shell tools will start a login \
         shell instead",
        shell.display()
    )
}

/// Run the capture, bounded by `timeout`. The probe gets its own process group:
/// a profile that waits on a child script must not leave that script running —
/// and holding the stdout pipe — after the shell itself is killed.
#[cfg(unix)]
fn run_capture(shell: &Path, script: &str, timeout: std::time::Duration) -> Option<Vec<u8>> {
    use std::io::Read;
    use std::os::unix::process::CommandExt as _;
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
        .process_group(0)
        .spawn()
        .ok()?;
    let pid = child.id();
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
    let outcome = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if !status.success() {
                    break None;
                }
                // The reader may still be draining the last of the pipe; give
                // it a moment rather than dropping a complete capture.
                break rx.recv_timeout(std::time::Duration::from_millis(200)).ok();
            }
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            break None;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    // Every way out of the loop — a nonzero exit, a success whose stdout a
    // background descendant still holds open, a timeout, a wait error — means
    // the capture is unusable, and the probe's group must not outlive it. A
    // plain `Child` drop would leave those descendants running.
    kill_group(pid, &mut child);
    outcome
}

/// Kill the probe's whole process group, then reap its root. The reader thread
/// ends on its own once every writer of the pipe is gone.
#[cfg(unix)]
fn kill_group(pid: u32, child: &mut std::process::Child) {
    if let Some(pid) = rustix::process::Pid::from_raw(pid as _) {
        let _ = rustix::process::kill_process_group(pid, rustix::process::Signal::KILL);
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// A parsed capture: the `NAME=VALUE` pairs kloop can replay, and every name the
/// login shell reported — including entries whose value is not UTF-8, which are
/// kept as names so they are not mistaken for names the profile dropped.
#[cfg(unix)]
#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedEnv {
    pairs: Vec<(String, String)>,
    names: BTreeSet<String>,
}

/// The NUL-separated `NAME=VALUE` records between the capture markers. An entry
/// with no `=` is dropped; an entry whose value is not UTF-8 keeps its name but
/// no value, because kloop cannot replay what it cannot read.
#[cfg(unix)]
fn parse_env0(bytes: &[u8]) -> ParsedEnv {
    let Some(start) = bytes.iter().position(|byte| *byte == CAPTURE_MARKER) else {
        return ParsedEnv::default();
    };
    let Some(end) = bytes.iter().rposition(|byte| *byte == CAPTURE_MARKER) else {
        return ParsedEnv::default();
    };
    if end <= start {
        return ParsedEnv::default();
    }
    let mut parsed = ParsedEnv::default();
    for entry in bytes[start + 1..end].split(|byte| *byte == 0) {
        if entry.is_empty() {
            continue;
        }
        let Some(equals) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let name = String::from_utf8_lossy(&entry[..equals]).into_owned();
        if name.is_empty() {
            continue;
        }
        parsed.names.insert(name.clone());
        if let Ok(value) = std::str::from_utf8(&entry[equals + 1..]) {
            parsed.pairs.push((name, value.to_string()));
        }
    }
    parsed
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
        'PATH=/login/bin\\000KLOOP_TEST_SENTINEL=1\\000ANTHROPIC_API_KEY=leak\\000PWD=/login\\000BLOCKED=cfg\\000BASH_ENV=/dev/null\\000ENV=/dev/null\\000'; \
        printf '\\001'";

    #[test]
    fn parse_env0_reads_only_the_marked_payload() {
        let bytes = b"profile banner\x01A=1\0B=2\0\x01trailing";
        assert_eq!(
            parse_env0(bytes),
            ParsedEnv {
                pairs: vec![
                    ("A".to_string(), "1".to_string()),
                    ("B".to_string(), "2".to_string())
                ],
                names: ["A".to_string(), "B".to_string()].into_iter().collect(),
            }
        );
    }

    #[test]
    fn parse_env0_ignores_entries_with_no_name_or_no_value() {
        assert_eq!(parse_env0(b"\x01\0=NOPE\0A\0\x01"), ParsedEnv::default());
    }

    /// A value kloop cannot decode still means the login shell *has* the name.
    /// Reading it as absent would make the replay drop it — and a `PATH` whose
    /// directories are not valid UTF-8 would take the user's toolchain with it.
    #[test]
    fn an_undecodable_value_keeps_its_name_but_no_value() {
        let parsed = parse_env0(b"\x01PATH=/bin\0WEIRD=\xff\xfe\0\x01");
        assert_eq!(
            parsed.pairs,
            vec![("PATH".to_string(), "/bin".to_string())],
            "only the decodable value is replayable"
        );
        assert!(parsed.names.contains("WEIRD"), "{:?}", parsed.names);

        let removed = removals(
            vec!["PATH".to_string(), "WEIRD".to_string()],
            &parsed.names,
            &[],
        );
        assert!(removed.is_empty(), "{removed:?}");
    }

    #[test]
    fn replay_unsets_what_it_dropped_and_quotes_what_it_keeps() {
        let env = ShellLoginEnv::test_fixture_with_removals(
            &[("GREETING", "it's a line\nwith both")],
            &["PYTHONHOME", "not-an-identifier"],
        );
        let script = env.replay();
        assert!(script.starts_with("unset "), "{script}");
        assert!(script.contains(" PYTHONHOME"), "{script}");
        assert!(script.contains(" OPENAI_API_KEY"), "{script}");
        assert!(
            !script.contains("not-an-identifier"),
            "a name a shell cannot parse stays out of the script: {script}"
        );
        assert!(
            script.contains(
                r#"export GREETING='it'\''s a line
with both'"#
            ),
            "quoted literally, newline and all: {script}"
        );
    }

    #[test]
    fn capture_merges_config_values_without_secrets_or_startup_hooks() {
        let shell = fake_shell("keep", &format!("#!/bin/sh\n{MARKED}\n"));
        let config = [
            ("BLOCKED".into(), "from-config".into()),
            ("PATH".into(), "/configured/bin".into()),
            ("CONFIG_ONLY".into(), "present".into()),
            ("OPENAI_API_KEY".into(), "config-secret".into()),
            ("BASH_ENV".into(), "/configured-hook".into()),
            ("ENV".into(), "/configured-hook".into()),
        ];
        let (env, warning) = ShellLoginEnv::capture(&shell, &config);
        assert!(warning.is_none());
        assert!(env.is_active());
        assert_eq!(
            env.vars,
            BTreeMap::from([
                ("BLOCKED".into(), "from-config".into()),
                ("CONFIG_ONLY".into(), "present".into()),
                ("KLOOP_TEST_SENTINEL".into(), "1".into()),
                ("PATH".into(), "/configured/bin".into()),
            ])
        );
        assert!(!env.removed.contains("CONFIG_ONLY"));
    }

    #[test]
    fn replay_file_is_private_and_lives_until_the_last_capture_is_dropped() {
        let env = ShellLoginEnv::test_fixture(&[("PATH", "/login/bin")]);
        let path = env.replay_path().unwrap().to_path_buf();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), env.replay());
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o400
        );
        let shared = env.clone();
        drop(env);
        assert!(path.exists());
        drop(shared);
        assert!(!path.exists());
    }

    #[test]
    fn a_failing_shell_capture_degrades_to_none() {
        let shell = fake_shell("fail", "#!/bin/sh\nexit 3\n");
        let (env, warning) = ShellLoginEnv::capture(&shell, &[]);
        assert!(!env.is_active());
        assert!(warning.unwrap().contains("login environment"));
    }

    #[test]
    fn config_values_do_not_turn_an_empty_capture_into_a_snapshot() {
        let shell = fake_shell("empty", "#!/bin/sh\nprintf '\\001\\001'\n");
        let config = [("PATH".into(), "/configured/bin".into())];
        let (env, warning) = ShellLoginEnv::capture(&shell, &config);
        assert_eq!(env, ShellLoginEnv::none());
        assert!(warning.is_some());
    }

    #[test]
    fn a_wedged_shell_capture_times_out() {
        let shell = fake_shell("slow", "#!/bin/sh\n/bin/sleep 30\n");
        let start = std::time::Instant::now();
        let (env, warning) =
            ShellLoginEnv::capture_within(&shell, &[], std::time::Duration::from_secs(2));
        assert!(!env.is_active());
        assert!(warning.is_some());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(6),
            "the deadline, not the shell, must end the probe"
        );
    }

    /// The timeout must end the probe's whole process group, not just the shell: a
    /// profile that left a child behind would otherwise keep running (and hold
    /// the stdout pipe).
    #[test]
    fn kill_group_ends_the_whole_group() {
        use std::os::unix::process::CommandExt as _;
        use std::process::Command;
        use std::process::Stdio;
        use std::time::Instant;

        let dir = std::env::temp_dir().join(format!("kloop-shell-env-kill-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let ready = dir.join("grandchild-running");

        // Shaped like the probe: its own group, plus a child of its own that it
        // announces, so the test knows the tree is real before it kills it.
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "/bin/sleep 30 & printf x > '{}'; /bin/sleep 30",
                ready.display()
            ))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id();

        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            ready.exists(),
            "the probe must have a child before we kill it"
        );
        assert!(group_alive(pgid), "the probe group is running");

        kill_group(pgid, &mut child);

        // The reparented grandchild is reaped a moment later, so poll rather
        // than race the reaper.
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        while group_alive(pgid) && Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            !group_alive(pgid),
            "nothing may survive in the probe's group"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Whether any process in `pgid` still exists (signal 0) — the same probe
    /// the process-tree killer uses.
    fn group_alive(pgid: u32) -> bool {
        let Some(pid) = rustix::process::Pid::from_raw(pgid as _) else {
            return false;
        };
        matches!(
            rustix::process::test_kill_process_group(pid),
            Ok(()) | Err(rustix::io::Errno::PERM)
        )
    }

    #[test]
    fn removals_are_what_the_profile_dropped() {
        let kept: BTreeSet<String> = ["PATH".to_string()].into_iter().collect();
        let removed = removals(
            vec![
                "PATH".to_string(),
                "PYTHONHOME".to_string(),
                "CONFIG_ONLY".to_string(),
                "KLOOP_SANDBOX".to_string(),
            ],
            &kept,
            &["CONFIG_ONLY".to_string()],
        );
        assert_eq!(
            removed,
            ["PYTHONHOME".to_string()].into_iter().collect(),
            "a name the capture kept, the config owns, or kloop adds is not a removal"
        );
    }

    /// Both failure paths must clean up too: a nonzero exit, and a success whose
    /// stdout a background descendant still holds open.
    #[test]
    fn both_failure_paths_leave_nothing_behind() {
        use std::time::Instant;

        for (tag, exit) in [("nonzero-exit", "exit 3"), ("held-pipe", "exit 0")] {
            let dir =
                std::env::temp_dir().join(format!("kloop-shell-env-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let ready = dir.join("child.pid");
            let shell = dir.join("shell");
            // The descendant holds our stdout, so the held-pipe case is exactly
            // the one where the reader would otherwise wait for it.
            std::fs::write(
                &shell,
                format!(
                    "#!/bin/sh\n/bin/sleep 30 & printf %s $! > '{}'; {exit}\n",
                    ready.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();

            let (env, _) =
                ShellLoginEnv::capture_within(&shell, &[], std::time::Duration::from_secs(5));
            assert!(!env.is_active(), "{tag}");

            // A freshly written executable pays a one-off exec cost on macOS, so
            // wait for the descendant to announce itself rather than assume it
            // is already there.
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while !ready.exists() && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let pid: i32 = std::fs::read_to_string(&ready)
                .unwrap_or_else(|e| panic!("{tag}: no descendant pid: {e}"))
                .trim()
                .parse()
                .unwrap();

            let deadline = Instant::now() + std::time::Duration::from_secs(2);
            while process_alive(pid) && Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            assert!(
                !process_alive(pid),
                "{tag}: a descendant outlived the capture"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// Whether `pid` still exists (signal 0); a killed child is reaped a moment
    /// later, so callers poll.
    fn process_alive(pid: i32) -> bool {
        let Some(pid) = rustix::process::Pid::from_raw(pid) else {
            return false;
        };
        matches!(
            rustix::process::test_kill_process(pid),
            Ok(()) | Err(rustix::io::Errno::PERM)
        )
    }

    #[test]
    fn replay_writes_the_removals_and_the_additions() {
        let env =
            ShellLoginEnv::test_fixture_with_removals(&[("PATH", "/login/bin")], &["PYTHONHOME"]);
        let mut spec = ProcessSpec::new("/bin/true", std::env::current_dir().unwrap());
        env.apply(&mut spec);
        let removed: Vec<String> = spec
            .env_remove
            .iter()
            .map(|name| name.to_string_lossy().into_owned())
            .collect();
        assert!(removed.contains(&"PYTHONHOME".to_string()), "{removed:?}");
        assert!(removed.contains(&"BASH_ENV".to_string()), "{removed:?}");
        assert!(removed.contains(&"ENV".to_string()), "{removed:?}");
        assert!(
            spec.env_add.contains(&("PATH".into(), "/login/bin".into())),
            "{:?}",
            spec.env_add
        );
        assert_eq!(spec.env_add, vec![("PATH".into(), "/login/bin".into())]);
    }
}
