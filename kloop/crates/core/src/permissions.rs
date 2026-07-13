//! permissions — the layered gate run before every tool execution, shaped
//! after claude-code's `hasPermissionsToUseToolInner` pipeline:
//!
//! deny rules → safety checks → ask rules → sandbox auto-allow → bypass →
//! read-only self-verdict → acceptEdits → allow rules → session cache →
//! ask the user.
//!
//! Two invariants carried over from cc: **deny always beats allow**, and
//! **safety checks (destructive commands, sensitive paths) are immune to
//! bypass mode**. A denial becomes an is_error tool_result — the model can
//! take another approach; the turn does not end.
//!
//! The sandbox auto-allow layer (cc's `autoAllowBashIfSandboxed`) is the
//! sandbox/approval coupling: a bash call the OS sandbox will contain needs
//! no human sign-off — containment replaces the parse-level vetting of the
//! layers below it, opaque scripts included. Everything above it still has
//! its say: deny rules, safety checks, and — deliberately stricter than cc —
//! explicit ask rules ("always confirm this" is the user's word, no
//! automation outranks it). Dispatch feeds the verdict in per call, so an
//! escaped call (`disable_sandbox: true`) faces the gate like any other.
//!
//! Bash content decisions run on the tree-sitter analysis in [`crate::shell`]:
//! a script the parser cannot fully vouch for is *opaque* — it can never be
//! auto-approved, never match an allow rule, and never enter the session
//! cache; it always goes to the user.

use std::collections::HashSet;
use std::future::Future;
use std::path::Component;
use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::bail;
use anyhow::Context as _;
use anyhow::Result;
use globset::Glob;
use globset::GlobMatcher;
use serde_json::Value;

use crate::shell::analyze_bash;
use crate::shell::argv_is_dangerous;
use crate::shell::argv_is_readonly;
use crate::shell::strip_wrappers;
use crate::shell::BashAnalysis;

/// An approver's answer to one confirmation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Allow this call only.
    Allow,
    /// Allow and cache the call's signatures for the rest of the session.
    AllowSession,
    /// Allow and persist the suggested allow rules (via the persist sink).
    AllowAlways,
    Deny,
}

/// One confirmation request. `remember_rules` carries the suggested
/// persistent rules when the call is remember-able; `None` means only
/// allow-once / deny apply (opaque bash, sensitive paths, explicit ask
/// rules, sandbox escalation). `preview` carries a file-change diff for
/// `write_file`/`edit_file` so the human sees the change before approving;
/// `None` for everything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConfirmRequest {
    pub description: String,
    pub remember_rules: Option<Vec<String>>,
    pub preview: Option<String>,
}

/// The outcome of [`Permissions::escalate_sandbox`] — the code-level
/// escalation loop's consent step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EscalationOutcome {
    /// Approved (or bypass mode): re-run the command without the sandbox.
    Approved,
    /// The user was asked and declined: keep the sandboxed failure and do
    /// not invite a disable_sandbox retry.
    Declined,
    /// Not asked (no approver, or tests/`--mock`): fall back to the
    /// model-driven denial hint.
    NotAttempted,
}

/// The asking seam, separate from `Ui` so the streaming-output trait stays
/// synchronous. The type-erased future shape mirrors `execute_tool`: it
/// keeps the trait object-safe without an async-trait dependency.
pub trait Approver: Send + Sync {
    fn confirm(&self, req: ConfirmRequest) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>>;
}

/// Sink invoked on [`Decision::AllowAlways`] with the rule strings to
/// persist; the CLI writes them to `.kloop/config.toml`. Errors are the
/// sink's problem to report (the gate has no UI).
pub type PersistFn = Box<dyn Fn(&[String]) + Send + Sync>;

/// Gating mode, after cc's permission modes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Default,
    /// File writes inside the working directory are auto-approved.
    AcceptEdits,
    /// Everything is approved except deny rules and safety checks (cc
    /// `bypassPermissions` semantics — those two layers are immune).
    Bypass,
}

/// Raw rule strings by behavior, before parsing. Formats:
/// `tool_name` (whole tool), `bash(<tokens>)` / `bash(<tokens> *)`
/// (per-segment argv prefix), `write_file(<glob>)` / `edit_file(<glob>)` /
/// `read_file(<glob>)` (path glob, gitignore-style `**` supported).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PermissionRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    pub ask: Vec<String>,
}

#[derive(Clone, Debug)]
enum Rule {
    Tool(String),
    BashPrefix { tokens: Vec<String>, wildcard: bool },
    PathGlob { tool: String, glob: GlobMatcher },
}

impl Rule {
    fn matches_tool(&self, name: &str) -> bool {
        matches!(self, Rule::Tool(t) if t == name)
    }

    /// Whether this rule vouches for one parsed bash argv.
    fn matches_argv(&self, argv: &[String]) -> bool {
        match self {
            Rule::Tool(t) => t == "bash",
            Rule::BashPrefix { tokens, wildcard } => {
                if *wildcard {
                    argv.len() >= tokens.len() && argv.iter().zip(tokens).all(|(a, t)| a == t)
                } else {
                    argv.len() == tokens.len() && argv.iter().zip(tokens).all(|(a, t)| a == t)
                }
            }
            Rule::PathGlob { .. } => false,
        }
    }

    fn matches_path(&self, tool: &str, facts: &PathFacts) -> bool {
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
                        || glob.is_match(&facts.normalized))
            }
            Rule::BashPrefix { .. } => false,
        }
    }
}

fn parse_rule(entry: &str) -> Result<Rule> {
    if let Some((tool, inner)) = entry
        .split_once('(')
        .and_then(|(t, rest)| rest.strip_suffix(')').map(|inner| (t, inner)))
    {
        match tool {
            "bash" => {
                let mut tokens: Vec<String> =
                    inner.split_whitespace().map(str::to_string).collect();
                let wildcard = tokens.last().is_some_and(|t| t == "*");
                if wildcard {
                    tokens.pop();
                }
                if tokens.is_empty() {
                    bail!("rule 'bash({inner})': empty command pattern");
                }
                Ok(Rule::BashPrefix { tokens, wildcard })
            }
            "write_file" | "edit_file" | "read_file" => {
                let glob = Glob::new(inner.trim())
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

fn parse_rules(entries: &[String]) -> Result<Vec<Rule>> {
    entries
        .iter()
        .map(String::as_str)
        .map(str::trim)
        .filter(|e| !e.is_empty())
        .map(parse_rule)
        .collect()
}

pub struct Permissions {
    /// Tests and `--mock` only: skip every layer including deny.
    allow_everything: bool,
    mode: Mode,
    /// Mutable: `AllowAlways` appends at runtime.
    allow: Mutex<Vec<Rule>>,
    deny: Vec<Rule>,
    ask: Vec<Rule>,
    session: Mutex<HashSet<String>>,
    approver: Option<Arc<dyn Approver>>,
    cwd: PathBuf,
    persist: Option<PersistFn>,
}

impl Permissions {
    /// No gating at all — for tests and the keyless `--mock` demo, where
    /// nobody is at the keyboard. The CLI's `--yolo` is NOT this; it is
    /// [`Mode::Bypass`], which deny rules and safety checks survive.
    pub fn allow_all() -> Self {
        Permissions {
            allow_everything: true,
            mode: Mode::Bypass,
            allow: Mutex::new(Vec::new()),
            deny: Vec::new(),
            ask: Vec::new(),
            session: Mutex::new(HashSet::new()),
            approver: None,
            cwd: PathBuf::from("/"),
            persist: None,
        }
    }

    pub fn new(
        mode: Mode,
        rules: &PermissionRules,
        cwd: PathBuf,
        approver: Option<Arc<dyn Approver>>,
        persist: Option<PersistFn>,
    ) -> Result<Self> {
        Ok(Permissions {
            allow_everything: false,
            mode,
            allow: Mutex::new(parse_rules(&rules.allow)?),
            deny: parse_rules(&rules.deny)?,
            ask: parse_rules(&rules.ask)?,
            session: Mutex::new(HashSet::new()),
            approver,
            cwd: lexical_normalize(Path::new("/"), &cwd),
            persist,
        })
    }

    /// Whether this tool call may run: `Ok(())` to proceed, `Err(reason)`
    /// with the message the model receives as an is_error tool_result.
    /// The sandbox-blind form (no auto-allow layer); dispatch uses
    /// [`Permissions::check_call`] with the per-call sandbox verdict.
    pub async fn check(&self, name: &str, input: &Value, depth: u8) -> Result<(), String> {
        self.check_call(name, input, depth, /*sandbox_auto_allow*/ false)
            .await
    }

    /// `sandbox_auto_allow`: this call will execute inside an OS sandbox
    /// whose policy opts into approval-free contained runs. Only dispatch
    /// can know that (it is a fact of the call, not of the gate), so it
    /// arrives as a parameter.
    pub async fn check_call(
        &self,
        name: &str,
        input: &Value,
        depth: u8,
        sandbox_auto_allow: bool,
    ) -> Result<(), String> {
        if self.allow_everything {
            return Ok(());
        }
        let call = CallFacts::gather(name, input, &self.cwd);

        // 1. Deny rules — before everything, immune to every mode.
        if self.matches_deny(name, &call) {
            return Err(format!(
                "{name}: blocked by a deny permission rule. Do not retry this call or try to \
                 work around the rule; choose a different approach or ask the user."
            ));
        }

        // 2. Safety checks — bypass-immune, straight to the user.
        if let Some(hazard) = call.hazard(name) {
            let remember = hazard
                .rememberable
                .then(|| remember_payload(name, &call))
                .flatten();
            return self
                .ask_user(name, input, depth, Some(hazard.tag), remember)
                .await;
        }

        // 3. Explicit ask rules — "always confirm this"; never remembered.
        if self.matches_ask(name, &call) {
            return self.ask_user(name, input, depth, None, None).await;
        }

        // 4. Sandbox auto-allow — the OS sandbox will contain this call, so
        // nothing below (parse-level vetting, rules, the human) needs to be
        // consulted. Sits under deny/safety/ask: those keep their say.
        if sandbox_auto_allow {
            return Ok(());
        }

        // 5. Bypass mode.
        if self.mode == Mode::Bypass {
            return Ok(());
        }

        // 6. Read-only self-verdict.
        if call.is_readonly(name) {
            return Ok(());
        }

        // 7. acceptEdits: file writes inside the working directory.
        if self.mode == Mode::AcceptEdits
            && matches!(name, "write_file" | "edit_file")
            && call.path.as_ref().is_some_and(|p| p.inside_cwd)
        {
            return Ok(());
        }

        // 8. Allow rules.
        if self.matches_allow(name, &call) {
            return Ok(());
        }

        // 9. Session cache.
        let remember = remember_payload(name, &call);
        if let Some(remember) = &remember {
            let session = self.session.lock().unwrap();
            if remember.signatures.iter().all(|s| session.contains(s)) {
                return Ok(());
            }
        }

        // 10. Ask.
        self.ask_user(name, input, depth, None, remember).await
    }

    fn matches_deny(&self, name: &str, call: &CallFacts) -> bool {
        rules_hit(&self.deny, name, call, /*strip_for_match*/ true)
    }

    fn matches_ask(&self, name: &str, call: &CallFacts) -> bool {
        rules_hit(&self.ask, name, call, /*strip_for_match*/ true)
    }

    /// Allow is the strict direction: every bash argv must be read-only or
    /// rule-matched (un-stripped — wrappers must be spelled out), and opaque
    /// bash never matches.
    fn matches_allow(&self, name: &str, call: &CallFacts) -> bool {
        let allow = self.allow.lock().unwrap();
        match (&call.bash, &call.path) {
            (Some(BashAnalysis::Commands(cmds)), _) => cmds
                .iter()
                .all(|argv| argv_is_readonly(argv) || allow.iter().any(|r| r.matches_argv(argv))),
            (Some(BashAnalysis::Opaque), _) => false,
            (None, Some(path)) => allow.iter().any(|r| r.matches_path(name, path)),
            (None, None) => allow.iter().any(|r| r.matches_tool(name)),
        }
    }

    async fn ask_user(
        &self,
        name: &str,
        input: &Value,
        depth: u8,
        hazard_tag: Option<&str>,
        remember: Option<Remember>,
    ) -> Result<(), String> {
        let Some(approver) = &self.approver else {
            return Err(format!(
                "{name}: approval required but no approver is available in this mode; denied."
            ));
        };
        let req = ConfirmRequest {
            description: describe(name, input, depth, hazard_tag),
            remember_rules: remember.as_ref().map(|r| r.rules.clone()),
            preview: crate::diff::file_change_preview(name, input).await,
        };
        match approver.confirm(req).await {
            Decision::Allow => Ok(()),
            Decision::AllowSession => {
                if let Some(remember) = remember {
                    self.session.lock().unwrap().extend(remember.signatures);
                }
                Ok(())
            }
            Decision::AllowAlways => {
                if let Some(remember) = remember {
                    if let Ok(parsed) = parse_rules(&remember.rules) {
                        self.allow.lock().unwrap().extend(parsed);
                    }
                    if let Some(persist) = &self.persist {
                        persist(&remember.rules);
                    }
                }
                Ok(())
            }
            Decision::Deny => Err(format!(
                "The user declined this {name} call. Do not retry the same call; take a \
                 different approach, or ask the user how to proceed."
            )),
        }
    }

    /// The escalation loop's consent step (codex's retry-on-denial): a
    /// bash command the OS sandbox contained failed in a denial-shaped way;
    /// ask whether to re-run it without the sandbox. The command already
    /// cleared this gate — deny rules and safety checks sit above the
    /// sandbox layers — so this asks only about removing containment, never
    /// re-litigates whether the command may run.
    pub async fn escalate_sandbox(&self, command: &str, depth: u8) -> EscalationOutcome {
        // Tests / `--mock`: nobody is watching (and the sandbox is off in
        // `--mock` anyway). Don't silently escalate.
        if self.allow_everything {
            return EscalationOutcome::NotAttempted;
        }
        // Bypass (`--yolo`) means "don't ask" — escalate as the model-driven
        // disable_sandbox retry already would (it auto-passes the bypass
        // layer of the gate).
        if self.mode == Mode::Bypass {
            return EscalationOutcome::Approved;
        }
        let Some(approver) = &self.approver else {
            return EscalationOutcome::NotAttempted;
        };
        let req = ConfirmRequest {
            description: describe_escalation(command, depth),
            remember_rules: None,
            preview: None,
        };
        match approver.confirm(req).await {
            Decision::Allow | Decision::AllowSession | Decision::AllowAlways => {
                EscalationOutcome::Approved
            }
            Decision::Deny => EscalationOutcome::Declined,
        }
    }
}

/// Deny/ask matching is the aggressive direction: bash argv are matched
/// after wrapper stripping so `sudo rm` / `env FOO=1 rm` cannot dodge a
/// `bash(rm *)` rule, and any single matching segment hits.
fn rules_hit(rules: &[Rule], name: &str, call: &CallFacts, strip_for_match: bool) -> bool {
    if rules.iter().any(|r| r.matches_tool(name)) {
        return true;
    }
    match (&call.bash, &call.path) {
        (Some(BashAnalysis::Commands(cmds)), _) => cmds.iter().any(|argv| {
            let stripped;
            let target: &[String] = if strip_for_match {
                stripped = strip_wrappers(argv);
                &stripped
            } else {
                argv
            };
            rules
                .iter()
                .any(|r| r.matches_argv(target) || r.matches_argv(argv))
        }),
        // Opaque scripts can't be inspected; they never auto-run, so the
        // human sees them at the ask step instead of a rule deciding here.
        (Some(BashAnalysis::Opaque), _) => false,
        (None, Some(path)) => rules.iter().any(|r| r.matches_path(name, path)),
        (None, None) => false,
    }
}

struct Hazard {
    tag: &'static str,
    /// Destructive commands may be remembered (precise signature);
    /// sensitive paths must be confirmed every time.
    rememberable: bool,
}

struct CallFacts {
    bash: Option<BashAnalysis>,
    path: Option<PathFacts>,
}

struct PathFacts {
    normalized: PathBuf,
    /// Present when the path is inside the working directory.
    relative: Option<PathBuf>,
    inside_cwd: bool,
    sensitive: bool,
}

impl CallFacts {
    fn gather(name: &str, input: &Value, cwd: &Path) -> Self {
        let bash = (name == "bash").then(|| {
            input["command"]
                .as_str()
                .map_or(BashAnalysis::Opaque, analyze_bash)
        });
        let path = matches!(name, "write_file" | "edit_file" | "read_file")
            .then(|| {
                input["path"]
                    .as_str()
                    .map(|raw| PathFacts::gather(raw, cwd))
            })
            .flatten();
        CallFacts { bash, path }
    }

    fn hazard(&self, name: &str) -> Option<Hazard> {
        if let Some(BashAnalysis::Commands(cmds)) = &self.bash {
            if cmds
                .iter()
                .any(|argv| argv_is_dangerous(argv) || argv_is_dangerous(&strip_wrappers(argv)))
            {
                return Some(Hazard {
                    tag: "destructive",
                    rememberable: true,
                });
            }
        }
        if matches!(name, "write_file" | "edit_file")
            && self.path.as_ref().is_some_and(|p| p.sensitive)
        {
            return Some(Hazard {
                tag: "sensitive path",
                rememberable: false,
            });
        }
        None
    }

    fn is_readonly(&self, name: &str) -> bool {
        match name {
            "read_file" | "read_offloaded" | "grep" | "glob" => true,
            // bash_output reads registry state; kill_bash only signals
            // processes the agent itself started via bash — neither can
            // touch anything the original bash call wasn't already gated on.
            "bash_output" | "kill_bash" => true,
            // task itself touches nothing; every tool call the sub-agent
            // makes passes through this same gate.
            "task" => true,
            // exec (code-mode) itself touches nothing; every tools.<name>()
            // and agent() call the program makes re-enters this same gate.
            "exec" => true,
            // tool_search only reads tool definitions and marks them
            // unlocked; the unlocked tool's own calls still pass this gate.
            "tool_search" => true,
            // todo_write mutates only the in-memory task list — nothing on
            // the user's system to sign off on (cc never prompts for it).
            "todo_write" => true,
            "bash" => matches!(&self.bash, Some(BashAnalysis::Commands(cmds))
                if !cmds.is_empty() && cmds.iter().all(|c| argv_is_readonly(c))),
            _ => false,
        }
    }
}

impl PathFacts {
    fn gather(raw: &str, cwd: &Path) -> Self {
        let normalized = lexical_normalize(cwd, Path::new(raw));
        let relative = normalized.strip_prefix(cwd).ok().map(Path::to_path_buf);
        let inside_cwd = relative.is_some();
        let sensitive = path_is_sensitive(&normalized);
        PathFacts {
            normalized,
            relative,
            inside_cwd,
            sensitive,
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

/// Paths where a write is privilege escalation, not editing: VCS internals
/// (hooks run code), kloop's own state, key material, shell/git rc files
/// (cc's `checkPathSafetyForAutoEdit` list, trimmed to this project's
/// blast radius).
fn path_is_sensitive(normalized: &Path) -> bool {
    const SENSITIVE_DIRS: &[&str] = &[".git", ".kloop", ".ssh", ".gnupg", ".aws"];
    const SENSITIVE_FILES: &[&str] = &[
        ".bashrc",
        ".zshrc",
        ".zshenv",
        ".zprofile",
        ".bash_profile",
        ".profile",
        ".gitconfig",
        ".envrc",
    ];
    let mut components = normalized.components().peekable();
    while let Some(comp) = components.next() {
        let Component::Normal(name) = comp else {
            continue;
        };
        let Some(name) = name.to_str() else {
            return true; // non-UTF8 path: refuse to vouch
        };
        let is_last = components.peek().is_none();
        if SENSITIVE_DIRS.contains(&name) && !is_last {
            return true;
        }
        if is_last
            && (SENSITIVE_DIRS.contains(&name)
                || SENSITIVE_FILES.contains(&name)
                || name.starts_with(".env"))
        {
            return true;
        }
    }
    false
}

struct Remember {
    /// Suggested persistent rules (`bash(git commit *)`, `write_file(src/**)`).
    rules: Vec<String>,
    /// Session-cache keys, one per non-covered segment / path scope.
    signatures: Vec<String>,
}

/// cc's "always allow" granularity: bash remembers a two-word command
/// prefix per segment (`git push` never covers `git commit`); file writes
/// remember the parent directory. `None` = not remember-able (opaque bash,
/// or tokens that would corrupt a rule string).
fn remember_payload(name: &str, call: &CallFacts) -> Option<Remember> {
    match (&call.bash, &call.path) {
        (Some(BashAnalysis::Commands(cmds)), _) => {
            let mut rules = Vec::new();
            let mut signatures = Vec::new();
            for argv in cmds {
                if argv_is_readonly(argv) {
                    continue;
                }
                let prefix: Vec<&str> = argv.iter().take(2).map(String::as_str).collect();
                if prefix
                    .iter()
                    .any(|t| t.contains(['(', ')', ',']) || t.chars().any(char::is_whitespace))
                {
                    return None;
                }
                let head = prefix.join(" ");
                let rule = format!("bash({head} *)");
                if !rules.contains(&rule) {
                    rules.push(rule);
                    signatures.push(format!("bash:{head}"));
                }
            }
            if rules.is_empty() {
                return None;
            }
            Some(Remember { rules, signatures })
        }
        (Some(BashAnalysis::Opaque), _) => None,
        (None, Some(path)) => {
            let scope = path
                .relative
                .as_deref()
                .unwrap_or(&path.normalized)
                .parent()
                .map(|d| d.to_string_lossy().into_owned())
                .unwrap_or_default();
            let pattern = if scope.is_empty() {
                "*".to_string()
            } else {
                format!("{scope}/**")
            };
            Some(Remember {
                rules: vec![format!("{name}({pattern})")],
                signatures: vec![format!("{name}:{scope}")],
            })
        }
        (None, None) => Some(Remember {
            rules: vec![name.to_string()],
            signatures: vec![name.to_string()],
        }),
    }
}

fn describe(name: &str, input: &Value, depth: u8, hazard_tag: Option<&str>) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    let hazard = hazard_tag.map_or(String::new(), |t| format!("[{t}] "));
    // The escape hatch from the OS sandbox is worth flagging to the human:
    // this run gets full filesystem and network access if approved.
    let no_sandbox = if name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false) {
        "[no sandbox] "
    } else {
        ""
    };
    let detail: String = match name {
        "bash" => input["command"].as_str().unwrap_or("?").to_string(),
        "write_file" | "edit_file" | "read_file" => {
            input["path"].as_str().unwrap_or("?").to_string()
        }
        _ => input.to_string(),
    }
    .chars()
    .take(200)
    .collect();
    format!("{agent}{hazard}{no_sandbox}{name}: {detail}")
}

/// The escalation prompt: single-line (the TUI popup renders one line) with
/// a tag that says why it is being asked.
fn describe_escalation(command: &str, depth: u8) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    let cmd: String = command.chars().take(200).collect();
    format!("{agent}[sandbox denied — run without sandbox?] bash: {cmd}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Pops one scripted decision per confirm (Deny once exhausted) and
    /// records every request it was asked.
    struct ScriptedApprover {
        script: Mutex<Vec<Decision>>,
        asked: Mutex<Vec<ConfirmRequest>>,
    }

    impl ScriptedApprover {
        fn new(script: Vec<Decision>) -> Arc<Self> {
            Arc::new(ScriptedApprover {
                script: Mutex::new(script),
                asked: Mutex::new(Vec::new()),
            })
        }

        fn asked(&self) -> Vec<ConfirmRequest> {
            self.asked.lock().unwrap().clone()
        }

        fn ask_count(&self) -> usize {
            self.asked.lock().unwrap().len()
        }
    }

    impl Approver for ScriptedApprover {
        fn confirm(
            &self,
            req: ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
            self.asked.lock().unwrap().push(req);
            let mut script = self.script.lock().unwrap();
            let decision = if script.is_empty() {
                Decision::Deny
            } else {
                script.remove(0)
            };
            Box::pin(async move { decision })
        }
    }

    fn rules(allow: &[&str], deny: &[&str], ask: &[&str]) -> PermissionRules {
        let v = |s: &[&str]| s.iter().map(|x| x.to_string()).collect();
        PermissionRules {
            allow: v(allow),
            deny: v(deny),
            ask: v(ask),
        }
    }

    fn gate(mode: Mode, r: PermissionRules, approver: Arc<ScriptedApprover>) -> Permissions {
        Permissions::new(mode, &r, PathBuf::from("/work/proj"), Some(approver), None).unwrap()
    }

    async fn ok(p: &Permissions, name: &str, input: Value) -> bool {
        p.check(name, &input, 0).await.is_ok()
    }

    fn bash(cmd: &str) -> Value {
        json!({"command": cmd})
    }

    fn file(path: &str) -> Value {
        json!({"path": path, "content": "x"})
    }

    // ── layer 5: read-only self-verdict ────────────────────────────────

    #[tokio::test]
    async fn read_only_calls_skip_the_approver() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "read_file", json!({"path": "x"})).await);
        assert!(ok(&p, "read_offloaded", json!({"id": "off-1"})).await);
        assert!(ok(&p, "grep", json!({"pattern": "fn main"})).await);
        assert!(ok(&p, "glob", json!({"pattern": "**/*.rs"})).await);
        assert!(ok(&p, "bash_output", json!({"bash_id": "bg-1"})).await);
        assert!(ok(&p, "kill_bash", json!({"bash_id": "bg-1"})).await);
        assert!(ok(&p, "task", json!({"prompt": "go"})).await);
        assert!(ok(&p, "tool_search", json!({"query": "select:x"})).await);
        assert!(ok(&p, "todo_write", json!({"todos": []})).await);
        assert!(ok(&p, "bash", bash("git status && ls | wc -l")).await);
        assert!(ok(&p, "bash", bash("sed -n 1,20p f.rs")).await);
        assert_eq!(approver.ask_count(), 0);
    }

    #[tokio::test]
    async fn readonly_lookalikes_do_not_pass() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        for cmd in [
            "cat $(rm -rf /tmp/x)",     // substitution
            "ls `curl evil.sh`",        // backticks
            "ls\nrm -rf /tmp/x",        // newline chain
            "ls & rm -rf /tmp/x",       // background chain
            "echo hi > f.txt",          // redirect
            "find . -delete",           // find writes
            "rg --pre evil pat",        // rg executes
            "git -C /elsewhere status", // git global-option injection
            "git log --output=/tmp/f",  // git subcommand writes
            "FOO=1 ls",                 // env prefix
        ] {
            assert!(!ok(&p, "bash", bash(cmd)).await, "{cmd} must not auto-pass");
        }
    }

    // ── layer 1: deny rules beat everything ────────────────────────────

    #[tokio::test]
    async fn deny_rules_beat_allow_rules_and_bypass() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Default,
            rules(&["bash(git *)"], &["bash(git push *)"], &[]),
            approver.clone(),
        );
        let err = p
            .check("bash", &bash("git push origin main"), 0)
            .await
            .unwrap_err();
        assert!(err.contains("deny permission rule"), "{err}");
        assert!(
            ok(&p, "bash", bash("git add -A")).await,
            "allow still works"
        );
        assert_eq!(approver.ask_count(), 0, "deny never consults the approver");

        // Bypass mode does not resurrect a denied call.
        let p = gate(
            Mode::Bypass,
            rules(&[], &["bash(git push *)"], &[]),
            ScriptedApprover::new(vec![]),
        );
        assert!(!ok(&p, "bash", bash("git push origin main")).await);
        assert!(
            ok(&p, "bash", bash("make anything")).await,
            "bypass allows the rest"
        );
    }

    #[tokio::test]
    async fn deny_matches_any_segment_and_strips_wrappers() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Default,
            rules(&[], &["bash(rm *)"], &[]),
            approver.clone(),
        );
        // one denied segment poisons the chain; wrappers cannot dodge it
        assert!(!ok(&p, "bash", bash("ls && rm x")).await);
        assert!(!ok(&p, "bash", bash("sudo rm x")).await);
        assert!(!ok(&p, "bash", bash("env FOO=1 rm x")).await);
        assert!(!ok(&p, "bash", bash("timeout 5s rm x")).await);
        assert!(!ok(&p, "bash", bash("xargs rm")).await);
        assert_eq!(approver.ask_count(), 0);

        // whole-tool and path-glob deny forms
        let p = gate(
            Mode::Default,
            rules(&["write_file"], &["write_file(secrets/**)", "task"], &[]),
            ScriptedApprover::new(vec![]),
        );
        assert!(!ok(&p, "write_file", file("secrets/key.pem")).await);
        assert!(ok(&p, "write_file", file("src/main.rs")).await);
        assert!(!ok(&p, "task", json!({"prompt": "x"})).await);
    }

    // ── layer 2: safety checks are bypass-immune ───────────────────────

    #[tokio::test]
    async fn destructive_commands_ask_even_in_bypass_and_over_allowlist() {
        let approver = ScriptedApprover::new(vec![Decision::Allow]);
        let p = gate(
            Mode::Bypass,
            rules(&["bash(rm *)"], &[], &[]),
            approver.clone(),
        );
        assert!(ok(&p, "bash", bash("rm -rf build")).await);
        let asked = approver.asked();
        assert_eq!(
            asked.len(),
            1,
            "allow rule and bypass must not silence the check"
        );
        assert!(asked[0].description.contains("[destructive]"));

        // second time: deny (script exhausted) → the call is refused
        assert!(!ok(&p, "bash", bash("rm -rf build")).await);
    }

    #[tokio::test]
    async fn sensitive_paths_ask_every_time_and_never_remember() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession, Decision::Allow]);
        let p = gate(
            Mode::AcceptEdits,
            rules(&["write_file"], &[], &[]),
            approver.clone(),
        );
        for path in [".git/hooks/pre-commit", "../proj/.git/config"] {
            assert!(ok(&p, "write_file", file(path)).await, "{path}");
        }
        let asked = approver.asked();
        assert_eq!(
            asked.len(),
            2,
            "AllowSession must not stick for sensitive paths"
        );
        assert!(asked
            .iter()
            .all(|r| r.description.contains("[sensitive path]")));
        assert!(asked.iter().all(|r| r.remember_rules.is_none()));

        // more of the sensitive list, incl. escapes out of cwd
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Default,
            rules(&["write_file"], &[], &[]),
            approver.clone(),
        );
        for path in [
            "/home/u/.bashrc",
            ".env.local",
            "sub/.kloop/config.toml",
            "/x/.ssh/id_rsa",
        ] {
            assert!(!ok(&p, "write_file", file(path)).await, "{path}");
        }
        assert_eq!(approver.ask_count(), 4);
    }

    // ── layer 3: ask rules ──────────────────────────────────────────────

    #[tokio::test]
    async fn ask_rules_override_allow_and_are_not_cached() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession, Decision::Allow]);
        let p = gate(
            Mode::Default,
            rules(&["bash(cargo *)"], &[], &["bash(cargo publish *)"]),
            approver.clone(),
        );
        assert!(
            ok(&p, "bash", bash("cargo build")).await,
            "allow rule covers build"
        );
        assert!(ok(&p, "bash", bash("cargo publish --dry-run")).await);
        assert!(ok(&p, "bash", bash("cargo publish --dry-run")).await);
        assert_eq!(approver.ask_count(), 2, "ask rule confirms every time");
        assert!(approver.asked().iter().all(|r| r.remember_rules.is_none()));
    }

    // ── layers 6/7: acceptEdits and allow rules ─────────────────────────

    #[tokio::test]
    async fn accept_edits_auto_allows_writes_inside_cwd_only() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::AcceptEdits, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "write_file", file("src/main.rs")).await);
        assert!(
            ok(
                &p,
                "edit_file",
                json!({"path": "README.md", "old_string": "a", "new_string": "b"})
            )
            .await
        );
        assert!(
            !ok(&p, "write_file", file("/etc/hosts")).await,
            "outside cwd still asks"
        );
        assert!(
            !ok(&p, "write_file", file("../other/f.txt")).await,
            ".. escape still asks"
        );
        assert!(
            !ok(&p, "bash", bash("make build")).await,
            "acceptEdits is files-only"
        );
        assert_eq!(approver.ask_count(), 3);
    }

    #[tokio::test]
    async fn allow_rules_match_tool_bash_prefix_and_path_glob() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Default,
            rules(
                &[
                    "edit_file",
                    "bash(cargo *)",
                    "write_file(src/**)",
                    "bash(make)",
                ],
                &[],
                &[],
            ),
            approver.clone(),
        );
        assert!(
            ok(
                &p,
                "edit_file",
                json!({"path": "any.rs", "old_string": "a", "new_string": "b"})
            )
            .await
        );
        assert!(ok(&p, "bash", bash("cargo test --all")).await);
        assert!(
            ok(&p, "bash", bash("cargo build && ls")).await,
            "readonly segment rides along"
        );
        assert!(
            ok(&p, "bash", bash("make")).await,
            "exact rule matches bare command"
        );
        assert!(ok(&p, "write_file", file("src/deep/mod.rs")).await);
        assert_eq!(approver.ask_count(), 0);

        assert!(
            !ok(&p, "bash", bash("make clean")).await,
            "exact rule is exact"
        );
        assert!(
            !ok(&p, "bash", bash("cargo build && make clean")).await,
            "one uncovered segment asks"
        );
        assert!(
            !ok(&p, "write_file", file("docs/x.md")).await,
            "glob scope holds"
        );
        assert!(
            !ok(&p, "bash", bash("cargo $(evil)")).await,
            "opaque never matches allow"
        );
        assert_eq!(approver.ask_count(), 4);
    }

    // ── layers 8/9: session cache and remember payloads ────────────────

    #[tokio::test]
    async fn allow_session_caches_two_word_prefix() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "bash", bash("git commit -m one")).await);
        assert!(
            ok(&p, "bash", bash("git commit --amend")).await,
            "same two-word prefix cached"
        );
        assert_eq!(approver.ask_count(), 1);
        // different second word → asks again (script exhausted → deny)
        assert!(!ok(&p, "bash", bash("git rebase main")).await);
        assert_eq!(approver.ask_count(), 2);
    }

    #[tokio::test]
    async fn allow_always_extends_rules_and_persists() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let persisted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = persisted.clone();
        let approver = ScriptedApprover::new(vec![Decision::AllowAlways]);
        let p = Permissions::new(
            Mode::Default,
            &rules(&[], &[], &[]),
            PathBuf::from("/work/proj"),
            Some(approver.clone()),
            Some(Box::new(move |rules| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                sink.lock().unwrap().extend(rules.iter().cloned());
            })),
        )
        .unwrap();

        assert!(ok(&p, "bash", bash("cargo build")).await);
        assert_eq!(*persisted.lock().unwrap(), vec!["bash(cargo build *)"]);
        // the new rule is live immediately: same prefix no longer asks
        assert!(ok(&p, "bash", bash("cargo build --release")).await);
        assert_eq!(approver.ask_count(), 1);
    }

    #[tokio::test]
    async fn opaque_bash_is_never_cacheable() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession, Decision::AllowSession]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "bash", bash("cargo build")).await);
        assert!(
            ok(&p, "bash", bash("cargo $(evil)")).await,
            "user may still grant it once"
        );
        // the opaque grant must not have stuck
        assert!(!ok(&p, "bash", bash("cargo $(evil)")).await);
        assert_eq!(approver.ask_count(), 3);
        let asked = approver.asked();
        assert_eq!(asked[1].remember_rules, None, "opaque offers no remember");
    }

    #[tokio::test]
    async fn file_write_remembers_parent_directory_scope() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "write_file", file("src/a.rs")).await);
        assert!(
            ok(&p, "write_file", file("src/b.rs")).await,
            "same directory cached"
        );
        assert!(
            !ok(&p, "write_file", file("src/deep/c.rs")).await,
            "subdirectory asks"
        );
        assert_eq!(approver.ask_count(), 2);
        assert_eq!(
            approver.asked()[0].remember_rules,
            Some(vec!["write_file(src/**)".to_string()])
        );
    }

    // ── denial semantics & misc ─────────────────────────────────────────

    #[tokio::test]
    async fn user_denial_message_guides_the_model() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        let err = p.check("write_file", &file("x.txt"), 0).await.unwrap_err();
        assert!(err.contains("declined"), "{err}");
        assert!(err.contains("different approach"), "{err}");
    }

    #[tokio::test]
    async fn no_approver_auto_denies_instead_of_hanging() {
        let p = Permissions::new(
            Mode::Default,
            &rules(&[], &[], &[]),
            PathBuf::from("/work/proj"),
            None,
            None,
        )
        .unwrap();
        assert!(p.check("write_file", &file("x"), 0).await.is_err());
        assert!(
            p.check("bash", &bash("ls"), 0).await.is_ok(),
            "read-only still passes"
        );
    }

    #[tokio::test]
    async fn description_carries_depth_hazard_and_detail() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        let _ = p.check("bash", &bash("rm -rf x"), 1).await;
        let _ = p.check("write_file", &file("a.txt"), 0).await;
        let asked = approver.asked();
        assert_eq!(
            asked[0].description,
            "[sub-agent] [destructive] bash: rm -rf x"
        );
        assert_eq!(asked[1].description, "write_file: a.txt");
    }

    /// The approval request carries a file-change diff for edit/write so the
    /// human sees the change; other tools carry none.
    #[tokio::test]
    async fn confirm_request_carries_a_change_preview() {
        let approver = ScriptedApprover::new(vec![Decision::Deny, Decision::Deny, Decision::Deny]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        let _ = p
            .check(
                "edit_file",
                &json!({"path": "f.rs", "old_string": "foo", "new_string": "bar"}),
                0,
            )
            .await;
        // A path that does not exist is previewed as a new file.
        let _ = p
            .check(
                "write_file",
                &json!({"path": "/work/proj/does-not-exist.txt", "content": "hi\n"}),
                0,
            )
            .await;
        let _ = p.check("bash", &bash("rm -rf x"), 0).await;
        let asked = approver.asked();
        // f.rs cannot be read here, so the edit degrades to a two-string diff.
        assert_eq!(asked[0].preview.as_deref(), Some("-1  foo\n+1  bar"));
        assert_eq!(asked[1].preview.as_deref(), Some("(new file)\n+1  hi"));
        assert_eq!(asked[2].preview, None, "non-file calls carry no preview");
    }

    /// The sandbox auto-allow layer: a contained call runs without asking —
    /// opaque scripts included, containment replaces analysis — while the
    /// identical un-contained call still asks.
    #[tokio::test]
    async fn sandbox_auto_allow_skips_asking_for_contained_calls_only() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        for cmd in ["echo x > f.txt", "cargo build", "ls $(evil)"] {
            assert!(
                p.check_call("bash", &bash(cmd), 0, /*sandbox_auto_allow*/ true)
                    .await
                    .is_ok(),
                "{cmd}"
            );
        }
        assert_eq!(approver.ask_count(), 0);

        // Same non-readonly call, not contained: reaches the ask layer
        // (empty script = deny).
        assert!(p
            .check_call(
                "bash",
                &bash("cargo build"),
                0,
                /*sandbox_auto_allow*/ false
            )
            .await
            .is_err());
        assert_eq!(approver.ask_count(), 1);
    }

    /// Everything above the auto-allow layer keeps its say: deny rules
    /// reject outright, safety checks and explicit ask rules still go to
    /// the human (the ask-rule half is deliberately stricter than cc).
    #[tokio::test]
    async fn deny_safety_and_ask_rules_outrank_sandbox_auto_allow() {
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(
            Mode::Default,
            rules(&[], &["bash(git push *)"], &[]),
            approver.clone(),
        );
        assert!(p
            .check_call("bash", &bash("git push origin"), 0, true)
            .await
            .is_err());
        assert_eq!(approver.ask_count(), 0, "deny is a verdict, not a question");

        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert!(p
            .check_call("bash", &bash("rm -rf /tmp/x"), 0, true)
            .await
            .is_err());
        assert!(approver.asked()[0].description.contains("[destructive]"));

        let approver = ScriptedApprover::new(vec![Decision::Allow]);
        let p = gate(
            Mode::Default,
            rules(&[], &[], &["bash(cargo publish *)"]),
            approver.clone(),
        );
        assert!(p
            .check_call("bash", &bash("cargo publish --dry-run"), 0, true)
            .await
            .is_ok());
        assert_eq!(
            approver.ask_count(),
            1,
            "ask rule asked despite the sandbox"
        );
    }

    /// The escalation consent step: approver decisions map to Approved/
    /// Declined, bypass auto-approves without asking, and no approver (or
    /// mock) reports NotAttempted so the caller falls back to the hint.
    #[tokio::test]
    async fn escalate_sandbox_maps_decision_and_mode() {
        let approver = ScriptedApprover::new(vec![Decision::Allow, Decision::Deny]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("npm install", 0).await,
            EscalationOutcome::Approved
        );
        assert_eq!(
            p.escalate_sandbox("git push", 0).await,
            EscalationOutcome::Declined
        );
        assert_eq!(approver.ask_count(), 2);
        assert!(approver.asked()[0]
            .description
            .contains("[sandbox denied — run without sandbox?] bash: npm install"));

        // Bypass (--yolo): escalate without asking.
        let approver = ScriptedApprover::new(vec![]);
        let p = gate(Mode::Bypass, rules(&[], &[], &[]), approver.clone());
        assert_eq!(
            p.escalate_sandbox("curl x", 0).await,
            EscalationOutcome::Approved
        );
        assert_eq!(approver.ask_count(), 0, "bypass does not prompt");

        // allow_all (tests / --mock): never auto-escalate.
        assert_eq!(
            Permissions::allow_all().escalate_sandbox("rm x", 0).await,
            EscalationOutcome::NotAttempted
        );

        // No approver available: NotAttempted, so the caller keeps the hint.
        let p = Permissions::new(
            Mode::Default,
            &PermissionRules::default(),
            PathBuf::from("/"),
            None,
            None,
        )
        .unwrap();
        assert_eq!(
            p.escalate_sandbox("touch x", 0).await,
            EscalationOutcome::NotAttempted
        );
    }

    #[tokio::test]
    async fn description_flags_sandbox_escape() {
        let approver = ScriptedApprover::new(vec![Decision::Deny]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        let input = serde_json::json!({"command": "git push", "disable_sandbox": true});
        let _ = p.check("bash", &input, 0).await;
        assert_eq!(
            approver.asked()[0].description,
            "[no sandbox] bash: git push"
        );
    }

    #[tokio::test]
    async fn allow_all_skips_every_layer() {
        let p = Permissions::allow_all();
        assert!(p.check("bash", &bash("rm -rf /"), 0).await.is_ok());
        assert!(p
            .check("write_file", &file(".git/hooks/x"), 0)
            .await
            .is_ok());
    }

    /// The contract MCP integration relies on: an unknown (external) tool
    /// name defaults to asking; "allow for session" caches by whole tool
    /// name; the suggested persistent rule is the tool name and parses back.
    #[tokio::test]
    async fn mcp_style_tools_ask_by_default_and_remember_by_name() {
        let approver = ScriptedApprover::new(vec![Decision::AllowSession]);
        let p = gate(Mode::Default, rules(&[], &[], &[]), approver.clone());
        assert!(ok(&p, "memory__create_entities", json!({"k": "v"})).await);
        let asked = approver.asked();
        assert_eq!(
            asked[0].remember_rules,
            Some(vec!["memory__create_entities".to_string()])
        );
        assert!(parse_rule("memory__create_entities").is_ok());

        // Session-cached now; a different tool on the same server still asks.
        assert!(ok(&p, "memory__create_entities", json!({})).await);
        assert!(!ok(&p, "memory__delete_entities", json!({})).await);
        assert_eq!(approver.ask_count(), 2);

        // An allow rule by qualified name skips the approver entirely.
        let approver2 = ScriptedApprover::new(vec![]);
        let p2 = gate(
            Mode::Default,
            rules(&["memory__create_entities"], &[], &[]),
            approver2.clone(),
        );
        assert!(ok(&p2, "memory__create_entities", json!({})).await);
        assert_eq!(approver2.ask_count(), 0);
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
    }

    #[test]
    fn sensitive_path_detection_is_component_based() {
        let s = |p: &str| path_is_sensitive(Path::new(p));
        assert!(s("/w/proj/.git/hooks/pre-commit"));
        assert!(s("/w/proj/.git"));
        assert!(s("/w/proj/.kloop/sessions/x.jsonl"));
        assert!(s("/home/u/.bashrc"));
        assert!(s("/w/proj/.env.production"));
        assert!(s("/home/u/.ssh/id_rsa"));

        assert!(!s("/w/proj/src/main.rs"));
        assert!(!s("/w/proj/git/readme.md"), "git dir ≠ .git dir");
        assert!(!s("/w/proj/environment.rs"), ".env prefix is filename-only");
    }
}
