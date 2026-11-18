//! The one place a tool anchors a path argument. The permission gate resolves
//! the same way, so the check and the IO cannot disagree about the target.

/// Anchor a tool's path argument at the agent's cwd: an absolute path is used
/// as-is, a relative one resolves against `cwd`. For the main agent `cwd` is
/// the process cwd, so this is a no-op there; for a worktree sub-agent (plan
/// 35) it is what keeps the sub-agent's relative reads/writes inside its own
/// tree instead of leaking to the process cwd. Same anchor the permission gate
/// uses, so the check and the IO never disagree about where a path points.
pub(crate) fn resolve_path(cwd: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}
