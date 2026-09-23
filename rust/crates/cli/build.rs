//! Stamp the commit this binary was built from into it (plan 161).
//!
//! `option_env!("KLOOP_BUILD_SHA")` reads the result back in `args.rs`, and the
//! session banner shows it beside the crate version — which is `0.1.0` for
//! every build there has ever been, so on its own it says nothing about which
//! source a running kloop came from.

use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    // A packager (or anyone building outside a checkout) can hand the sha in.
    println!("cargo:rerun-if-env-changed=KLOOP_BUILD_SHA");
    if let Some(sha) = std::env::var_os("KLOOP_BUILD_SHA") {
        println!("cargo:rustc-env=KLOOP_BUILD_SHA={}", sha.to_string_lossy());
        return;
    }
    // No git, no checkout, or an empty repository: emit nothing, and the banner
    // shows the bare crate version rather than a made-up commit.
    let Some(sha) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return;
    };
    println!("cargo:rustc-env=KLOOP_BUILD_SHA={sha}");
    // Re-stamp only when HEAD moves: committing rewrites the branch's ref file
    // (or packed-refs), checking out rewrites HEAD itself. Editing the tree
    // without committing does NOT re-run this script — the sha means "the commit
    // this was built on", never a claim that the tree was clean.
    for path in head_inputs() {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}

/// The paths that change when HEAD moves. A path Cargo cannot stat counts as
/// always dirty, which would re-run this script on every build, so a missing
/// one is never handed over as is.
///
/// The branch ref flips between two states and both directions must be seen.
/// Loose → packed (`git gc`): the file we watch disappears, which is a change.
/// Packed → loose (the next commit after a pack): a file appears that nobody
/// was watching, and `packed-refs` does not move. Dropping the missing ref here
/// once froze the stamp for 63 commits. So a missing ref is watched through its
/// nearest existing directory instead — Cargo scans a directory for anything
/// newer, and the file being created is exactly that.
fn head_inputs() -> Vec<PathBuf> {
    let Some(git_dir) = git(&["rev-parse", "--absolute-git-dir"]).map(PathBuf::from) else {
        return Vec::new();
    };
    // A linked worktree keeps its own HEAD but shares refs with the main
    // checkout, so refs resolve against the common dir, not the gitdir.
    let common = git(&["rev-parse", "--git-common-dir"])
        .map(from_manifest_dir)
        .unwrap_or_else(|| git_dir.clone());
    let mut inputs = vec![git_dir.join("HEAD")];
    if let Some(head_ref) = git(&["symbolic-ref", "--quiet", "HEAD"]) {
        inputs.push(common.join("packed-refs"));
        inputs.extend(existing_or_ancestor(&common.join(head_ref)));
    }
    inputs.retain(|p| p.exists());
    inputs
}

fn existing_or_ancestor(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.exists()).map(Path::to_path_buf)
}

/// `--git-common-dir` answers relative to the invocation directory, which for a
/// build script is the package root.
fn from_manifest_dir(dir: String) -> PathBuf {
    let dir = PathBuf::from(dir);
    if dir.is_absolute() {
        return dir;
    }
    match std::env::var_os("CARGO_MANIFEST_DIR") {
        Some(root) => Path::new(&root).join(dir),
        None => dir,
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}
