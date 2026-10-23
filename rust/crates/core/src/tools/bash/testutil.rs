use crate::event::Event;
use crate::tools::testutil::*;
#[cfg(windows)]
use base64::Engine as _;
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

#[derive(Default)]
pub(super) struct RecordingUi {
    pub(super) events: Mutex<Vec<Event>>,
}

impl crate::agent::Ui for RecordingUi {
    fn emit(&self, event: &Event) {
        self.events.lock().unwrap().push(event.clone());
    }
}

pub(super) fn recording_ctx(tag: &str) -> (crate::tools::ToolCtx, Arc<RecordingUi>) {
    let mut ctx = test_ctx(0, tag);
    let ui = Arc::new(RecordingUi::default());
    ctx.ui = ui.clone();
    (ctx, ui)
}

#[cfg(unix)]
pub(super) struct ForegroundTree {
    root: PathBuf,
}

#[cfg(unix)]
impl ForegroundTree {
    pub(super) fn new(tag: &str) -> Self {
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

    pub(super) fn path(&self) -> &Path {
        &self.root
    }

    pub(super) fn command(&self) -> String {
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
pub(super) struct WindowsBackgroundTree {
    root: PathBuf,
}

#[cfg(windows)]
impl WindowsBackgroundTree {
    pub(super) fn new(tag: &str) -> Self {
        let sequence = FOREGROUND_TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "kloop-background-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    pub(super) fn command(&self) -> String {
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

    pub(super) fn command_with_powershell(
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

    pub(super) fn path(&self) -> &Path {
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
pub(super) async fn wait_for_windows_tree_pids(tree: &WindowsBackgroundTree) -> [u32; 2] {
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
pub(super) async fn assert_windows_processes_dead(pids: [u32; 2]) {
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
pub(super) async fn wait_for_tree_pids(tree: &ForegroundTree) -> [i32; 2] {
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
pub(super) async fn assert_processes_dead(pids: [i32; 2]) {
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
pub(super) fn bg_id(spawn_message: &str) -> String {
    spawn_message
        .split("ID: ")
        .nth(1)
        .and_then(|rest| rest.split('.').next())
        .unwrap_or_else(|| panic!("no id in: {spawn_message}"))
        .to_string()
}

#[cfg(target_os = "macos")]
pub(super) mod seatbelt {
    use super::*;
    use crate::sandbox::SandboxPolicy;
    use crate::sandbox::WritableRoot;

    /// A ctx whose bash runs sandboxed with exactly one writable root
    /// (returned canonicalized, seatbelt matches resolved paths).
    pub(in crate::tools::bash) fn sandbox_ctx(
        tag: &str,
    ) -> (crate::tools::ToolCtx, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("kloop-sbx-{tag}"));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let policy = SandboxPolicy {
            credential_home: None,
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

    /// A directory outside every writable root of `sandbox_ctx`.
    pub(in crate::tools::bash) fn outside_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-sbx-out-{tag}"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(&dir).unwrap()
    }
}
