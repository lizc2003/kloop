//! Pure assembly of the system prompt and the project-instructions message.
//! All IO — instruction-file discovery, git commands — is the front-end's
//! job; this module only turns already-gathered inputs into prompt text, so
//! every output shape is testable without a filesystem.

use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use crate::rollout::civil_from_days;

/// Base instructions shared by every assembled system prompt (and used
/// verbatim by `--mock`, which skips assembly to stay hermetic).
pub const BASE_SYSTEM: &str = "You are a coding agent working in a CLI. Use the provided tools \
to inspect and modify files and run commands; keep answers short.";

/// Total byte budget across all instruction files (codex's
/// project_doc_max_bytes default); files beyond it are truncated or skipped
/// with a warning, never an error.
pub const INSTRUCTIONS_MAX_BYTES: usize = 32 * 1024;

/// Byte cap on the `git status --short` snapshot inside the system prompt.
pub const GIT_STATUS_MAX_BYTES: usize = 1000;

pub struct EnvInfo {
    pub cwd: String,
    /// `std::env::consts::OS` at the front end ("macos", "linux", …).
    pub platform: String,
    /// UTC calendar date, `YYYY-MM-DD`.
    pub date: String,
    pub is_git_repo: bool,
}

/// Opening git snapshot; collected once at startup, so the prompt says so.
pub struct GitInfo {
    pub branch: String,
    /// `git status --short` output; empty means a clean tree.
    pub status: String,
    /// `git log --oneline -n 5` output.
    pub recent_commits: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InstructionScope {
    /// `~/.kloop/` — applies to every project.
    Global,
    /// Found between the git root and cwd.
    Project,
}

pub struct InstructionFile {
    /// Display path, used to label the file's origin in the prompt.
    pub path: String,
    pub scope: InstructionScope,
    pub content: String,
}

pub struct AssembledInstructions {
    /// The full `<project-instructions>` user message; None when no
    /// instruction file had content.
    pub message: Option<String>,
    /// Human-readable truncation/skip notices for the UI, not the model.
    pub warnings: Vec<String>,
}

/// System prompt = base instructions + environment block + optional git
/// snapshot. Project instructions deliberately do NOT go here — they ride as
/// a synthetic user message (see `Config::project_instructions`).
pub fn assemble_system(base: &str, env: &EnvInfo, git: Option<&GitInfo>) -> String {
    let mut out = format!(
        "{base}\n\n# Environment\n\
         - Working directory: {cwd}\n\
         - Platform: {platform}\n\
         - Today's date: {date}\n\
         - Is a git repository: {git_repo}",
        cwd = env.cwd,
        platform = env.platform,
        date = env.date,
        git_repo = env.is_git_repo,
    );
    if let Some(git) = git {
        let status = match truncate_at_char_boundary(&git.status, GIT_STATUS_MAX_BYTES) {
            s if s.len() < git.status.len() => format!("{s}\n…[status truncated]"),
            "" => "(clean)".to_string(),
            s => s.to_string(),
        };
        out.push_str(&format!(
            "\n\nThis is the git state at the start of the session; it is a snapshot and will \
             not update.\nCurrent branch: {branch}\nStatus:\n{status}\nRecent commits:\n{log}",
            branch = git.branch,
            log = git.recent_commits,
        ));
    }
    out
}

/// Concatenate instruction files under one total byte budget (applied to the
/// contents, not the framing). Files come ordered global → git root → cwd;
/// later files land closer to the end of the message, where instructions
/// carry the most weight against earlier ones.
pub fn assemble_instructions(files: &[InstructionFile], max_bytes: usize) -> AssembledInstructions {
    let mut warnings = Vec::new();
    let mut entries = Vec::new();
    let mut remaining = max_bytes;
    for file in files {
        let content = file.content.trim();
        if content.is_empty() {
            continue;
        }
        if remaining == 0 {
            warnings.push(format!(
                "instructions budget ({max_bytes} bytes) exhausted; skipped {}",
                file.path
            ));
            continue;
        }
        let kept = truncate_at_char_boundary(content, remaining);
        if kept.len() < content.len() {
            warnings.push(format!(
                "instructions budget ({max_bytes} bytes) exceeded; truncated {}",
                file.path
            ));
        }
        remaining -= kept.len();
        let label = match file.scope {
            InstructionScope::Global => "global instructions for all projects",
            InstructionScope::Project => "project instructions",
        };
        entries.push(format!("Contents of {} ({label}):\n\n{kept}", file.path));
    }
    if entries.is_empty() {
        return AssembledInstructions {
            message: None,
            warnings,
        };
    }
    let message = format!(
        "<project-instructions>\nProject instruction files are shown below. Adhere to them; \
         they override default behavior.\n\n{}\n</project-instructions>",
        entries.join("\n\n"),
    );
    AssembledInstructions {
        message: Some(message),
        warnings,
    }
}

/// Largest prefix of `s` that fits `max` bytes without splitting a char.
fn truncate_at_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// UTC calendar date for a unix timestamp, `YYYY-MM-DD`.
pub fn utc_date(unix_secs: u64) -> String {
    let (y, m, d) = civil_from_days((unix_secs / 86_400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

pub fn utc_today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    utc_date(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env() -> EnvInfo {
        EnvInfo {
            cwd: "/work/demo".into(),
            platform: "macos".into(),
            date: "2026-07-10".into(),
            is_git_repo: true,
        }
    }

    #[test]
    fn system_without_git_is_base_plus_environment() {
        let env = EnvInfo {
            is_git_repo: false,
            ..env()
        };
        assert_eq!(
            assemble_system("Base.", &env, None),
            "Base.\n\n# Environment\n\
             - Working directory: /work/demo\n\
             - Platform: macos\n\
             - Today's date: 2026-07-10\n\
             - Is a git repository: false"
        );
    }

    #[test]
    fn system_with_git_appends_the_snapshot() {
        let git = GitInfo {
            branch: "main".into(),
            status: " M src/lib.rs".into(),
            recent_commits: "abc1234 fix things".into(),
        };
        assert_eq!(
            assemble_system("Base.", &env(), Some(&git)),
            "Base.\n\n# Environment\n\
             - Working directory: /work/demo\n\
             - Platform: macos\n\
             - Today's date: 2026-07-10\n\
             - Is a git repository: true\n\n\
             This is the git state at the start of the session; it is a snapshot and will not update.\n\
             Current branch: main\n\
             Status:\n M src/lib.rs\n\
             Recent commits:\nabc1234 fix things"
        );
    }

    #[test]
    fn empty_git_status_reads_clean() {
        let git = GitInfo {
            branch: "main".into(),
            status: String::new(),
            recent_commits: "abc1234 first".into(),
        };
        let system = assemble_system("Base.", &env(), Some(&git));
        assert!(system.contains("Status:\n(clean)\n"), "got: {system}");
    }

    #[test]
    fn oversized_git_status_is_truncated_on_a_char_boundary() {
        let git = GitInfo {
            branch: "main".into(),
            // Multibyte content straddling the cap must not split a char.
            status: "文".repeat(GIT_STATUS_MAX_BYTES),
            recent_commits: "abc1234 first".into(),
        };
        let system = assemble_system("Base.", &env(), Some(&git));
        assert!(system.contains("…[status truncated]"), "got: {system}");
        // 999 is not a multiple of the 3-byte char, so the boundary backs off.
        assert!(system.contains(&"文".repeat(333)));
        assert!(!system.contains(&"文".repeat(334)));
    }

    fn file(path: &str, scope: InstructionScope, content: &str) -> InstructionFile {
        InstructionFile {
            path: path.into(),
            scope,
            content: content.into(),
        }
    }

    #[test]
    fn no_files_means_no_message() {
        let assembled = assemble_instructions(&[], INSTRUCTIONS_MAX_BYTES);
        assert!(assembled.message.is_none());
        assert!(assembled.warnings.is_empty());
    }

    #[test]
    fn whitespace_only_files_are_dropped() {
        let files = [file("/repo/AGENTS.md", InstructionScope::Project, "  \n\t")];
        let assembled = assemble_instructions(&files, INSTRUCTIONS_MAX_BYTES);
        assert!(assembled.message.is_none());
        assert!(assembled.warnings.is_empty());
    }

    #[test]
    fn files_are_labeled_by_scope_in_order() {
        let files = [
            file(
                "/home/u/.kloop/AGENTS.md",
                InstructionScope::Global,
                "be kind",
            ),
            file("/repo/AGENTS.md", InstructionScope::Project, "run tests"),
        ];
        let assembled = assemble_instructions(&files, INSTRUCTIONS_MAX_BYTES);
        assert_eq!(
            assembled.message.as_deref(),
            Some(
                "<project-instructions>\nProject instruction files are shown below. Adhere to \
                 them; they override default behavior.\n\n\
                 Contents of /home/u/.kloop/AGENTS.md (global instructions for all projects):\n\n\
                 be kind\n\n\
                 Contents of /repo/AGENTS.md (project instructions):\n\n\
                 run tests\n\
                 </project-instructions>"
            )
        );
        assert!(assembled.warnings.is_empty());
    }

    #[test]
    fn budget_truncates_the_overflowing_file_and_skips_the_rest() {
        let files = [
            file("/repo/AGENTS.md", InstructionScope::Project, "abcdefgh"),
            file("/repo/sub/AGENTS.md", InstructionScope::Project, "ijkl"),
        ];
        let assembled = assemble_instructions(&files, 4);
        let message = assembled.message.unwrap();
        assert!(message.contains("abcd"), "got: {message}");
        assert!(!message.contains("abcde"));
        assert!(!message.contains("ijkl"));
        assert_eq!(
            assembled.warnings,
            vec![
                "instructions budget (4 bytes) exceeded; truncated /repo/AGENTS.md",
                "instructions budget (4 bytes) exhausted; skipped /repo/sub/AGENTS.md",
            ]
        );
    }

    #[test]
    fn exact_budget_fit_carries_no_warning() {
        let files = [file("/repo/AGENTS.md", InstructionScope::Project, "abcd")];
        let assembled = assemble_instructions(&files, 4);
        assert!(assembled.message.unwrap().contains("abcd"));
        assert!(assembled.warnings.is_empty());
    }

    #[test]
    fn utc_date_matches_known_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_782_864_000), "2026-07-01");
    }
}
