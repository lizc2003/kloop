//! Project-context gathering: instruction-file discovery, environment info,
//! and the opening git snapshot. All the IO lives here; the prompt text is
//! assembled by `kloop_core::context` (pure functions, tested there).

use std::path::Path;
use std::path::PathBuf;

use kloop_core::context::assemble_instructions;
use kloop_core::context::assemble_system;
use kloop_core::context::utc_today;
use kloop_core::context::EnvInfo;
use kloop_core::context::GitInfo;
use kloop_core::context::InstructionFile;
use kloop_core::context::InstructionScope;
use kloop_core::context::BASE_SYSTEM;
use kloop_core::context::INSTRUCTIONS_MAX_BYTES;

/// What Config needs from the project, gathered once per process — server
/// threads share the process cwd, so they share one assembly too.
#[derive(Clone)]
pub struct GatheredContext {
    pub system: String,
    pub instructions: Option<String>,
    /// Truncation/skip notices to surface at startup.
    pub warnings: Vec<String>,
}

pub fn gather(cwd: &Path) -> GatheredContext {
    let global_dir = std::env::home_dir().map(|home| home.join(".kloop"));
    let git_root = find_git_root(cwd);
    let files = discover_instruction_files(cwd, global_dir.as_deref(), git_root.as_deref());
    let assembled = assemble_instructions(&files, INSTRUCTIONS_MAX_BYTES);
    let env = EnvInfo {
        cwd: cwd.display().to_string(),
        platform: std::env::consts::OS.to_string(),
        date: utc_today(),
        is_git_repo: git_root.is_some(),
    };
    let git = if git_root.is_some() {
        git_info(cwd)
    } else {
        None
    };
    GatheredContext {
        system: assemble_system(BASE_SYSTEM, &env, git.as_ref()),
        instructions: assembled.message,
        warnings: assembled.warnings,
    }
}

/// `--mock` stays hermetic: no file reads, no git commands — the same
/// hardcoded prompt as before this seam existed.
pub fn mock(cwd: &Path) -> GatheredContext {
    GatheredContext {
        system: format!("{BASE_SYSTEM} Current working directory: {}", cwd.display()),
        instructions: None,
        warnings: Vec::new(),
    }
}

/// Per directory: AGENTS.md wins, CLAUDE.md is the compatibility fallback.
const INSTRUCTION_FILE_NAMES: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];

/// Global layer first, then the chain of directories from the git root down
/// to cwd — closest to cwd last, where instructions carry the most weight.
/// Missing files are simply absent, never an error.
fn discover_instruction_files(
    cwd: &Path,
    global_dir: Option<&Path>,
    git_root: Option<&Path>,
) -> Vec<InstructionFile> {
    let mut files = Vec::new();
    if let Some(dir) = global_dir {
        files.extend(read_instruction_file(dir, InstructionScope::Global));
    }
    for dir in project_dirs(cwd, git_root) {
        files.extend(read_instruction_file(&dir, InstructionScope::Project));
    }
    files
}

fn read_instruction_file(dir: &Path, scope: InstructionScope) -> Option<InstructionFile> {
    for name in INSTRUCTION_FILE_NAMES {
        let path = dir.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            return Some(InstructionFile {
                path: path.display().to_string(),
                scope,
                content,
            });
        }
    }
    None
}

/// Directories from the git root (inclusive) down to cwd; without a git root
/// only cwd itself is consulted — an unbounded upward walk would slurp
/// unrelated files from home or filesystem root.
fn project_dirs(cwd: &Path, git_root: Option<&Path>) -> Vec<PathBuf> {
    let Some(root) = git_root else {
        return vec![cwd.to_path_buf()];
    };
    let mut chain: Vec<PathBuf> = cwd
        .ancestors()
        .take_while(|dir| *dir != root)
        .map(Path::to_path_buf)
        .collect();
    chain.push(root.to_path_buf());
    chain.reverse();
    chain
}

/// Nearest ancestor (including cwd) containing `.git` — a directory in a
/// normal checkout, a file in a worktree, so only existence is checked.
fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// Opening git snapshot; any failing command (e.g. an empty repo without
/// commits) drops the whole block rather than injecting a partial one.
fn git_info(cwd: &Path) -> Option<GitInfo> {
    let git = |args: &[&str]| -> Option<String> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(cwd)
            .output()
            .ok()?;
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim_end().to_string())
    };
    Some(GitInfo {
        branch: git(&["rev-parse", "--abbrev-ref", "HEAD"])?,
        status: git(&["status", "--short"])?,
        recent_commits: git(&["log", "--oneline", "-n", "5"])?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Unique per-test tree under the OS tempdir.
    fn test_tree(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kloop-ctx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), content).unwrap();
    }

    fn paths(files: &[InstructionFile]) -> Vec<&str> {
        files.iter().map(|f| f.path.as_str()).collect()
    }

    #[test]
    fn agents_md_wins_over_claude_md_in_the_same_directory() {
        let root = test_tree("prefer");
        write(&root, "AGENTS.md", "agents rules");
        write(&root, "CLAUDE.md", "claude rules");
        let files = discover_instruction_files(&root, None, Some(&root));
        assert_eq!(
            paths(&files),
            vec![root.join("AGENTS.md").display().to_string()]
        );
        assert_eq!(files[0].content, "agents rules");
        assert_eq!(files[0].scope, InstructionScope::Project);
    }

    #[test]
    fn claude_md_is_the_fallback_when_agents_md_is_absent() {
        let root = test_tree("fallback");
        write(&root, "CLAUDE.md", "claude rules");
        let files = discover_instruction_files(&root, None, Some(&root));
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "claude rules");
    }

    #[test]
    fn chain_runs_root_to_cwd_and_ignores_dirs_above_the_git_root() {
        let base = test_tree("chain");
        // base/ has a file that must NOT be picked up (above the git root).
        write(&base, "AGENTS.md", "outside");
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "root rules");
        let cwd = root.join("crates").join("core");
        write(&root.join("crates"), "CLAUDE.md", "mid rules");
        std::fs::create_dir_all(&cwd).unwrap();
        write(&cwd, "AGENTS.md", "leaf rules");

        let files = discover_instruction_files(&cwd, None, Some(&root));
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["root rules", "mid rules", "leaf rules"]);
    }

    #[test]
    fn without_a_git_root_only_cwd_is_consulted() {
        let base = test_tree("no-root");
        write(&base, "AGENTS.md", "parent rules");
        let cwd = base.join("sub");
        write(&cwd, "AGENTS.md", "cwd rules");
        let files = discover_instruction_files(&cwd, None, None);
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["cwd rules"]);
    }

    #[test]
    fn global_layer_comes_first() {
        let base = test_tree("global");
        let global = base.join("home-kloop");
        write(&global, "AGENTS.md", "global rules");
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "root rules");

        let files = discover_instruction_files(&root, Some(&global), Some(&root));
        let scoped: Vec<(InstructionScope, &str)> = files
            .iter()
            .map(|f| (f.scope, f.content.as_str()))
            .collect();
        assert_eq!(
            scoped,
            vec![
                (InstructionScope::Global, "global rules"),
                (InstructionScope::Project, "root rules"),
            ]
        );
    }

    #[test]
    fn missing_files_everywhere_is_fine() {
        let root = test_tree("empty");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let files = discover_instruction_files(&root, Some(&root.join("nope")), Some(&root));
        assert!(files.is_empty());
    }

    #[test]
    fn find_git_root_walks_up_from_cwd() {
        let base = test_tree("git-root");
        let root = base.join("repo");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let cwd = root.join("deep").join("dir");
        std::fs::create_dir_all(&cwd).unwrap();
        assert_eq!(find_git_root(&cwd), Some(root));
        assert_eq!(find_git_root(&base.join("elsewhere")), None);
    }
}
