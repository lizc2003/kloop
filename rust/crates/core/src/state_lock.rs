use std::fs::File;
use std::io;
use std::path::Path;

#[derive(Debug)]
pub struct StateLock {
    file: File,
}

impl StateLock {
    fn open(path: &Path, create: bool) -> io::Result<File> {
        if create && let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .truncate(false)
            .create(create);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt as _;
            options.custom_flags(
                windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT,
            );
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(io::Error::other("state lock must be a regular file"));
        }
        Ok(file)
    }

    pub fn acquire(path: &Path) -> io::Result<Self> {
        let file = Self::open(path, true)?;
        file.lock()?;
        Ok(Self { file })
    }

    pub fn try_acquire(path: &Path) -> io::Result<Self> {
        let file = Self::open(path, true)?;
        file.try_lock().map_err(io::Error::other)?;
        Ok(Self { file })
    }

    /// Lock a file that must already exist. Reading a session for recovery uses
    /// this instead of [`StateLock::try_acquire`]: a missing transcript is an
    /// error, not a new empty file.
    pub fn try_acquire_existing(path: &Path) -> io::Result<Self> {
        let file = Self::open(path, false)?;
        file.try_lock().map_err(io::Error::other)?;
        Ok(Self { file })
    }
}

impl Drop for StateLock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_lock_probe() {
        let Some(path) = std::env::var_os("KLOOP_TEST_STATE_LOCK_PATH") else {
            return;
        };
        let locked = std::env::var_os("KLOOP_TEST_STATE_LOCK_HELD").is_some();
        assert_eq!(StateLock::try_acquire(Path::new(&path)).is_err(), locked);
    }

    #[test]
    fn independent_processes_cannot_share_a_live_lease() {
        let dir = std::env::temp_dir().join(crate::resource_id::fresh("kloop-lock-").unwrap());
        let path = dir.join("session.lock");
        let lease = StateLock::try_acquire(&path).unwrap();
        let probe = || {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args(["--exact", "state_lock::tests::child_lock_probe"]);
            command.env("KLOOP_TEST_STATE_LOCK_PATH", &path);
            command
        };
        assert!(
            probe()
                .env("KLOOP_TEST_STATE_LOCK_HELD", "1")
                .status()
                .unwrap()
                .success()
        );
        drop(lease);
        assert!(
            probe()
                .env_remove("KLOOP_TEST_STATE_LOCK_HELD")
                .status()
                .unwrap()
                .success()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// Reading a session for recovery must not mint a transcript: a missing
    /// path is an error, while an existing one is locked in place.
    #[test]
    fn an_existing_only_lease_never_creates_the_file() {
        let dir = std::env::temp_dir().join(crate::resource_id::fresh("kloop-lock-open-").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session.jsonl");
        assert!(StateLock::try_acquire_existing(&path).is_err());
        assert!(!path.exists(), "the failed lease left no file");

        std::fs::write(&path, b"{}\n").unwrap();
        let lease = StateLock::try_acquire_existing(&path).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\n");
        assert!(StateLock::try_acquire_existing(&path).is_err());
        drop(lease);
        assert!(StateLock::try_acquire_existing(&path).is_ok());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn lock_paths_do_not_follow_symlinks() {
        let dir = std::env::temp_dir().join(crate::resource_id::fresh("kloop-lock-link-").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("target");
        let link = dir.join("linked.lock");
        std::fs::write(&target, b"original").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(StateLock::try_acquire(&link).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"original");
        std::fs::remove_dir_all(dir).unwrap();
    }
}
