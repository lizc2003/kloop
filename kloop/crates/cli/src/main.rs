//! kloop CLI: environment-driven configuration, a line-based REPL with
//! Ctrl+C interruption, session persistence (`--resume`, `--fork`,
//! `--list-sessions`), MCP server wiring, and the keyless `--mock` demo.

mod context;
mod mcp;
mod web;

use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use tokio::io::AsyncBufReadExt;
use tokio::io::BufReader;
use tokio_util::sync::CancellationToken;

use kloop_core::agent::run_turn;
use kloop_core::agent::EndReason;
use kloop_core::agent::Ui;
use kloop_core::agents::AgentType;
use kloop_core::history::History;
use kloop_core::hooks::HookDef;
use kloop_core::hooks::HookEvent;
use kloop_core::hooks::Hooks;
use kloop_core::permissions::Approver;
use kloop_core::permissions::ConfirmRequest;
use kloop_core::permissions::Decision;
use kloop_core::permissions::Mode;
use kloop_core::permissions::PermissionRules;
use kloop_core::permissions::Permissions;
use kloop_core::rollout::first_user_snippet;
use kloop_core::rollout::fork_origin;
use kloop_core::rollout::fork_session;
use kloop_core::rollout::load_session;
use kloop_core::rollout::new_session_id;
use kloop_core::rollout::resume_session;
use kloop_core::rollout::session_id_of;
use kloop_core::rollout::session_path;
use kloop_core::rollout::sessions_by_recency;
use kloop_core::rollout::Rollout;
use kloop_core::tools::tool_merge_warnings;
use kloop_core::tools::ToolSource;
use kloop_core::Config;
use kloop_protocol::ContentBlock;
use kloop_protocol::Message;
use kloop_provider::Provider;
use kloop_provider::ThinkingMode;

#[derive(Clone, Debug, PartialEq, Eq)]
enum SessionChoice {
    New,
    /// `--continue`: the most recently modified session.
    Continue,
    /// `--resume` with no id: pick from a numbered list.
    Pick,
    Resume(String),
    /// `--fork <id>[#<seq>]`: branch a new session off an existing one at a
    /// line boundary (no seq = at the end) and continue there. Covers rewind
    /// too: fork the current session at an earlier point and take the other
    /// road — the original file is never touched.
    Fork {
        id: String,
        cut: Option<u64>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CliArgs {
    mock: bool,
    yolo: bool,
    accept_edits: bool,
    list_sessions: bool,
    plain: bool,
    serve: bool,
    session: SessionChoice,
}

fn parse_args(args: &[String]) -> Result<CliArgs> {
    let mut parsed = CliArgs {
        mock: false,
        yolo: false,
        accept_edits: false,
        list_sessions: false,
        plain: false,
        serve: false,
        session: SessionChoice::New,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--mock" => parsed.mock = true,
            "--yolo" => parsed.yolo = true,
            "--accept-edits" => parsed.accept_edits = true,
            "--list-sessions" => parsed.list_sessions = true,
            "--plain" => parsed.plain = true,
            "--serve" => parsed.serve = true,
            "--continue" => parsed.session = SessionChoice::Continue,
            "--resume" => {
                parsed.session = match args.get(i + 1) {
                    Some(id) if !id.starts_with('-') => {
                        i += 1;
                        SessionChoice::Resume(id.clone())
                    }
                    _ => SessionChoice::Pick,
                };
            }
            "--fork" => {
                let arg = match args.get(i + 1) {
                    Some(arg) if !arg.starts_with('-') => arg,
                    _ => bail!("--fork needs a session (<id> or <id>#<seq>)"),
                };
                i += 1;
                parsed.session = match arg.split_once('#') {
                    None => SessionChoice::Fork {
                        id: arg.clone(),
                        cut: None,
                    },
                    Some((id, seq)) => SessionChoice::Fork {
                        id: id.to_string(),
                        cut: Some(seq.parse().with_context(|| {
                            format!("--fork: '{seq}' is not a line number (<id>#<seq>)")
                        })?),
                    },
                };
            }
            other => bail!(
                "unknown argument '{other}' (--mock | --yolo | --accept-edits | --plain | --serve | --continue | --resume [id] | --fork <id>[#<seq>] | --list-sessions)"
            ),
        }
        i += 1;
    }
    Ok(parsed)
}

fn session_line(path: &Path) -> String {
    let id = session_id_of(path);
    let origin = fork_origin(path)
        .map(|o| format!("  [forked from {o}]"))
        .unwrap_or_default();
    match load_session(path) {
        Ok(messages) => format!(
            "{id}  {} message(s)  {}{origin}",
            messages.len(),
            first_user_snippet(&messages)
        ),
        Err(e) => format!("{id}  (unreadable: {e}){origin}"),
    }
}

fn list_sessions(sessions_dir: &Path) {
    let sessions = sessions_by_recency(sessions_dir);
    if sessions.is_empty() {
        println!("no saved sessions in {}", sessions_dir.display());
        return;
    }
    for path in sessions {
        println!("{}", session_line(&path));
    }
}

/// `--resume` with no id: numbered list on stdout, one line of stdin picks.
/// Runs before any UI starts, so plain blocking stdio is fine.
fn pick_session(sessions_dir: &Path) -> Result<PathBuf> {
    let sessions = sessions_by_recency(sessions_dir);
    if sessions.is_empty() {
        bail!("no saved sessions to resume");
    }
    println!("saved sessions (most recent first):");
    for (i, path) in sessions.iter().enumerate() {
        println!("{:>3}. {}", i + 1, session_line(path));
    }
    print!("resume which? [1-{}, empty = 1] > ", sessions.len());
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let index = pick_index(&line, sessions.len())?;
    Ok(sessions[index].clone())
}

/// 1-based selection, empty input = the first (most recent) entry.
fn pick_index(input: &str, len: usize) -> Result<usize> {
    let input = input.trim();
    if input.is_empty() {
        return Ok(0);
    }
    match input.parse::<usize>() {
        Ok(n) if (1..=len).contains(&n) => Ok(n - 1),
        _ => bail!("invalid selection '{input}' (expected 1-{len})"),
    }
}

/// Build the History for this run: a fresh persisted session by default, or
/// one replayed from disk for `--resume`.
fn open_history(
    offload_dir: PathBuf,
    choice: &SessionChoice,
    sessions_dir: &Path,
) -> Result<(History, String)> {
    let resume_path = match choice {
        SessionChoice::New => {
            let id = new_session_id(sessions_dir);
            let mut history = History::new(offload_dir);
            history.attach_rollout(Rollout::new(session_path(sessions_dir, &id)));
            return Ok((history, id));
        }
        SessionChoice::Resume(id) => {
            let path = session_path(sessions_dir, id);
            if !path.exists() {
                bail!("no session '{id}' (try --list-sessions)");
            }
            path
        }
        SessionChoice::Continue => sessions_by_recency(sessions_dir)
            .into_iter()
            .next()
            .context("no saved sessions to continue")?,
        SessionChoice::Pick => pick_session(sessions_dir)?,
        SessionChoice::Fork { id, cut } => {
            let src = session_path(sessions_dir, id);
            if !src.exists() {
                bail!("no session '{id}' (try --list-sessions)");
            }
            let path = fork_session(&src, *cut, sessions_dir)
                .with_context(|| format!("cannot fork session '{id}'"))?;
            println!(
                "[forked {id}#{cut} → {fork_id}]",
                cut = fork_origin(&path)
                    .and_then(|origin| origin.rsplit('#').next().map(str::to_string))
                    .unwrap_or_default(),
                fork_id = session_id_of(&path),
            );
            path
        }
    };
    let id = session_id_of(&resume_path);
    let (messages, rollout) = resume_session(&resume_path)
        .with_context(|| format!("cannot read session file {}", resume_path.display()))?;
    println!("[resumed session {id}: {} message(s)]", messages.len());
    let history = History::resume(offload_dir, messages, rollout);
    Ok((history, id))
}

const PERMISSIONS_CONFIG: &str = ".kloop/config.toml";

/// Parse `[[hooks]]` tables from `.kloop/config.toml`. A missing file or
/// missing section is an empty list; a malformed entry is an error (a
/// silently dropped hook would look like a policy that never fires).
fn load_hooks(config_path: &Path) -> Result<Vec<HookDef>> {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return Ok(Vec::new());
    };
    let value: toml::Table = raw
        .parse()
        .with_context(|| format!("cannot parse {}", config_path.display()))?;
    let Some(entries) = value.get("hooks") else {
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
            format!("hooks[{i}] has unknown event '{event}' (pre_turn | post_turn | pre_tool | post_tool)")
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

/// Rules from `.kloop/config.toml` `[permissions]` (allow/deny/ask string
/// arrays), with AGENT_ALLOW / AGENT_DENY / AGENT_ASK (comma-separated)
/// appended on top.
fn load_permission_rules(config_path: &Path) -> Result<PermissionRules> {
    let mut rules = PermissionRules::default();
    if let Ok(raw) = std::fs::read_to_string(config_path) {
        let value: toml::Table = raw
            .parse()
            .with_context(|| format!("cannot parse {}", config_path.display()))?;
        let read = |key: &str, out: &mut Vec<String>| -> Result<()> {
            let Some(entries) = value.get("permissions").and_then(|p| p.get(key)) else {
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
    let env = |var: &str, out: &mut Vec<String>| {
        if let Ok(raw) = std::env::var(var) {
            out.extend(
                raw.split(',')
                    .map(str::trim)
                    .filter(|e| !e.is_empty())
                    .map(str::to_string),
            );
        }
    };
    env("AGENT_ALLOW", &mut rules.allow);
    env("AGENT_DENY", &mut rules.deny);
    env("AGENT_ASK", &mut rules.ask);
    Ok(rules)
}

/// Append allow rules to `[permissions].allow`, preserving everything else
/// in the file (toml::Value round-trip; comments are not preserved).
fn persist_allow_rules(config_path: &Path, new_rules: &[String]) -> Result<()> {
    let mut table: toml::Table = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw
            .parse()
            .with_context(|| format!("cannot parse {}", config_path.display()))?,
        Err(_) => toml::Table::new(),
    };
    let permissions = table
        .entry("permissions")
        .or_insert_with(|| toml::Value::Table(toml::Table::new()))
        .as_table_mut()
        .context("[permissions] must be a table")?;
    let allow = permissions
        .entry("allow")
        .or_insert_with(|| toml::Value::Array(Vec::new()))
        .as_array_mut()
        .context("permissions.allow must be an array")?;
    for rule in new_rules {
        if !allow.iter().any(|v| v.as_str() == Some(rule)) {
            allow.push(toml::Value::String(rule.clone()));
        }
    }
    if let Some(parent) = config_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(config_path, toml::to_string_pretty(&table)?)
        .with_context(|| format!("cannot write {}", config_path.display()))?;
    Ok(())
}

/// `approver` and `notify` are the UI-facing halves of the permission gate:
/// the plain REPL passes a blocking stdin prompt + stderr printer, the TUI a
/// popup + transcript note.
fn build_permissions(
    args: &CliArgs,
    approver: Arc<dyn Approver>,
    notify: kloop_tui::NoteFn,
) -> Result<Permissions> {
    // --mock runs a canned turn with nobody at the keyboard: no gating at
    // all. --yolo is bypass mode — deny rules and safety checks still apply.
    if args.mock {
        return Ok(Permissions::allow_all());
    }
    let mode = if args.yolo {
        Mode::Bypass
    } else if args.accept_edits {
        Mode::AcceptEdits
    } else {
        Mode::Default
    };
    let config_path = PathBuf::from(PERMISSIONS_CONFIG);
    let rules = load_permission_rules(&config_path)?;
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    let persist =
        Box::new(
            move |rules: &[String]| match persist_allow_rules(&config_path, rules) {
                Ok(()) => notify(&format!(
                    "saved to {PERMISSIONS_CONFIG}: {}",
                    rules.join(", ")
                )),
                Err(e) => notify(&format!("failed to save allow rule: {e:#}")),
            },
        );
    Permissions::new(mode, &rules, cwd, Some(approver), Some(persist))
        .context("invalid permission rules (config.toml / AGENT_ALLOW / AGENT_DENY / AGENT_ASK)")
}

/// `[sandbox]` in `.kloop/config.toml`: `enabled` (default true),
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

/// Custom agent types from `.kloop/config.toml` `[agents.<name>]`: each is a
/// table with a required `description` and optional `system` / `model` /
/// `tools` (string array). Order follows the file so the task description
/// lists them stably. `--mock` skips this like every other config read.
fn load_agent_types(config_path: &Path) -> Result<Vec<AgentType>> {
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return Ok(Vec::new());
    };
    let value: toml::Table = raw
        .parse()
        .with_context(|| format!("cannot parse {}", config_path.display()))?;
    let Some(agents) = value.get("agents") else {
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

fn load_sandbox_settings(config_path: &Path) -> Result<SandboxSettings> {
    let mut settings = SandboxSettings {
        enabled: true,
        allow_network: false,
        writable_roots: Vec::new(),
        auto_allow: true,
        escalate: true,
    };
    let Ok(raw) = std::fs::read_to_string(config_path) else {
        return Ok(settings);
    };
    let value: toml::Table = raw
        .parse()
        .with_context(|| format!("cannot parse {}", config_path.display()))?;
    let Some(section) = value.get("sandbox") else {
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

/// The session sandbox policy, or None with a warning when unavailable —
/// fail-open like hooks: the permission gate stays the enforcement layer.
/// Built once per process and shared into every Config (server threads too).
fn build_sandbox(
    args: &CliArgs,
    cwd: &Path,
    warn: impl Fn(&str),
) -> Result<Option<Arc<kloop_core::sandbox::SandboxPolicy>>> {
    // --mock stays hermetic; AGENT_SANDBOX=off is the env escape hatch.
    if args.mock
        || matches!(
            std::env::var("AGENT_SANDBOX").ok().as_deref(),
            Some("off") | Some("0") | Some("false")
        )
    {
        return Ok(None);
    }
    let settings = load_sandbox_settings(Path::new(PERMISSIONS_CONFIG))?;
    if !settings.enabled {
        return Ok(None);
    }
    match kloop_core::sandbox::availability() {
        Ok(()) => {
            let mut policy = kloop_core::sandbox::SandboxPolicy::workspace(
                cwd,
                &settings.writable_roots,
                settings.allow_network,
            );
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

/// AGENT_DEFER_THRESHOLD: total tool count above which MCP tool definitions
/// are deferred behind tool_search. Lower it to exercise deferral with a
/// small server; raise it to effectively disable deferral.
fn defer_threshold_from_env() -> Result<usize> {
    match std::env::var("AGENT_DEFER_THRESHOLD").ok() {
        Some(raw) => raw
            .parse::<usize>()
            .context("AGENT_DEFER_THRESHOLD must be a tool count"),
        None => Ok(kloop_core::tools::TOOL_DEFER_THRESHOLD),
    }
}

fn config_from_env(
    args: &CliArgs,
    approver: Arc<dyn Approver>,
    notify: kloop_tui::NoteFn,
    tool_sources: &[Arc<dyn ToolSource>],
    project: &context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    agent_types: Arc<Vec<AgentType>>,
) -> Result<Config> {
    let permissions = Arc::new(build_permissions(args, approver, notify)?);
    // --mock stays hermetic: no config reads, no hook child processes.
    let hooks = if args.mock {
        Hooks::none()
    } else {
        Hooks {
            defs: load_hooks(Path::new(PERMISSIONS_CONFIG))?,
        }
    };
    let offload_dir = PathBuf::from(".kloop/offload");
    // AGENT_CONTEXT_WINDOW: token budget for compaction ("off" disables).
    let context_window = match std::env::var("AGENT_CONTEXT_WINDOW").ok().as_deref() {
        Some("off") | Some("0") => None,
        Some(raw) => Some(
            raw.parse::<u64>()
                .context("AGENT_CONTEXT_WINDOW must be a token count or 'off'")?,
        ),
        None => Some(200_000),
    };
    let base = Config {
        provider: Arc::new(Provider::mock(vec![])),
        model: "mock".into(),
        system: project.system.clone(),
        project_instructions: project.instructions.clone(),
        max_rounds: 30,
        offload_dir,
        context_window,
        fallback_model: std::env::var("AGENT_FALLBACK_MODEL").ok(),
        permissions,
        tool_sources: tool_sources.to_vec(),
        // The caller stamps the real session id once it knows it (after
        // open_history / per server thread).
        session_id: String::new(),
        agent_label: String::new(),
        hooks: Arc::new(hooks),
        background_shells: kloop_core::tools::BackgroundShells::new(),
        sandbox,
        agent_types,
        tool_allowlist: None,
        defer_threshold: defer_threshold_from_env()?,
        unlocked_tools: Default::default(),
        todos: Default::default(),
    };
    if args.mock {
        return Ok(Config {
            provider: Arc::new(Provider::mock(mock_demo_turns())),
            ..base
        });
    }

    let anthropic = || -> Result<Config> {
        let key = std::env::var("ANTHROPIC_API_KEY").context("ANTHROPIC_API_KEY not set")?;
        // Prompt caching is a pure cost saving, so it defaults on; the escape
        // hatch is for diagnosing cache behavior against a live endpoint.
        let cache = !matches!(
            std::env::var("AGENT_CACHE").ok().as_deref(),
            Some("off") | Some("0") | Some("false")
        );
        // No AGENT_THINKING = no thinking field: current models then run
        // adaptive on their own. The blocks they send are replayed either way.
        let thinking = match std::env::var("AGENT_THINKING").ok().as_deref() {
            None => ThinkingMode::Unset,
            Some("off") => ThinkingMode::Off,
            Some("adaptive") => ThinkingMode::Adaptive,
            Some(raw) => ThinkingMode::Budget(raw.parse().context(
                "AGENT_THINKING must be off | adaptive | <budget tokens for pre-adaptive models>",
            )?),
        };
        Ok(Config {
            provider: Arc::new(Provider::Anthropic {
                key,
                base: std::env::var("ANTHROPIC_BASE_URL")
                    .unwrap_or_else(|_| "https://api.anthropic.com".into()),
                cache,
                thinking,
            }),
            model: std::env::var("AGENT_MODEL").unwrap_or_else(|_| "claude-sonnet-5".into()),
            ..base.clone()
        })
    };
    let openai = |responses: bool| -> Result<Config> {
        let key = std::env::var("OPENAI_API_KEY").context("OPENAI_API_KEY not set")?;
        let model = std::env::var("AGENT_MODEL")
            .context("AGENT_MODEL is required for the openai providers")?;
        let base_url =
            std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".into());
        let provider = if responses {
            Provider::OpenAiResponses {
                key,
                base: base_url,
                // Also the reasoning-capture switch: without the field some
                // backends never emit reasoning items.
                effort: std::env::var("AGENT_EFFORT").ok(),
            }
        } else {
            Provider::OpenAiCompat {
                key,
                base: base_url,
            }
        };
        Ok(Config {
            provider: Arc::new(provider),
            model,
            ..base.clone()
        })
    };

    match std::env::var("AGENT_PROVIDER").ok().as_deref() {
        Some("anthropic") => anthropic(),
        Some("openai") | Some("openai-compat") => openai(false),
        Some("openai-responses") => openai(true),
        Some(other) => {
            bail!("unknown AGENT_PROVIDER '{other}' (anthropic | openai | openai-responses)")
        }
        None => {
            if let Ok(cfg) = anthropic() {
                Ok(cfg)
            } else if let Ok(cfg) = openai(false) {
                Ok(cfg)
            } else {
                bail!(
                    "no provider configured: set ANTHROPIC_API_KEY or OPENAI_API_KEY \
                         (+ AGENT_MODEL), or run with --mock"
                )
            }
        }
    }
}

/// ANSI-color a diff preview for the plain REPL: additions green, deletions
/// red, everything else (context, hunk gaps, markers) dim.
fn color_diff(preview: &str) -> String {
    preview
        .lines()
        .map(|line| match line.chars().next() {
            Some('+') => format!("\x1b[32m{line}\x1b[0m"),
            Some('-') => format!("\x1b[31m{line}\x1b[0m"),
            _ => format!("\x1b[2m{line}\x1b[0m"),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Interactive y/a/p/n prompt on the terminal. The REPL's own stdin reader
/// is idle while a turn runs, so a direct blocking read is safe; if the turn
/// is Ctrl+C-interrupted mid-prompt, the orphaned read may swallow one
/// subsequent input line — accepted edge for a line-based REPL.
#[derive(Default)]
struct CliApprover {
    /// Parallel sub-agents ask concurrently; one prompt owns the terminal at
    /// a time, the rest wait here (the TUI gets the same via its queue).
    prompting: tokio::sync::Mutex<()>,
}

impl Approver for CliApprover {
    fn confirm(
        &self,
        req: ConfirmRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Decision> + Send + '_>> {
        Box::pin(async move {
            let _one_at_a_time = self.prompting.lock().await;
            let options = match &req.remember_rules {
                Some(rules) => format!(
                    "y = allow once / a = allow for this session / p = allow always (saves {} to {PERMISSIONS_CONFIG}) / n = deny",
                    rules.join(", ")
                ),
                None => "y = allow once / n = deny".to_string(),
            };
            let preview = req
                .preview
                .as_deref()
                .map(|p| format!("\n{}", color_diff(p)))
                .unwrap_or_default();
            print!("\n[approve?] {}{preview}\n  {options} > ", req.description);
            let _ = std::io::stdout().flush();
            let line = tokio::task::spawn_blocking(|| {
                let mut buf = String::new();
                std::io::stdin().read_line(&mut buf).map(|_| buf)
            })
            .await;
            match line {
                // 'a'/'p' on a non-remember-able call degrade to allow-once
                // in the gate (it ignores the remember part), matching the
                // user's evident intent to allow.
                Ok(Ok(answer)) => match answer.trim().to_lowercase().as_str() {
                    "y" | "yes" => Decision::Allow,
                    "a" | "always" => Decision::AllowSession,
                    "p" | "persist" => Decision::AllowAlways,
                    _ => Decision::Deny,
                },
                // Reader died or stdin closed: the safe answer is no.
                _ => Decision::Deny,
            }
        })
    }
}

struct StdoutUi;

impl Ui for StdoutUi {
    fn text_delta(&self, s: &str) {
        print!("{s}");
        let _ = std::io::stdout().flush();
    }

    fn thinking_delta(&self, s: &str) {
        // Dim gray, inline with the stream: reasoning is context, not answer.
        print!("\x1b[2m{s}\x1b[0m");
        let _ = std::io::stdout().flush();
    }

    fn note(&self, s: &str) {
        eprintln!("\x1b[2m[{s}]\x1b[0m");
    }

    fn tool_start(&self, agent: &str, _id: &str, name: &str, summary: &str) {
        // todo_write is rendered as a checklist by todo_update, not a note.
        if name == "todo_write" {
            return;
        }
        if agent.is_empty() {
            self.note(&format!("{name} {summary}"));
        } else {
            self.note(&format!("{agent} · {name} {summary}"));
        }
    }

    fn todo_update(&self, agent: &str, todos: &[kloop_core::tools::TodoItem]) {
        use kloop_core::tools::TodoStatus;
        let prefix = if agent.is_empty() {
            String::new()
        } else {
            format!("{agent} · ")
        };
        eprintln!("\x1b[2m[{prefix}todos]\x1b[0m");
        for todo in todos {
            let (mark, text) = match todo.status {
                TodoStatus::Completed => ("✓", &todo.content),
                TodoStatus::InProgress => ("▶", &todo.active_form),
                TodoStatus::Pending => ("○", &todo.content),
            };
            eprintln!("\x1b[2m  {mark} {text}\x1b[0m");
        }
    }
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

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args = parse_args(&args)?;
    let sessions_dir = PathBuf::from(".kloop/sessions");
    if args.list_sessions {
        list_sessions(&sessions_dir);
        return Ok(());
    }
    // MCP servers connect once per process (before any UI owns the terminal)
    // and are shared into every Config — including all server-mode threads.
    // --mock stays hermetic: no child processes, no config reads.
    let tool_sources = if args.mock {
        Vec::new()
    } else {
        let warn = |s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m");
        // Web tools ride the same ToolSource seam, registered before MCP so
        // a colliding MCP tool name loses (and is warned about).
        let web_cfg = web::load_web_config(Path::new(PERMISSIONS_CONFIG))?;
        let mut sources: Vec<Arc<dyn ToolSource>> = Vec::new();
        sources.extend(web::build_web_source(&web_cfg, &warn));
        let servers = mcp::load_mcp_servers(Path::new(PERMISSIONS_CONFIG))?;
        sources.extend(mcp::connect_servers(servers, &warn).await);
        for warning in tool_merge_warnings(&sources, defer_threshold_from_env()?) {
            warn(&warning);
        }
        sources
    };
    // Project context (instruction files, env block, git snapshot) is
    // gathered once per process and shared into every Config the same way.
    let cwd = std::env::current_dir().context("cannot determine cwd")?;
    let project = if args.mock {
        context::mock(&cwd)
    } else {
        context::gather(&cwd)
    };
    for warning in &project.warnings {
        eprintln!("\x1b[2m[{warning}]\x1b[0m");
    }
    // The sandbox policy is process-stable (cwd + config), so it is built
    // once and shared into every Config — server threads included.
    let sandbox = build_sandbox(&args, &cwd, |s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m"))?;
    // Agent types are likewise config-derived and process-stable; --mock stays
    // hermetic (no config reads).
    let agent_types = Arc::new(if args.mock {
        Vec::new()
    } else {
        load_agent_types(Path::new(PERMISSIONS_CONFIG))?
    });
    if args.serve {
        // Multi-session JSON-RPC server on stdio; each thread gets its own
        // Config (and thus its own permission gate + session cache).
        let factory: kloop_server::ConfigFactory = {
            let args = args.clone();
            Arc::new(move |approver, notify| {
                config_from_env(
                    &args,
                    approver,
                    notify,
                    &tool_sources,
                    &project,
                    sandbox.clone(),
                    agent_types.clone(),
                )
            })
        };
        return kloop_server::serve_stdio(
            factory,
            kloop_server::ServerPaths {
                sessions_dir,
                offload_dir: PathBuf::from(".kloop/offload"),
            },
        )
        .await;
    }
    let (history, session_id) = open_history(
        PathBuf::from(".kloop/offload"),
        &args.session,
        &sessions_dir,
    )?;

    // The TUI is the default entry point; --plain keeps the line-based REPL,
    // and --mock's scripted demo stays on plain output where it is readable.
    if args.mock || args.plain {
        return plain_main(
            args,
            history,
            session_id,
            tool_sources,
            project,
            sandbox,
            agent_types,
        )
        .await;
    }
    let factory_session_id = session_id.clone();
    kloop_tui::run(
        move |approver, notify| {
            let mut cfg = config_from_env(
                &args,
                approver,
                notify,
                &tool_sources,
                &project,
                sandbox.clone(),
                agent_types.clone(),
            )?;
            cfg.session_id = factory_session_id.clone();
            Ok(cfg)
        },
        history,
        session_id,
    )
    .await
}

async fn plain_main(
    args: CliArgs,
    mut history: History,
    session_id: String,
    tool_sources: Vec<Arc<dyn ToolSource>>,
    project: context::GatheredContext,
    sandbox: Option<Arc<kloop_core::sandbox::SandboxPolicy>>,
    agent_types: Arc<Vec<AgentType>>,
) -> Result<()> {
    let notify: kloop_tui::NoteFn = Arc::new(|s: &str| eprintln!("\x1b[2m[{s}]\x1b[0m"));
    let mut cfg = config_from_env(
        &args,
        Arc::new(CliApprover::default()),
        notify,
        &tool_sources,
        &project,
        sandbox,
        agent_types,
    )?;
    cfg.session_id = session_id.clone();
    let cfg = Arc::new(cfg);
    let ui: Arc<dyn Ui> = Arc::new(StdoutUi);

    if args.mock {
        history.record(Message::user_text("run the demo"));
        let cancel = CancellationToken::new();
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        println!(
            "\n--- mock run: {:?} after {} round(s) ---",
            outcome.reason, outcome.rounds
        );
        return Ok(());
    }

    println!(
        "kloop — session {session_id}; type a task, 'exit' or Ctrl+D to quit, \
         Ctrl+C to interrupt a running turn"
    );
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let Some(line) = lines.next_line().await? else {
            break;
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }
        if line == "exit" {
            break;
        }

        history.record(Message::user_text(line));
        let cancel = CancellationToken::new();
        let watcher = {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                if tokio::signal::ctrl_c().await.is_ok() {
                    cancel.cancel();
                }
            })
        };
        let outcome = run_turn(&cfg, &mut history, &ui, &cancel, 0).await;
        watcher.abort();
        println!();
        match outcome.reason {
            EndReason::Completed => {}
            EndReason::MaxRounds => println!("[stopped: hit max rounds ({})]", cfg.max_rounds),
            EndReason::Aborted => {
                println!("[interrupted — history patched; Ctrl+D or 'exit' to quit]")
            }
            EndReason::Error(e) => println!("[error: {e}]"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn color_diff_wraps_lines_by_sign() {
        assert_eq!(
            color_diff("+1  add\n-2  del\n 3  ctx"),
            "\x1b[32m+1  add\x1b[0m\n\x1b[31m-2  del\x1b[0m\n\x1b[2m 3  ctx\x1b[0m"
        );
    }

    #[test]
    fn parse_args_covers_all_flags() {
        assert_eq!(
            parse_args(&[]).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--mock", "--resume"])).unwrap(),
            CliArgs {
                mock: true,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::Pick,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--continue"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::Continue,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--serve"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: true,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--resume", "20260709-120000", "--plain"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: true,
                serve: false,
                session: SessionChoice::Resume("20260709-120000".into()),
            }
        );
        assert_eq!(
            parse_args(&strings(&["--list-sessions", "--yolo", "--accept-edits"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: true,
                accept_edits: true,
                list_sessions: true,
                plain: false,
                serve: false,
                session: SessionChoice::New,
            }
        );
        assert_eq!(
            parse_args(&strings(&["--fork", "20260709-120000#4"])).unwrap(),
            CliArgs {
                mock: false,
                yolo: false,
                accept_edits: false,
                list_sessions: false,
                plain: false,
                serve: false,
                session: SessionChoice::Fork {
                    id: "20260709-120000".into(),
                    cut: Some(4),
                },
            }
        );
        assert_eq!(
            parse_args(&strings(&["--fork", "20260709-120000"]))
                .unwrap()
                .session,
            SessionChoice::Fork {
                id: "20260709-120000".into(),
                cut: None,
            }
        );
        assert!(parse_args(&strings(&["--fork"])).is_err(), "id required");
        assert!(
            parse_args(&strings(&["--fork", "id#notanumber"])).is_err(),
            "seq must parse"
        );
        assert!(parse_args(&strings(&["--bogus"])).is_err());
    }

    #[test]
    fn agent_types_parse_fields_and_reject_malformed() {
        let dir = std::env::temp_dir().join(format!("kloop-agents-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Missing file / no [agents]: empty.
        assert!(load_agent_types(&path).unwrap().is_empty());
        std::fs::write(&path, "[permissions]\nallow = []\n").unwrap();
        assert!(load_agent_types(&path).unwrap().is_empty());

        std::fs::write(
            &path,
            "[agents.researcher]\n\
             description = \"Searches the codebase.\"\n\
             system = \"You research.\"\n\
             model = \"claude-haiku-4-5\"\n\
             tools = [\"grep\", \"read_file\"]\n\
             \n\
             [agents.reviewer]\n\
             description = \"Reviews a diff.\"\n",
        )
        .unwrap();
        let types = load_agent_types(&path).unwrap();
        assert_eq!(types.len(), 2);
        let researcher = types.iter().find(|t| t.name == "researcher").unwrap();
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
        let reviewer = types.iter().find(|t| t.name == "reviewer").unwrap();
        assert_eq!(reviewer.system, None);
        assert_eq!(reviewer.model, None);
        assert_eq!(reviewer.tools, None);

        for bad in [
            "[agents.x]\n",                                        // no description
            "[agents.x]\ndescription = 3\n",                       // wrong type
            "[agents.x]\ndescription = \"d\"\nmodel = 5\n",        // wrong type
            "[agents.x]\ndescription = \"d\"\ntools = \"grep\"\n", // tools not an array
            "[agents.x]\ndescription = \"d\"\ntools = [3]\n",      // tools not strings
            "[agents.x]\ndescription = \"d\"\nprompt = \"p\"\n",   // unknown key
            "agents = 3\n",                                        // [agents] not a table
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(load_agent_types(&path).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn sandbox_settings_parse_defaults_and_reject_unknown_keys() {
        let dir = std::env::temp_dir().join(format!("kloop-sbxcfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Missing file / missing section: sandbox on, network off, no
        // extras, auto-allow on, escalate on.
        let settings = load_sandbox_settings(&path).unwrap();
        assert!(settings.enabled);
        assert!(!settings.allow_network);
        assert!(settings.writable_roots.is_empty());
        assert!(settings.auto_allow);
        assert!(settings.escalate);
        std::fs::write(&path, "[permissions]\nallow = []\n").unwrap();
        assert!(load_sandbox_settings(&path).unwrap().enabled);

        std::fs::write(
            &path,
            "[sandbox]\nenabled = true\nallow_network = true\nwritable_roots = [\"/opt/data\"]\n",
        )
        .unwrap();
        let settings = load_sandbox_settings(&path).unwrap();
        assert!(settings.enabled);
        assert!(settings.allow_network);
        assert_eq!(settings.writable_roots, vec![PathBuf::from("/opt/data")]);

        std::fs::write(&path, "[sandbox]\nenabled = false\n").unwrap();
        assert!(!load_sandbox_settings(&path).unwrap().enabled);

        std::fs::write(&path, "[sandbox]\nauto_allow = false\n").unwrap();
        assert!(!load_sandbox_settings(&path).unwrap().auto_allow);

        std::fs::write(&path, "[sandbox]\nescalate = false\n").unwrap();
        assert!(!load_sandbox_settings(&path).unwrap().escalate);

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
            std::fs::write(&path, bad).unwrap();
            assert!(load_sandbox_settings(&path).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn permission_config_round_trip_and_merge() {
        let dir = std::env::temp_dir().join(format!("kloop-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Missing file: empty rules, no error.
        assert_eq!(
            load_permission_rules(&path).unwrap(),
            PermissionRules::default()
        );

        // Persist into a file that has unrelated content to preserve.
        std::fs::write(
            &path,
            "[provider]\nname = \"anthropic\"\n\n[permissions]\ndeny = [\"bash(git push *)\"]\n",
        )
        .unwrap();
        persist_allow_rules(&path, &["bash(cargo build *)".into()]).unwrap();
        persist_allow_rules(&path, &["bash(cargo build *)".into()]).unwrap(); // dedup

        let rules = load_permission_rules(&path).unwrap();
        assert_eq!(
            rules,
            PermissionRules {
                allow: vec!["bash(cargo build *)".into()],
                deny: vec!["bash(git push *)".into()],
                ask: vec![],
            }
        );
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(
            raw.contains("[provider]"),
            "unrelated sections preserved:\n{raw}"
        );
        assert_eq!(raw.matches("cargo build").count(), 1, "no duplicate rule");

        // Malformed arrays are an error, not a silent skip.
        std::fs::write(&path, "[permissions]\nallow = \"not-an-array\"\n").unwrap();
        assert!(load_permission_rules(&path).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_hooks_full_round_trip() {
        let dir = std::env::temp_dir().join(format!("kloop-hooks-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");

        // Missing file / missing section: empty, no error.
        assert_eq!(
            load_hooks(Path::new("/nonexistent/kloop.toml")).unwrap(),
            vec![]
        );
        std::fs::write(&path, "[permissions]\nallow = []\n").unwrap();
        assert_eq!(load_hooks(&path).unwrap(), vec![]);

        std::fs::write(
            &path,
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
        )
        .unwrap();
        assert_eq!(
            load_hooks(&path).unwrap(),
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
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn load_hooks_rejects_malformed_entries() {
        let dir = std::env::temp_dir().join(format!("kloop-hooks-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
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
        ] {
            std::fs::write(&path, bad).unwrap();
            assert!(load_hooks(&path).is_err(), "{tag} should fail");
        }
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pick_index_covers_empty_valid_and_garbage() {
        assert_eq!(pick_index("", 5).unwrap(), 0);
        assert_eq!(pick_index("  \n", 5).unwrap(), 0);
        assert_eq!(pick_index("1", 5).unwrap(), 0);
        assert_eq!(pick_index(" 5 \n", 5).unwrap(), 4);
        assert!(pick_index("0", 5).is_err());
        assert!(pick_index("6", 5).is_err());
        assert!(pick_index("abc", 5).is_err());
    }
}
