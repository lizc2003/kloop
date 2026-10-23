//! 调用的 shell、路径、敏感读取与危险特征；不依赖权限管线。

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::sandbox::ExecutionContext;
use crate::shell::BashAnalysis;
use crate::shell::argv_is_dangerous;
use crate::shell::strip_wrappers;
use crate::shell::{BashFacts, CommandFact, Direction, WordValue, analyze_bash_facts};
use crate::tools::Builtin;

use super::argv_roles;

pub(super) struct Hazard {
    pub(super) tag: &'static str,
}

pub(super) enum ShellFacts {
    Bash(BashAnalysis),
    PowerShellOpaque,
}

/// The decomposition one tool call is judged on, gathered once per call. Two
/// built-ins answer [`Builtin::readonly`] from the call rather than the name,
/// so exactly those two facts are visible to the crate; the rest belong to this
/// module's gate.
pub(crate) struct CallFacts {
    pub(super) shell: Option<ShellFacts>,
    pub(super) writes: Vec<PathFacts>,
    pub(super) path: Option<PathFacts>,
    pub(super) path_key: Option<&'static str>,
    pub(super) sensitive_read: bool,
    pub(super) risk_commands: Vec<CommandFact>,
    pub(super) risk_writes: Vec<PathFacts>,
    unresolved_sensitive_write: bool,
    powershell_sensitive: bool,
    entering_existing_worktree: bool,
    pub(crate) removing_worktree: bool,
    /// Present only when the bash analysis gave up: with no argv to key on, the
    /// script's own text is what rules and session memories are written about.
    pub(super) opaque_script: Option<OpaqueScript>,
}

/// An unparseable bash script kept whole. `no_sandbox` is the call's
/// `disable_sandbox` flag, carried alongside the text because "may run" and
/// "may run outside the sandbox" are different consents about the same words.
pub(super) struct OpaqueScript {
    pub(super) command: String,
    pub(super) no_sandbox: bool,
}

/// 敏感路径的类别，决定元数据访问是否算读取。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Sensitivity {
    /// 仓库内部（`.git`）：元数据访问不是读取，内容访问仍是硬边界。
    Vcs,
    /// 凭据或 agent state：元数据也按读取处理。
    Secret,
}

#[derive(Clone)]
pub(super) struct PathFacts {
    /// Canonical existing ancestor plus any not-yet-created suffix.
    pub(super) normalized: PathBuf,
    /// Resolved path relative to the canonical working directory.
    pub(super) relative: Option<PathBuf>,
    /// Original lexical spelling, retained so explicit path rules keep matching
    /// aliases in addition to the resolved target.
    pub(super) original: PathBuf,
    pub(super) original_relative: Option<PathBuf>,
    pub(super) inside_cwd: bool,
    pub(super) sensitive: bool,
    pub(super) sensitivity: Option<Sensitivity>,
}

impl CallFacts {
    #[cfg(test)]
    pub(super) fn gather(
        name: &str,
        input: &Value,
        cwd: &Path,
        resolved_path: Option<&Path>,
    ) -> Self {
        Self::gather_with_execution(name, input, cwd, resolved_path, ExecutionContext::default())
    }

    pub(super) fn gather_with_execution(
        name: &str,
        input: &Value,
        cwd: &Path,
        resolved_path: Option<&Path>,
        execution: ExecutionContext<'_>,
    ) -> Self {
        let shell_command = matches!(name, "bash" | "powershell")
            .then(|| input["command"].as_str())
            .flatten();
        let bash_facts = (name == "bash")
            .then(|| shell_command.map(analyze_bash_facts))
            .flatten();
        let shell = match (name, shell_command) {
            ("bash", Some(_)) => {
                let analysis = bash_facts.as_ref().unwrap().analysis.clone();
                // Git Bash 的 POSIX 路径不能按原生 Windows cwd 规范化；未建模映射前不生成文件授权。
                let analysis = if cfg!(windows) && !analysis.writes().is_empty() {
                    BashAnalysis::Opaque
                } else {
                    analysis
                };
                Some(ShellFacts::Bash(analysis))
            }
            ("powershell", Some(_)) => Some(ShellFacts::PowerShellOpaque),
            _ => None,
        };
        let writes = shell
            .as_ref()
            .and_then(|shell| match shell {
                ShellFacts::Bash(analysis) => Some(analysis.writes()),
                ShellFacts::PowerShellOpaque => None,
            })
            .unwrap_or_default()
            .iter()
            .map(|path| PathFacts::gather(Path::new(path), cwd))
            .collect();
        let path_key = match name {
            "write_file" | "edit_file" | "read_file" => Some("path"),
            "notebook_edit" => Some("notebook_path"),
            _ => None,
        };
        let path = path_key
            .and_then(|key| input[key].as_str())
            .map(|raw| PathFacts::gather_with_resolved(Path::new(raw), cwd, resolved_path));
        let sensitive_read = (name == "read_file"
            && path.as_ref().is_some_and(|path| path.sensitive))
            || (name == "bash"
                && shell_command
                    .zip(shell.as_ref())
                    .is_some_and(|(command, facts)| {
                        let ShellFacts::Bash(_) = facts else {
                            return false;
                        };
                        bash_reads_sensitive_path(
                            command,
                            bash_facts.as_ref().unwrap(),
                            cwd,
                            execution,
                        )
                        .blocked()
                    }));
        let powershell_sensitive =
            name == "powershell" && shell_command.is_some_and(powershell_mentions_sensitive_path);
        let opaque_script = match (&shell, shell_command) {
            (Some(ShellFacts::Bash(BashAnalysis::Opaque)), Some(command)) => {
                let command = command.trim();
                (!command.is_empty()).then(|| OpaqueScript {
                    command: command.to_string(),
                    no_sandbox: input["disable_sandbox"].as_bool().unwrap_or(false),
                })
            }
            _ => None,
        };
        CallFacts {
            shell,
            writes,
            path,
            path_key,
            sensitive_read,
            risk_commands: bash_facts
                .as_ref()
                .map_or_else(Vec::new, |facts| facts.commands.clone()),
            unresolved_sensitive_write: bash_facts.as_ref().is_some_and(|facts| facts.redirects.iter().any(|redirect| {
                redirect.direction == Direction::Write && redirect.context.cwd_unknown
                    && matches!(&redirect.target, WordValue::Literal(path) if !Path::new(path).is_absolute() && path_sensitivity(Path::new(path)).is_some())
            })),
            risk_writes: bash_facts.as_ref().map_or_else(Vec::new, |facts| {
                facts
                    .redirects
                    .iter()
                    .filter_map(|redirect| {
                        if redirect.direction != Direction::Write {
                            return None;
                        }
                        let WordValue::Literal(path) = &redirect.target else {
                            return None;
                        };
                        if path == "/dev/null" { return None; }
                        (!redirect.context.cwd_unknown || Path::new(path).is_absolute())
                            .then(|| PathFacts::gather(Path::new(path), cwd))
                    })
                    .collect()
            }),
            powershell_sensitive,
            entering_existing_worktree: name == "enter_worktree"
                && input.get("path").is_some_and(Value::is_string),
            removing_worktree: name == "exit_worktree"
                && input["action"].as_str() == Some("remove"),
            opaque_script,
        }
    }

    pub(super) fn hazard(&self, name: &str) -> Option<Hazard> {
        if self.entering_existing_worktree {
            return Some(Hazard {
                tag: "existing worktree",
            });
        }
        if self.removing_worktree {
            return Some(Hazard { tag: "destructive" });
        }
        if self.powershell_sensitive {
            return Some(Hazard {
                tag: "sensitive PowerShell path",
            });
        }
        if let Some(cmds) = self.shell.as_ref().and_then(|shell| match shell {
            ShellFacts::Bash(analysis) => analysis.commands(),
            ShellFacts::PowerShellOpaque => None,
        }) && cmds
            .iter()
            .any(|argv| argv_is_dangerous(argv) || argv_is_dangerous(&strip_wrappers(argv)))
        {
            return Some(Hazard { tag: "destructive" });
        }
        if self.risk_commands.iter().any(|command| {
            // 未知词用不可执行占位符保留位置；只用于 blocklist，不交给角色表或授权。
            let argv: Vec<_> = command
                .words
                .iter()
                .map(|word| match &word.value {
                    WordValue::Literal(value) => value.clone(),
                    WordValue::Unknown => "\0unknown".into(),
                })
                .collect();
            argv_is_dangerous(&argv) || argv_is_dangerous(&strip_wrappers(&argv))
        }) {
            return Some(Hazard { tag: "destructive" });
        }
        if self.unresolved_sensitive_write
            || self
                .risk_writes
                .iter()
                .any(|path| path.sensitive || path.normalized.starts_with("/dev"))
        {
            return Some(Hazard {
                tag: "sensitive redirect path",
            });
        }
        if matches!(name, "write_file" | "edit_file" | "notebook_edit")
            && self.path.as_ref().is_some_and(|p| p.sensitive)
        {
            return Some(Hazard {
                tag: "sensitive path",
            });
        }
        None
    }

    /// Whether this shell call decomposes into nothing but known read-only
    /// commands. Safe-to-run and safe-to-parallelize are two verdicts over this
    /// one decomposition, so `Builtin::concurrency_safe` asks the same question
    /// of its own copy of the analysis.
    pub(crate) fn bash_is_readonly(&self) -> bool {
        matches!(&self.shell, Some(ShellFacts::Bash(analysis)) if analysis.is_readonly())
    }

    /// Read-only in the permission sense: nothing on the user's system to sign
    /// off on. Built-ins answer for themselves ([`Builtin::readonly`]) — the
    /// verdict is one of the facts a built-in owns, not a second table here.
    pub(super) fn is_readonly(&self, name: &str) -> bool {
        if let Some(builtin) = Builtin::from_name(name) {
            return builtin.readonly(self);
        }
        // Listing only discloses the catalog a configured MCP server
        // advertises. Reading a model-selected URI still requires the normal
        // external-tool approval; read-only here would bypass it. Every other
        // source tool is judged not read-only: its source's own claim governs
        // batching, never whether the user is asked.
        name == "list_mcp_resources"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadVerdict {
    Clear,
    Sensitive,
    Unresolved,
    RequiresProtection,
}

impl ReadVerdict {
    fn blocked(self) -> bool {
        matches!(self, Self::Sensitive | Self::Unresolved)
    }
}

fn argv_reads_sensitive_path(argv: &[String], cwd: &Path) -> bool {
    let paths = argv_roles::path_arguments(argv);
    // 已建模命令解析失败时角色不明，按内容访问处理；未建模命令（`ls`、
    // `stat`）没有角色表可失败，元数据判定直接生效。
    let metadata = argv_roles::is_metadata_access(argv)
        && (paths.is_some() || !argv_roles::is_modeled_command(argv));
    let sensitive = match paths {
        // 识别出参数角色：只检查命令真正会打开的值。
        Some(paths) => paths
            .into_iter()
            .any(|arg| path_argument_is_sensitive(arg, cwd, metadata)),
        // 未知命令或语法：退回逐参数检查。
        None => argv.iter().skip(1).any(|arg| {
            if arg.starts_with('-') && !arg.contains('/') {
                return false;
            }
            path_argument_is_sensitive(arg, cwd, metadata)
        }),
    };
    sensitive || recursive_search_covers_sensitive_path(argv, cwd)
}

fn bash_reads_sensitive_path(
    command: &str,
    facts: &BashFacts,
    cwd: &Path,
    execution: ExecutionContext<'_>,
) -> ReadVerdict {
    let raw_sensitive = raw_mentions_sensitive_path(command);
    let mut masked = command.to_string();
    let mut requires_protection = false;
    if raw_sensitive && facts.reliable {
        for input in &facts.literal_inputs {
            if !input.eligible {
                continue;
            }
            let body = &command[input.body.clone()];
            if raw_mentions_sensitive_path(body) && protected_body(body, execution) {
                // 只替换正文的字节，保留所有正文外命令与同一行的兄弟节点。
                masked.replace_range(input.body.clone(), &" ".repeat(body.len()));
                requires_protection = true;
            }
        }
    }
    if raw_mentions_sensitive_path(&masked) {
        return ReadVerdict::Unresolved;
    }
    if let Some(commands) = facts.analysis.commands()
        && commands
            .iter()
            .any(|argv| argv_reads_sensitive_path(argv, cwd))
    {
        return ReadVerdict::Sensitive;
    }
    {
        for command in &facts.commands {
            if let Some(argv) = command.argv() {
                if argv_reads_sensitive_path(&argv, cwd) { return ReadVerdict::Sensitive; }
            } else if command.words.iter().skip(1).any(|word| {
                matches!(&word.value, WordValue::Literal(value) if path_argument_is_sensitive(value, cwd, false))
            }) { return ReadVerdict::Sensitive; }
        }
        for redirect in &facts.redirects {
            if redirect.direction == Direction::Read
                && matches!(&redirect.target, WordValue::Literal(path) if path_argument_is_sensitive(path, cwd, false))
            {
                return ReadVerdict::Sensitive;
            }
        }
    }
    if requires_protection {
        ReadVerdict::RequiresProtection
    } else {
        ReadVerdict::Clear
    }
}

fn protected_body(body: &str, execution: ExecutionContext<'_>) -> bool {
    // 只识别路径样本的词法边界，不解析 Rust/Python；每处 needle 都须落在受保护路径中。
    body.split(|ch: char| ch.is_whitespace() || "\"'`(){}[];,|<>=&".contains(ch))
        .filter(|token| raw_mentions_sensitive_path(token))
        .all(|token| {
            !token.contains(['\\', '$', '*', '?']) && execution.protected_literal_path(token)
        })
}

/// 每个实际启动入口都复核；只有依赖本轮正文例外的调用改变执行契约。
pub(crate) fn bash_protection_requirement(
    command: &str,
    cwd: &Path,
    execution: ExecutionContext<'_>,
) -> Result<bool, String> {
    let facts = analyze_bash_facts(command);
    if !facts
        .literal_inputs
        .iter()
        .any(|input| input.eligible && raw_mentions_sensitive_path(&command[input.body.clone()]))
    {
        return Ok(false);
    }
    match bash_reads_sensitive_path(command, &facts, cwd, execution) {
        ReadVerdict::RequiresProtection => Ok(true),
        ReadVerdict::Clear => Ok(false),
        ReadVerdict::Sensitive | ReadVerdict::Unresolved => Err("bash: this literal-input command requires credential read/write protection. Running it without that protection is blocked; sandbox escalation is unavailable.".into()),
    }
}

fn path_argument_is_sensitive(raw: &str, cwd: &Path, metadata: bool) -> bool {
    let path = expand_shell_path(raw, cwd);
    match PathFacts::gather(&path, cwd).sensitivity {
        None => false,
        // 凭据和 agent state 连名字都算敏感，元数据访问也不例外。
        Some(Sensitivity::Secret) => true,
        // `.git` 的元数据（stat、列目录）不是内容读取；内容访问仍是硬边界。
        Some(Sensitivity::Vcs) => !metadata,
    }
}

/// The one exemption to `.kloop` being sensitive: `.kloop/worktrees/<name>` holds
/// checkouts the agent is *supposed* to edit wholesale. Only this exact segment
/// pair is structural — a nested `.kloop` inside the worktree, and every other
/// child of `.kloop`, stays sensitive.
const WORKTREE_SEGMENT: &str = "worktrees";

/// Whether a raw command string may mention `.kloop/worktrees/`. The string
/// layer is the only protection left when a command is too opaque to parse into
/// argv, so it refuses to grant the exemption to anything carrying a `..`: a
/// spelling like `.kloop/worktrees/../sessions` must not walk out through the
/// hole. Normalized paths are exempted structurally instead, in
/// [`path_sensitivity`].
fn worktree_mention_is_structural(command: &str) -> bool {
    !command.contains("..")
}

/// The second structural exemption to `.kloop` being sensitive: a spilled tool
/// result (`…/offload/off-0001.txt`) or a background shell's output
/// (`…/offload/bg-3.out`). These are the model's *own* output — it was handed a
/// head/tail preview and this exact path — so re-reading one discloses nothing a
/// tool would not have returned anyway, while refusing turns every oversized
/// result into a dead end. Nothing else in the store qualifies: the provider
/// credential is in `config.toml` and every project's transcript in `sessions/`,
/// and both stay sensitive.
fn is_spill_file_name(name: &str) -> bool {
    let random = |rest: &str, ext: &str| {
        rest.strip_suffix(ext)
            .is_some_and(crate::resource_id::is_suffix)
    };
    name.strip_prefix("off-")
        .is_some_and(|rest| random(rest, ".txt"))
        || name
            .strip_prefix("bg-")
            .is_some_and(|rest| random(rest, ".out"))
}

/// Whether one shell token is exactly such a spill path. Token-wise rather than
/// a substring scan of the whole command, so masking one path never unmasks a
/// different `.kloop` mention sitting beside it. A `..` forfeits the exemption,
/// as it does for worktrees.
fn is_spill_path_token(token: &str) -> bool {
    if token.contains("..") {
        return false;
    }
    let Some((parent, name)) = token.rsplit_once('/') else {
        return false;
    };
    parent.ends_with("/offload") && is_spill_file_name(name)
}

/// Neutralize the `.kloop` inside spill tokens only, leaving every other token
/// for the needle scan. Separators are preserved so the rebuilt string keeps the
/// original shape.
fn mask_spill_tokens(command: &str) -> String {
    const SEPARATORS: &[char] = &[
        ' ', '\t', '\n', '\r', '\'', '"', '`', '(', ')', ';', '|', '<', '>', '=', ',', '&', '{',
        '}',
    ];
    let mut out = String::with_capacity(command.len());
    let mut token = String::new();
    let flush = |token: &mut String, out: &mut String| {
        if is_spill_path_token(token) {
            out.push_str(&token.replace("/.kloop/", "/kloop/"));
        } else {
            out.push_str(token);
        }
        token.clear();
    };
    for ch in command.chars() {
        if SEPARATORS.contains(&ch) {
            flush(&mut token, &mut out);
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    flush(&mut token, &mut out);
    out
}

fn raw_mentions_sensitive_path(command: &str) -> bool {
    let folded = fs_fold(command);
    let masked = mask_spill_tokens(folded.as_ref());
    let command = masked.as_str();
    if command.contains("/.kloop/worktrees/") && worktree_mention_is_structural(command) {
        // Re-test with the managed worktree prefix neutralized, so the rest of
        // the command is still screened for real state paths.
        let masked = command.replace("/.kloop/worktrees/", "/worktrees/");
        return mentions_sensitive_needle(&masked);
    }
    mentions_sensitive_needle(command)
}

fn mentions_sensitive_needle(command: &str) -> bool {
    [
        "~/.kloop", "/.kloop/", "~/.ssh", "/.ssh/", "~/.gnupg", "/.gnupg/", "~/.aws", "/.aws/",
    ]
    .iter()
    .any(|needle| command.contains(needle))
}

fn powershell_mentions_sensitive_path(command: &str) -> bool {
    let normalized = command.to_lowercase().replace('\\', "/");
    let normalized = if normalized.contains("/.kloop/worktrees/")
        && worktree_mention_is_structural(&normalized)
    {
        normalized.replace("/.kloop/worktrees/", "/worktrees/")
    } else {
        normalized
    };
    let normalized = normalized.as_str();
    [".kloop", ".ssh", ".gnupg", ".aws", ".env"]
        .into_iter()
        .any(|needle| {
            normalized.match_indices(needle).any(|(index, _)| {
                let before = normalized[..index].chars().next_back();
                let after = normalized[index + needle.len()..].chars().next();
                let boundary = |character: Option<char>| {
                    character.is_none_or(|character| {
                        character == '/'
                            || character.is_whitespace()
                            || matches!(
                                character,
                                '\'' | '"'
                                    | '`'
                                    | '('
                                    | ')'
                                    | '['
                                    | ']'
                                    | '{'
                                    | '}'
                                    | '='
                                    | ':'
                                    | ';'
                                    | ','
                            )
                    })
                };
                boundary(before) && (needle == ".env" || boundary(after))
            })
        })
}

fn expand_shell_path(raw: &str, cwd: &Path) -> PathBuf {
    if raw == "~" {
        return std::env::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    }
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    lexical_normalize(cwd, Path::new(raw))
}

fn recursive_search_covers_sensitive_path(argv: &[String], cwd: &Path) -> bool {
    let Some(command) = argv.first().and_then(|raw| raw.rsplit('/').next()) else {
        return false;
    };
    let recursive = match command {
        // ripgrep is recursive, but only exposes hidden credential directories
        // when --hidden / -u is requested.
        "rg" => argv[1..].iter().any(|arg| {
            arg == "--hidden"
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg.chars().skip(1).any(|ch| ch == 'u'))
        }),
        "grep" => argv[1..]
            .iter()
            .any(|arg| matches!(arg.as_str(), "-r" | "-R" | "--recursive")),
        _ => false,
    };
    if !recursive {
        return false;
    }
    let roots: Vec<PathBuf> = argv[1..]
        .iter()
        .filter(|arg| !arg.starts_with('-'))
        .map(|arg| expand_shell_path(arg, cwd))
        .filter(|path| path.is_dir())
        .collect();
    let roots = if roots.is_empty() {
        vec![cwd.to_path_buf()]
    } else {
        roots
    };
    roots.iter().any(|root| {
        let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
        let local_sensitive = [".kloop", ".ssh", ".gnupg", ".aws"]
            .iter()
            .any(|name| root.join(name).exists());
        let global_config_is_below = std::env::home_dir()
            .map(|home| home.join(".kloop/config.toml").starts_with(&root))
            .unwrap_or(false);
        local_sensitive || global_config_is_below
    })
}

impl PathFacts {
    fn gather(raw: &Path, cwd: &Path) -> Self {
        Self::gather_with_resolved(raw, cwd, None)
    }

    pub(super) fn gather_with_resolved(
        raw: &Path,
        cwd: &Path,
        resolved_path: Option<&Path>,
    ) -> Self {
        let original = lexical_normalize(cwd, raw);
        let canonical_cwd =
            std::fs::canonicalize(cwd).unwrap_or_else(|_| lexical_normalize(Path::new("/"), cwd));
        let normalized = resolved_path.map(Path::to_path_buf).unwrap_or_else(|| {
            canonicalize_nearest_existing(&original).unwrap_or_else(|| original.clone())
        });
        let relative = normalized
            .strip_prefix(&canonical_cwd)
            .ok()
            .map(Path::to_path_buf);
        let original_relative = original.strip_prefix(cwd).ok().map(Path::to_path_buf);
        let inside_cwd = relative.is_some();
        let sensitivity =
            merge_sensitivity(path_sensitivity(&original), path_sensitivity(&normalized));
        let sensitive = sensitivity.is_some();
        PathFacts {
            normalized,
            relative,
            original,
            original_relative,
            inside_cwd,
            sensitive,
            sensitivity,
        }
    }
}

/// Canonicalize the nearest existing ancestor and reattach a missing suffix.
/// This resolves parent symlinks even when the final write target does not yet
/// exist, so permissions judge the same path the filesystem executor will use.
fn canonicalize_nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    loop {
        match std::fs::canonicalize(ancestor) {
            Ok(mut resolved) => {
                for component in missing.iter().rev() {
                    resolved.push(component);
                }
                return Some(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(ancestor.file_name()?.to_os_string());
                ancestor = ancestor.parent()?;
            }
            Err(_) => return None,
        }
    }
}

/// Resolve `.`/`..` lexically (no filesystem access — the target of a write
/// may not exist yet), anchoring relative paths at `cwd`.
fn lexical_normalize(cwd: &Path, path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for comp in joined.components() {
        match comp {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Case-folding filesystems (macOS/Windows default): `.GIT` and `.git` are
/// the same inode, so a cased alias would slip a write past a case-sensitive
/// sensitive-path or deny check and land on the real `.git/hooks`. Fold case
/// there; compare verbatim where distinct names are distinct files.
pub(super) const FS_FOLDS_CASE: bool = cfg!(any(target_os = "macos", target_os = "windows"));

fn fs_fold(name: &str) -> std::borrow::Cow<'_, str> {
    if FS_FOLDS_CASE {
        std::borrow::Cow::Owned(name.to_ascii_lowercase())
    } else {
        std::borrow::Cow::Borrowed(name)
    }
}

/// Whether a normalized path ends in `offload/<spill file>` — the structural
/// form of [`is_spill_file_name`], for callers that already have components.
fn path_tail_is_spill(normalized: &Path) -> bool {
    let Some(name) = normalized.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    if !is_spill_file_name(fs_fold(name).as_ref()) {
        return false;
    }
    normalized
        .parent()
        .and_then(Path::file_name)
        .and_then(|parent| parent.to_str())
        .is_some_and(|parent| fs_fold(parent) == "offload")
}

fn merge_sensitivity(a: Option<Sensitivity>, b: Option<Sensitivity>) -> Option<Sensitivity> {
    if a == Some(Sensitivity::Secret) || b == Some(Sensitivity::Secret) {
        Some(Sensitivity::Secret)
    } else {
        a.or(b)
    }
}

/// Paths where a write is privilege escalation, not editing (VCS internals,
/// kloop's own state, key material, shell/git rc files) and their read
/// classification: `Vcs` allows metadata access, `Secret` does not.
/// `Secret` wins over `Vcs`: a path that reaches both a repository and a
/// credential store is a secret path for every access kind.
fn path_sensitivity(normalized: &Path) -> Option<Sensitivity> {
    const VCS_DIR: &str = ".git";
    const SECRET_DIRS: &[&str] = &[".kloop", ".ssh", ".gnupg", ".aws"];
    const SECRET_FILES: &[&str] = &[
        ".bashrc",
        ".zshrc",
        ".zshenv",
        ".zprofile",
        ".bash_profile",
        ".profile",
        ".gitconfig",
        ".envrc",
    ];
    // Callers normalize first, but the exemption below must not depend on that:
    // `.kloop/worktrees/../sessions` is only a worktree path if you stop reading
    // at the second segment. A `..` anywhere forfeits the exemption entirely.
    let traversal = normalized
        .components()
        .any(|comp| matches!(comp, Component::ParentDir));
    let mut found = None;
    let mut components = normalized.components().peekable();
    while let Some(comp) = components.next() {
        let Component::Normal(name) = comp else {
            continue;
        };
        let Some(name) = name.to_str() else {
            return Some(Sensitivity::Secret); // non-UTF8 path: refuse to vouch
        };
        let folded = fs_fold(name);
        let name = folded.as_ref();
        let is_last = components.peek().is_none();
        // `.kloop/worktrees/...` is a checkout, not kloop's state. Skipping the
        // `.kloop` segment (rather than returning false) keeps the scan running
        // over the rest, so a nested `.kloop` inside the worktree still trips.
        if name == ".kloop"
            && !traversal
            && components.peek().is_some_and(|next| {
                matches!(next, Component::Normal(next)
                    if fs_fold(&next.to_string_lossy()) == WORKTREE_SEGMENT)
            })
        {
            continue;
        }
        // The spill exemption reads the *tail* rather than the next segment,
        // because the store partitions by project between the two
        // (`.kloop/projects/v1/<id>/offload/off-0001.txt`). Skipping `.kloop`
        // rather than returning keeps the scan running, so a `.ssh` or a nested
        // `.kloop` elsewhere in the path still trips.
        if name == ".kloop" && !traversal && path_tail_is_spill(normalized) {
            continue;
        }
        if name == VCS_DIR {
            found = Some(Sensitivity::Vcs);
            continue;
        }
        if SECRET_DIRS.contains(&name) {
            return Some(Sensitivity::Secret);
        }
        if is_last && (SECRET_FILES.contains(&name) || name.starts_with(".env")) {
            return Some(Sensitivity::Secret);
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan239_partial_commands_keep_sensitive_reads() {
        let call = CallFacts::gather(
            "bash",
            &serde_json::json!({"command": "echo $VALUE; cat .env"}),
            Path::new("/workspace"),
            None,
        );
        assert!(call.sensitive_read);
    }

    #[test]
    fn plan239_partial_commands_keep_hazards() {
        let call = CallFacts::gather(
            "bash",
            &serde_json::json!({"command": "echo $VALUE; rm -rf build"}),
            Path::new("/workspace"),
            None,
        );
        assert_eq!(
            call.hazard("bash").map(|hazard| hazard.tag),
            Some("destructive")
        );
    }

    #[test]
    fn plan239_literal_probe_passes_sensitive_gate() {
        let home = std::env::temp_dir().join("kloop-plan239-facts-home");
        std::fs::create_dir_all(&home).unwrap();
        let policy = crate::sandbox::SandboxPolicy::workspace(&home, &[], false)
            .with_credential_roots(&home);
        let call = CallFacts::gather_with_execution(
            "bash",
            &serde_json::json!({"command": "cat > /tmp/probe.rs <<'EOF'\nlet path = \"~/.ssh/id_rsa\";\nEOF\ncargo run"}),
            Path::new("/workspace"),
            None,
            ExecutionContext::sandboxed(Some(&policy)),
        );
        assert_eq!(call.sensitive_read, !cfg!(target_os = "macos"));
    }

    /// The string layer is the only screen left for a command too opaque to
    /// parse into argv, so its exemption must not become a traversal hole.
    #[test]
    fn raw_command_exemption_covers_worktrees_but_never_a_traversal() {
        assert!(!raw_mentions_sensitive_path(
            "cat /w/proj/.kloop/worktrees/wt/src/main.rs"
        ));
        assert!(raw_mentions_sensitive_path(
            "cat /w/proj/.kloop/sessions/x.jsonl"
        ));
        assert!(raw_mentions_sensitive_path(
            "cat /w/proj/.kloop/worktrees/../sessions/x.jsonl"
        ));
        // The rest of an exempted command is still screened.
        assert!(raw_mentions_sensitive_path(
            "cp /w/proj/.kloop/worktrees/wt/a ~/.ssh/authorized_keys"
        ));
        assert!(powershell_mentions_sensitive_path(
            "Get-Content /w/proj/.kloop/sessions/x.jsonl"
        ));
        assert!(!powershell_mentions_sensitive_path(
            "Get-Content /w/proj/.kloop/worktrees/wt/src/main.rs"
        ));
    }

    #[test]
    fn sensitive_path_detection_is_component_based() {
        let s = |p: &str| path_sensitivity(Path::new(p)).is_some();
        assert!(s("/w/proj/.git/hooks/pre-commit"));
        assert!(s("/w/proj/.git"));
        assert!(s("/w/proj/.kloop/sessions/x.jsonl"));
        assert!(s("/home/u/.bashrc"));
        assert!(s("/w/proj/.env.production"));
        assert!(s("/home/u/.ssh/id_rsa"));

        assert!(!s("/w/proj/src/main.rs"));
        // A managed worktree is a checkout the agent edits wholesale: the one
        // exempt segment pair. Everything else under `.kloop` stays sensitive,
        // including a nested `.kloop` inside the worktree and a traversal that
        // re-enters kloop's own state through the exemption.
        let tree = format!("/w/proj/{}/wt", crate::worktree::WORKTREES_DIR);
        assert!(!s(&format!("{tree}/src/main.rs")));
        assert!(s(&format!("{tree}/.kloop/config.toml")), "nested state");
        assert!(s("/w/proj/.kloop/worktrees/../sessions/x.jsonl"));
        assert!(
            s("/w/proj/.kloop/worktreesx/src/main.rs"),
            "prefix ≠ segment"
        );
        assert!(!s("/w/proj/git/readme.md"), "git dir ≠ .git dir");
        assert!(!s("/w/proj/environment.rs"), ".env prefix is filename-only");
    }

    #[test]
    fn sensitive_path_folds_case_on_case_insensitive_fs() {
        let s = |p: &str| path_sensitivity(Path::new(p)).is_some();
        // A cased alias resolves to the real `.git`/`.ssh`/`.env*` on a
        // case-folding FS, so it must be caught there; on a case-sensitive FS
        // those are genuinely distinct paths and stay non-sensitive.
        assert_eq!(s("/w/proj/.GIT/hooks/pre-commit"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.SSH/id_rsa"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.Env.production"), FS_FOLDS_CASE);
        assert_eq!(s("/home/u/.Bashrc"), FS_FOLDS_CASE);
    }
}
