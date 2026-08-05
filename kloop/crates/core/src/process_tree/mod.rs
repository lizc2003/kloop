use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use tokio::io::AsyncRead;

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(unix)]
use unix as platform;
#[cfg(windows)]
use windows as platform;

pub(crate) type ProcessPipe = Pin<Box<dyn AsyncRead + Send>>;

pub(crate) enum ProcessStdio {
    Null,
    Pipe,
    File(File),
}

pub(crate) struct ProcessSpec {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub env_add: Vec<(OsString, OsString)>,
    pub env_remove: Vec<OsString>,
    pub stdin: ProcessStdio,
    pub stdout: ProcessStdio,
    pub stderr: ProcessStdio,
    #[cfg(windows)]
    windows_debug_descendants: bool,
}

impl ProcessSpec {
    pub(crate) fn new(executable: impl Into<PathBuf>, cwd: impl Into<PathBuf>) -> Self {
        Self {
            executable: executable.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env_add: Vec::new(),
            env_remove: Vec::new(),
            stdin: ProcessStdio::Null,
            stdout: ProcessStdio::Pipe,
            stderr: ProcessStdio::Pipe,
            #[cfg(windows)]
            windows_debug_descendants: false,
        }
    }

    pub(crate) fn arg(&mut self, arg: impl Into<OsString>) {
        self.args.push(arg.into());
    }

    pub(crate) fn env(&mut self, name: impl Into<OsString>, value: impl Into<OsString>) {
        self.env_add.push((name.into(), value.into()));
    }

    pub(crate) fn env_remove(&mut self, name: impl Into<OsString>) {
        self.env_remove.push(name.into());
    }

    #[cfg(windows)]
    pub(crate) fn require_windows_descendant_debugging(&mut self) {
        self.windows_debug_descendants = true;
    }

    #[cfg(all(test, windows))]
    pub(crate) fn windows_debug_descendants(&self) -> bool {
        self.windows_debug_descendants
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ProcessExit {
    code: Option<i32>,
    success: bool,
}

impl ProcessExit {
    pub(crate) fn code(self) -> Option<i32> {
        self.code
    }

    pub(crate) fn success(self) -> bool {
        self.success
    }
}

pub(crate) struct ProcessTreeChild {
    inner: platform::Child,
}

#[derive(Clone)]
pub(crate) struct ProcessTreeKiller {
    inner: platform::Killer,
}

pub(crate) fn spawn(spec: ProcessSpec) -> io::Result<ProcessTreeChild> {
    platform::spawn(spec).map(|inner| ProcessTreeChild { inner })
}

impl ProcessTreeChild {
    pub(crate) fn take_stdout(&mut self) -> Option<ProcessPipe> {
        self.inner.take_stdout()
    }

    pub(crate) fn take_stderr(&mut self) -> Option<ProcessPipe> {
        self.inner.take_stderr()
    }

    pub(crate) fn killer(&self) -> ProcessTreeKiller {
        ProcessTreeKiller {
            inner: self.inner.killer(),
        }
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ProcessExit> {
        self.inner.wait().await
    }

    pub(crate) async fn cleanup_after_exit(&mut self, timeout: Duration) -> io::Result<()> {
        self.inner.cleanup_after_exit(timeout).await
    }

    pub(crate) async fn terminate_and_wait(
        &mut self,
        timeout: Duration,
    ) -> io::Result<ProcessExit> {
        self.inner.terminate_and_wait(timeout).await
    }
}

impl ProcessTreeKiller {
    pub(crate) fn terminate(&self) -> io::Result<()> {
        self.inner.terminate()
    }

    #[cfg(test)]
    pub(crate) fn is_alive(&self) -> io::Result<bool> {
        self.inner.is_alive()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn root_exit_is_reaped_and_tree_becomes_empty() {
        let mut spec = ProcessSpec::new("/bin/sh", std::env::current_dir().unwrap());
        spec.arg("-c");
        spec.arg("exit 7");
        let mut child = spawn(spec).unwrap();
        let killer = child.killer();
        let exit = child.wait().await.unwrap();
        assert_eq!(exit.code(), Some(7));
        assert!(!exit.success());
        child
            .cleanup_after_exit(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!killer.is_alive().unwrap());
    }

    #[tokio::test]
    async fn repeated_terminate_is_idempotent() {
        let mut spec = ProcessSpec::new("/bin/sh", std::env::current_dir().unwrap());
        spec.arg("-c");
        spec.arg("sleep 30");
        let mut child = spawn(spec).unwrap();
        let killer = child.killer();
        killer.terminate().unwrap();
        killer.terminate().unwrap();
        child
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!killer.is_alive().unwrap());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    use std::process::Command;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use tokio::io::AsyncReadExt as _;

    const HELPER_ENV: &str = "KLOOP_PROCESS_TREE_WINDOWS_HELPER";
    const HELPER_MARKER_ENV: &str = "KLOOP_PROCESS_TREE_WINDOWS_MARKER";
    const HELPER_ENV_ROUNDTRIP_MARKER: &str = "KLOOP_PROCESS_TREE_WINDOWS_ENV_ROUNDTRIP_MARKER";
    static TEST_SEQUENCE: AtomicUsize = AtomicUsize::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "kloop-process-tree-{tag}-{}-{}",
                std::process::id(),
                TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn cmd_path() -> PathBuf {
        PathBuf::from(std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()))
            .join("System32")
            .join("cmd.exe")
    }

    fn cmd_spec(command: &str) -> ProcessSpec {
        let mut spec = ProcessSpec::new(cmd_path(), std::env::current_dir().unwrap());
        spec.arg("/D");
        spec.arg("/S");
        spec.arg("/C");
        spec.arg(command);
        spec
    }

    fn roundtrip_env_name() -> OsString {
        use std::os::windows::ffi::OsStringExt as _;

        OsString::from_wide(&[
            b'K' as u16,
            b'L' as u16,
            b'O' as u16,
            b'O' as u16,
            b'P' as u16,
            b'_' as u16,
            0xd800,
        ])
    }

    fn roundtrip_env_value() -> OsString {
        use std::os::windows::ffi::OsStringExt as _;

        OsString::from_wide(&[0xe000, 0xd83d, 0xde80])
    }

    fn handle_count() -> u32 {
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        use windows_sys::Win32::System::Threading::GetProcessHandleCount;

        let mut count = 0;
        let ok = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
        assert_ne!(ok, 0);
        count
    }

    fn process_has_exited(pid: u32) -> bool {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::Foundation::WAIT_OBJECT_0;
        use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
        use windows_sys::Win32::System::Threading::OpenProcess;
        use windows_sys::Win32::System::Threading::WaitForSingleObject;
        use windows_sys::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION;

        let handle =
            unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, 0, pid) };
        if handle == 0 {
            return true;
        }
        let exited = unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0;
        unsafe {
            CloseHandle(handle);
        }
        exited
    }

    async fn wait_for_process_exit(pid: u32) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !process_has_exited(pid) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("process {pid} survived containment cleanup"));
    }

    #[test]
    fn windows_descendant_helper() {
        if std::env::var_os(HELPER_ENV).is_none() {
            return;
        }
        let marker = PathBuf::from(std::env::var_os(HELPER_MARKER_ENV).unwrap());
        let child = Command::new(cmd_path())
            .args(["/D", "/C", "ping 127.0.0.1 -n 30 > NUL"])
            .spawn()
            .unwrap();
        std::fs::write(marker, child.id().to_string()).unwrap();
        drop(child);
    }

    #[test]
    fn windows_environment_helper() {
        let Some(marker) = std::env::var_os(HELPER_ENV_ROUNDTRIP_MARKER) else {
            return;
        };
        assert_eq!(
            std::env::var_os(roundtrip_env_name()),
            Some(roundtrip_env_value())
        );
        std::fs::write(marker, b"ok").unwrap();
    }

    #[tokio::test]
    async fn environment_keys_and_values_round_trip_without_loss() {
        let dir = TestDir::new("environment");
        let marker = dir.0.join("roundtrip.txt");
        let mut spec = ProcessSpec::new(std::env::current_exe().unwrap(), &dir.0);
        spec.arg("--exact");
        spec.arg("process_tree::windows_tests::windows_environment_helper");
        spec.arg("--nocapture");
        spec.env(HELPER_ENV_ROUNDTRIP_MARKER, marker.as_os_str());
        spec.env(roundtrip_env_name(), roundtrip_env_value());
        let mut child = spawn(spec).unwrap();
        let exit = child.wait().await.unwrap();
        child
            .cleanup_after_exit(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(exit.success());
        assert_eq!(std::fs::read(marker).unwrap(), b"ok");
    }

    #[test]
    fn suspended_spawn_faults_never_run_user_code() {
        let dir = TestDir::new("suspended");
        for (index, fault) in [
            super::windows::SpawnFault::BeforeAssign,
            super::windows::SpawnFault::BeforeResume,
        ]
        .into_iter()
        .enumerate()
        {
            let marker = dir.0.join(format!("fault-{index}.txt"));
            let command = format!("echo ran>\"{}\"", marker.display());
            let error = super::windows::spawn_with_fault(cmd_spec(&command), fault)
                .err()
                .expect("fault must fail")
                .to_string();
            assert!(error.contains("injected failure"), "{error}");
            assert!(!marker.exists(), "suspended user code wrote {marker:?}");
        }
    }

    #[test]
    fn real_job_assignment_failure_reaps_suspended_root_without_running_it() {
        let dir = TestDir::new("assign-failure");
        let marker = dir.0.join("assign-failure.txt");
        let command = format!("echo ran>\"{}\"", marker.display());
        let error = super::windows::spawn_with_fault(
            cmd_spec(&command),
            super::windows::SpawnFault::RealAssignFailure,
        )
        .err()
        .expect("active-process limit must reject the second Job member");
        assert!(
            error.raw_os_error().is_some(),
            "failure must come from AssignProcessToJobObject: {error}"
        );
        assert!(!marker.exists(), "suspended user code wrote {marker:?}");
    }

    #[tokio::test]
    async fn leader_exit_cleans_descendant_and_releases_inherited_pipes() {
        let dir = TestDir::new("descendant");
        let marker = dir.0.join("descendant.pid");
        let mut spec = ProcessSpec::new(std::env::current_exe().unwrap(), &dir.0);
        spec.arg("--exact");
        spec.arg("process_tree::windows_tests::windows_descendant_helper");
        spec.arg("--nocapture");
        spec.env(HELPER_ENV, "1");
        spec.env(HELPER_MARKER_ENV, marker.as_os_str());
        let mut child = spawn(spec).unwrap();
        let killer = child.killer();
        let mut stdout = child.take_stdout().unwrap();
        let mut stderr = child.take_stderr().unwrap();
        let stdout_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stdout.read_to_end(&mut bytes).await.map(|_| bytes)
        });
        let stderr_reader = tokio::spawn(async move {
            let mut bytes = Vec::new();
            stderr.read_to_end(&mut bytes).await.map(|_| bytes)
        });

        let exit = child.wait().await.unwrap();
        assert!(exit.success());
        assert!(marker.is_file());
        assert!(
            killer.is_alive().unwrap(),
            "descendant should still be a Job member"
        );
        child
            .cleanup_after_exit(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!killer.is_alive().unwrap());
        tokio::time::timeout(Duration::from_secs(2), stdout_reader)
            .await
            .expect("stdout reader reached EOF")
            .unwrap()
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), stderr_reader)
            .await
            .expect("stderr reader reached EOF")
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn debug_gate_recaptures_a_silent_breakaway_descendant() {
        let dir = TestDir::new("debug-breakaway");
        let marker = dir.0.join("descendant.pid");
        let mut spec = ProcessSpec::new(std::env::current_exe().unwrap(), &dir.0);
        spec.arg("--exact");
        spec.arg("process_tree::windows_tests::windows_descendant_helper");
        spec.arg("--nocapture");
        spec.env(HELPER_ENV, "1");
        spec.env(HELPER_MARKER_ENV, marker.as_os_str());
        spec.require_windows_descendant_debugging();
        let mut child = super::windows::spawn_with_fault(
            spec,
            super::windows::SpawnFault::DebugDescendantBreakaway,
        )
        .unwrap();
        let killer = child.killer();

        assert!(child.wait().await.unwrap().success());
        let pid: u32 = std::fs::read_to_string(&marker)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        assert!(
            !process_has_exited(pid),
            "breakaway helper exited too early"
        );
        assert!(
            killer.is_alive().unwrap(),
            "debug gate must place the breakaway process in a containment Job"
        );
        child
            .cleanup_after_exit(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!killer.is_alive().unwrap());
        wait_for_process_exit(pid).await;
    }

    #[tokio::test]
    async fn repeated_terminate_reaps_the_job() {
        let mut child = spawn(cmd_spec("ping 127.0.0.1 -n 30 > NUL")).unwrap();
        let killer = child.killer();
        killer.terminate().unwrap();
        killer.terminate().unwrap();
        child
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
        assert!(!killer.is_alive().unwrap());
    }

    #[tokio::test]
    async fn dropping_wait_future_and_child_terminates_the_job() {
        let mut child = spawn(cmd_spec("ping 127.0.0.1 -n 30 > NUL")).unwrap();
        let killer = child.killer();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), child.wait())
                .await
                .is_err(),
            "root unexpectedly exited before the wait future was dropped"
        );
        drop(child);
        tokio::time::timeout(Duration::from_secs(2), async {
            while killer.is_alive().unwrap() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("Child::drop terminated every Job member");
    }

    #[tokio::test]
    async fn cancelled_waits_reuse_one_process_waiter() {
        let mut child = spawn(cmd_spec("ping 127.0.0.1 -n 30 > NUL")).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(20), child.wait())
                .await
                .is_err(),
            "root unexpectedly exited before wait cancellation"
        );
        let waiter_id = child
            .inner
            .waiter_id()
            .expect("cancelled wait keeps its blocking process waiter");
        for _ in 0..16 {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), child.wait())
                    .await
                    .is_err(),
                "root unexpectedly exited before repeated wait cancellation"
            );
            assert_eq!(
                child.inner.waiter_id(),
                Some(waiter_id),
                "wait cancellation must reuse one duplicated process handle"
            );
        }
        child
            .terminate_and_wait(Duration::from_secs(2))
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn process_job_thread_and_pipe_handles_do_not_grow_linearly() {
        let before = handle_count();
        for _ in 0..32 {
            let mut child = spawn(cmd_spec("exit 0")).unwrap();
            assert!(child.wait().await.unwrap().success());
            child
                .cleanup_after_exit(Duration::from_secs(2))
                .await
                .unwrap();
        }
        let after = handle_count();
        assert!(
            after <= before.saturating_add(4),
            "handle count grew from {before} to {after}"
        );
    }

    #[tokio::test]
    async fn debugged_process_job_thread_and_pipe_handles_do_not_grow_linearly() {
        let mut warmup = cmd_spec("exit 0");
        warmup.require_windows_descendant_debugging();
        let mut child = spawn(warmup).unwrap();
        assert!(child.wait().await.unwrap().success());
        child
            .cleanup_after_exit(Duration::from_secs(2))
            .await
            .unwrap();

        let before = handle_count();
        for _ in 0..16 {
            let mut spec = cmd_spec("exit 0");
            spec.require_windows_descendant_debugging();
            let mut child = spawn(spec).unwrap();
            assert!(child.wait().await.unwrap().success());
            child
                .cleanup_after_exit(Duration::from_secs(2))
                .await
                .unwrap();
        }
        let after = handle_count();
        assert!(
            after <= before.saturating_add(8),
            "debugged spawns grew the handle count from {before} to {after}"
        );
    }
}
