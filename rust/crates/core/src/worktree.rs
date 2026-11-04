//! Owned git-worktree resources shared by session switching and task isolation.
//!
//! A handle records repository provenance, custody, and owner. Deletion always
//! revalidates that provenance under the process-wide Git mutation lock. Session
//! and task handles use the same Git operations but never share lifecycle state.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LockResult;
use std::sync::RwLock;
use std::sync::RwLockReadGuard;
use std::sync::RwLockWriteGuard;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::Ordering;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::task::TaskTracker;

use crate::config::Config;
use crate::permissions::Permissions;
use crate::sandbox::SandboxPolicy;

pub(crate) use crate::git::git_compatible_path;
#[cfg(test)]
pub(crate) use crate::git::isolate_test_git_environment;

/// Managed worktrees live in kloop's own repository namespace, not in another
/// agent's directory (cc's `.claude/worktrees`).
///
/// Nesting under `.kloop/` costs an exemption: `.kloop` is otherwise a sensitive
/// path component — kloop's own state, where a write is privilege escalation and
/// a *read* is a hard unapprovable deny — so a checkout below it would classify
/// every edit and every read the agent makes in its own worktree as an attack on
/// kloop itself. [`crate::permissions`] carves out exactly this segment pair; cc
/// pays the same price for the same tidiness (`isDangerousFilePathToAutoEdit`).
pub(crate) const WORKTREES_DIR: &str = ".kloop/worktrees";
/// Branches carry kloop's namespace for the same reason the directory does:
/// cc creates `worktree-<slug>` verbatim, so sharing the prefix makes the two
/// tools' branches indistinguishable in `git branch`.
///
/// Flat rather than a slashed `kloop/worktree/<name>`: refs are files, so a
/// slashed prefix needs `refs/heads/kloop` to be a directory, which reserves the
/// branch name `kloop` for as long as any managed tree lives — a plausible name
/// to want, traded for a cosmetic grouping win. It is the same D/F conflict
/// [`encode_name`] already avoids inside the name itself.
const WORKTREE_BRANCH_PREFIX: &str = "kloop-worktree-";
static WORKTREE_MUTATION_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
#[cfg(test)]
static GENERATED_NAME_SEQUENCE: AtomicU64 = AtomicU64::new(1);

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

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorktreeChanges {
    pub changed_files: usize,
    pub commits: usize,
    /// Fully ignored directories (`target/`, `node_modules/`) created since the
    /// checkout. They are build output, not work: a tree holding only these is
    /// removed, with the names reported. Ignored files still count as content,
    /// because `.env.local` and local configs live in that shape.
    pub ignored_dirs: Vec<String>,
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
pub async fn create(cwd: &Path, name: &str, cleanup_tasks: &TaskTracker) -> Result<Worktree> {
    create_managed(
        cwd,
        name,
        WorktreeOwner::Task(name.to_string()),
        BasePolicy::Head,
        cleanup_tasks,
    )
    .await
}

/// A worktree creation whose git mutation succeeded but whose caller was
/// dropped before it committed. Rollback needs the mutation lock and git, so it
/// runs as a tracked task rather than in `Drop`; `tasks` is the session tracker,
/// so shutdown waits for it like any other cleanup supervisor.
struct CreationGuard {
    tasks: TaskTracker,
    repository_root: PathBuf,
    branch: String,
    path: PathBuf,
    armed: bool,
}

impl CreationGuard {
    fn new(tasks: &TaskTracker, repository_root: &Path, branch: &str, path: &Path) -> Self {
        Self {
            tasks: tasks.clone(),
            repository_root: repository_root.to_path_buf(),
            branch: branch.to_string(),
            path: path.to_path_buf(),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CreationGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let root = self.repository_root.clone();
        let branch = self.branch.clone();
        let path = self.path.clone();
        self.tasks.spawn_on(
            async move { rollback_created(&root, &branch, &path).await },
            &handle,
        );
    }
}

async fn rollback_created(repository_root: &Path, branch: &str, path: &Path) {
    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    rollback_created_locked(repository_root, branch, path).await;
}

/// 调用方已持有修改锁；只回滚 add 已成功的资源，短暂失败沿用有界重试。
async fn rollback_created_locked(repository_root: &Path, branch: &str, path: &Path) {
    let path_text = git_compatible_path(path).to_string_lossy().to_string();
    for attempt in 0..5 {
        let removed = git_text(
            repository_root,
            &["worktree", "remove", "--force", &path_text],
        )
        .await
        .is_ok();
        let deleted = git_text(repository_root, &["branch", "-D", branch])
            .await
            .is_ok();
        if removed && deleted {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50 * (attempt + 1))).await;
    }
}

#[cfg(test)]
pub(crate) mod creation_probe {
    use std::sync::Mutex;

    use tokio::sync::oneshot;

    struct Probe {
        reached: oneshot::Sender<()>,
        release: oneshot::Receiver<()>,
    }

    static PROBES: Mutex<Vec<(String, Probe)>> = Mutex::new(Vec::new());
    static BEFORE_ADD_PROBES: Mutex<Vec<(String, Probe)>> = Mutex::new(Vec::new());

    /// Pause the named creation after `git worktree add`. The returned sender
    /// releases it; keeping it alive parks the creation at the gate.
    pub(crate) fn install(name: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        install_into(&PROBES, name)
    }

    /// Pause before `git worktree add`, after the pre-checks passed.
    pub(crate) fn install_before_add(name: &str) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        install_into(&BEFORE_ADD_PROBES, name)
    }

    fn install_into(
        gates: &Mutex<Vec<(String, Probe)>>,
        name: &str,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached, reached_rx) = oneshot::channel();
        let (release, release_rx) = oneshot::channel();
        gates.lock().unwrap().push((
            name.to_string(),
            Probe {
                reached,
                release: release_rx,
            },
        ));
        (reached_rx, release)
    }

    pub(crate) async fn pause(name: &str) {
        pause_in(&PROBES, name).await;
    }

    pub(crate) async fn pause_before_add(name: &str) {
        pause_in(&BEFORE_ADD_PROBES, name).await;
    }

    async fn pause_in(gates: &Mutex<Vec<(String, Probe)>>, name: &str) {
        let probe = {
            let mut probes = gates.lock().unwrap();
            probes
                .iter()
                .position(|(candidate, _)| candidate == name)
                .map(|index| probes.swap_remove(index))
        };
        if let Some((_, Probe { reached, release })) = probe {
            let _ = reached.send(());
            let _ = release.await;
        }
    }
}

async fn create_managed(
    cwd: &Path,
    name: &str,
    owner: WorktreeOwner,
    base_policy: BasePolicy,
    cleanup_tasks: &TaskTracker,
) -> Result<Worktree> {
    validate_name(name)?;
    let repository = discover_repository(cwd)
        .await
        .context("worktree isolation requires a git repository")?;
    let encoded_name = encode_name(name);
    let path = repository.root.join(WORKTREES_DIR).join(&encoded_name);
    let branch = format!("{WORKTREE_BRANCH_PREFIX}{encoded_name}");

    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    if tokio::fs::symlink_metadata(&path).await.is_ok() {
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
    tokio::fs::create_dir_all(repository.root.join(WORKTREES_DIR))
        .await
        .context("creating managed worktree directory")?;
    exclude_worktrees_dir(&repository.common_dir).await?;

    #[cfg(test)]
    creation_probe::pause_before_add(name).await;
    if let Err(error) = git_worktree_add(&repository.root, &branch, &path, &base).await {
        // A concurrently started kloop (the process-wide lock is not a
        // cross-process one) may have created this exact path/branch between
        // our pre-check and `add`. A failed add therefore proves nothing about
        // ownership, and force-removing here could delete another session's
        // tree, branch, and uncommitted work. Leave whatever exists in place.
        return Err(error.context(format!("git worktree add for branch {branch}")));
    }
    // add 成功才确认删除权；此前取消也不能强制删除并发创建者的同名资源。
    let mut creation = CreationGuard::new(cleanup_tasks, &repository.root, &branch, &path);
    #[cfg(test)]
    creation_probe::pause(name).await;

    let created = async {
        let canonical_path = tokio::fs::canonicalize(&path)
            .await
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
        rollback_created_locked(&repository.root, &branch, &path).await;
    }
    creation.disarm();
    created
}

/// 取消时结束直接子进程；add 尚未返回成功时不取得删除权，可能留下部分资源。
async fn git_worktree_add(root: &Path, branch: &str, path: &Path, base: &str) -> Result<()> {
    let path_text = git_compatible_path(path).to_string_lossy().to_string();
    let mut command = git_command(
        root,
        &[
            "worktree",
            "add",
            "--no-track",
            "-b",
            branch,
            &path_text,
            base,
        ],
    );
    command.kill_on_drop(true);
    let output = kloop_process_spawn::output(&mut command)
        .await
        .context("spawning git")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
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
/// retained. Fully ignored directories are build output and go with the tree;
/// the returned note names them. Probe/remove failures propagate and leave the
/// resource in place.
pub async fn finish(mut worktree: Worktree) -> Result<Option<String>> {
    let _mutation = WORKTREE_MUTATION_LOCK.lock().await;
    verify_provenance(&worktree).await?;
    let changes = inspect_changes(&worktree).await?;
    if changes.changed_files > 0 || changes.commits > 0 {
        worktree.lifecycle = WorktreeLifecycle::Kept;
        return Ok(Some(retained_note(&worktree)));
    }
    remove_verified_managed_locked(&mut worktree, /* force */ false).await?;
    Ok(discarded_ignored_note(&changes.ignored_dirs))
}

pub(crate) async fn compute_overrides(
    base_cwd: &Path,
    base_permissions: &Arc<Permissions>,
    base_sandbox: &Option<Arc<SandboxPolicy>>,
    base_system: &str,
    worktree_path: &Path,
) -> Result<(Arc<Permissions>, Option<Arc<SandboxPolicy>>, String)> {
    let expected_project = base_permissions.identity().project_id();
    let probe_path = worktree_path.to_path_buf();
    // Identity probing canonicalizes and starts Git; keep it off the async
    // workers. The parent identity is already carried by the live permission
    // policy, so only the worktree needs a fresh probe.
    let worktree_identity = tokio::task::spawn_blocking(move || {
        crate::project::WorkspaceIdentity::resolve(&probe_path)
    })
    .await
    .context("resolving worktree project identity")?;
    if expected_project.is_none() || worktree_identity.project_id() != expected_project {
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

pub fn generated_name() -> Result<String> {
    Ok(crate::resource_id::fresh("wt-")?)
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
    let worktree =
        create_managed(&cfg.cwd, name, owner, BasePolicy::Fresh, &cfg.cleanup_tasks).await?;
    let message = format!(
        "Created worktree at {path} on branch {branch}. The session is now working in the worktree. Use exit_worktree to leave mid-session.",
        path = worktree.path.display(),
        branch = worktree.branch,
    );
    let overrides = match active_overrides(cfg, &worktree).await {
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
    // The tool resolves relative paths against the session's effective cwd,
    // never the process cwd: server threads and a session already inside a
    // worktree do not share the front-end's working directory.
    let requested = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cfg.effective_cwd().join(path)
    };
    let canonical_path = tokio::fs::canonicalize(&requested)
        .await
        .with_context(|| format!("Cannot enter worktree: {}", requested.display()))?;
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
                "Cannot enter worktree: {} is not under {}. Switching from this session is limited to worktrees managed under .kloop/worktrees of this repository.",
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
    let overrides = active_overrides(cfg, &worktree).await?;
    install_active(cfg, worktree, overrides);
    Ok(message)
}

type WorktreeOverrides = (Arc<Permissions>, Option<Arc<SandboxPolicy>>, String);

async fn active_overrides(cfg: &Config, worktree: &Worktree) -> Result<WorktreeOverrides> {
    compute_overrides(
        &cfg.cwd,
        &cfg.permissions,
        &cfg.sandbox,
        &cfg.system,
        &worktree.path,
    )
    .await
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
            if tokio::fs::try_exists(&active.wt.path)
                .await
                .unwrap_or(false)
            {
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
    remove_verified_managed_locked(worktree, discard_changes).await?;
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
    let ignored = if changes.ignored_dirs.is_empty() {
        String::new()
    } else {
        format!(
            " Ignored build output removed: {}.",
            describe_ignored_dirs(&changes.ignored_dirs)
        )
    };
    let detail = if discarded.is_empty() {
        String::new()
    } else {
        format!(" Discarded {}.", discarded.join(" and "))
    };
    format!(
        "Exited and removed worktree at {}.{detail}{ignored} Session is now back in {original_cwd}.",
        worktree.path.display(),
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

fn discarded_ignored_note(ignored_dirs: &[String]) -> Option<String> {
    if ignored_dirs.is_empty() {
        return None;
    }
    Some(format!(
        "\n\n[Worktree removed; build output in ignored directories was discarded: {}]",
        describe_ignored_dirs(ignored_dirs)
    ))
}

/// Bound the note: a checkout may have arbitrarily many ignored directories.
fn describe_ignored_dirs(ignored_dirs: &[String]) -> String {
    const SHOWN: usize = 8;
    let shown = ignored_dirs
        .iter()
        .take(SHOWN)
        .map(|dir| format!("{dir}/"))
        .collect::<Vec<_>>()
        .join(", ");
    match ignored_dirs.len().saturating_sub(SHOWN) {
        0 => shown,
        more => format!("{shown} and {more} more"),
    }
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
    let mut ignored_dirs = Vec::new();
    while let Some(field) = fields.next() {
        let status = field.get(..2).unwrap_or_default();
        if status.contains('R') || status.contains('C') {
            let _ = fields.next();
        }
        let path = field.get(3..).unwrap_or_default();
        if status == "!!"
            && let Some(dir) = path.strip_suffix('/')
        {
            // A fully ignored directory is collapsed to one trailing-slash
            // entry, so a large target/ scan never walks its files. Whether it
            // is build output needs the ignore rule below; ignored files stay
            // individual and still count as content.
            ignored_dirs.push(dir.to_string());
            continue;
        }
        changed_files += 1;
    }
    ignored_dirs.sort();
    ignored_dirs.dedup();
    let (ignored_dirs, retained_dirs) = classify_ignored_dirs(worktree, &ignored_dirs).await;
    changed_files += retained_dirs;
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
        ignored_dirs,
    })
}

/// A directory is build output only when the ignore pattern that decides it is
/// directory-scoped (`target/`, `secrets/`, `tmp/`). `/*` and bare `node_modules`
/// also match directories but may contain hand-written files; those count as
/// content. A directory whose contents merely match (`*.log`) is never
/// collapsed, so it arrives as individual ignored files instead.
/// `check-ignore` runs once, only when a tree has ignored directories, and any
/// failure retains everything rather than delete by guess.
async fn classify_ignored_dirs(worktree: &Worktree, dirs: &[String]) -> (Vec<String>, usize) {
    if dirs.is_empty() {
        return (Vec::new(), 0);
    }
    let mut input = String::new();
    for dir in dirs {
        input.push_str(dir);
        input.push('/');
        input.push('\0');
    }
    let output = match git_text_with_stdin(
        &worktree.path,
        &["check-ignore", "-v", "-z", "--stdin"],
        &input,
    )
    .await
    {
        Ok(output) => output,
        Err(_) => return (Vec::new(), dirs.len()),
    };
    let mut fields = output.split('\0');
    let mut build_dirs = Vec::new();
    while let (Some(_source), Some(_line), Some(pattern), Some(path)) =
        (fields.next(), fields.next(), fields.next(), fields.next())
    {
        if pattern.ends_with('/') && !pattern.starts_with('!') {
            build_dirs.push(path.trim_end_matches('/').to_string());
        }
    }
    build_dirs.sort();
    build_dirs.dedup();
    let retained = dirs.len().saturating_sub(build_dirs.len());
    (build_dirs, retained)
}

async fn git_text_with_stdin(dir: &Path, args: &[&str], input: &str) -> Result<String> {
    let mut command = git_command(dir, args);
    command
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = kloop_process_spawn::spawn(&mut command).context("spawning git")?;
    let stdin_pipe = child.stdin.take();
    let (written, output) = tokio::join!(
        async {
            if let Some(mut pipe) = stdin_pipe {
                pipe.write_all(input.as_bytes()).await?;
            }
            Ok::<_, std::io::Error>(())
        },
        child.wait_with_output()
    );
    let output = output.context("waiting for git")?;
    if !output.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    written.context("writing git stdin")?;
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// Callers must have validated provenance under the same mutation lock; this
/// is the single deletion path so a verification never runs twice.
async fn remove_verified_managed_locked(worktree: &mut Worktree, force: bool) -> Result<()> {
    if worktree.custody != WorktreeCustody::Managed {
        bail!("refusing to remove an external worktree");
    }
    let path = git_compatible_path(&worktree.path)
        .to_string_lossy()
        .to_string();
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
    let actual_path = tokio::fs::canonicalize(&worktree.path)
        .await
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
    let root = registered_repository_root(Path::new(current_root.trim())).await?;
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
    tokio::fs::canonicalize(path.trim())
        .await
        .context("canonicalizing git common directory")
}

async fn registered_worktrees(repository_root: &Path) -> Result<Vec<RegisteredWorktree>> {
    let output = git_text(repository_root, &["worktree", "list", "--porcelain"]).await?;
    let mut entries = Vec::new();
    for registration in parse_registrations(&output) {
        // One stale registration (a directory removed by hand, an unmounted
        // volume) must not fail every worktree operation. An operation that
        // targets it still fails on its own canonicalize/registration check.
        let Ok(path) = tokio::fs::canonicalize(&registration.path).await else {
            continue;
        };
        entries.push(RegisteredWorktree {
            path,
            head: registration.head,
            branch: registration.branch,
        });
    }
    Ok(entries)
}

/// The first registration is the repository root. Its disappearance is not a
/// stale peer: nothing can place a managed tree without it, so fail loudly
/// instead of promoting a linked worktree to root.
async fn registered_repository_root(repository_root: &Path) -> Result<PathBuf> {
    let output = git_text(repository_root, &["worktree", "list", "--porcelain"]).await?;
    let first = parse_registrations(&output)
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("git worktree registry is empty"))?;
    tokio::fs::canonicalize(&first.path).await.with_context(|| {
        format!(
            "canonicalizing registered worktree {}",
            first.path.display()
        )
    })
}

#[derive(Debug)]
struct Registration {
    path: PathBuf,
    head: String,
    branch: Option<String>,
}

fn parse_registrations(output: &str) -> Vec<Registration> {
    let mut entries = Vec::new();
    let mut path: Option<PathBuf> = None;
    let mut head = String::new();
    let mut branch = None;
    for line in output.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(path) = path.take() {
                entries.push(Registration {
                    path,
                    head: std::mem::take(&mut head),
                    branch: branch.take(),
                });
            }
            continue;
        }
        if let Some(value) = line.strip_prefix("worktree ") {
            path = Some(PathBuf::from(value));
            head.clear();
            branch = None;
        } else if let Some(value) = line.strip_prefix("HEAD ") {
            head = value.to_string();
        } else if let Some(value) = line.strip_prefix("branch refs/heads/") {
            branch = Some(value.to_string());
        }
    }
    entries
}

async fn exclude_worktrees_dir(common_dir: &Path) -> Result<()> {
    let common_dir = common_dir.to_path_buf();
    // The flock and the read/replace cycle are blocking; keep them off the
    // async workers while the mutation lock is held.
    tokio::task::spawn_blocking(move || exclude_worktrees_dir_blocking(&common_dir))
        .await
        .context("updating git info exclude")?
}

fn exclude_worktrees_dir_blocking(common_dir: &Path) -> Result<()> {
    let exclude = common_dir.join("info").join("exclude");
    let Some(parent) = exclude.parent() else {
        return Ok(());
    };
    std::fs::create_dir_all(parent).context("creating git info directory")?;
    let _lock = crate::state_lock::StateLock::acquire(&parent.join(".kloop-exclude.lock"))?;
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
    crate::resource_id::replace_atomically(
        parent,
        ".kloop-exclude-",
        &exclude,
        next.as_bytes(),
        false,
    )
    .context("updating git info exclude")
}

fn git_command(dir: &Path, args: &[&str]) -> Command {
    let mut command = crate::git::tokio_command(dir);
    command.args(args);
    command
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
            fixture_git(&root, &args).await;
        }
        root
    }

    /// Fixture commits stay off the host's Git configuration, so a global
    /// `commit.gpgSign` or hooks path cannot fail the setup.
    async fn fixture_git(dir: &Path, args: &[&str]) {
        let mut command = crate::git::tokio_hermetic_command(dir);
        command.args(args);
        let output = kloop_process_spawn::output(&mut command).await.unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// `.git/info/exclude` belongs to the user's repository, not to kloop's
    /// private store: replacing it must not hand it the 0600 temp-file mode.
    #[cfg(unix)]
    #[tokio::test]
    async fn excluding_worktrees_keeps_existing_permissions() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "kloop-wt-exclude-{}-{}",
            std::process::id(),
            GENERATED_NAME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        let info = root.join("info");
        std::fs::create_dir_all(&info).unwrap();
        let exclude = info.join("exclude");
        std::fs::write(&exclude, "existing\n").unwrap();
        std::fs::set_permissions(&exclude, std::fs::Permissions::from_mode(0o644)).unwrap();

        exclude_worktrees_dir(&root).await.unwrap();

        assert_eq!(
            std::fs::metadata(&exclude).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(
            std::fs::read_to_string(&exclude).unwrap(),
            "existing\n.kloop/worktrees/\n"
        );
        let _ = std::fs::remove_dir_all(&root);
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

    /// Creation needs an owner for a cancelled rollback; these tests do not
    /// inspect that tracker.
    fn cleanup_tasks() -> TaskTracker {
        TaskTracker::default()
    }

    /// A creation cancelled after `git worktree add` — here parked at the
    /// post-add gate — must give back the tree and branch it already made; the
    /// tool future is dropped with no chance to run the error path (plan 249).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_creation_rolls_back_the_tree_and_branch() {
        let root = temp_repo("cancel-create").await;
        let name = "cancel-create";
        let (reached, _release) = creation_probe::install(name);
        let tasks = cleanup_tasks();
        let create = tokio::spawn({
            let root = root.clone();
            let tasks = tasks.clone();
            async move { create(&root, name, &tasks).await }
        });

        reached.await.expect("creation reached the post-add gate");
        create.abort();
        let _ = create.await;
        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(10), tasks.wait())
            .await
            .expect("the rollback finished");

        let trees: Vec<_> = std::fs::read_dir(root.join(WORKTREES_DIR))
            .map(|entries| entries.filter_map(Result::ok).collect())
            .unwrap_or_default();
        assert!(
            trees.is_empty(),
            "the half-created tree was removed: {trees:?}"
        );
        let branches = crate::git::command(&root)
            .args(["branch", "--list", "kloop-worktree-*"])
            .output()
            .unwrap();
        assert!(
            String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the half-created branch was removed"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// A failed `git worktree add` proves nothing about ownership: another
    /// kloop process can create the same name between our pre-check and add.
    /// The loser must not force-remove the winner's tree, branch, or work.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_failed_add_does_not_remove_another_creators_worktree() {
        let root = temp_repo("create-race").await;
        let name = "shared-name";
        let (reached, release) = creation_probe::install_before_add(name);
        let tasks = cleanup_tasks();
        let loser = tokio::spawn({
            let root = root.clone();
            let tasks = tasks.clone();
            async move { create(&root, name, &tasks).await }
        });

        reached.await.expect("the loser reached the pre-add gate");
        // Simulate the other process winning while this one is paused.
        let path = root.join(WORKTREES_DIR).join(name);
        let path_text = path.to_string_lossy().into_owned();
        fixture_git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "kloop-worktree-shared-name",
                path_text.as_str(),
                "HEAD",
            ],
        )
        .await;
        std::fs::write(path.join("winner.txt"), "keep").unwrap();
        release.send(()).unwrap();

        let _error = loser.await.unwrap().unwrap_err();
        assert!(
            path.join("winner.txt").exists(),
            "the winner's tree and uncommitted work survived"
        );
        let branches = crate::git::command(&root)
            .args(["branch", "--list", "kloop-worktree-shared-name"])
            .output()
            .unwrap();
        assert!(
            !String::from_utf8_lossy(&branches.stdout).trim().is_empty(),
            "the winner's branch survived"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // 尚未确认 add 成功就取消，不能把并发创建者的资源当成本次调用的资源删除。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cancelled_add_does_not_remove_another_creators_worktree() {
        let root = temp_repo("cancel-create-race").await;
        let name = "cancelled-shared-name";
        let (reached, _release) = creation_probe::install_before_add(name);
        let tasks = cleanup_tasks();
        let loser = tokio::spawn({
            let root = root.clone();
            let tasks = tasks.clone();
            async move { create(&root, name, &tasks).await }
        });

        reached.await.expect("the loser reached the pre-add gate");
        let path = root.join(WORKTREES_DIR).join(name);
        let path_text = path.to_string_lossy().into_owned();
        let branch = format!("{WORKTREE_BRANCH_PREFIX}{name}");
        fixture_git(
            &root,
            &["worktree", "add", "-q", "-b", &branch, &path_text, "HEAD"],
        )
        .await;
        std::fs::write(path.join("winner.txt"), "keep").unwrap();
        let head = git_text(&root, &["rev-parse", &branch]).await.unwrap();

        loser.abort();
        assert!(loser.await.unwrap_err().is_cancelled());
        tasks.close();
        tokio::time::timeout(std::time::Duration::from_secs(10), tasks.wait())
            .await
            .expect("any rollback finished before checking the winner");

        assert_eq!(
            std::fs::read_to_string(path.join("winner.txt")).ok(),
            Some("keep".to_string()),
            "the winner's uncommitted work survived the loser's cancellation"
        );
        assert_eq!(
            git_text(&root, &["rev-parse", &branch]).await.unwrap(),
            head
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn create_records_owned_provenance_and_removes_clean_tree() {
        let root = temp_repo("create").await;
        let worktree = create(&root, "agent-1", &cleanup_tasks()).await.unwrap();
        let path = worktree.path.clone();
        assert_eq!(worktree.custody, WorktreeCustody::Managed);
        assert_eq!(worktree.owner, WorktreeOwner::Task("agent-1".into()));
        assert!(path.ends_with(".kloop/worktrees/agent-1"));
        assert_eq!(worktree.branch, "kloop-worktree-agent-1");
        assert!(finish(worktree).await.unwrap().is_none());
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn finish_keeps_dirty_task_tree() {
        let root = temp_repo("dirty").await;
        let worktree = create(&root, "agent-2", &cleanup_tasks()).await.unwrap();
        std::fs::write(worktree.path.join("new.txt"), "work").unwrap();
        let path = worktree.path.clone();
        let note = finish(worktree).await.unwrap().unwrap();
        assert!(note.contains("kloop-worktree-agent-2"));
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ignored_files_are_content_changes() {
        let root = temp_repo("ignored").await;
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        fixture_git(&root, &["add", ".gitignore"]).await;
        fixture_git(&root, &["commit", "-qm", "ignore"]).await;
        let worktree = create(&root, "agent-ignored", &cleanup_tasks())
            .await
            .unwrap();
        std::fs::write(worktree.path.join("ignored.txt"), "work").unwrap();
        let changes = inspect_changes(&worktree).await.unwrap();
        assert_eq!(changes.changed_files, 1);
        assert!(changes.ignored_dirs.is_empty());
        assert!(finish(worktree).await.unwrap().is_some());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ignored_build_directories_are_discarded_not_retained() {
        let root = temp_repo("ignored-dir").await;
        std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
        fixture_git(&root, &["add", ".gitignore"]).await;
        fixture_git(&root, &["commit", "-qm", "ignore"]).await;
        let worktree = create(&root, "agent-build", &cleanup_tasks())
            .await
            .unwrap();
        let path = worktree.path.clone();
        std::fs::create_dir_all(path.join("target/debug")).unwrap();
        std::fs::write(path.join("target/debug/artifact"), "bin").unwrap();

        let changes = inspect_changes(&worktree).await.unwrap();
        assert_eq!(changes.changed_files, 0);
        assert_eq!(changes.ignored_dirs, vec!["target".to_string()]);
        let note = finish(worktree).await.unwrap().expect("discard note");
        assert!(note.contains("target/"), "{note}");
        assert!(
            !path.exists(),
            "a tree holding only build output is removed"
        );
        let _ = std::fs::remove_dir_all(root);
    }

    /// A tree where ignored directories are classified one by one: `target/`
    /// is decided by a directory-scoped rule and dropped, while `notes/` is
    /// swept in by `/*` and keeps the tree.
    #[tokio::test]
    async fn ignored_directories_are_classified_one_by_one() {
        let root = temp_repo("ignored-mixed").await;
        std::fs::write(
            root.join(".gitignore"),
            "/*\n!/src/\n!/.gitignore\ntarget/\n",
        )
        .unwrap();
        fixture_git(&root, &["add", ".gitignore"]).await;
        fixture_git(&root, &["commit", "-qm", "ignore"]).await;
        let worktree = create(&root, "agent-mixed", &cleanup_tasks())
            .await
            .unwrap();
        let path = worktree.path.clone();
        for entry in ["target/debug/artifact", "notes/plan.md"] {
            let file = path.join(entry);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "content").unwrap();
        }

        let changes = inspect_changes(&worktree).await.unwrap();
        assert_eq!(changes.ignored_dirs, vec!["target".to_string()]);
        assert_eq!(changes.changed_files, 1, "{changes:?}");
        assert!(finish(worktree).await.unwrap().is_some());
        assert!(path.join("notes/plan.md").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    /// `/*`, a bare `secrets`, and a bare `node_modules` match directories but
    /// are not directory-scoped rules, so they may hold hand-written work and
    /// must keep the tree.
    #[tokio::test]
    async fn ignored_directories_without_a_directory_rule_are_content() {
        let root = temp_repo("ignored-broad").await;
        std::fs::write(
            root.join(".gitignore"),
            "/*\n!/src/\n!/.gitignore\nsecrets\nnode_modules\n",
        )
        .unwrap();
        fixture_git(&root, &["add", ".gitignore"]).await;
        fixture_git(&root, &["commit", "-qm", "ignore"]).await;
        let worktree = create(&root, "agent-broad", &cleanup_tasks())
            .await
            .unwrap();
        let path = worktree.path.clone();
        for entry in [
            "notes/plan.md",
            "src/secrets/key",
            "node_modules/pkg/index.js",
        ] {
            let file = path.join(entry);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, "hand-written").unwrap();
        }

        let changes = inspect_changes(&worktree).await.unwrap();
        assert_eq!(changes.changed_files, 3, "{changes:?}");
        assert!(changes.ignored_dirs.is_empty(), "{changes:?}");
        assert!(finish(worktree).await.unwrap().is_some());
        assert!(path.exists(), "the hand-written tree is retained");
        let _ = std::fs::remove_dir_all(root);
    }

    /// A registration whose directory was removed must not fail every
    /// worktree operation of the repository.
    #[tokio::test]
    async fn a_stale_registration_does_not_break_other_worktree_operations() {
        let root = temp_repo("stale-registration").await;
        let first = create(&root, "first", &cleanup_tasks()).await.unwrap();
        let stale = root.join(WORKTREES_DIR).join("stale");
        let stale_text = stale.to_string_lossy().into_owned();
        fixture_git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "kloop-worktree-stale",
                &stale_text,
                "HEAD",
            ],
        )
        .await;
        std::fs::remove_dir_all(&stale).unwrap();

        let second = create(&root, "second", &cleanup_tasks()).await.unwrap();
        assert!(finish(first).await.unwrap().is_none());
        let cfg = config_for_repo(&root, "stale-registration");
        enter(&cfg, "session").await.unwrap();
        exit(&cfg, ExitAction::Remove, true).await.unwrap();
        assert!(finish(second).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }

    /// `finish` verifies provenance, probes changes, and removes: the deletion
    /// itself must not repeat the two Git calls of the verification.
    #[tokio::test]
    async fn finish_verifies_provenance_once() {
        let root = temp_repo("finish-git-count").await;
        let worktree = create(&root, "counted", &cleanup_tasks()).await.unwrap();
        let before = crate::git::test_git_command_count();
        assert!(finish(worktree).await.unwrap().is_none());
        let calls = crate::git::test_git_command_count() - before;
        assert_eq!(calls, 6, "finish must verify provenance exactly once");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn workspace_overrides_reject_project_drift_from_loaded_permissions() {
        let trusted = temp_repo("trusted-policy").await;
        let changed = temp_repo("changed-policy").await;
        let candidate = create(&changed, "candidate", &cleanup_tasks())
            .await
            .unwrap();
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
            .await
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
        let task = create(&root, "task-owner", &cleanup_tasks()).await.unwrap();
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
        let tasks = cleanup_tasks();
        let (first, second) = tokio::join!(
            create(&root, "agent-a", &tasks),
            create(&root, "agent-b", &tasks)
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert!(finish(first).await.unwrap().is_none());
        assert!(finish(second).await.unwrap().is_none());
        let _ = std::fs::remove_dir_all(root);
    }
}
