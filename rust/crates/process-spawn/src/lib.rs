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
/// explicit application path, and that form neither searches `PATH` nor
/// completes a relative path beyond the current directory. Mirroring what
/// `std::process::Command` does keeps `command = ["npx", …]` and
/// `command = [".\\tools\\server.exe", …]` working.
///
/// `cwd` is the directory the child will run in. `path_env` is the `PATH` the
/// child will actually get (a configured environment may replace the inherited
/// one, e.g. a virtualenv); `None` means "inherit".
pub fn resolve_program(
    program: &str,
    cwd: &std::path::Path,
    path_env: Option<&std::ffi::OsStr>,
) -> std::path::PathBuf {
    #[cfg(not(windows))]
    {
        let _ = (cwd, path_env);
        std::path::PathBuf::from(program)
    }
    #[cfg(windows)]
    {
        let search = match path_env {
            Some(value) => Some(value.to_os_string()),
            None => std::env::var_os("PATH"),
        };
        let dirs = search
            .map(|dirs| std::env::split_paths(&dirs).collect())
            .unwrap_or_default();
        resolve_relative(program, cwd, dirs)
    }
}

/// The Windows resolution rule, kept free of platform APIs so it can be tested
/// on any host. `dirs` are the `PATH` entries in search order.
#[cfg(any(windows, test))]
fn resolve_relative(
    program: &str,
    cwd: &std::path::Path,
    dirs: Vec<std::path::PathBuf>,
) -> std::path::PathBuf {
    let path = std::path::Path::new(program);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    // A relative path with separators is relative to the child's working
    // directory, not to whatever directory happens to be current when the
    // launcher runs.
    if program.contains(['/', '\\']) {
        return cwd.join(path);
    }
    if let Some(found) = find_program_in(cwd, program) {
        return found;
    }
    for dir in dirs {
        if let Some(found) = find_program_in(&dir, program) {
            return found;
        }
    }
    path.to_path_buf()
}

#[cfg(any(windows, test))]
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

#[cfg(test)]
mod resolve_tests {
    use super::*;
    use std::path::Path;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-resolve-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_relative_path_is_completed_against_the_child_cwd() {
        let cwd = Path::new("/work/dir");
        assert_eq!(
            resolve_relative("./tools/server.exe", cwd, Vec::new()),
            cwd.join("./tools/server.exe")
        );
        assert_eq!(
            resolve_relative("tools\\server.exe", cwd, Vec::new()),
            cwd.join("tools\\server.exe")
        );
    }

    #[test]
    fn the_cwd_is_searched_before_path() {
        let cwd = temp_dir("cwd-first");
        let other = temp_dir("cwd-first-other");
        std::fs::write(cwd.join("server.exe"), b"").unwrap();
        std::fs::write(other.join("server.exe"), b"").unwrap();
        assert_eq!(
            resolve_relative("server.exe", &cwd, vec![other.clone()]),
            cwd.join("server.exe")
        );
        std::fs::remove_dir_all(&cwd).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn path_entries_resolve_bare_names_and_add_exe() {
        let cwd = temp_dir("path-search");
        let bin = temp_dir("path-search-bin");
        std::fs::write(bin.join("server.exe"), b"").unwrap();
        assert_eq!(
            resolve_relative("server", &cwd, vec![bin.clone()]),
            bin.join("server.exe")
        );
        // An extension is never doubled.
        assert_eq!(
            resolve_relative("server.exe", &cwd, vec![bin.clone()]),
            bin.join("server.exe")
        );
        std::fs::remove_dir_all(&cwd).unwrap();
        std::fs::remove_dir_all(&bin).unwrap();
    }

    #[test]
    fn an_unresolvable_name_is_left_for_the_spawn_error() {
        let cwd = temp_dir("unresolved");
        assert_eq!(
            resolve_relative("missing-server", &cwd, Vec::new()),
            PathBuf::from("missing-server")
        );
        std::fs::remove_dir_all(&cwd).unwrap();
    }
}
