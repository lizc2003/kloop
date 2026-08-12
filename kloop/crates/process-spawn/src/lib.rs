//! Process-wide serialization for child creation.
//!
//! Windows requires stdio handles named by `PROC_THREAD_ATTRIBUTE_HANDLE_LIST`
//! to be inheritable while `CreateProcessW` runs. Every production Windows
//! child-spawn path takes this same gate so an unrelated concurrent child cannot
//! inherit those temporary handles.

use std::io;

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
