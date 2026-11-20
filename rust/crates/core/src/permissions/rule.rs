//! 权限规则语言、解析与两个方向的匹配。

use std::path::Path;

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use globset::GlobMatcher;

use super::facts::CallFacts;
use super::facts::FS_FOLDS_CASE;
use super::facts::PathFacts;
use super::facts::ShellFacts;
use super::facts::WriteTarget;
use crate::shell::BashAnalysis;
use crate::shell::WordValue;
use crate::shell::argv_is_readonly;
use crate::shell::risk_wrapper_views;

#[derive(Clone, Debug)]
pub(super) enum Rule {
    Tool(String),
    BashPrefix {
        tokens: Vec<String>,
        wildcard: bool,
    },
    PathGlob {
        tool: String,
        glob: GlobMatcher,
    },
    /// Consent to re-run one bash command *outside* the OS sandbox after it was
    /// denied inside it. Deliberately its own variant rather than a `bash(...)`
    /// rule: allowing a command to run is not the same permission as allowing it
    /// to run uncontained, so neither form may stand in for the other. It never
    /// matches the ordinary gate (`matches_argv` below) and `bash`/`Tool` rules
    /// never match an escalation.
    SandboxEscalatePrefix {
        tokens: Vec<String>,
        wildcard: bool,
    },
    /// `sandbox_network(<program>)`: a contained bash call may reach the
    /// network when every segment with an effect runs a program so named
    /// (plan 283). Writes stay contained; nothing else is vouched for.
    SandboxNetwork {
        program: String,
    },
    /// `sandbox_write(<absolute dir>)`: a writable root added to every
    /// contained bash call in the project (plan 283) — a wider sandbox, not an
    /// escape from it.
    SandboxWrite {
        dir: std::path::PathBuf,
    },
}

/// The two directions of one path-glob rule. Allow only trusts the canonical
/// target (`**` crosses `/`, so a relative glob cannot be aimed at absolute
/// spellings either); deny/ask also match every alias the call could mean.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PathDirection {
    Allows,
    Blocks,
}

/// Allow 方向的路径判定只有一条：绝对 glob 对规范绝对目标，相对 glob 只对 cwd 内的
/// 规范相对目标。`**` 能跨 `/`，所以相对 glob 不能顺手拿去匹配绝对路径。
fn glob_allows_path(glob: &GlobMatcher, facts: &PathFacts) -> bool {
    if !facts.resolved {
        return false;
    }
    if Path::new(glob.glob().glob()).is_absolute() {
        glob.is_match(&facts.normalized)
    } else {
        facts
            .relative
            .as_deref()
            .is_some_and(|path| glob.is_match(path))
    }
}

impl Rule {
    fn matches_tool(&self, name: &str) -> bool {
        matches!(self, Rule::Tool(t) if t == name)
    }

    /// Whether this rule vouches for one parsed bash argv.
    fn matches_argv(&self, argv: &[String]) -> bool {
        match self {
            Rule::Tool(t) => t == "bash",
            Rule::BashPrefix { tokens, wildcard } => argv_has_prefix(tokens, *wildcard, argv),
            Rule::PathGlob { .. }
            | Rule::SandboxEscalatePrefix { .. }
            | Rule::SandboxNetwork { .. }
            | Rule::SandboxWrite { .. } => false,
        }
    }

    /// Whether this rule vouches for re-running one parsed bash argv without the
    /// sandbox. Only the dedicated variant can: a whole-tool `bash` rule or a
    /// `bash(<prefix>)` rule says the call may run, not that it may run
    /// uncontained.
    pub(super) fn matches_escalation_argv(&self, argv: &[String]) -> bool {
        match self {
            Rule::SandboxEscalatePrefix { tokens, wildcard } => {
                argv_has_prefix(tokens, *wildcard, argv)
            }
            Rule::Tool(_)
            | Rule::BashPrefix { .. }
            | Rule::PathGlob { .. }
            | Rule::SandboxNetwork { .. }
            | Rule::SandboxWrite { .. } => false,
        }
    }

    /// The one path-glob matcher. `bash_write`, `write_file` allow and the
    /// deny/ask layers used to be separate implementations of the same
    /// question, which is how their spellings drifted apart.
    pub(super) fn matches_path(
        &self,
        tool: &str,
        facts: &PathFacts,
        direction: PathDirection,
    ) -> bool {
        let Rule::PathGlob {
            tool: rule_tool,
            glob,
        } = self
        else {
            // deny/ask 的整工具规则也拦这类路径（`read_path_blocked` 依赖它）；
            // allow 方向的整工具规则由调用方单独处理，因为 bash_write 没有这种写法。
            return direction == PathDirection::Blocks
                && matches!(self, Rule::Tool(t) if t == tool);
        };
        if rule_tool != tool {
            return false;
        }
        match direction {
            PathDirection::Allows => glob_allows_path(glob, facts),
            PathDirection::Blocks => {
                facts
                    .relative
                    .as_ref()
                    .is_some_and(|rel| glob.is_match(rel))
                    || facts
                        .original_relative
                        .as_ref()
                        .is_some_and(|rel| glob.is_match(rel))
                    || facts
                        .deny_alias_relative
                        .as_ref()
                        .is_some_and(|rel| glob.is_match(rel))
                    || glob.is_match(&facts.normalized)
                    || glob.is_match(&facts.original)
                    || facts
                        .deny_alias
                        .as_ref()
                        .is_some_and(|path| glob.is_match(path))
            }
        }
    }

    /// `bash_write` has no whole-tool form: the rule names files, and a
    /// `bash_write` rule is the only spelling that reaches this call.
    pub(super) fn matches_shell_write(&self, tool: &str, facts: &PathFacts) -> bool {
        self.matches_path(tool, facts, PathDirection::Allows)
    }

    /// allow 只认规范目标：原文别名可能与符号链接父目录的落点不同，不能凭它把
    /// 外面的写入带进这条授权。
    pub(super) fn matches_allow_path(&self, tool: &str, facts: &PathFacts) -> bool {
        match self {
            Rule::Tool(t) => t == tool,
            Rule::PathGlob { .. } => self.matches_path(tool, facts, PathDirection::Allows),
            Rule::BashPrefix { .. }
            | Rule::SandboxEscalatePrefix { .. }
            | Rule::SandboxNetwork { .. }
            | Rule::SandboxWrite { .. } => false,
        }
    }
}

/// Whether `argv` matches an argv-prefix pattern: the whole command when the
/// rule was written without a trailing `*`, just its head when it had one.
/// `bash(...)` and `sandbox_escalate(...)` are the same shape over two
/// different consents, so the shape itself lives in one place.
fn argv_has_prefix(tokens: &[String], wildcard: bool, argv: &[String]) -> bool {
    let length_matches = if wildcard {
        argv.len() >= tokens.len()
    } else {
        argv.len() == tokens.len()
    };
    length_matches && argv.iter().zip(tokens).all(|(a, t)| a == t)
}

/// deny/ask 方向：`prefix` 之后跟着未知词，未加引号时可能展开成任意个词（含零个），
/// 所以只要已知前缀与规则在公共长度上一致，规则就可能命中。`git $X push` 对
/// `git push *` 算命中；`git status "$X"` 不算。只用于有未决读取的调用（plan 284 前
/// 它们整条硬拒）；`rm "$f"` 这类原本就放行的调用沿用原匹配，不因此新增拒绝。
fn prefix_may_match(tokens: &[String], wildcard: bool, prefix: &[String]) -> bool {
    (wildcard || prefix.len() <= tokens.len()) && prefix.iter().zip(tokens).all(|(a, t)| a == t)
}

/// `<tokens…>` or `<tokens…> *` — the pattern both argv-prefix rules are
/// written in. `tool` only names the rule in the error.
fn parse_prefix_pattern(tool: &str, inner: &str) -> Result<(Vec<String>, bool)> {
    let mut tokens: Vec<String> = inner.split_whitespace().map(str::to_string).collect();
    let wildcard = tokens.last().is_some_and(|t| t == "*");
    if wildcard {
        tokens.pop();
    }
    if tokens.is_empty() {
        bail!("rule '{tool}({inner})': empty command pattern");
    }
    Ok((tokens, wildcard))
}

pub(super) fn parse_rule(entry: &str) -> Result<Rule> {
    if let Some((tool, inner)) = entry
        .split_once('(')
        .and_then(|(t, rest)| rest.strip_suffix(')').map(|inner| (t, inner)))
    {
        match tool {
            "bash" => {
                let (tokens, wildcard) = parse_prefix_pattern(tool, inner)?;
                Ok(Rule::BashPrefix { tokens, wildcard })
            }
            "sandbox_escalate" => {
                let (tokens, wildcard) = parse_prefix_pattern(tool, inner)?;
                // 升级的键跳过只读段，以只读命令开头的规则永远命不中；旧文件里的这类规则直接判非法，
                // 不再保留可读（2026-11-20 用户定：不考虑兼容）。
                if crate::shell::readonly_whatever_the_arguments(&tokens[0]) {
                    bail!(
                        "rule '{entry}': sandbox_escalate never matches a read-only command such as '{}'",
                        tokens[0]
                    );
                }
                Ok(Rule::SandboxEscalatePrefix { tokens, wildcard })
            }
            "sandbox_network" => {
                let program = inner.trim();
                if program.is_empty()
                    || program
                        .chars()
                        .any(|c| c.is_whitespace() || matches!(c, '/' | '*' | '?' | '[' | ','))
                {
                    bail!("rule '{entry}': sandbox_network takes one program name");
                }
                Ok(Rule::SandboxNetwork {
                    program: program.to_string(),
                })
            }
            "sandbox_write" => {
                let dir = std::path::Path::new(inner);
                // 授权生效时逐字比对解析后的物理路径（plan 287），尾斜杠、`.`、`..`、`//` 这类写法
                // 永远对不上；直接判非法，而不是读进来再在使用时丢掉。
                let spelled_canonically = !inner.ends_with('/')
                    && !inner.contains("//")
                    && inner
                        .split('/')
                        .skip(1)
                        .all(|component| !matches!(component, "." | ".."));
                if !dir.is_absolute()
                    || dir.parent().is_none()
                    || !spelled_canonically
                    || inner.contains(['*', '?', '[', ']', '{', '}', '\n', '\r'])
                {
                    bail!(
                        "rule '{entry}': sandbox_write takes one absolute directory other than /, spelled without glob characters, '.', '..', '//' or a trailing '/'"
                    );
                }
                Ok(Rule::SandboxWrite {
                    dir: dir.to_path_buf(),
                })
            }
            "powershell" => {
                bail!(
                    "rule '{entry}': powershell(...) prefix rules are unsupported; PowerShell v1 only accepts the whole-tool rule 'powershell'"
                )
            }
            "write_file"
            | "edit_file"
            | "read_file"
            | "notebook_edit"
            | "bash_write"
            | "sandbox_escalate_write" => {
                // Match case-insensitively on case-folding filesystems so a
                // deny like `write_file(secrets/**)` is not slipped by `Secrets/`.
                // Shell 目录名中的前导空白属于路径，不能裁掉后再写成另一条授权。
                let pattern = if matches!(tool, "bash_write" | "sandbox_escalate_write") {
                    inner
                } else {
                    inner.trim()
                };
                let glob = globset::GlobBuilder::new(pattern)
                    .case_insensitive(FS_FOLDS_CASE)
                    .build()
                    .with_context(|| format!("rule '{entry}': invalid glob"))?
                    .compile_matcher();
                Ok(Rule::PathGlob {
                    tool: tool.to_string(),
                    glob,
                })
            }
            other => bail!("rule '{entry}': tool '{other}' does not take a pattern"),
        }
    } else if !entry.is_empty() && entry.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        Ok(Rule::Tool(entry.to_string()))
    } else {
        bail!("rule '{entry}': expected a tool name, bash(<tokens>), or <file_tool>(<glob>)");
    }
}

/// 一条授权规则为什么非法，说明里不复述规则内容：`bash(...)` 一类规则可能带着命令参数里的密钥，
/// 而这段说明会显示给用户（项目权限文件加载失败的提示）。只点出已知的工具名，以及
/// `sandbox_escalate` 开头那个只读命令的名字（固定的命令名，不是参数）。合法或空白时返回 `None`。
pub fn invalid_rule_reason(entry: &str) -> Option<String> {
    let entry = entry.trim();
    if entry.is_empty() || parse_rule(entry).is_ok() {
        return None;
    }
    const PATTERN_TOOLS: &[&str] = &[
        "bash",
        "sandbox_escalate",
        "sandbox_network",
        "sandbox_write",
        "powershell",
        "write_file",
        "edit_file",
        "read_file",
        "notebook_edit",
        "bash_write",
        "sandbox_escalate_write",
    ];
    let (tool, inner) = match entry.split_once('(') {
        Some((tool, rest)) if PATTERN_TOOLS.contains(&tool) => (Some(tool), rest),
        _ => (None, ""),
    };
    Some(match tool {
        Some("sandbox_escalate") => {
            let head = inner
                .split_whitespace()
                .next()
                .map(|head| head.rsplit('/').next().unwrap_or(head))
                .filter(|head| crate::shell::readonly_whatever_the_arguments(head));
            match head {
                Some(head) => format!(
                    "`sandbox_escalate({head} …)` never matches: an escalation rule cannot start with a read-only command such as `{head}`"
                ),
                None => "`sandbox_escalate(…)` is not a valid command prefix".to_string(),
            }
        }
        Some("sandbox_write") => "`sandbox_write(…)` must name one absolute directory other than `/`, spelled without glob characters, `.`, `..`, `//` or a trailing `/`".to_string(),
        Some(tool) => format!("`{tool}(…)` is not a valid rule"),
        None => "not a valid rule".to_string(),
    })
}

pub(super) fn parse_rules(entries: &[String]) -> Result<Vec<Rule>> {
    entries
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(parse_rule)
        .collect()
}

pub(super) fn allow_rules_match(allow: &[Rule], name: &str, call: &CallFacts) -> bool {
    match (&call.shell, &call.path) {
        (
            Some(ShellFacts::Bash(
                BashAnalysis::Commands(commands) | BashAnalysis::WithWrites { commands, .. },
            )),
            _,
        ) => {
            commands.iter().all(|argv| {
                argv_is_readonly(argv) || allow.iter().any(|rule| rule.matches_argv(argv))
            }) && call.writes.iter().all(|path| {
                allow
                    .iter()
                    .any(|rule| rule.matches_shell_write("bash_write", path))
            })
        }
        // Nothing vouches for a script the parser could not read, whole-tool
        // `bash` included: that rule was written without anyone having seen
        // this text, and no layer below can vet its content.
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => false,
        (Some(ShellFacts::PowerShellOpaque), _) => allow.iter().any(|rule| rule.matches_tool(name)),
        (None, Some(path)) => allow.iter().any(|rule| rule.matches_allow_path(name, path)),
        (None, None) => allow.iter().any(|rule| rule.matches_tool(name)),
    }
}

/// Deny/ask matching is the aggressive direction: bash argv are matched
/// after wrapper stripping so `sudo rm` / `env FOO=1 rm` cannot dodge a
/// `bash(rm *)` rule, and any single matching segment hits.
pub(super) fn rules_hit(rules: &[Rule], name: &str, call: &CallFacts) -> bool {
    if rules.iter().any(|r| r.matches_tool(name))
        || call.risk_writes.iter().any(|target| {
            rules.iter().any(|rule| match target {
                WriteTarget::Resolved(path) => {
                    rule.matches_path("bash_write", path, PathDirection::Blocks)
                }
                // 未知目标无法证明与显式规则不相交；allow 不消费这类事实。
                WriteTarget::Partial { .. } | WriteTarget::Unknown => matches!(rule,
                    Rule::Tool(tool) | Rule::PathGlob { tool, .. } if tool == "bash_write"),
            })
        })
    {
        return true;
    }
    if call.risk_commands.iter().any(|command| {
        if command.context.argument_rewrite {
            // 被替换的命令名可能展开成任意执行体；已知命令的未知参数则保留位置。
            return rules.iter().any(|rule| match rule {
                Rule::BashPrefix { tokens, wildcard } => {
                    matches!(
                        command.words.first().map(|word| &word.value),
                        Some(WordValue::Unknown)
                    ) || ((*wildcard && command.words.len() >= tokens.len()
                        || !*wildcard && command.words.len() == tokens.len())
                        && command.words.iter().zip(tokens).all(|(word, token)| {
                            match &word.value {
                                WordValue::Literal(value) => value == token,
                                WordValue::Unknown => true,
                            }
                        }))
                }
                Rule::Tool(_)
                | Rule::PathGlob { .. }
                | Rule::SandboxEscalatePrefix { .. }
                | Rule::SandboxNetwork { .. }
                | Rule::SandboxWrite { .. } => false,
            });
        }
        if let Some(argv) = command.argv() {
            return rules.iter().any(|rule| {
                rule.matches_argv(&argv)
                    || risk_wrapper_views(&argv)
                        .iter()
                        .any(|view| rule.matches_argv(view))
            });
        }
        let argv = command.risk_argv();
        // 循环绑定的词在 value 视图里是未知词，包装器却按 syntax 剥离：
        // `for w in rm; do env "$w" -rf x; done` 只在 syntax 视图里是 `rm`。deny/ask 两个视图都看。
        let bound = command.bound_risk_argv();
        rules.iter().any(|rule| {
            [&argv, &bound].into_iter().any(|argv| {
                rule.matches_argv(argv)
                    || risk_wrapper_views(argv)
                        .iter()
                        .any(|view| rule.matches_argv(view))
            }) || matches!(rule, Rule::BashPrefix { tokens, wildcard }
            if call.unresolved_read && std::iter::once(bound.clone())
                .chain(risk_wrapper_views(&bound))
                .any(|view| {
                    let known = view.iter().take_while(|word| !word.contains('\0')).count();
                    known < view.len() && prefix_may_match(tokens, *wildcard, &view[..known])
                }))
        })
    }) {
        return true;
    }
    match (&call.shell, &call.path) {
        (
            Some(ShellFacts::Bash(
                BashAnalysis::Commands(cmds) | BashAnalysis::WithWrites { commands: cmds, .. },
            )),
            _,
        ) => cmds.iter().any(|argv| {
            rules.iter().any(|r| {
                r.matches_argv(argv)
                    || risk_wrapper_views(argv)
                        .iter()
                        .any(|view| r.matches_argv(view))
            })
        }),
        // Opaque scripts cannot be inspected, so content-level Bash/PowerShell
        // rules never decide here. Whole-tool rules were handled above.
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => false,
        (Some(ShellFacts::PowerShellOpaque), _) => false,
        (None, Some(path)) => rules
            .iter()
            .any(|r| r.matches_path(name, path, PathDirection::Blocks)),
        (None, None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// "May run" and "may run outside the sandbox" are different permissions,
    /// so neither rule form may stand in for the other.
    #[test]
    fn escalation_rules_and_bash_rules_do_not_substitute_for_each_other() {
        let argv = vec!["go".to_string(), "test".to_string(), "./pkg".to_string()];
        let escalate = parse_rule("sandbox_escalate(go test *)").unwrap();
        let bash = parse_rule("bash(go test *)").unwrap();
        let whole_tool = parse_rule("bash").unwrap();

        assert!(escalate.matches_escalation_argv(&argv));
        assert!(
            !bash.matches_escalation_argv(&argv),
            "bash(...) is not consent to uncontained"
        );
        assert!(
            !whole_tool.matches_escalation_argv(&argv),
            "the whole-tool bash rule is not consent to uncontained either"
        );

        assert!(bash.matches_argv(&argv));
        assert!(
            !escalate.matches_argv(&argv),
            "escalation consent does not let the command past the ordinary gate"
        );
        assert!(!escalate.matches_tool("bash"));
    }

    #[test]
    fn powershell_prefix_rules_are_rejected_explicitly() {
        let error = parse_rule("powershell(Get-ChildItem *)")
            .unwrap_err()
            .to_string();
        assert!(error.contains("PowerShell v1"), "{error}");
        assert!(parse_rule("powershell").is_ok());
    }

    /// 非法规则的说明不复述规则原文，只点出已知工具名和 `sandbox_escalate` 开头的只读命令名；
    /// 合法规则和空白没有说明。
    #[test]
    fn invalid_rule_reasons_name_the_problem_without_the_rule() {
        for valid in [
            "bash(cargo test *)",
            "sandbox_write(/opt/out)",
            "web_search",
            "  ",
        ] {
            assert_eq!(invalid_rule_reason(valid), None, "{valid}");
        }
        assert_eq!(
            invalid_rule_reason("sandbox_escalate(/bin/echo token-123 *)").as_deref(),
            Some(
                "`sandbox_escalate(echo …)` never matches: an escalation rule cannot start with a read-only command such as `echo`"
            )
        );
        for (entry, reason) in [
            (
                "sandbox_escalate()",
                "`sandbox_escalate(…)` is not a valid command prefix",
            ),
            (
                "write_file([secret-glob)",
                "`write_file(…)` is not a valid rule",
            ),
            ("sk_live_secret(x)", "not a valid rule"),
            ("sentinel-secret(", "not a valid rule"),
        ] {
            let got = invalid_rule_reason(entry).unwrap();
            assert_eq!(got, reason, "{entry}");
            assert!(!got.contains("secret") && !got.contains("token"), "{got}");
        }
    }

    #[test]
    fn rule_parsing_accepts_valid_and_rejects_malformed() {
        assert!(parse_rule("write_file").is_ok());
        assert!(parse_rule("bash(cargo *)").is_ok());
        assert!(parse_rule("bash(git push origin)").is_ok());
        assert!(parse_rule("write_file(src/**)").is_ok());
        assert!(parse_rule("read_file(**/*.pem)").is_ok());

        assert!(parse_rule("bash()").is_err());
        assert!(parse_rule("bash(*)").is_err());
        assert!(parse_rule("task(x)").is_err());
        assert!(parse_rule("write file").is_err());
        // Plan 145 took the verbatim-script rule back out: an opaque script is
        // remembered for the session or not at all, so nothing durable may
        // spell one — including a store written by an older build.
        assert!(parse_rule("bash_script(cat > f <<EOF)").is_err());
        assert!(parse_rule("bash_script_no_sandbox(cat > f)").is_err());
    }

    /// 没有可信物理落点时，allow 不能拿词法拼写顶替；deny 仍看原拼写。
    #[cfg(unix)]
    #[test]
    fn plan245_unresolved_targets_never_match_an_allow_glob() {
        let base = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan245-rule-").expect("fresh id"));
        std::fs::create_dir_all(&base).unwrap();
        let base = std::fs::canonicalize(base).unwrap();
        std::os::unix::fs::symlink(base.join("missing"), base.join("broken")).unwrap();

        let facts = PathFacts::gather(Path::new("broken"), &base);
        assert!(!facts.resolved);
        let allow = parse_rule(&format!("write_file({}/**)", base.display())).unwrap();
        assert!(!allow.matches_allow_path("write_file", &facts));
        assert!(allow.matches_path("write_file", &facts, PathDirection::Blocks));
        std::fs::remove_dir_all(&base).unwrap();
    }
}
