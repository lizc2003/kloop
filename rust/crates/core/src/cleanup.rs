//! Cleanup guards for resources created before a step that can fail or be
//! cancelled.
//!
//! Tool dispatch may drop a tool future at any await once its turn is
//! cancelled (plan 249). A resource created before that point is released by
//! this guard's `Drop`, not by code after the next `?`.

use std::path::PathBuf;

/// Removes a file this process created unless [`disarm`](Self::disarm) says
/// the operation finished. Synchronous on purpose: removal needs no executor,
/// so it also runs while a dropped future is being torn down.
#[derive(Debug)]
pub struct RemoveFileOnDrop {
    path: Option<PathBuf>,
}

impl RemoveFileOnDrop {
    pub fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// Keep the file: whatever now owns it has committed.
    pub fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for RemoveFileOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}
