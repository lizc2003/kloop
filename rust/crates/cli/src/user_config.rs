//! The single automatic TOML configuration source: `~/.kloop/config.toml`.
//!
//! This module owns path discovery and root-schema validation. Private-file IO
//! is shared with other application state through [`crate::private_store`]; cwd
//! is never a configuration source.

use std::path::{Path, PathBuf};

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;

use crate::private_store::read_private_string;
#[cfg(test)]
use crate::private_store::write_private_atomic;

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

/// The private state root — `~/.kloop`, parent of the global config. Home
/// resolution only, never a config read: `--list-sessions` has to keep working
/// when `config.toml` itself is unparseable.
pub(crate) fn private_state_root() -> Result<PathBuf> {
    Ok(global_config_path()?
        .parent()
        .expect("global config always has a parent")
        .to_path_buf())
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
                | "effort"
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
