//! git worktree isolation for parallel sub-agents (plan 35).
//!
//! A `task {isolation: "worktree"}` sub-agent runs in its own checkout so two
//! agents can edit the same relative path at once without colliding. The shape
//! is where claude-code and codex independently converged: a repo-local
//! worktree dir + one branch per tree, created off HEAD, and a lifecycle that
//! **never merges back** — an unchanged tree is torn down, a changed one is
//! kept on its branch for the user (or parent) to reconcile. Nothing here is
//! automatic beyond create/teardown; there is no PR or auto-merge path, by
//! design (both references stop here too).
//!
//! Layering: this lives in core because the `task` tool (core) owns it and the
//! cwd it rewires anchors core-owned types (permissions, sandbox). It shells
//! out to `git` directly — the same subprocess dependency the bash tool
//! already carries — rather than routing through a provider seam.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::Config;
use crate::permissions::Permissions;
use crate::sandbox::SandboxPolicy;

/// Repo-relative directory holding managed worktrees, one subdir per sub-agent.
/// Deliberately NOT under `.kloop/`: `.kloop` is a protected sensitive path in
/// both the permission gate (`path_is_sensitive`) and the sandbox (`.kloop`
/// read-only subpath), so a worktree there would make every write inside it
/// look like a write to kloop's own config — the gate would ask on each one
/// (bypass-immune) and headless runs would deny it. A sibling dir dodges both
/// while staying repo-local and git-excluded.
const WORKTREES_DIR: &str = ".kloop-worktrees";

/// Serializes worktree git *mutations* across parallel sub-agents. Concurrent
/// `git worktree add` / branch-create / `worktree remove` on one repo race on
/// the repo's ref + worktree-admin locks and one silently loses its tree
/// (reproduced: two parallel isolated sub-agents, only one tree survives). The
/// operations are brief, so a process-wide lock costs nothing and buys
/// correctness. Read probes (`status`/`rev-list` inside a single worktree)
/// don't take it — they don't touch shared repo state.
static WORKTREE_LOCK: Mutex<()> = Mutex::const_new(());

/// A live worktree the `task` tool created for a sub-agent. Dropping it does
/// NOT clean up — the caller runs [`finish`] once the sub-agent ends, because
/// teardown depends on whether the tree was left dirty.
#[derive(Debug)]
pub struct Worktree {
    /// The sub-agent's cwd: `<repo>/.kloop-worktrees/<name>`.
    pub path: PathBuf,
    /// The branch the tree checks out: `kloop/worktree/<name>`.
    pub branch: String,
    /// The commit the tree was branched from (HEAD at creation); teardown
    /// keeps the tree if any commit landed beyond it.
    base: String,
    /// The main repository root (worktree registry + `.git` live here).
    repo_root: PathBuf,
}

/// Create an isolated worktree named `name` for a sub-agent whose parent cwd
/// is `cwd`. Fail-closed (cc's shape): a non-git cwd, a name collision, or a
/// git failure is an error — never a silent fall back to the shared cwd.
pub async fn create(cwd: &Path, name: &str) -> Result<Worktree> {
    let repo_root = repo_root(cwd)
        .await
        .context("worktree isolation requires a git repository")?;
    let base = git(&repo_root, &["rev-parse", "HEAD"])
        .await
        .context("cannot read HEAD (repository has no commits yet?)")?
        .trim()
        .to_string();

    let path = repo_root.join(WORKTREES_DIR).join(name);
    if path.exists() {
        bail!("worktree {} already exists", path.display());
    }
    let branch = format!("kloop/worktree/{name}");

    let path_str = path.to_string_lossy().to_string();
    {
        let _guard = WORKTREE_LOCK.lock().await;
        // Keep the managed worktrees dir out of the MAIN repo's status
        // (codex's move); idempotent so repeated spawns don't duplicate the
        // line, and under the lock so parallel spawns don't clobber the file.
        exclude_worktrees_dir(&repo_root)?;
        git(
            &repo_root,
            &[
                "worktree",
                "add",
                "--no-track",
                "-B",
                &branch,
                &path_str,
                &base,
            ],
        )
        .await
        .with_context(|| format!("git worktree add for branch {branch}"))?;
    }

    Ok(Worktree {
        path,
        branch,
        base,
        repo_root,
    })
}

/// End-of-life for a sub-agent's worktree: tear it down if untouched, keep it
/// if the sub-agent left changes. Returns `Some(note)` naming the retained
/// tree + branch for the parent's tool_result (so a human can merge or discard
/// it), `None` when the tree was removed. **Fail-closed**: if the change probe
/// errors, the tree is treated as changed and kept — never silently discarded.
pub async fn finish(wt: Worktree) -> Option<String> {
    if has_changes(&wt).await {
        return Some(format!(
            "\n\n[Changes were left in the worktree {path} (branch {branch}); they were NOT \
             merged. Inspect them there — commit and `git merge {branch}` to bring them in, or \
             `git worktree remove {path}` to discard.]",
            branch = wt.branch,
            path = wt.path.display(),
        ));
    }
    // Untouched: remove the tree and its branch.
    wt.remove().await;
    None
}

impl Worktree {
    /// Delete the tree and its (never-merged) branch. `--force` covers a tree
    /// git still considers "in use"; under the lock so a remove racing a
    /// sibling's `worktree add` can't corrupt the registry.
    async fn remove(&self) {
        let path_str = self.path.to_string_lossy().to_string();
        let _guard = WORKTREE_LOCK.lock().await;
        let _ = git(
            &self.repo_root,
            &["worktree", "remove", "--force", &path_str],
        )
        .await;
        let _ = git(&self.repo_root, &["branch", "-D", &self.branch]).await;
    }
}

/// The cwd-anchored overrides a worktree imposes, computed from a base (the
/// agent's current effective state) — shared by the `task` sub-agent rewire
/// (slice 1) and the session-level [`enter`] (slice 2). Rewriting the system
/// prompt's working-directory line is what stops the model building absolute
/// paths from the OLD cwd and writing past the tree. Returns everything but the
/// cwd itself (which is just `wt_path`).
pub(crate) fn compute_overrides(
    base_cwd: &Path,
    base_permissions: &Arc<Permissions>,
    base_sandbox: &Option<Arc<SandboxPolicy>>,
    base_system: &str,
    wt_path: &Path,
) -> (Arc<Permissions>, Option<Arc<SandboxPolicy>>, String) {
    let old = format!("- Working directory: {}", base_cwd.display());
    let new = format!("- Working directory: {}", wt_path.display());
    let system = base_system.replacen(&old, &new, 1);
    let permissions = Arc::new(base_permissions.rebased(wt_path.to_path_buf()));
    let sandbox = base_sandbox
        .as_ref()
        .map(|sb| Arc::new(sb.with_writable_root(wt_path)));
    (permissions, sandbox, system)
}

/// A worktree the *session itself* has entered (slice 2: `enter_worktree` /
/// `--worktree`), as opposed to a `task` sub-agent's throwaway tree. Held in a
/// mutable slot on [`Config`] so `enter`/`exit` switch the session's cwd at
/// runtime; the `effective_*` accessors read it. One per session at a time.
pub struct ActiveWorktree {
    wt: Worktree,
    pub cwd: PathBuf,
    /// The tree's branch (`kloop/worktree/<name>`), surfaced to a client that
    /// tracks the session cwd (the server's `thread/cwd/updated` notification).
    pub branch: String,
    pub permissions: Arc<Permissions>,
    pub sandbox: Option<Arc<SandboxPolicy>>,
    pub system: String,
}

/// Enter a fresh worktree named `name` for the whole session (mutates the
/// active-worktree slot so the switch takes effect immediately). Errors if
/// already inside one (one tree per session, codex's rule) or if creation
/// fails (fail-closed). Returns the model-facing confirmation.
pub async fn enter(cfg: &Config, name: &str) -> Result<String> {
    if cfg.active_worktree.read().unwrap().is_some() {
        bail!("already inside a worktree; call exit_worktree before entering another");
    }
    // The slot is empty, so the base is cfg.cwd (the main checkout).
    let wt = create(&cfg.cwd, name).await?;
    let (permissions, sandbox, system) = compute_overrides(
        &cfg.cwd,
        &cfg.permissions,
        &cfg.sandbox,
        &cfg.system,
        &wt.path,
    );
    let msg = format!(
        "Entered worktree {path} on branch {branch}. Your working directory is now this tree; \
         edits here are isolated from the main checkout until you exit_worktree.",
        path = wt.path.display(),
        branch = wt.branch,
    );
    *cfg.active_worktree.write().unwrap() = Some(ActiveWorktree {
        cwd: wt.path.clone(),
        branch: wt.branch.clone(),
        permissions,
        sandbox,
        system,
        wt,
    });
    Ok(msg)
}

/// Exit the session's active worktree. Without `discard` it follows the slice-1
/// lifecycle (changes kept on the branch, a clean tree removed); `discard`
/// force-removes even a dirty tree. A friendly no-op when not in one.
pub async fn exit(cfg: &Config, discard: bool) -> Result<String> {
    // Take out of the slot BEFORE any await — never hold the lock across one.
    let active = cfg.active_worktree.write().unwrap().take();
    let Some(active) = active else {
        return Ok("Not currently in a worktree.".to_string());
    };
    // The session returns to `cfg.cwd`: enter only proceeds when the slot is
    // empty (one tree, no nesting), so the pre-entry cwd was always `cfg.cwd`.
    let back = cfg.cwd.display().to_string();
    if discard {
        let branch = active.wt.branch.clone();
        active.wt.remove().await;
        return Ok(format!(
            "Exited and discarded the worktree (branch {branch} deleted). Back in {back}."
        ));
    }
    match finish(active.wt).await {
        Some(note) => Ok(format!("Exited worktree. Back in {back}.{note}")),
        None => Ok(format!(
            "Exited worktree (no changes; tree removed). Back in {back}."
        )),
    }
}

/// Tear down the session's active worktree at shutdown (dirty kept, clean
/// removed), if any. Returns a note when a tree was kept, for the caller to
/// surface. Safe to call when not in a worktree.
pub async fn finish_active(cfg: &Config) -> Option<String> {
    let active = cfg.active_worktree.write().unwrap().take()?;
    finish(active.wt).await
}

/// Whether the sub-agent left anything worth keeping: an uncommitted change in
/// the tree, or a commit past the base. Any git error counts as "changed"
/// (fail-closed) — a probe we can't trust must not license deletion.
async fn has_changes(wt: &Worktree) -> bool {
    let Ok(status) = git(&wt.path, &["status", "--porcelain"]).await else {
        return true;
    };
    if !status.trim().is_empty() {
        return true;
    }
    let range = format!("{}..HEAD", wt.base);
    let Ok(commits) = git(&wt.path, &["rev-list", &range]).await else {
        return true;
    };
    !commits.trim().is_empty()
}

/// The main repository root for `cwd`, or an error when `cwd` is not in a git
/// repository.
async fn repo_root(cwd: &Path) -> Result<PathBuf> {
    let out = git(cwd, &["rev-parse", "--show-toplevel"]).await?;
    Ok(PathBuf::from(out.trim()))
}

/// Append the managed worktrees dir to `.git/info/exclude` unless present.
fn exclude_worktrees_dir(repo_root: &Path) -> Result<()> {
    let exclude = repo_root.join(".git").join("info").join("exclude");
    // A worktree checkout has a `.git` FILE, not this dir — but we only ever
    // create from the main repo (sub-agents can't spawn sub-agents), so the
    // info dir exists. Best-effort: a missing parent just skips the exclude.
    let Some(parent) = exclude.parent() else {
        return Ok(());
    };
    if !parent.is_dir() {
        return Ok(());
    }
    let line = format!("{WORKTREES_DIR}/");
    let line = line.as_str();
    let current = std::fs::read_to_string(&exclude).unwrap_or_default();
    if current.lines().any(|l| l.trim() == line) {
        return Ok(());
    }
    let mut next = current;
    if !next.is_empty() && !next.ends_with('\n') {
        next.push('\n');
    }
    next.push_str(line);
    next.push('\n');
    std::fs::write(&exclude, next).context("updating .git/info/exclude")?;
    Ok(())
}

/// Run `git -C <dir> <args>` and return stdout on success, an error carrying
/// stderr otherwise.
async fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .await
        .context("spawning git")?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A throwaway git repo with one commit, so worktrees can branch off HEAD.
    async fn temp_repo(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("kloop-wt-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "t@example.com"],
            vec!["config", "user.name", "t"],
            vec!["commit", "--allow-empty", "-qm", "base"],
        ] {
            git(&root, &args).await.unwrap();
        }
        root
    }

    #[tokio::test]
    async fn create_makes_tree_branch_and_exclude_entry() {
        let root = temp_repo("create").await;
        let wt = create(&root, "agent-1").await.unwrap();
        // `git rev-parse --show-toplevel` resolves symlinks (macOS /var →
        // /private/var), so compare against the canonicalized root.
        let canon = std::fs::canonicalize(&root).unwrap();
        assert_eq!(wt.path, canon.join(".kloop-worktrees/agent-1"));
        assert_eq!(wt.branch, "kloop/worktree/agent-1");
        assert!(wt.path.join(".git").exists(), "the worktree is checked out");
        let branches = git(&root, &["branch", "--list", "kloop/worktree/agent-1"])
            .await
            .unwrap();
        assert!(branches.contains("kloop/worktree/agent-1"), "{branches}");
        let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap_or_default();
        assert!(
            exclude.lines().any(|l| l.trim() == ".kloop-worktrees/"),
            "exclude injected: {exclude:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn finish_removes_an_untouched_tree() {
        let root = temp_repo("clean").await;
        let wt = create(&root, "agent-1").await.unwrap();
        let path = wt.path.clone();
        let note = finish(wt).await;
        assert!(note.is_none(), "untouched tree reports nothing to keep");
        assert!(!path.exists(), "tree removed");
        let branches = git(&root, &["branch", "--list", "kloop/worktree/agent-1"])
            .await
            .unwrap();
        assert!(branches.trim().is_empty(), "branch deleted: {branches:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn finish_keeps_a_dirty_tree_and_names_it() {
        let root = temp_repo("dirty").await;
        let wt = create(&root, "agent-2").await.unwrap();
        let path = wt.path.clone();
        std::fs::write(path.join("new.txt"), "work").unwrap();
        let note = finish(wt).await.expect("dirty tree is kept");
        assert!(note.contains("kloop/worktree/agent-2"), "{note}");
        assert!(note.contains(&path.display().to_string()), "{note}");
        assert!(path.exists(), "tree preserved for the user");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn create_rejects_a_non_git_dir() {
        let dir = std::env::temp_dir().join(format!("kloop-wt-nogit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let err = create(&dir, "agent-1").await.unwrap_err();
        assert!(err.to_string().contains("git repository"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Two worktrees created (and torn down) concurrently don't clobber each
    /// other — the serialization lock keeps parallel `git worktree add` /
    /// branch-create / remove off each other's repo locks. Each left dirty is
    /// kept with its own file.
    #[tokio::test]
    async fn parallel_create_and_finish_keep_both_dirty_trees() {
        let root = temp_repo("parconc").await;
        let (a, b) = tokio::join!(create(&root, "agent-1"), create(&root, "agent-2"));
        let a = a.unwrap();
        let b = b.unwrap();
        std::fs::write(a.path.join("a.txt"), "a").unwrap();
        std::fs::write(b.path.join("b.txt"), "b").unwrap();
        let (na, nb) = tokio::join!(finish(a), finish(b));
        assert!(na.is_some() && nb.is_some(), "both dirty trees are kept");
        assert!(root.join(".kloop-worktrees/agent-1/a.txt").exists());
        assert!(root.join(".kloop-worktrees/agent-2/b.txt").exists());
        let branches = git(&root, &["branch", "--list", "kloop/worktree/*"])
            .await
            .unwrap();
        assert!(
            branches.contains("agent-1") && branches.contains("agent-2"),
            "{branches}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn create_rejects_a_name_collision() {
        let root = temp_repo("collide").await;
        let _first = create(&root, "agent-1").await.unwrap();
        let err = create(&root, "agent-1").await.unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err:#}");
        let _ = std::fs::remove_dir_all(&root);
    }
}
