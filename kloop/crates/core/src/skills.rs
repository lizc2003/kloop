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
    /// Tool allowlist for a `Fork` skill's sub-agent (plan 28 slice 3), mapped
    /// from the frontmatter `allowed-tools` to kloop's tool names. `Some`
    /// restricts the sub-agent to exactly these (plus the always-on
    /// `grep`); None inherits the full set. Ignored for an `Inline`
    /// skill, which runs in the caller's own context. This is a capability
    /// restriction, not a permission grant — the tools still face the gate.
    pub allowed_tools: Option<Vec<String>>,
    /// Where this entry came from, which decides who may invoke it. A
    /// `SKILL.md` is a model capability (`Skill`): it rides the catalog and the
    /// model can trigger it. A `.kloop/commands/*.md` file is a `Command`: a
    /// user shortcut, `/name`-invocable only, kept out of the catalog and the
    /// `skill` tool (plan 36 — cc's legacy `disable-model-invocation` default).
    pub source: SkillSource,
}

/// What kind of entry a [`Skill`] is — which discovery root it came from and,
/// consequently, whether the model may invoke it. Defaults to `Skill` so an
/// entry built without naming a source is a full model-facing skill.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SkillSource {
    /// A `SKILL.md` skill: listed in the catalog and triggerable by the model
    /// via the `skill` tool, as well as `/name`-invocable.
    #[default]
    Skill,
    /// A single-file user command (`.kloop/commands/*.md`): `/name`-invocable
    /// only, never advertised to or triggerable by the model.
    Command,
    /// A skill compiled into the binary (plan 119). Behaves exactly like a
    /// discovered `SKILL.md` — catalog, `skill` tool, `/name` — but has no
    /// directory on disk, and a same-named skill in either discovery root
    /// replaces it.
    Builtin,
}

impl SkillSource {
    /// Whether the model may see and trigger this entry. Only a user command is
    /// hidden; a builtin is a skill like any other.
    pub fn model_invocable(self) -> bool {
        self != SkillSource::Command
    }
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

/// Frontmatter fields we read. serde drops every other key (`metadata`,
/// `version`, `license`, …) for free — those are later slices. `Default` is the
/// "no frontmatter at all" case for a single-file command (plan 36).
#[derive(Deserialize, Default)]
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
    /// Tools a `fork` skill's sub-agent may use — a YAML list or a
    /// space/comma-separated string (both appear in the wild).
    #[serde(default, rename = "allowed-tools")]
    allowed_tools: Option<AllowedTools>,
}

/// `allowed-tools` accepts either a list or a single string.
#[derive(Deserialize)]
#[serde(untagged)]
enum AllowedTools {
    List(Vec<String>),
    Str(String),
}

impl Frontmatter {
    /// Build a [`Skill`] from the already-resolved `name`/`description`/`body`,
    /// filling `context`/`model`/`allowed_tools` from the remaining frontmatter.
    /// Shared by [`Skill::parse`] and [`Skill::parse_command`], which differ
    /// only in how they derive name and description.
    fn into_skill(
        self,
        name: String,
        description: String,
        body: String,
        dir: &str,
        source: SkillSource,
    ) -> Result<Skill, String> {
        let context = match self.context.as_deref().map(str::trim) {
            Some("fork") => SkillContext::Fork,
            _ => SkillContext::Inline,
        };
        let model = match self.model {
            Some(model) if model.trim().is_empty() => {
                return Err("model must be a non-blank string".into());
            }
            Some(model) => Some(model),
            None => None,
        };
        let allowed_tools = self.allowed_tools.and_then(|at| {
            let raw = match at {
                AllowedTools::List(v) => v,
                // Split on comma or whitespace: `"Read, Bash"` and `"Read Bash"`
                // both occur.
                AllowedTools::Str(s) => s.split([',', ' ']).map(str::to_string).collect(),
            };
            let mapped: Vec<String> = raw
                .iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(map_tool_name)
                .collect();
            (!mapped.is_empty()).then_some(mapped)
        });
        Ok(Skill {
            name,
            description,
            body,
            dir: dir.to_string(),
            context,
            model,
            allowed_tools,
            source,
        })
    }
}

/// A command's description when its frontmatter omits one: the body's first
/// non-empty line, a markdown header prefix stripped, truncated to 100 chars —
/// cc's `extractDescriptionFromMarkdown`. An empty body yields a generic label.
fn description_from_body(body: &str) -> String {
    for line in body.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // Strip a leading `#`-header only when it is `#+` followed by
        // whitespace (`## Title` → `Title`); `#foo` is left intact, like cc.
        let rest = trimmed.trim_start_matches('#');
        let text = if rest.len() < trimmed.len() && rest.starts_with(char::is_whitespace) {
            rest.trim_start()
        } else {
            trimmed
        };
        return if text.chars().count() > 100 {
            format!("{}...", text.chars().take(97).collect::<String>())
        } else {
            text.to_string()
        };
    }
    "Custom command".to_string()
}

impl Skill {
    /// Parse one `SKILL.md`. `dir_name` is the containing directory's name (the
    /// default skill name); `dir` its display path. Errors are strings the CLI
    /// turns into skip-with-warning — a malformed skill never aborts startup.
    pub fn parse(dir_name: &str, dir: &str, content: &str) -> Result<Skill, String> {
        Self::parse_with_source(dir_name, dir, content, SkillSource::Skill)
    }

    fn parse_with_source(
        dir_name: &str,
        dir: &str,
        content: &str,
        source: SkillSource,
    ) -> Result<Skill, String> {
        let (yaml, body) = split_frontmatter(content)
            .ok_or("no YAML frontmatter (expected a `---` delimited block at the top)")?;
        let mut fm: Frontmatter =
            serde_yaml_ng::from_str(yaml).map_err(|e| format!("invalid frontmatter: {e}"))?;
        let name = fm
            .name
            .take()
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| dir_name.to_string());
        let description = fm
            .description
            .take()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .ok_or("missing 'description' (the field the model matches on)")?;
        fm.into_skill(name, description, body.trim().to_string(), dir, source)
    }

    /// Parse one single-file user command (`.kloop/commands/*.md`, plan 36).
    /// Unlike a `SKILL.md`: the frontmatter is optional and `description` may be
    /// omitted — it then falls back to the body's first non-empty line (cc's
    /// legacy-command behavior). `name` is the file stem (the command name; a
    /// frontmatter `name` is ignored, matching cc). The result is a
    /// `SkillSource::Command`, so it is `/name`-invocable but stays out of the
    /// model's catalog and the `skill` tool.
    pub fn parse_command(name: &str, dir: &str, content: &str) -> Result<Skill, String> {
        let (mut fm, body) = match split_frontmatter(content) {
            Some((yaml, body)) => (
                serde_yaml_ng::from_str::<Frontmatter>(yaml)
                    .map_err(|e| format!("invalid frontmatter: {e}"))?,
                body,
            ),
            // No `---` fence: the whole file is the body.
            None => (Frontmatter::default(), content),
        };
        let body = body.trim().to_string();
        let description = fm
            .description
            .take()
            .map(|d| d.trim().to_string())
            .filter(|d| !d.is_empty())
            .unwrap_or_else(|| description_from_body(&body));
        fm.into_skill(
            name.to_string(),
            description,
            body,
            dir,
            SkillSource::Command,
        )
    }

    /// Resolve a skill by name among `candidates`, or an error naming what is
    /// available — the same discoverable shape as an unknown agent_type, and
    /// model-facing (it rides back as an is_error tool_result), so it doubles as
    /// a correction. Callers scope `candidates`: the slash path searches every
    /// loaded entry, the `skill` tool only the model-invocable ones (so a user
    /// command can't be triggered by the model, and the error never points at
    /// one).
    pub fn lookup<'a>(
        candidates: impl Iterator<Item = &'a Skill> + Clone,
        name: &str,
    ) -> Result<&'a Skill, String> {
        if let Some(skill) = candidates.clone().find(|s| s.name == name) {
            return Ok(skill);
        }
        let available = candidates.map(|s| s.name.as_str()).collect::<Vec<_>>();
        Err(if available.is_empty() {
            format!("unknown skill '{name}': no skills are defined")
        } else {
            format!(
                "unknown skill '{name}' (available: {})",
                available.join(", ")
            )
        })
    }
}

/// Map a cc / Agent-Skills tool name to kloop's, so a downloaded skill's
/// `allowed-tools: [Read, Bash]` restricts the right kloop tools. Unknown names
/// pass through unchanged (kloop-native names like `read_file` and MCP names
/// like `srv__x` already match). A cc scope qualifier (`Bash(git:*)`) is
/// dropped to the bare tool — kloop scopes commands through permission rules,
/// not the skill's tool list.
fn map_tool_name(name: &str) -> String {
    let base = name.split('(').next().unwrap_or(name).trim();
    match base {
        "Read" => "read_file",
        "Write" => "write_file",
        "Edit" => "edit_file",
        "Bash" => "bash",
        "PowerShell" => "powershell",
        "BashOutput" => "bash_output",
        "KillShell" | "KillBash" => "stop_bash",
        "Grep" => "grep",
        "Glob" => "glob",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        "Task" => "run_agent",
        "Skill" => "skill",
        other => other,
    }
    .to_string()
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

/// Skills compiled into the binary, as `(name, SKILL.md source)` pairs — the
/// one list a new builtin has to touch.
const BUILTIN_SKILLS: &[(&str, &str)] =
    &[("code-review", include_str!("skills/code-review/SKILL.md"))];

/// Skills that ship with the binary (plan 119), so a repository that has never
/// heard of kloop still gets them. They have no directory on disk, so `dir` is
/// empty and a builtin body must not reference `${CLAUDE_SKILL_DIR}` — there is
/// nothing to substitute. A malformed builtin is a kloop bug rather than a user
/// error (the bodies are compiled in and covered by tests), so it panics instead
/// of silently vanishing from the catalog.
pub fn builtin() -> Vec<Skill> {
    BUILTIN_SKILLS
        .iter()
        .map(|(name, content)| {
            Skill::parse_with_source(name, "", content, SkillSource::Builtin)
                .unwrap_or_else(|e| panic!("builtin skill '{name}' is malformed: {e}"))
        })
        .collect()
}

/// The progressive-disclosure catalog: name + description for every skill,
/// riding the injected first user message alongside the deferred-tools notice.
/// Session-stable (config-derived), so it stays byte-stable for the prompt
/// cache. None when no skills are loaded.
pub fn skills_catalog(skills: &[Skill]) -> Option<String> {
    // User commands (`SkillSource::Command`) are `/name`-only; they never enter
    // the model's catalog (plan 36 decision 3).
    let listed: Vec<&Skill> = skills
        .iter()
        .filter(|s| s.source.model_invocable())
        .collect();
    if listed.is_empty() {
        return None;
    }
    let mut out = String::from(
        "<system-reminder>\nThe following skills are available — reusable instruction \
         packs you can activate when a task matches one. Only a name and description are \
         shown here; a skill's full instructions load only when you activate it by calling \
         the `skill` tool with its name. Activate a skill when the task matches its \
         description; otherwise ignore this list.\n",
    );
    for s in listed {
        out.push_str(&format!("\n- {}: {}", s.name, s.description));
    }
    out.push_str("\n</system-reminder>");
    Some(out)
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
    fn parse_rejects_blank_model_override() {
        assert_eq!(
            Skill::parse(
                "s",
                "/s",
                "---\ndescription: d\ncontext: fork\nmodel: '  '\n---\nb"
            )
            .unwrap_err(),
            "model must be a non-blank string"
        );
    }

    #[test]
    fn parse_maps_allowed_tools() {
        // List form: cc names map to kloop's, a scope qualifier is dropped, and
        // kloop-native / MCP names pass through unchanged.
        let list = Skill::parse(
            "s",
            "/s",
            "---\ndescription: d\nallowed-tools:\n  - Read\n  - Bash(git log:*)\n  - Task\n  - KillBash\n  - BashOutput\n  - srv__x\n  - read_file\n---\nb",
        )
        .unwrap();
        assert_eq!(
            list.allowed_tools.unwrap(),
            vec![
                "read_file",
                "bash",
                "run_agent",
                "stop_bash",
                "bash_output",
                "srv__x",
                "read_file",
            ]
        );
        // String form: comma- or space-separated.
        let str_form = Skill::parse(
            "s",
            "/s",
            "---\ndescription: d\nallowed-tools: Read, Grep Bash\n---\nb",
        )
        .unwrap();
        assert_eq!(
            str_form.allowed_tools.unwrap(),
            vec!["read_file", "grep", "bash"]
        );
        // Absent → None.
        assert_eq!(
            Skill::parse("s", "/s", "---\ndescription: d\n---\nb")
                .unwrap()
                .allowed_tools,
            None
        );
    }

    #[test]
    fn parse_rejects_missing_frontmatter_and_missing_description() {
        assert!(Skill::parse("s", "/s", "just a body, no fence").is_err());
        assert!(
            Skill::parse("s", "/s", "---\nname: s\n---\nbody")
                .unwrap_err()
                .contains("description")
        );
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
        assert_eq!(
            Skill::lookup(skills.iter(), "review").unwrap().name,
            "review"
        );
        assert_eq!(
            Skill::lookup(skills.iter(), "ghost").unwrap_err(),
            "unknown skill 'ghost' (available: commit, review)"
        );
        assert_eq!(
            Skill::lookup([].iter(), "ghost").unwrap_err(),
            "unknown skill 'ghost': no skills are defined"
        );
    }

    #[test]
    fn catalog_lists_name_and_description_and_is_none_when_empty() {
        assert_eq!(skills_catalog(&[]), None);
        let catalog = skills_catalog(&skills()).unwrap();
        assert!(catalog.starts_with("<system-reminder>"));
        assert!(
            catalog
                .contains("\n- commit: Write a conventional-commit message. Use when committing.")
        );
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

    /// A single-file command: the name is the file stem (a frontmatter `name` is
    /// ignored, unlike a skill), the source is `Command`, and it reuses the same
    /// body/argument machinery. Frontmatter is optional.
    #[test]
    fn parse_command_uses_file_stem_and_marks_source() {
        let cmd = Skill::parse_command(
            "deploy",
            "/repo/.kloop/commands",
            "---\nname: ignored-name\ndescription: Ship it.\n---\nRun the deploy for $ARGUMENTS.",
        )
        .unwrap();
        assert_eq!(
            cmd,
            Skill {
                name: "deploy".into(),
                description: "Ship it.".into(),
                body: "Run the deploy for $ARGUMENTS.".into(),
                dir: "/repo/.kloop/commands".into(),
                source: SkillSource::Command,
                ..Default::default()
            }
        );
        // No frontmatter at all: whole file is the body.
        let bare = Skill::parse_command("note", "/c", "Just do the thing with $0.").unwrap();
        assert_eq!(bare.body, "Just do the thing with $0.");
        assert_eq!(bare.source, SkillSource::Command);
    }

    /// A command's description falls back to the body's first non-empty line
    /// (markdown header stripped), unlike a skill where it is required.
    #[test]
    fn parse_command_description_falls_back_to_first_line() {
        // No description in frontmatter → first body line, `#`-header stripped.
        let headed = Skill::parse_command(
            "c",
            "/c",
            "---\nmodel: x\n---\n\n# Summarize the diff\n\nDetails follow.",
        )
        .unwrap();
        assert_eq!(headed.description, "Summarize the diff");
        // No frontmatter → still the first non-empty line.
        let plain = Skill::parse_command("c", "/c", "\n\nFirst real line.\nSecond.").unwrap();
        assert_eq!(plain.description, "First real line.");
        // Empty body → a generic label, never an error.
        assert_eq!(
            Skill::parse_command("c", "/c", "---\nmodel: x\n---\n")
                .unwrap()
                .description,
            "Custom command"
        );
    }

    #[test]
    fn description_from_body_strips_headers_and_truncates() {
        assert_eq!(description_from_body("## Title here\nbody"), "Title here");
        // `#` without following whitespace is not a header.
        assert_eq!(description_from_body("#hashtag stays"), "#hashtag stays");
        assert_eq!(description_from_body("   \n\n  real  \n"), "real");
        assert_eq!(description_from_body(""), "Custom command");
        let long = "x".repeat(200);
        let out = description_from_body(&long);
        assert_eq!(out.chars().count(), 100);
        assert!(out.ends_with("..."));
    }

    /// The catalog is the model's view: it lists `SKILL.md` skills but never
    /// user commands, and is `None` when only commands are loaded.
    #[test]
    fn catalog_excludes_commands() {
        let mut mixed = skills();
        mixed.push(Skill {
            name: "deploy".into(),
            description: "Ship it.".into(),
            source: SkillSource::Command,
            ..Default::default()
        });
        let catalog = skills_catalog(&mixed).unwrap();
        assert!(catalog.contains("\n- commit:") && catalog.contains("\n- review:"));
        assert!(!catalog.contains("deploy"), "commands stay out: {catalog}");
        // Only commands loaded → nothing to advertise.
        let only_commands = vec![Skill {
            name: "deploy".into(),
            description: "Ship it.".into(),
            source: SkillSource::Command,
            ..Default::default()
        }];
        assert_eq!(skills_catalog(&only_commands), None);
    }

    /// Every builtin parses (they ship with the binary, so a malformed one is a
    /// kloop bug) and carries no directory — which means a builtin body must not
    /// reference `${CLAUDE_SKILL_DIR}`, as there is nothing to substitute.
    #[test]
    fn builtins_parse_and_reference_no_directory() {
        let builtins = builtin();
        assert!(!builtins.is_empty(), "at least code-review ships");
        for skill in &builtins {
            assert_eq!(skill.source, SkillSource::Builtin, "{}", skill.name);
            assert_eq!(skill.dir, "", "a builtin has no directory: {}", skill.name);
            assert!(
                !skill.body.contains("${CLAUDE_SKILL_DIR}"),
                "{} points at a directory it does not have",
                skill.name
            );
            assert!(!skill.description.is_empty(), "{}", skill.name);
        }
    }

    /// The `code-review` builtin (plan 119) forks, so a review's dozens of tool
    /// calls stay out of the delegating context, and keeps the full tool set —
    /// a review needs bash for `git` and for the affected package's tests.
    #[test]
    fn code_review_builtin_forks_with_the_full_tool_set() {
        let skill = builtin()
            .into_iter()
            .find(|s| s.name == "code-review")
            .expect("code-review ships");
        assert_eq!(skill.context, SkillContext::Fork);
        assert_eq!(skill.allowed_tools, None);
        assert!(skill.body.contains("$ARGUMENTS"), "takes a review target");
    }

    /// A builtin is model-facing like a discovered skill: it rides the catalog
    /// and the `skill` tool, unlike a user command.
    #[test]
    fn builtins_ride_the_catalog() {
        let catalog = skills_catalog(&builtin()).expect("builtins are advertised");
        assert!(catalog.contains("\n- code-review:"), "{catalog}");
        assert!(SkillSource::Builtin.model_invocable());
        assert!(SkillSource::Skill.model_invocable());
        assert!(!SkillSource::Command.model_invocable());
    }

    /// `lookup` searches whatever candidates the caller scopes to: the model's
    /// `skill`-tool view (skills only) can't reach a command, while the slash
    /// path (everything) can.
    #[test]
    fn lookup_respects_candidate_scope() {
        let mut all = skills();
        all.push(Skill {
            name: "deploy".into(),
            description: "Ship it.".into(),
            source: SkillSource::Command,
            ..Default::default()
        });
        // Slash path: every entry is reachable.
        assert_eq!(Skill::lookup(all.iter(), "deploy").unwrap().name, "deploy");
        // Model path: commands filtered out, so `deploy` is unknown and unlisted.
        let invocable = all.iter().filter(|s| s.source == SkillSource::Skill);
        assert_eq!(
            Skill::lookup(invocable, "deploy").unwrap_err(),
            "unknown skill 'deploy' (available: commit, review)"
        );
    }
}
