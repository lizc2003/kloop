//! Bounded, component-safe storage for resumable program and Workflow runs.

use std::path::Path;
use std::path::PathBuf;
#[cfg(unix)]
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;

const MAX_RUN_ID_BYTES: usize = 100;
const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
static TEMP_SEQ: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) struct RunId(String);

impl RunId {
    pub(super) fn parse(raw: &str) -> Result<Self> {
        let valid = !raw.is_empty()
            && raw.len() <= MAX_RUN_ID_BYTES
            && raw
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'));
        if !valid || matches!(raw, "." | "..") {
            return Err(anyhow!(
                "invalid run id: use 1..={MAX_RUN_ID_BYTES} ASCII letters, digits, '_' or '-'"
            ));
        }
        Ok(Self(raw.to_string()))
    }

    pub(super) fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy)]
pub(super) enum RunNamespace {
    Program,
    Workflow,
}

impl RunNamespace {
    fn directory(self) -> &'static str {
        match self {
            Self::Program => "program-runs",
            Self::Workflow => "workflow-runs",
        }
    }
}

pub(super) struct RunStore {
    root: PathBuf,
    #[cfg(unix)]
    root_dir: Arc<std::fs::File>,
}

impl RunStore {
    pub(super) fn new(offload_dir: &Path, namespace: RunNamespace) -> Result<Self> {
        let base = offload_dir.parent().unwrap_or(offload_dir);
        ensure_not_symlink(base)?;
        std::fs::create_dir_all(base)
            .with_context(|| format!("cannot create run-store base {}", base.display()))?;
        let base = std::fs::canonicalize(base)
            .with_context(|| format!("cannot canonicalize run-store base {}", base.display()))?;
        let root = base.join(namespace.directory());
        ensure_not_symlink(&root)?;
        std::fs::create_dir_all(&root)
            .with_context(|| format!("cannot create run-store namespace {}", root.display()))?;
        let canonical = std::fs::canonicalize(&root).with_context(|| {
            format!("cannot canonicalize run-store namespace {}", root.display())
        })?;
        if canonical.parent() != Some(base.as_path()) {
            return Err(anyhow!("run-store namespace escaped its .kloop base"));
        }
        #[cfg(unix)]
        let root_dir = Arc::new(open_directory(&canonical)?);
        Ok(Self {
            root: canonical,
            #[cfg(unix)]
            root_dir,
        })
    }

    pub(super) fn create(&self, id: &RunId) -> Result<RunDir> {
        self.verify_root()?;
        #[cfg(unix)]
        {
            use rustix::fs::Mode;
            rustix::fs::mkdirat(
                &*self.root_dir,
                id.as_str(),
                Mode::RUSR
                    | Mode::WUSR
                    | Mode::XUSR
                    | Mode::RGRP
                    | Mode::XGRP
                    | Mode::ROTH
                    | Mode::XOTH,
            )
            .with_context(|| format!("cannot create run {}", id.as_str()))?;
        }
        #[cfg(not(unix))]
        {
            let path = self.root.join(id.as_str());
            ensure_not_symlink(&path)?;
            std::fs::create_dir(&path)
                .with_context(|| format!("cannot create run {}", id.as_str()))?;
        }
        self.open(id)
    }

    pub(super) fn open(&self, id: &RunId) -> Result<RunDir> {
        self.verify_root()?;
        let path = self.root.join(id.as_str());
        ensure_not_symlink(&path)?;
        #[cfg(unix)]
        let dir = Arc::new(open_directory_at(&self.root_dir, id.as_str())?);
        let canonical = std::fs::canonicalize(&path)
            .with_context(|| format!("cannot open run {}", id.as_str()))?;
        if canonical.parent() != Some(self.root.as_path()) || !canonical.is_dir() {
            return Err(anyhow!("run {} escaped its namespace", id.as_str()));
        }
        #[cfg(unix)]
        if !same_file(&dir.metadata()?, &std::fs::symlink_metadata(&canonical)?) {
            return Err(anyhow!("run {} changed while it was opened", id.as_str()));
        }
        Ok(RunDir {
            id: id.clone(),
            path: canonical,
            root: self.root.clone(),
            #[cfg(unix)]
            dir,
            #[cfg(unix)]
            root_dir: self.root_dir.clone(),
        })
    }

    fn verify_root(&self) -> Result<()> {
        ensure_not_symlink(&self.root)?;
        let canonical =
            std::fs::canonicalize(&self.root).context("cannot canonicalize run-store namespace")?;
        if canonical != self.root {
            return Err(anyhow!("run-store namespace changed while in use"));
        }
        #[cfg(unix)]
        if !same_file(
            &self.root_dir.metadata()?,
            &std::fs::symlink_metadata(&canonical)?,
        ) {
            return Err(anyhow!("run-store namespace changed while in use"));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct RunDir {
    id: RunId,
    path: PathBuf,
    root: PathBuf,
    #[cfg(unix)]
    dir: Arc<std::fs::File>,
    #[cfg(unix)]
    root_dir: Arc<std::fs::File>,
}

impl RunDir {
    pub(super) fn id(&self) -> &RunId {
        &self.id
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn acquire(&self) -> Result<RunLease> {
        self.verify()?;
        #[cfg(unix)]
        {
            use rustix::fs::Mode;
            use rustix::fs::OFlags;
            let fd = rustix::fs::openat(
                &*self.dir,
                "run.lock",
                OFlags::WRONLY | OFlags::CREATE | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::RUSR | Mode::WUSR,
            )
            .context("cannot open run lock")?;
            let mut file = std::fs::File::from(fd);
            rustix::fs::flock(&file, rustix::fs::FlockOperation::NonBlockingLockExclusive)
                .with_context(|| format!("run {} is already active", self.id.as_str()))?;
            file.set_len(0).context("cannot reset run lock")?;
            use std::io::Write as _;
            writeln!(file, "{}", std::process::id()).context("cannot write run lock")?;
            file.sync_all().context("cannot sync run lock")?;
            Ok(RunLease { _file: file })
        }
        #[cfg(not(unix))]
        {
            let path = self.file_path("run.lock")?;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("run {} is already active", self.id.as_str()))?;
            use std::io::Write as _;
            writeln!(file, "{}", std::process::id()).context("cannot write run lock")?;
            Ok(RunLease { path })
        }
    }

    pub(super) fn file_path(&self, name: &'static str) -> Result<PathBuf> {
        validate_file_name(name)?;
        self.verify()?;
        let path = self.path.join(name);
        ensure_not_symlink(&path)?;
        Ok(path)
    }

    pub(super) fn write_atomic(&self, name: &'static str, bytes: &[u8]) -> Result<PathBuf> {
        validate_file_name(name)?;
        if bytes.len() > MAX_FILE_BYTES {
            return Err(anyhow!("run file exceeds the {MAX_FILE_BYTES}-byte limit"));
        }
        self.verify()?;
        #[cfg(unix)]
        self.write_atomic_unix(name, bytes)?;
        #[cfg(not(unix))]
        self.write_atomic_path(name, bytes)?;
        Ok(self.path.join(name))
    }

    #[cfg(unix)]
    fn write_atomic_unix(&self, name: &'static str, bytes: &[u8]) -> Result<()> {
        use rustix::fs::AtFlags;
        use rustix::fs::Mode;
        use rustix::fs::OFlags;

        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = format!(".{name}.tmp-{}-{seq}", std::process::id());
        let fd = rustix::fs::openat(
            &*self.dir,
            temp.as_str(),
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )
        .with_context(|| format!("cannot create temporary run file {temp}"))?;
        let mut file = std::fs::File::from(fd);
        use std::io::Write as _;
        let result = (|| -> Result<()> {
            file.write_all(bytes)
                .context("cannot write temporary run file")?;
            file.sync_all().context("cannot sync temporary run file")?;
            rustix::fs::renameat(&*self.dir, temp.as_str(), &*self.dir, name)
                .context("cannot atomically replace run file")?;
            self.dir.sync_all().context("cannot sync run directory")?;
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&*self.dir, temp.as_str(), AtFlags::empty());
        }
        result
    }

    #[cfg(not(unix))]
    fn write_atomic_path(&self, name: &'static str, bytes: &[u8]) -> Result<()> {
        let target = self.path.join(name);
        ensure_not_symlink(&target)?;
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let temp = self
            .path
            .join(format!(".{name}.tmp-{}-{seq}", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)
            .with_context(|| format!("cannot create temporary run file {}", temp.display()))?;
        use std::io::Write as _;
        if let Err(error) = file.write_all(bytes).and_then(|_| file.sync_all()) {
            let _ = std::fs::remove_file(&temp);
            return Err(error).context("cannot write temporary run file");
        }
        if let Err(error) = std::fs::rename(&temp, &target) {
            let _ = std::fs::remove_file(&temp);
            return Err(error).context("cannot atomically replace run file");
        }
        Ok(())
    }

    pub(super) fn read(&self, name: &'static str) -> Result<Vec<u8>> {
        self.read_bounded(name, MAX_FILE_BYTES)
    }

    pub(super) fn read_bounded(&self, name: &'static str, max_bytes: usize) -> Result<Vec<u8>> {
        self.read_optional_bounded(name, max_bytes)?
            .ok_or_else(|| anyhow!("run artifact {name} is missing"))
    }

    pub(super) fn read_optional_bounded(
        &self,
        name: &'static str,
        max_bytes: usize,
    ) -> Result<Option<Vec<u8>>> {
        validate_file_name(name)?;
        if max_bytes == 0 || max_bytes > MAX_FILE_BYTES {
            return Err(anyhow!("run read limit must be 1..={MAX_FILE_BYTES} bytes"));
        }
        self.verify()?;
        #[cfg(unix)]
        {
            use rustix::fs::Mode;
            use rustix::fs::OFlags;
            let fd = match rustix::fs::openat(
                &*self.dir,
                name,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
                Mode::empty(),
            ) {
                Ok(fd) => fd,
                Err(rustix::io::Errno::NOENT) => return Ok(None),
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("cannot open run file {}", self.path.join(name).display())
                    });
                }
            };
            let file = std::fs::File::from(fd);
            let metadata = file.metadata().context("cannot stat run file")?;
            if !metadata.is_file() {
                return Err(anyhow!("run artifact {name} is not a regular file"));
            }
            if metadata.len() > max_bytes as u64 {
                return Err(anyhow!("run file exceeds the {max_bytes}-byte limit"));
            }
            use std::io::Read as _;
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.take((max_bytes + 1) as u64)
                .read_to_end(&mut bytes)
                .context("cannot read run file")?;
            if bytes.len() > max_bytes {
                return Err(anyhow!("run file exceeds the {max_bytes}-byte limit"));
            }
            Ok(Some(bytes))
        }
        #[cfg(not(unix))]
        {
            let path = self.file_path(name)?;
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("cannot stat run file {}", path.display()));
                }
            };
            if !metadata.is_file() {
                return Err(anyhow!("run artifact {name} is not a regular file"));
            }
            if metadata.len() > max_bytes as u64 {
                return Err(anyhow!("run file exceeds the {max_bytes}-byte limit"));
            }
            let file = std::fs::File::open(&path)
                .with_context(|| format!("cannot read run file {}", path.display()))?;
            use std::io::Read as _;
            let mut bytes = Vec::with_capacity(metadata.len() as usize);
            file.take((max_bytes + 1) as u64)
                .read_to_end(&mut bytes)
                .with_context(|| format!("cannot read run file {}", path.display()))?;
            if bytes.len() > max_bytes {
                return Err(anyhow!("run file exceeds the {max_bytes}-byte limit"));
            }
            Ok(Some(bytes))
        }
    }

    fn verify(&self) -> Result<()> {
        ensure_not_symlink(&self.root)?;
        ensure_not_symlink(&self.path)?;
        let root = std::fs::canonicalize(&self.root).context("cannot verify run namespace")?;
        let path = std::fs::canonicalize(&self.path).context("cannot verify run directory")?;
        if root != self.root || path != self.path || path.parent() != Some(root.as_path()) {
            return Err(anyhow!("run directory changed or escaped while in use"));
        }
        #[cfg(unix)]
        {
            if !same_file(
                &self.root_dir.metadata()?,
                &std::fs::symlink_metadata(&root)?,
            ) || !same_file(&self.dir.metadata()?, &std::fs::symlink_metadata(&path)?)
            {
                return Err(anyhow!("run directory changed or escaped while in use"));
            }
        }
        Ok(())
    }
}

pub(super) struct RunLease {
    #[cfg(unix)]
    _file: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

#[cfg(unix)]
impl Drop for RunLease {
    fn drop(&mut self) {
        // A concurrent fork can briefly inherit the CLOEXEC descriptor. Unlock
        // explicitly so that inherited copies cannot extend the released lease.
        let _ = rustix::fs::flock(&self._file, rustix::fs::FlockOperation::Unlock);
    }
}

#[cfg(not(unix))]
impl Drop for RunLease {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn validate_file_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && !matches!(name, "." | "..")
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if !valid {
        return Err(anyhow!("invalid run artifact name {name:?}"));
    }
    Ok(())
}

fn ensure_not_symlink(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(anyhow!(
            "refusing symlink in run-store path {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("cannot inspect {}", path.display())),
    }
}

#[cfg(unix)]
fn open_directory(path: &Path) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("cannot open run-store directory {}", path.display()))?;
    Ok(std::fs::File::from(fd))
}

#[cfg(unix)]
fn open_directory_at(parent: &std::fs::File, name: &str) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;
    let fd = rustix::fs::openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .with_context(|| format!("cannot open run-store directory {name}"))?;
    Ok(std::fs::File::from(fd))
}

#[cfg(unix)]
fn same_file(left: &std::fs::Metadata, right: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_ids_reject_path_components_and_unicode() {
        for raw in ["", ".", "..", "../x", "x/y", "x\\y", "/tmp/x", "é", "a b"] {
            assert!(RunId::parse(raw).is_err(), "accepted {raw:?}");
        }
        assert_eq!(RunId::parse("wf_ABC-123").unwrap().as_str(), "wf_ABC-123");
    }

    #[test]
    fn store_round_trips_atomic_files_and_rejects_symlinks() {
        let base = std::env::temp_dir().join(format!("kloop-run-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let store = RunStore::new(&base.join("offload"), RunNamespace::Workflow).unwrap();
        // The namespace is a sibling of `offload`, so run artifacts follow the
        // session store into its project partition rather than the cwd.
        assert_eq!(
            store.root,
            std::fs::canonicalize(base.join("workflow-runs")).unwrap()
        );
        let run = store.create(&RunId::parse("wf_1").unwrap()).unwrap();
        run.write_atomic("script.js", b"return 1").unwrap();
        assert_eq!(run.read("script.js").unwrap(), b"return 1");
        let lease = run.acquire().unwrap();
        let error = match run.acquire() {
            Ok(_) => panic!("second lease unexpectedly succeeded"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("active"));
        drop(lease);
        drop(run.acquire().unwrap());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&base, store.root.join("wf_link")).unwrap();
            assert!(store.open(&RunId::parse("wf_link").unwrap()).is_err());

            let outside = base.join("outside.txt");
            std::fs::write(&outside, b"outside").unwrap();
            let artifact = run.path().join("journal.jsonl");
            std::os::unix::fs::symlink(&outside, &artifact).unwrap();
            assert!(run.read("journal.jsonl").is_err());
            run.write_atomic("journal.jsonl", b"safe").unwrap();
            assert_eq!(std::fs::read(&outside).unwrap(), b"outside");
            assert_eq!(run.read("journal.jsonl").unwrap(), b"safe");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
