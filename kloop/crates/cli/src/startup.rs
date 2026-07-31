//! Startup wiring: parse the process-global user config and KLOOP_* env once,
//! then combine that immutable policy with each session's cwd-bound project
//! context, sandbox workspace root, skills, and commands.

use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;

use kloop_core::agent_type::AgentType;
use kloop_core::hooks::HookDef;
use kloop_core::hooks::HookEvent;
use kloop_core::hooks::Hooks;
use kloop_core::interaction::Questioner;
use kloop_core::permissions::Approver;
use kloop_core::permissions::PermissionRules;
use kloop_core::permissions::Permissions;
use kloop_core::skills::Skill;
use kloop_core::skills::SkillContext as CoreSkillContext;
use kloop_core::skills::SkillSource;
use kloop_core::tools::ToolSource;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_provider::Provider;
use kloop_server::ConfigSnapshot;
use kloop_server::SandboxConfigInfo;
use kloop_server::SkillContext as ServerSkillContext;
use kloop_server::SkillInfo;
use kloop_server::SkillScope;
use kloop_server::SkillsSnapshot;

use crate::args::CliArgs;
use crate::context;
use crate::provider_config::ResolvedProviderSettings;
use crate::user_config::UserConfig;

/// Process-wide runtime policy parsed from the one global user config. Cwd is
/// deliberately absent: sessions add only their workspace-specific anchors.
pub(crate) struct RuntimeSettings {
    config_path: PathBuf,
    oauth_store_path: PathBuf,
    permission_rules: Arc<Mutex<PermissionRules>>,
    hooks: Arc<Hooks>,
    sandbox: SandboxSettings,
    sandbox_disabled_by_env: bool,
    agent_types: Arc<Vec<AgentType>>,
    program_limits: kloop_core::ProgramLimits,
    context_window: Option<u64>,
    fallback_model: Option<String>,
    defer_threshold: usize,
}

impl RuntimeSettings {
    pub(crate) fn load(config: &UserConfig, mock_mode: bool) -> Result<Self> {
        if mock_mode {
            return Ok(Self {
                config_path: PathBuf::new(),
                oauth_store_path: PathBuf::new(),
                permission_rules: Arc::new(Mutex::new(PermissionRules::default())),
                hooks: Arc::new(Hooks::none()),
                sandbox: SandboxSettings::default(),
                sandbox_disabled_by_env: false,
                agent_types: Arc::new(Vec::new()),
                program_limits: kloop_core::ProgramLimits::default(),
                context_window: Some(200_000),
                fallback_model: None,
                defer_threshold: kloop_core::tools::TOOL_DEFER_THRESHOLD,
            });
        }
        let table = config.table();
        let config_path = config.path().to_path_buf();
        let oauth_store_path = config_path
            .parent()
            .expect("global config always has a parent")
            .join(crate::user_config::OAUTH_STORE);
        Ok(Self {
            config_path,
            oauth_store_path,
            permission_rules: Arc::new(Mutex::new(load_permission_rules(table)?)),
            hooks: Arc::new(Hooks {
                defs: load_hooks(table)?,
            }),
            sandbox: load_sandbox_settings(table)?,
            sandbox_disabled_by_env: sandbox_disabled_from_env(),
            agent_types: Arc::new(load_agent_types(table)?),
            program_limits: load_program_limits(table)?,
            context_window: context_window_from_env()?,
            fallback_model: std::env::var("KLOOP_FALLBACK_MODEL").ok(),
            defer_threshold: defer_threshold_from_env()?,
        })
    }

    pub(crate) fn defer_threshold(&self) -> usize {
        self.defer_threshold
    }
}

/// Parse `[[hooks]]` tables from the global user config. A missing section is
/// an empty list; malformed entries are errors.
fn load_hooks(root: &toml::Table) -> Result<Vec<HookDef>> {
    let Some(entries) = root.get("hooks") else {
        return Ok(Vec::new());
    };
    let entries = entries
        .as_array()
        .context("[[hooks]] must be an array of tables")?;
    let mut defs = Vec::new();
    for (i, entry) in entries.iter().enumerate() {
        let spec = entry
            .as_table()
            .with_context(|| format!("hooks[{i}] must be a table"))?;
        for key in spec.keys() {
            if !matches!(key.as_str(), "event" | "command" | "matcher" | "timeout_ms") {
                bail!(
                    "hooks[{i}] has unknown key '{key}' (event | command | matcher | timeout_ms)"
                );
            }
        }
        let event = spec
            .get("event")
            .and_then(|v| v.as_str())
            .with_context(|| format!("hooks[{i}] needs an 'event' string"))?;
        let event = HookEvent::parse(event).with_context(|| {
            format!("hooks[{i}] has unknown event '{event}' (pre_turn | post_turn | pre_tool | post_tool | subagent_start | subagent_stop)")
        })?;
        let command: Vec<String> = spec
            .get("command")
            .and_then(|v| v.as_array())
            .and_then(|list| {
                list.iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .with_context(|| format!("hooks[{i}] needs a 'command' string array"))?;
        if command.is_empty() {
            bail!("hooks[{i}].command must not be empty");
        }
        let matcher = match spec.get("matcher") {
            None => None,
            Some(v) => {
                let m = v
                    .as_str()
                    .with_context(|| format!("hooks[{i}].matcher must be a string"))?;
                if !event.is_tool_event() {
                    bail!(
                        "hooks[{i}]: matcher is only valid for pre_tool/post_tool, not {}",
                        event.name()
                    );
                }
                Some(m.to_string())
            }
        };
        let timeout_ms = match spec.get("timeout_ms") {
            None => kloop_core::hooks::DEFAULT_TIMEOUT_MS,
            Some(v) => v
                .as_integer()
                .filter(|&t| t > 0)
                .with_context(|| format!("hooks[{i}].timeout_ms must be a positive integer"))?
                as u64,
        };
        defs.push(HookDef {
            event,
            command,
            matcher,
            timeout_ms,
        });
    }
    Ok(defs)
}

/// Rules from global `[permissions]`, with KLOOP_ALLOW / KLOOP_DENY /
/// KLOOP_ASK (comma-separated) appended once at process startup.
fn parse_permission_rules(root: &toml::Table) -> Result<PermissionRules> {
    let mut rules = PermissionRules::default();
    if let Some(section) = root.get("permissions") {
        let section = section
            .as_table()
            .context("[permissions] must be a table")?;
        for key in section.keys() {
            if !matches!(key.as_str(), "allow" | "deny" | "ask") {
                bail!("[permissions] has unknown key '{key}' (allow | deny | ask)");
            }
        }
        let read = |key: &str, out: &mut Vec<String>| -> Result<()> {
            let Some(entries) = section.get(key) else {
                return Ok(());
            };
            let list = entries
                .as_array()
                .with_context(|| format!("permissions.{key} must be an array of strings"))?;
            for entry in list {
                out.push(
                    entry
                        .as_str()
                        .with_context(|| format!("permissions.{key} must be an array of strings"))?
                        .to_string(),
                );
            }
            Ok(())
        };
        read("allow", &mut rules.allow)?;
        read("deny", &mut rules.deny)?;
        read("ask", &mut rules.ask)?;
    }
    Ok(rules)
}

fn load_permission_rules(root: &toml::Table) -> Result<PermissionRules> {
    let mut rules = parse_permission_rules(root)?;
    let append_env = |name: &str, out: &mut Vec<String>| {
        if let Ok(raw) = std::env::var(name) {
            out.extend(
                raw.split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(str::to_string),
            );
        }
    };
    append_env("KLOOP_ALLOW", &mut rules.allow);
    append_env("KLOOP_DENY", &mut rules.deny);
    append_env("KLOOP_ASK", &mut rules.ask);
    Ok(rules)
}

fn persist_global_allow_rules(
    path: &Path,
    shared_rules: &Mutex<PermissionRules>,
    new_rules: &[String],
) -> Result<()> {
    let mut snapshot = shared_rules.lock().unwrap();
    crate::user_config::persist_allow_rules(path, new_rules)?;
    for rule in new_rules {
        if !snapshot.allow.contains(rule) {
            snapshot.allow.push(rule.clone());
        }
    }
    Ok(())
}

/// `approver` and `notify` are the UI-facing halves of the permission gate:
/// the plain REPL passes a blocking stdin prompt + stderr printer, the TUI a
/// popup + transcript note.
fn build_permissions(
    args: &CliArgs,
    cwd: &Path,
    settings: &RuntimeSettings,
    approver: Arc<dyn Approver>,
    notify: kloop_tui::NoteFn,
) -> Result<Permissions> {
    // --mock runs a canned turn with nobody at the keyboard: no gating at
    // all. Otherwise the mode comes straight from --permission-mode (manual);
    // bypass still enforces deny rules and safety checks.
    if args.mock {
        return Ok(Permissions::allow_all());
    }
    let mode = args.permission_mode;
    let rules = settings.permission_rules.lock().unwrap().clone();
    let persist_path = settings.config_path.clone();
    let shared_rules = Arc::clone(&settings.permission_rules);
    let persist = Arc::new(move |rules: &[String]| {
        match persist_global_allow_rules(&persist_path, &shared_rules, rules) {
            Ok(()) => notify(&format!(
                "saved global allow rule for all workspaces: {}",
                rules.join(", ")
            )),
            Err(error) => notify(&format!("failed to save global allow rule: {error:#}")),
        }
    });
    Permissions::new(
        mode,
        &rules,
        cwd.to_path_buf(),
        Some(approver),
        Some(persist),
    )
    .context(
        "invalid permission rules in ~/.kloop/config.toml / KLOOP_ALLOW / KLOOP_DENY / KLOOP_ASK",
    )
}

/// `[sandbox]` in the global user config: `enabled` (default true),
/// `allow_network` (default false), `writable_roots` (extra writable
/// directories, default none), `auto_allow` (default true: sandboxed bash
/// skips the asking layers of the permission gate), `escalate` (default
/// true: a sandbox-denied command is offered for an unsandboxed re-run).
struct SandboxSettings {
    enabled: bool,
    allow_network: bool,
    writable_roots: Vec<PathBuf>,
    auto_allow: bool,
    escalate: bool,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            allow_network: false,
            writable_roots: Vec::new(),
            auto_allow: true,
            escalate: true,
        }
    }
}

/// Custom agent types from global `[agents.<name>]`: each is a table with a
/// required `description` and optional `system` / `model` / `tools` (string
/// array). Order follows the file so the task description lists them stably.
pub(crate) fn load_agent_types(root: &toml::Table) -> Result<Vec<AgentType>> {
    let Some(agents) = root.get("agents") else {
        return Ok(Vec::new());
    };
    let agents = agents
        .as_table()
        .context("[agents] must be a table of named agent definitions")?;
    let mut types = Vec::new();
    for (name, def) in agents {
        let def = def
            .as_table()
            .with_context(|| format!("[agents.{name}] must be a table"))?;
        for key in def.keys() {
            if !matches!(key.as_str(), "description" | "system" | "model" | "tools") {
                bail!("[agents.{name}] has unknown key '{key}' (description | system | model | tools)");
            }
        }
        let description = def
            .get("description")
            .and_then(|v| v.as_str())
            .with_context(|| format!("[agents.{name}] needs a 'description' string"))?
            .to_string();
        let str_field = |key: &str| -> Result<Option<String>> {
            match def.get(key) {
                None => Ok(None),
                Some(v) => Ok(Some(
                    v.as_str()
                        .with_context(|| format!("[agents.{name}].{key} must be a string"))?
                        .to_string(),
                )),
            }
        };
        let tools = match def.get("tools") {
            None => None,
            Some(v) => {
                let list = v.as_array().with_context(|| {
                    format!("[agents.{name}].tools must be an array of strings")
                })?;
                let mut names = Vec::new();
                for entry in list {
                    names.push(
                        entry
                            .as_str()
                            .with_context(|| {
                                format!("[agents.{name}].tools must be an array of strings")
                            })?
                            .to_string(),
                    );
                }
                Some(names)
            }
        };
        types.push(AgentType {
            name: name.clone(),
            description,
            system: str_field("system")?,
            model: str_field("model")?,
            tools,
        });
    }
    Ok(types)
}

/// Skills discovered from `.kloop/skills/<name>/SKILL.md` (plan 28): the
/// project's dir (cwd-relative), then the global `~/.kloop/skills/`. A skill
/// downloaded for the Agent Skills ecosystem works as-is once its directory is
/// dropped in — the SKILL.md format is what matters, so we scan only kloop's
/// own dir, not cc's `.claude/`. The project layer wins on a name collision,
/// letting it override a global skill. A malformed skill is skipped with a
/// warning, never an error; `--mock` skips discovery entirely (hermetic).
pub(crate) fn load_skills(cwd: &Path) -> (Vec<Skill>, Vec<String>) {
    let home = std::env::home_dir();
    let mut skill_roots = vec![cwd.join(".kloop").join("skills")];
    let mut command_roots = vec![cwd.join(".kloop").join("commands")];
    if let Some(home) = &home {
        skill_roots.push(home.join(".kloop").join("skills"));
        command_roots.push(home.join(".kloop").join("commands"));
    }
    let (skills, mut warnings) = skills_from_roots(&skill_roots);
    // User commands (plan 36) join the same registry as `SkillSource::Command`
    // entries, so the rest of the wiring is unchanged.
    let (commands, command_warnings) = commands_from_roots(&command_roots);
    warnings.extend(command_warnings);
    (merge_commands(skills, commands), warnings)
}

/// Cwd-scoped metadata for the native `skills/list` read surface. User commands
/// share the runtime registry but are intentionally omitted here: they are
/// slash shortcuts, not model-invocable skills. Bodies/allowed-tools are also
/// omitted so listing cannot disclose prompt contents.
pub(crate) fn server_skills_snapshot(cwd: &Path) -> SkillsSnapshot {
    let (skills, warnings) = load_skills(cwd);
    let project_root = cwd.join(".kloop").join("skills");
    let skills = skills
        .into_iter()
        .filter(|skill| skill.source == SkillSource::Skill)
        .map(|skill| {
            let scope = if Path::new(&skill.dir).starts_with(&project_root) {
                SkillScope::Project
            } else {
                SkillScope::User
            };
            let context = match skill.context {
                CoreSkillContext::Inline => ServerSkillContext::Inline,
                CoreSkillContext::Fork => ServerSkillContext::Fork,
            };
            SkillInfo {
                name: skill.name,
                description: skill.description,
                path: Path::new(&skill.dir)
                    .join("SKILL.md")
                    .to_string_lossy()
                    .to_string(),
                scope,
                context,
                model: skill.model,
            }
        })
        .collect();
    SkillsSnapshot {
        cwd: cwd.to_string_lossy().to_string(),
        skills,
        warnings,
    }
}

/// Fold discovered commands into the skill registry, with a skill winning on a
/// name collision — the directory form is the fuller one, so a command only
/// fills a name no skill already claimed (silently, like project-over-global).
/// Split out so the precedence is testable without touching the filesystem.
fn merge_commands(mut skills: Vec<Skill>, commands: Vec<Skill>) -> Vec<Skill> {
    let taken: std::collections::HashSet<&str> = skills.iter().map(|s| s.name.as_str()).collect();
    let fresh: Vec<Skill> = commands
        .into_iter()
        .filter(|c| !taken.contains(c.name.as_str()))
        .collect();
    skills.extend(fresh);
    skills
}

/// Walk command roots in order (earlier roots win on name collision), reading
/// each top-level `*.md` as a single-file user command (plan 36). Subdirectory
/// namespaces are deferred, so nested files are not walked. Mirrors
/// [`skills_from_roots`]; a malformed command is skipped with a warning.
fn commands_from_roots(roots: &[PathBuf]) -> (Vec<Skill>, Vec<String>) {
    let mut commands = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        // Stable order so the unknown-command listing is deterministic.
        let mut files: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.extension().is_some_and(|e| e.eq_ignore_ascii_case("md")))
            .collect();
        files.sort();
        for file in files {
            let Ok(content) = std::fs::read_to_string(&file) else {
                continue;
            };
            let name = file
                .file_stem()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            let dir = file
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            match Skill::parse_command(name, &dir, &content) {
                // First name wins: a project command overrides a global one.
                Ok(cmd) if seen.insert(cmd.name.clone()) => commands.push(cmd),
                Ok(_) => {}
                Err(e) => warnings.push(format!("skipped command at {}: {e}", file.display())),
            }
        }
    }
    (commands, warnings)
}

/// Walk skill roots in order (earlier roots win on name collision), reading each
/// `<name>/SKILL.md`. Split from [`load_skills`] so the discovery logic is
/// testable without touching `$HOME`. Missing roots are simply absent.
fn skills_from_roots(roots: &[PathBuf]) -> (Vec<Skill>, Vec<String>) {
    let mut skills = Vec::new();
    let mut warnings = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for root in roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        // read_dir order is filesystem-dependent; sort so the catalog (and the
        // prompt-cache prefix it rides in) is stable across runs.
        let mut dirs: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            let skill_md = dir.join("SKILL.md");
            let Ok(content) = std::fs::read_to_string(&skill_md) else {
                continue;
            };
            let dir_name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            match Skill::parse(dir_name, &dir.display().to_string(), &content) {
                // First name wins: a project skill silently overrides a global
                // one (the ecosystem's expected override), so no shadow warning.
                Ok(skill) if seen.insert(skill.name.clone()) => skills.push(skill),
                Ok(_) => {}
                Err(e) => warnings.push(format!("skipped skill at {}: {e}", skill_md.display())),
            }
        }
    }
    (skills, warnings)
}

fn load_sandbox_settings(root: &toml::Table) -> Result<SandboxSettings> {
    let mut settings = SandboxSettings::default();
    let Some(section) = root.get("sandbox") else {
        return Ok(settings);
    };
    let section = section.as_table().context("[sandbox] must be a table")?;
    for (key, value) in section {
        match key.as_str() {
            "enabled" => {
                settings.enabled = value
                    .as_bool()
                    .context("sandbox.enabled must be a boolean")?;
            }
            "allow_network" => {
                settings.allow_network = value
                    .as_bool()
                    .context("sandbox.allow_network must be a boolean")?;
            }
            "writable_roots" => {
                let list = value
                    .as_array()
                    .context("sandbox.writable_roots must be an array of strings")?;
                for entry in list {
                    settings.writable_roots.push(PathBuf::from(
                        entry
                            .as_str()
                            .context("sandbox.writable_roots must be an array of strings")?,
                    ));
                }
            }
            "auto_allow" => {
                settings.auto_allow = value
                    .as_bool()
                    .context("sandbox.auto_allow must be a boolean")?;
            }
            "escalate" => {
                settings.escalate = value
                    .as_bool()
                    .context("sandbox.escalate must be a boolean")?;
            }
            other => bail!(
                "[sandbox] has unknown key '{other}' (enabled | allow_network | writable_roots | auto_allow | escalate)"
            ),
        }
    }
    Ok(settings)
}

/// `[codemode]` in the global user config (all optional; defaults in
/// `Limits::default`): `memory_mb`, `stack_kb`, `cpu_secs` (engine resource
/// limits) and `max_agents`, `max_items` (orchestration runaway ceilings). Each
/// is also overridable via `KLOOP_PROGRAM_<KEY>` env, which wins over the config
/// value. Bounds one `run_program` (code-mode) run.
fn load_program_limits(root: &toml::Table) -> Result<kloop_core::ProgramLimits> {
    let mut limits = kloop_core::ProgramLimits::default();
    if let Some(section) = root.get("codemode") {
        let section = section.as_table().context("[codemode] must be a table")?;
        for (key, value) in section {
            let need = || {
                value
                    .as_integer()
                    .filter(|&number| number > 0)
                    .with_context(|| format!("codemode.{key} must be a positive integer"))
            };
            match key.as_str() {
                "memory_mb" => limits.memory_bytes = need()? as usize * 1024 * 1024,
                "stack_kb" => limits.max_stack_bytes = need()? as usize * 1024,
                "cpu_secs" => limits.cpu_burst = Duration::from_secs(need()? as u64),
                "max_agents" => limits.max_agents = need()? as u64,
                "max_items" => limits.max_items_per_call = need()? as usize,
                other => bail!(
                    "[codemode] has unknown key '{other}' (memory_mb | stack_kb | cpu_secs | max_agents | max_items)"
                ),
            }
        }
    }
    let env_uint = |name: &str| -> Result<Option<u64>> {
        match std::env::var(name) {
            Ok(s) => {
                Ok(Some(s.parse().with_context(|| {
                    format!("{name} must be a positive integer")
                })?))
            }
            Err(_) => Ok(None),
        }
    };
    if let Some(n) = env_uint("KLOOP_PROGRAM_MEMORY_MB")? {
        limits.memory_bytes = n as usize * 1024 * 1024;
    }
    if let Some(n) = env_uint("KLOOP_PROGRAM_STACK_KB")? {
        limits.max_stack_bytes = n as usize * 1024;
    }
    if let Some(n) = env_uint("KLOOP_PROGRAM_CPU_SECS")? {
        limits.cpu_burst = Duration::from_secs(n);
    }
    if let Some(n) = env_uint("KLOOP_PROGRAM_MAX_AGENTS")? {
        limits.max_agents = n;
    }
    if let Some(n) = env_uint("KLOOP_PROGRAM_MAX_ITEMS")? {
        limits.max_items_per_call = n as usize;
    }
    Ok(limits)
}

fn sandbox_disabled_from_env() -> bool {
    matches!(
        std::env::var("KLOOP_SANDBOX").ok().as_deref(),
        Some("off") | Some("0") | Some("false")
    )
}

/// The session sandbox policy, or None with a warning when unavailable —
/// fail-open like hooks: the permission gate stays the enforcement layer. The
/// global switches are process-stable; cwd is the per-session writable root.
pub(crate) fn build_sandbox(
    args: &CliArgs,
    cwd: &Path,
    runtime: &RuntimeSettings,
    warn: impl Fn(&str),
) -> Result<Option<Arc<kloop_core::sandbox::SandboxPolicy>>> {
    // --mock stays hermetic; the env escape hatch was captured at startup.
    if args.mock || runtime.sandbox_disabled_by_env {
        return Ok(None);
    }
    let settings = &runtime.sandbox;
    if !settings.enabled {
        return Ok(None);
    }
    match kloop_core::sandbox::availability() {
        Ok(()) => {
            let mut policy = kloop_core::sandbox::SandboxPolicy::workspace(
                cwd,
                &settings.writable_roots,
                settings.allow_network,
            )
            .with_denied_read_path(&runtime.config_path)
            .with_denied_read_path(&runtime.oauth_store_path);
            policy.auto_allow = settings.auto_allow;
            policy.escalate = settings.escalate;
            Ok(Some(Arc::new(policy)))
        }
        Err(reason) => {
            warn(&format!(
                "sandbox unavailable ({reason}); bash commands run unsandboxed"
            ));
            Ok(None)
        }
    }
}

/// KLOOP_DEFER_THRESHOLD: total tool count above which MCP tool definitions
/// are deferred behind tool_search. Lower it to exercise deferral with a
/// small server; raise it to effectively disable deferral.
fn defer_threshold_from_env() -> Result<usize> {
    match std::env::var("KLOOP_DEFER_THRESHOLD").ok() {
        Some(raw) => raw
            .parse::<usize>()
            .context("KLOOP_DEFER_THRESHOLD must be a tool count"),
        None => Ok(kloop_core::tools::TOOL_DEFER_THRESHOLD),
    }
}

fn context_window_from_env() -> Result<Option<u64>> {
    match std::env::var("KLOOP_CONTEXT_WINDOW").ok().as_deref() {
        Some("off") | Some("0") => Ok(None),
        Some(raw) => Ok(Some(
            raw.parse::<u64>()
                .context("KLOOP_CONTEXT_WINDOW must be a token count or 'off'")?,
        )),
        None => Ok(Some(200_000)),
    }
}

/// Safe effective-config allowlist for native `config/read`. The source table
/// may contain secrets, but only these non-sensitive values cross the protocol.
pub(crate) fn server_config_snapshot(
    args: &CliArgs,
    cwd: &Path,
    provider: &ResolvedProviderSettings,
    runtime: &RuntimeSettings,
) -> Result<ConfigSnapshot> {
    let settings = &runtime.sandbox;
    let sandbox_enabled = !args.mock
        && settings.enabled
        && !runtime.sandbox_disabled_by_env
        && kloop_core::sandbox::availability().is_ok();
    Ok(ConfigSnapshot {
        cwd: cwd.to_string_lossy().to_string(),
        model: Some(provider.model().to_string()),
        permission_mode: if args.mock {
            "mock".into()
        } else {
            args.permission_mode.label().into()
        },
        context_window: runtime.context_window,
        defer_threshold: runtime.defer_threshold,
        sandbox: SandboxConfigInfo {
            enabled: sandbox_enabled,
            allow_network: !args.mock && settings.allow_network,
            auto_allow: !args.mock && settings.auto_allow,
            escalate: !args.mock && settings.escalate,
        },
        worktree_enabled: !args.mock,
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn config_from_settings(
    args: &CliArgs,
    provider: &ResolvedProviderSettings,
    runtime: &RuntimeSettings,
    approver: Arc<dyn Approver>,
    questioner: Option<Arc<dyn Questioner>>,
    notify: kloop_tui::NoteFn,
    tool_sources: &[Arc<dyn ToolSource>],
    project: &context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    skills: Arc<Vec<Skill>>,
    cwd: &Path,
) -> Result<Config> {
    let permissions = Arc::new(build_permissions(args, cwd, runtime, approver, notify)?);
    let offload_dir = PathBuf::from(".kloop/offload");
    let sessions_dir = PathBuf::from(".kloop/sessions");
    // The main agent's cwd anchor is the process cwd (same value build_permissions
    // reads); a worktree sub-agent later rewires its own clone off this.
    let cwd = cwd.to_path_buf();
    let questions_enabled = questioner.is_some();
    let base = Config {
        provider: Arc::new(Provider::mock(vec![])),
        model: "mock".into(),
        system: project.system.clone(),
        project_instructions: project.instructions.clone(),
        max_rounds: None,
        cwd,
        offload_dir,
        sessions_dir,
        context_window: runtime.context_window,
        fallback_model: runtime.fallback_model.clone(),
        permissions,
        questioner,
        file_state: Default::default(),
        tool_sources: tool_sources.to_vec(),
        // The caller stamps the real session id once it knows it (after
        // open_history / per server thread).
        session_id: String::new(),
        agent_label: String::new(),
        hooks: Arc::clone(&runtime.hooks),
        background_shells: kloop_core::tools::BackgroundShells::new(),
        background_tasks: kloop_core::tools::BackgroundTasks::new(),
        sandbox,
        agent_types: Arc::clone(&runtime.agent_types),
        tool_allowlist: None,
        defer_threshold: runtime.defer_threshold,
        unlocked_tools: Default::default(),
        todos: Default::default(),
        inbox: Default::default(),
        program_limits: runtime.program_limits,
        skills,
        active_worktree: Arc::new(std::sync::RwLock::new(None)),
        surface: kloop_core::config::SurfaceCapabilities {
            questions: questions_enabled,
            plan_control: !args.headless,
            workflow: !args.headless,
            worktree: !args.mock,
        },
    };
    if args.mock {
        return Ok(Config {
            provider: Arc::new(Provider::mock(mock_demo_turns())),
            ..base
        });
    }
    Ok(Config {
        provider: Arc::new(provider.provider()),
        model: provider.model().to_string(),
        ..base
    })
}

/// Scripted turns for `--mock`, exercising all five bets (plus the todo list)
/// without an API key: round 1 lays out a todo list, round 2 batches two
/// read-only bash calls concurrently, round 3 runs an unsafe command whose
/// oversized output triggers offloading, round 4 reads it back, round 5 spawns
/// a sub-agent (round 6 is the sub-agent's own reply), round 7 finishes with
/// plain text.
fn mock_demo_turns() -> Vec<Vec<ContentBlock>> {
    let tool_use = |id: &str, name: &str, input: serde_json::Value| ContentBlock::ToolUse {
        id: id.into(),
        name: name.into(),
        input,
    };
    let text = |t: &str| ContentBlock::Text { text: t.into() };
    vec![
        vec![
            text("Planning the demo as a todo list…\n"),
            tool_use(
                "t0",
                "todo_write",
                json!({"todos": [
                    {"content": "Look around", "activeForm": "Looking around", "status": "in_progress"},
                    {"content": "Offload a big output and read it back", "activeForm": "Offloading a big output", "status": "pending"},
                    {"content": "Delegate to a sub-agent", "activeForm": "Delegating to a sub-agent", "status": "pending"},
                ]}),
            ),
        ],
        vec![
            text("Looking around (these two run as one concurrent batch)…\n"),
            tool_use("t1", "bash", json!({"command": "pwd"})),
            tool_use("t2", "bash", json!({"command": "ls"})),
        ],
        vec![
            text("Now a non-read-only command with huge output (runs sequentially, result gets offloaded)…\n"),
            tool_use("t3", "bash", json!({"command": "yes offload-me | head -n 3000"})),
        ],
        vec![
            text("Reading the offloaded output back…\n"),
            tool_use("t4", "read_offloaded", json!({"id": "off-0001"})),
        ],
        vec![
            text("Delegating to a sub-agent…\n"),
            tool_use("t5", "task", json!({"prompt": "say hi"})),
        ],
        // consumed by the sub-agent's own run_turn
        vec![text("hi from the sub-agent")],
        vec![text("Demo complete: parallel batch, offload + read-back, and a sub-agent all worked.")],
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(raw: &str) -> toml::Table {
        raw.parse().unwrap()
    }

    #[test]
    fn config_snapshot_uses_global_policy_and_ignores_cwd_config() {
        let base =
            std::env::temp_dir().join(format!("kloop-global-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let cwd_a = base.join("a");
        let cwd_b = base.join("b");
        std::fs::create_dir_all(cwd_a.join(".kloop")).unwrap();
        std::fs::create_dir_all(cwd_b.join(".kloop")).unwrap();
        std::fs::write(cwd_a.join(".kloop/config.toml"), "not valid toml = [").unwrap();
        std::fs::write(
            cwd_b.join(".kloop/config.toml"),
            "[sandbox]\nallow_network = false\nauto_allow = true\n",
        )
        .unwrap();
        let root = config(
            r#"
model = "secret-model"
[model_providers.secret]
wire_api = "responses"
http_headers = { Authorization = "Bearer SENTINEL-PROVIDER" }
[permissions]
allow = ["bash(secret *)"]
[[hooks]]
event = "pre_turn"
command = ["SENTINEL-HOOK"]
[sandbox]
allow_network = true
auto_allow = false
escalate = false
[mcp.servers.remote]
url = "https://mcp.example.test"
http_headers = { Authorization = "SENTINEL-MCP" }
"#,
        );
        let config = UserConfig::from_parts(base.join("home/.kloop/config.toml"), root);
        let runtime = RuntimeSettings::load(&config, /*mock_mode=*/ false).unwrap();
        let args = crate::args::parse_args(&[]).unwrap();
        let provider = ResolvedProviderSettings::mock();

        let snapshot_a = server_config_snapshot(&args, &cwd_a, &provider, &runtime).unwrap();
        let snapshot_b = server_config_snapshot(&args, &cwd_b, &provider, &runtime).unwrap();

        let mut normalized_a = snapshot_a.clone();
        normalized_a.cwd = snapshot_b.cwd.clone();
        assert_eq!(normalized_a, snapshot_b);
        assert!(snapshot_a.sandbox.allow_network);
        assert!(!snapshot_a.sandbox.auto_allow);
        assert!(!snapshot_a.sandbox.escalate);
        let wire = serde_json::to_string(&snapshot_a).unwrap();
        for secret in [
            "secret-model",
            "SENTINEL-PROVIDER",
            "bash(secret *)",
            "SENTINEL-HOOK",
            "SENTINEL-MCP",
        ] {
            assert!(!wire.contains(secret), "config/read leaked {secret}");
        }
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn sandbox_denies_global_config_and_oauth_store_reads() {
        if kloop_core::sandbox::availability().is_err()
            || matches!(
                std::env::var("KLOOP_SANDBOX").ok().as_deref(),
                Some("off") | Some("0") | Some("false")
            )
        {
            return;
        }
        let base =
            std::env::temp_dir().join(format!("kloop-global-sandbox-deny-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        let config_path = base.join("home/.kloop/config.toml");
        let oauth_path = config_path
            .parent()
            .unwrap()
            .join(crate::user_config::OAUTH_STORE);
        let config = UserConfig::from_parts(config_path.clone(), toml::Table::new());
        let runtime = RuntimeSettings::load(&config, /*mock_mode=*/ false).unwrap();
        let args = crate::args::parse_args(&[]).unwrap();

        let policy = build_sandbox(&args, &base, &runtime, |_| {})
            .unwrap()
            .unwrap();

        assert!(policy.denied_read_paths.contains(&config_path));
        assert!(policy.denied_read_paths.contains(&oauth_path));
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn native_skills_snapshot_omits_commands_and_bodies() {
        let base = std::env::temp_dir().join(format!("kloop-native-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let skill_dir = base.join(".kloop/skills/review");
        let command_dir = base.join(".kloop/commands");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::create_dir_all(&command_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\ndescription: Review changes\ncontext: fork\n---\nSECRET BODY",
        )
        .unwrap();
        std::fs::write(command_dir.join("deploy.md"), "# Deploy\n\nSECRET COMMAND").unwrap();

        let snapshot = server_skills_snapshot(&base);
        let review = snapshot
            .skills
            .iter()
            .find(|skill| skill.name == "review")
            .unwrap();
        assert_eq!(
            review,
            &SkillInfo {
                name: "review".into(),
                description: "Review changes".into(),
                path: skill_dir.join("SKILL.md").to_string_lossy().to_string(),
                scope: SkillScope::Project,
                context: ServerSkillContext::Fork,
                model: None,
            }
        );
        assert!(!snapshot.skills.iter().any(|skill| skill.name == "deploy"));
        let wire = serde_json::to_string(&snapshot).unwrap();
        assert!(!wire.contains("SECRET BODY"));
        assert!(!wire.contains("SECRET COMMAND"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn agent_types_parse_fields_and_reject_malformed() {
        assert!(load_agent_types(&toml::Table::new()).unwrap().is_empty());
        assert!(load_agent_types(&config("[permissions]\nallow = []\n"))
            .unwrap()
            .is_empty());

        let root = config(
            "[agents.researcher]\n\
             description = \"Searches the codebase.\"\n\
             system = \"You research.\"\n\
             model = \"claude-haiku-4-5\"\n\
             tools = [\"grep\", \"read_file\"]\n\
             \n\
             [agents.reviewer]\n\
             description = \"Reviews a diff.\"\n",
        );
        let types = load_agent_types(&root).unwrap();
        assert_eq!(types.len(), 2);
        let researcher = types
            .iter()
            .find(|agent| agent.name == "researcher")
            .unwrap();
        assert_eq!(
            (
                researcher.description.as_str(),
                researcher.system.as_deref(),
                researcher.model.as_deref(),
                researcher.tools.clone(),
            ),
            (
                "Searches the codebase.",
                Some("You research."),
                Some("claude-haiku-4-5"),
                Some(vec!["grep".to_string(), "read_file".to_string()]),
            )
        );
        let reviewer = types.iter().find(|agent| agent.name == "reviewer").unwrap();
        assert_eq!(reviewer.system, None);
        assert_eq!(reviewer.model, None);
        assert_eq!(reviewer.tools, None);

        for bad in [
            "[agents.x]\n",
            "[agents.x]\ndescription = 3\n",
            "[agents.x]\ndescription = \"d\"\nmodel = 5\n",
            "[agents.x]\ndescription = \"d\"\ntools = \"grep\"\n",
            "[agents.x]\ndescription = \"d\"\ntools = [3]\n",
            "[agents.x]\ndescription = \"d\"\nprompt = \"p\"\n",
            "agents = 3\n",
        ] {
            assert!(load_agent_types(&config(bad)).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn skills_discovery_applies_precedence_and_warns_on_malformed() {
        let base = std::env::temp_dir().join(format!("kloop-skills-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join(".kloop").join("skills");
        let global = base.join("home").join(".kloop").join("skills");
        let write_skill = |root: &Path, name: &str, body: &str| {
            let dir = root.join(name);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("SKILL.md"), body).unwrap();
        };
        // `commit` in both scopes: the project one must win. A global-only
        // `review`. A malformed skill (no description) warns and is skipped. A
        // directory without a SKILL.md is ignored.
        write_skill(
            &project,
            "commit",
            "---\ndescription: project commit\n---\nproject body",
        );
        write_skill(
            &global,
            "commit",
            "---\ndescription: global commit\n---\nglobal body",
        );
        write_skill(
            &global,
            "review",
            "---\ndescription: review a diff\n---\nreview body",
        );
        write_skill(&global, "broken", "no frontmatter here");
        std::fs::create_dir_all(global.join("empty-dir")).unwrap();

        let (skills, warnings) = skills_from_roots(&[project.clone(), global.clone()]);

        let by_name = |n: &str| skills.iter().find(|s| s.name == n).unwrap();
        assert_eq!(
            skills.len(),
            2,
            "commit deduped, broken skipped: {skills:?}"
        );
        assert_eq!(by_name("commit").description, "project commit");
        assert_eq!(by_name("commit").body, "project body");
        assert_eq!(
            by_name("commit").dir,
            project.join("commit").display().to_string()
        );
        assert_eq!(by_name("review").description, "review a diff");
        assert_eq!(
            warnings.len(),
            1,
            "only the malformed skill warns: {warnings:?}"
        );
        assert!(warnings[0].contains("broken") && warnings[0].contains("frontmatter"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn commands_discovery_reads_single_files_with_precedence() {
        let base = std::env::temp_dir().join(format!("kloop-commands-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let project = base.join(".kloop").join("commands");
        let global = base.join("home").join(".kloop").join("commands");
        let write = |root: &Path, file: &str, body: &str| {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(root.join(file), body).unwrap();
        };
        // `deploy` in both scopes: the project one wins. A global-only `note`
        // whose description falls back to its first line. A malformed command
        // (unparseable frontmatter) warns and is skipped. A non-`.md` file and a
        // subdirectory (namespaces deferred) are ignored.
        write(
            &project,
            "deploy.md",
            "---\ndescription: project deploy\n---\nbody",
        );
        write(
            &global,
            "deploy.md",
            "---\ndescription: global deploy\n---\nbody",
        );
        write(&global, "note.md", "# Jot a note\n\nDetails.");
        write(&global, "broken.md", "---\nnot: [valid\n---\nbody");
        write(&global, "readme.txt", "ignored, not markdown");
        std::fs::create_dir_all(global.join("sub")).unwrap();
        std::fs::write(global.join("sub").join("nested.md"), "nested").unwrap();

        let (commands, warnings) = commands_from_roots(&[project.clone(), global.clone()]);

        let by_name = |n: &str| commands.iter().find(|c| c.name == n).unwrap();
        assert_eq!(
            commands.len(),
            2,
            "deploy deduped, broken skipped: {commands:?}"
        );
        assert_eq!(by_name("deploy").description, "project deploy");
        assert_eq!(
            by_name("deploy").source,
            kloop_core::skills::SkillSource::Command
        );
        assert_eq!(by_name("note").description, "Jot a note");
        assert_eq!(
            warnings.len(),
            1,
            "only the malformed command warns: {warnings:?}"
        );
        assert!(warnings[0].contains("broken.md"));

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn merge_commands_lets_skills_win_on_name_collision() {
        let skill = |name: &str| Skill {
            name: name.into(),
            description: format!("skill {name}"),
            source: kloop_core::skills::SkillSource::Skill,
            ..Default::default()
        };
        let command = |name: &str| Skill {
            name: name.into(),
            description: format!("command {name}"),
            source: kloop_core::skills::SkillSource::Command,
            ..Default::default()
        };
        let merged = merge_commands(
            vec![skill("commit")],
            vec![command("commit"), command("deploy")],
        );
        // `commit` keeps the skill (the command is dropped); `deploy` is added.
        assert_eq!(merged.len(), 2);
        let by_name = |n: &str| merged.iter().find(|s| s.name == n).unwrap();
        assert_eq!(by_name("commit").description, "skill commit");
        assert_eq!(
            by_name("commit").source,
            kloop_core::skills::SkillSource::Skill
        );
        assert_eq!(
            by_name("deploy").source,
            kloop_core::skills::SkillSource::Command
        );
    }

    #[test]
    fn sandbox_settings_parse_defaults_and_reject_unknown_keys() {
        let settings = load_sandbox_settings(&toml::Table::new()).unwrap();
        assert!(settings.enabled);
        assert!(!settings.allow_network);
        assert!(settings.writable_roots.is_empty());
        assert!(settings.auto_allow);
        assert!(settings.escalate);
        assert!(
            load_sandbox_settings(&config("[permissions]\nallow = []\n"))
                .unwrap()
                .enabled
        );

        let settings = load_sandbox_settings(&config(
            "[sandbox]\nenabled = true\nallow_network = true\nwritable_roots = [\"/opt/data\"]\n",
        ))
        .unwrap();
        assert!(settings.enabled);
        assert!(settings.allow_network);
        assert_eq!(settings.writable_roots, vec![PathBuf::from("/opt/data")]);

        assert!(
            !load_sandbox_settings(&config("[sandbox]\nenabled = false\n"))
                .unwrap()
                .enabled
        );
        assert!(
            !load_sandbox_settings(&config("[sandbox]\nauto_allow = false\n"))
                .unwrap()
                .auto_allow
        );
        assert!(
            !load_sandbox_settings(&config("[sandbox]\nescalate = false\n"))
                .unwrap()
                .escalate
        );

        for bad in [
            "[sandbox]\nenabled = \"yes\"\n",
            "[sandbox]\nallow_network = 1\n",
            "[sandbox]\nwritable_roots = \"/opt\"\n",
            "[sandbox]\nwritable_roots = [1]\n",
            "[sandbox]\nauto_allow = \"on\"\n",
            "[sandbox]\nescalate = 1\n",
            "[sandbox]\nnetwork = true\n",
            "sandbox = true\n",
        ] {
            assert!(
                load_sandbox_settings(&config(bad)).is_err(),
                "accepted: {bad}"
            );
        }
    }

    #[test]
    fn codemode_config_rejects_unknown_and_nonpositive_limits() {
        for bad in [
            "codemode = false\n",
            "[codemode]\nmax_agents = 0\n",
            "[codemode]\nmemory_mb = \"large\"\n",
            "[codemode]\nunknown = 1\n",
        ] {
            assert!(
                load_program_limits(&config(bad)).is_err(),
                "accepted: {bad}"
            );
        }
    }

    #[test]
    fn permission_config_parses_and_rejects_malformed_sections() {
        assert_eq!(
            parse_permission_rules(&toml::Table::new()).unwrap(),
            PermissionRules::default()
        );

        let rules = parse_permission_rules(&config(
            "[permissions]\nallow = [\"bash(cargo build *)\"]\ndeny = [\"bash(git push *)\"]\nask = [\"web_fetch\"]\n",
        ))
        .unwrap();
        assert_eq!(
            rules,
            PermissionRules {
                allow: vec!["bash(cargo build *)".into()],
                deny: vec!["bash(git push *)".into()],
                ask: vec!["web_fetch".into()],
            }
        );

        for bad in [
            "[permissions]\nallow = \"not-an-array\"\n",
            "[permissions]\nallow = [3]\n",
            "[permissions]\nunknown = []\n",
            "permissions = false\n",
        ] {
            assert!(
                parse_permission_rules(&config(bad)).is_err(),
                "accepted: {bad}"
            );
        }
    }

    #[test]
    fn persisted_allow_rules_update_the_process_snapshot() {
        let dir = std::env::temp_dir().join(format!(
            "kloop-global-permission-snapshot-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let path = dir.join("config.toml");
        crate::user_config::write_private_atomic(
            &path,
            "test config",
            b"[permissions]\ndeny = [\"bash(git push *)\"]\n",
        )
        .unwrap();
        let shared = Arc::new(Mutex::new(PermissionRules {
            deny: vec!["bash(git push *)".into()],
            ..PermissionRules::default()
        }));

        persist_global_allow_rules(&path, &shared, &["bash(cargo test *)".into()]).unwrap();

        assert_eq!(
            shared.lock().unwrap().clone(),
            PermissionRules {
                allow: vec!["bash(cargo test *)".into()],
                deny: vec!["bash(git push *)".into()],
                ask: vec![],
            }
        );
        let raw = crate::user_config::read_private_string(&path, "test config")
            .unwrap()
            .unwrap();
        let disk: toml::Table = raw.parse().unwrap();
        assert_eq!(
            disk["permissions"]["allow"].as_array().unwrap(),
            &[toml::Value::String("bash(cargo test *)".into())]
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_hooks_full_round_trip() {
        assert_eq!(load_hooks(&toml::Table::new()).unwrap(), vec![]);
        assert_eq!(
            load_hooks(&config("[permissions]\nallow = []\n")).unwrap(),
            vec![]
        );

        let root = config(
            r#"
[[hooks]]
event = "pre_tool"
command = ["./guard.sh", "--strict"]
matcher = "bash"
timeout_ms = 5000

[[hooks]]
event = "post_turn"
command = ["notify-send"]
"#,
        );
        assert_eq!(
            load_hooks(&root).unwrap(),
            vec![
                HookDef {
                    event: HookEvent::PreTool,
                    command: vec!["./guard.sh".into(), "--strict".into()],
                    matcher: Some("bash".into()),
                    timeout_ms: 5000,
                },
                HookDef {
                    event: HookEvent::PostTurn,
                    command: vec!["notify-send".into()],
                    matcher: None,
                    timeout_ms: kloop_core::hooks::DEFAULT_TIMEOUT_MS,
                },
            ]
        );
    }

    #[test]
    fn load_hooks_rejects_malformed_entries() {
        for (tag, bad) in [
            ("noevent", "[[hooks]]\ncommand = [\"x\"]\n"),
            (
                "badevent",
                "[[hooks]]\nevent = \"on_tool\"\ncommand = [\"x\"]\n",
            ),
            ("nocmd", "[[hooks]]\nevent = \"pre_tool\"\n"),
            (
                "emptycmd",
                "[[hooks]]\nevent = \"pre_tool\"\ncommand = []\n",
            ),
            (
                "cmdstr",
                "[[hooks]]\nevent = \"pre_tool\"\ncommand = \"x\"\n",
            ),
            (
                "turnmatcher",
                "[[hooks]]\nevent = \"pre_turn\"\ncommand = [\"x\"]\nmatcher = \"bash\"\n",
            ),
            (
                "badtimeout",
                "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\ntimeout_ms = -1\n",
            ),
            (
                "unknownkey",
                "[[hooks]]\nevent = \"pre_tool\"\ncommand = [\"x\"]\nwhen = \"always\"\n",
            ),
            ("section", "hooks = false\n"),
        ] {
            assert!(load_hooks(&config(bad)).is_err(), "{tag} should fail");
        }
    }
}
