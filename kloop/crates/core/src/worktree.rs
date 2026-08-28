//! Owned git-worktree resources shared by session switching and task isolation.
//!
//! A handle records repository provenance, custody, and owner. Deletion always
//! revalidates that provenance under the process-wide Git mutation lock. Session
//! and task handles use the same Git operations but never share lifecycle state.

#[cfg(windows)]
use std::ffi::OsString;
#[cfg(windows)]
use std::os::windows::ffi::OsStrExt as _;
#[cfg(windows)]
use std::os::windows::ffi::OsStringExt as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LockResult;
#[cfg(test)]
use std::sync::Mutex as StdMutex;
#[cfg(test)]
use std::sync::OnceLock;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::permissions::Permissions;
use crate::sandbox::SandboxPolicy;

/// Managed worktrees live in kloop's own repository namespace, not in another
/// agent's directory. Deliberately a sibling of `.kloop/` rather than
/// `.kloop/worktrees`: `.kloop` is a sensitive path component (kloop's own
/// state — a write there is privilege escalation, not editing) and a read-only
/// subpath of every sandbox writable root, so a checkout nested inside it would
/// classify every edit the agent makes in its own worktree as an escalation.
pub(crate) const WORKTREES_DIR: &str = ".kloop-worktrees";
const WORKTREE_BRANCH_PREFIX: &str = "worktree-";
static WORKTREE_MUTATION_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
static GENERATED_NAME_SEQUENCE: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
#[derive(Clone)]
struct TestGitEnvironment {
    root: PathBuf,
    home: PathBuf,
    xdg_config_home: PathBuf,
}

#[cfg(test)]
static TEST_GIT_ENVIRONMENTS: OnceLock<StdMutex<Vec<TestGitEnvironment>>> = OnceLock::new();

#[cfg(test)]
pub(crate) struct TestGitEnvironmentGuard {
    root: PathBuf,
}

#[cfg(test)]
impl Drop for TestGitEnvironmentGuard {
    fn drop(&mut self) {
        let environments = TEST_GIT_ENVIRONMENTS.get_or_init(|| StdMutex::new(Vec::new()));
        environments
            .lock()
            .unwrap()
            .retain(|environment| environment.root != self.root);
    }
}

#[cfg(test)]
pub(crate) fn isolate_test_git_environment(
    root: &Path,
    home: &Path,
    xdg_config_home: &Path,
) -> TestGitEnvironmentGuard {
    let environments = TEST_GIT_ENVIRONMENTS.get_or_init(|| StdMutex::new(Vec::new()));
    let mut environments = environments.lock().unwrap();
    assert!(
        environments
            .iter()
            .all(|environment| environment.root != root),
        "duplicate test Git environment for {}",
        root.display()
    );
    environments.push(TestGitEnvironment {
        root: root.to_path_buf(),
        home: home.to_path_buf(),
        xdg_config_home: xdg_config_home.to_path_buf(),
    });
    TestGitEnvironmentGuard {
        root: root.to_path_buf(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorktreeOwner {
    Session(String),
    Task(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeCustody {
    Managed,
    External,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WorktreeLifecycle {
    Active,
    Kept,
    Removed,
    Orphaned,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExitAction {
    Keep,
    Remove,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WorktreeChanges {
    pub changed_files: usize,
    pub commits: usize,
}

#[derive(Debug)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    pub custody: WorktreeCustody,
    pub owner: WorktreeOwner,
    pub lifecycle: WorktreeLifecycle,
    base: String,
    repository_root: PathBuf,
    common_dir: PathBuf,
    canonical_path: PathBuf,
}

pub struct ActiveWorktree {
    wt: Worktree,
    pub cwd: PathBuf,
    pub branch: String,
    pub permissions: Arc<Permissions>,
    pub file_state: Arc<crate::file_state::FileState>,
    pub sandbox: Option<Arc<SandboxPolicy>>,
    pub system: String,
}

/// One session's active handle plus an async operation lock. The ordinary read
/// and write methods preserve the old Config accessor seam; lifecycle operations
/// take `operation` before inspecting or replacing the slot.
pub struct ActiveWorktreeState {
    current: RwLock<Option<ActiveWorktree>>,
    operation: AsyncMutex<()>,
    transition_epoch: AtomicU64,
}

impl Default for ActiveWorktreeState {
    fn default() -> Self {
        Self {
            current: RwLock::new(None),
            operation: AsyncMutex::new(()),
            transition_epoch: AtomicU64::new(0),
        }
    }
}

impl ActiveWorktreeState {
    pub fn read(&self) -> LockResult<RwLockReadGuard<'_, Option<ActiveWorktree>>> {
        self.current.read()
    }

    pub fn write(&self) -> LockResult<RwLockWriteGuard<'_, Option<ActiveWorktree>>> {
        self.current.write()
    }

    pub(crate) fn transition_epoch(&self) -> u64 {
        self.transition_epoch.load(Ordering::Acquire)
    }

    fn advance_transition_epoch(&self) {
        self.transition_epoch
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .expect("worktree transition epoch exhausted");
    }

    fn take_for_transition(&self) -> Option<ActiveWorktree> {
        let mut current = self.current.write().unwrap();
        let active = current.take()?;
        self.advance_transition_epoch();
        drop(current);
        Some(active)
    }

    fn restore_after_transition(&self, active: ActiveWorktree) {
        let mut current = self.current.write().unwrap();
        debug_assert!(current.is_none());
        *current = Some(active);
        self.advance_transition_epoch();
        drop(current);
    }
}

#[derive(Debug)]
struct Repository {
    root: PathBuf,
    common_dir: PathBuf,
}

#[derive(Debug)]
struct RegisteredWorktree {
    path: PathBuf,
    head: String,
    branch: Option<String>,
}

#[derive(Clone, Copy)]
enum BasePolicy {
    Head,
    Fresh,
}

/// Create an owned worktree for a task. Task isolation deliberately preserves
/// its established HEAD base semantics; session creation uses the fresh policy.
pub async fn create(cwd: &Path, name: &str) -> Result<Worktree> {
    create_managed(
        cwd,
        name,
        WorktreeOwner::Task(name.to_string()),
        BasePolicy::Head,
    )
    .await
}

async fn create_managed(
    cwd: &Path,
    name: &str,
    owner: WorktreeOwner,
    base_policy: BasePolicy,
) -> Result<Worktree> {
    validate_name(name)?;
    let repository = discover_repository(cwd)
        .await
        .context("worktree isolation requires a git repository")?;
    let encoded_name = encode_name(name);
    let path = repository.root.join(WORKTREES_DIR).join(&encoded_name);
    let branch = format!("{WORKTREE_BRANCH_PREFIX}{encoded_name}");

    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    if path.symlink_metadata().is_ok() {
        bail!("worktree {} already exists", path.display());
    }
    if git_success(
        &repository.root,
        &[
            "show-ref",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .await?
    {
        bail!("worktree branch {branch} already exists");
    }
    let base = match base_policy {
        BasePolicy::Head => git_text(cwd, &["rev-parse", "HEAD"])
            .await
            .context("cannot read HEAD (repository has no commits yet?)")?,
        BasePolicy::Fresh => fresh_base(&repository.root).await?,
    };
    let base = base.trim().to_string();
    std::fs::create_dir_all(repository.root.join(WORKTREES_DIR))
        .context("creating managed worktree directory")?;
    exclude_worktrees_dir(&repository.common_dir)?;

    let path_text = git_compatible_path(&path).to_string_lossy().to_string();
    git_text(
        &repository.root,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            &branch,
            &path_text,
            &base,
        ],
    )
    .await
    .with_context(|| format!("git worktree add for branch {branch}"))?;

    let created = async {
        let canonical_path = std::fs::canonicalize(&path)
            .with_context(|| format!("canonicalizing created worktree {}", path.display()))?;
        let worktree = Worktree {
            path: canonical_path.clone(),
            branch: branch.clone(),
            custody: WorktreeCustody::Managed,
            owner,
            lifecycle: WorktreeLifecycle::Active,
            base,
            repository_root: repository.root.clone(),
            common_dir: repository.common_dir.clone(),
            canonical_path,
        };
        verify_provenance(&worktree).await?;
        Ok::<_, anyhow::Error>(worktree)
    }
    .await;
    if created.is_err() {
        let _ = git_text(
            &repository.root,
            &["worktree", "remove", "--force", &path_text],
        )
        .await;
        let _ = git_text(&repository.root, &["branch", "-D", &branch]).await;
    }
    created
}

async fn fresh_base(repository_root: &Path) -> Result<String> {
    if let Ok(reference) = git_text(
        repository_root,
        &["symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"],
    )
    .await
    {
        return git_text(repository_root, &["rev-parse", reference.trim()]).await;
    }
    if let Ok(branch) = git_text(
        repository_root,
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .await
    {
        let remote = format!("origin/{}", branch.trim());
        if git_success(
            repository_root,
            &[
                "show-ref",
                "--verify",
                "--quiet",
                &format!("refs/remotes/{remote}"),
            ],
        )
        .await?
        {
            return git_text(repository_root, &["rev-parse", &remote]).await;
        }
    }
    git_text(repository_root, &["rev-parse", "HEAD"])
        .await
        .context("cannot read HEAD (repository has no commits yet?)")
}

/// Task teardown: clean resources are removed, while any content or commit is
/// retained. Probe/remove failures propagate and leave the resource in place.
pub async fn finish(mut worktree: Worktree) -> Result<Option<String>> {
    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    verify_provenance(&worktree).await?;
    let changes = inspect_changes(&worktree).await?;
    if changes.changed_files > 0 || changes.commits > 0 {
        worktree.lifecycle = WorktreeLifecycle::Kept;
        return Ok(Some(retained_note(&worktree)));
    }
    remove_managed_locked(&mut worktree, /* force */ false).await?;
    Ok(None)
}

pub(crate) fn compute_overrides(
    base_cwd: &Path,
    base_permissions: &Arc<Permissions>,
    base_sandbox: &Option<Arc<SandboxPolicy>>,
    base_system: &str,
    worktree_path: &Path,
) -> Result<(Arc<Permissions>, Option<Arc<SandboxPolicy>>, String)> {
    let expected_project = base_permissions.identity().project_id();
    let parent_identity = crate::project::WorkspaceIdentity::resolve(base_cwd);
    let worktree_identity = crate::project::WorkspaceIdentity::resolve(worktree_path);
    if expected_project.is_none()
        || parent_identity.project_id() != expected_project
        || worktree_identity.project_id() != expected_project
    {
        bail!(
            "worktree project identity is unavailable or does not match the permission policy's project"
        );
    }
    let old = format!("- Working directory: {}", base_cwd.display());
    let new = format!("- Working directory: {}", worktree_path.display());
    let system = base_system.replacen(&old, &new, 1);
    let permissions = Arc::new(base_permissions.for_workspace(worktree_identity));
    let sandbox = base_sandbox
        .as_ref()
        .map(|sandbox| Arc::new(sandbox.for_workspace(worktree_path)));
    Ok((permissions, sandbox, system))
}

pub fn generated_name() -> String {
    const ADJECTIVES: &[&str] = &["bright", "calm", "gentle", "quiet", "swift", "wild"];
    const VERBS: &[&str] = &[
        "drifting", "flowing", "growing", "humming", "moving", "rising",
    ];
    const NOUNS: &[&str] = &["brook", "forest", "meadow", "mist", "river", "stone"];
    let sequence = GENERATED_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos() as u64);
    let value = sequence ^ nanos.rotate_left(17) ^ u64::from(std::process::id());
    let adjective = ADJECTIVES[(value as usize) % ADJECTIVES.len()];
    let verb = VERBS[((value >> 8) as usize) % VERBS.len()];
    let noun = NOUNS[((value >> 16) as usize) % NOUNS.len()];
    format!("{adjective}-{verb}-{noun}")
}

pub fn validate_name(name: &str) -> Result<()> {
    if name.len() > 64 {
        bail!(
            "Invalid worktree name: must be 64 characters or fewer (got {})",
            name.len()
        );
    }
    let segments: Vec<_> = name.split('/').collect();
    if segments
        .iter()
        .any(|segment| matches!(*segment, "." | ".."))
    {
        bail!("Invalid worktree name \"{name}\": must not contain \".\" or \"..\" path segments");
    }
    if segments.iter().any(|segment| {
        segment.is_empty()
            || !segment
                .chars()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
    }) {
        bail!(
            "Invalid worktree name \"{name}\": each \"/\"-separated segment must be non-empty and contain only letters, digits, dots, underscores, and dashes"
        );
    }
    Ok(())
}

fn encode_name(name: &str) -> String {
    name.replace('/', "+")
}

pub async fn enter(cfg: &Config, name: &str) -> Result<String> {
    let _operation = cfg.active_worktree.operation.lock().await;
    if cfg.active_worktree.read().unwrap().is_some() {
        bail!(
            "Already in a worktree session. Pass `path` to switch into another existing worktree, or use exit_worktree to leave this one before creating a new worktree."
        );
    }
    let owner = WorktreeOwner::Session(cfg.session_id.clone());
    let worktree = create_managed(&cfg.cwd, name, owner, BasePolicy::Fresh).await?;
    let message = format!(
        "Created worktree at {path} on branch {branch}. The session is now working in the worktree. Use exit_worktree to leave mid-session.",
        path = worktree.path.display(),
        branch = worktree.branch,
    );
    let overrides = match active_overrides(cfg, &worktree) {
        Ok(overrides) => overrides,
        Err(error) => {
            return match finish(worktree).await {
                Ok(_) => Err(error),
                Err(cleanup) => Err(error.context(format!(
                    "cleaning up worktree after identity validation failed: {cleanup:#}"
                ))),
            };
        }
    };
    install_active(cfg, worktree, overrides);
    Ok(message)
}

pub async fn enter_existing(cfg: &Config, path: &Path) -> Result<String> {
    let _operation = cfg.active_worktree.operation.lock().await;
    let repository = discover_repository(&cfg.cwd)
        .await
        .context("cannot enter worktree outside a git repository")?;
    let canonical_path = std::fs::canonicalize(path)
        .with_context(|| format!("Cannot enter worktree: {}", path.display()))?;
    let registered = registered_worktrees(&repository.root)
        .await?
        .into_iter()
        .find(|entry| entry.path == canonical_path)
        .ok_or_else(|| {
            anyhow!(
                "Cannot enter worktree: {} is not a registered worktree of {}. Run 'git -C {} worktree list' to see registered worktrees.",
                canonical_path.display(),
                repository.root.display(),
                repository.root.display(),
            )
        })?;
    let candidate_common = canonical_common_dir(&canonical_path).await?;
    if candidate_common != repository.common_dir {
        bail!(
            "Cannot enter worktree: {} belongs to a different repository",
            canonical_path.display()
        );
    }

    if cfg.active_worktree.read().unwrap().is_some() {
        let managed_root = repository.root.join(WORKTREES_DIR);
        if !canonical_path.starts_with(&managed_root) {
            bail!(
                "Cannot enter worktree: {} is not under {}. Switching from this session is limited to worktrees managed under .kloop-worktrees of this repository.",
                canonical_path.display(),
                managed_root.display(),
            );
        }
    }

    let branch = registered
        .branch
        .clone()
        .unwrap_or_else(|| "detached".to_string());
    let worktree = Worktree {
        path: canonical_path.clone(),
        branch: branch.clone(),
        custody: WorktreeCustody::External,
        owner: WorktreeOwner::Session(cfg.session_id.clone()),
        lifecycle: WorktreeLifecycle::Active,
        base: registered.head,
        repository_root: repository.root,
        common_dir: repository.common_dir,
        canonical_path,
    };
    let message = format!(
        "Entered worktree at {path} on branch {branch}. The session is now working in the worktree. Use exit_worktree to leave mid-session.",
        path = worktree.path.display(),
    );
    let overrides = active_overrides(cfg, &worktree)?;
    install_active(cfg, worktree, overrides);
    Ok(message)
}

type WorktreeOverrides = (Arc<Permissions>, Option<Arc<SandboxPolicy>>, String);

fn active_overrides(cfg: &Config, worktree: &Worktree) -> Result<WorktreeOverrides> {
    compute_overrides(
        &cfg.cwd,
        &cfg.permissions,
        &cfg.sandbox,
        &cfg.system,
        &worktree.path,
    )
}

fn install_active(cfg: &Config, worktree: Worktree, overrides: WorktreeOverrides) {
    let (permissions, sandbox, system) = overrides;
    let active = ActiveWorktree {
        cwd: worktree.path.clone(),
        branch: worktree.branch.clone(),
        permissions,
        file_state: Arc::new(crate::file_state::FileState::default()),
        sandbox,
        system,
        wt: worktree,
    };
    let mut current = cfg.active_worktree.write().unwrap();
    if let Some(mut previous) = current.replace(active) {
        previous.wt.lifecycle = WorktreeLifecycle::Kept;
    }
    cfg.active_worktree.advance_transition_epoch();
    drop(current);
}

pub async fn exit(cfg: &Config, action: ExitAction, discard_changes: bool) -> Result<String> {
    let _operation = cfg.active_worktree.operation.lock().await;
    let Some(mut active) = cfg.active_worktree.take_for_transition() else {
        bail!(
            "No-op: there is no active enter_worktree session to exit. This tool only operates on worktrees entered in the current session. No filesystem changes were made."
        );
    };
    let original_cwd = cfg.cwd.display().to_string();
    if action == ExitAction::Keep {
        active.wt.lifecycle = WorktreeLifecycle::Kept;
        return Ok(format!(
            "Exited worktree. Your work is preserved at {path} on branch {branch}. Session is now back in {original_cwd}.",
            path = active.wt.path.display(),
            branch = active.wt.branch,
        ));
    }
    if active.wt.custody != WorktreeCustody::Managed
        || !matches!(active.wt.owner, WorktreeOwner::Session(_))
    {
        let path = active.wt.path.display().to_string();
        cfg.active_worktree.restore_after_transition(active);
        bail!(
            "This session is not the owner of the worktree at {path}, so exit_worktree will not remove it. Use action: \"keep\" to return to {original_cwd}."
        );
    }

    let result = remove_for_session(&mut active.wt, discard_changes).await;
    match result {
        Ok(changes) => Ok(format_remove_message(&active.wt, &original_cwd, changes)),
        Err(error) => {
            if active.wt.path.exists() {
                cfg.active_worktree.restore_after_transition(active);
            }
            Err(error)
        }
    }
}

async fn remove_for_session(
    worktree: &mut Worktree,
    discard_changes: bool,
) -> Result<WorktreeChanges> {
    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    verify_provenance(worktree).await?;
    let changes = inspect_changes(worktree).await?;
    if !discard_changes && (changes.changed_files > 0 || changes.commits > 0) {
        let mut parts = Vec::new();
        if changes.changed_files > 0 {
            let suffix = if changes.changed_files == 1 {
                "file"
            } else {
                "files"
            };
            parts.push(format!("{} uncommitted {suffix}", changes.changed_files));
        }
        if changes.commits > 0 {
            let suffix = if changes.commits == 1 {
                "commit"
            } else {
                "commits"
            };
            parts.push(format!(
                "{} {suffix} on {}",
                changes.commits, worktree.branch
            ));
        }
        bail!(
            "Worktree has {}. Removing will discard this work permanently. Confirm with the user, then re-invoke with discard_changes: true — or use action: \"keep\" to preserve the worktree.",
            parts.join(" and ")
        );
    }
    remove_managed_locked(worktree, discard_changes).await?;
    Ok(changes)
}

fn format_remove_message(
    worktree: &Worktree,
    original_cwd: &str,
    changes: WorktreeChanges,
) -> String {
    let mut discarded = Vec::new();
    if changes.changed_files > 0 {
        let suffix = if changes.changed_files == 1 {
            "file"
        } else {
            "files"
        };
        discarded.push(format!("{} uncommitted {suffix}", changes.changed_files));
    }
    if changes.commits > 0 {
        let suffix = if changes.commits == 1 {
            "commit"
        } else {
            "commits"
        };
        discarded.push(format!("{} {suffix}", changes.commits));
    }
    let detail = if discarded.is_empty() {
        String::new()
    } else {
        format!(" Discarded {}.", discarded.join(" and "))
    };
    format!(
        "Exited and removed worktree at {}.{} Session is now back in {original_cwd}.",
        worktree.path.display(),
        detail,
    )
}

/// Session shutdown is deliberately conservative: without an explicit remove
/// action the active resource is retained, even when clean.
pub async fn finish_active(cfg: &Config) -> Option<String> {
    let _operation = cfg.active_worktree.operation.lock().await;
    let mut active = cfg.active_worktree.take_for_transition()?;
    active.wt.lifecycle = WorktreeLifecycle::Kept;
    Some(retained_note(&active.wt).trim().to_string())
}

fn retained_note(worktree: &Worktree) -> String {
    format!(
        "\n\n[Worktree retained at {path} (branch {branch}); it was NOT merged or removed.]",
        path = worktree.path.display(),
        branch = worktree.branch,
    )
}

async fn inspect_changes(worktree: &Worktree) -> Result<WorktreeChanges> {
    let status = git_text(
        &worktree.path,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )
    .await
    .context("probing worktree status")?;
    let mut fields = status.split('\0').filter(|field| !field.is_empty());
    let mut changed_files = 0;
    while let Some(field) = fields.next() {
        changed_files += 1;
        let status = field.get(..2).unwrap_or_default();
        if status.contains('R') || status.contains('C') {
            let _ = fields.next();
        }
    }
    let range = format!("{}..HEAD", worktree.base);
    let commits = git_text(&worktree.path, &["rev-list", "--count", &range])
        .await
        .context("probing worktree commits")?
        .trim()
        .parse::<usize>()
        .context("parsing worktree commit count")?;
    Ok(WorktreeChanges {
        changed_files,
        commits,
    })
}

async fn remove_managed_locked(worktree: &mut Worktree, force: bool) -> Result<()> {
    if worktree.custody != WorktreeCustody::Managed {
        bail!("refusing to remove an external worktree");
    }
    verify_provenance(worktree).await?;
    let path = worktree.path.to_string_lossy().to_string();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&path);
    if let Err(error) = git_text(&worktree.repository_root, &args).await {
        worktree.lifecycle = WorktreeLifecycle::Orphaned;
        return Err(error).context("removing managed worktree");
    }
    if let Err(error) = git_text(
        &worktree.repository_root,
        &["branch", "-D", &worktree.branch],
    )
    .await
    {
        worktree.lifecycle = WorktreeLifecycle::Orphaned;
        return Err(error).context("deleting managed worktree branch");
    }
    worktree.lifecycle = WorktreeLifecycle::Removed;
    Ok(())
}

async fn verify_provenance(worktree: &Worktree) -> Result<()> {
    let actual_path = std::fs::canonicalize(&worktree.path)
        .with_context(|| format!("worktree path {} is unavailable", worktree.path.display()))?;
    if actual_path != worktree.canonical_path {
        bail!("worktree path identity changed; refusing lifecycle mutation");
    }
    let common_dir = canonical_common_dir(&actual_path).await?;
    if common_dir != worktree.common_dir {
        bail!("worktree repository identity changed; refusing lifecycle mutation");
    }
    let registered = registered_worktrees(&worktree.repository_root)
        .await?
        .into_iter()
        .find(|entry| entry.path == actual_path)
        .ok_or_else(|| anyhow!("worktree is no longer registered; refusing lifecycle mutation"))?;
    if registered.branch.as_deref() != Some(worktree.branch.as_str()) {
        bail!("worktree branch identity changed; refusing lifecycle mutation");
    }
    Ok(())
}

async fn discover_repository(cwd: &Path) -> Result<Repository> {
    let common_dir = canonical_common_dir(cwd).await?;
    let current_root = git_text(cwd, &["rev-parse", "--show-toplevel"]).await?;
    let entries = registered_worktrees(Path::new(current_root.trim())).await?;
    let root = entries
        .first()
        .map(|entry| entry.path.clone())
        .ok_or_else(|| anyhow!("git worktree registry is empty"))?;
    let root_common = canonical_common_dir(&root).await?;
    if root_common != common_dir {
        bail!("git worktree registry repository identity mismatch");
    }
    Ok(Repository { root, common_dir })
}

async fn canonical_common_dir(cwd: &Path) -> Result<PathBuf> {
    let path = git_text(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    std::fs::canonicalize(path.trim()).context("canonicalizing git common directory")
}

async fn registered_worktrees(repository_root: &Path) -> Result<Vec<RegisteredWorktree>> {
    let output = git_text(repository_root, &["worktree", "list", "--porcelain"]).await?;
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut head = String::new();
    let mut branch = None;
    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(raw_path) = path.take() {
                let path = std::fs::canonicalize(&raw_path).with_context(|| {
                    format!("canonicalizing registered worktree {}", raw_path.display())
                })?;
                entries.push(RegisteredWorktree {
                    path,
                    head: std::mem::take(&mut head),
                    branch: branch.take(),
                });
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
        } else if let Some(value) = line.strip_prefix("HEAD ") {
            head = value.to_string();
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_string());
        }
    }
    Ok(entries)
}

fn exclude_worktrees_dir(common_dir: &Path) -> Result<()> {
    let exclude = common_dir.join("info").join("exclude");
    let Some(parent) = exclude.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).context("creating git info directory")?;
    let line = format!("{WORKTREES_DIR}/");
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|current| current.trim() == line) {
        return Ok(());
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(&line);
    next.push('\n');
    std::fs::write(&exclude, next).context("updating git info exclude")
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.arg("-C").arg(git_compatible_path(dir)).args(args);
    #[cfg(test)]
    {
        let environments = TEST_GIT_ENVIRONMENTS.get_or_init(|| StdMutex::new(Vec::new()));
        let environment = environments
            .lock()
            .unwrap()
            .iter()
            .filter(|environment| dir.starts_with(&environment.root))
            .max_by_key(|environment| environment.root.components().count())
            .cloned();
        if let Some(environment) = environment {
            let path = std::env::var_os("PATH");
            command.env_clear();
            if let Some(path) = path {
                command.env("PATH", path);
            }
            command
                .env("HOME", environment.home)
                .env("XDG_CONFIG_HOME", environment.xdg_config_home)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("LC_ALL", "C");
        }
    }
    command
}

#[cfg(windows)]
pub(crate) fn git_compatible_path(path: &Path) -> PathBuf {
    const VERBATIM: &[u16] = &[b'\\' as u16, b'\\' as u16, b'?' as u16, b'\\' as u16];
    const UNC: &[u16] = &[b'U' as u16, b'N' as u16, b'C' as u16, b'\\' as u16];

    let units: Vec<u16> = path.as_os_str().encode_wide().collect();
    if !units.starts_with(VERBATIM) {
        return path.to_path_buf();
    }
    let suffix = &units[VERBATIM.len()..];
    let compatible = if suffix.starts_with(UNC) {
        [b'\\' as u16, b'\\' as u16]
            .into_iter()
            .chain(suffix[UNC.len()..].iter().copied())
            .collect()
    } else {
        suffix.to_vec()
    };
    PathBuf::from(OsString::from_wide(&compatible))
}

#[cfg(not(windows))]
pub(crate) fn git_compatible_path(path: &Path) -> PathBuf {
    path.to_path_buf()
}

async fn git_success(dir: &Path, args: &[&str]) -> Result<bool> {
    let mut command = git_command(dir, args);
    let output = kloop_process_spawn::output(&mut command)
        .await
        .context("spawning git")?;
    Ok(output.status.success())
}

async fn git_text(dir: &Path, args: &[&str]) -> Result<String> {
    let mut command = git_command(dir, args);
    let output = kloop_process_spawn::output(&mut command)
        .await
        .context("spawning git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "kloop-wt-{tag}-{}-{}",
            std::process::id(),
            GENERATED_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            vec!["init", "-q", "-b", "main"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-qm", "base"],
        ] {
            git_text(&root, &args).await.unwrap();
        }
        root
    }

    fn config_for_repo(root: &Path, tag: &str) -> Config {
        let mut cfg = crate::tools::testutil::test_ctx(0, tag).cfg.test_clone();
        cfg.cwd = root.to_path_buf();
        cfg.system = format!("- Working directory: {}", root.display());
        cfg.permissions = Arc::new(
            crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &Default::default(),
                root.to_path_buf(),
                None,
            )
            .unwrap(),
        );
        cfg
    }

    #[tokio::test]
    async fn create_records_owned_provenance_and_removes_clean_tree() {
        let root = temp_repo("create").await;
        let worktree = create(&root, "agent-1").await.unwrap();
        let path = worktree.path.clone();
        assert_eq!(worktree.custody, WorktreeCustody::Managed);
        assert_eq!(worktree.owner, WorktreeOwner::Task("agent-1".into()));
        assert!(path.ends_with(".kloop-worktrees/agent-1"));
        assert_eq!(worktree.branch, "worktree-agent-1");
        assert!(finish(worktree).await.unwrap().is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn finish_keeps_dirty_task_tree() {
        let root = temp_repo("dirty").await;
        let worktree = create(&root, "agent-2").await.unwrap();
        std::fs::write(worktree.path.join("new.txt"), "work").unwrap();
        let path = worktree.path.clone();
        let note = finish(worktree).await.unwrap().unwrap();
        assert!(note.contains("worktree-agent-2"));
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ignored_files_are_content_changes() {
        let root = temp_repo("ignored").await;
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        git_text(&root, &["add", ".gitignore"]).await.unwrap();
        git_text(&root, &["commit", "-qm", "ignore"]).await.unwrap();
        let worktree = create(&root, "agent-ignored").await.unwrap();
        std::fs::write(worktree.path.join("ignored.txt"), "work").unwrap();
        let changes = inspect_changes(&worktree).await.unwrap();
        assert_eq!(changes.changed_files, 1);
        assert!(finish(worktree).await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_overrides_reject_project_drift_from_loaded_permissions() {
        let trusted = temp_repo("trusted-policy").await;
        let changed = temp_repo("changed-policy").await;
        let candidate = create(&changed, "candidate").await.unwrap();
        let permissions = Arc::new(
            crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &Default::default(),
                trusted.clone(),
                None,
            )
            .unwrap(),
        );
        let system = format!("- Working directory: {}", changed.display());
        let error = match compute_overrides(&changed, &permissions, &None, &system, &candidate.path)
        {
            Ok(_) => panic!("project drift reused the loaded permission policy"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("permission policy's project"));
        assert!(finish(candidate).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(trusted);
        let _ = std::fs::remove_dir_all(changed);
    }

    #[test]
    fn names_allow_segments_but_reject_escapes() {
        validate_name("feature/plan56").unwrap();
        assert_eq!(encode_name("feature/plan56"), "feature+plan56");
        for name in ["", ".", "..", "/absolute", "a//b", "a/../b", "a b"] {
            assert!(validate_name(name).is_err(), "accepted {name:?}");
        }
        assert!(validate_name(&"x".repeat(65)).is_err());
    }

    #[tokio::test]
    async fn external_handle_cannot_be_removed() {
        let root = temp_repo("external").await;
        let external = root.join("external");
        git_text(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "external-branch",
                external.to_str().unwrap(),
                "HEAD",
            ],
        )
        .await
        .unwrap();
        let cfg = config_for_repo(&root, "external");
        enter_existing(&cfg, &external).await.unwrap();
        let error = exit(&cfg, ExitAction::Remove, true).await.unwrap_err();
        assert!(error.to_string().contains("not the owner"));
        assert!(cfg.active_worktree.read().unwrap().is_some());
        exit(&cfg, ExitAction::Keep, false).await.unwrap();
        assert!(external.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remove_requires_discard_and_restores_active_on_refusal() {
        let root = temp_repo("refuse").await;
        let cfg = config_for_repo(&root, "refuse");
        assert_eq!(cfg.active_worktree.transition_epoch(), 0);
        enter(&cfg, "dirty").await.unwrap();
        assert_eq!(cfg.active_worktree.transition_epoch(), 1);
        let path = cfg.effective_cwd();
        std::fs::write(path.join("dirty.txt"), "dirty").unwrap();
        let error = exit(&cfg, ExitAction::Remove, false).await.unwrap_err();
        assert_eq!(cfg.active_worktree.transition_epoch(), 3);
        assert!(error.to_string().contains("1 uncommitted file"));
        assert_eq!(cfg.effective_cwd(), path);
        exit(&cfg, ExitAction::Remove, true).await.unwrap();
        assert_eq!(cfg.active_worktree.transition_epoch(), 4);
        assert_eq!(cfg.effective_cwd(), root);
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shutdown_retains_even_clean_session_tree() {
        let root = temp_repo("shutdown").await;
        let cfg = config_for_repo(&root, "shutdown");
        enter(&cfg, "kept").await.unwrap();
        let path = cfg.effective_cwd();
        let note = finish_active(&cfg).await.unwrap();
        assert!(note.contains("retained"));
        assert!(path.exists());
        assert_eq!(cfg.effective_cwd(), root);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn session_remove_does_not_touch_task_owned_tree() {
        let root = temp_repo("owners").await;
        let task = create(&root, "task-owner").await.unwrap();
        let task_path = task.path.clone();
        std::fs::write(task_path.join("task.txt"), "task").unwrap();

        let cfg = config_for_repo(&root, "owners");
        enter(&cfg, "session-owner").await.unwrap();
        let session_path = cfg.effective_cwd();
        exit(&cfg, ExitAction::Remove, false).await.unwrap();
        assert!(!session_path.exists());
        assert!(task_path.join("task.txt").exists());
        assert!(finish(task).await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn concurrent_session_enter_publishes_exactly_one_active_handle() {
        let root = temp_repo("session-concurrent").await;
        let cfg = config_for_repo(&root, "session-concurrent");
        let (first, second) = tokio::join!(enter(&cfg, "first"), enter(&cfg, "second"));
        assert_eq!(usize::from(first.is_ok()) + usize::from(second.is_ok()), 1);
        let active_path = cfg.effective_cwd();
        let canonical_root = std::fs::canonicalize(&root).unwrap();
        assert!(active_path.starts_with(canonical_root.join(WORKTREES_DIR)));
        exit(&cfg, ExitAction::Remove, true).await.unwrap();
        assert_eq!(cfg.effective_cwd(), root);
        let entries = registered_worktrees(&cfg.cwd).await.unwrap();
        assert_eq!(entries.len(), 1);
        let _ = std::fs::remove_dir_all(&cfg.cwd);
    }

    #[tokio::test]
    async fn parallel_task_create_and_finish_are_serialized() {
        let root = temp_repo("parallel").await;
        let (first, second) = tokio::join!(create(&root, "agent-a"), create(&root, "agent-b"));
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(finish(first).await.unwrap().is_none());
        assert!(finish(second).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
