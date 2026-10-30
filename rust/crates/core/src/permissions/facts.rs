//! 调用的 shell、路径、敏感读取与危险特征；不依赖权限管线。

use std::path::Component;
use std::path::Path;
use std::path::PathBuf;

use serde_json::Value;

use crate::sandbox::ExecutionContext;
use crate::shell::BashAnalysis;
use crate::shell::argv_is_dangerous;
use crate::shell::argv_is_readonly;
use crate::shell::risk_wrapper_views;
use crate::shell::{
    BashFacts, CommandFact, Context, Direction, WordEvaluation, WordValue, analyze_bash_facts,
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
    /// 可解析 Bash 的原文，供逐字会话记忆与升级授权使用；其它工具为空。
    pub(super) command: Option<String>,
    /// `disable_sandbox`：脱沙箱运行是另一种权限，普通 `bash(...)` 授权不能覆盖。
    pub(super) no_sandbox: bool,
    pub(super) verbatim_escalation: bool,
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
                path.sensitivity(Access::Write).is_some()
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
                .is_some_and(|path| path.sensitivity(Access::Read).is_some()))
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
        let no_sandbox = name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false);
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
            command: (name == "bash")
                .then_some(shell_command)
                .flatten()
                .map(str::to_string),
            no_sandbox,
            verbatim_escalation: bash_facts
                .as_ref()
                .is_some_and(|facts| facts.verbatim_escalation),
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
        }) && cmds.iter().any(|argv| {
            argv_is_dangerous(argv)
                || risk_wrapper_views(argv)
                    .iter()
                    .any(|view| argv_is_dangerous(view))
        }) {
            return Some(Hazard { tag: "destructive" });
        }
        if self.risk_commands.iter().any(|command| {
            if command.context.argument_rewrite && !arguments_are_text(command) {
                return true;
            }
            let argv = command.risk_argv();
            argv_is_dangerous(&argv)
                || risk_wrapper_views(&argv)
                    .iter()
                    .any(|view| argv_is_dangerous(view))
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
                .is_some_and(|p| p.sensitivity(Access::Write).is_some())
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

/// 访问方向。只影响 `.kloop`：写入不分位置，只豁免 worktree；读取时，落点可信、
/// 且能证明不是私有状态根 `~/.kloop` 的 `.kloop`（项目里的）整个放行。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    /// 落点可信的读取：原生工具解析出的物理路径，或 Bash 里能定位的拼写。
    Read,
    /// 落点不可信的读取（见 [`Placement`]）：`.kloop` 按名字算。
    ReadUnlocated,
    Write,
}

/// 一条 Bash 命令里的读取拼写能否定位到具体目录。只信只读命令：会写或未知的命令
/// 可能自带换目录、换根的选项（`tar -C`、`bwrap --bind`），`git -C` 同理。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Placement {
    Untrusted,
    /// 只读命令，但前面改过 cwd：只信绝对拼写。
    AbsoluteOnly,
    Trusted,
}

impl Placement {
    fn of_command(argv: &[String], context: &Context) -> Self {
        let git = argv.first().and_then(|arg| arg.rsplit('/').next()) == Some("git");
        if !argv_is_readonly(argv) || git {
            Self::Untrusted
        } else {
            Self::of_shell(context)
        }
    }

    /// shell 自己打开的路径（读重定向）：只看 cwd 是否已知。
    fn of_shell(context: &Context) -> Self {
        if context.cwd_unknown {
            Self::AbsoluteOnly
        } else {
            Self::Trusted
        }
    }

    /// `..` 在解析符号链接之前就被词法消掉，含 `..` 的拼写不定位；Windows 上有根无
    /// 盘符的路径（Git Bash 的 `/c/…`，映射未建模）也不定位。
    fn access(self, spelling: &str) -> Access {
        let path = Path::new(spelling);
        let traversal = path
            .components()
            .any(|component| matches!(component, Component::ParentDir));
        let relative =
            !path.has_root() && !matches!(path.components().next(), Some(Component::Prefix(_)));
        let located = !traversal
            && match self {
                Self::Untrusted => false,
                Self::AbsoluteOnly => path.is_absolute(),
                Self::Trusted => path.is_absolute() || relative,
            };
        if located {
            Access::Read
        } else {
            Access::ReadUnlocated
        }
    }
}

/// 未知词换成不可执行的占位符，只用来判断命令是否只读。
fn placeholder_argv(words: &[WordEvaluation]) -> Vec<String> {
    words
        .iter()
        .map(|word| match word {
            WordEvaluation::Resolved(text) | WordEvaluation::Partial(text) => text.clone(),
            WordEvaluation::Unknown => "\0unknown".into(),
        })
        .collect()
}

fn argv_reads_sensitive_path(argv: &[String], cwd: &Path, placement: Placement) -> bool {
    let paths = argv_roles::path_arguments(argv);
    // 已建模命令解析失败时角色不明，按内容访问处理；未建模命令（`ls`、
    // `stat`）没有角色表可失败，元数据判定直接生效。
    let metadata = argv_roles::is_metadata_access(argv)
        && (paths.is_some() || !argv_roles::is_modeled_command(argv));
    let sensitive = match paths {
        // 识别出参数角色：只检查命令真正会打开的值。
        Some(paths) => paths
            .into_iter()
            .any(|arg| path_argument_is_sensitive(arg, cwd, metadata, placement.access(arg))),
        // 未知命令或语法：退回逐参数检查。
        None => argv.iter().skip(1).any(|arg| {
            if arg.starts_with('-') && !arg.contains('/') {
                return false;
            }
            path_argument_is_sensitive(arg, cwd, metadata, placement.access(arg))
        }),
    };
    sensitive || recursive_search_covers_sensitive_path(argv, cwd)
}

/// 只读命令里能定位、且落在项目 `.kloop` 的参数，原文层不再按名字筛：它们已由
/// 路径判定证明不是私有状态根。`echo`/`printf` 的文本不算——别的命令可能换个目录
/// 重新解释它；嵌套 `-c` 的词范围属于它自己的源字符串，也不算。
fn mask_project_kloop_words(
    masked: &mut String,
    facts: &BashFacts,
    cwd: &Path,
    home: Option<&Path>,
) {
    let mut mask = |range: &std::ops::Range<usize>, word: &WordEvaluation, placement: Placement| {
        let (WordEvaluation::Resolved(spelling) | WordEvaluation::Partial(spelling)) = word else {
            return;
        };
        if spelling.contains('\0') || !fs_fold(spelling).contains(sensitive::KLOOP_DIR) {
            return;
        }
        let access = placement.access(spelling);
        if access == Access::Read && !path_argument_is_sensitive(spelling, cwd, false, access) {
            masked.replace_range(range.clone(), &" ".repeat(range.len()));
        }
    };
    for command in &facts.commands {
        if command.context.script_depth != 0 || arguments_are_text(command) {
            continue;
        }
        let home = home.filter(|_| !command.context.home_unknown);
        let words: Vec<_> = command
            .words
            .iter()
            .map(|word| word.syntax.evaluate(home))
            .collect();
        let placement = Placement::of_command(&placeholder_argv(&words), &command.context);
        for (word, evaluation) in command.words.iter().zip(&words).skip(1) {
            mask(&word.range, evaluation, placement);
        }
    }
    for redirect in &facts.redirects {
        if redirect.direction != Direction::Read || redirect.context.script_depth != 0 {
            continue;
        }
        let home = home.filter(|_| !redirect.context.home_unknown);
        mask(
            &redirect.range,
            &redirect.syntax.evaluate(home),
            Placement::of_shell(&redirect.context),
        );
    }
}

fn bash_reads_sensitive_path(
    command: &str,
    facts: &BashFacts,
    cwd: &Path,
    execution: ExecutionContext<'_>,
) -> ReadVerdict {
    let home = execution.shell_home();
    let mut masked = command.to_string();
    if facts.reliable {
        mask_project_kloop_words(&mut masked, facts, cwd, home.as_deref());
    }
    let raw_sensitive = raw_mentions_sensitive_path(&masked);
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
        for command in &facts.commands {
            if command.context.argument_rewrite
                && !arguments_are_text(command)
                && !matches!(command.words.first().map(|word| &word.value),
                    Some(WordValue::Literal(name)) if name.rsplit('/').next() == Some("rm"))
            {
                // 替换可能生成敏感读取或新的读取选项，不能按替换前的拼写排除。
                // rm 只删除目录项；参数未知仍触发危险判定，不因此变成内容读取。
                return ReadVerdict::Unresolved;
            }
            let home = home.as_deref().filter(|_| !command.context.home_unknown);
            let words: Vec<_> = command
                .words
                .iter()
                .map(|word| word.syntax.evaluate(home))
                .collect();
            let placement = Placement::of_command(&placeholder_argv(&words), &command.context);
            // 不删除未知参数：角色表只有在每个位置都有完整拼写时才能使用。
            let argv: Option<Vec<_>> = words
                .iter()
                .map(|word| match word {
                    WordEvaluation::Resolved(text) => Some(text.clone()),
                    WordEvaluation::Partial(_) | WordEvaluation::Unknown => None,
                })
                .collect();
            if let Some(argv) = argv {
                if argv_reads_sensitive_path(&argv, cwd, placement) {
                    return ReadVerdict::Sensitive;
                }
            } else if !arguments_are_text(command)
                && words.iter().skip(1).any(|word| {
                    evaluation_is_sensitive(word, cwd, false, |spelling| placement.access(spelling))
                })
            {
                return ReadVerdict::Sensitive;
            }
        }
        for redirect in &facts.redirects {
            if redirect.direction != Direction::Read {
                continue;
            }
            let home = home.as_deref().filter(|_| !redirect.context.home_unknown);
            let target = redirect.syntax.evaluate(home);
            let placement = Placement::of_shell(&redirect.context);
            if evaluation_is_sensitive(&target, cwd, false, |spelling| placement.access(spelling)) {
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
    // 词法归一化先消掉 `..`，可能跨过了符号链接；再按物理落点判一次，只会多拦。
    let physical = Path::new(raw)
        .components()
        .any(|component| matches!(component, Component::ParentDir))
        .then(|| canonicalize_nearest_existing(&cwd.join(raw)))
        .flatten();
    let sensitivity = [
        PathFacts::gather(&path, cwd).sensitivity(access),
        glob_spelling_sensitivity(&path, metadata, access),
        physical.and_then(|physical| path_sensitivity(&physical, access)),
    ]
    .into_iter()
    .fold(None, merge_sensitivity);
    match sensitivity {
        None => false,
        // 凭据和 agent state 连名字都算敏感，元数据访问也不例外。
        Some(Sensitivity::Secret) => true,
        // `.git` 的元数据（stat、列目录）不是内容读取；内容访问仍是硬边界。
        Some(Sensitivity::Vcs) => !metadata,
    }
}

/// `access_of` 按拼写给出访问口径：读取看它所在命令能否定位，写入不分。
fn evaluation_is_sensitive(
    word: &WordEvaluation,
    cwd: &Path,
    metadata: bool,
    access_of: impl Fn(&str) -> Access,
) -> bool {
    match word {
        WordEvaluation::Resolved(path) => {
            path_argument_is_sensitive(path, cwd, metadata, access_of(path))
        }
        WordEvaluation::Partial(spelling) if !spelling.contains('\0') => {
            path_argument_is_sensitive(spelling, cwd, metadata, access_of(spelling))
        }
        WordEvaluation::Partial(spelling) => {
            // 未知片段不能假装成文件系统落点，但已知的敏感分量仍然成立。
            let access = match access_of(spelling) {
                Access::Read => Access::ReadUnlocated,
                other => other,
            };
            let path = Path::new(spelling);
            match merge_sensitivity(
                path_sensitivity(path, access),
                glob_spelling_sensitivity(path, metadata, access),
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
fn glob_spelling_sensitivity(path: &Path, metadata: bool, access: Access) -> Option<Sensitivity> {
    let components: Vec<_> = path.components().collect();
    let mut found = None;
    for (index, component) in components.iter().enumerate() {
        let Component::Normal(name) = component else {
            continue;
        };
        let Some(name) = name.to_str() else {
            continue;
        };
        let last = index + 1 == components.len();
        // 能拼出 `.kloop` 的分量与字面 `.kloop` 同一口径：worktree 豁免；读取时父目录下
        // 的 `.kloop` 能证明在项目里才放行。
        if sensitive::glob_component_may_be_kloop(name) {
            let worktree = matches!(components.get(index + 1), Some(Component::Normal(next))
                if fs_fold(&next.to_string_lossy()) == WORKTREE_SEGMENT);
            let parent: PathBuf = components[..index].iter().collect();
            let project = access == Access::Read
                && kloop_dir_is_elsewhere(&parent.join(sensitive::KLOOP_DIR));
            if !worktree && !project {
                return Some(Sensitivity::Secret);
            }
        }
        match sensitive::glob_component_sensitivity(name, last) {
            Some(Sensitivity::Secret) => return Some(Sensitivity::Secret),
            Some(Sensitivity::Vcs) => found = Some(Sensitivity::Vcs),
            None => {}
        }
    }
    found.filter(|_| !metadata)
}

/// The one exemption to `.kloop` being sensitive wherever it lives:
/// `.kloop/worktrees/<name>` holds checkouts the agent is *supposed* to edit
/// wholesale. Only this exact segment pair is structural — a nested `.kloop`
/// inside the worktree, and every other child of `.kloop`, stays sensitive
/// unless the read gate proves the directory is a project's.
const WORKTREE_SEGMENT: &str = "worktrees";

/// The second structural exemption to `.kloop` being sensitive: a spilled tool
/// result (`…/offload/off-0001.txt`) or a background shell's output
/// (`…/offload/bg-3.out`). These are the model's *own* output — it was handed a
/// head/tail preview and this exact path — so re-reading one discloses nothing a
/// tool would not have returned anyway, while refusing turns every oversized
/// result into a dead end. Nothing else in the store qualifies: the provider
/// credential is in `config.toml` and every project's transcript in `sessions/`,
/// and both stay sensitive.
fn is_spill_file_name(name: &str) -> bool {
    let Some((prefix, rest)) = name.split_once('-') else {
        return false;
    };
    let Some((id, extension)) = rest.rsplit_once('.') else {
        return false;
    };
    // 路径结构可以折叠大小写，Base58 身份不能：`L` 合法，`l` 不合法。
    matches!(
        (fs_fold(prefix).as_ref(), fs_fold(extension).as_ref()),
        ("off", "txt") | ("bg", "out")
    ) && crate::resource_id::is_suffix(id)
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
    fs_fold(parent).ends_with("/offload") && is_spill_file_name(name)
}

/// Neutralize the structurally exempt `.kloop` mentions token by token, leaving
/// every other mention for the needle scan: a spill token, and each
/// `/.kloop/worktrees/`. A nested `.kloop` later in the same token, or another
/// token, is still screened. A `..` in the token forfeits the worktree
/// exemption — `.kloop/worktrees/../sessions` must not walk out through it.
/// Project `.kloop` paths are not text-level facts; the read gate unmasks them
/// word by word once it has located them ([`mask_project_kloop_words`]).
/// Separators are preserved so the rebuilt string keeps the original shape.
fn mask_exempt_kloop_tokens(command: &str) -> String {
    const SEPARATORS: &[char] = &[
        ' ', '\t', '\n', '\r', '\'', '"', '`', '(', ')', ';', '|', '<', '>', '=', ',', '&', '{',
        '}',
    ];
    const MENTION: &str = "/.kloop/";
    let mask = |token: &str| {
        let spill = is_spill_path_token(token);
        let folded = fs_fold(token);
        let token = folded.as_ref();
        if spill {
            return token.replace(MENTION, "/kloop/");
        }
        if token.contains("..") {
            return token.to_string();
        }
        let mut out = String::with_capacity(token.len());
        let mut rest = token;
        while let Some(index) = rest.find(MENTION) {
            let after = &rest[index + MENTION.len()..];
            let entry = after.split('/').next().unwrap_or_default();
            out.push_str(&rest[..index]);
            out.push_str(if fs_fold(entry) == WORKTREE_SEGMENT {
                "/kloop/"
            } else {
                MENTION
            });
            rest = after;
        }
        out.push_str(rest);
        out
    };
    let mut out = String::with_capacity(command.len());
    let mut token = String::new();
    for ch in command.chars() {
        if SEPARATORS.contains(&ch) {
            out.push_str(&mask(&token));
            token.clear();
            out.push(ch);
        } else {
            token.push(ch);
        }
    }
    out.push_str(&mask(&token));
    out
}

fn raw_mentions_sensitive_path(command: &str) -> bool {
    mentions_sensitive_needle(&mask_exempt_kloop_tokens(command))
}

/// 文本层只认目录拼写：`~/name` 与 `/name/`，`.kloop` 也按名字认。文件名
/// （`.env*`、rc 文件）走路径与 argv 判定；把它们塞进子串扫描会让
/// `echo "add .env to .gitignore"` 这类纯文本也变成敏感读取。
fn mentions_sensitive_needle(command: &str) -> bool {
    sensitive::SECRET_DIRS
        .iter()
        .chain([&sensitive::KLOOP_DIR])
        .any(|dir| command.contains(&format!("~/{dir}")) || command.contains(&format!("/{dir}/")))
}

fn powershell_mentions_sensitive_path(command: &str) -> bool {
    let normalized = mask_exempt_kloop_tokens(&command.replace('\\', "/")).to_lowercase();
    let normalized = normalized.as_str();
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
    if path_sensitivity(&root, Access::Read).is_some() {
        return true;
    }
    // 子项检查只看一层；根在私有状态根之上时（`grep -r … /Users`）按位置补上。
    if private_state_roots()
        .iter()
        .any(|state| starts_with_folded(state, &root))
    {
        return true;
    }
    // `.git` 不在名单里：仓库内 `grep -r` 是日常操作，而 `.git` 的内容访问
    // 已由路径参数判定兜底。`.kloop` 子目录能证明在项目里（用过 worktree 的仓库）
    // 就放行。
    std::fs::read_dir(&root).is_ok_and(|entries| {
        entries.flatten().any(|entry| {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return true;
            };
            sensitive::name_is_secret(name)
                || (fs_fold(name) == sensitive::KLOOP_DIR && !kloop_dir_is_elsewhere(&entry.path()))
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
                        |_| Access::Write,
                    ),
                }),
                WordEvaluation::Partial(_) => Some(WriteTarget::Partial {
                    sensitive: evaluation_is_sensitive(&target, cwd, false, |_| Access::Write),
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

    /// 原拼写与规范目标任一落在名单里即算；`access` 只影响 `.kloop` 下的条目。
    pub(super) fn sensitivity(&self, access: Access) -> Option<Sensitivity> {
        merge_sensitivity(
            path_sensitivity(&self.original, access),
            path_sensitivity(&self.normalized, access),
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
    if !is_spill_file_name(name) {
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
/// credential store is a secret path for every access kind. A `.kloop` the read
/// gate can place outside the private state root is a project's and passes
/// whole; a `.kloop` or credential name nested inside it is judged again.
fn path_sensitivity(normalized: &Path, access: Access) -> Option<Sensitivity> {
    // Callers normalize first, but the exemptions below must not depend on that:
    // `.kloop/worktrees/../sessions` is only a worktree path if you stop reading
    // at the second segment. A `..` anywhere forfeits them entirely.
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
            // Skipping the `.kloop` segment (rather than returning false) keeps
            // the scan running over the rest, so a nested `.kloop` further down
            // still trips.
            let worktree = components.peek().is_some_and(|next| {
                matches!(next, Component::Normal(next)
                    if fs_fold(&next.to_string_lossy()) == WORKTREE_SEGMENT)
            });
            // The spill exemption reads the *tail* rather than the next segment,
            // because the store partitions by project between the two
            // (`.kloop/projects/v1/<id>/offload/off-0001.txt`).
            let exempt = !traversal && (worktree || path_tail_is_spill(normalized));
            let project = access == Access::Read && !traversal && kloop_dir_is_elsewhere(&prefix);
            if exempt || project {
                continue;
            }
            return Some(Sensitivity::Secret);
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

/// 这个 `.kloop` 目录能否证明不是私有状态根：真实存在，且与 `~/.kloop` 不是同一个
/// 文件（按 dev+inode 比，firmlink、bind mount 也认得出）。证明不了按状态根处理。
fn kloop_dir_is_elsewhere(dir: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(dir) else {
        return false;
    };
    if !metadata.is_dir() {
        return false;
    }
    let roots = private_state_roots();
    let Some(root) = roots.first() else {
        return false;
    };
    let spelled = roots.iter().any(|root| {
        dir.components().count() == root.components().count() && starts_with_folded(dir, root)
    });
    !spelled && !same_file(dir, &metadata, root)
}

#[cfg(unix)]
fn same_file(_: &Path, metadata: &std::fs::Metadata, other: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    std::fs::metadata(other)
        .is_ok_and(|other| metadata.dev() == other.dev() && metadata.ino() == other.ino())
}

#[cfg(not(unix))]
fn same_file(path: &Path, _: &std::fs::Metadata, other: &Path) -> bool {
    match (std::fs::canonicalize(path), std::fs::canonicalize(other)) {
        (Ok(path), Ok(other)) => path == other,
        _ => false,
    }
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
    fn spill_ids_are_checked_before_case_folding() {
        for name in ["off-aL23456789Z.txt", "bg-aL23456789Z.out"] {
            let path = format!("/home/u/.kloop/projects/v1/p1_test/offload/{name}");
            assert_eq!(
                (
                    raw_mentions_sensitive_path(&format!("tail -45 {path}")),
                    path_sensitivity(Path::new(&path), Access::ReadUnlocated),
                    powershell_mentions_sensitive_path(&format!("Get-Content {path}")),
                ),
                (false, None, false),
                "{path}"
            );
        }
    }

    #[test]
    fn spill_token_exemptions_keep_sensitive_neighbors_blocked() {
        let spill = "/home/u/.kloop/projects/v1/p1_test/offload/bg-aL23456789Z.out";
        for path in [
            "/home/u/.kloop/config.toml",
            "/home/u/.kloop/projects/v1/p1_test/sessions/session.jsonl",
            "/home/u/.kloop/projects/v1/p1_test/offload/bg-al23456789Z.out",
            "/home/u/.kloop/projects/v1/p1_test/offload/bg-aL234567890.out",
            "/home/u/.kloop/projects/v1/p1_test/offload/../../../config.toml",
        ] {
            assert_eq!(
                (
                    raw_mentions_sensitive_path(&format!("cat {spill} {path}")),
                    powershell_mentions_sensitive_path(&format!("Get-Content {spill}, {path}")),
                ),
                (true, true),
                "{path}"
            );
        }
    }

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
        let s = |p: &str| path_sensitivity(Path::new(p), Access::Write).is_some();
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

    /// 读取时能证明不是私有状态根的 `.kloop`（项目里的）整个放行；证明不了的、状态根
    /// 本身、落点不可信的拼写都按名字算。写入不分位置，只豁免 worktree。
    #[test]
    fn project_kloop_reads_pass_only_when_placed() {
        let project = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan271-project-").expect("fresh id"));
        std::fs::create_dir_all(project.join(".kloop/rules")).unwrap();
        let home = std::env::home_dir().expect("home");
        let read = |path: &Path| path_sensitivity(path, Access::Read);
        let secret = Some(Sensitivity::Secret);

        assert_eq!(read(&project.join(".kloop/rules/style.md")), None);
        assert_eq!(read(&project.join(".kloop/config.toml")), None);
        assert_eq!(read(&project.join(".kloop")), None);
        // 项目 `.kloop` 里的凭据名、嵌套的 `.kloop` 照旧再判。
        assert_eq!(read(&project.join(".kloop/rules/.env")), secret);
        assert_eq!(
            read(&project.join(".kloop/rules/.kloop/config.toml")),
            secret
        );
        for path in [
            home.join(".kloop/config.toml"),
            home.join(".kloop/skills/x/SKILL.md"),
            home.join(".kloop"),
            // 不存在的目录无从证明。
            PathBuf::from("/w/proj/.kloop/rules/style.md"),
        ] {
            assert_eq!(read(&path), secret, "{}", path.display());
        }
        assert_eq!(
            read(&home.join(".KLOOP/config.toml")).is_some(),
            FS_FOLDS_CASE
        );
        let rules = project.join(".kloop/rules/style.md");
        assert_eq!(path_sensitivity(&rules, Access::ReadUnlocated), secret);
        assert_eq!(path_sensitivity(&rules, Access::Write), secret);
        assert_eq!(
            path_sensitivity(&project.join(".kloop/worktrees/wt/x"), Access::Write),
            None
        );

        let glob = |path: &Path, access| glob_spelling_sensitivity(path, false, access);
        assert_eq!(glob(&project.join(".k*/rules/x"), Access::Read), None);
        assert_eq!(glob(&project.join(".k*/rules/x"), Access::Write), secret);
        assert_eq!(glob(&home.join(".k*/skills"), Access::Read), secret);
        assert_eq!(glob(Path::new("/w/proj/.k*/rules"), Access::Read), secret);
        let _ = std::fs::remove_dir_all(&project);
    }

    /// 别名落回私有状态根时照拦：规范路径与文件身份都指向它。
    #[cfg(unix)]
    #[test]
    fn aliases_of_the_private_state_root_stay_secret() {
        let home = std::env::home_dir().expect("home");
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan271-alias-").expect("fresh id"));
        std::fs::create_dir_all(&root).unwrap();
        std::os::unix::fs::symlink(&home, root.join("h")).unwrap();
        let facts = PathFacts::gather(&root.join("h/.kloop/config.toml"), &root);
        assert_eq!(facts.sensitivity(Access::Read), Some(Sensitivity::Secret));
        assert!(!kloop_dir_is_elsewhere(&root.join("h/.kloop")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 词法归一化先消掉 `..`，可能跨过符号链接；Bash 拼写再按物理落点判一次。
    #[cfg(unix)]
    #[test]
    fn traversal_through_a_symlink_is_judged_physically() {
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan271-traversal-").expect("fresh id"));
        std::fs::create_dir_all(root.join("vault/.ssh/keys")).unwrap();
        std::fs::write(root.join("vault/.ssh/id_rsa"), "key").unwrap();
        std::os::unix::fs::symlink(root.join("vault/.ssh/keys"), root.join("kx")).unwrap();
        let raw = root.join("kx/../id_rsa");
        let raw = raw.to_str().unwrap();
        assert!(path_argument_is_sensitive(raw, &root, false, Access::Read));
        // `..` 本身也让拼写失去定位。
        assert_eq!(Placement::Trusted.access(raw), Access::ReadUnlocated);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 递归根下的 `.kloop` 子目录能证明在项目里就放行；根包住私有状态根时照拦。
    #[test]
    fn recursive_roots_place_their_kloop() {
        let root = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan271-recursive-").expect("fresh id"));
        std::fs::create_dir_all(root.join(".kloop/worktrees")).unwrap();
        std::fs::write(root.join(".kloop/config.toml"), "x").unwrap();
        let search = |target: &Path| {
            let argv = [
                "grep".to_string(),
                "-rn".into(),
                "KEY".into(),
                target.display().to_string(),
            ];
            recursive_search_covers_sensitive_path(&argv, &root)
        };
        assert!(!search(&root));
        assert!(!search(&root.join(".kloop")));
        let home = std::env::home_dir().expect("home");
        assert!(search(&home));
        assert!(search(home.parent().expect("home has a parent")));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 名单只有一份：同一组名字在文本扫描、路径判定、递归搜索与沙箱凭据根里
    /// 给出相同答案。递归搜索漏掉 `.env*` 就是名单分家的结果。
    #[test]
    fn sensitive_names_agree_across_every_caller() {
        let scopes = [Access::Read, Access::Write];
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
        let read = |raw: &str| path_argument_is_sensitive(raw, cwd, false, Access::Read);
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
        let s = |p: &str| path_sensitivity(Path::new(p), Access::Write).is_some();
        // A cased alias resolves to the real `.git`/`.ssh`/`.env*` on a
        // case-folding FS, so it must be caught there; on a case-sensitive FS
        // those are genuinely distinct paths and stay non-sensitive.
        assert_eq!(s("/w/proj/.GIT/hooks/pre-commit"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.SSH/id_rsa"), FS_FOLDS_CASE);
        assert_eq!(s("/w/proj/.Env.production"), FS_FOLDS_CASE);
        assert_eq!(s("/home/u/.Bashrc"), FS_FOLDS_CASE);
    }
}
