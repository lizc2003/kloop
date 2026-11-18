//! Session-level lifecycle shared by every front-end.
//!
//! Ending a session is not the same as ending a turn: the detached work the
//! session started has to stop, and its worktree has to be left the way the
//! shell would find it. The order and the report are the same whichever
//! front-end is holding the session, so they live here instead of being
//! spelled out again beside each event loop.

use std::sync::Arc;

use crate::agent::Ui;
use crate::config::Config;

/// What stopping a session did, in the terms a front-end reports it.
pub struct ShutdownReport {
    /// Detached work still running when the shutdown began.
    pub running: usize,
    /// Work that missed the bounded reap deadline.
    pub missed: usize,
    /// The kept-worktree note, when the session left a tree behind.
    pub worktree: Option<String>,
}

/// Stop everything `cfg` still runs, then leave its worktree. Background work
/// goes first: it may still write through the session, and the worktree must
/// only be retired once nothing can.
pub async fn shutdown_session(cfg: &Config, ui: &Arc<dyn Ui>) -> ShutdownReport {
    let running =
        cfg.background_executions.running_count() + cfg.background_shells.running_count();
    let missed = cfg.shutdown_background_work(ui).await;
    ShutdownReport {
        running,
        missed,
        worktree: crate::worktree::finish_active(cfg).await,
    }
}
