//! 审批记忆载荷、拒绝说明与前端展示文案。

use std::path::Path;

use serde_json::Value;

use super::facts::CallFacts;
use super::facts::PathFacts;
use super::facts::ShellFacts;
use crate::shell::BashAnalysis;
use crate::shell::analyze_bash;
use crate::shell::argv_is_readonly;
use crate::tools::Builtin;

/// What the sandbox actually refused, for the approval prompt. "The sandbox
/// blocked this" alone is the one thing the reader already knows; the class and
/// the line that proves it are what decide whether the answer is a writable
/// root, the network switch, or a different command.
pub(super) fn describe_denial(denial: Option<&crate::sandbox::SandboxDenial>) -> String {
    let Some(denial) = denial else {
        return "the OS sandbox blocked this — run it without the sandbox?".to_string();
    };
    format!(
        "the OS sandbox blocked this ({}) — run it without the sandbox?\n{}",
        denial.kind.label(),
        denial.evidence
    )
}

/// The session-cache key standing for "every sandbox escalation in this
/// workspace session". Never written to the durable store: a script this
/// coarse cannot be spelled as a rule, and consent that broad should not
/// outlive the sitting it was given in.
pub(super) const BLANKET_ESCALATION_SIGNATURE: &str = "sandbox_escalate:*";

/// What the ordinary gate's prompt shows under the remembering choices for a
/// script that can only be keyed on its own text. There is no rule to echo —
/// this memory is the session cache alone — and reprinting the command already
/// on screen two lines up would say nothing, so the echo states the granularity
/// instead. Unlike the escalation blanket below it covers exactly one command
/// text: the gate is the first door, so "every opaque script" would just be
/// bypass mode by another name.
const VERBATIM_REMEMBER_RULE: &str = "only this exact command text";

/// What the prompt shows under the remembering choices when the script is too
/// opaque to key on — the scope really is every escalation, and saying so is
/// the difference between an informed yes and a misread one.
pub(super) const BLANKET_ESCALATION_RULE: &str = "every sandbox escalation this session";

/// Escalation consent is remembered at the same granularity the ordinary gate
/// uses for bash — a two-word command prefix per segment — but under its own
/// `sandbox_escalate(...)` rule, because "may run" and "may run uncontained"
/// are different permissions. `None` = opaque script or a token that would
/// corrupt a rule string, i.e. not remember-able.
pub(super) fn escalation_remember_payload(command: &str, cwd: &Path) -> Option<Remember> {
    let analysis = analyze_bash(command);
    if cfg!(windows) && !analysis.writes().is_empty() {
        return None;
    }
    let cmds = analysis.commands()?;
    let writes: Vec<_> = analysis
        .writes()
        .iter()
        .map(|path| PathFacts::gather_with_resolved(Path::new(path), cwd, None))
        .collect();
    bash_prefix_memory(
        cmds,
        &writes,
        "sandbox_escalate",
        ReadOnlySegments::Remember,
    )
}

pub(super) fn user_denial(name: &str) -> String {
    format!(
        "The user declined this {name} call. Do not retry the same call; take a different approach, or ask the user how to proceed."
    )
}

/// What [`super::Refusal::Stopped`] tells the model. The turn ends after this batch,
/// so "take a different approach" would be advice it gets no round to follow.
pub(crate) fn user_stop(name: &str) -> String {
    format!(
        "The user declined this {name} call and stopped the turn to tell you what to do instead. Their next message says what they want; do not retry this call unless they ask for it."
    )
}

pub(super) struct Remember {
    /// Suggested persistent rules (`bash(git commit *)`, `write_file(src/**)`).
    /// Empty when nothing durable can be written down.
    pub(super) rules: Vec<String>,
    /// What the prompt echoes under the remembering choices. Usually the rules
    /// themselves; a verbatim script rule is the command text the panel already
    /// shows two lines up, so it states the granularity instead of repeating it.
    pub(super) echo: Vec<String>,
    /// Session-cache keys, one per non-covered segment / path scope.
    pub(super) signatures: Vec<String>,
    /// The parsed argv of each remembered segment. Only the sandbox-escalation
    /// path reads this — it has to test the command against durable rules
    /// itself, where the ordinary gate has already done that upstream. Empty
    /// for the ordinary gate's payloads.
    pub(super) argvs: Vec<Vec<String>>,
    /// 升级记忆匹配还需复核同一组规范输出目标。
    pub(super) writes: Vec<PathFacts>,
}

impl Remember {
    /// Whether this memory can be written to the project store at all — the
    /// difference between offering the durable scope and offering only the
    /// session one.
    pub(super) fn persistable(&self) -> bool {
        !self.rules.is_empty()
    }

    /// The ordinary shape: what the prompt echoes is the rule itself.
    fn echoing_rules(rules: Vec<String>, signatures: Vec<String>, argvs: Vec<Vec<String>>) -> Self {
        Self {
            echo: rules.clone(),
            rules,
            signatures,
            argvs,
            writes: Vec::new(),
        }
    }
}

/// Whether a read-only segment needs remembering. The ordinary gate skips them
/// — a read-only command was never going to be asked about, so there is nothing
/// to remember. The escalation does not skip: a read-only command still had to
/// be *denied inside the sandbox* to reach that prompt, so it is part of what
/// the yes was given for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadOnlySegments {
    Skip,
    Remember,
}

/// One rule per distinct two-word command prefix, in `tool`'s own vocabulary:
/// `bash(git commit *)` for the ordinary gate, `sandbox_escalate(git commit *)`
/// for the escalation. The two consents are different permissions written at
/// the same granularity, so only the vocabulary and [`ReadOnlySegments`] differ
/// — everything else used to be a second copy of these twenty lines.
///
/// `None` = a token that would corrupt a rule string (a paren, a comma, or
/// embedded whitespace), so nothing can be written down at all. An empty argv
/// is rejected for the same reason: the parser never produces one, but a rule
/// reading `bash( *)` would be worse than no memory.
/// 固定输出独立追加父目录写入规则；无法安全表示目录范围时整组不生成可复用授权。
fn bash_prefix_memory(
    cmds: &[Vec<String>],
    writes: &[PathFacts],
    tool: &str,
    segments: ReadOnlySegments,
) -> Option<Remember> {
    let mut rules = Vec::new();
    let mut signatures = Vec::new();
    let mut argvs = Vec::new();
    for argv in cmds {
        if segments == ReadOnlySegments::Skip && argv_is_readonly(argv) {
            continue;
        }
        let prefix: Vec<&str> = argv.iter().take(2).map(String::as_str).collect();
        if prefix.is_empty()
            || prefix
                .iter()
                .any(|t| t.contains(['(', ')', ',']) || t.chars().any(char::is_whitespace))
        {
            return None;
        }
        let head = prefix.join(" ");
        let rule = format!("{tool}({head} *)");
        if !rules.contains(&rule) {
            rules.push(rule);
            signatures.push(format!("{tool}:{head}"));
        }
        // Only a payload that remembered *every* segment may later stand in for
        // the whole command; the skipping form hands back none on purpose.
        if segments == ReadOnlySegments::Remember {
            argvs.push(argv.clone());
        }
    }
    for path in writes {
        // cwd 本身没有可授权的文件父目录；只有根部文件的空 parent 才能锚回 cwd。
        let parent = match path.relative.as_deref() {
            Some(relative) => {
                let parent = relative.parent()?;
                if parent.as_os_str().is_empty() {
                    path.normalized.parent()?
                } else {
                    parent
                }
            }
            None => path.normalized.parent()?,
        };
        let scope = parent.to_str()?;
        // 不把目录名中的 glob 语法当通配授权，也不自动建议文件系统根目录授权。
        if scope == "/" || scope.contains(['*', '?', '[', ']', '{', '}', '\\', '\n', '\r']) {
            return None;
        }
        let pattern = format!("{scope}/**");
        let rule = format!("{tool}_write({pattern})");
        if !rules.contains(&rule) {
            rules.push(rule);
            signatures.push(format!("{tool}-write:{scope}"));
        }
    }
    if rules.is_empty() {
        return None;
    }
    let mut remember = Remember::echoing_rules(rules, signatures, argvs);
    remember.writes = writes.to_vec();
    Some(remember)
}

/// cc's "always allow" granularity: bash remembers a two-word command
/// prefix per segment (`git push` never covers `git commit`); file writes
/// remember the parent directory; an unparseable script is remembered
/// verbatim, session-only. `None` = not remember-able (a resource URI, or
/// tokens that would corrupt a rule string).
pub(super) fn remember_payload(name: &str, call: &CallFacts) -> Option<Remember> {
    // Resource URIs are model-selected dynamic locators. Remembering the generic
    // tool name would let approval for one server/URI authorize every future
    // resource read, so only an explicit configured allow rule may do that.
    if matches!(name, "read_mcp_resource" | "read_mcp_resource_dir") {
        return None;
    }
    match (&call.shell, &call.path) {
        (
            Some(ShellFacts::Bash(
                BashAnalysis::Commands(cmds) | BashAnalysis::WithWrites { commands: cmds, .. },
            )),
            _,
        ) => bash_prefix_memory(cmds, &call.writes, "bash", ReadOnlySegments::Skip),
        // PowerShell stays unremember-able: bash at least gets parsed and only
        // then gives up, while PowerShell has no analysis at all — so "every
        // call asks" is the only safety net it has left.
        (Some(ShellFacts::PowerShellOpaque), _) => None,
        // No prefix to key on, so the script is keyed by its own text: only
        // this exact command, re-offered in this same session, skips the
        // question. Session-only, and `rules` stays empty so the durable scope
        // is never offered (plan 145): the key is the whole command, and a real
        // command carries this run's own test filter, package list and `tail
        // -3`, so the next run of the same intent writes a different key — a
        // stored one is dead weight from the moment it lands (38 of them on the
        // user's disk, 0 hits between them).
        (Some(ShellFacts::Bash(BashAnalysis::Opaque)), _) => {
            let script = call.opaque_script.as_ref()?;
            // Escaping the sandbox is part of what the yes was given for, so it
            // is spelled into the session key: a contained run's approval must
            // never cover an uncontained one.
            let escape = if script.no_sandbox { "!no-sandbox" } else { "" };
            let command = &script.command;
            Some(Remember {
                rules: Vec::new(),
                echo: vec![VERBATIM_REMEMBER_RULE.to_string()],
                signatures: vec![format!("bash-script{escape}:{command}")],
                argvs: Vec::new(),
                writes: Vec::new(),
            })
        }
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
            Some(Remember::echoing_rules(
                vec![format!("{name}({pattern})")],
                vec![format!("{name}:{scope}")],
                Vec::new(),
            ))
        }
        (None, None) => Some(Remember::echoing_rules(
            vec![name.to_string()],
            vec![name.to_string()],
            Vec::new(),
        )),
    }
}

/// The one thing a gated call acts on: the command, the path, or — for a tool
/// with neither — its whole input. Shared by the flat `description` and the
/// multi-line parts so the two can never name different targets for the same
/// approval.
fn call_detail(name: &str, input: &Value) -> String {
    match name {
        "bash" | "powershell" => input["command"].as_str().unwrap_or("?").to_string(),
        "write_file" | "edit_file" | "read_file" => {
            input["path"].as_str().unwrap_or("?").to_string()
        }
        "notebook_edit" => input["notebook_path"].as_str().unwrap_or("?").to_string(),
        _ => input.to_string(),
    }
}

pub(super) fn describe(name: &str, input: &Value, depth: u8, hazard_tag: Option<&str>) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    let hazard = hazard_tag.map_or(String::new(), |t| format!("[{t}] "));
    // The escape hatch from the OS sandbox is worth flagging to the human:
    // this run gets full filesystem and network access if approved.
    let no_sandbox = if name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false) {
        "[no sandbox] "
    } else {
        ""
    };
    let detail = clip(&call_detail(name, input));
    let shell_class = if name == "powershell" {
        "[unclassified PowerShell] "
    } else {
        ""
    };
    format!("{agent}{hazard}{no_sandbox}{shell_class}{name}: {detail}")
}

/// [`describe`] pulled apart for a multi-line frontend: the kind of action, the
/// one thing it acts on, and why it is being asked. Same inputs, same facts —
/// only the shape differs, so the two can never disagree about what is gated.
pub(super) fn describe_parts(
    name: &str,
    input: &Value,
    depth: u8,
    hazard_tag: Option<&str>,
) -> (String, String, Option<String>) {
    let mut notices: Vec<String> = Vec::new();
    if depth > 0 {
        notices.push("requested by a sub-agent".to_string());
    }
    if let Some(tag) = hazard_tag {
        notices.push(tag.to_string());
    }
    if name == "bash" && input["disable_sandbox"].as_bool().unwrap_or(false) {
        notices.push("no OS sandbox — full filesystem and network access".to_string());
    }
    if name == "powershell" {
        notices.push("unclassified PowerShell".to_string());
    }
    let detail = call_detail(name, input);
    (
        tool_title(name).to_string(),
        clip(&detail),
        (!notices.is_empty()).then(|| notices.join(" · ")),
    )
}

/// A human name for the action a tool performs, for the panel's header row.
/// Built-ins all have one ([`Builtin::title`]); so do the two web tools, which
/// kloop names itself even though they arrive through the source seam. An MCP
/// tool keeps its own name — that is what the user configured and recognizes.
pub(super) fn tool_title(name: &str) -> &str {
    if let Some(builtin) = Builtin::from_name(name) {
        return builtin.title();
    }
    match name {
        crate::tools::web::WEB_FETCH => "Fetch a URL",
        crate::tools::web::WEB_SEARCH => "Web search",
        other => other,
    }
}

pub(super) fn sub_agent_notice(depth: u8) -> Option<String> {
    (depth > 0).then(|| "requested by a sub-agent".to_string())
}

pub(super) fn join_notices(first: Option<String>, second: &str) -> String {
    match first {
        Some(first) => format!("{first} · {second}"),
        None => second.to_string(),
    }
}

/// Prompt text is display, not a payload: cap it so one enormous command or
/// path cannot dominate the panel.
pub(super) fn clip(text: &str) -> String {
    text.chars().take(200).collect()
}

/// The escalation prompt: single-line (the flat `description` form) with
/// a tag that says why it is being asked.
pub(super) fn describe_escalation(command: &str, depth: u8) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    let cmd: String = command.chars().take(200).collect();
    format!("{agent}[sandbox denied — run without sandbox?] bash: {cmd}")
}

/// The exit-plan-mode prompt heading; the plan text itself rides in the
/// request's `preview` as [`super::ConfirmPreview::Plan`], which the frontends put in
/// the transcript rather than the prompt (plan 194).
pub(super) fn describe_plan_exit(depth: u8) -> String {
    let agent = if depth > 0 { "[sub-agent] " } else { "" };
    format!("{agent}Exit plan mode and start on this plan?")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use serde_json::json;

    /// The two bash memories are the same granularity in two vocabularies, and
    /// they disagree about exactly one thing: whether a read-only segment is
    /// part of what was approved. Writing them through one helper is only
    /// correct while that disagreement survives — so it is asserted from both
    /// ends of the same command.
    #[test]
    fn the_two_bash_memories_differ_only_in_vocabulary_and_read_only_segments() {
        let command = "ls -la && git commit -m x";
        let call = CallFacts::gather("bash", &json!({"command": command}), Path::new("/"), None);

        // The ordinary gate never had to ask about `ls`, so it remembers only
        // the segment that would have been asked about.
        let ordinary = remember_payload("bash", &call).expect("a parsed command is remember-able");
        assert_eq!(ordinary.rules, vec!["bash(git commit *)"]);
        assert_eq!(ordinary.signatures, vec!["bash:git commit"]);
        assert!(
            ordinary.argvs.is_empty(),
            "a payload that skipped a segment must not stand in for the whole command"
        );

        // The sandbox denied the whole line, `ls` included, so the escalation
        // remembers both — under its own rule name.
        let escalation = escalation_remember_payload(command, &super::super::testutil::test_cwd())
            .expect("same command, other door");
        assert_eq!(
            escalation.rules,
            vec![
                "sandbox_escalate(ls -la *)",
                "sandbox_escalate(git commit *)"
            ]
        );
        assert_eq!(
            escalation.signatures,
            vec!["sandbox_escalate:ls -la", "sandbox_escalate:git commit"]
        );
        assert_eq!(escalation.argvs.len(), 2);

        // Read-only throughout: nothing the ordinary gate could write down,
        // while the escalation still has two denied commands to consent to.
        let read_only = CallFacts::gather(
            "bash",
            &json!({"command": "ls && pwd"}),
            Path::new("/"),
            None,
        );
        assert!(remember_payload("bash", &read_only).is_none());
        assert_eq!(
            escalation_remember_payload("ls && pwd", &super::super::testutil::test_cwd())
                .unwrap()
                .rules,
            vec!["sandbox_escalate(ls *)", "sandbox_escalate(pwd *)"]
        );

        // A prefix token that would corrupt a rule string stops both. The
        // command still parses: one that does not takes the verbatim path
        // (plan 135) instead, which is a different memory altogether.
        let corrupt = r#"git "commit x" -m y"#;
        let corrupt_call =
            CallFacts::gather("bash", &json!({"command": corrupt}), Path::new("/"), None);
        assert!(matches!(
            corrupt_call.shell,
            Some(ShellFacts::Bash(BashAnalysis::Commands(_)))
        ));
        assert!(remember_payload("bash", &corrupt_call).is_none());
        assert!(
            escalation_remember_payload(corrupt, &super::super::testutil::test_cwd()).is_none()
        );
    }
}
