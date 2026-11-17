//! What the two session-wide background registries share: the activity watch
//! that wakes waiters, the close-and-reap shutdown, and the `BackgroundTask`
//! projection they publish to the frontend.
//!
//! [`crate::tools::BackgroundShells`] and [`BackgroundExecutions`] keep their own
//! state machines — a shell kills a process tree, an execution cancels a worker
//! and aborts it — but a waiter, a shutdown, and a UI row work the same way for
//! both, so they live here once.
//!
//! [`BackgroundExecutions`]: crate::tools::background_executions::BackgroundExecutions

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;

use super::background_executions::ExecutionStatus;
use crate::agent::Ui;
use crate::event::BackgroundTask;
use crate::event::BackgroundTaskKind;
use crate::event::BackgroundTaskStatus;
use crate::event::Event;

/// Grace a hard-killed shell or worker gets to publish its terminal state
/// before shutdown reports it as still running.
const FORCE_GRACE: Duration = Duration::from_secs(1);

/// What one registry still owes, read in a single pass under its lock.
/// `active` counts everything a waiter must not see as zero — running,
/// stopping, and finished-but-undelivered — while `delivering` is the finished
/// part of it. One read, one lock: two separate counts could report more
/// deliveries than active items after an item finished in between.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ActiveCount {
    pub(crate) active: usize,
    pub(crate) delivering: usize,
}

/// Wait until `probe` reports done, re-reading it on every activity bump and
/// returning the last reading.
///
/// Subscribing before the read is what makes the pair race-free: a transition
/// that lands between the read and the wait advances the receiver instead of
/// becoming a lost wakeup. The value comes back so callers that report what
/// they saw — a status line, a straggler count — do not read the table twice.
pub(super) async fn wait_until<T>(
    activity: &mut watch::Receiver<u64>,
    deadline: tokio::time::Instant,
    mut probe: impl FnMut() -> Option<T>,
    mut done: impl FnMut(&T) -> bool,
) -> Option<T> {
    loop {
        let value = probe()?;
        if done(&value) {
            return Some(value);
        }
        match tokio::time::timeout_at(deadline, activity.changed()).await {
            Ok(Ok(())) => {}
            // The deadline passed, or the registry is gone and no further bump
            // can arrive: answer with what is visible now.
            Ok(Err(_)) | Err(_) => return probe(),
        }
    }
}

/// Close a registry the way both do: `cancel` marks the table closed and asks
/// every active child to stop, their supervisors get `timeout` to publish, then
/// `force` hard-kills or aborts whatever is left for [`FORCE_GRACE`] more.
/// Returns the number still active when the second window closed.
pub(super) async fn shutdown(
    activity: &watch::Sender<u64>,
    timeout: Duration,
    cancel: impl FnOnce(),
    force: impl FnOnce(),
    active: impl Fn() -> usize,
) -> usize {
    let mut wake = activity.subscribe();
    cancel();
    let idle = wait_until(
        &mut wake,
        tokio::time::Instant::now() + timeout,
        || Some(active()),
        |count| *count == 0,
    )
    .await
    .unwrap_or(0);
    if idle == 0 {
        return 0;
    }
    force();
    wait_until(
        &mut wake,
        tokio::time::Instant::now() + FORCE_GRACE,
        || Some(active()),
        |count| *count == 0,
    )
    .await
    .unwrap_or(0)
}

/// Publish one lifecycle row for a background agent, program, or Workflow. All
/// three report through this, so a status the frontend sees cannot mean
/// different things per kind.
#[allow(clippy::too_many_arguments)]
pub(super) fn emit(
    ui: &Arc<dyn Ui>,
    id: &str,
    run_id: Option<&str>,
    kind: BackgroundTaskKind,
    description: &str,
    status: ExecutionStatus,
    output_path: Option<String>,
    detail: Option<String>,
) {
    let status = match status {
        ExecutionStatus::Running => BackgroundTaskStatus::Running,
        ExecutionStatus::Completed | ExecutionStatus::MaxRounds => BackgroundTaskStatus::Completed,
        ExecutionStatus::Failed => BackgroundTaskStatus::Failed,
        ExecutionStatus::Aborted => BackgroundTaskStatus::Cancelled,
    };
    ui.emit(&Event::BackgroundTaskUpdated(BackgroundTask {
        id: id.to_string(),
        run_id: run_id.map(str::to_string),
        kind,
        description: description.to_string(),
        command: None,
        status,
        output_path,
        detail,
    }));
}

/// The extra line a terminal row carries. The status itself already says
/// completed, failed, or cancelled, so the detail only earns its place where the
/// status cannot say what happened: a round-limited agent reports as completed,
/// and a stop is what a preserved worktree location is appended to.
pub(super) fn status_detail(status: ExecutionStatus) -> Option<String> {
    match status {
        ExecutionStatus::Running | ExecutionStatus::Completed | ExecutionStatus::Failed => None,
        ExecutionStatus::MaxRounds => Some("stopped at round limit".into()),
        ExecutionStatus::Aborted => Some("stopped".into()),
    }
}
