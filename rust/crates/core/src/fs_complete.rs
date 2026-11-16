//! Fuzzy file-path completion for the TUI `@file` menu (plan 38 slice 4).
//!
//! Reuses the `ignore` crate (plan 14's grep/glob family) so it honors
//! .gitignore and never descends into VCS internals, exactly like the model's
//! search tools — a `@`-mention should surface the same files the agent sees.
//! Pure I/O over a directory tree; the interactive glue (trigger detection,
//! menu) lives in the TUI. Kept in core because that is where the `ignore`
//! dependency already is, so the TUI needn't take it on.

use std::path::Path;

use ignore::WalkBuilder;

/// Walk `root` (gitignore-honoring, hidden files included, skipping `.git` and
/// friends) and return up to `cap` file paths matching `query`, most relevant
/// first. `query` is matched case-insensitively as a subsequence of the
/// cwd-relative path; an empty query lists the shallowest files. Directories are
/// omitted — a mention resolves to a file to read.
///
/// The walk is bounded by `SCAN_CAP` visited files so a giant tree can't stall
/// the keystroke that triggered it; the ranking then trims to `cap`.
pub fn complete_files(root: &Path, query: &str, cap: usize) -> Vec<String> {
    /// Stop collecting candidates past this many matched files — enough to rank
    /// well without walking an unbounded tree on every keystroke.
    const SCAN_CAP: usize = 4000;

    let needle = query.to_lowercase();
    let mut walk = WalkBuilder::new(root);
    walk.hidden(/*skip_hidden*/ false);
    // One filter, not one per name: `filter_entry` stores a single predicate,
    // so a loop of them keeps only the last and descends into the other three.
    walk.filter_entry(|entry| !is_vcs_dir(entry.file_name()));

    let mut scored: Vec<(u8, usize, String)> = Vec::new();
    for entry in walk.build().flatten() {
        // Files only: a mention resolves to a file, and the walk still descends
        // into directories to reach the files under them.
        if entry.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        let rel = match entry.path().strip_prefix(root) {
            Ok(p) => p.to_string_lossy().replace('\\', "/"),
            Err(_) => continue,
        };
        if rel.is_empty() {
            continue;
        }
        if let Some(rank) = score(&rel, &needle) {
            scored.push((rank, rel.len(), rel));
            if scored.len() >= SCAN_CAP {
                break;
            }
        }
    }
    // Best rank first, then shorter (shallower) paths, then lexicographic for a
    // stable order.
    scored.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    scored.into_iter().take(cap).map(|(_, _, p)| p).collect()
}

/// Match `path` against a lowercased `needle`, returning a rank (lower is
/// better) or `None` when it doesn't match. Ranks, best to worst: basename
/// prefix, basename substring, path substring, scattered subsequence. An empty
/// needle matches everything at the top rank.
fn score(path: &str, needle: &str) -> Option<u8> {
    if needle.is_empty() {
        return Some(0);
    }
    let lower = path.to_lowercase();
    let base = lower.rsplit('/').next().unwrap_or(&lower);
    if base.starts_with(needle) {
        Some(0)
    } else if base.contains(needle) {
        Some(1)
    } else if lower.contains(needle) {
        Some(2)
    } else if is_subsequence(&lower, needle) {
        Some(3)
    } else {
        None
    }
}

/// Whether `needle`'s chars appear in `haystack` in order (not necessarily
/// contiguous). Both are already lowercased.
fn is_subsequence(haystack: &str, needle: &str) -> bool {
    let mut chars = haystack.chars();
    needle.chars().all(|n| chars.any(|h| h == n))
}

/// The VCS internals a mention never resolves to. A `.git` can be a file (a
/// worktree or submodule pointer), so this matches the name rather than a
/// directory.
fn is_vcs_dir(name: &std::ffi::OsStr) -> bool {
    matches!(name.to_str(), Some(".git" | ".hg" | ".svn" | ".jj"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Tree(std::path::PathBuf);
    impl Tree {
        fn new(tag: &str) -> Self {
            let dir =
                std::env::temp_dir().join(format!("kloop-fscomplete-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // A `.git` marker so .gitignore is honored by the ignore crate.
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            Tree(dir)
        }
        fn write(&self, rel: &str, body: &str) {
            let p = self.0.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }
    impl Drop for Tree {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn ranks_basename_prefix_over_scattered_and_honors_gitignore() {
        let t = Tree::new("rank");
        t.write("src/main.rs", "");
        t.write("src/lib/main_helper.rs", "");
        t.write("my_app_index.md", ""); // scattered m-a-i-n subsequence
        t.write(".gitignore", "target/\n");
        t.write("target/main.o", "");

        let got = complete_files(&t.0, "main", 10);
        // gitignored file excluded; the shortest basename-prefix match wins.
        assert!(
            !got.iter().any(|p| p.contains("target/")),
            "gitignore: {got:?}"
        );
        assert_eq!(got.first().map(String::as_str), Some("src/main.rs"));
        assert!(got.contains(&"src/lib/main_helper.rs".to_string()));
        // Only a scattered subsequence, so it ranks last but still appears.
        assert!(got.contains(&"my_app_index.md".to_string()), "got: {got:?}");
        assert_eq!(got.last().map(String::as_str), Some("my_app_index.md"));
    }

    #[test]
    fn empty_query_lists_shallowest_first_and_skips_git_dir() {
        let t = Tree::new("empty");
        t.write("a.txt", "");
        t.write("deep/nested/b.txt", "");
        // Real VCS internals: the walk has to prune them, not just produce no
        // file for the empty `.git` marker every test tree carries.
        t.write(".git/HEAD", "ref: refs/heads/main\n");
        t.write(".git/config", "");
        for object in 0..10 {
            t.write(&format!(".git/objects/aa/{object}"), "");
        }
        for vcs in [".hg", ".svn", ".jj"] {
            t.write(&format!("{vcs}/store/data"), "");
        }
        let got = complete_files(&t.0, "", 100);
        assert_eq!(got.first().map(String::as_str), Some("a.txt"));
        for internal in [".git", ".hg", ".svn", ".jj"] {
            assert!(
                !got.iter().any(|p| p.starts_with(internal)),
                "{internal} leaked: {got:?}"
            );
        }
    }

    #[test]
    fn case_insensitive_and_path_substring() {
        let t = Tree::new("case");
        t.write("src/Config.rs", "");
        let got = complete_files(&t.0, "src/config", 10);
        assert_eq!(got, vec!["src/Config.rs".to_string()]);
    }

    #[test]
    fn cap_limits_results() {
        let t = Tree::new("cap");
        for i in 0..20 {
            t.write(&format!("f{i}.txt"), "");
        }
        assert_eq!(complete_files(&t.0, "", 5).len(), 5);
    }
}
