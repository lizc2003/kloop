//! The single automatic TOML configuration source: `~/.kloop/config.toml`.
//!
//! This module owns path discovery, root-schema validation, private-file reads,
//! and atomic private writes. Feature modules parse their own typed sections
//! from the one root table; cwd is never a configuration source.

#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::io::Read as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;

pub(crate) const GLOBAL_CONFIG: &str = ".kloop/config.toml";
pub(crate) const OAUTH_STORE: &str = "mcp-oauth.json";

/// Parsed user configuration. Deliberately no Debug/Serialize: the table can
/// contain provider credentials and MCP headers.
pub(crate) struct UserConfig {
    path: PathBuf,
    table: toml::Table,
}

impl UserConfig {
    /// Load the one user config. `--mock` stays hermetic: no HOME or file read.
    pub(crate) fn load(mock_mode: bool) -> Result<Self> {
        if mock_mode {
            return Ok(Self {
                path: PathBuf::new(),
                table: toml::Table::new(),
            });
        }
        let path = global_config_path()?;
        let table = match read_private_string(&path, "~/.kloop/config.toml")? {
            Some(raw) => parse_root(&raw)?,
            None => toml::Table::new(),
        };
        Ok(Self { path, table })
    }

    #[cfg(test)]
    pub(crate) fn from_parts(path: PathBuf, table: toml::Table) -> Self {
        Self { path, table }
    }

    pub(crate) fn table(&self) -> &toml::Table {
        &self.table
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

pub(crate) fn global_config_path() -> Result<PathBuf> {
    let home =
        std::env::home_dir().context("cannot determine home directory for ~/.kloop/config.toml")?;
    Ok(home.join(GLOBAL_CONFIG))
}

pub(crate) fn oauth_store_path() -> Result<PathBuf> {
    Ok(global_config_path()?
        .parent()
        .expect("global config always has a parent")
        .join(OAUTH_STORE))
}

fn parse_root(raw: &str) -> Result<toml::Table> {
    let table: toml::Table = raw
        .parse()
        .map_err(|_| anyhow!("cannot parse ~/.kloop/config.toml (TOML syntax error)"))?;
    validate_root(&table)?;
    Ok(table)
}

fn validate_root(table: &toml::Table) -> Result<()> {
    for key in table.keys() {
        if !matches!(
            key.as_str(),
            "model"
                | "model_provider"
                | "model_reasoning_effort"
                | "model_providers"
                | "permissions"
                | "mcp"
                | "web"
                | "hooks"
                | "sandbox"
                | "shells"
                | "agents"
                | "codemode"
        ) {
            bail!("~/.kloop/config.toml has unknown top-level key '{key}'");
        }
    }
    Ok(())
}

/// Read an optional secret-bearing file after enforcing the same boundary as
/// the global config: private parent, regular file, and no group/other access.
pub(crate) fn read_private_string(path: &Path, label: &str) -> Result<Option<String>> {
    read_private_string_impl(path, label)
}

#[cfg(unix)]
fn read_private_string_impl(path: &Path, label: &str) -> Result<Option<String>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let Some(dir) = open_private_dir(parent, label)? else {
        return Ok(None);
    };
    let name = private_file_name(path, label)?;
    let fd = match rustix::fs::openat(
        &dir,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(rustix::io::Errno::LOOP) => {
            bail!("{label} must be a regular file, not a symlink");
        }
        Err(_) => bail!("cannot open {label}"),
    };
    read_opened_private_file(std::fs::File::from(fd), label).map(Some)
}

#[cfg(not(unix))]
fn read_private_string_impl(path: &Path, label: &str) -> Result<Option<String>> {
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
        Err(_) => bail!("cannot open {label}"),
    };
    read_opened_private_file(file, label).map(Some)
}

fn read_opened_private_file(mut file: std::fs::File, label: &str) -> Result<String> {
    let metadata = file
        .metadata()
        .map_err(|_| anyhow!("cannot inspect {label}"))?;
    if !metadata.is_file() {
        bail!("{label} must be a regular file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if metadata.permissions().mode() & 0o077 != 0 {
            bail!("{label} contains credentials; restrict it to mode 0600");
        }
    }

    let mut raw = String::new();
    file.read_to_string(&mut raw)
        .map_err(|_| anyhow!("cannot read {label}"))?;
    Ok(raw)
}

/// Atomically replace a private state file. The temporary file is created in
/// the same directory at 0600, so rename never exposes a permissive version.
pub(crate) fn write_private_atomic(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    write_private_atomic_impl(path, label, bytes)
}

#[cfg(unix)]
fn write_private_atomic_impl(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    if let Some(existing) = read_private_string(path, label)? {
        drop(existing);
    }
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    let dir = ensure_private_dir_open(parent, label)?;
    let name = private_file_name(path, label)?;
    let stem = name.to_string_lossy();

    for suffix in 0..100_u8 {
        let temp = format!(".{stem}.tmp-{}-{suffix}", std::process::id());
        let fd = match rustix::fs::openat(
            &dir,
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
            // Creation mode is filtered by umask; enforce the promised final 0600
            // on the opened inode before it can replace the durable file.
            rustix::fs::fchmod(&file, Mode::RUSR | Mode::WUSR)
                .with_context(|| format!("cannot restrict temporary {label}"))?;
            file.write_all(bytes)
                .with_context(|| format!("cannot write temporary {label}"))?;
            file.sync_all()
                .with_context(|| format!("cannot sync temporary {label}"))?;
            rustix::fs::renameat(&dir, temp.as_str(), &dir, name)
                .with_context(|| format!("cannot replace {label}"))?;
            let _ = dir.sync_all();
            Ok(())
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&dir, temp.as_str(), AtFlags::empty());
        }
        return result;
    }
    bail!("cannot create temporary {label}")
}

#[cfg(not(unix))]
fn write_private_atomic_impl(path: &Path, label: &str, bytes: &[u8]) -> Result<()> {
    if let Some(existing) = read_private_string(path, label)? {
        drop(existing);
    }
    let parent = path
        .parent()
        .with_context(|| format!("{label} has no parent directory"))?;
    ensure_private_dir(parent, label)?;

    let stem = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("private");
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
                    std::fs::rename(&temp, path)
                        .with_context(|| format!("cannot replace {label}"))?;
                    if let Ok(dir) = std::fs::File::open(parent) {
                        let _ = dir.sync_all();
                    }
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

#[cfg(unix)]
fn private_file_name<'a>(path: &'a Path, label: &str) -> Result<&'a std::ffi::OsStr> {
    path.file_name()
        .with_context(|| format!("{label} has no file name"))
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn private_dir_access_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::PATH
}

#[cfg(target_os = "macos")]
fn private_dir_access_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::from_bits_retain(libc::O_SEARCH as _)
}

#[cfg(all(
    unix,
    not(any(target_os = "linux", target_os = "android", target_os = "macos"))
))]
fn private_dir_access_flags() -> rustix::fs::OFlags {
    rustix::fs::OFlags::RDONLY
}

#[cfg(unix)]
fn validate_private_dir(dir: std::fs::File, label: &str) -> Result<std::fs::File> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = dir
        .metadata()
        .map_err(|_| anyhow!("cannot inspect directory for {label}"))?;
    if !metadata.is_dir() {
        bail!("directory for {label} must be a regular directory, not a symlink");
    }
    if metadata.permissions().mode() & 0o077 != 0 {
        bail!("directory for {label} must not be accessible by group or other");
    }
    Ok(dir)
}

#[cfg(unix)]
fn open_private_dir(path: &Path, label: &str) -> Result<Option<std::fs::File>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::open(
        path,
        private_dir_access_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
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

#[cfg(unix)]
fn open_private_dir_at(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    label: &str,
) -> Result<Option<std::fs::File>> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;

    let fd = match rustix::fs::openat(
        parent,
        name,
        private_dir_access_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
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

#[cfg(unix)]
fn open_stable_parent_dir(path: &Path, label: &str) -> Result<std::fs::File> {
    use rustix::fs::Mode;
    use rustix::fs::OFlags;
    use std::os::unix::fs::PermissionsExt as _;

    let fd = rustix::fs::open(
        path,
        private_dir_access_flags() | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
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

#[cfg(unix)]
fn ensure_private_dir_open(path: &Path, label: &str) -> Result<std::fs::File> {
    use rustix::fs::AtFlags;
    use rustix::fs::Mode;

    let parent = path
        .parent()
        .with_context(|| format!("directory for {label} has no parent"))?;
    let name = private_file_name(path, label)?;
    let anchor = open_stable_parent_dir(parent, label)?;
    if let Some(dir) = open_private_dir_at(&anchor, name, label)? {
        return Ok(dir);
    }
    match rustix::fs::mkdirat(&anchor, name, Mode::RUSR | Mode::WUSR | Mode::XUSR) {
        Ok(()) => {
            // mkdir mode is filtered by umask; restore the promised private mode
            // while the non-writable parent descriptor keeps the name stable.
            rustix::fs::chmodat(
                &anchor,
                name,
                Mode::RUSR | Mode::WUSR | Mode::XUSR,
                AtFlags::empty(),
            )
            .with_context(|| format!("cannot restrict directory for {label}"))?;
        }
        Err(rustix::io::Errno::EXIST) => {}
        Err(_) => bail!("cannot create directory for {label}"),
    }
    open_private_dir_at(&anchor, name, label)?
        .with_context(|| format!("cannot open directory for {label}"))
}

#[cfg(not(unix))]
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

#[cfg(not(unix))]
fn ensure_private_dir(path: &Path, label: &str) -> Result<()> {
    if inspect_private_dir(path, label)? {
        return Ok(());
    }
    std::fs::create_dir_all(path)
        .with_context(|| format!("cannot create directory for {label}"))?;
    Ok(())
}

/// Append global allow rules while preserving every other section semantically.
pub(crate) fn persist_allow_rules(path: &Path, new_rules: &[String]) -> Result<()> {
    let mut table = match read_private_string(path, "~/.kloop/config.toml")? {
        Some(raw) => parse_root(&raw)?,
        None => toml::Table::new(),
    };
    let permissions = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("[permissions] must be a table")?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .context("permissions.allow must be an array")?;
    for rule in new_rules {
        if !allow.iter().any(|value| value.as_str() == Some(rule)) {
            allow.push(toml::Value::String(rule.clone()));
        }
    }
    let encoded = toml::to_string_pretty(&table)?;
    write_private_atomic(path, "~/.kloop/config.toml", encoded.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kloop-user-config-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        dir.join("config.toml")
    }

    #[test]
    fn mock_load_is_an_empty_pathless_snapshot() {
        let config = UserConfig::load(/*mock_mode=*/ true).unwrap();
        assert!(config.path().as_os_str().is_empty());
        assert!(config.table().is_empty());
    }

    #[test]
    fn private_read_missing_parent_is_none_without_creating_it() {
        let parent = std::env::temp_dir().join(format!(
            "kloop-user-config-{}-missing-parent",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&parent);
        let path = parent.join("config.toml");

        assert_eq!(read_private_string(&path, "test config").unwrap(), None);
        assert!(!parent.exists());
    }

    #[test]
    fn root_accepts_provider_and_runtime_sections() {
        let table = parse_root(
            r#"
model = "m"
model_provider = "openai"
[permissions]
allow = ["bash(cargo test *)"]
[web]
search_provider = "brave"
[mcp.servers.local]
command = ["server"]
[[hooks]]
event = "pre_turn"
command = ["true"]
[sandbox]
enabled = true
[shells]
bash = "/trusted/bash.exe"
[agents.reviewer]
description = "review"
[codemode]
max_agents = 3
"#,
        )
        .unwrap();
        assert_eq!(table["model"].as_str(), Some("m"));
        assert!(table.contains_key("permissions"));
        assert!(table.contains_key("mcp"));
    }

    #[test]
    fn root_rejects_unknown_keys_without_echoing_values() {
        let error = parse_root("mystery = \"sentinel-secret\"")
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown top-level key 'mystery'"));
        assert!(!error.contains("sentinel-secret"));
    }

    #[test]
    fn persist_allow_is_private_atomic_and_preserves_sections() {
        let path = path("persist");
        write_private_atomic(
            &path,
            "test config",
            b"model = \"m\"\n[web]\nsearch_provider = \"brave\"\n",
        )
        .unwrap();
        persist_allow_rules(&path, &["bash(cargo test *)".into()]).unwrap();
        persist_allow_rules(&path, &["bash(cargo test *)".into()]).unwrap();

        let raw = read_private_string(&path, "test config").unwrap().unwrap();
        let table: toml::Table = raw.parse().unwrap();
        assert_eq!(table["model"].as_str(), Some("m"));
        assert_eq!(table["web"]["search_provider"].as_str(), Some("brave"));
        assert_eq!(
            table["permissions"]["allow"].as_array().unwrap(),
            &[toml::Value::String("bash(cargo test *)".into())]
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn private_write_enforces_0600_under_restrictive_umask() {
        use std::os::unix::fs::PermissionsExt as _;

        const CHILD: &str = "KLOOP_PRIVATE_UMASK_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new("/bin/sh")
                .args([
                    "-c",
                    "umask 0700; exec \"$1\" --exact \
                     user_config::tests::private_write_enforces_0600_under_restrictive_umask",
                    "sh",
                ])
                .arg(std::env::current_exe().unwrap())
                .env(CHILD, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "child failed: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let anchor_path = path("restrictive-umask-anchor");
        let anchor = anchor_path.parent().unwrap();
        let private_dir = anchor.join("private");
        let config_path = private_dir.join("config.toml");
        write_private_atomic(&config_path, "test config", b"model = \"m\"\n").unwrap();
        assert_eq!(
            std::fs::metadata(&private_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(anchor);
    }

    #[cfg(unix)]
    #[test]
    fn private_write_creates_private_directory_and_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let anchor_path = path("create-private-anchor");
        let anchor = anchor_path.parent().unwrap();
        let private_dir = anchor.join("private");
        let config_path = private_dir.join("config.toml");

        write_private_atomic(&config_path, "test config", b"model = \"m\"\n").unwrap();

        assert_eq!(
            std::fs::metadata(&private_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&config_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let _ = std::fs::remove_dir_all(anchor);
    }

    #[cfg(unix)]
    #[test]
    fn private_writes_reject_open_directories_without_creating_a_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = path("open-dir");
        let parent = path.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(write_private_atomic(&path, "test config", b"model = \"m\"\n").is_err());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(parent);
    }

    #[cfg(any(target_os = "linux", target_os = "android", target_os = "macos"))]
    #[test]
    fn private_reads_allow_owner_search_only_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let path = path("read-search-only-dir");
        std::fs::write(&path, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let parent = path.parent().unwrap();
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o100)).unwrap();

        assert_eq!(
            read_private_string(&path, "test config").unwrap(),
            Some("model = \"m\"\n".into())
        );

        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700)).unwrap();
        let _ = std::fs::remove_dir_all(parent);
    }

    #[cfg(unix)]
    #[test]
    fn private_io_rejects_open_and_symlinked_directories() {
        use std::os::unix::fs::PermissionsExt as _;

        let open_path = path("read-open-dir");
        std::fs::write(&open_path, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&open_path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let open_parent = open_path.parent().unwrap();
        std::fs::set_permissions(open_parent, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(read_private_string(&open_path, "test config").is_err());
        let _ = std::fs::remove_dir_all(open_parent);

        let target = path("read-symlink-dir");
        std::fs::write(&target, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let target_parent = target.parent().unwrap();
        let link_parent = target_parent.with_file_name(format!(
            "kloop-user-config-{}-read-dir-link",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&link_parent);
        std::os::unix::fs::symlink(target_parent, &link_parent).unwrap();
        let linked_config = link_parent.join("config.toml");
        assert!(read_private_string(&linked_config, "test config").is_err());
        assert!(write_private_atomic(&linked_config, "test config", b"replacement").is_err());
        let _ = std::fs::remove_file(link_parent);
        let _ = std::fs::remove_dir_all(target_parent);
    }

    #[cfg(unix)]
    #[test]
    fn private_reads_reject_fifo_without_blocking() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;
        use std::time::Instant;

        const CHILD_PATH: &str = "KLOOP_PRIVATE_FIFO_TEST_PATH";
        if let Some(path) = std::env::var_os(CHILD_PATH) {
            let error = read_private_string(Path::new(&path), "test config")
                .unwrap_err()
                .to_string();
            assert!(error.contains("regular file"));
            return;
        }

        let fifo = path("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        std::fs::set_permissions(&fifo, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "user_config::tests::private_reads_reject_fifo_without_blocking",
            ])
            .env(CHILD_PATH, &fifo)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        let child_status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_dir_all(fifo.parent().unwrap());
                panic!("private FIFO read blocked");
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        let _ = std::fs::remove_dir_all(fifo.parent().unwrap());
        assert!(child_status.success());
    }

    #[cfg(unix)]
    #[test]
    fn private_reads_reject_symlinks_and_open_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let target = path("private");
        std::fs::write(&target, "model = \"m\"\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_private_string(&target, "test config").is_err());
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();

        let link = target.with_file_name("link.toml");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let error = read_private_string(&link, "test config")
            .unwrap_err()
            .to_string();
        assert!(error.contains("symlink"));
        let _ = std::fs::remove_dir_all(target.parent().unwrap());
    }
}
