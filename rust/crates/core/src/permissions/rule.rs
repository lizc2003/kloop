//! 权限规则语言、解析与两个方向的匹配。

use anyhow::Context as _;
use anyhow::Result;
use anyhow::bail;
use globset::GlobMatcher;

use super::facts::CallFacts;
use super::facts::FS_FOLDS_CASE;
use super::facts::PathFacts;
use super::facts::ShellFacts;
use crate::shell::BashAnalysis;
use crate::shell::argv_is_readonly;
use crate::shell::strip_wrappers;

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
            Rule::PathGlob { .. } | Rule::SandboxEscalatePrefix { .. } => false,
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
            Rule::Tool(_) | Rule::BashPrefix { .. } | Rule::PathGlob { .. } => false,
        }
    }

    pub(super) fn matches_path(&self, tool: &str, facts: &PathFacts) -> bool {
        match self {
            Rule::Tool(t) => t == tool,
            Rule::PathGlob {
                tool: rule_tool,
                glob,
            } => {
                rule_tool == tool
                    && (facts
                        .relative
                        .as_ref()
                        .is_some_and(|rel| glob.is_match(rel))
                        || facts
                            .original_relative
                            .as_ref()
                            .is_some_and(|rel| glob.is_match(rel))
                        || glob.is_match(&facts.normalized)
                        || glob.is_match(&facts.original))
            }
            Rule::BashPrefix { .. } | Rule::SandboxEscalatePrefix { .. } => false,
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
                Ok(Rule::SandboxEscalatePrefix { tokens, wildcard })
            }
            "powershell" => {
                bail!(
                    "rule '{entry}': powershell(...) prefix rules are unsupported; PowerShell v1 only accepts the whole-tool rule 'powershell'"
                )
            }
            "write_file" | "edit_file" | "read_file" | "notebook_edit" => {
                // Match case-insensitively on case-folding filesystems so a
                // deny like `write_file(secrets/**)` is not slipped by `Secrets/`.
                let glob = globset::GlobBuilder::new(inner.trim())
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
        (Some(ShellFacts::Bash(BashAnalysis::Commands(commands))), _) => commands
            .iter()
            .all(|argv| argv_is_readonly(argv) || allow.iter().any(|rule| rule.matches_argv(argv))),
        // Nothing vouches for a script the parser could not read, whole-tool
        // `bash` included: that rule was written without anyone having seen
        // this text, and no layer below can vet its content.
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => false,
        (Some(ShellFacts::PowerShellOpaque), _) => allow.iter().any(|rule| rule.matches_tool(name)),
        (None, Some(path)) => allow.iter().any(|rule| rule.matches_path(name, path)),
        (None, None) => allow.iter().any(|rule| rule.matches_tool(name)),
    }
}

/// Deny/ask matching is the aggressive direction: bash argv are matched
/// after wrapper stripping so `sudo rm` / `env FOO=1 rm` cannot dodge a
/// `bash(rm *)` rule, and any single matching segment hits.
pub(super) fn rules_hit(rules: &[Rule], name: &str, call: &CallFacts) -> bool {
    if rules.iter().any(|r| r.matches_tool(name)) {
        return true;
    }
    match (&call.shell, &call.path) {
        (Some(ShellFacts::Bash(BashAnalysis::Commands(cmds))), _) => cmds.iter().any(|argv| {
            let stripped = strip_wrappers(argv);
            rules
                .iter()
                .any(|r| r.matches_argv(&stripped) || r.matches_argv(argv))
        }),
        // Opaque scripts cannot be inspected, so content-level Bash/PowerShell
        // rules never decide here. Whole-tool rules were handled above.
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => false,
        (Some(ShellFacts::PowerShellOpaque), _) => false,
        (None, Some(path)) => rules.iter().any(|r| r.matches_path(name, path)),
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
}
