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

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use tokio::process::Command;
use tokio::sync::Mutex;

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
            "\n\n[This sub-agent left changes in its worktree {path} (branch {branch}); they were \
             NOT merged. Inspect them there — commit and `git merge {branch}` to bring them in, or \
             `git worktree remove {path}` to discard.]",
            branch = wt.branch,
            path = wt.path.display(),
        ));
    }
    // Untouched: remove the tree and its branch. --force covers a tree git
    // still considers "in use"; branch -D since it was never merged. Under the
    // lock — a remove racing a sibling's `worktree add` corrupts the registry.
    let path_str = wt.path.to_string_lossy().to_string();
    let _guard = WORKTREE_LOCK.lock().await;
    let _ = git(&wt.repo_root, &["worktree", "remove", "--force", &path_str]).await;
    let _ = git(&wt.repo_root, &["branch", "-D", &wt.branch]).await;
    None
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
