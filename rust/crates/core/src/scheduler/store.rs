use std::fs::{self, File, OpenOptions};
use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use super::{CronSpec, ScheduledJob};

const STORE_VERSION: u32 = 1;
const MAX_STORE_BYTES: u64 = 4 * 1024 * 1024;
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Serialize, Deserialize)]
struct StoreFile {
    version: u32,
    project_key: String,
    jobs: Vec<ScheduledJob>,
}

#[cfg(unix)]
fn lock_store_file(file: &File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::LockExclusive)
        .context("lock scheduler store")
}

#[cfg(unix)]
fn unlock_store_file(file: &File) -> Result<()> {
    rustix::fs::flock(file, rustix::fs::FlockOperation::Unlock).context("unlock scheduler store")
}

#[cfg(windows)]
fn lock_store_file(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK;
    use windows_sys::Win32::Storage::FileSystem::LockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        LockFileEx(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            LOCKFILE_EXCLUSIVE_LOCK,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("lock scheduler store");
    }
    Ok(())
}

#[cfg(windows)]
fn unlock_store_file(file: &File) -> Result<()> {
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { std::mem::zeroed() };
    let result = unsafe {
        UnlockFileEx(
            file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if result == 0 {
        return Err(std::io::Error::last_os_error()).context("unlock scheduler store");
    }
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn lock_store_file(_file: &File) -> Result<()> {
    bail!("durable scheduler locking is unsupported on this platform")
}

#[cfg(not(any(unix, windows)))]
fn unlock_store_file(_file: &File) -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_store_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .context("sync scheduler store directory")
}

#[cfg(not(unix))]
fn sync_store_directory(_path: &Path) -> Result<()> {
    // Windows does not expose a portable directory-entry flush equivalent.
    Ok(())
}

#[derive(Clone, Debug)]
pub struct DurableStore {
    path: PathBuf,
    lock_path: PathBuf,
    project_key: String,
}

impl DurableStore {
    pub fn new(path: PathBuf, project_key: String) -> Self {
        let lock_path = path.with_extension("lock");
        Self {
            path,
            lock_path,
            project_key,
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn load(&self) -> Result<Vec<ScheduledJob>> {
        self.with_lock(|store| Ok(store.jobs.clone()))
    }

    pub(super) fn transaction<T>(
        &self,
        operation: impl FnOnce(&mut Vec<ScheduledJob>) -> Result<T>,
    ) -> Result<T> {
        self.with_lock(|store| {
            let output = operation(&mut store.jobs)?;
            self.write_locked(store)?;
            Ok(output)
        })
    }

    pub(super) fn transaction_if_changed<T>(
        &self,
        operation: impl FnOnce(&mut Vec<ScheduledJob>) -> Result<(T, bool)>,
    ) -> Result<T> {
        self.with_lock(|store| {
            let (output, changed) = operation(&mut store.jobs)?;
            if changed {
                self.write_locked(store)?;
            }
            Ok(output)
        })
    }

    fn with_lock<T>(&self, operation: impl FnOnce(&mut StoreFile) -> Result<T>) -> Result<T> {
        let parent = self
            .path
            .parent()
            .ok_or_else(|| anyhow!("scheduler store path has no parent"))?;
        ensure_private_dir(parent)?;
        reject_symlink(&self.lock_path)?;
        let lock = private_open(&self.lock_path, true, false)?;
        lock_store_file(&lock)?;
        let mut store = self.read_locked()?;
        let result = operation(&mut store);
        let _ = unlock_store_file(&lock);
        result
    }

    fn read_locked(&self) -> Result<StoreFile> {
        reject_symlink(&self.path)?;
        let mut file = match private_open(&self.path, false, false) {
            Ok(file) => file,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound) =>
            {
                return Ok(StoreFile {
                    version: STORE_VERSION,
                    project_key: self.project_key.clone(),
                    jobs: Vec::new(),
                });
            }
            Err(error) => return Err(error),
        };
        let metadata = file.metadata().context("inspect scheduler store")?;
        if !metadata.is_file() || metadata.len() > MAX_STORE_BYTES {
            bail!("scheduler store must be a regular file no larger than 4 MiB");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut bytes)
            .context("read scheduler store")?;
        let store: StoreFile = serde_json::from_slice(&bytes).context("parse scheduler store")?;
        if store.version != STORE_VERSION || store.project_key != self.project_key {
            bail!("scheduler store identity or schema version mismatch");
        }
        for job in &store.jobs {
            CronSpec::parse(&job.cron)
                .with_context(|| format!("scheduler store contains invalid job {}", job.id))?;
            if job.owner.is_empty()
                || !job
                    .id
                    .strip_prefix("job-")
                    .is_some_and(crate::resource_id::is_suffix)
            {
                bail!("scheduler store contains malformed job identity");
            }
        }
        Ok(store)
    }

    fn write_locked(&self, store: &StoreFile) -> Result<()> {
        reject_symlink(&self.path)?;
        let parent = self.path.parent().unwrap();
        let name = self
            .path
            .file_name()
            .and_then(|value| value.to_str())
            .ok_or_else(|| anyhow!("scheduler store filename is not UTF-8"))?;
        let sequence = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = parent.join(format!(".{name}.tmp-{}-{sequence}", std::process::id()));
        let payload = serde_json::to_vec_pretty(store).context("serialize scheduler store")?;
        let result = (|| {
            let mut file = private_open(&temp, true, true)?;
            file.write_all(&payload)
                .context("write scheduler temp file")?;
            file.write_all(b"\n")
                .context("finish scheduler temp file")?;
            file.sync_all().context("sync scheduler temp file")?;
            fs::rename(&temp, &self.path).context("replace scheduler store")?;
            sync_store_directory(parent)?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }
}

#[cfg(unix)]
fn private_open(path: &Path, create: bool, create_new: bool) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create || create_new)
        .create(create)
        .create_new(create_new)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW);
    options
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

#[cfg(not(unix))]
fn private_open(path: &Path, create: bool, create_new: bool) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .write(create || create_new)
        .create(create)
        .create_new(create_new)
        .open(path)
        .with_context(|| format!("open {}", path.display()))
}

fn reject_symlink(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            bail!("scheduler path is a symlink: {}", path.display())
        }
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("inspect {}", path.display())),
    }
}

fn ensure_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create {}", path.display()))?;
    let metadata =
        fs::symlink_metadata(path).with_context(|| format!("inspect {}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        bail!("scheduler store parent must be a real directory");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions)
            .with_context(|| format!("protect {}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::ScheduledKind;
    use super::super::testutil::TEMP_SEQ;
    use super::*;

    #[test]
    fn durable_store_is_atomic_owner_partitioned_and_fail_closed() {
        let root = std::env::temp_dir().join(format!(
            "kloop-scheduler-store-{}-{}",
            std::process::id(),
            TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("scheduled_tasks.json");
        let store = DurableStore::new(path.clone(), "project-a".into());
        store
            .transaction(|jobs| {
                jobs.push(ScheduledJob {
                    id: "job-11111111111".into(),
                    owner: "owner-a".into(),
                    cron: "0 0 * * *".into(),
                    prompt: "persist".into(),
                    created_at_ms: 1,
                    last_fired_at_ms: None,
                    next_fire_at_ms: 2,
                    recurring: false,
                    durable: true,
                    kind: ScheduledKind::Cron,
                    reason: None,
                    generation: 1,
                    check: None,
                });
                Ok(())
            })
            .unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
        fs::write(&path, b"{bad json").unwrap();
        assert!(store.load().is_err());
        assert_eq!(fs::read(&path).unwrap(), b"{bad json");
        assert!(fs::read_dir(&root).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));
        fs::remove_dir_all(root).unwrap();
    }
}
