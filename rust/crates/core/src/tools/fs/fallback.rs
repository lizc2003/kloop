//! `fallback`: the platform half of the file tools — descriptor-relative opens, the
//! identity and hard-link probes, and the temp/rename primitives the atomic
//! replace is built from. What stays in the parent is tool semantics: what
//! `edit_file` qualifies on, how a page is numbered, what a mutation means.
//!
//! The three siblings are one interface, reached through the parent's
//! `mod platform`, so nothing above this line carries a `#[cfg]`.

use std::ffi::OsStr;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;

use super::*;

pub(super) fn open_read_target(
    _parent: &std::fs::File,
    _parent_path: &Path,
    _leaf: &OsStr,
    _display_path: &str,
) -> Result<std::fs::File> {
    bail!("safe file reads are unsupported on this platform")
}

pub(super) fn open_or_create_child_directory(
    _parent: &std::fs::File,
    _name: &OsStr,
    _tool: &str,
    _display_path: &str,
) -> Result<(std::fs::File, bool)> {
    bail!("safe recursive file mutation is unsupported on this platform")
}

pub(super) fn open_parent_directory(
    _path: &Path,
    _tool: &str,
    _display_path: &str,
) -> Result<std::fs::File> {
    bail!("safe file mutation is unsupported on this platform")
}

pub(super) fn has_multiple_hard_links(_file: &std::fs::File) -> Result<bool> {
    Ok(false)
}

pub(super) fn file_identity(_file: &std::fs::File) -> Result<FileIdentity> {
    bail!("safe file mutation is unsupported on this platform")
}

pub(super) fn open_regular_target(
    _parent: &std::fs::File,
    _parent_path: &Path,
    _leaf: &OsStr,
    _tool: &str,
    _display_path: &str,
) -> Result<Option<TargetFile>> {
    bail!("safe file mutation is unsupported on this platform")
}

pub(super) fn sync_parent(_parent: &std::fs::File, _tool: &str, _display_path: &str) -> Result<()> {
    bail!("safe file mutation is unsupported on this platform")
}

/// Nothing was created, so there is nothing to clean up.
pub(super) fn cleanup_created_directories(created: Vec<CreatedDirectory>) {
    drop(created);
}

/// The atomic replace is built from these three, and a platform without them
/// cannot offer it. Each carries the sentence the whole operation used to refuse
/// with, so the parent needs no branch of its own.
pub(super) fn create_temp(_parent: &std::fs::File, _name: &OsStr) -> Result<std::fs::File> {
    bail!("safe file mutation is unsupported on this platform")
}

pub(super) fn commit_rename(
    _temp: &std::fs::File,
    _parent: &std::fs::File,
    _name: &OsStr,
    _leaf: &OsStr,
    _replace_existing: bool,
) -> Result<()> {
    bail!("safe file mutation is unsupported on this platform")
}

pub(super) fn discard_temp(_temp: &std::fs::File, _parent: &std::fs::File, _name: &OsStr) {}

/// Nothing here reads a path component the way Windows does, so a component that
/// already passed the shared checks is accepted.
pub(super) fn reject_unsafe_component(
    _component: &OsStr,
    _tool: &str,
    _display_path: &str,
) -> Result<()> {
    Ok(())
}
