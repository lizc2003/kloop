//! Skills (plan 28): model-selected reusable prompt packs. A skill is a
//! `SKILL.md` (public Agent Skills format — YAML frontmatter + markdown body)
//! living in `<name>/SKILL.md`; the CLI discovers them and hands core an
//! already-loaded registry on `Config.skills`.
//!
//! This module is the pure half (parse + assemble + expand), mirroring
//! `context.rs`: the CLI does the directory walking and file IO, everything
//! here is testable without a filesystem. It stitches three existing seams —
//! progressive disclosure (the catalog rides the injected first user message,
//! like the deferred-tools notice), model triggering (the `skill` tool, like
//! `tool_search`), and inline expansion (`$ARGUMENTS`/`$N`/`${CLAUDE_SKILL_DIR}`
//! substitution, the user-slash-template shape plan 23 left as a seam).
//!
//! A triggered skill's body is returned as the `skill` tool's result, so the
//! instructions enter the model's context and the turn continues — no new
//! injection mechanism, unlike cc's SkillTool which queues a user message.

use serde::Deserialize;
use serde_json::json;

use kloop_protocol::ToolDef;

/// One loaded skill. `body` is held in memory but only reaches the model when
/// the skill is triggered — the catalog exposes just `name` + `description`
/// (progressive disclosure). `dir` is the skill's directory, substituted for
/// `${CLAUDE_SKILL_DIR}` so a skill can point at its own bundled `scripts/*`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Skill {
    pub name: String,
    /// The model-facing match signal: the public spec's `description` carries
    /// both what the skill does and when to use it.
    pub description: String,
    pub body: String,
    /// Absolute path to the skill's directory (for `${CLAUDE_SKILL_DIR}`).
    pub dir: String,
    /// How a model-triggered skill runs (plan 28 slice 2). `Inline` (default)
    /// returns the body into the current context; `Fork` runs it as an isolated
    /// sub-agent so only its result returns.
    pub context: SkillContext,
    /// Model override for a `Fork` skill; None inherits the caller's model.
    pub model: Option<String>,
}

/// A skill's execution mode (its `context` frontmatter field).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SkillContext {
    /// Body is returned into the current conversation and the turn continues.
    #[default]
    Inline,
    /// Body runs as an isolated sub-agent (like a `task`); only its final
    /// result comes back, keeping the skill's intermediate work out of the
    /// delegating model's context.
    Fork,
}

/// Frontmatter fields we read. serde drops every other key (`allowed-tools`,
/// `metadata`, `version`, …) for free — those are later slices.
#[derive(Deserialize)]
struct Frontmatter {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// `inline` (default) | `fork`. Anything else is treated as inline.
    #[serde(default)]
    context: Option<String>,
    /// Model override, honored only for a `fork` skill's sub-agent.
    #[serde(default)]
    model: Option<String>,
}

impl Skill {
    /// Parse one `SKILL.md`. `dir_name` is the containing directory's name (the
    /// default skill name); `dir` its display path. Errors are strings the CLI
    /// turns into skip-with-warning — a malformed skill never aborts startup.
    pub fn parse(dir_name: &str, dir: &str, content: &str) -> Result<Skill, String> {
        let (yaml, body) = split_frontmatter(content)
            .ok_or("no YAML frontmatter (expected a `---` delimited block at the top)")?;
        let fm: Frontmatter =
            serde_yaml_ng::from_str(yaml).map_err(|e| format!("invalid frontmatter: {e}"))?;
        let name = fm
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| dir_name.to_string());
        let description = fm
            .description
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .ok_or("missing 'description' (the field the model matches on)")?;
        let context = match fm.context.as_deref().map(str::trim) {
            Some("fork") => SkillContext::Fork,
            _ => SkillContext::Inline,
        };
        let model = fm
            .model
            .map(|m| m.trim().to_string())
            .filter(|m| !m.is_empty());
        Ok(Skill {
            name,
            description,
            body: body.trim().to_string(),
            dir: dir.to_string(),
            context,
            model,
        })
    }

    /// Resolve a skill by name, or an error naming the available skills — the
    /// same discoverable shape as an unknown agent_type. Model-facing (it rides
    /// back as an is_error tool_result), so it doubles as a correction.
    pub fn lookup<'a>(skills: &'a [Skill], name: &str) -> Result<&'a Skill, String> {
        skills.iter().find(|s| s.name == name).ok_or_else(|| {
            if skills.is_empty() {
                format!("unknown skill '{name}': no skills are defined")
            } else {
                let available = skills
                    .iter()
                    .map(|s| s.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("unknown skill '{name}' (available: {available})")
            }
        })
    }
}

/// Split `content` into (frontmatter YAML, body) at the leading `---` fence.
/// None when there is no opening fence or no closing `---` line.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let after_open = content
        .strip_prefix("---\n")
        .or_else(|| content.strip_prefix("---\r\n"))?;
    let mut offset = 0;
    for line in after_open.split_inclusive('\n') {
        if line.trim_end_matches('\n').trim_end_matches('\r') == "---" {
            return Some((&after_open[..offset], &after_open[offset + line.len()..]));
        }
        offset += line.len();
    }
    None
}

/// The progressive-disclosure catalog: name + description for every skill,
/// riding the injected first user message alongside the deferred-tools notice.
/// Session-stable (config-derived), so it stays byte-stable for the prompt
/// cache. None when no skills are loaded.
pub fn skills_catalog(skills: &[Skill]) -> Option<String> {
    if skills.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<system-reminder>\nThe following skills are available — reusable instruction \
         packs you can activate when a task matches one. Only a name and description are \
         shown here; a skill's full instructions load only when you activate it by calling \
         the `skill` tool with its name. Activate a skill when the task matches its \
         description; otherwise ignore this list.\n",
    );
    for s in skills {
        out.push_str(&format!("\n- {}: {}", s.name, s.description));
    }
    out.push_str("\n</system-reminder>");
    Some(out)
}

/// The `skill` tool: how the model triggers a skill. Registered only when
/// skills are loaded (see `turn_rounds`). Its definition is skill-independent —
/// which skills exist is advertised by the catalog, keeping this def and the
/// injected list both byte-stable for the cache.
pub fn skill_tool_def() -> ToolDef {
    ToolDef {
        name: "skill".into(),
        description: "Activate one of the available skills, loading its full instructions into the conversation so you can carry them out. The available skills are listed by name and description in the context; pick the one whose description matches the task. Pass any relevant user input as `arguments`.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Name of the skill to activate, exactly as listed in the context"},
                "arguments": {"type": "string", "description": "Optional arguments/context to pass to the skill"}
            },
            "required": ["name"]
        }),
    }
}

/// Execute the `skill` tool: look up the named skill and expand its body with
/// the given arguments. An `Inline` skill returns the expanded body as the tool
/// result — the instructions enter the model's context and the turn continues.
/// A `Fork` skill (plan 28 slice 2) instead runs the body as an isolated
/// sub-agent and returns only its final result, keeping the skill's
/// intermediate work out of the delegating model's context. An unknown name
/// comes back as an is_error result listing the skills that exist. The tool
/// itself is read-only (auto-allowed, see `CallFacts::is_readonly`): a fork's
/// sub-agent and any tool the inline instructions later prompt are each gated
/// on their own.
pub(crate) async fn skill_tool(
    input: &serde_json::Value,
    ctx: &crate::tools::ToolCtx,
) -> anyhow::Result<String> {
    let name = crate::tools::str_arg(input, "name", "skill")?;
    let args = input
        .get("arguments")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    let skill = Skill::lookup(&ctx.cfg.skills, name).map_err(|e| anyhow::anyhow!(e))?;
    let body = expand_body(&skill.body, &skill.dir, args);
    match skill.context {
        SkillContext::Inline => Ok(body),
        SkillContext::Fork => crate::tools::fork_skill(ctx, skill, body).await,
    }
}

/// Expand a skill body for injection: `${CLAUDE_SKILL_DIR}` → the skill's
/// directory, `$ARGUMENTS` → the whole argument string, `$N` → the Nth
/// shell-split word (0-indexed, cc's numbering). When the body has no argument
/// placeholder at all but arguments were given, they are appended as an
/// `ARGUMENTS:` line so nothing the user typed is silently dropped.
pub fn expand_body(body: &str, skill_dir: &str, args: &str) -> String {
    let words = split_words(args);
    let mut out = String::with_capacity(body.len());
    let mut used_args = false;
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < body.len() {
        if bytes[i] == b'$' {
            let rest = &body[i + 1..];
            if let Some(r) = rest.strip_prefix("{CLAUDE_SKILL_DIR}") {
                out.push_str(skill_dir);
                i = body.len() - r.len();
                continue;
            }
            if let Some(r) = rest.strip_prefix("ARGUMENTS") {
                out.push_str(args);
                used_args = true;
                i = body.len() - r.len();
                continue;
            }
            let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if let Ok(idx) = digits.parse::<usize>() {
                if let Some(word) = words.get(idx) {
                    out.push_str(word);
                }
                used_args = true;
                i += 1 + digits.len();
                continue;
            }
        }
        // Not a placeholder: copy one char (indexing is byte-based, so step by
        // the char's UTF-8 width to stay on boundaries).
        let ch = body[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    if !used_args && !args.trim().is_empty() {
        out.push_str(&format!("\n\nARGUMENTS: {}", args.trim()));
    }
    out
}

/// Minimal shell-style word split for `$N`: whitespace separates words, single
/// and double quotes group, backslash escapes the next char (outside single
/// quotes). Enough for argument lists; not a full shell parser.
fn split_words(s: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut has = false;
    let mut in_single = false;
    let mut in_double = false;
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => {
                in_single = !in_single;
                has = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                has = true;
            }
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    has = true;
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if has {
                    words.push(std::mem::take(&mut cur));
                    has = false;
                }
            }
            c => {
                cur.push(c);
                has = true;
            }
        }
    }
    if has {
        words.push(cur);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skills() -> Vec<Skill> {
        vec![
            Skill {
                name: "commit".into(),
                description: "Write a conventional-commit message. Use when committing.".into(),
                body: "Write a commit for $ARGUMENTS".into(),
                dir: "/repo/.kloop/skills/commit".into(),
                ..Default::default()
            },
            Skill {
                name: "review".into(),
                description: "Review a diff.".into(),
                body: "Review it.".into(),
                dir: "/repo/.kloop/skills/review".into(),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn parse_reads_name_and_description_and_body() {
        let content = "---\nname: my-skill\ndescription:  Do a thing. Use when asked.  \n---\n\n# Body\n\nSteps here.\n";
        let skill = Skill::parse("dir-name", "/skills/my-skill", content).unwrap();
        assert_eq!(
            skill,
            Skill {
                name: "my-skill".into(),
                description: "Do a thing. Use when asked.".into(),
                body: "# Body\n\nSteps here.".into(),
                dir: "/skills/my-skill".into(),
                ..Default::default()
            }
        );
    }

    #[test]
    fn parse_defaults_name_to_directory_and_ignores_unknown_keys() {
        let content = "---\ndescription: A skill.\nallowed-tools: [Read, Bash]\nversion: 2\nmetadata:\n  author: x\n---\nbody";
        let skill = Skill::parse("pdf-tools", "/skills/pdf-tools", content).unwrap();
        assert_eq!(skill.name, "pdf-tools");
        assert_eq!(skill.description, "A skill.");
        assert_eq!(skill.body, "body");
    }

    #[test]
    fn parse_supports_a_block_scalar_description() {
        let content =
            "---\nname: s\ndescription: >\n  A long folded\n  description line.\n---\nbody";
        let skill = Skill::parse("s", "/s", content).unwrap();
        assert_eq!(skill.description, "A long folded description line.");
    }

    #[test]
    fn parse_reads_context_and_model() {
        let fork = Skill::parse(
            "s",
            "/s",
            "---\ndescription: d\ncontext: fork\nmodel: cheap-1\n---\nb",
        )
        .unwrap();
        assert_eq!(fork.context, SkillContext::Fork);
        assert_eq!(fork.model.as_deref(), Some("cheap-1"));
        // Default is inline; an unrecognized context value is also inline; a
        // missing model stays None.
        let inline = Skill::parse("s", "/s", "---\ndescription: d\n---\nb").unwrap();
        assert_eq!(inline.context, SkillContext::Inline);
        assert_eq!(inline.model, None);
        let weird =
            Skill::parse("s", "/s", "---\ndescription: d\ncontext: sideways\n---\nb").unwrap();
        assert_eq!(weird.context, SkillContext::Inline);
    }

    #[test]
    fn parse_rejects_missing_frontmatter_and_missing_description() {
        assert!(Skill::parse("s", "/s", "just a body, no fence").is_err());
        assert!(Skill::parse("s", "/s", "---\nname: s\n---\nbody")
            .unwrap_err()
            .contains("description"));
        // Empty description is treated as missing.
        assert!(Skill::parse("s", "/s", "---\ndescription:   \n---\nbody").is_err());
    }

    #[test]
    fn parse_handles_crlf_and_no_trailing_newline() {
        let skill = Skill::parse("s", "/s", "---\r\ndescription: d\r\n---\r\nbody").unwrap();
        assert_eq!(skill.description, "d");
        assert_eq!(skill.body, "body");
        // Closing fence as the final line, no trailing newline: empty body.
        let skill = Skill::parse("s", "/s", "---\ndescription: d\n---").unwrap();
        assert_eq!(skill.body, "");
    }

    #[test]
    fn lookup_finds_by_name_and_lists_available_on_miss() {
        let skills = skills();
        assert_eq!(Skill::lookup(&skills, "review").unwrap().name, "review");
        assert_eq!(
            Skill::lookup(&skills, "ghost").unwrap_err(),
            "unknown skill 'ghost' (available: commit, review)"
        );
        assert_eq!(
            Skill::lookup(&[], "ghost").unwrap_err(),
            "unknown skill 'ghost': no skills are defined"
        );
    }

    #[test]
    fn catalog_lists_name_and_description_and_is_none_when_empty() {
        assert_eq!(skills_catalog(&[]), None);
        let catalog = skills_catalog(&skills()).unwrap();
        assert!(catalog.starts_with("<system-reminder>"));
        assert!(catalog
            .contains("\n- commit: Write a conventional-commit message. Use when committing."));
        assert!(catalog.contains("\n- review: Review a diff."));
        assert!(catalog.contains("`skill` tool"));
        assert!(catalog.ends_with("</system-reminder>"));
    }

    #[test]
    fn expand_substitutes_arguments_dir_and_positionals() {
        let body = "In ${CLAUDE_SKILL_DIR} run scripts/x.py on $0 and $1; all: $ARGUMENTS";
        let out = expand_body(body, "/skills/s", "alpha beta");
        assert_eq!(
            out,
            "In /skills/s run scripts/x.py on alpha and beta; all: alpha beta"
        );
    }

    #[test]
    fn expand_missing_positional_is_empty_and_out_of_range_ok() {
        assert_eq!(expand_body("[$0][$2]", "/s", "only"), "[only][]");
    }

    #[test]
    fn expand_quotes_group_positional_words() {
        assert_eq!(
            expand_body("$0 | $1", "/s", "\"two words\" third"),
            "two words | third"
        );
    }

    #[test]
    fn expand_appends_arguments_when_no_placeholder() {
        assert_eq!(
            expand_body("Do the thing.", "/s", "  extra context  "),
            "Do the thing.\n\nARGUMENTS: extra context"
        );
        // With a placeholder present, no append even if a word is unused.
        assert_eq!(expand_body("use $0", "/s", "a b"), "use a");
        // No args, no append.
        assert_eq!(expand_body("plain", "/s", ""), "plain");
    }

    #[test]
    fn expand_leaves_lone_dollar_and_non_digit_dollar_intact() {
        // `$` not followed by a placeholder token is copied verbatim; only
        // `$ARGUMENTS`/`$N`/`${CLAUDE_SKILL_DIR}` are substituted. ($N with a
        // digit — including prose like `$5` — is treated as a positional, an
        // accepted ambiguity shared with cc.)
        assert_eq!(
            expand_body("cost is $ for x$y", "/s", "z"),
            "cost is $ for x$y\n\nARGUMENTS: z"
        );
    }

    #[test]
    fn split_words_handles_quotes_and_escapes() {
        assert_eq!(split_words("a  b\tc"), vec!["a", "b", "c"]);
        assert_eq!(split_words("'a b' c"), vec!["a b", "c"]);
        assert_eq!(split_words(r#""a b" c\ d"#), vec!["a b", "c d"]);
        assert_eq!(split_words("   "), Vec::<String>::new());
    }
}
