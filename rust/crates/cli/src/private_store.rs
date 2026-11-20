//! Private application-owned file storage.
//!
//! Callers anchor nested state below a private directory; every component and
//! leaf is opened without following symlinks, and nothing is readable beyond
//! the owner. What that costs differs per platform — descriptor-relative
//! `openat` on unix, `NtCreateFile` with `OBJ_DONT_REPARSE` on windows, bare
//! paths where neither exists — so each platform file implements one and the
//! same contract: `read_private_string`, `write_private_atomic`,
//! `validate_private_permissions`, `Dir`, `lock_file` and `unlock_file`.
//! Nothing below this file's four exported symbols is platform-aware.

use std::ffi::OsStr;
use std::io::Read as _;
use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

#[cfg(unix)]
#[path = "private_store/unix.rs"]
mod platform;
#[cfg(windows)]
#[path = "private_store/windows.rs"]
mod platform;
#[cfg(not(any(unix, windows)))]
#[path = "private_store/fallback.rs"]
mod platform;

pub(crate) fn read_private_string(path: &Path, label: &str) -> Result<Option<String>> {
    platform::read_private_string(path, label)
}

pub(crate) fn write_private_atomic(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    platform::write_private_atomic(path, label, bytes)
}

/// 加载失败后看现场的一条结论，对应一个能动手改的地方。只有 unix 实现会产出。
#[derive(Debug, PartialEq, Eq)]
#[cfg_attr(not(unix), allow(dead_code))]
pub(crate) enum Finding {
    /// 这一级是符号链接；检查停在这里，不越过它。
    Link {
        path: std::path::PathBuf,
        directory: bool,
    },
    /// 这一级应是目录却不是；检查停在这里。
    NotDirectory(std::path::PathBuf),
    /// 文件不是普通文件（FIFO、目录、设备……）；检查停在这里。
    NotRegularFile(std::path::PathBuf),
    /// 同组或其他用户可访问。
    LooseMode {
        path: std::path::PathBuf,
        directory: bool,
    },
    /// 打不开或看不了；检查停在这里。
    Inaccessible(std::path::PathBuf),
}

/// 看现场的结果：发现的问题，以及走到文件、确认是普通文件时读到的内容（不管权限位）。
#[derive(Debug, Default)]
pub(crate) struct Inspection {
    pub(crate) findings: Vec<Finding>,
    pub(crate) contents: Option<String>,
}

/// 和正式加载走同一条路径看现场：逐级持有目录句柄、每一级都不跟随链接、打开文件不阻塞，只是遇到
/// 权限位过宽不停下，而是记下来，好让调用方一次给出全部修法。只有 unix 实现；其它平台的加载器
/// 自己的错误已经说清楚（例如 Windows 拒绝一切重解析点），这里不另走一遍。
pub(crate) fn inspect_private_file(root: &Path, components: &[&OsStr], name: &OsStr) -> Inspection {
    platform::inspect_private_file(root, components, name)
}

fn read_opened_private_file(mut file: std::fs::File, label: &str) -> Result<String> {
    validate_private_file(&file, label)?;
    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| anyhow!("cannot read {label}"))?;
    Ok(raw)
}

fn validate_private_file(file: &std::fs::File, label: &str) -> Result<()> {
    let metadata = file
        .metadata()
        .map_err(|_| anyhow!("cannot inspect {label}"))?;
    if !metadata.is_file() {
        bail!("{label} must be a regular file");
    }
    platform::validate_private_permissions(&metadata, label)
}

fn validate_component(name: &OsStr, label: &str) -> Result<()> {
    let path = Path::new(name);
    if path.components().count() != 1 || matches!(path.to_str(), Some(".") | Some("..") | Some(""))
    {
        bail!("invalid private path component for {label}");
    }
    Ok(())
}

pub(crate) struct PrivateDir(platform::Dir);

impl PrivateDir {
    pub(crate) fn open(root: &Path, components: &[&OsStr], label: &str) -> Result<Option<Self>> {
        Ok(platform::Dir::open(root, components, label)?.map(Self))
    }

    pub(crate) fn ensure(root: &Path, components: &[&OsStr], label: &str) -> Result<Self> {
        platform::Dir::ensure(root, components, label).map(Self)
    }

    pub(crate) fn read_string(&self, name: &OsStr, label: &str) -> Result<Option<String>> {
        self.0.read_string(name, label)
    }

    pub(crate) fn write_atomic(&self, name: &OsStr, label: &str, bytes: &[u8]) -> Result<()> {
        self.0.write_atomic(name, label, bytes)
    }

    pub(crate) fn open_lock(&self, name: &OsStr, label: &str) -> Result<ExclusiveFileLock> {
        ExclusiveFileLock::acquire(self.0.open_lock_file(name, label)?, label)
    }
}

pub(crate) struct ExclusiveFileLock {
    file: std::fs::File,
}

impl ExclusiveFileLock {
    fn acquire(file: std::fs::File, label: &str) -> Result<Self> {
        platform::lock_file(&file).with_context(|| format!("cannot lock {label}"))?;
        Ok(Self { file })
    }
}

impl Drop for ExclusiveFileLock {
    fn drop(&mut self) {
        let _ = platform::unlock_file(&self.file);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    fn root(tag: &str) -> PathBuf {
        let path =
            std::env::temp_dir().join(format!("kloop-private-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        path
    }

    #[test]
    fn nested_private_directory_round_trip() {
        let root = root("nested");
        let parent = root.parent().unwrap();
        assert!(parent.is_dir());
        let dir = PrivateDir::ensure(
            &root,
            &[
                OsStr::new("projects"),
                OsStr::new("v1"),
                OsStr::new("p1_test"),
            ],
            "test private store",
        )
        .unwrap();
        dir.write_atomic(OsStr::new("permissions.json"), "test policy", b"secret")
            .unwrap();
        assert_eq!(
            dir.read_string(OsStr::new("permissions.json"), "test policy")
                .unwrap(),
            Some("secret".into())
        );
        let reopened = PrivateDir::open(
            &root,
            &[
                OsStr::new("projects"),
                OsStr::new("v1"),
                OsStr::new("p1_test"),
            ],
            "test private store",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            reopened
                .read_string(OsStr::new("permissions.json"), "test policy")
                .unwrap(),
            Some("secret".into())
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn nested_private_directory_rejects_symlink_component_and_fifo_leaf() {
        use std::os::unix::fs::symlink;

        let root = root("unsafe");
        let dir =
            PrivateDir::ensure(&root, &[OsStr::new("projects")], "test private store").unwrap();
        let outside = root.with_file_name(format!(
            "{}-outside",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, root.join("projects/v1")).unwrap();
        assert!(
            PrivateDir::open(
                &root,
                &[OsStr::new("projects"), OsStr::new("v1")],
                "test private store"
            )
            .is_err()
        );

        let fifo = root.join("projects/policy");
        let status = Command::new("mkfifo").arg(&fifo).status().unwrap();
        assert!(status.success());
        assert!(
            dir.read_string(OsStr::new("policy"), "test policy")
                .is_err()
        );
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn abrupt_process_exit_releases_descriptor_lock() {
        const CHILD_ROOT: &str = "KLOOP_PRIVATE_LOCK_EXIT_CHILD_ROOT";
        if let Some(root) = std::env::var_os(CHILD_ROOT) {
            let dir = PrivateDir::ensure(
                Path::new(&root),
                &[OsStr::new("projects")],
                "test private store",
            )
            .unwrap();
            let _lock = dir
                .open_lock(OsStr::new("policy.lock"), "test policy")
                .unwrap();
            std::process::exit(17);
        }

        let root = root("lock-exit");
        // Captured, not inherited: the child's own test report would land in
        // the middle of the parent's and break every count read off it.
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "private_store::tests::abrupt_process_exit_releases_descriptor_lock",
                "--test-threads=1",
            ])
            .env(CHILD_ROOT, &root)
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(17),
            "child: {}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );

        let dir = PrivateDir::open(&root, &[OsStr::new("projects")], "test private store")
            .unwrap()
            .unwrap();
        let _lock = dir
            .open_lock(OsStr::new("policy.lock"), "test policy")
            .unwrap();
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn nested_private_directory_rejects_junction_component() {
        let root = root("junction");
        PrivateDir::ensure(&root, &[OsStr::new("projects")], "test private store").unwrap();
        let outside = root.with_file_name(format!(
            "{}-outside",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&outside).unwrap();
        let junction = root.join("projects/v1");
        let status = Command::new("cmd")
            .args([
                "/C",
                "mklink",
                "/J",
                junction.to_str().unwrap(),
                outside.to_str().unwrap(),
            ])
            .status()
            .unwrap();
        assert!(status.success(), "mklink /J must be available on NTFS CI");
        let error = PrivateDir::open(
            &root,
            &[OsStr::new("projects"), OsStr::new("v1")],
            "test private store",
        )
        .err()
        .expect("junction component must be rejected");
        assert!(error.to_string().contains("reparse"), "{error:#}");

        let _ = std::fs::remove_dir_all(junction);
        let _ = std::fs::remove_dir_all(root);
        let _ = std::fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn created_directories_and_files_are_private_despite_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = root("mode");
        let dir =
            PrivateDir::ensure(&root, &[OsStr::new("projects")], "test private store").unwrap();
        dir.write_atomic(OsStr::new("policy"), "test policy", b"x")
            .unwrap();
        let lock = dir
            .open_lock(OsStr::new("policy.lock"), "test policy")
            .unwrap();
        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("projects"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(root.join("projects/policy"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(root.join("projects/policy.lock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(lock);
        let names = std::fs::read_dir(root.join("projects"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 2);
        assert!(names.contains(&OsStr::new("policy").to_os_string()));
        assert!(names.contains(&OsStr::new("policy.lock").to_os_string()));
        let _ = std::fs::remove_dir_all(root);
    }
}
