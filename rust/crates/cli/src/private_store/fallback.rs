//! Path-based private storage for platforms that are neither unix nor windows.
//!
//! Without descriptor-relative opens the walk can only re-inspect each path for
//! symlinks; locking has no portable primitive left and is refused outright.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;

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
    if !inspect_private_dir(parent, label)? {
        return Ok(None);
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("{label} must be a regular file, not a symlink");
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => bail!("cannot inspect {label}"),
    }
    let file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => bail!("cannot open {label}: {error}"),
    };
    read_opened_private_file(file, label).map(Some)
}

pub(super) fn write_private_atomic(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    if let Some(existing) = read_private_string(path, label)? {
        drop(existing);
    }
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    ensure_private_dir(parent, label)?;
    write_private_atomic_path(parent, path.file_name().unwrap_or_default(), label, bytes)
}

/// Neither mode bits nor ACLs are reachable portably, so this platform makes no
/// promise about permissions — only the symlink refusals along the walk.
pub(super) fn validate_private_permissions(
    _metadata: &std::fs::Metadata,
    _label: &str,
) -> Result<()> {
    Ok(())
}

fn inspect_private_dir(path: &Path, label: &str) -> Result<bool> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(_) => bail!("cannot inspect directory for {label}"),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("directory for {label} must be a regular directory, not a symlink");
    }
    Ok(true)
}

fn sync_private_directory(path: &Path, label: &str) -> Result<()> {
    std::fs::File::open(path)
        .with_context(|| format!("cannot open parent directory for {label}"))?
        .sync_all()
        .with_context(|| format!("cannot sync parent directory for {label}"))
}

fn replace_private_path(temp: &Path, path: &Path, label: &str) -> Result<()> {
    std::fs::rename(temp, path).with_context(|| format!("cannot replace {label}"))
}

fn ensure_private_dir(path: &Path, label: &str) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    if inspect_private_dir(path, label)? {
        return sync_private_directory(parent, label);
    }
    std::fs::create_dir(path).with_context(|| format!("cannot create directory for {label}"))?;
    sync_private_directory(parent, label)
}

pub(super) fn write_private_atomic_path(
    parent: &Path,
    name: &OsStr,
    label: &str,
    bytes: &[u8],
) -> Result<()> {
    validate_component(name, label)?;
    let path = parent.join(name);
    if let Some(existing) = read_private_string(&path, label)? {
        drop(existing);
    }
    let stem = name.to_string_lossy();
    for suffix in 0..100_u8 {
        let temp = parent.join(format!(".{stem}.tmp-{}-{suffix}", std::process::id()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        match options.open(&temp) {
            Ok(mut file) => {
                let result = (|| -> Result<()> {
                    file.write_all(bytes)
                        .with_context(|| format!("cannot write temporary {label}"))?;
                    file.sync_all()
                        .with_context(|| format!("cannot sync temporary {label}"))?;
                    replace_private_path(&temp, &path, label)?;
                    sync_private_directory(parent, label)?;
                    Ok(())
                })();
                if result.is_err() {
                    let _ = std::fs::remove_file(&temp);
                }
                return result;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => bail!("cannot create temporary {label}"),
        }
    }
    bail!("cannot create temporary {label}")
}

pub(super) struct Dir {
    path: PathBuf,
}

impl Dir {
    pub(super) fn open(root: &Path, components: &[&OsStr], label: &str) -> Result<Option<Self>> {
        if !inspect_private_dir(root, label)? {
            return Ok(None);
        }
        let mut current = root.to_path_buf();
        for component in components {
            validate_component(component, label)?;
            current.push(component);
            if !inspect_private_dir(&current, label)? {
                return Ok(None);
            }
        }
        Ok(Some(Self { path: current }))
    }

    pub(super) fn ensure(root: &Path, components: &[&OsStr], label: &str) -> Result<Self> {
        let parent = root
            .parent()
            .with_context(|| format!("directory for {label} has no parent"))?;
        if !inspect_private_dir(parent, label)? {
            bail!("cannot open parent directory for {label}");
        }
        ensure_private_dir(root, label)?;
        let mut current = root.to_path_buf();
        for component in components {
            validate_component(component, label)?;
            current.push(component);
            ensure_private_dir(&current, label)?;
        }
        Ok(Self { path: current })
    }

    pub(super) fn read_string(&self, name: &OsStr, label: &str) -> Result<Option<String>> {
        read_private_string(&self.path.join(name), label)
    }

    pub(super) fn write_atomic(&self, name: &OsStr, label: &str, bytes: &[u8]) -> Result<()> {
        write_private_atomic_path(&self.path, name, label, bytes)
    }

    pub(super) fn open_lock_file(&self, name: &OsStr, label: &str) -> Result<std::fs::File> {
        validate_component(name, label)?;
        let path = self.path.join(name);
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                bail!("{label} must be a regular file, not a symlink");
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => bail!("cannot inspect {label}"),
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true);
        let file = options
            .open(path)
            .map_err(|_| anyhow!("cannot open {label}"))?;
        validate_private_file(&file, label)?;
        Ok(file)
    }
}

pub(super) fn lock_file(_file: &std::fs::File) -> Result<()> {
    bail!("private store locking is unsupported on this platform")
}

pub(super) fn unlock_file(_file: &std::fs::File) -> Result<()> {
    Ok(())
}
