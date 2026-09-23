//! `unix`: the platform half of the file tools — descriptor-relative opens, the
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
    parent: &std::fs::File,
    _parent_path: &Path,
    leaf: &OsStr,
    display_path: &str,
) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = rustix::fs::openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    )
    .with_context(|| format!("read_file: cannot read {display_path}"))?;
    let file = std::fs::File::from(fd);
    if !file
        .metadata()
        .with_context(|| format!("read_file: cannot inspect {display_path}"))?
        .is_file()
    {
        bail!("read_file: cannot read {display_path}: not a regular file");
    }
    Ok(file)
}

pub(super) fn open_or_create_child_directory(
    parent: &std::fs::File,
    name: &OsStr,
    tool: &str,
    display_path: &str,
) -> Result<(std::fs::File, bool)> {
    use rustix::fs::Mode;

    let mode = Mode::RUSR
        | Mode::WUSR
        | Mode::XUSR
        | Mode::RGRP
        | Mode::WGRP
        | Mode::XGRP
        | Mode::ROTH
        | Mode::WOTH
        | Mode::XOTH;
    let created = match rustix::fs::mkdirat(parent, name, mode) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(error) => {
            return Err(std::io::Error::from_raw_os_error(error.raw_os_error())).with_context(
                || format!("{tool}: cannot create parent directory for {display_path}"),
            );
        }
    };
    let child = open_child_directory(parent, name, tool, display_path)?;
    Ok((child, created))
}

pub(super) fn open_child_directory(
    parent: &std::fs::File,
    name: &OsStr,
    tool: &str,
    display_path: &str,
) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| {
        format!("{tool}: parent component is not a safe directory for {display_path}")
    })?;
    Ok(std::fs::File::from(fd))
}

pub(super) fn open_parent_directory(
    path: &Path,
    tool: &str,
    display_path: &str,
) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("{tool}: cannot open parent directory for {display_path}"))?;
    Ok(std::fs::File::from(fd))
}

pub(super) fn has_multiple_hard_links(file: &std::fs::File) -> Result<bool> {
    use std::os::unix::fs::MetadataExt as _;

    Ok(file.metadata()?.nlink() > 1)
}

pub(super) fn file_identity(file: &std::fs::File) -> Result<FileIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let metadata = file.metadata()?;
    Ok(FileIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(super) fn open_regular_target(
    parent: &std::fs::File,
    _parent_path: &Path,
    leaf: &OsStr,
    tool: &str,
    display_path: &str,
) -> Result<Option<TargetFile>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::openat(
        parent,
        leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            bail!("{tool}: refuses to replace symbolic link {display_path}")
        }
        Err(error) => {
            return Err(std::io::Error::from_raw_os_error(error.raw_os_error()))
                .with_context(|| format!("{tool}: cannot inspect {display_path}"));
        }
    };
    let file = std::fs::File::from(fd);
    let metadata = file
        .metadata()
        .with_context(|| format!("{tool}: cannot inspect {display_path}"))?;
    if !metadata.is_file() {
        bail!("{tool}: {display_path} is not a regular file");
    }
    Ok(Some(TargetFile { file, metadata }))
}

/// Create `name` under `parent`, failing if it already exists. The caller
/// retries on [`std::io::ErrorKind::AlreadyExists`] and on nothing else, so the
/// error has to survive the round trip as an `io::Error`.
pub(super) fn create_temp(parent: &std::fs::File, name: &OsStr) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = rustix::fs::openat(
        parent,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH,
    )
    .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()))?;
    Ok(std::fs::File::from(fd))
}

/// Move the temp file onto `leaf`, both relative to `parent`.
///
/// `replace_existing` is the one genuine difference between the platforms:
/// Windows has to be told, while `renameat` always replaces — there is no
/// portable non-replacing rename, so unix answers the question by ignoring it.
pub(super) fn commit_rename(
    _temp: &std::fs::File,
    parent: &std::fs::File,
    name: &OsStr,
    leaf: &OsStr,
    _replace_existing: bool,
) -> Result<()> {
    rustix::fs::renameat(parent, name, parent, leaf)
        .map_err(|error| std::io::Error::from_raw_os_error(error.raw_os_error()).into())
}

/// Best effort removal of a temp file a failed commit left behind. unix drops
/// the name, Windows drops the open handle; both are unreachable from anywhere
/// else, so a failure here has nothing left to report to.
pub(super) fn discard_temp(_temp: &std::fs::File, parent: &std::fs::File, name: &OsStr) {
    let _ = rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty());
}

pub(super) fn sync_parent(parent: &std::fs::File, tool: &str, display_path: &str) -> Result<()> {
    parent
        .sync_all()
        .with_context(|| format!("{tool}: cannot sync parent directory for {display_path}"))
}

/// POSIX has no portable handle-bound `rmdir`: checking the retained inode and
/// then unlinking a name leaves a swap window that could delete a replacement
/// someone else created. Conservatively retain the empty directories rather than
/// touch an unbound name — the windows sibling, which can bind the handle, does
/// remove them.
pub(super) fn cleanup_created_directories(created: Vec<CreatedDirectory>) {
    for CreatedDirectory {
        parent,
        name,
        directory,
        identity,
    } in created
    {
        drop((parent, name, directory, identity));
    }
}

/// Nothing here reads a path component the way Windows does, so a component that
/// already passed the shared checks is accepted.
pub(super) fn reject_unsafe_component(
    _component: &OsStr,
    _tool: &str,
    _display_path: &str,
) -> Result<()> {
    Ok(())
}
