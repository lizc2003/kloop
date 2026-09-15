//! Stable, machine-local project and workspace identities.
//!
//! Repository content is never an authorization source. Git contributes only
//! canonical topology: a common directory partitions durable project state and
//! a checkout root partitions session-scoped workspace state.

use std::ffi::OsStr;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::str::FromStr;

use sha2::Digest as _;
use sha2::Sha256;

const PROJECT_GIT_DOMAIN: &[u8] = b"kloop-project/git/v1\0";
const WORKSPACE_GIT_DOMAIN: &[u8] = b"kloop-workspace/git/v1\0";
const PROJECT_DIRECTORY_DOMAIN: &[u8] = b"kloop-project/directory/v1\0";
const WORKSPACE_DIRECTORY_DOMAIN: &[u8] = b"kloop-workspace/directory/v1\0";
const WORKSPACE_UNAVAILABLE_DOMAIN: &[u8] = b"kloop-workspace/unavailable/v1\0";
const GIT_IDENTITY_ENV_VARS: [&str; 12] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_INDEX_FILE",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_SYSTEM",
];
#[cfg(unix)]
const GIT_NULL_CONFIG: &str = "/dev/null";
#[cfg(windows)]
const GIT_NULL_CONFIG: &str = "NUL";
#[cfg(not(any(unix, windows)))]
const GIT_NULL_CONFIG: &str = "/dev/null";

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("ProjectId").field(&self.0).finish()
    }
}

impl fmt::Display for ProjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for ProjectId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_id(value, "p1_")?;
        Ok(Self(value.to_string()))
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct WorkspaceId(String);

impl WorkspaceId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("WorkspaceId").field(&self.0).finish()
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for WorkspaceId {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        validate_id(value, "w1_")?;
        Ok(Self(value.to_string()))
    }
}

fn validate_id(value: &str, prefix: &str) -> Result<(), &'static str> {
    let Some(digest) = value.strip_prefix(prefix) else {
        return Err("identity has the wrong version prefix");
    };
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err("identity must contain one full lowercase SHA-256 digest");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IdentityUnavailableReason {
    CwdCanonicalization,
    GitUnavailable,
    GitProbeFailed,
    GitOutputInvalid,
    GitPathCanonicalization,
    NotWorktree,
}

impl fmt::Display for IdentityUnavailableReason {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::CwdCanonicalization => "working directory could not be canonicalized",
            Self::GitUnavailable => "Git could not be executed",
            Self::GitProbeFailed => "Git workspace probing failed",
            Self::GitOutputInvalid => "Git workspace probing returned invalid output",
            Self::GitPathCanonicalization => "Git workspace paths could not be canonicalized",
            Self::NotWorktree => "the Git directory is not a worktree",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProjectIdentityStatus {
    Available,
    Unavailable(IdentityUnavailableReason),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    project_id: Option<ProjectId>,
    workspace_id: WorkspaceId,
    cwd: PathBuf,
    workspace_root: PathBuf,
    project_anchor: Option<PathBuf>,
    status: ProjectIdentityStatus,
}

fn git_probe_command(canonical_cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(canonical_cwd)
        .args([
            "rev-parse",
            "--path-format=absolute",
            "--is-inside-work-tree",
            "--show-toplevel",
            "--git-common-dir",
        ])
        .env("LC_ALL", "C")
        .env("LANG", "C");
    for name in GIT_IDENTITY_ENV_VARS {
        command.env_remove(name);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", GIT_NULL_CONFIG);
    command
}

fn git_marker_root(canonical_cwd: &Path) -> Option<&Path> {
    canonical_cwd.ancestors().find(|ancestor| {
        match std::fs::symlink_metadata(ancestor.join(".git")) {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(_) => true,
        }
    })
}

fn has_git_marker(canonical_cwd: &Path) -> bool {
    git_marker_root(canonical_cwd).is_some()
}

impl WorkspaceIdentity {
    /// Process-local identity for hermetic mock/test surfaces. It never probes
    /// Git and never enables project persistence.
    pub fn ephemeral(cwd: PathBuf) -> Self {
        Self::unavailable(cwd, IdentityUnavailableReason::GitUnavailable)
    }

    pub fn resolve(cwd: &Path) -> Self {
        let canonical_cwd = match std::fs::canonicalize(cwd) {
            Ok(cwd) => cwd,
            Err(_) => {
                let stable_cwd = std::path::absolute(cwd).unwrap_or_else(|_| cwd.to_path_buf());
                return Self::unavailable(
                    stable_cwd,
                    IdentityUnavailableReason::CwdCanonicalization,
                );
            }
        };

        let mut command = git_probe_command(&canonical_cwd);
        let output = match kloop_process_spawn::output_std(&mut command) {
            Ok(output) => output,
            Err(_) => {
                return Self::unavailable(canonical_cwd, IdentityUnavailableReason::GitUnavailable);
            }
        };
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            if stderr.contains("not a git repository") && !has_git_marker(&canonical_cwd) {
                return Self::directory(canonical_cwd);
            }
            return Self::unavailable(canonical_cwd, IdentityUnavailableReason::GitProbeFailed);
        }
        let Ok(stdout) = std::str::from_utf8(&output.stdout) else {
            return Self::unavailable(canonical_cwd, IdentityUnavailableReason::GitOutputInvalid);
        };
        let lines: Vec<&str> = stdout.lines().collect();
        if lines.len() != 3 || lines[0] != "true" || lines[1].is_empty() || lines[2].is_empty() {
            let reason = if lines.first() == Some(&"false") {
                IdentityUnavailableReason::NotWorktree
            } else {
                IdentityUnavailableReason::GitOutputInvalid
            };
            return Self::unavailable(canonical_cwd, reason);
        }
        let Ok(workspace_root) = std::fs::canonicalize(lines[1]) else {
            return Self::unavailable(
                canonical_cwd,
                IdentityUnavailableReason::GitPathCanonicalization,
            );
        };
        if git_marker_root(&canonical_cwd) != Some(workspace_root.as_path()) {
            return Self::unavailable(canonical_cwd, IdentityUnavailableReason::GitOutputInvalid);
        }
        let Ok(project_anchor) = std::fs::canonicalize(lines[2]) else {
            return Self::unavailable(
                canonical_cwd,
                IdentityUnavailableReason::GitPathCanonicalization,
            );
        };

        Self {
            project_id: Some(ProjectId(hash_id(
                "p1_",
                PROJECT_GIT_DOMAIN,
                project_anchor.as_os_str(),
            ))),
            workspace_id: WorkspaceId(hash_id(
                "w1_",
                WORKSPACE_GIT_DOMAIN,
                workspace_root.as_os_str(),
            )),
            cwd: canonical_cwd,
            workspace_root,
            project_anchor: Some(project_anchor),
            status: ProjectIdentityStatus::Available,
        }
    }

    fn directory(canonical_cwd: PathBuf) -> Self {
        Self {
            project_id: Some(ProjectId(hash_id(
                "p1_",
                PROJECT_DIRECTORY_DOMAIN,
                canonical_cwd.as_os_str(),
            ))),
            workspace_id: WorkspaceId(hash_id(
                "w1_",
                WORKSPACE_DIRECTORY_DOMAIN,
                canonical_cwd.as_os_str(),
            )),
            workspace_root: canonical_cwd.clone(),
            project_anchor: Some(canonical_cwd.clone()),
            cwd: canonical_cwd,
            status: ProjectIdentityStatus::Available,
        }
    }

    fn unavailable(cwd: PathBuf, reason: IdentityUnavailableReason) -> Self {
        Self {
            project_id: None,
            workspace_id: WorkspaceId(hash_id(
                "w1_",
                WORKSPACE_UNAVAILABLE_DOMAIN,
                cwd.as_os_str(),
            )),
            workspace_root: cwd.clone(),
            project_anchor: None,
            cwd,
            status: ProjectIdentityStatus::Unavailable(reason),
        }
    }

    pub fn project_id(&self) -> Option<&ProjectId> {
        self.project_id.as_ref()
    }

    /// Storage partition for session transcripts and offloaded tool output.
    /// Project *policy* stays disabled when Git identity is unavailable —
    /// repository content must never grant authority — but a transcript still
    /// has to land somewhere, so a failed probe degrades to the directory
    /// domain instead of losing the session.
    pub fn session_partition(&self) -> ProjectId {
        self.project_id.clone().unwrap_or_else(|| {
            ProjectId(hash_id(
                "p1_",
                PROJECT_DIRECTORY_DOMAIN,
                self.cwd.as_os_str(),
            ))
        })
    }

    /// The path [`Self::session_partition`] is named after: the Git common
    /// directory when the probe succeeded, else the directory itself. Display
    /// only — never an identity or authorization source.
    pub fn partition_anchor(&self) -> &Path {
        self.project_anchor.as_deref().unwrap_or(&self.cwd)
    }

    pub fn workspace_id(&self) -> &WorkspaceId {
        &self.workspace_id
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn workspace_root(&self) -> &Path {
        &self.workspace_root
    }

    pub fn status(&self) -> ProjectIdentityStatus {
        self.status.clone()
    }

    pub fn project_available(&self) -> bool {
        self.project_id.is_some()
    }
}

fn hash_id(prefix: &str, domain: &[u8], path: &OsStr) -> String {
    let mut hasher = Sha256::new();
    hasher.update(domain);
    update_path_digest(&mut hasher, path);
    format!("{prefix}{:x}", hasher.finalize())
}

#[cfg(unix)]
fn update_path_digest(hasher: &mut Sha256, path: &OsStr) {
    use std::os::unix::ffi::OsStrExt as _;
    hasher.update(path.as_bytes());
}

#[cfg(windows)]
fn update_path_digest(hasher: &mut Sha256, path: &OsStr) {
    use std::os::windows::ffi::OsStrExt as _;
    for unit in path.encode_wide() {
        hasher.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn update_path_digest(hasher: &mut Sha256, path: &OsStr) {
    hasher.update(path.to_string_lossy().as_bytes());
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-project-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn git(cwd: &Path, args: &[&str]) {
        let mut command = Command::new("git");
        command.arg("-C").arg(cwd).args(args).env("LC_ALL", "C");
        for name in GIT_IDENTITY_ENV_VARS {
            command.env_remove(name);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", GIT_NULL_CONFIG);
        let status = command.status().unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn init_repository(tag: &str) -> PathBuf {
        let root = temp_dir(tag);
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("seed"), "seed\n").unwrap();
        git(&root, &["add", "seed"]);
        git(
            &root,
            &[
                "-c",
                "user.name=kloop",
                "-c",
                "user.email=kloop@example.invalid",
                "commit",
                "-qm",
                "seed",
            ],
        );
        root
    }

    #[test]
    fn linked_worktrees_share_project_but_not_workspace() {
        let root = init_repository("linked");
        let tree_a = root.with_file_name(format!(
            "{}-tree-a",
            root.file_name().unwrap().to_string_lossy()
        ));
        let tree_b = root.with_file_name(format!(
            "{}-tree-b",
            root.file_name().unwrap().to_string_lossy()
        ));
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "tree-a",
                tree_a.to_str().unwrap(),
            ],
        );
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "tree-b",
                tree_b.to_str().unwrap(),
            ],
        );

        let main = WorkspaceIdentity::resolve(&root);
        let first = WorkspaceIdentity::resolve(&tree_a);
        let second = WorkspaceIdentity::resolve(&tree_b);
        assert_eq!(main.project_id(), first.project_id());
        assert_eq!(main.project_id(), second.project_id());
        assert_ne!(main.workspace_id(), first.workspace_id());
        assert_ne!(first.workspace_id(), second.workspace_id());
        assert!(main.project_id().unwrap().as_str().starts_with("p1_"));
        assert_eq!(main.project_id().unwrap().as_str().len(), 67);

        let _ = std::fs::remove_dir_all(tree_a);
        let _ = std::fs::remove_dir_all(tree_b);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn nested_cwd_keeps_checkout_identity_and_effective_cwd() {
        let root = init_repository("nested");
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        let top = WorkspaceIdentity::resolve(&root);
        let child = WorkspaceIdentity::resolve(&nested);
        assert_eq!(top.project_id(), child.project_id());
        assert_eq!(top.workspace_id(), child.workspace_id());
        assert_eq!(child.cwd(), std::fs::canonicalize(&nested).unwrap());
        assert_eq!(
            child.workspace_root(),
            std::fs::canonicalize(&root).unwrap()
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn non_git_identity_is_canonical_and_domain_separated() {
        let root = temp_dir("directory");
        let identity = WorkspaceIdentity::resolve(&root);
        assert!(identity.project_available());
        assert_ne!(
            identity
                .project_id()
                .unwrap()
                .as_str()
                .trim_start_matches("p1_"),
            identity.workspace_id().as_str().trim_start_matches("w1_")
        );
        assert_eq!(identity.cwd(), std::fs::canonicalize(&root).unwrap());

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let alias = root.with_file_name(format!(
                "{}-alias",
                root.file_name().unwrap().to_string_lossy()
            ));
            symlink(&root, &alias).unwrap();
            assert_eq!(identity, WorkspaceIdentity::resolve(&alias));
            std::fs::remove_file(alias).unwrap();
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn git_probe_ignores_repository_selection_environment() {
        let command = git_probe_command(Path::new("/tmp"));
        for name in GIT_IDENTITY_ENV_VARS {
            assert!(
                command
                    .get_envs()
                    .any(|(key, value)| key == OsStr::new(name) && value.is_none()),
                "{name} was inherited by the identity probe"
            );
        }
        let configured = |name: &str| {
            command
                .get_envs()
                .find(|(key, _)| *key == OsStr::new(name))
                .and_then(|(_, value)| value)
        };
        assert_eq!(configured("GIT_CONFIG_NOSYSTEM"), Some(OsStr::new("1")));
        assert_eq!(
            configured("GIT_CONFIG_GLOBAL"),
            Some(OsStr::new(GIT_NULL_CONFIG))
        );
    }
    #[test]
    fn local_core_worktree_cannot_redirect_workspace_identity() {
        let root = init_repository("core-worktree");
        let redirected = temp_dir("core-worktree-target");
        git(
            &root,
            &["config", "core.worktree", redirected.to_str().unwrap()],
        );

        let identity = WorkspaceIdentity::resolve(&root);
        assert_eq!(identity.project_id(), None);
        assert!(matches!(
            identity.status(),
            ProjectIdentityStatus::Unavailable(
                IdentityUnavailableReason::NotWorktree
                    | IdentityUnavailableReason::GitOutputInvalid
                    | IdentityUnavailableReason::GitProbeFailed
            )
        ));

        let _ = std::fs::remove_dir_all(redirected);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn broken_git_marker_disables_project_policy_instead_of_becoming_a_directory_project() {
        let root = temp_dir("broken-marker");
        std::fs::write(root.join(".git"), "gitdir: missing-git-dir\n").unwrap();
        let identity = WorkspaceIdentity::resolve(&root);
        assert_eq!(identity.project_id(), None);
        assert_eq!(
            identity.status(),
            ProjectIdentityStatus::Unavailable(IdentityUnavailableReason::GitProbeFailed)
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn missing_cwd_disables_project_scope_without_exposing_path_in_id() {
        let root = temp_dir("missing");
        std::fs::remove_dir_all(&root).unwrap();
        let identity = WorkspaceIdentity::resolve(&root);
        assert_eq!(identity.project_id(), None);
        assert_eq!(
            identity.status(),
            ProjectIdentityStatus::Unavailable(IdentityUnavailableReason::CwdCanonicalization)
        );
        assert!(!identity.workspace_id().as_str().contains("kloop-project"));
        assert_eq!(identity.workspace_id().as_str().len(), 67);
    }

    #[test]
    fn ids_reject_truncation_uppercase_and_wrong_prefix() {
        let valid = format!("p1_{}", "a".repeat(64));
        assert!(ProjectId::from_str(&valid).is_ok());
        assert!(ProjectId::from_str(&format!("p1_{}", "a".repeat(63))).is_err());
        assert!(ProjectId::from_str(&format!("p1_{}", "A".repeat(64))).is_err());
        assert!(ProjectId::from_str(&format!("w1_{}", "a".repeat(64))).is_err());
    }
}
