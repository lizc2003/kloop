//! Project-context gathering: instruction-file discovery, environment info,
//! and the opening git snapshot. All the IO lives here; the prompt text is
//! assembled by `kloop_core::context` (pure functions, tested there).

use std::collections::HashSet;
use std::path::Component;
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
    let Discovered {
        files,
        mut warnings,
    } = discover_instruction_files(cwd, global_dir.as_deref(), git_root.as_deref());
    let assembled = assemble_instructions(&files, INSTRUCTIONS_MAX_BYTES);
    warnings.extend(assembled.warnings);
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
        warnings,
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
/// Private, gitignored override files; AGENTS.local.md wins over CLAUDE.local.md.
const LOCAL_INSTRUCTION_FILE_NAMES: [&str; 2] = ["AGENTS.local.md", "CLAUDE.local.md"];
/// Modular rule fragments, loaded per directory (`<dir>/.kloop/rules/*.md`).
const RULES_DIR: [&str; 2] = [".kloop", "rules"];
/// Cap on `@import` recursion; matches cc's MAX_INCLUDE_DEPTH. A file at this
/// depth is skipped, so a chain loads depths 0..MAX (MAX files deep).
const MAX_INCLUDE_DEPTH: usize = 5;

/// Result of instruction-file discovery: the ordered files plus human-readable
/// notices (missing/skipped imports) to surface at startup.
pub struct Discovered {
    pub files: Vec<InstructionFile>,
    pub warnings: Vec<String>,
}

/// Global layer first, then the chain of directories from the git root down
/// to cwd — closest to cwd last, where instructions carry the most weight.
/// Within each project directory the order is: main file (AGENTS/CLAUDE.md),
/// then `.kloop/rules/*.md`, then the local override — local last so it wins.
/// Every file's `@import` references are expanded in place. Missing top-level
/// files are simply absent; a missing *imported* file is a warning, not an
/// error.
fn discover_instruction_files(
    cwd: &Path,
    global_dir: Option<&Path>,
    git_root: Option<&Path>,
) -> Discovered {
    // External-import boundary: project/local files may only import from within
    // the git root (or cwd without one). Global files import from anywhere.
    let boundary_src = git_root.unwrap_or(cwd);
    let boundary =
        std::fs::canonicalize(boundary_src).unwrap_or_else(|_| boundary_src.to_path_buf());
    let mut d = Discovery {
        files: Vec::new(),
        warnings: Vec::new(),
        processed: HashSet::new(),
        boundary,
        home: std::env::home_dir(),
    };
    if let Some(dir) = global_dir {
        d.add_main_file(dir, InstructionScope::Global);
    }
    for dir in project_dirs(cwd, git_root) {
        d.add_main_file(&dir, InstructionScope::Project);
        d.add_rules_dir(&dir);
        d.add_local_file(&dir);
    }
    Discovered {
        files: d.files,
        warnings: d.warnings,
    }
}

/// Discovery state threaded through the directory walk and `@import` recursion:
/// `processed` dedups files (and breaks import cycles) across the whole walk.
struct Discovery {
    files: Vec<InstructionFile>,
    warnings: Vec<String>,
    /// Canonical paths already loaded — shared by top-level files and imports.
    processed: HashSet<PathBuf>,
    /// Canonical git root (or cwd); imports outside it are external.
    boundary: PathBuf,
    home: Option<PathBuf>,
}

impl Discovery {
    /// Main instruction file for a directory (AGENTS.md preferred), expanded.
    fn add_main_file(&mut self, dir: &Path, scope: InstructionScope) {
        for name in INSTRUCTION_FILE_NAMES {
            if self.expand(&dir.join(name), scope, 0, false) {
                return; // AGENTS.md wins; don't also load CLAUDE.md
            }
        }
    }

    /// Private override for a directory (AGENTS.local.md preferred), expanded.
    fn add_local_file(&mut self, dir: &Path) {
        for name in LOCAL_INSTRUCTION_FILE_NAMES {
            if self.expand(&dir.join(name), InstructionScope::Local, 0, false) {
                return;
            }
        }
    }

    /// Every `*.md` in `<dir>/.kloop/rules/`, sorted for a stable order.
    fn add_rules_dir(&mut self, dir: &Path) {
        let rules = RULES_DIR
            .iter()
            .fold(dir.to_path_buf(), |p, seg| p.join(seg));
        let Ok(entries) = std::fs::read_dir(&rules) else {
            return; // no rules dir is the norm
        };
        let mut md: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e == "md"))
            .collect();
        md.sort();
        for path in md {
            self.expand(&path, InstructionScope::Project, 0, false);
        }
    }

    /// Read `path`, push it, and recursively expand its `@import` references.
    /// Returns whether a non-empty file was loaded. `warn_missing` is set only
    /// for imports that look file-like (so a missing top-level file, or a prose
    /// `@mention`, stays silent, while a typo'd `@./file.md` warns).
    fn expand(
        &mut self,
        path: &Path,
        scope: InstructionScope,
        depth: usize,
        warn_missing: bool,
    ) -> bool {
        if depth >= MAX_INCLUDE_DEPTH {
            return false;
        }
        // Canonicalize to resolve symlinks and give a stable dedup key; failure
        // means the file is absent (or unreadable).
        let canon = match std::fs::canonicalize(path) {
            Ok(c) => c,
            Err(_) => {
                if warn_missing {
                    self.warnings.push(format!(
                        "instruction import not found, skipped: {}",
                        path.display()
                    ));
                }
                return false;
            }
        };
        if !self.processed.insert(canon.clone()) {
            return false; // already loaded, or an import cycle
        }
        let Ok(content) = std::fs::read_to_string(&canon) else {
            return false;
        };
        if content.trim().is_empty() {
            return false;
        }
        self.files.push(InstructionFile {
            path: path.display().to_string(),
            scope,
            content: content.clone(),
        });
        // Imports resolve relative to the (canonical) directory of this file, so
        // their content lands after this file's — an import refines its parent.
        let base = canon.parent().unwrap_or(&canon).to_path_buf();
        for spec in extract_imports(&content) {
            let Some(resolved) = self.resolve_import(&spec, &base) else {
                continue;
            };
            if scope != InstructionScope::Global && self.is_external(&resolved) {
                self.warnings.push(format!(
                    "instruction import outside the project, skipped: {}",
                    resolved.display()
                ));
                continue;
            }
            // A spec with a path separator or extension is a real file
            // reference; a bare word is likely prose — don't warn if it misses.
            let warn = spec.contains('/') || spec.contains('.');
            self.expand(&resolved, scope, depth + 1, warn);
        }
        true
    }

    /// Resolve an `@import` spec against the importing file's directory.
    /// Supports `@./rel`, `@../rel`, `@~/home`, `@/abs`, and bare `@rel`.
    fn resolve_import(&self, spec: &str, base: &Path) -> Option<PathBuf> {
        let raw = if let Some(rest) = spec.strip_prefix("~/") {
            self.home.as_ref()?.join(rest)
        } else if spec.starts_with('/') {
            PathBuf::from(spec)
        } else {
            base.join(spec)
        };
        Some(lexical_normalize(&raw))
    }

    /// Whether a resolved import lands outside the project boundary.
    fn is_external(&self, resolved: &Path) -> bool {
        !resolved.starts_with(&self.boundary)
    }
}

/// Resolve `.`/`..` components without touching the filesystem (the target may
/// not exist yet when we test whether an import is external).
fn lexical_normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Collect `@import` targets from instruction text. A token counts only when
/// `@` sits at a line start or after whitespace (so `a@b.com` is not an
/// import) and points at a plausible path; fenced code blocks are skipped.
fn extract_imports(content: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut fence: Option<&str> = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        if let Some(marker) = fence {
            if trimmed.starts_with(marker) {
                fence = None;
            }
            continue;
        }
        if trimmed.starts_with("```") {
            fence = Some("```");
            continue;
        }
        if trimmed.starts_with("~~~") {
            fence = Some("~~~");
            continue;
        }
        // split_whitespace tokens starting with '@' are exactly the ones with
        // '@' at a line start or after whitespace.
        for token in line.split_whitespace() {
            if let Some(rest) = token.strip_prefix('@') {
                let spec = rest.split('#').next().unwrap_or(rest); // drop #fragment
                if is_valid_import_spec(spec) {
                    out.push(spec.to_string());
                }
            }
        }
    }
    out
}

/// Mirror cc's acceptance test for an `@import` path: explicit `./`, `../`,
/// `~/`, `/` prefixes, or a bare name starting with an alnum/`.`/`_`/`-`.
fn is_valid_import_spec(spec: &str) -> bool {
    if spec.is_empty() || spec == "/" {
        return false;
    }
    if spec.starts_with("./") || spec.starts_with("../") || spec.starts_with("~/") {
        return true;
    }
    if spec.starts_with('/') {
        return true;
    }
    let first = spec.chars().next().unwrap();
    first.is_ascii_alphanumeric() || first == '.' || first == '_' || first == '-'
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
        let files = discover_instruction_files(&root, None, Some(&root)).files;
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
        let files = discover_instruction_files(&root, None, Some(&root)).files;
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

        let files = discover_instruction_files(&cwd, None, Some(&root)).files;
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["root rules", "mid rules", "leaf rules"]);
    }

    #[test]
    fn without_a_git_root_only_cwd_is_consulted() {
        let base = test_tree("no-root");
        write(&base, "AGENTS.md", "parent rules");
        let cwd = base.join("sub");
        write(&cwd, "AGENTS.md", "cwd rules");
        let files = discover_instruction_files(&cwd, None, None).files;
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

        let files = discover_instruction_files(&root, Some(&global), Some(&root)).files;
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
        let files = discover_instruction_files(&root, Some(&root.join("nope")), Some(&root)).files;
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

    #[test]
    fn import_expands_with_the_main_file_first() {
        let root = test_tree("import-basic");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "main rules\n@./extra.md");
        write(&root, "extra.md", "extra rules");
        let files = discover_instruction_files(&root, None, Some(&root)).files;
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        // Parent before child, matching cc's processMemoryFile order.
        assert_eq!(contents, vec!["main rules\n@./extra.md", "extra rules"]);
        assert_eq!(files[1].scope, InstructionScope::Project);
    }

    #[test]
    fn import_depth_is_capped_at_five() {
        let root = test_tree("import-depth");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "level0\n@./a1.md");
        write(&root, "a1.md", "level1\n@./a2.md");
        write(&root, "a2.md", "level2\n@./a3.md");
        write(&root, "a3.md", "level3\n@./a4.md");
        write(&root, "a4.md", "level4\n@./a5.md");
        write(&root, "a5.md", "level5");
        let files = discover_instruction_files(&root, None, Some(&root)).files;
        let joined: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        // Depths 0..=4 load; a5.md at depth 5 is dropped.
        assert!(joined.iter().any(|c| c.contains("level4")));
        assert!(!joined.iter().any(|c| c.contains("level5")));
    }

    #[test]
    fn import_cycle_terminates_without_duplicating() {
        let root = test_tree("import-cycle");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "a\n@./b.md");
        write(&root, "b.md", "b\n@./AGENTS.md");
        let files = discover_instruction_files(&root, None, Some(&root)).files;
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        // Each file loaded once; the cycle back to AGENTS.md is a no-op.
        assert_eq!(contents, vec!["a\n@./b.md", "b\n@./AGENTS.md"]);
    }

    #[test]
    fn external_imports_are_skipped_for_project_but_allowed_for_global() {
        let base = test_tree("import-external");
        let outside = base.join("outside");
        write(&outside, "shared.md", "shared");
        let repo = base.join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        write(
            &repo,
            "AGENTS.md",
            &format!("proj\n@{}/shared.md", outside.display()),
        );
        let d = discover_instruction_files(&repo, None, Some(&repo));
        assert!(!d.files.iter().any(|f| f.content == "shared"));
        assert!(d.warnings.iter().any(|w| w.contains("outside the project")));

        // A global file may import from anywhere.
        let global = base.join("home-kloop");
        write(
            &global,
            "AGENTS.md",
            &format!("glob\n@{}/shared.md", outside.display()),
        );
        let d2 = discover_instruction_files(&repo, Some(&global), Some(&repo));
        assert!(d2.files.iter().any(|f| f.content == "shared"));
    }

    #[test]
    fn missing_import_warns_but_does_not_crash() {
        let root = test_tree("import-missing");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "main\n@./nope.md");
        let d = discover_instruction_files(&root, None, Some(&root));
        assert_eq!(d.files.len(), 1);
        assert!(d
            .warnings
            .iter()
            .any(|w| w.contains("not found") && w.contains("nope.md")));
    }

    #[test]
    fn prose_at_mention_does_not_warn() {
        let root = test_tree("import-prose");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "ping @someone before merging");
        let d = discover_instruction_files(&root, None, Some(&root));
        assert_eq!(d.files.len(), 1);
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn imports_inside_fenced_code_blocks_are_ignored() {
        let root = test_tree("import-fence");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "main\n```\n@./secret.md\n```\n");
        write(&root, "secret.md", "secret");
        let d = discover_instruction_files(&root, None, Some(&root));
        assert!(!d.files.iter().any(|f| f.content == "secret"));
        assert!(d.warnings.is_empty());
    }

    #[test]
    fn rules_dir_files_are_loaded_sorted_after_the_main_file() {
        let root = test_tree("rules");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "main");
        let rules = root.join(".kloop").join("rules");
        write(&rules, "20-style.md", "style");
        write(&rules, "10-testing.md", "testing");
        write(&rules, "notes.txt", "ignored");
        let files = discover_instruction_files(&root, None, Some(&root)).files;
        let contents: Vec<&str> = files.iter().map(|f| f.content.as_str()).collect();
        assert_eq!(contents, vec!["main", "testing", "style"]);
    }

    #[test]
    fn local_override_is_loaded_last_and_scoped_local() {
        let root = test_tree("local");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.md", "shared");
        write(&root, "AGENTS.local.md", "private");
        let files = discover_instruction_files(&root, None, Some(&root)).files;
        let scoped: Vec<(InstructionScope, &str)> = files
            .iter()
            .map(|f| (f.scope, f.content.as_str()))
            .collect();
        assert_eq!(
            scoped,
            vec![
                (InstructionScope::Project, "shared"),
                (InstructionScope::Local, "private"),
            ]
        );
    }

    #[test]
    fn agents_local_wins_over_claude_local() {
        let root = test_tree("local-prefer");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        write(&root, "AGENTS.local.md", "agents local");
        write(&root, "CLAUDE.local.md", "claude local");
        let locals: Vec<String> = discover_instruction_files(&root, None, Some(&root))
            .files
            .into_iter()
            .filter(|f| f.scope == InstructionScope::Local)
            .map(|f| f.content)
            .collect();
        assert_eq!(locals, vec!["agents local".to_string()]);
    }
}
