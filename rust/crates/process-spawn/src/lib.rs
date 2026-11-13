//! Process-wide serialization for child creation, plus the owned process tree
//! shared by core's shells and the MCP stdio transport.
//!
//! Windows requires stdio handles named by `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
//! to be inheritable while `CreateProcessW` runs. Every production Windows
//! child-spawn path takes this same gate so an unrelated concurrent child cannot
//! inherit those temporary handles.
//!
//! [`spawn_process_tree`] starts a child in its own process group (Unix) or Job
//! Object (Windows) so terminating it also terminates whatever it launched;
//! [`spawn`] is the gated plain `tokio` spawn for one-shot helpers.

use std::io;

mod process_tree;

pub use process_tree::ProcessExit;
pub use process_tree::ProcessPipe;
pub use process_tree::ProcessSpec;
pub use process_tree::ProcessStdio;
pub use process_tree::ProcessTreeChild;
pub use process_tree::ProcessTreeKiller;
pub use process_tree::ProcessWriter;
pub use process_tree::spawn as spawn_process_tree;

#[cfg(windows)]
use std::sync::Mutex;
#[cfg(windows)]
use std::sync::MutexGuard;

#[cfg(windows)]
static PROCESS_CREATION: Mutex<()> = Mutex::new(());

#[cfg(windows)]
#[must_use = "dropping the guard releases the process-creation gate"]
pub struct ProcessCreationGuard {
    _guard: MutexGuard<'static, ()>,
}

#[cfg(not(windows))]
#[must_use = "dropping the guard releases the process-creation gate"]
pub struct ProcessCreationGuard;

pub fn lock() -> ProcessCreationGuard {
    #[cfg(windows)]
    {
        ProcessCreationGuard {
            _guard: PROCESS_CREATION
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        }
    }
    #[cfg(not(windows))]
    {
        ProcessCreationGuard
    }
}

pub fn spawn(command: &mut tokio::process::Command) -> io::Result<tokio::process::Child> {
    let _guard = lock();
    command.spawn()
}

/// Resolve a configured command for [`ProcessSpec`]: a bare name keeps its
/// `PATH` meaning on Unix, but the Windows spawner hands `CreateProcessW` an
/// explicit application path, and that form does neither search `PATH` nor
/// append `.exe`. Mirroring what `std::process::Command` does (current
/// directory, then `PATH`, then `.exe`) keeps `command = ["npx", …]` working.
pub fn resolve_program(program: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(program);
    #[cfg(not(windows))]
    {
        path.to_path_buf()
    }
    #[cfg(windows)]
    {
        if path.is_absolute() || program.contains(['/', '\\']) {
            return path.to_path_buf();
        }
        if let Ok(cwd) = std::env::current_dir()
            && let Some(found) = find_program_in(&cwd, program)
        {
            return found;
        }
        if let Some(dirs) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&dirs) {
                if let Some(found) = find_program_in(&dir, program) {
                    return found;
                }
            }
        }
        path.to_path_buf()
    }
}

#[cfg(windows)]
fn find_program_in(dir: &std::path::Path, program: &str) -> Option<std::path::PathBuf> {
    let direct = dir.join(program);
    if direct.is_file() {
        return Some(direct);
    }
    if std::path::Path::new(program).extension().is_none() {
        let exe = dir.join(format!("{program}.exe"));
        if exe.is_file() {
            return Some(exe);
        }
    }
    None
}

pub async fn output(command: &mut tokio::process::Command) -> io::Result<std::process::Output> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    spawn(command)?.wait_with_output().await
}

pub fn spawn_std(command: &mut std::process::Command) -> io::Result<std::process::Child> {
    let _guard = lock();
    command.spawn()
}

pub fn output_std(command: &mut std::process::Command) -> io::Result<std::process::Output> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    spawn_std(command)?.wait_with_output()
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn child_creation_waits_for_the_process_wide_gate() {
        let guard = lock();
        let (tx, rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let cmd = PathBuf::from(
                std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into()),
            )
            .join("System32")
            .join("cmd.exe");
            let mut command = std::process::Command::new(cmd);
            command.args(["/D", "/C", "exit 0"]);
            let result = spawn_std(&mut command).and_then(|mut child| child.wait());
            tx.send(result).unwrap();
        });

        assert!(
            matches!(
                rx.recv_timeout(Duration::from_millis(50)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ),
            "concurrent process creation bypassed the shared gate"
        );
        drop(guard);
        assert!(
            rx.recv_timeout(Duration::from_secs(5))
                .unwrap()
                .unwrap()
                .success()
        );
        worker.join().unwrap();
    }
}
