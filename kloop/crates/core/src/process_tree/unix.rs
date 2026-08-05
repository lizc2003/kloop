use std::io;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;

use super::ProcessExit;
use super::ProcessPipe;
use super::ProcessSpec;
use super::ProcessStdio;

#[derive(Clone)]
pub(super) struct Killer {
    tree: Arc<Tree>,
}

struct Tree {
    pid: u32,
    terminated: AtomicBool,
}

pub(super) struct Child {
    child: tokio::process::Child,
    killer: Killer,
    stdout: Option<ProcessPipe>,
    stderr: Option<ProcessPipe>,
}

pub(super) fn spawn(spec: ProcessSpec) -> io::Result<Child> {
    let mut command = tokio::process::Command::new(&spec.executable);
    command
        .args(spec.args)
        .current_dir(spec.cwd)
        .kill_on_drop(true);
    for (name, value) in spec.env_add {
        command.env(name, value);
    }
    for name in spec.env_remove {
        command.env_remove(name);
    }
    command
        .stdin(to_stdio(spec.stdin)?)
        .stdout(to_stdio(spec.stdout)?)
        .stderr(to_stdio(spec.stderr)?);
    command.process_group(0);

    let mut child = command.spawn()?;
    let pid = child
        .id()
        .ok_or_else(|| io::Error::other("spawned process has no pid"))?;
    let stdout = child
        .stdout
        .take()
        .map(|pipe| Box::pin(pipe) as ProcessPipe);
    let stderr = child
        .stderr
        .take()
        .map(|pipe| Box::pin(pipe) as ProcessPipe);
    Ok(Child {
        child,
        killer: Killer {
            tree: Arc::new(Tree {
                pid,
                terminated: AtomicBool::new(false),
            }),
        },
        stdout,
        stderr,
    })
}

fn to_stdio(stdio: ProcessStdio) -> io::Result<Stdio> {
    Ok(match stdio {
        ProcessStdio::Null => Stdio::null(),
        ProcessStdio::Pipe => Stdio::piped(),
        ProcessStdio::File(file) => Stdio::from(file),
    })
}

impl Child {
    pub(super) fn take_stdout(&mut self) -> Option<ProcessPipe> {
        self.stdout.take()
    }

    pub(super) fn take_stderr(&mut self) -> Option<ProcessPipe> {
        self.stderr.take()
    }

    pub(super) fn killer(&self) -> Killer {
        self.killer.clone()
    }

    pub(super) async fn wait(&mut self) -> io::Result<ProcessExit> {
        let status = self.child.wait().await?;
        Ok(ProcessExit {
            code: status.code(),
            success: status.success(),
        })
    }

    pub(super) async fn cleanup_after_exit(&mut self, timeout: Duration) -> io::Result<()> {
        if self.killer.is_alive()? {
            self.killer.terminate()?;
        }
        wait_tree_empty(&self.killer, timeout).await
    }

    pub(super) async fn terminate_and_wait(
        &mut self,
        timeout: Duration,
    ) -> io::Result<ProcessExit> {
        let group_result = self.killer.terminate();
        let direct_result = self.child.start_kill();
        let status = tokio::time::timeout(timeout, self.wait())
            .await
            .map_err(|_| {
                io::Error::new(io::ErrorKind::TimedOut, "timed out reaping process root")
            })??;
        wait_tree_empty(&self.killer, timeout).await?;
        if let Err(error) = group_result {
            direct_result?;
            return Err(error);
        }
        Ok(status)
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        let _ = self.killer.terminate();
        let _ = self.child.start_kill();
    }
}

impl Killer {
    pub(super) fn terminate(&self) -> io::Result<()> {
        if self.tree.terminated.load(Ordering::Acquire) {
            return Ok(());
        }
        let pid = rustix::process::Pid::from_raw(self.tree.pid as _)
            .context("process group id must be non-zero")
            .map_err(io::Error::other)?;
        match rustix::process::kill_process_group(pid, rustix::process::Signal::Kill) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => {
                self.tree.terminated.store(true, Ordering::Release);
                Ok(())
            }
            Err(rustix::io::Errno::PERM) if self.tree.terminated.load(Ordering::Acquire) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn is_alive(&self) -> io::Result<bool> {
        let pid = rustix::process::Pid::from_raw(self.tree.pid as _)
            .context("process group id must be non-zero")
            .map_err(io::Error::other)?;
        match rustix::process::test_kill_process_group(pid) {
            Ok(()) | Err(rustix::io::Errno::PERM) => Ok(true),
            Err(rustix::io::Errno::SRCH) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }
}

async fn wait_tree_empty(killer: &Killer, timeout: Duration) -> io::Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while killer.is_alive()? {
        if tokio::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("process group {} did not exit", killer.tree.pid),
            ));
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    killer.tree.terminated.store(true, Ordering::Release);
    Ok(())
}
