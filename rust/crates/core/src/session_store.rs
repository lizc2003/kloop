//! Where session transcripts and offloaded tool output live on disk.
//!
//! Sessions are global and partitioned by [`ProjectId`] rather than stored
//! below the working directory: one repository has one history no matter which
//! subdirectory or linked worktree started it, and every project on the machine
//! can be enumerated from a single root. The cwd-relative layout it replaces
//! made "show me all my sessions" unanswerable — a session was only findable
//! from the exact directory that created it.
//!
//! The partition directory is shared with the durable project permission store
//! (`permissions.json`), so both are created with owner-only permissions; the
//! policy store rejects a partition any group or other can reach.

use std::io;
use std::path::Path;
use std::path::PathBuf;

use crate::project::ProjectId;
use crate::project::WorkspaceIdentity;

const PROJECTS: &str = "projects";
/// Layout version of one project's private state directory, shared with the
/// permission policy store.
const LAYOUT_VERSION: &str = "v1";
const SESSIONS: &str = "sessions";
const OFFLOAD: &str = "offload";
/// The partition's label file. The CLI's project store writes it too — adding
/// the record of a trust grant — so its shape lives here, in
/// [`project_label_bytes`], rather than being spelled twice.
pub const PROJECT_LABEL: &str = "project.json";

/// One project's session storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionDirs {
    /// Rollout transcripts, `{session-id}.jsonl`.
    pub sessions: PathBuf,
    /// Oversized tool results (`off-NNNN.txt`) and background shell output
    /// (`bg-N.out`).
    pub offload: PathBuf,
}

/// A project partition discovered on disk, for cross-project listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectBucket {
    pub project_id: String,
    /// The path this partition was named after, recovered from `project.json`.
    /// `None` when the file is missing or unreadable — the sessions are still
    /// listable, just unlabelled.
    pub anchor: Option<PathBuf>,
    pub dirs: SessionDirs,
}

/// Root of the session store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionStore {
    /// `~/.kloop`, partitioned by project.
    Global(PathBuf),
    /// `<cwd>/.kloop`, unpartitioned. `--mock` only: the scripted demo resolves
    /// neither HOME nor Git, and must never write into a real project's bucket.
    Hermetic(PathBuf),
}

impl SessionStore {
    /// `root` is the private state directory itself (`~/.kloop`), the same one
    /// that holds `config.toml` and the project permission store.
    pub fn global(root: PathBuf) -> Self {
        Self::Global(root)
    }

    pub fn hermetic(cwd: &Path) -> Self {
        Self::Hermetic(cwd.join(".kloop"))
    }

    pub fn root(&self) -> &Path {
        match self {
            Self::Global(root) | Self::Hermetic(root) => root,
        }
    }

    /// Resolve this working directory's partition without touching the disk.
    /// The global store probes Git here (once per call — callers resolve once
    /// and pass the result around); the hermetic store never does.
    pub fn dirs(&self, cwd: &Path) -> SessionDirs {
        match self {
            Self::Hermetic(root) => dirs_under(root),
            Self::Global(root) => {
                let identity = WorkspaceIdentity::resolve(cwd);
                dirs_under(&partition_dir(root, &identity.session_partition()))
            }
        }
    }

    /// [`Self::dirs`] plus the directory tree, created owner-only, and the
    /// partition's `project.json` label.
    pub fn ensure(&self, cwd: &Path) -> io::Result<SessionDirs> {
        let dirs = match self {
            Self::Hermetic(root) => {
                let dirs = dirs_under(root);
                create_private_dir_all(root)?;
                dirs
            }
            Self::Global(root) => {
                let identity = WorkspaceIdentity::resolve(cwd);
                let partition = identity.session_partition();
                let dir = partition_dir(root, &partition);
                create_private_dir_all(&dir)?;
                write_project_label(&dir, &partition, identity.partition_anchor())?;
                dirs_under(&dir)
            }
        };
        create_private_dir_all(&dirs.sessions)?;
        create_private_dir_all(&dirs.offload)?;
        Ok(dirs)
    }

    /// Every partition that has a sessions directory, for cross-project
    /// listing. Ordering is by partition id — callers that want recency sort
    /// by the transcripts themselves.
    pub fn buckets(&self) -> Vec<ProjectBucket> {
        let root = match self {
            Self::Hermetic(root) => {
                let dirs = dirs_under(root);
                return if dirs.sessions.is_dir() {
                    vec![ProjectBucket {
                        project_id: String::new(),
                        anchor: None,
                        dirs,
                    }]
                } else {
                    Vec::new()
                };
            }
            Self::Global(root) => root.join(PROJECTS).join(LAYOUT_VERSION),
        };
        let Ok(entries) = std::fs::read_dir(&root) else {
            return Vec::new();
        };
        let mut buckets: Vec<ProjectBucket> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let dir = entry.path();
                let dirs = dirs_under(&dir);
                if !dirs.sessions.is_dir() {
                    return None;
                }
                Some(ProjectBucket {
                    project_id: entry.file_name().to_string_lossy().into_owned(),
                    anchor: read_project_anchor(&dir),
                    dirs,
                })
            })
            .collect();
        buckets.sort_by(|a, b| a.project_id.cmp(&b.project_id));
        buckets
    }
}

fn partition_dir(root: &Path, project_id: &ProjectId) -> PathBuf {
    root.join(PROJECTS)
        .join(LAYOUT_VERSION)
        .join(project_id.as_str())
}

fn dirs_under(partition: &Path) -> SessionDirs {
    SessionDirs {
        sessions: partition.join(SESSIONS),
        offload: partition.join(OFFLOAD),
    }
}

/// `create_dir_all` that restricts only the levels it creates. An existing
/// directory keeps its permissions: the hermetic root is a real project's
/// `.kloop/`, which also holds rules and skills the user manages.
fn create_private_dir_all(path: &Path) -> io::Result<()> {
    if path.is_dir() {
        return Ok(());
    }
    if let Some(parent) = path.parent() {
        create_private_dir_all(parent)?;
    }
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// The bytes of one partition's label: the single definition of that file's
/// shape, shared with the CLI store that adds the trust grant to it.
///
/// `granted_at` is absent when nobody was asked — a project this machine's
/// owner only ever ran `--headless` in is labelled, not granted.
pub fn project_label_bytes(
    project_id: &ProjectId,
    anchor: &Path,
    granted_at: Option<&str>,
) -> Vec<u8> {
    let mut label = serde_json::json!({
        "version": 1,
        "project_id": project_id.as_str(),
        "anchor": anchor.to_string_lossy(),
    });
    if let Some(granted_at) = granted_at {
        label["granted_at"] = serde_json::Value::String(granted_at.to_string());
    }
    format!("{label}\n").into_bytes()
}

/// Label the partition with the path it was named after, so a cross-project
/// listing can print real directories instead of digests.
///
/// Written once. A file that already names this project is left byte for byte
/// as it is, which is what lets the CLI record a trust grant in the same file
/// without this write erasing it; the anchor cannot go stale, because it is the
/// hash input the `ProjectId` was derived from. A label that is absent,
/// unreadable, or names a different project is replaced, so a damaged one
/// repairs itself instead of staying wrong forever.
fn write_project_label(dir: &Path, project_id: &ProjectId, anchor: &Path) -> io::Result<()> {
    let path = dir.join(PROJECT_LABEL);
    if let Ok(existing) = std::fs::read_to_string(&path)
        && serde_json::from_str::<serde_json::Value>(&existing).is_ok_and(|value| {
            value.get("project_id").and_then(serde_json::Value::as_str) == Some(project_id.as_str())
        })
    {
        return Ok(());
    }
    std::fs::write(&path, project_label_bytes(project_id, anchor, None))
}

fn read_project_anchor(dir: &Path) -> Option<PathBuf> {
    let raw = std::fs::read_to_string(dir.join(PROJECT_LABEL)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    value
        .get("anchor")
        .and_then(serde_json::Value::as_str)
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use std::process::Command;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(tag: &str) -> PathBuf {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "kloop-session-store-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn init_repository(tag: &str) -> PathBuf {
        let root = temp_dir(tag);
        let status = Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["init", "-q"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");
        root
    }

    #[cfg(unix)]
    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The whole point of the layout: one repository has one history, whichever
    /// subdirectory the session was started from.
    #[test]
    fn one_repository_is_one_partition_whatever_the_cwd() {
        let repo = init_repository("partition");
        let nested = repo.join("crates/deep");
        std::fs::create_dir_all(&nested).unwrap();
        let other = init_repository("partition-other");
        let store = SessionStore::global(temp_dir("partition-root"));

        assert_eq!(store.dirs(&repo), store.dirs(&nested));
        assert_ne!(store.dirs(&repo), store.dirs(&other));
    }

    #[test]
    fn global_layout_puts_sessions_beside_the_project_policy() {
        let repo = init_repository("layout");
        let root = temp_dir("layout-root");
        let store = SessionStore::global(root.clone());
        let dirs = store.ensure(&repo).unwrap();

        let partition = dirs.sessions.parent().unwrap();
        assert_eq!(partition.parent().unwrap(), root.join("projects/v1"));
        assert_eq!(dirs.offload, partition.join("offload"));
        assert!(
            partition
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("p1_")
        );
    }

    /// `--mock` resolves neither HOME nor Git, and must never write into a real
    /// project's bucket.
    #[test]
    fn hermetic_store_stays_cwd_local_and_unpartitioned() {
        let cwd = temp_dir("hermetic");
        let store = SessionStore::hermetic(&cwd);
        let dirs = store.ensure(&cwd).unwrap();

        assert_eq!(dirs.sessions, cwd.join(".kloop/sessions"));
        assert_eq!(dirs.offload, cwd.join(".kloop/offload"));
        // A different cwd argument cannot move a hermetic store.
        assert_eq!(store.dirs(Path::new("/somewhere/else")), dirs);
    }

    /// The partition directory is shared with the durable permission store,
    /// which refuses to open one that group or other can reach. Creating it
    /// with the default mode would silently break project approvals.
    #[cfg(unix)]
    #[test]
    fn created_directories_are_owner_only() {
        let repo = init_repository("mode");
        let root = temp_dir("mode-root");
        let dirs = SessionStore::global(root.clone()).ensure(&repo).unwrap();

        for path in [
            root.join("projects"),
            root.join("projects/v1"),
            dirs.sessions.parent().unwrap().to_path_buf(),
            dirs.sessions.clone(),
            dirs.offload.clone(),
        ] {
            assert_eq!(mode(&path), 0o700, "{}", path.display());
        }
    }

    /// The hermetic root is a real project's `.kloop/`, which also holds rules
    /// and skills the user manages: an existing directory keeps its mode.
    #[cfg(unix)]
    #[test]
    fn existing_directories_keep_their_permissions() {
        use std::os::unix::fs::PermissionsExt as _;
        let cwd = temp_dir("keep-mode");
        let existing = cwd.join(".kloop");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::set_permissions(&existing, std::fs::Permissions::from_mode(0o755)).unwrap();

        SessionStore::hermetic(&cwd).ensure(&cwd).unwrap();

        assert_eq!(mode(&existing), 0o755);
    }

    #[test]
    fn buckets_label_every_partition_with_its_anchor() {
        let repo = init_repository("buckets");
        let plain = temp_dir("buckets-plain");
        let root = temp_dir("buckets-root");
        let store = SessionStore::global(root);
        let repo_dirs = store.ensure(&repo).unwrap();
        let plain_dirs = store.ensure(&plain).unwrap();

        let buckets = store.buckets();
        assert_eq!(buckets.len(), 2);
        let anchors: Vec<PathBuf> = buckets.iter().filter_map(|b| b.anchor.clone()).collect();
        // A repository is anchored on its Git common directory, a bare
        // directory on itself.
        assert!(anchors.contains(&std::fs::canonicalize(repo.join(".git")).unwrap()));
        assert!(anchors.contains(&std::fs::canonicalize(&plain).unwrap()));
        let dirs: Vec<SessionDirs> = buckets.into_iter().map(|b| b.dirs).collect();
        assert!(dirs.contains(&repo_dirs));
        assert!(dirs.contains(&plain_dirs));
    }

    /// The merge's load-bearing property: the CLI writes a trust grant into
    /// this same file, and an ordinary start must not erase it. A label that
    /// already names this project is left byte for byte alone.
    #[test]
    fn a_label_that_already_names_the_project_is_left_alone() {
        let repo = init_repository("label-kept");
        let root = temp_dir("label-kept-root");
        let store = SessionStore::global(root);
        let dirs = store.ensure(&repo).unwrap();
        let partition = dirs.sessions.parent().unwrap().to_path_buf();
        let identity = WorkspaceIdentity::resolve(&repo);
        let granted = project_label_bytes(
            &identity.session_partition(),
            identity.partition_anchor(),
            Some("2026-09-22T07:46:15Z"),
        );
        std::fs::write(partition.join(PROJECT_LABEL), &granted).unwrap();

        store.ensure(&repo).unwrap();

        assert_eq!(
            std::fs::read(partition.join(PROJECT_LABEL)).unwrap(),
            granted
        );
    }

    /// Self-healing survives that: a label naming another project, or one that
    /// is not JSON at all, is rewritten instead of staying wrong forever.
    #[test]
    fn a_foreign_or_unreadable_label_is_repaired() {
        let repo = init_repository("label-repair");
        let root = temp_dir("label-repair-root");
        let store = SessionStore::global(root);
        let dirs = store.ensure(&repo).unwrap();
        let partition = dirs.sessions.parent().unwrap().to_path_buf();
        let identity = WorkspaceIdentity::resolve(&repo);
        let expected = project_label_bytes(
            &identity.session_partition(),
            identity.partition_anchor(),
            None,
        );

        for damaged in [
            String::new(),
            "{".to_string(),
            serde_json::json!({"version": 1, "project_id": "p1_elsewhere", "anchor": "/elsewhere"})
                .to_string(),
        ] {
            std::fs::write(partition.join(PROJECT_LABEL), &damaged).unwrap();
            store.ensure(&repo).unwrap();
            assert_eq!(
                std::fs::read(partition.join(PROJECT_LABEL)).unwrap(),
                expected,
                "damaged label {damaged:?}"
            );
        }
        assert_eq!(
            read_project_anchor(&partition),
            Some(identity.partition_anchor().to_path_buf())
        );
    }

    #[test]
    fn an_empty_store_lists_nothing() {
        let store = SessionStore::global(temp_dir("empty-root"));
        assert_eq!(store.buckets(), Vec::new());
    }
}
