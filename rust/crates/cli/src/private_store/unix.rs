//! Descriptor-relative private storage for unix.
//!
//! Every component and leaf is opened with `NOFOLLOW` below a directory
//! descriptor, so a symlink swapped in mid-walk cannot move the target.

use std::ffi::OsStr;
use std::io::Write as _;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

use super::read_opened_private_file;
use super::validate_component;
use super::validate_private_file;

pub(super) fn read_private_string(path: &Path, label: &str) -> Result<Option<String>> {
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let Some(dir) = open_private_dir(parent, label)? else {
        return Ok(None);
    };
    read_private_string_at(&dir, private_file_name(path, label)?, label)
}

pub(super) fn write_private_atomic(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    if let Some(existing) = read_private_string(path, label)? {
        drop(existing);
    }
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let dir = ensure_private_dir_open(parent, label)?;
    write_private_atomic_at(&dir, private_file_name(path, label)?, label, bytes)
}

pub(super) fn validate_private_permissions(
    metadata: &std::fs::Metadata,
    label: &str,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "{label} is readable or writable by other users (mode {mode:04o}); it must be mode 0600"
        );
    }
    Ok(())
}

pub(super) fn inspect_private_file(
    root: &Path,
    components: &[&OsStr],
    name: &OsStr,
) -> super::Inspection {
    use super::Finding;
    use rustix::fs::AtFlags;
    use rustix::fs::FileType;
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let mut inspection = super::Inspection::default();
    let lookup =
        private_dir_lookup_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    let mut path = root.to_path_buf();
    let directory_problem =
        |stat: &rustix::fs::Stat, path: &Path| match FileType::from_raw_mode(stat.st_mode as _) {
            FileType::Symlink => Some(Finding::Link {
                path: path.to_path_buf(),
                directory: true,
            }),
            FileType::Directory => None,
            _ => Some(Finding::NotDirectory(path.to_path_buf())),
        };
    let opened = |result: rustix::io::Result<rustix::fd::OwnedFd>, path: &Path, directory: bool| {
        match result {
            Ok(fd) => Ok(Some(fd)),
            Err(rustix::io::Errno::NOENT) => Ok(None),
            Err(rustix::io::Errno::LOOP) => Err(Finding::Link {
                path: path.to_path_buf(),
                directory,
            }),
            Err(rustix::io::Errno::NOTDIR) if directory => {
                Err(Finding::NotDirectory(path.to_path_buf()))
            }
            Err(_) => Err(Finding::Inaccessible(path.to_path_buf())),
        }
    };

    let stat = match rustix::fs::lstat(root) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return inspection,
        Err(_) => {
            inspection.findings.push(Finding::Inaccessible(path));
            return inspection;
        }
    };
    if let Some(finding) = directory_problem(&stat, &path) {
        inspection.findings.push(finding);
        return inspection;
    }
    let mut current = match opened(rustix::fs::open(root, lookup, Mode::empty()), &path, true) {
        Ok(Some(fd)) => fd,
        Ok(None) => return inspection,
        Err(finding) => {
            inspection.findings.push(finding);
            return inspection;
        }
    };
    if !inspect_opened_mode(
        &current,
        &stat,
        &path,
        /*directory*/ true,
        &mut inspection,
    ) {
        return inspection;
    }
    for component in components {
        path.push(component);
        let stat = match rustix::fs::statat(&current, *component, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => stat,
            Err(rustix::io::Errno::NOENT) => return inspection,
            Err(_) => {
                inspection.findings.push(Finding::Inaccessible(path));
                return inspection;
            }
        };
        if let Some(finding) = directory_problem(&stat, &path) {
            inspection.findings.push(finding);
            return inspection;
        }
        current = match opened(
            rustix::fs::openat(&current, *component, lookup, Mode::empty()),
            &path,
            true,
        ) {
            Ok(Some(fd)) => fd,
            Ok(None) => return inspection,
            Err(finding) => {
                inspection.findings.push(finding);
                return inspection;
            }
        };
        if !inspect_opened_mode(
            &current,
            &stat,
            &path,
            /*directory*/ true,
            &mut inspection,
        ) {
            return inspection;
        }
    }
    path.push(name);
    let stat = match rustix::fs::statat(&current, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(rustix::io::Errno::NOENT) => return inspection,
        Err(_) => {
            inspection.findings.push(Finding::Inaccessible(path));
            return inspection;
        }
    };
    match FileType::from_raw_mode(stat.st_mode as _) {
        FileType::Symlink => {
            inspection.findings.push(Finding::Link {
                path,
                directory: false,
            });
            return inspection;
        }
        FileType::RegularFile => {}
        _ => {
            inspection.findings.push(Finding::NotRegularFile(path));
            return inspection;
        }
    }
    let fd = match opened(
        rustix::fs::openat(
            &current,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        ),
        &path,
        false,
    ) {
        Ok(Some(fd)) => fd,
        Ok(None) => return inspection,
        Err(finding) => {
            inspection.findings.push(finding);
            return inspection;
        }
    };
    if !inspect_opened_mode(&fd, &stat, &path, /*directory*/ false, &mut inspection) {
        return inspection;
    }
    let mut file = std::fs::File::from(fd);
    let mut raw = String::new();
    if std::io::Read::read_to_string(&mut file, &mut raw).is_ok() {
        inspection.contents = Some(raw);
    }
    inspection
}

/// 只有已经打开、且与目录项检查读到的是同一个对象时，才记录权限位。
fn inspect_opened_mode(
    fd: &rustix::fd::OwnedFd,
    before: &rustix::fs::Stat,
    path: &Path,
    directory: bool,
    inspection: &mut super::Inspection,
) -> bool {
    use super::Finding;
    use rustix::fs::FileType;

    let Ok(stat) = rustix::fs::fstat(fd) else {
        inspection
            .findings
            .push(Finding::Inaccessible(path.to_path_buf()));
        return false;
    };
    let expected = if directory {
        FileType::Directory
    } else {
        FileType::RegularFile
    };
    if stat.st_dev != before.st_dev
        || stat.st_ino != before.st_ino
        || FileType::from_raw_mode(stat.st_mode as _) != expected
    {
        inspection
            .findings
            .push(Finding::Changed(path.to_path_buf()));
        return false;
    }
    if (stat.st_mode as u32) & 0o077 != 0 {
        inspection.findings.push(Finding::LooseMode {
            path: path.to_path_buf(),
            directory,
        });
    }
    true
}

#[cfg(test)]
#[test]
fn inspecting_a_replaced_entry_withholds_the_mode_fix() {
    use std::os::unix::fs::PermissionsExt as _;

    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("permissions.json");
    let replacement = root.path().join("replacement");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let before = rustix::fs::lstat(&path).unwrap();
    std::fs::write(&replacement, "").unwrap();
    std::fs::rename(&replacement, &path).unwrap();
    let fd = std::fs::File::open(&path).unwrap().into();
    let mut inspection = super::Inspection::default();
    assert!(!inspect_opened_mode(
        &fd,
        &before,
        &path,
        /*directory*/ false,
        &mut inspection
    ));
    assert_eq!(inspection.findings, vec![super::Finding::Changed(path)]);
}

fn private_file_name<'a>(path: &'a Path, label: &str) -> Result<&'a OsStr> {
    path.file_name()
        .with_context(|| format!("{label} has no file name"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn private_dir_lookup_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::PATH
}

#[cfg(target_os = "macos")]
fn private_dir_lookup_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::from_bits_retain(libc::O_SEARCH as _)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
fn private_dir_lookup_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
}

fn private_dir_sync_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
}

fn validate_private_dir(dir: std::fs::File, label: &str) -> Result<std::fs::File> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = dir
        .metadata()
        .map_err(|_| anyhow!("cannot inspect directory for {label}"))?;
    if !metadata.is_dir() {
        bail!("directory for {label} must be a regular directory, not a symlink");
    }
    let mode = metadata.permissions().mode() & 0o7777;
    if mode & 0o077 != 0 {
        bail!(
            "directory for {label} is accessible by other users (mode {mode:04o}); it must be mode 0700"
        );
    }
    Ok(dir)
}

fn open_private_dir_with_flags(
    path: &Path,
    label: &str,
    access: rustix::fs::OFlags,
) -> Result<Option<std::fs::File>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::open(
        path,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
            bail!("directory for {label} must be a regular directory, not a symlink");
        }
        Err(_) => bail!("cannot open directory for {label}"),
    };
    validate_private_dir(std::fs::File::from(fd), label).map(Some)
}

fn open_private_dir(path: &Path, label: &str) -> Result<Option<std::fs::File>> {
    open_private_dir_with_flags(path, label, private_dir_lookup_flags())
}

fn open_private_dir_at_with_flags(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
    access: rustix::fs::OFlags,
) -> Result<Option<std::fs::File>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::openat(
        parent,
        name,
        access | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => {
            bail!("directory for {label} must be a regular directory, not a symlink");
        }
        Err(_) => bail!("cannot open directory for {label}"),
    };
    validate_private_dir(std::fs::File::from(fd), label).map(Some)
}

fn open_private_dir_at(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    open_private_dir_at_with_flags(parent, name, label, private_dir_lookup_flags())
}

fn open_private_dir_at_sync(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    open_private_dir_at_with_flags(parent, name, label, private_dir_sync_flags())
}

fn open_stable_parent_dir(path: &Path, label: &str) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;
    use std::os::unix::fs::PermissionsExt as _;

    let fd = rustix::fs::open(
        path,
        private_dir_sync_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(|_| anyhow!("cannot open parent directory for {label}"))?;
    let dir = std::fs::File::from(fd);
    let metadata = dir
        .metadata()
        .map_err(|_| anyhow!("cannot inspect parent directory for {label}"))?;
    if !metadata.is_dir() {
        bail!("parent directory for {label} must be a regular directory, not a symlink");
    }
    if metadata.permissions().mode() & 0o022 != 0 {
        bail!("parent directory for {label} must not be writable by group or other");
    }
    Ok(dir)
}

fn ensure_private_dir_open(path: &Path, label: &str) -> Result<std::fs::File> {
    let parent = path
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    let name = private_file_name(path, label)?;
    let anchor = open_stable_parent_dir(parent, label)?;
    ensure_private_dir_at(&anchor, name, label)
}

fn ensure_private_dir_at(
    parent: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<std::fs::File> {
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;

    validate_component(name, label)?;
    if let Some(dir) = open_private_dir_at_sync(parent, name, label)? {
        parent
            .sync_all()
            .with_context(|| format!("cannot sync parent directory for {label}"))?;
        return Ok(dir);
    }
    let created = match rustix::fs::mkdirat(parent, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(_) => bail!("cannot create directory for {label}"),
    };
    if created {
        rustix::fs::chmodat(
            parent,
            name,
            Mode::RUSR | Mode::WUSR | Mode::XUSR,
            AtFlags::empty(),
        )
        .with_context(|| format!("cannot restrict directory for {label}"))?;
        parent
            .sync_all()
            .with_context(|| format!("cannot sync parent directory for {label}"))?;
    }
    open_private_dir_at_sync(parent, name, label)?
        .with_context(|| format!("cannot open directory for {label}"))
}

pub(super) fn read_private_string_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
) -> Result<Option<String>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    validate_component(name, label)?;
    let fd = match rustix::fs::openat(
        dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => bail!("{label} must be a regular file, not a symlink"),
        Err(error) => bail!("cannot open {label}: {error}"),
    };
    read_opened_private_file(std::fs::File::from(fd), label).map(Some)
}

pub(super) fn write_private_atomic_at(
    dir: &std::fs::File,
    name: &OsStr,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    validate_component(name, label)?;
    if let Some(existing) = read_private_string_at(dir, name, label)? {
        drop(existing);
    }
    let stem = name.to_string_lossy();
    for suffix in 0..100_u8 {
        let temp = format!(".{stem}.tmp-{}-{suffix}", std::process::id());
        let fd = match rustix::fs::openat(
            dir,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::EXIST) => continue,
            Err(_) => bail!("cannot create temporary {label}"),
        };
        let mut file = std::fs::File::from(fd);
        let result = (|| -> Result<()> {
            rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
                .with_context(|| format!("cannot restrict temporary {label}"))?;
            file.write_all(bytes)
                .with_context(|| format!("cannot write temporary {label}"))?;
            file.sync_all()
                .with_context(|| format!("cannot sync temporary {label}"))?;
            rustix::fs::renameat(dir, temp.as_str(), dir, name)
                .with_context(|| format!("cannot replace {label}"))?;
            dir.sync_all()
                .with_context(|| format!("cannot sync directory for {label}"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(dir, temp.as_str(), AtFlags::empty());
        }
        return result;
    }
    bail!("cannot create temporary {label}")
}

fn open_private_lock_at(dir: &std::fs::File, name: &OsStr, label: &str) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    validate_component(name, label)?;
    let common = OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK;
    let fd = loop {
        match rustix::fs::openat(
            dir,
            name,
            common | OFlags::CREATE | OFlags::EXCL,
            Mode::RUSR | Mode::WUSR,
        ) {
            Ok(fd) => break fd,
            Err(rustix::io::Errno::EXIST) => {
                match rustix::fs::openat(dir, name, common, Mode::empty()) {
                    Ok(fd) => break fd,
                    Err(rustix::io::Errno::NOENT) => {
                        std::thread::yield_now();
                        continue;
                    }
                    Err(rustix::io::Errno::LOOP) => {
                        bail!("{label} must be a regular file, not a symlink");
                    }
                    Err(error) => bail!("cannot open {label}: {error}"),
                }
            }
            Err(rustix::io::Errno::LOOP) => {
                bail!("{label} must be a regular file, not a symlink");
            }
            Err(error) => bail!("cannot open {label}: {error}"),
        }
    };
    let file = std::fs::File::from(fd);
    rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
        .with_context(|| format!("cannot restrict {label}"))?;
    validate_private_file(&file, label)?;
    Ok(file)
}

pub(super) struct Dir {
    file: std::fs::File,
}

impl Dir {
    pub(super) fn open(root: &Path, components: &[&OsStr], label: &str) -> Result<Option<Self>> {
        let Some(mut current) = open_private_dir(root, label)? else {
            return Ok(None);
        };
        for component in components {
            validate_component(component, label)?;
            let Some(next) = open_private_dir_at(&current, component, label)? else {
                return Ok(None);
            };
            current = next;
        }
        Ok(Some(Self { file: current }))
    }

    pub(super) fn ensure(root: &Path, components: &[&OsStr], label: &str) -> Result<Self> {
        let mut current = ensure_private_dir_open(root, label)?;
        for component in components {
            current = ensure_private_dir_at(&current, component, label)?;
        }
        Ok(Self { file: current })
    }

    pub(super) fn read_string(&self, name: &OsStr, label: &str) -> Result<Option<String>> {
        read_private_string_at(&self.file, name, label)
    }

    pub(super) fn write_atomic(&self, name: &OsStr, label: &str, bytes: &[u8]) -> Result<()> {
        write_private_atomic_at(&self.file, name, label, bytes)
    }

    pub(super) fn open_lock_file(&self, name: &OsStr, label: &str) -> Result<std::fs::File> {
        open_private_lock_at(&self.file, name, label)
    }
}

pub(super) fn lock_file(file: &std::fs::File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive).context("lock private store")
}

pub(super) fn unlock_file(file: &std::fs::File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock).context("unlock private store")
}
