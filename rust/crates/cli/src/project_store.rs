//! User-private, project-scoped durable state: the allow rules a human
//! granted for this project, and whether this project is trusted at all
//! (plan 193). Trust is not a rule — it is whether the rules get to start — so
//! it is not a row in the rule table. It is a note on the partition's own
//! label file (`project.json`), written once, at the moment the directory is
//! created; the decision itself is that directory existing.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use serde::Deserialize;
use serde::Serialize;

use kloop_core::permissions::ProjectAllowRules;
use kloop_core::permissions::ProjectPermissionWriter;
use kloop_core::permissions::ProjectPolicySnapshot;
use kloop_core::permissions::ProjectPolicyStoreError;
use kloop_core::project::ProjectId;
use kloop_core::session_store::PROJECT_LABEL;
use kloop_core::session_store::project_label_bytes;

use crate::private_store::PrivateDir;

const PROJECTS: &str = "projects";
const VERSION: &str = "v1";
const POLICY_FILE: &str = "permissions.json";
const LOCK_FILE: &str = "permissions.lock";
const POLICY_LABEL: &str = "project permission policy";
const LABEL_LABEL: &str = "project label";

#[derive(Clone)]
pub(crate) struct ProjectStore {
    root: Arc<PathBuf>,
    locks: Arc<Mutex<HashMap<ProjectId, Arc<tokio::sync::Mutex<()>>>>>,
}

impl ProjectStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root: Arc::new(root),
            locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[cfg(test)]
    pub(crate) async fn load(
        &self,
        project_id: &ProjectId,
    ) -> std::result::Result<ProjectPolicySnapshot, ProjectPolicyStoreError> {
        let store = self.clone();
        let project_id = project_id.clone();
        tokio::task::spawn_blocking(move || store.load_blocking(&project_id))
            .await
            .map_err(|_| ProjectPolicyStoreError::Unavailable)?
            .map_err(|_| ProjectPolicyStoreError::Invalid)
    }

    pub(crate) fn load_blocking(&self, project_id: &ProjectId) -> Result<ProjectPolicySnapshot> {
        let Some(dir) = self.open_project(project_id)? else {
            return Ok(ProjectPolicySnapshot::empty());
        };
        read_policy(&dir, project_id)
    }

    async fn append(
        &self,
        project_id: ProjectId,
        additions: ProjectAllowRules,
    ) -> std::result::Result<ProjectPolicySnapshot, ProjectPolicyStoreError> {
        let project_lock = self.project_lock(&project_id);
        let _guard = project_lock.lock().await;
        let store = self.clone();
        let result =
            tokio::task::spawn_blocking(move || store.append_blocking(&project_id, additions))
                .await
                .map_err(|_| ProjectPolicyStoreError::PersistenceFailed)?;
        result.map_err(|_| ProjectPolicyStoreError::PersistenceFailed)
    }

    fn append_blocking(
        &self,
        project_id: &ProjectId,
        additions: ProjectAllowRules,
    ) -> Result<ProjectPolicySnapshot> {
        let dir = self.ensure_project(project_id)?;
        let _lock = dir.open_lock(OsStr::new(LOCK_FILE), POLICY_LABEL)?;
        let current = read_policy(&dir, project_id)?;
        let mut allow = current.allow.raw().to_vec();
        let mut changed = false;
        for rule in additions.raw() {
            if !allow.contains(rule) {
                allow.push(rule.clone());
                changed = true;
            }
        }
        if !changed {
            return Ok(current);
        }
        let revision = current
            .revision
            .checked_add(1)
            .context("project permission revision overflow")?;
        let parsed = ProjectAllowRules::parse(&allow)?;
        let file = StoreFile {
            version: 1,
            project_id: project_id.as_str().to_string(),
            revision,
            allow,
        };
        let mut encoded = serde_json::to_vec_pretty(&file)?;
        encoded.push(b'\n');
        dir.write_atomic(OsStr::new(POLICY_FILE), POLICY_LABEL, &encoded)?;
        Ok(ProjectPolicySnapshot {
            revision,
            allow: parsed,
        })
    }

    /// Whether this project is one kloop has state for. The directory under
    /// the private state root is the whole answer: nothing but kloop creates
    /// it, so its existence means this machine's owner has already run here —
    /// whether it holds a grant record, transcripts, or durable approvals.
    /// That is also why the grant below writes a file at all: creating this
    /// directory is its job, the contents are for a human to read.
    pub(crate) fn trusted_blocking(&self, project_id: &ProjectId) -> bool {
        self.project_dir(project_id).is_dir()
    }

    /// Record the human's "yes" — which creates the project directory and
    /// writes the partition's label with the grant in it. A failure to persist
    /// is reported: the session still runs on the answer just given, but the
    /// next one asks again.
    ///
    /// No lock, because there is nothing to read back and modify: this runs
    /// only when the directory is absent, so the file it writes is one no
    /// other writer has touched, and two launches answering at once only
    /// decide whose timestamp wins.
    pub(crate) fn grant_trust_blocking(&self, project_id: &ProjectId, anchor: &Path) -> Result<()> {
        let dir = self.ensure_project(project_id)?;
        let label = project_label_bytes(
            project_id,
            anchor,
            Some(&kloop_core::context::utc_now_timestamp()),
        )?;
        dir.write_atomic(OsStr::new(PROJECT_LABEL), LABEL_LABEL, &label)
    }

    fn project_lock(&self, project_id: &ProjectId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap();
        Arc::clone(
            locks
                .entry(project_id.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    fn components<'a>(&self, project_id: &'a ProjectId) -> [&'a OsStr; 3] {
        [
            OsStr::new(PROJECTS),
            OsStr::new(VERSION),
            OsStr::new(project_id.as_str()),
        ]
    }

    fn open_project(&self, project_id: &ProjectId) -> Result<Option<PrivateDir>> {
        PrivateDir::open(&self.root, &self.components(project_id), POLICY_LABEL)
    }

    fn ensure_project(&self, project_id: &ProjectId) -> Result<PrivateDir> {
        PrivateDir::ensure(&self.root, &self.components(project_id), POLICY_LABEL)
    }

    #[cfg(test)]
    pub(crate) fn policy_path(&self, project_id: &ProjectId) -> PathBuf {
        self.project_dir(project_id).join(POLICY_FILE)
    }

    #[cfg(test)]
    pub(crate) fn label_path(&self, project_id: &ProjectId) -> PathBuf {
        self.project_dir(project_id).join(PROJECT_LABEL)
    }

    fn project_dir(&self, project_id: &ProjectId) -> PathBuf {
        self.root
            .join(PROJECTS)
            .join(VERSION)
            .join(project_id.as_str())
    }
}

impl ProjectPermissionWriter for ProjectStore {
    fn append_allow(
        &self,
        project_id: ProjectId,
        additions: ProjectAllowRules,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = std::result::Result<ProjectPolicySnapshot, ProjectPolicyStoreError>,
                > + Send
                + '_,
        >,
    > {
        Box::pin(self.append(project_id, additions))
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoreFile {
    version: u32,
    project_id: String,
    revision: u64,
    allow: Vec<String>,
}

fn read_policy(dir: &PrivateDir, project_id: &ProjectId) -> Result<ProjectPolicySnapshot> {
    let Some(raw) = dir.read_string(OsStr::new(POLICY_FILE), POLICY_LABEL)? else {
        return Ok(ProjectPolicySnapshot::empty());
    };
    let file: StoreFile =
        serde_json::from_str(&raw).context("invalid project permission policy")?;
    if file.version != 1 {
        bail!("unsupported project permission policy version");
    }
    if file.project_id != project_id.as_str() {
        bail!("project permission policy identity mismatch");
    }
    let allow = ProjectAllowRules::parse(&file.allow)
        .map_err(|_| anyhow::anyhow!("invalid project permission policy rules"))?;
    Ok(ProjectPolicySnapshot {
        revision: file.revision,
        allow,
    })
}

pub(crate) fn project_store_root(global_config_path: &Path) -> Result<PathBuf> {
    global_config_path
        .parent()
        .map(Path::to_path_buf)
        .context("global config path has no private state directory")
}

#[cfg(test)]
mod tests {
    use std::str::FromStr as _;
    use std::sync::atomic::AtomicU64;
    use std::sync::atomic::Ordering;

    use kloop_core::project::WorkspaceIdentity;
    use kloop_core::session_store::SessionStore;

    use super::*;

    static TEST_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    fn fixture(tag: &str) -> (PathBuf, ProjectStore, ProjectId) {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "kloop-project-store-{tag}-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let root = base.join(".kloop");
        let store = ProjectStore::new(root);
        let id = ProjectId::from_str(&format!("p1_{}", "a".repeat(64))).unwrap();
        (base, store, id)
    }

    #[tokio::test]
    async fn missing_store_is_empty_without_creating_state() {
        let (base, store, id) = fixture("missing");
        assert_eq!(
            store.load(&id).await.unwrap(),
            ProjectPolicySnapshot::empty()
        );
        assert!(!base.join(".kloop").exists());
        let _ = std::fs::remove_dir_all(base);
    }

    /// The decision is the directory: absent before anything ran here, present
    /// once the grant lands, and equally present for a project that only ever
    /// held transcripts or durable approvals. The grant itself rides in the
    /// partition's label — one file, no lock beside it, and the rules in
    /// `permissions.json` untouched.
    #[test]
    fn the_project_directory_is_the_answer_and_the_label_carries_the_grant() {
        let (base, store, id) = fixture("trust");
        assert!(!store.trusted_blocking(&id), "nothing has run here");
        assert!(
            !base.join(".kloop").exists(),
            "asking must not create state"
        );

        store
            .grant_trust_blocking(&id, Path::new("/work/here"))
            .unwrap();
        assert!(store.trusted_blocking(&id));
        assert!(!store.policy_path(&id).exists(), "rules are untouched");
        let mut written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.label_path(&id)).unwrap()).unwrap();
        let granted_at = written["granted_at"].take();
        assert_eq!(
            written,
            serde_json::json!({
                "version": 1,
                "project_id": id.as_str(),
                "anchor": "/work/here",
                "granted_at": null,
            })
        );
        let granted_at = granted_at.as_str().expect("granted_at is a string");
        assert_eq!(granted_at.len(), 20, "{granted_at}");
        assert!(granted_at.ends_with('Z'), "{granted_at}");

        // The grant is one write, not a read-modify-write, so the partition
        // holds that one file and no lock.
        let names: Vec<String> = std::fs::read_dir(store.project_dir(&id))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [PROJECT_LABEL]);

        // Answering the same question again is the same answer.
        store
            .grant_trust_blocking(&id, Path::new("/work/here"))
            .unwrap();
        assert!(store.trusted_blocking(&id));
        let again: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(store.label_path(&id)).unwrap()).unwrap();
        assert_eq!(again["project_id"], id.as_str());
        assert!(again["granted_at"].is_string());

        // A project that predates the question — sessions but no record — is
        // answered for all the same, and one project says nothing about another.
        let (other_base, other_store, other_id) = fixture("trust-sessions");
        let untouched = ProjectId::from_str(&format!("p1_{}", "b".repeat(64))).unwrap();
        std::fs::create_dir_all(other_store.project_dir(&other_id).join("sessions")).unwrap();
        assert!(other_store.trusted_blocking(&other_id));
        assert!(
            !other_store.trusted_blocking(&untouched),
            "a different project"
        );
        let _ = std::fs::remove_dir_all(other_base);
        let _ = std::fs::remove_dir_all(base);
    }

    /// The wiring, not just the shape: the anchor the CLI hands the store is the
    /// one a cross-project listing reads back out of the label, and the
    /// `ensure()` that follows the grant in the same launch leaves the grant in
    /// place. A real repository is what makes this sharp — there the anchor is
    /// the Git common directory, not the cwd, so passing the wrong one of the
    /// two shows up as a listing that prints the wrong path.
    #[test]
    fn the_grant_writes_the_anchor_a_cross_project_listing_reads_back() {
        let sequence = TEST_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let base = std::env::temp_dir().join(format!(
            "kloop-project-store-wiring-{}-{sequence}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let repo = base.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["init", "-q"])
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap();
        assert!(status.success(), "git init failed");

        let identity = WorkspaceIdentity::resolve(&repo);
        let project_id = identity.project_id().expect("a repository has an identity");
        let root = base.join(".kloop");
        let sessions = SessionStore::global(root.clone());
        let store = ProjectStore::new(root);

        // The interleaving a concurrent `--headless` or `--serve` run in the
        // same repository produces: the session store creates the partition and
        // its label first, and the grant then writes into a file it did not
        // create. The CLI's private-store reader rejects a label any group or
        // other can reach, so a label written world-readable would make this
        // fail rather than merely look untidy.
        sessions.ensure(&repo).unwrap();
        store
            .grant_trust_blocking(project_id, identity.partition_anchor())
            .unwrap();

        // And the reverse order, which is every ordinary interactive launch: the
        // `ensure()` behind the grant must leave the grant in place.
        sessions.ensure(&repo).unwrap();

        let bucket = sessions
            .buckets()
            .into_iter()
            .next()
            .expect("one partition");
        assert_eq!(bucket.anchor.as_deref(), Some(identity.partition_anchor()));
        let label: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(bucket.dirs.sessions.parent().unwrap().join(PROJECT_LABEL))
                .unwrap(),
        )
        .unwrap();
        assert!(
            label["granted_at"].is_string(),
            "the launch after the grant erased it: {label}"
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn append_round_trips_deduplicates_and_increments_revision() {
        let (base, store, id) = fixture("round-trip");
        let first = store
            .append(
                id.clone(),
                ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        let duplicate = store
            .append(
                id.clone(),
                ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(duplicate, first);
        let second = store
            .append(
                id.clone(),
                ProjectAllowRules::parse(&["write_file(src/**)".into()]).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(second.revision, 2);
        assert_eq!(
            second.allow.raw(),
            &[
                "bash(cargo test *)".to_string(),
                "write_file(src/**)".to_string()
            ]
        );
        assert_eq!(store.load(&id).await.unwrap(), second);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn revision_overflow_fails_without_replacing_policy() {
        let (base, store, id) = fixture("overflow");
        store
            .append(
                id.clone(),
                ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
            )
            .await
            .unwrap();
        let path = store.policy_path(&id);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["revision"] = serde_json::json!(u64::MAX);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let before = store.load(&id).await.unwrap();

        assert_eq!(
            store
                .append(
                    id.clone(),
                    ProjectAllowRules::parse(&["write_file(src/**)".into()]).unwrap(),
                )
                .await,
            Err(ProjectPolicyStoreError::PersistenceFailed)
        );
        assert_eq!(store.load(&id).await.unwrap(), before);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn strict_schema_identity_and_rule_validation_fail_closed() {
        let (base, store, id) = fixture("strict");
        store
            .append(
                id.clone(),
                ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
            )
            .await
            .unwrap();
        let path = store.policy_path(&id);
        let mut value: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        value["unknown"] = serde_json::json!(true);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert_eq!(store.load(&id).await, Err(ProjectPolicyStoreError::Invalid));

        value.as_object_mut().unwrap().remove("unknown");
        value["project_id"] = serde_json::json!(format!("p1_{}", "b".repeat(64)));
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        assert_eq!(store.load(&id).await, Err(ProjectPolicyStoreError::Invalid));

        value["project_id"] = serde_json::json!(id.as_str());
        value["allow"] = serde_json::json!(["sentinel-secret("]);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let error = store.load_blocking(&id).unwrap_err().to_string();
        assert!(error.contains("invalid project permission policy rules"));
        assert!(!error.contains("sentinel-secret"));
        assert_eq!(store.load(&id).await, Err(ProjectPolicyStoreError::Invalid));
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn concurrent_writers_publish_the_union_without_lost_updates() {
        let (base, store, id) = fixture("concurrent");
        let first_store = store.clone();
        let first_id = id.clone();
        let first = tokio::spawn(async move {
            first_store
                .append(
                    first_id,
                    ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
                )
                .await
        });
        let second_store = store.clone();
        let second_id = id.clone();
        let second = tokio::spawn(async move {
            second_store
                .append(
                    second_id,
                    ProjectAllowRules::parse(&["write_file(src/**)".into()]).unwrap(),
                )
                .await
        });
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        let snapshot = store.load(&id).await.unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.allow.raw().len(), 2);
        assert!(snapshot.allow.raw().contains(&"bash(cargo test *)".into()));
        assert!(snapshot.allow.raw().contains(&"write_file(src/**)".into()));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn multiprocess_append_child() {
        let Ok(root) = std::env::var("KLOOP_PROJECT_STORE_CHILD_ROOT") else {
            return;
        };
        let id = ProjectId::from_str(
            &std::env::var("KLOOP_PROJECT_STORE_CHILD_ID").expect("child project id"),
        )
        .unwrap();
        let rule = std::env::var("KLOOP_PROJECT_STORE_CHILD_RULE").expect("child rule");
        ProjectStore::new(PathBuf::from(root))
            .append_blocking(&id, ProjectAllowRules::parse(&[rule]).unwrap())
            .unwrap();
    }

    #[test]
    fn multiprocess_writers_rmw_without_lost_updates() {
        let (base, store, id) = fixture("multiprocess");
        let root = base.join(".kloop");
        let child = |rule: &str| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "project_store::tests::multiprocess_append_child",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("KLOOP_PROJECT_STORE_CHILD_ROOT", &root)
                .env("KLOOP_PROJECT_STORE_CHILD_ID", id.as_str())
                .env("KLOOP_PROJECT_STORE_CHILD_RULE", rule)
                .spawn()
                .unwrap()
        };
        let first = child("bash(cargo test *)");
        let second = child("write_file(src/**)");
        assert!(first.wait_with_output().unwrap().status.success());
        assert!(second.wait_with_output().unwrap().status.success());
        let snapshot = store.load_blocking(&id).unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.allow.raw().len(), 2);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn project_ids_have_separate_files_and_locks() {
        let (base, store, first_id) = fixture("isolated");
        let second_id = ProjectId::from_str(&format!("p1_{}", "b".repeat(64))).unwrap();
        store
            .append(
                first_id.clone(),
                ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
            )
            .await
            .unwrap();
        store
            .append(
                second_id.clone(),
                ProjectAllowRules::parse(&["write_file(src/**)".into()]).unwrap(),
            )
            .await
            .unwrap();
        assert_ne!(store.policy_path(&first_id), store.policy_path(&second_id));
        assert_ne!(
            store.load(&first_id).await.unwrap().allow,
            store.load(&second_id).await.unwrap().allow
        );
        let _ = std::fs::remove_dir_all(base);
    }
}
