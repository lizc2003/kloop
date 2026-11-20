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
    pub(crate) fn grant_trust_blocking(&self, project_id: &ProjectId, anchor: &Path) -> Result<()> {
        let dir = self.ensure_project(project_id)?;
        let _lock = dir.open_lock(
            OsStr::new(kloop_core::session_store::PROJECT_LABEL_LOCK),
            LABEL_LABEL,
        )?;
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

    pub(crate) fn policy_path(&self, project_id: &ProjectId) -> PathBuf {
        self.project_dir(project_id).join(POLICY_FILE)
    }

    /// 权限文件读不了时给用户的修法，每条都能照做。规则非法时加载器已经安全地读过文件，修法就是
    /// 它带回来的逐条说明；其余情况和正式加载走同一条按句柄逐级、不跟随链接、不阻塞的路径再看一遍
    /// 现场（[`crate::private_store::inspect_private_file`]），权限位过宽给 `chmod`，链接或类型不对
    /// 的条目给针对它的修法，走得到文件时连同里面的非法规则一起列出。其他平台只在加载器读到了
    /// 内容无效的文件时给通用修法。规则原文
    /// 不出现在修法里（见 [`kloop_core::permissions::invalid_rule_reason`]）。
    pub(crate) fn load_failure_fixes(
        &self,
        project_id: &ProjectId,
        error: &anyhow::Error,
    ) -> Vec<String> {
        if let Some(InvalidRules(fixes)) = error.downcast_ref::<InvalidRules>() {
            return fixes.clone();
        }
        let inspection = crate::private_store::inspect_private_file(
            &self.root,
            &self.components(project_id),
            OsStr::new(POLICY_FILE),
        );
        use crate::private_store::Finding;
        let findings = match inspection.findings.last() {
            Some(blocker) if !matches!(blocker, Finding::LooseMode { .. }) => {
                std::slice::from_ref(blocker)
            }
            _ => &inspection.findings,
        };
        let mut fixes: Vec<String> = findings
            .iter()
            .map(|finding| match finding {
                Finding::Link {
                    path,
                    directory: true,
                } => format!(
                    "replace the symlink {} with a real directory (kloop does not follow links in its private state)",
                    shell_path(path)
                ),
                Finding::Link {
                    path,
                    directory: false,
                } => format!(
                    "replace the symlink {} with a regular file",
                    shell_path(path)
                ),
                Finding::NotDirectory(path) => format!(
                    "{} must be a directory; move it out of the way",
                    shell_path(path)
                ),
                Finding::NotRegularFile(path) => format!(
                    "{} is not a regular file; move it out of the way",
                    shell_path(path)
                ),
                Finding::LooseMode {
                    path,
                    directory: true,
                } => format!("run: chmod 700 {}", shell_path(path)),
                Finding::LooseMode {
                    path,
                    directory: false,
                } => format!("run: chmod 600 {}", shell_path(path)),
                Finding::Inaccessible(path) => format!(
                    "cannot inspect {}; check its owner and permissions",
                    shell_path(path)
                ),
                Finding::Changed(path) => format!(
                    "{} changed during inspection; check the entry and retry",
                    shell_path(path)
                ),
            })
            .collect();
        if let Some(allow) = inspection
            .contents
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|value| {
                value
                    .get("allow")
                    .and_then(|allow| allow.as_array())
                    .cloned()
            })
        {
            fixes.extend(rule_fixes(allow.iter().map(serde_json::Value::as_str)));
        }
        if fixes.is_empty() && (cfg!(unix) || error.is::<InvalidPolicyFile>()) {
            fixes.push(
                "fix the file by hand, or delete it (that forgets this project's remembered approvals)"
                    .to_string(),
            );
        }
        fixes
    }

    pub(crate) fn load_failure_report(
        &self,
        project_id: &ProjectId,
        error: &anyhow::Error,
    ) -> String {
        let mut report = format!(
            "the project permission policy could not be loaded ({error:#})\n  file: {}",
            shell_path(&self.policy_path(project_id))
        );
        for fix in self.load_failure_fixes(project_id, error) {
            report.push_str("\n  ");
            report.push_str(&fix);
        }
        report
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

/// 给用户照抄进终端的路径：家目录缩成 `~/`；剩下的部分只要有白名单之外的字符就整体加单引号
/// （`~/` 留在引号外才会展开）。白名单之外包括空白、引号和全部 shell 元字符，也包括非 ASCII 字符。
/// POSIX 的写法只在 unix 上用；其它平台给原生路径（`chmod` 修法本来也只在 unix 出现）。
pub(crate) fn shell_path(path: &Path) -> String {
    if !cfg!(unix) {
        return path.display().to_string();
    }
    let (prefix, rest) = match std::env::home_dir()
        .and_then(|home| path.strip_prefix(&home).ok().map(Path::to_path_buf))
        .filter(|relative| !relative.as_os_str().is_empty())
    {
        Some(relative) => ("~/", relative.display().to_string()),
        None => ("", path.display().to_string()),
    };
    let plain = rest.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '/' | '.' | '_' | '-' | '+' | '@' | ':' | ',')
    });
    if plain {
        format!("{prefix}{rest}")
    } else {
        format!("{prefix}'{}'", rest.replace('\'', "'\\''"))
    }
}

/// 权限文件里有非法规则。显示时只说有非法规则（不复述规则原文）；逐条修法给加载失败的警告用。
#[derive(Debug)]
pub(crate) struct InvalidRules(pub(crate) Vec<String>);

impl std::fmt::Display for InvalidRules {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid project permission policy rules")
    }
}

impl std::error::Error for InvalidRules {}

/// `allow` 里非法条目的修法：同一个原因的条目合成一行（`entries 15, 17–19`），条目再多警告也短，
/// 不会被前端截掉。`None` 是不是字符串的条目。
fn rule_fixes<'a>(entries: impl IntoIterator<Item = Option<&'a str>>) -> Vec<String> {
    let mut grouped: Vec<(String, Vec<usize>)> = Vec::new();
    for (index, entry) in entries.into_iter().enumerate() {
        let reason = match entry {
            Some(text) => kloop_core::permissions::invalid_rule_reason(text),
            None => Some("not a string".to_string()),
        };
        let Some(reason) = reason else {
            continue;
        };
        match grouped.iter_mut().find(|(known, _)| *known == reason) {
            Some((_, numbers)) => numbers.push(index + 1),
            None => grouped.push((reason, vec![index + 1])),
        }
    }
    grouped
        .into_iter()
        .map(|(reason, numbers)| {
            let noun = if numbers.len() == 1 {
                "entry"
            } else {
                "entries"
            };
            format!(
                "remove {noun} {} of \"allow\": {reason}",
                number_ranges(&numbers)
            )
        })
        .collect()
}

/// `1, 2, 3, 5` → `1–3, 5`。
fn number_ranges(numbers: &[usize]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut index = 0;
    while index < numbers.len() {
        let start = numbers[index];
        let mut end = start;
        while index + 1 < numbers.len() && numbers[index + 1] == end + 1 {
            index += 1;
            end = numbers[index];
        }
        parts.push(if end > start + 1 {
            format!("{start}–{end}")
        } else if end == start + 1 {
            format!("{start}, {end}")
        } else {
            start.to_string()
        });
        index += 1;
    }
    parts.join(", ")
}

/// 只有加载器已打开普通文件并读到内容时才产生，和路径遍历错误区分开。
#[derive(Debug)]
struct InvalidPolicyFile(&'static str);

impl std::fmt::Display for InvalidPolicyFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for InvalidPolicyFile {}

fn read_policy(dir: &PrivateDir, project_id: &ProjectId) -> Result<ProjectPolicySnapshot> {
    let Some(raw) = dir.read_string(OsStr::new(POLICY_FILE), POLICY_LABEL)? else {
        return Ok(ProjectPolicySnapshot::empty());
    };
    let file: StoreFile = serde_json::from_str(&raw)
        .context(InvalidPolicyFile("invalid project permission policy"))?;
    if file.version != 1 {
        return Err(InvalidPolicyFile("unsupported project permission policy version").into());
    }
    if file.project_id != project_id.as_str() {
        return Err(InvalidPolicyFile("project permission policy identity mismatch").into());
    }
    let allow = ProjectAllowRules::parse(&file.allow).map_err(|_| {
        anyhow::Error::new(InvalidRules(rule_fixes(
            file.allow.iter().map(|entry| Some(entry.as_str())),
        )))
    })?;
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
        let id = ProjectId::from_str(&format!("p1_{}", "3".repeat(25))).unwrap();
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

        let mut names: Vec<String> = std::fs::read_dir(store.project_dir(&id))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [PROJECT_LABEL, kloop_core::session_store::PROJECT_LABEL_LOCK]
        );

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
        let untouched = ProjectId::from_str(&format!("p1_{}", "4".repeat(25))).unwrap();
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
        let status = kloop_core::git::hermetic_command(&repo)
            .args(["init", "-q"])
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
        value["project_id"] = serde_json::json!(format!("p1_{}", "4".repeat(25)));
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

    /// 读不了权限文件时的修法要能照做：权限位过宽给出 `chmod`，非法规则指出是第几条、为什么，
    /// 同一个原因的条目合成一行，且不复述规则原文（规则可能带着命令参数里的密钥）。权限位过宽时
    /// 加载器读不到规则，看现场那一遍连同里面的非法规则一起列出。
    #[cfg(unix)]
    #[tokio::test]
    async fn load_failures_explain_how_to_fix_the_file() {
        use std::os::unix::fs::PermissionsExt as _;

        let (base, store, id) = fixture("fixes");
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
        value["allow"] = serde_json::json!([
            "bash(cargo test *)",
            "sandbox_escalate(cd /work/repo *)",
            "sandbox_escalate(cd .. *)",
            "sandbox_escalate(cd /work/repo/rust *)",
            "sandbox_write(/opt/out/)",
            "sentinel-secret(",
            "sandbox_escalate(cd /tmp *)",
        ]);
        std::fs::write(&path, serde_json::to_vec_pretty(&value).unwrap()).unwrap();
        let rule_fixes = vec![
            "remove entries 2–4, 7 of \"allow\": `sandbox_escalate(cd …)` never matches (it starts with the read-only command `cd`)".to_string(),
            "remove entry 5 of \"allow\": `sandbox_write(…)` needs a plain absolute directory (no glob, `.`, `..`, `//` or trailing `/`)".to_string(),
            "remove entry 6 of \"allow\": not a valid rule".to_string(),
        ];

        // 权限位没问题：非法规则的修法由加载器带回，诊断不必再读文件。
        let error = store.load_blocking(&id).unwrap_err();
        assert_eq!(
            format!("{error:#}"),
            "invalid project permission policy rules"
        );
        assert_eq!(store.load_failure_fixes(&id, &error), rule_fixes);

        // 权限位也过宽：加载器停在权限位上，看现场时一并列出 chmod 和非法规则。
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let error = store.load_blocking(&id).unwrap_err();
        let shown = format!("{error:#}");
        assert!(shown.contains("mode 0644"), "{shown}");
        assert!(shown.contains("must be mode 0600"), "{shown}");
        let mut expected = vec![format!("run: chmod 600 {}", shell_path(&path))];
        expected.extend(rule_fixes);
        assert_eq!(store.load_failure_fixes(&id, &error), expected);
        let _ = std::fs::remove_dir_all(base);
    }

    /// 看现场时和加载一样不跟随、不阻塞：权限文件是 FIFO 时不能挂住启动，是目录时不能建议
    /// `chmod 600` 去掉它的搜索位；上级目录是符号链接时只指出那一级，不越过链接去看、去读。
    #[cfg(unix)]
    #[tokio::test]
    async fn load_failure_fixes_stop_at_odd_entries_without_following_them() {
        let (base, store, id) = fixture("odd");
        store
            .append(
                id.clone(),
                ProjectAllowRules::parse(&["bash(cargo test *)".into()]).unwrap(),
            )
            .await
            .unwrap();
        let path = store.policy_path(&id);
        let diagnose = |store: &ProjectStore| {
            let error = store.load_blocking(&id).unwrap_err();
            store.load_failure_fixes(&id, &error)
        };

        std::fs::remove_file(&path).unwrap();
        let status = std::process::Command::new("mkfifo")
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        let (sender, receiver) = std::sync::mpsc::channel();
        let probe = store.clone();
        let probe_id = id.clone();
        std::thread::spawn(move || {
            let error = probe.load_blocking(&probe_id).unwrap_err();
            let _ = sender.send(probe.load_failure_fixes(&probe_id, &error));
        });
        let fixes = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("a FIFO must not block the load or the diagnosis");
        assert_eq!(
            fixes,
            vec![format!(
                "{} is not a regular file; move it out of the way",
                shell_path(&path)
            )]
        );

        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert_eq!(
            diagnose(&store),
            vec![format!(
                "{} is not a regular file; move it out of the way",
                shell_path(&path)
            )]
        );

        // 上级目录换成符号链接：只指出那一级；链接另一头的权限位过宽也不去看。
        let project = path.parent().unwrap().to_path_buf();
        let real = base.join("elsewhere");
        std::fs::rename(&project, &real).unwrap();
        std::os::unix::fs::symlink(&real, &project).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            let projects = project.parent().unwrap().parent().unwrap();
            std::fs::set_permissions(projects, std::fs::Permissions::from_mode(0o755)).unwrap();
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            diagnose(&store),
            vec![format!(
                "replace the symlink {} with a real directory (kloop does not follow links in its private state)",
                shell_path(&project)
            )]
        );
        let _ = std::fs::remove_dir_all(base);
    }

    #[cfg(windows)]
    #[test]
    fn failed_policy_directory_names_the_entry_without_a_delete_suggestion() {
        let (base, store, id) = fixture("wrong-directory");
        std::fs::create_dir_all(&*store.root).unwrap();
        let entry = store.root.join(PROJECTS);
        std::fs::write(&entry, "").unwrap();
        let error = store.load_blocking(&id).unwrap_err();
        assert!(format!("{error:#}").contains(&entry.display().to_string()));
        assert!(store.load_failure_fixes(&id, &error).is_empty());
        let _ = std::fs::remove_dir_all(base);
    }

    /// `1, 2, 3, 5` 合成 `1–3, 5`；两个相邻的不写成区间。
    #[test]
    fn invalid_entries_are_numbered_as_ranges() {
        assert_eq!(number_ranges(&[1, 2, 3, 5]), "1–3, 5");
        assert_eq!(number_ranges(&[4, 5, 9]), "4, 5, 9");
        assert_eq!(number_ranges(&[7]), "7");
    }

    /// 修法里的路径要能原样贴进 shell：白名单之外的字符一律整体加单引号，家目录缩成 `~/`。
    #[cfg(unix)]
    #[test]
    fn shell_paths_quote_anything_outside_a_safe_set() {
        assert_eq!(
            shell_path(Path::new("/tmp/a-b_c.d/x+y@z:1,2")),
            "/tmp/a-b_c.d/x+y@z:1,2"
        );
        for (raw, quoted) in [
            ("/tmp/work(1)/p", "'/tmp/work(1)/p'"),
            ("/tmp/a&b", "'/tmp/a&b'"),
            ("/tmp/dir[1]", "'/tmp/dir[1]'"),
            ("/tmp/a*b?c", "'/tmp/a*b?c'"),
            ("/tmp/#x ~y !z", "'/tmp/#x ~y !z'"),
            ("/tmp/项目", "'/tmp/项目'"),
            ("/tmp/it's", "'/tmp/it'\\''s'"),
        ] {
            assert_eq!(shell_path(Path::new(raw)), quoted, "{raw}");
        }
        if let Some(home) = std::env::home_dir() {
            assert_eq!(shell_path(&home.join(".kloop/x")), "~/.kloop/x");
            assert_eq!(shell_path(&home.join("a b")), "~/'a b'");
        }
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
                // Captured, not inherited: the child's own test report would
                // land in the middle of the parent's.
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap()
        };
        let first = child("bash(cargo test *)");
        let second = child("write_file(src/**)");
        for child in [first, second] {
            let output = child.wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "child: {}{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let snapshot = store.load_blocking(&id).unwrap();
        assert_eq!(snapshot.revision, 2);
        assert_eq!(snapshot.allow.raw().len(), 2);
        let _ = std::fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn project_ids_have_separate_files_and_locks() {
        let (base, store, first_id) = fixture("isolated");
        let second_id = ProjectId::from_str(&format!("p1_{}", "4".repeat(25))).unwrap();
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
