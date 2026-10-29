//! 调用的 shell、路径、敏感读取与危险特征；不依赖权限管线。

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::sandbox::ExecutionContext;
use crate::shell::BashAnalysis;
use crate::shell::argv_is_dangerous;
use crate::shell::strip_wrappers;
use crate::shell::{
    BashFacts, CommandFact, Direction, WordEvaluation, WordValue, analyze_bash_facts,
};
use crate::tools::Builtin;

use super::argv_roles;
use super::sensitive;
pub(super) use super::sensitive::FS_FOLDS_CASE;
pub(super) use super::sensitive::Sensitivity;
use super::sensitive::fs_fold;

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
    pub(super) risk_writes: Vec<WriteTarget>,
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

/// 未知效果不能从规则输入中消失；只有 Resolved 可按确切路径排除 deny/ask。
pub(super) enum WriteTarget {
    Resolved(PathFacts),
    Partial { sensitive: bool },
    Unknown,
}

impl WriteTarget {
    fn sensitive(&self) -> bool {
        match self {
            Self::Resolved(path) => {
                path.sensitivity(KloopScope::Anywhere).is_some()
                    || (path.normalized.starts_with("/dev")
                        && !is_stream_device(&path.original)
                        && !is_stream_device(&path.normalized))
            }
            Self::Partial { sensitive } => *sensitive,
            Self::Unknown => false,
        }
    }
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
            && path
                .as_ref()
                .is_some_and(|path| path.sensitivity(KloopScope::PrivateRoot).is_some()))
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
        let risk_writes = bash_facts
            .as_ref()
            .map_or_else(Vec::new, |facts| write_targets(facts, cwd, execution));
        CallFacts {
            shell,
            writes,
            path,
            path_key,
            sensitive_read,
            risk_commands: bash_facts
                .as_ref()
                .map_or_else(Vec::new, |facts| facts.commands.clone()),
            risk_writes,
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
        if self.risk_writes.iter().any(WriteTarget::sensitive) {
            return Some(Hazard {
                tag: "sensitive redirect path",
            });
        }
        if matches!(name, "write_file" | "edit_file" | "notebook_edit")
            && self
                .path
                .as_ref()
                .is_some_and(|p| p.sensitivity(KloopScope::Anywhere).is_some())
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

/// `.kloop` 在哪里算敏感（见 [`sensitive::KLOOP_DIR`]）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum KloopScope {
    /// 写入，以及落点未知的读取：任何位置的 `.kloop` 都算。
    Anywhere,
    /// 落点已知的读取：只有私有状态根 `~/.kloop` 算。
    PrivateRoot,
}

/// 一条路径拼写的访问方向，决定它的 `.kloop` 口径。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Access {
    Write,
    /// `cwd_unknown`：前面有 `cd` 一类命令，相对拼写不能按会话 cwd 定位。
    Read {
        cwd_unknown: bool,
    },
}

impl Access {
    fn kloop_scope(self, spelling: &str) -> KloopScope {
        match self {
            Self::Read { cwd_unknown } if !cwd_unknown || Path::new(spelling).is_absolute() => {
                KloopScope::PrivateRoot
            }
            Self::Read { .. } | Self::Write => KloopScope::Anywhere,
        }
    }
}

fn argv_reads_sensitive_path(argv: &[String], cwd: &Path, access: Access) -> bool {
    let paths = argv_roles::path_arguments(argv);
    // 已建模命令解析失败时角色不明，按内容访问处理；未建模命令（`ls`、
    // `stat`）没有角色表可失败，元数据判定直接生效。
    let metadata = argv_roles::is_metadata_access(argv)
        && (paths.is_some() || !argv_roles::is_modeled_command(argv));
    let sensitive = match paths {
        // 识别出参数角色：只检查命令真正会打开的值。
        Some(paths) => paths
            .into_iter()
            .any(|arg| path_argument_is_sensitive(arg, cwd, metadata, access)),
        // 未知命令或语法：退回逐参数检查。
        None => argv.iter().skip(1).any(|arg| {
            if arg.starts_with('-') && !arg.contains('/') {
                return false;
            }
            path_argument_is_sensitive(arg, cwd, metadata, access)
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
    {
        let home = execution.shell_home();
        for command in &facts.commands {
            let home = home.as_deref().filter(|_| !command.context.home_unknown);
            let access = Access::Read {
                cwd_unknown: command.context.cwd_unknown,
            };
            let words: Vec<_> = command
                .words
                .iter()
                .map(|word| word.syntax.evaluate(home))
                .collect();
            // 不删除未知参数：角色表只有在每个位置都有完整拼写时才能使用。
            let argv: Option<Vec<_>> = words
                .iter()
                .map(|word| match word {
                    WordEvaluation::Resolved(text) => Some(text.clone()),
                    WordEvaluation::Partial(_) | WordEvaluation::Unknown => None,
                })
                .collect();
            if let Some(argv) = argv {
                if argv_reads_sensitive_path(&argv, cwd, access) {
                    return ReadVerdict::Sensitive;
                }
            } else if !arguments_are_text(command)
                && words
                    .iter()
                    .skip(1)
                    .any(|word| evaluation_is_sensitive(word, cwd, false, access))
            {
                return ReadVerdict::Sensitive;
            }
        }
        for redirect in &facts.redirects {
            if redirect.direction != Direction::Read {
                continue;
            }
            let home = home.as_deref().filter(|_| !redirect.context.home_unknown);
            let access = Access::Read {
                cwd_unknown: redirect.context.cwd_unknown,
            };
            if evaluation_is_sensitive(&redirect.syntax.evaluate(home), cwd, false, access) {
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

/// `echo`/`printf` 的参数只是文本。参数角色表在 argv 完整时才给出这条豁免；
/// 参数里出现动态词（`echo "$PWD/.env"`）走不了角色表，这里补上同一句。
fn arguments_are_text(command: &CommandFact) -> bool {
    matches!(
        command.words.first().map(|word| &word.value),
        Some(WordValue::Literal(name))
            if matches!(name.rsplit('/').next(), Some("echo" | "printf"))
    )
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
        ReadVerdict::Sensitive | ReadVerdict::Unresolved => {
            Err(crate::sandbox::PROTECTION_REQUIRED.into())
        }
    }
}

fn path_argument_is_sensitive(raw: &str, cwd: &Path, metadata: bool, access: Access) -> bool {
    let path = lexical_normalize(cwd, Path::new(raw));
    let scope = access.kloop_scope(raw);
    let sensitivity = merge_sensitivity(
        PathFacts::gather(&path, cwd).sensitivity(scope),
        glob_spelling_sensitivity(&path, metadata, scope),
    );
    match sensitivity {
        None => false,
        // 凭据和 agent state 连名字都算敏感，元数据访问也不例外。
        Some(Sensitivity::Secret) => true,
        // `.git` 的元数据（stat、列目录）不是内容读取；内容访问仍是硬边界。
        Some(Sensitivity::Vcs) => !metadata,
    }
}

fn evaluation_is_sensitive(
    word: &WordEvaluation,
    cwd: &Path,
    metadata: bool,
    access: Access,
) -> bool {
    match word {
        WordEvaluation::Resolved(path) => path_argument_is_sensitive(path, cwd, metadata, access),
        WordEvaluation::Partial(spelling) if !spelling.contains('\0') => {
            path_argument_is_sensitive(spelling, cwd, metadata, access)
        }
        WordEvaluation::Partial(spelling) => {
            // 未知片段不能假装成文件系统落点，但已知的敏感分量仍然成立；
            // 落点未知，`.kloop` 也只能按名字算。
            let path = Path::new(spelling);
            match merge_sensitivity(
                path_sensitivity(path, KloopScope::Anywhere),
                glob_spelling_sensitivity(path, metadata, KloopScope::Anywhere),
            ) {
                Some(Sensitivity::Secret) => true,
                Some(Sensitivity::Vcs) => !metadata,
                None => false,
            }
        }
        WordEvaluation::Unknown => false,
    }
}

/// 未展开的 glob 分量能否拼出敏感名字。判定不读文件系统：只看拼写，所以与
/// bash 何时展开无关，也不会因为目录里恰好没有该文件而放行。
fn glob_spelling_sensitivity(
    path: &Path,
    metadata: bool,
    scope: KloopScope,
) -> Option<Sensitivity> {
    let components: Vec<_> = path.components().collect();
    let mut found = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            continue;
        };
        let Some(name) = name.to_str() else {
            continue;
        };
        if sensitive::glob_component_may_be_kloop(name)
            && match scope {
                KloopScope::Anywhere => true,
                KloopScope::PrivateRoot => {
                    let parent: PathBuf = components[..index].iter().collect();
                    let canonical = canonicalize_nearest_existing(&parent);
                    std::iter::once(parent)
                        .chain(canonical)
                        .any(|parent| is_private_state_root(&parent.join(sensitive::KLOOP_DIR)))
                }
            }
        {
            return Some(Sensitivity::Secret);
        }
        let last = index + 1 == components.len();
        match sensitive::glob_component_sensitivity(name, last) {
            Some(Sensitivity::Secret) => return Some(Sensitivity::Secret),
            Some(Sensitivity::Vcs) => found = Some(Sensitivity::Vcs),
            None => {}
        }
    }
    found.filter(|_| !metadata)
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

/// 文本层只认目录拼写：凭据目录认 `~/name` 与 `/name/`；`.kloop` 只认私有状态根
/// 的拼写（`~`、`$HOME`、`${HOME}` 前缀与它的绝对路径），项目里的
/// `/repo/.kloop/rules` 不算。文件名（`.env*`、rc 文件）走路径与 argv 判定；把它们
/// 塞进子串扫描会让 `echo "add .env to .gitignore"` 这类纯文本也变成敏感读取。
/// 调用方传入已按文件系统折叠大小写的原文。
fn mentions_sensitive_needle(command: &str) -> bool {
    let kloop = sensitive::KLOOP_DIR;
    sensitive::SECRET_DIRS
        .iter()
        .any(|dir| command.contains(&format!("~/{dir}")) || command.contains(&format!("/{dir}/")))
        || ["~", "$HOME", "${HOME}"]
            .into_iter()
            .map(|home| format!("{home}/{kloop}"))
            .chain(
                private_state_roots()
                    .iter()
                    .map(|root| root.to_string_lossy().into_owned()),
            )
            .any(|needle| command.contains(fs_fold(&needle).as_ref()))
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
    // PowerShell 没有解析层能给出落点，`.kloop` 只能按名字算。
    sensitive::SECRET_DIRS
        .iter()
        .copied()
        .chain([sensitive::KLOOP_DIR, sensitive::ENV_PREFIX])
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
                boundary(before) && (needle == sensitive::ENV_PREFIX || boundary(after))
            })
        })
}

/// 递归搜索会读出根下所有文件的内容：命令是否真的递归、以及根里有没有凭据名，
/// 两件事都要问清楚。递归开关走命令自己的选项簇解析（`-rn`、`-d recurse`、
/// `-uu`、`-.`），根下名单与敏感判定共用 [`sensitive`]。
fn recursive_search_covers_sensitive_path(argv: &[String], cwd: &Path) -> bool {
    if !argv_roles::search_is_recursive(argv) {
        return false;
    }
    let roots: Option<Vec<PathBuf>> = argv_roles::search_roots(argv).map(|paths| {
        paths
            .iter()
            .map(|path| lexical_normalize(cwd, Path::new(path)))
            .collect()
    });
    match roots {
        // 角色表说没有路径参数：grep/rg 在 cwd 下递归。
        Some(paths) if paths.is_empty() => recursive_root_is_sensitive(cwd),
        // 明确给了路径：只读这些根。给了普通文件就不是目录遍历，不能拿 cwd 顶替。
        Some(paths) => paths
            .iter()
            .filter(|path| path.is_dir())
            .any(|root| recursive_root_is_sensitive(root)),
        // 选项簇解析失败时角色不明：能认出目录参数的按根检查，认不出时按可能读
        // cwd 处理（保守方向）。
        None => {
            let roots: Vec<PathBuf> = argv[1..]
                .iter()
                .filter(|arg| !arg.starts_with('-'))
                .map(|arg| lexical_normalize(cwd, Path::new(arg)))
                .filter(|path| path.is_dir())
                .collect();
            if roots.is_empty() {
                recursive_root_is_sensitive(cwd)
            } else {
                roots.iter().any(|root| recursive_root_is_sensitive(root))
            }
        }
    }
}

fn recursive_root_is_sensitive(root: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if path_sensitivity(&root, KloopScope::PrivateRoot).is_some() {
        return true;
    }
    if private_state_roots()
        .iter()
        .any(|state| starts_with_folded(state, &root))
    {
        return true;
    }
    // `.git` 不在名单里：仓库内 `grep -r` 是日常操作，而 `.git` 的内容访问
    // 已由路径参数判定兜底。项目里的 `.kloop/` 也不在：私有状态根由上一条按
    // 位置判定。
    std::fs::read_dir(&root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(sensitive::name_is_secret)
        })
    })
}

/// argv 与重定向共用求值器；不确定的写效果必须保留给 deny/ask。
fn write_targets(
    facts: &BashFacts,
    cwd: &Path,
    execution: ExecutionContext<'_>,
) -> Vec<WriteTarget> {
    let home = execution.shell_home();
    let mut writes: Vec<_> = facts
        .redirects
        .iter()
        .filter(|redirect| redirect.direction == Direction::Write)
        .filter_map(|redirect| {
            let home = home.as_deref().filter(|_| !redirect.context.home_unknown);
            let target = redirect.syntax.evaluate(home);
            match target {
                // `/dev/null` 只是丢掉输出，没有写效果。
                WordEvaluation::Resolved(path) if path == "/dev/null" => None,
                WordEvaluation::Resolved(path)
                    if !redirect.context.cwd_unknown || Path::new(&path).is_absolute() =>
                {
                    Some(WriteTarget::Resolved(PathFacts::gather(
                        Path::new(&path),
                        cwd,
                    )))
                }
                WordEvaluation::Resolved(path) => Some(WriteTarget::Partial {
                    sensitive: evaluation_is_sensitive(
                        &WordEvaluation::Partial(path),
                        cwd,
                        false,
                        Access::Write,
                    ),
                }),
                WordEvaluation::Partial(_) => Some(WriteTarget::Partial {
                    sensitive: evaluation_is_sensitive(&target, cwd, false, Access::Write),
                }),
                WordEvaluation::Unknown => Some(WriteTarget::Unknown),
            }
        })
        .collect();
    if !facts.reliable {
        // 达到解析界限时不能用空效果列表证明“不写文件”。
        writes.push(WriteTarget::Unknown);
    }
    writes
}

/// 纯流设备：写进去没有持久副作用，不按敏感目标处理。
fn is_stream_device(path: &Path) -> bool {
    matches!(
        path.to_str(),
        Some("/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty")
    )
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
        PathFacts {
            normalized,
            relative,
            original,
            original_relative,
            inside_cwd,
        }
    }

    /// 原拼写与规范目标任一落在名单里即算；`scope` 决定 `.kloop` 的口径：
    /// 写入安全用 [`KloopScope::Anywhere`]，读取用 [`KloopScope::PrivateRoot`]。
    pub(super) fn sensitivity(&self, scope: KloopScope) -> Option<Sensitivity> {
        merge_sensitivity(
            path_sensitivity(&self.original, scope),
            path_sensitivity(&self.normalized, scope),
        )
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
/// credential store is a secret path for every access kind. `scope` decides
/// which `.kloop` directories count.
fn path_sensitivity(normalized: &Path, scope: KloopScope) -> Option<Sensitivity> {
    // Callers normalize first, but the exemption below must not depend on that:
    // `.kloop/worktrees/../sessions` is only a worktree path if you stop reading
    // at the second segment. A `..` anywhere forfeits the exemption entirely.
    let traversal = normalized
        .components()
        .any(|comp| matches!(comp, Component::ParentDir));
    let mut found = None;
    let mut prefix = PathBuf::new();
    let mut components = normalized.components().peekable();
    while let Some(comp) = components.next() {
        prefix.push(comp);
        let Component::Normal(name) = comp else {
            continue;
        };
        let Some(name) = name.to_str() else {
            return Some(Sensitivity::Secret); // non-UTF8 path: refuse to vouch
        };
        let folded = fs_fold(name);
        let name = folded.as_ref();
        let is_last = components.peek().is_none();
        if name == sensitive::KLOOP_DIR {
            // `.kloop/worktrees/...` is a checkout, not kloop's state. Skipping
            // the `.kloop` segment (rather than returning false) keeps the scan
            // running over the rest, so a nested `.kloop` inside the worktree
            // still trips.
            let worktree = components.peek().is_some_and(|next| {
                matches!(next, Component::Normal(next)
                    if fs_fold(&next.to_string_lossy()) == WORKTREE_SEGMENT)
            });
            // The spill exemption reads the *tail* rather than the next segment,
            // because the store partitions by project between the two
            // (`.kloop/projects/v1/<id>/offload/off-0001.txt`).
            let exempt = !traversal && (worktree || path_tail_is_spill(normalized));
            let counted = match scope {
                KloopScope::Anywhere => true,
                KloopScope::PrivateRoot => is_private_state_root(&prefix),
            };
            if counted && !exempt {
                return Some(Sensitivity::Secret);
            }
            continue;
        }
        if name == sensitive::VCS_DIR {
            found = Some(Sensitivity::Vcs);
            continue;
        }
        if sensitive::dir_name_is_secret(name) {
            return Some(Sensitivity::Secret);
        }
        if is_last && sensitive::file_name_is_secret(name) {
            return Some(Sensitivity::Secret);
        }
    }
    found
}

/// 私有状态根 `~/.kloop` 的字面与规范两种拼写。与 CLI 的 `private_state_root`
/// 同源（进程的 home），不是工具 shell 的 `HOME`。
fn private_state_roots() -> Vec<PathBuf> {
    let Some(home) = std::env::home_dir() else {
        return Vec::new();
    };
    let root = home.join(sensitive::KLOOP_DIR);
    let canonical = canonicalize_nearest_existing(&root).filter(|canonical| *canonical != root);
    std::iter::once(root).chain(canonical).collect()
}

fn is_private_state_root(path: &Path) -> bool {
    private_state_roots().iter().any(|root| {
        path.components().count() == root.components().count() && starts_with_folded(path, root)
    })
}

/// 按文件系统的大小写口径逐分量比较前缀。
fn starts_with_folded(path: &Path, base: &Path) -> bool {
    let mut path = path.components();
    base.components().all(|base| {
        path.next().is_some_and(|part| {
            fs_fold(&part.as_os_str().to_string_lossy())
                == fs_fold(&base.as_os_str().to_string_lossy())
        })
    })
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
        // A repository rooted at home keeps its worktrees inside the private
        // state root, so the exemption still matters at the text layer.
        let home = std::env::home_dir().expect("home");
        let home = home.display();
        assert!(!raw_mentions_sensitive_path(&format!(
            "cat {home}/.kloop/worktrees/wt/src/main.rs"
        )));
        assert!(raw_mentions_sensitive_path(&format!(
            "cat {home}/.kloop/sessions/x.jsonl"
        )));
        assert!(raw_mentions_sensitive_path(&format!(
            "cat {home}/.kloop/worktrees/../sessions/x.jsonl"
        )));
        // The rest of an exempted command is still screened.
        assert!(raw_mentions_sensitive_path(&format!(
            "cp {home}/.kloop/worktrees/wt/a ~/.ssh/authorized_keys"
        )));
        assert!(powershell_mentions_sensitive_path(
            "Get-Content /w/proj/.kloop/sessions/x.jsonl"
        ));
        assert!(!powershell_mentions_sensitive_path(
            "Get-Content /w/proj/.kloop/worktrees/wt/src/main.rs"
        ));
    }

    #[test]
    fn sensitive_path_detection_is_component_based() {
        let s = |p: &str| path_sensitivity(Path::new(p), KloopScope::Anywhere).is_some();
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

    /// 读取只认私有状态根，写入按名字；两种口径共用 worktree 与溢出文件豁免。
    #[test]
    fn kloop_reads_count_only_the_private_state_root() {
        let home = std::env::home_dir().expect("home");
        let state = home.join(".kloop");
        let read = |path: &Path| path_sensitivity(path, KloopScope::PrivateRoot);
        let write = |path: &Path| path_sensitivity(path, KloopScope::Anywhere);

        let project = Path::new("/w/proj/.kloop/rules/style.md");
        assert_eq!(read(project), None);
        assert_eq!(write(project), Some(Sensitivity::Secret));
        assert_eq!(read(&state), Some(Sensitivity::Secret));
        assert_eq!(read(&state.join("config.toml")), Some(Sensitivity::Secret));
        assert_eq!(
            read(&home.join(".KLOOP/config.toml")),
            FS_FOLDS_CASE.then_some(Sensitivity::Secret)
        );
        // 项目 `.kloop` 下还有凭据名时照旧按凭据算。
        assert_eq!(
            read(Path::new("/w/proj/.kloop/rules/.env")),
            Some(Sensitivity::Secret)
        );
        assert_eq!(read(&state.join("worktrees/wt/src/main.rs")), None);
        assert_eq!(
            read(&state.join("worktrees/../sessions/x.jsonl")),
            Some(Sensitivity::Secret)
        );

        let glob =
            |raw: &str| glob_spelling_sensitivity(Path::new(raw), false, KloopScope::PrivateRoot);
        assert_eq!(
            glob(&format!("{}/.k*/config.toml", home.display())),
            Some(Sensitivity::Secret)
        );
        assert_eq!(glob("/w/proj/.k*/rules"), None);
        assert_eq!(
            glob_spelling_sensitivity(Path::new("/w/proj/.k*/rules"), false, KloopScope::Anywhere),
            Some(Sensitivity::Secret)
        );
    }

    /// 别名经规范化后落回私有状态根，仍按状态根判定。
    #[cfg(unix)]
    #[test]
    fn kloop_reads_follow_aliases_into_the_private_state_root() {
        let home = std::env::home_dir().expect("home");
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan271-alias-").expect("fresh id"));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&home, root.join("h")).unwrap();
        let facts = PathFacts::gather(&root.join("h/.kloop/config.toml"), &root);
        assert_eq!(
            facts.sensitivity(KloopScope::PrivateRoot),
            Some(Sensitivity::Secret)
        );
        assert_eq!(
            glob_spelling_sensitivity(
                &root.join("h/.k*/config.toml"),
                false,
                KloopScope::PrivateRoot
            ),
            Some(Sensitivity::Secret)
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 原文扫描只认私有状态根的拼写；项目 `.kloop` 交给能定位的 argv 层。
    #[test]
    fn raw_scan_names_only_the_private_state_root() {
        let home = std::env::home_dir().expect("home");
        let home = home.display();
        for command in [
            "cat ~/.kloop/config.toml".to_string(),
            "cat $HOME/.kloop/config.toml".into(),
            "cat ${HOME}/.kloop/config.toml".into(),
            format!("python3 -c 'open(\"{home}/.kloop/config.toml\")'"),
        ] {
            assert!(raw_mentions_sensitive_path(&command), "{command}");
        }
        for command in [
            "cat /w/proj/.kloop/rules/style.md",
            "cat ./.kloop/skills/x/SKILL.md",
            "grep -rn x /w/proj/.kloop",
        ] {
            assert!(!raw_mentions_sensitive_path(command), "{command}");
        }
    }

    /// 递归根里有项目 `.kloop/` 不算；根把私有状态根包在里面才算。
    #[test]
    fn recursive_roots_judge_kloop_by_location() {
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan271-recursive-").expect("fresh id"));
        std::fs::create_dir_all(root.join(".kloop/worktrees")).unwrap();
        let project = ["grep", "-rn", "KEY", "."].map(str::to_string);
        assert!(!recursive_search_covers_sensitive_path(&project, &root));
        let home = std::env::home_dir().expect("home");
        let above_home = [
            "grep".to_string(),
            "-rn".into(),
            "KEY".into(),
            home.parent()
                .expect("home has a parent")
                .display()
                .to_string(),
        ];
        assert!(recursive_search_covers_sensitive_path(&above_home, &root));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 名单只有一份：同一组名字在文本扫描、路径判定、递归搜索与沙箱凭据根里
    /// 给出相同答案。递归搜索漏掉 `.env*` 就是名单分家的结果。
    #[test]
    fn sensitive_names_agree_across_every_caller() {
        let scopes = [KloopScope::Anywhere, KloopScope::PrivateRoot];
        for name in sensitive::SECRET_DIRS {
            for scope in scopes {
                assert!(
                    path_sensitivity(&Path::new("/w/proj").join(name).join("x"), scope).is_some(),
                    "{name}"
                );
            }
            assert!(mentions_sensitive_needle(&format!("~/{name}")), "{name}");
            assert!(
                powershell_mentions_sensitive_path(&format!(r"C:\Users\u\{name}")),
                "{name}"
            );
            let root = std::env::temp_dir()
                .join(crate::resource_id::fresh("kloop-plan243-names-").expect("fresh id"));
            std::fs::create_dir_all(root.join(name)).unwrap();
            let argv = [
                "grep".to_string(),
                "-rn".into(),
                "KEY".into(),
                root.display().to_string(),
            ];
            assert!(
                recursive_search_covers_sensitive_path(&argv, &root),
                "{name}"
            );
            let _ = std::fs::remove_dir_all(&root);
        }
        for name in sensitive::SECRET_FILES
            .iter()
            .copied()
            .chain([sensitive::ENV_PREFIX])
        {
            for scope in scopes {
                assert!(
                    path_sensitivity(&Path::new("/w/proj").join(name), scope).is_some(),
                    "{name}"
                );
            }
        }
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan243-env-").expect("fresh id"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "KEY=x").unwrap();
        let argv = [
            "grep".to_string(),
            "-rn".into(),
            "KEY".into(),
            root.display().to_string(),
        ];
        assert!(
            recursive_search_covers_sensitive_path(&argv, &root),
            "recursive search must see .env* like the path check does"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// glob 拼写不访问文件系统：能拼出敏感名字才拦，拼不出就照旧放行。
    #[test]
    fn glob_spellings_are_screened_without_touching_the_filesystem() {
        let cwd = Path::new("/w/proj");
        let read = |raw: &str| {
            path_argument_is_sensitive(raw, cwd, false, Access::Read { cwd_unknown: false })
        };
        assert!(read(".en?"));
        assert!(read("./.gi?/config"));
        assert!(read("~/.s*/id_rsa"));
        assert!(!read(".c*"));
        assert!(!read("*.env"));
        // 长度与字符类都要与 `.env*` 真正有交集，不能只看字面前缀。
        assert!(!read(".??"));
        assert!(!read(".e[ab]*"));
    }

    /// 指定了普通文件就只读那个文件；不能因为“根里没有目录”退回 cwd。
    #[test]
    fn explicit_file_roots_do_not_fall_back_to_cwd() {
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan243-file-root-").expect("fresh id"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "KEY=x").unwrap();
        std::fs::write(root.join("notes.txt"), "KEY").unwrap();
        let file = ["grep", "-rn", "KEY", "notes.txt"].map(str::to_string);
        assert!(
            !recursive_search_covers_sensitive_path(&file, &root),
            "`grep -rn KEY notes.txt` never reads the cwd's .env"
        );
        let no_root = ["grep", "-rn", "KEY"].map(str::to_string);
        assert!(
            recursive_search_covers_sensitive_path(&no_root, &root),
            "no explicit path searches cwd"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 选项读取的文件（`-f patterns.txt`）不是搜索根：没有位置参数时仍搜 cwd。
    #[test]
    fn pattern_files_are_not_search_roots() {
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan243-pattern-file-").expect("fresh id"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join(".env"), "KEY=x").unwrap();
        std::fs::write(root.join("patterns.txt"), "KEY").unwrap();
        std::fs::write(root.join("notes.txt"), "KEY").unwrap();
        let pattern_only = ["rg", "-uu", "-f", "patterns.txt"].map(str::to_string);
        assert!(
            recursive_search_covers_sensitive_path(&pattern_only, &root),
            "`rg -uu -f patterns.txt` still searches the cwd"
        );
        let explicit_file = ["rg", "-uu", "-f", "patterns.txt", "notes.txt"].map(str::to_string);
        assert!(
            !recursive_search_covers_sensitive_path(&explicit_file, &root),
            "an explicit file root reads only that file"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 动态词按 shell 规则还原后再查名字：`cat "$PWD/.env"` 仍是读取。
    #[test]
    fn dynamic_words_are_checked_after_unquoting() {
        let call = CallFacts::gather(
            "bash",
            &serde_json::json!({ "command": "cat \"$PWD/.env\"" }),
            Path::new("/workspace"),
            None,
        );
        assert!(call.sensitive_read);
    }

    /// 动态纯文本参数不是读取：echo/printf 的参数角色豁免对 Unknown 词同样成立。
    #[test]
    fn text_arguments_stay_text_when_dynamic() {
        let call = CallFacts::gather(
            "bash",
            &serde_json::json!({ "command": "echo \"$PWD/.env\"" }),
            Path::new("/workspace"),
            None,
        );
        assert!(!call.sensitive_read);
    }

    /// 静态拼不出落点的写目标，至少按名字过一遍敏感名单。
    #[test]
    fn dynamic_write_targets_are_screened_by_name() {
        let hazard = |command: &str| {
            CallFacts::gather(
                "bash",
                &serde_json::json!({ "command": command }),
                Path::new("/workspace"),
                None,
            )
            .hazard("bash")
            .map(|hazard| hazard.tag)
        };
        assert_eq!(
            hazard("echo x > \"$DIR/.bashrc\""),
            Some("sensitive redirect path")
        );
        assert_eq!(hazard("echo x > \"$DIR/out.json\""), None);
    }

    #[test]
    fn sensitive_path_folds_case_on_case_insensitive_fs() {
        let s = |p: &str| path_sensitivity(Path::new(p), KloopScope::Anywhere).is_some();
        // A cased alias resolves to the real `.git`/`.ssh`/`.env*` on a
        // case-folding FS, so it must be caught there; on a case-sensitive FS
        // those are genuinely distinct paths and stay non-sensitive.
        assert_eq!(s("/w/proj/.GIT/hooks/pre-commit"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.SSH/id_rsa"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.Env.production"), FS_FOLDS_CASE);
        assert_eq!(s("/home/u/.Bashrc"), FS_FOLDS_CASE);
    }
}
