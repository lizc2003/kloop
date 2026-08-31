//! The tool seam: definitions, per-input concurrency classification, and
//! the gated dispatch loop (hooks → permissions → execution). Individual
//! tool implementations live in the sibling modules; this file is what the
//! agent loop and the frontends depend on.

mod agent_message;
mod background_executions;
mod bash;
mod codemode;
mod fs;
mod inject;
pub(crate) mod notebook;
#[cfg(test)]
mod plan49_parity_tests;
#[cfg(test)]
mod plan50_parity_tests;
#[cfg(test)]
mod plan52_parity_tests;
#[cfg(test)]
mod plan53_parity_tests;
#[cfg(test)]
mod plan56_parity_tests;
#[cfg(test)]
mod plan57_parity_tests;
#[cfg(test)]
mod plan58_parity_tests;
#[cfg(test)]
mod plan59_acceptance_tests;
mod plan_mode;
mod powershell;
mod provenance_store;
mod question;
mod run_store;
mod scheduler;
mod search;
mod skill;
mod subagent;
mod task;
mod tool_search;
pub mod web;
mod workflow;
mod worktree_tool;

pub use background_executions::BackgroundExecutions;
pub use background_executions::ExecutionStatus;
pub use bash::BackgroundShells;
pub(crate) use codemode::ProgramToolManifest;
pub(crate) use codemode::capture_program_tool_manifest;
pub use tool_search::DeferredToolUnlocks;
// Slash-path prompt injections (`!cmd` / `@file`) for `/name` commands; the
// dispatch layer (`crate::commands`) calls this before running the turn.
pub(crate) use inject::expand_slash_injections;
// Registered on the run_agent peer set only at depth 0 with skills loaded
// (see `turn_rounds`); the pure skill logic it drives lives in `crate::skills`.
pub(crate) use skill::skill_tool_def;
pub use tool_search::deferred_notice;
// The skills module (`crate::skills`) dispatches a `context: fork` skill here,
// reusing the run_agent sub-agent machinery.
pub(crate) use subagent::fork_skill;
pub use task::TaskGraphSnapshot;
pub use task::TaskGraphTask;
pub use task::TaskRegistry;
pub use task::TaskStatus;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::agent::Ui;
use crate::config::Config;
use crate::config::EffectiveWorkspace;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
use crate::shell_programs::ShellPrograms;
use kloop_protocol::ContentBlock;
use kloop_protocol::ToolDef;
use kloop_protocol::ToolResultContent;

/// Past this many tools the definitions would crowd the context window, so
/// source tools are deferred behind tool_search instead of being sent.
/// Default for `Config.defer_threshold` (`KLOOP_DEFER_THRESHOLD` overrides).
pub const TOOL_DEFER_THRESHOLD: usize = 30;

/// An external provider of tools (Web tools, an MCP server, or another CLI
/// adapter). Core only knows this seam; transport and network implementations
/// live outside core. Implementations expose unique tool names; MCP adapters
/// namespace theirs as `{server}__{tool}` so cross-server collisions are config
/// mistakes, not the common case.
///
/// What a [`ToolSource`] call yields: the flattened `text` a tool_result carries
/// (the model-facing path) plus an optional `structured` value a code-mode
/// program receives instead — an MCP tool's raw `CallToolResult` object, so a
/// program can read `.structuredContent` / `.content` without parsing text.
/// `structured: None` → a program gets `text` as a JS string, like a built-in.
pub struct SourceOutput {
    pub text: String,
    /// Present when the result carries content the flattened `text` cannot hold
    /// — an MCP tool that returned an image. `Some(blocks)` makes the
    /// tool_result an image-bearing block array; `None` uses `text`.
    pub blocks: Option<Vec<ContentBlock>>,
    pub structured: Option<Value>,
}

impl SourceOutput {
    /// A text-only result (web tools, stubs): no structured form to hand a program.
    pub fn text(text: String) -> Self {
        Self {
            text,
            blocks: None,
            structured: None,
        }
    }

    /// The tool_result content this result becomes: an image-bearing block array
    /// when the source returned images, else the flattened text.
    pub fn into_content(self) -> ToolResultContent {
        match self.blocks {
            Some(blocks) => ToolResultContent::Blocks(blocks),
            None => ToolResultContent::Text(self.text),
        }
    }
}

/// Definition/catalog and live-readiness version captured as one source-owned
/// snapshot. Both axes must still match at the wire boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct SourceVersion {
    pub definition_generation: u64,
    pub readiness_revision: u64,
}

/// A source's verdict for one exact route. Dynamic adapters override this to
/// atomically pair a definition with live readiness; static sources use the
/// default implementation.
#[derive(Clone, Debug)]
pub enum SourceDefinitionState {
    Missing,
    Available {
        definition: ToolDef,
        version: SourceVersion,
    },
    Unavailable {
        reason: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SourceRouteState {
    Missing,
    Available(SourceCallBinding),
    Unavailable(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SourceCallBinding {
    source_slot: usize,
    version: SourceVersion,
}

struct SourceDefinitionSnapshot {
    definition: ToolDef,
    binding: SourceCallBinding,
}

struct ToolExecution {
    result: Result<ToolResultContent>,
    file_state_update: Option<(
        Arc<crate::file_state::FileState>,
        crate::file_state::FileStateUpdate,
    )>,
    path_lock: Option<tokio::sync::OwnedMutexGuard<()>>,
}

struct PreparedExecution<'a> {
    read: Option<&'a fs::PreparedRead>,
    mutation: Option<&'a fs::PreparedMutation>,
    source: Option<SourceCallBinding>,
    local_send_committed: &'a AtomicBool,
}

impl ToolExecution {
    fn from_result(result: Result<ToolResultContent>) -> Self {
        Self {
            result,
            file_state_update: None,
            path_lock: None,
        }
    }
}

pub trait ToolSource: Send + Sync {
    /// Tool definitions as advertised by the source (schema passed through).
    fn defs(&self) -> Arc<[ToolDef]>;
    fn should_defer(&self, _tool: &str) -> bool {
        false
    }
    /// Monotonic definition generation for a tool. Dynamic sources increment
    /// this only after atomically publishing a replacement catalog; tool_search
    /// binds each unlock to the generation whose schema it returned.
    fn definition_generation(&self, _tool: &str) -> u64 {
        0
    }
    /// One definition and the generation it belongs to. Dynamic sources
    /// override this so tool_search cannot bind a pre-refresh schema to a
    /// post-refresh generation.
    fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
        self.defs()
            .iter()
            .find(|def| def.name == tool)
            .cloned()
            .map(|def| (def, self.definition_generation(tool)))
    }
    /// Monotonic live-readiness revision. Static sources remain at zero.
    fn readiness_revision(&self, _tool: &str) -> u64 {
        0
    }
    /// Source-wide version used to prove that a provider request's generated
    /// Program API and callable manifest came from the same lifecycle snapshot.
    fn catalog_version(&self) -> SourceVersion {
        SourceVersion {
            definition_generation: self.definition_generation(""),
            readiness_revision: self.readiness_revision(""),
        }
    }
    /// Atomically classify one exact source route. An unavailable verdict is
    /// distinct from Missing so configured-but-failed sources cannot degrade to
    /// an unknown-tool error.
    fn definition_state(&self, tool: &str) -> SourceDefinitionState {
        match self.definition_snapshot(tool) {
            Some((definition, definition_generation)) => SourceDefinitionState::Available {
                definition,
                version: SourceVersion {
                    definition_generation,
                    readiness_revision: self.readiness_revision(tool),
                },
            },
            None => SourceDefinitionState::Missing,
        }
    }
    /// Source-specific fail-fast validation performed before pre-tool hooks and
    /// permission. It must be side-effect free; the source still revalidates at
    /// the wire boundary after any approval wait.
    fn preflight(&self, _tool: &str, _input: &Value) -> Result<()> {
        Ok(())
    }
    /// Whether this tool was explicitly marked read-only in config, making
    /// it eligible for concurrent dispatch. External tools default to NOT
    /// read-only — serial. (The permission gate is independent: external
    /// tools always ask unless covered by an allow rule or session cache.)
    fn is_readonly(&self, tool: &str) -> bool;
    /// Execute one call. `Ok(SourceOutput)` carries the tool_result text (and an
    /// optional structured form for programs); `Err(reason)` becomes is_error.
    /// Type-erased future for object safety, same shape as `execute_tool`.
    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>>;
    /// Execute against the definition generation the caller discovered. Static
    /// sources use the default; dynamic sources reject a stale generation before
    /// routing the input to a replacement schema.
    fn call_at_generation<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        _generation: Option<u64>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        self.call(tool, input)
    }
    /// Execute against both catalog generation and live-readiness revision.
    /// Dynamic sources override this so the final check and wire request share
    /// their lifecycle gate; static sources delegate to the generation seam.
    fn call_at_version<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
        version: Option<SourceVersion>,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            match self.definition_state(tool) {
                SourceDefinitionState::Missing => bail!("unknown source tool: {tool}"),
                SourceDefinitionState::Unavailable { reason } => bail!(reason),
                SourceDefinitionState::Available {
                    version: current, ..
                } if version.is_some_and(|expected| expected != current) => {
                    bail!(
                        "tool '{tool}' source readiness or definition changed after discovery; rediscover it before retrying"
                    )
                }
                SourceDefinitionState::Available { .. } => {
                    self.call_at_generation(
                        tool,
                        input,
                        version.map(|version| version.definition_generation),
                    )
                    .await
                }
            }
        })
    }
}

/// Per-call sink a code-mode program call carries: a tool drops a structured
/// value here (an MCP `CallToolResult`, or a built-in's natural array like
/// glob's path list) so the program receives it instead of the flattened text
/// `run_one` returns. One slot per call, so concurrent program calls never race.
pub type ProgramResultSink = Arc<std::sync::Mutex<Option<Value>>>;

/// Everything a tool execution needs; cheap to clone into spawned futures.
#[derive(Clone)]
pub struct ToolCtx {
    pub cfg: Arc<Config>,
    pub ui: Arc<dyn Ui>,
    pub cancel: CancellationToken,
    pub depth: u8,
    /// The execution enclosing this tool call. Root turns have none; admitted
    /// Agent turns and Program/Workflow bridges replace it with their fixed ref.
    pub(crate) enclosing_execution: Option<crate::execution_provenance::ExecutionRef>,
    /// stdout of allowing tool hooks, collected here because run_one has no
    /// history access; the agent loop drains it into history after the
    /// round's tool results are recorded.
    pub hook_context: Arc<std::sync::Mutex<Vec<String>>>,
    /// True for calls a `run_program` program fires through the code-mode
    /// bridge. Such a call skips the deferred-tool lock gate: the tool is
    /// already exposed on the program's `tools` object, so it is "loaded" for
    /// the program's purposes. Every other gate (deny rules, permission,
    /// sandbox, hooks) still applies — this is a discovery bypass, not a
    /// security one.
    pub from_program: bool,
    /// Callable source owner/generation manifest captured with the provider
    /// request that exposed run_program. None on non-provider/internal contexts.
    pub(crate) program_tool_manifest: Option<Arc<ProgramToolManifest>>,
    /// Id of the parent session's line that this round's assistant message was
    /// recorded as (`{stem}#{seq}`), or None for an in-memory-only session. The
    /// run_agent stamps it as the spawned sub-agent's `subagent_of`
    /// back-pointer. Constant across a round's concurrent tool calls.
    pub parent_rollout_id: Option<String>,
    /// Per-call sink the code-mode bridge sets so a tool's structured result
    /// (an MCP `CallToolResult`, or a built-in's natural array) reaches the
    /// program instead of the flattened text `run_one` returns. `execute_tool`
    /// fills it on the relevant tools; the bridge reads it after `run_one`.
    pub program_result: Option<ProgramResultSink>,
}

/// Built-ins plus external sources, in registration order. A name collision
/// (with a built-in or an earlier source) drops the later definition; the
/// CLI surfaces the same collisions as startup warnings via
/// [`tool_merge_warnings`].
///
/// Past `defer_threshold` total tools the source defs are withheld: the model
/// gets built-ins + tool_search only, and loads deferred definitions through
/// search results. Unlocking does not mutate the returned array; a dynamic
/// source refresh is picked up when the agent rebuilds the next round.
pub fn all_tool_defs(
    depth: u8,
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    surface: crate::config::SurfaceCapabilities,
    shell_programs: &ShellPrograms,
) -> Vec<ToolDef> {
    let mut defs = builtin_defs(depth, shell_programs);
    let merged = merged_source_defs(sources);
    let deferred = deferred_tool_defs(sources, defer_threshold, shell_programs);
    let deferred_names: std::collections::HashSet<&str> =
        deferred.iter().map(|def| def.name.as_str()).collect();
    if !deferred.is_empty() {
        defs.push(tool_search::tool_search_def());
        defs.push(tool_search::call_tool_def());
    }
    let inline_sources: Vec<ToolDef> = merged
        .into_iter()
        .filter(|def| !deferred_names.contains(def.name.as_str()))
        .collect();
    defs.extend(inline_sources.iter().cloned());
    // run_program is depth-0 only (like run_agent). Now that sources are visible, its
    // TypeScript API can list them: full declarations for inline source tools
    // (typed `Promise<CallToolResult>`), or a compact manifest for deferred
    // ones — both callable at runtime.
    if depth == 0 {
        defs.push(codemode::run_program_def(
            &builtin_defs(0, shell_programs),
            &inline_sources,
            &deferred,
        ));
        if surface.scheduler {
            defs.push(scheduler::cron_create_def());
            defs.push(scheduler::cron_delete_def());
            defs.push(scheduler::cron_list_def());
            defs.push(scheduler::schedule_wakeup_def());
        }
        // General questions are a user-interaction surface, not a permission
        // prompt. Kept out of run_program's tools API and limited to depth 0.
        if surface.questions {
            defs.push(question::ask_user_question_def());
        }
        // Plan controls are advertised together so the request's tool array stays
        // byte-stable while the session mode changes. Both are depth-0 only and
        // intentionally absent from run_program's generated TypeScript API.
        if surface.plan_control {
            defs.push(plan_mode::enter_plan_mode_def());
            defs.push(plan_mode::exit_plan_mode_def());
        }
        if surface.workflow {
            defs.push(workflow::workflow_def());
            defs.push(workflow::stop_workflow_def());
        }
        // Session worktree tools (plan 35 slice 2): only when the front-end
        // enables worktree mode (CLI/TUI/plain — not server threads or --mock),
        // and only top-level (a sub-agent isolates via run_agent {isolation}). Kept
        // out of run_program's TS API and the deferral count on purpose.
        if surface.worktree {
            defs.push(worktree_tool::enter_worktree_def());
            defs.push(worktree_tool::exit_worktree_def());
        }
    }
    defs
}

/// Whether the current depth-0 source snapshot has deferred tools. Parent and
/// sub-agents use the same threshold; a dynamic source refresh may change the
/// verdict at the next sampling round.
pub fn defer_active(
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    shell_programs: &ShellPrograms,
) -> bool {
    !deferred_tool_defs(sources, defer_threshold, shell_programs).is_empty()
}

/// The source tools hidden behind tool_search. An oversized catalog defers all
/// merged source tools; a source may also force selected helpers to remain
/// deferred even below the global threshold.
pub fn deferred_tool_defs(
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    shell_programs: &ShellPrograms,
) -> Vec<ToolDef> {
    let merged = merged_source_defs(sources);
    if tool_defs(0, shell_programs).len() + merged.len() > defer_threshold {
        return merged;
    }
    merged
        .into_iter()
        .filter(|def| {
            find_source(sources, &def.name).is_some_and(|source| source.should_defer(&def.name))
        })
        .collect()
}

fn reserve_surface_names(seen: &mut std::collections::HashSet<String>) {
    seen.extend(
        [
            "skill",
            "tool_search",
            "call_tool",
            "ask_user_question",
            "enter_plan_mode",
            "exit_plan_mode",
            "workflow",
            "stop_workflow",
            "enter_worktree",
            "exit_worktree",
            "structured_output",
            "cron_create",
            "cron_delete",
            "cron_list",
            "schedule_wakeup",
        ]
        .into_iter()
        .map(String::from),
    );
}

fn reserved_builtin_names() -> std::collections::HashSet<String> {
    let mut names: std::collections::HashSet<String> = tool_defs(
        0,
        &ShellPrograms {
            bash: Some(crate::shell_programs::ShellProgram {
                executable: "reserved-bash".into(),
                flavor: crate::shell_programs::ShellFlavor::GitBash,
            }),
            powershell: Some(crate::shell_programs::ShellProgram {
                executable: "reserved-powershell".into(),
                flavor: crate::shell_programs::ShellFlavor::PowerShell7,
            }),
        },
    )
    .into_iter()
    .map(|definition| definition.name)
    .collect();
    names.extend(
        [
            "bash",
            "bash_output",
            "stop_bash",
            "powershell",
            // Retired built-ins stay reserved so an MCP tool cannot impersonate an
            // old call from resumed history before the migration error fires.
            "task",
            "wait",
            "kill_bash",
            "todo_write",
        ]
        .into_iter()
        .map(String::from),
    );
    names
}

fn merged_source_defs(sources: &[Arc<dyn ToolSource>]) -> Vec<ToolDef> {
    let mut seen = reserved_builtin_names();
    reserve_surface_names(&mut seen);
    let mut defs = Vec::new();
    for source in sources {
        let source_defs = source.defs();
        for def in source_defs.iter() {
            if seen.insert(def.name.clone()) {
                defs.push(def.clone());
            }
        }
    }
    defs
}

/// Startup diagnostics for the merged tool set: name collisions (the later
/// definition is skipped) and a note when the deferred-tools regime kicked
/// in. Depth 0 is the authoritative view (it has the most built-ins).
pub fn tool_merge_warnings(
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    shell_programs: &ShellPrograms,
) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut seen = reserved_builtin_names();
    reserve_surface_names(&mut seen);
    for source in sources {
        let source_defs = source.defs();
        for def in source_defs.iter() {
            if !seen.insert(def.name.clone()) {
                warnings.push(format!(
                    "tool name collision: '{}' is already registered; the later definition is skipped",
                    def.name
                ));
            }
        }
    }
    let total = tool_defs(0, shell_programs).len() + merged_source_defs(sources).len();
    if total > defer_threshold {
        warnings.push(format!(
            "{total} tools registered (> {defer_threshold}); MCP tool definitions are deferred — the model loads them on demand via tool_search"
        ));
    }
    warnings
}

fn find_source_slot<'a>(
    sources: &'a [Arc<dyn ToolSource>],
    name: &str,
) -> Option<(usize, &'a Arc<dyn ToolSource>)> {
    let mut reserved = reserved_builtin_names();
    reserve_surface_names(&mut reserved);
    if reserved.contains(name) {
        return None;
    }
    // First source claiming the name wins, mirroring the merge order. The slot
    // is stable for a Config and binds a deferred receipt to that exact owner.
    sources
        .iter()
        .enumerate()
        .find(|(_, source)| source.defs().iter().any(|def| def.name == name))
}

fn find_source<'a>(
    sources: &'a [Arc<dyn ToolSource>],
    name: &str,
) -> Option<&'a Arc<dyn ToolSource>> {
    find_source_slot(sources, name).map(|(_, source)| source)
}

enum SourceResolution {
    Missing,
    Unavailable(String),
    Available {
        source_slot: usize,
        definition: ToolDef,
        version: SourceVersion,
    },
}

fn resolve_source(sources: &[Arc<dyn ToolSource>], name: &str) -> SourceResolution {
    let mut reserved = reserved_builtin_names();
    reserve_surface_names(&mut reserved);
    if reserved.contains(name) {
        return SourceResolution::Missing;
    }
    let mut unavailable = None;
    for (source_slot, source) in sources.iter().enumerate() {
        match source.definition_state(name) {
            SourceDefinitionState::Missing => {}
            SourceDefinitionState::Unavailable { reason } => {
                unavailable.get_or_insert(reason);
            }
            SourceDefinitionState::Available {
                definition,
                version,
            } => {
                return SourceResolution::Available {
                    source_slot,
                    definition,
                    version,
                };
            }
        }
    }
    unavailable.map_or(SourceResolution::Missing, SourceResolution::Unavailable)
}

fn source_definition_result(
    sources: &[Arc<dyn ToolSource>],
    name: &str,
) -> std::result::Result<Option<SourceDefinitionSnapshot>, String> {
    match resolve_source(sources, name) {
        SourceResolution::Available {
            source_slot,
            definition,
            version,
        } => Ok(Some(SourceDefinitionSnapshot {
            definition,
            binding: SourceCallBinding {
                source_slot,
                version,
            },
        })),
        SourceResolution::Missing => Ok(None),
        SourceResolution::Unavailable(reason) => Err(reason),
    }
}

fn source_definition_snapshot(
    sources: &[Arc<dyn ToolSource>],
    name: &str,
) -> Option<SourceDefinitionSnapshot> {
    source_definition_result(sources, name).ok().flatten()
}

fn source_route_state(sources: &[Arc<dyn ToolSource>], name: &str) -> SourceRouteState {
    match resolve_source(sources, name) {
        SourceResolution::Missing => SourceRouteState::Missing,
        SourceResolution::Unavailable(reason) => SourceRouteState::Unavailable(reason),
        SourceResolution::Available {
            source_slot,
            version,
            ..
        } => SourceRouteState::Available(SourceCallBinding {
            source_slot,
            version,
        }),
    }
}

/// The built-in tool defs (bash, file, search, and — at depth 0 — tasks plus
/// `run_agent`). This is the set `run_program` derives its TypeScript API from, so
/// it deliberately excludes `run_program` itself: no self-reference, and no
/// throwaway description regeneration when only counting is needed.
fn builtin_defs(depth: u8, shell_programs: &ShellPrograms) -> Vec<ToolDef> {
    let mut defs = vec![
        ToolDef {
            name: "bash".into(),
            description: "Run a shell command with `sh -lc`. Prefer the dedicated tools over shell equivalents: grep (not grep/rg), glob (not find), read_file (not cat/head/tail), edit_file (not sed); reserve bash for real shell work like builds, tests, installs, and git. stdout and stderr are merged; a non-zero exit status is appended. Default timeout 60s. For long-running commands (dev servers, watches, slow builds) set background=true instead of appending '&'. A background call returns a bg-N id and output file; inspect it with bash_output and stop it with stop_bash. When OS sandboxing is active, commands run with file writes limited to the workspace and temp directories and no network access; a failure that looks sandbox-caused is annotated in the result.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000); ignored when background=true"},
                    "background": {"type": "boolean", "description": "Run in the background: return immediately with a bg-N id and output file path (default false)"},
                    "disable_sandbox": {"type": "boolean", "description": "Run without the OS sandbox. Only set this after a command failed from sandbox restrictions (writes outside the workspace, network access) and that access is genuinely needed — never preemptively; the unsandboxed run requires user approval."}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "bash_output".into(),
            description: "Retrieve the status and output of a background bash command. Blocks until it finishes by default (up to timeout_ms); pass block=false to peek without waiting. Returns the tail of the output; read the output file for the rest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "ID from a background bash call, e.g. bg-1"},
                    "block": {"type": "boolean", "description": "Wait for completion (default true)"},
                    "timeout_ms": {"type": "integer", "description": "Max wait when blocking (default 30000, max 600000)"}
                },
                "required": ["bash_id"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "stop_bash".into(),
            description: "Stop a running background bash command by its bg-N id; terminates the whole owned process tree. Any other resource id is rejected with a directed correction.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "ID from a background bash call, e.g. bg-1"}
                },
                "required": ["bash_id"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "powershell".into(),
            description: "Run a foreground PowerShell command on native Windows with a fixed non-interactive, no-profile encoded invocation. PowerShell is treated as opaque and normally requires approval. Default timeout 60s; background execution and Windows shell sandboxing are unavailable.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The original PowerShell script to run"},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000)"}
                },
                "required": ["command"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "read_file".into(),
            description: "Read a file. Raw input is limited to 5 MiB for text and images, or 10 MiB for lowercase `.ipynb` notebooks. Text files return numbered lines formatted as `{n}\\t{line}` with a bounded character budget; use offset/limit to page. Jupyter notebooks return cell-aware `<cell id=\"…\">` content and code outputs, including image blocks. Empty files and offsets past EOF return explicit warnings. Image files (png, jpeg, gif, webp) are returned as an image you can see — offset/limit do not apply. PDFs return an explicit unsupported error.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Line number to start from (default 1; 0 is accepted for compatibility)"},
                    "limit": {"type": "integer", "minimum": 0, "description": "Max lines to return (omit or pass 0 for all lines within the character budget)"}
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: "write_file".into(),
            description: "Write the provided full content to a file. A new file safely creates missing parent directories only after approval. Overwriting an existing file requires a complete, fresh read in this session, then replaces it atomically with the provided content exactly as given.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolDef {
            name: "edit_file".into(),
            description: "Replace exact old_string matches with new_string in an existing UTF-8 file of at most 5 MiB. The entire file must have been freshly read in this session. Raw matches take priority; when none exist, LF old_string may match CRLF text without normalizing untouched bytes. Fails if old_string is absent or matches more than once without replace_all. Never creates a missing file or parent directory.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        ToolDef {
            name: "notebook_edit".into(),
            description: "Replaces, inserts, or deletes a single cell in a Jupyter notebook (.ipynb file).\n\nUsage:\n- You must use the read_file tool on the notebook in this conversation before editing — this tool will fail otherwise.\n- `notebook_path` must be an absolute path.\n- `cell_id` is the `id` attribute shown in the read_file tool's `<cell id=\"...\">` output. It is required for `replace` and `delete`.\n- `edit_mode` defaults to `replace`. Use `insert` to add a new cell after the cell with the given `cell_id` (or at the beginning of the notebook if `cell_id` is omitted) — `cell_type` is required when inserting. Use `delete` to remove the cell.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "notebook_path": {
                        "type": "string",
                        "description": "The absolute path to the Jupyter notebook file to edit (must be absolute, not relative)"
                    },
                    "cell_id": {
                        "type": "string",
                        "description": "The ID of the cell to edit. When inserting a new cell, the new cell will be inserted after the cell with this ID, or at the beginning if not specified."
                    },
                    "new_source": {
                        "type": "string",
                        "description": "The new source for the cell"
                    },
                    "cell_type": {
                        "type": "string",
                        "enum": ["code", "markdown"],
                        "description": "The type of the cell (code or markdown). If not specified, it defaults to the current cell type. If using edit_mode=insert, this is required."
                    },
                    "edit_mode": {
                        "type": "string",
                        "enum": ["replace", "insert", "delete"],
                        "description": "The type of edit to make (replace, insert, delete). Defaults to replace."
                    }
                },
                "required": ["notebook_path", "new_source"],
                "additionalProperties": false
            }),
        },
        ToolDef {
            name: "grep".into(),
            description: "Search file contents with a regular expression (ripgrep-style; Rust regex syntax, no backreferences/lookaround). Respects .gitignore, searches hidden files, skips binary files; prefer this over grep/rg in bash. Results cap at head_limit; page with offset.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regular expression to search for"},
                    "path": {"type": "string", "description": "File or directory to search (default: current directory)"},
                    "glob": {"type": "string", "description": "Filter files with a glob, e.g. \"*.rs\" or \"*.{ts,tsx}\""},
                    "type": {"type": "string", "description": "Filter by file type, e.g. rust, js, py, go"},
                    "output_mode": {"type": "string", "enum": ["files_with_matches", "content", "count"], "description": "files_with_matches: file paths newest-first (default); content: matching lines as path:line:text; count: per-file match counts"},
                    "-i": {"type": "boolean", "description": "Case-insensitive (default false)"},
                    "-n": {"type": "boolean", "description": "Show line numbers in content mode (default true)"},
                    "-A": {"type": "integer", "minimum": 0, "description": "Lines shown after each match (content mode only)"},
                    "-B": {"type": "integer", "minimum": 0, "description": "Lines shown before each match (content mode only)"},
                    "-C": {"type": "integer", "minimum": 0, "description": "Lines shown around each match (content mode only; overridden by context)"},
                    "context": {"type": "integer", "minimum": 0, "description": "Lines shown before and after each match (content mode only)"},
                    "-o": {"type": "boolean", "description": "Print only matched non-empty text (content mode only; default false)"},
                    "head_limit": {"type": "integer", "minimum": 0, "description": "Max results returned (default 250, 0 = unlimited)"},
                    "offset": {"type": "integer", "minimum": 0, "description": "Skip this many results before head_limit applies (default 0)"},
                    "multiline": {"type": "boolean", "description": "Patterns may span lines and . matches newlines (default false)"}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "glob".into(),
            description: "Find files by glob pattern, e.g. \"**/*.rs\", \"src/*.ts\" or \"*.{js,json}\" (gitignore-style: a bare name matches at any depth). Respects .gitignore. Returns paths sorted by modification time, newest first, capped at 100.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Glob pattern to match file paths against"},
                    "path": {"type": "string", "description": "Directory to search in (default: current directory)"}
                },
                "required": ["pattern"]
            }),
        },
        ToolDef {
            name: "read_offloaded".into(),
            description: "Fetch the full content of an offloaded tool result by its id (e.g. off-0001).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string", "description": "Offload id from a truncation pointer, e.g. off-0001"}
                },
                "required": ["id"]
            }),
        },
    ];
    defs.retain(|definition| match definition.name.as_str() {
        "bash" | "bash_output" | "stop_bash" => shell_programs.bash_available(),
        "powershell" => cfg!(windows) && shell_programs.powershell_available(),
        _ => true,
    });
    #[cfg(windows)]
    if let Some(bash) = defs.iter_mut().find(|definition| definition.name == "bash") {
        bash.description = "Run a command with the validated Git for Windows `bash.exe -lc`. stdout and stderr are merged; a non-zero exit status is appended. Default timeout 60s. For long-running commands set background=true. Windows Job Object containment owns the full process tree; filesystem/network sandboxing is not implemented. Prefer forward slashes inside Bash commands.".into();
        bash.schema["properties"]
            .as_object_mut()
            .expect("bash properties are an object")
            .remove("disable_sandbox");
    }
    // Local Agent mailbox tools remain available at every depth. The session
    // task graph is root-owned even though child Configs retain the same Arc.
    if depth == 0 {
        defs.extend(task::tool_defs());
    }
    defs.extend(agent_message::tool_defs());
    if depth == 0 {
        defs.push(ToolDef {
            name: "run_agent".into(),
            description: "Run one open-ended sub-agent with a fresh history on a self-contained prompt. Use Agent when the outcome is clear but the investigation path is not; use run_program for fixed code-controlled loops/tool batches, and Workflow only when the user explicitly requested multi-agent orchestration. By default this blocks and returns the final text; while main is synchronously waiting it has no model round in which to call send_message, so use background=true when main must send follow-up instructions during the run. Consecutive run_agent calls in one model response run in parallel. Set background=true to return immediately with an agent-N id and receive a bounded result preview later as an inbox message (oversized success text is offloaded for read_offloaded). Optional description is display-only and falls back to a prompt preview. Background results are delivered automatically; call wait_for_activity once only when you truly need to block for any activity, never as an output/status polling loop. Stop Agent only with that agent-N id. Background work is session-scoped, not durable across session shutdown. Sub-agents cannot spawn further sub-agents. Pass agent_type for a configured specialized agent; omit it for the general-purpose agent. Model-generated text is not deterministic and runtime gates still enforce tools, permissions, sandbox, and result limits.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "description": {"type": ["string", "null"], "minLength": 1, "maxLength": MAX_DISPLAY_DESCRIPTION_CHARS, "pattern": ".*\\S.*", "description": "Optional short, single-line display label. It never changes the prompt or result."},
                    "prompt": {"type": "string", "description": "Complete standalone work description"},
                    "agent_type": {"type": ["string", "null"], "minLength": 1, "description": "Name of a configured agent type; omit for a general-purpose sub-agent"},
                    "model": {"type": ["string", "null"], "minLength": 1, "pattern": ".*\\S.*", "description": "Optional model override on this sub-agent's inherited frozen provider; it must be in that provider's model allowlist"},
                    "background": {"type": "boolean", "description": "Return an agent-N id immediately and deliver the result later (default false)"},
                    "max_rounds": {"type": ["integer", "null"], "minimum": 1, "description": "Optional round cap; omitted means no round limit"},
                    "isolation": {"type": "string", "enum": ["shared", "worktree"], "description": "shared (default) uses the current workspace; worktree gives the agent a private git worktree"}
                },
                "required": ["prompt"],
                "additionalProperties": false
            }),
        });
        defs.push(ToolDef {
            name: "wait_for_activity".into(),
            description: "Wait once for any background shell, Agent, Program, or Workflow activity when the caller truly needs to block. Pending inbox input also wakes it. Takes no resource ID and never reads or drains a result; results are delivered automatically at the next step/final/idle boundary even if this tool is never called. A timeout is not a background failure and consumes nothing. Do not call repeatedly as a status/output polling loop.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "timeout_ms": {"type": "integer", "description": "Max wait (default 30000, min 10000, max 3600000)"}
                },
                "additionalProperties": false
            }),
        });
        defs.push(ToolDef {
            name: "stop_agent".into(),
            description: "Stop a running background agent by its agent-N id. It ends without reporting a result. Use stop_program for program-N, stop_workflow for workflow-N, or stop_bash for bg-N.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "The agent-N id from run_agent with background=true"}
                },
                "required": ["agent_id"],
                "additionalProperties": false
            }),
        });
        defs.push(ToolDef {
            name: "stop_program".into(),
            description: "Stop a running background code-mode program by its program-N id. It ends without reporting a result. Use stop_agent for agent-N, stop_workflow for workflow-N, or stop_bash for bg-N; this is not a durable run_id.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "program_id": {"type": "string", "description": "The program-N id from run_program with background=true"}
                },
                "required": ["program_id"],
                "additionalProperties": false
            }),
        });
    }
    defs
}

/// The built-ins plus a built-ins-only `run_program`. This is what tool-counting
/// (`defer_active`, `tool_merge_warnings`) sees, so `run_program` counts toward
/// the defer threshold like any other built-in. The definition actually sent to
/// the model — whose TypeScript API also lists the external source tools — is
/// built in [`all_tool_defs`], which can see the sources.
pub fn tool_defs(depth: u8, shell_programs: &ShellPrograms) -> Vec<ToolDef> {
    let mut defs = builtin_defs(depth, shell_programs);
    if depth == 0 {
        defs.push(codemode::run_program_def(&defs, &[], &[]));
    }
    defs
}

/// Concurrency safety by name AND input: read-only tools are always safe,
/// bash is safe only when the parsed command sequence is word-only and every
/// argv is a known read-only command (same analysis the permission gate
/// uses — safe-to-parallelize and safe-to-run are two verdicts over one
/// decomposition). External tools are safe only when their source marks them
/// read-only; built-in names shadow sources here exactly as they do in
/// dispatch.
pub fn is_concurrency_safe(name: &str, input: &Value, sources: &[Arc<dyn ToolSource>]) -> bool {
    match name {
        "read_file" | "read_offloaded" | "grep" | "glob" => true,
        // These inspect or signal resources already created by a gated call.
        "bash_output" | "stop_bash" => true,
        // tool_search grows the capability store. Keep it as an ordering barrier
        // so a following read-only deferred call deterministically observes the
        // receipt while a preceding call deterministically remains locked.
        "tool_search" => false,
        // skill only reads a skill file and returns its expanded body — pure,
        // no shared-state races (side effects come from tools the returned
        // instructions later prompt, gated individually).
        "skill" => true,
        "bash" => {
            input["command"]
                .as_str()
                .is_some_and(|cmd| match crate::shell::analyze_bash(cmd) {
                    crate::shell::BashAnalysis::Commands(cmds) => {
                        !cmds.is_empty() && cmds.iter().all(|c| crate::shell::argv_is_readonly(c))
                    }
                    crate::shell::BashAnalysis::Opaque => false,
                })
        }
        "ask_user_question" | "workflow" | "cron_list" | "list_agents" | "task_get"
        | "task_list" => true,
        "send_message" | "cron_create" | "cron_delete" | "schedule_wakeup" | "task_create"
        | "task_update" | "task_clear" => false,
        // Consecutive run_agent calls may run in parallel; child tool calls are
        // still gated independently.
        "run_agent" => true,
        // Resource-specific stops only signal an owned cancellation token.
        // wait_for_activity blocks, so it must run alone.
        "stop_agent" | "stop_program" | "stop_workflow" => true,
        "wait_for_activity" => false,
        "powershell" | "write_file" | "edit_file" | "notebook_edit" => false,
        other => find_source(sources, other).is_some_and(|s| s.is_readonly(other)),
    }
}

/// Normalize compatibility envelopes before any caller classifies a tool use.
/// Structured turns share this with the ordinary dispatcher so a deferred-mode
/// `call_tool` wrapper around the synthetic terminal tool is still intercepted.
pub(crate) fn normalize_tool_uses(
    tool_uses: Vec<(String, String, Value)>,
) -> Vec<(String, String, Value)> {
    tool_uses
        .into_iter()
        .map(|(id, name, input)| {
            let (name, input) = tool_search::unwrap_call_tool(name, input);
            (id, name, input)
        })
        .collect()
}

/// Execute one round of tool calls. Consecutive concurrency-safe calls run as
/// one concurrent batch (join_all); everything else runs sequentially. Every
/// tool_use always gets a paired tool_result: cancellation patches the
/// remaining calls with is_error "interrupted" results so history stays legal.
pub async fn dispatch_tools(
    tool_uses: Vec<(String, String, Value)>,
    ctx: &ToolCtx,
) -> Vec<ContentBlock> {
    // call_tool envelopes are unwrapped before anything else looks at the
    // calls: concurrency batching, hooks, permissions and the UI must all
    // judge the inner tool, never the wrapper.
    let tool_uses = normalize_tool_uses(tool_uses);
    let sources = &ctx.cfg.tool_sources;
    let expected_program_source = |name: &str| {
        ctx.from_program
            .then(|| tool_search::current_source_binding(name, &ctx.cfg))
            .flatten()
    };
    let mut results = Vec::with_capacity(tool_uses.len());
    let mut i = 0;
    while i < tool_uses.len() {
        let safe = is_concurrency_safe(&tool_uses[i].1, &tool_uses[i].2, sources);
        let mut j = i + 1;
        while j < tool_uses.len()
            && is_concurrency_safe(&tool_uses[j].1, &tool_uses[j].2, sources) == safe
        {
            j += 1;
        }
        let batch = &tool_uses[i..j];
        if ctx.cancel.is_cancelled() {
            results.extend(batch.iter().map(|(id, _, _)| interrupted(id)));
        } else if safe {
            let futs = batch.iter().map(|(id, name, input)| {
                run_one(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                    ctx.clone(),
                    expected_program_source(name),
                )
            });
            results.extend(futures::future::join_all(futs).await);
        } else {
            for (id, name, input) in batch {
                if ctx.cancel.is_cancelled() {
                    results.push(interrupted(id));
                } else {
                    results.push(
                        run_one(
                            id.clone(),
                            name.clone(),
                            input.clone(),
                            ctx.clone(),
                            expected_program_source(name),
                        )
                        .await,
                    );
                }
            }
        }
        i = j;
    }
    results
}

/// Also reused by rollout resume to patch tool_use blocks orphaned by a
/// killed session.
pub(crate) fn interrupted(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: "interrupted".into(),
        is_error: true,
    }
}

fn is_root_task_tool(name: &str) -> bool {
    matches!(
        name,
        "task_create" | "task_get" | "task_update" | "task_list" | "task_clear"
    )
}

async fn run_one(
    id: String,
    name: String,
    input: Value,
    ctx: ToolCtx,
    expected_program_source: Option<SourceCallBinding>,
) -> ContentBlock {
    let event_input = agent_message::event_input(&name, &input);
    ctx.ui.emit(&Event::ItemStarted {
        id: id.clone(),
        item: Item::ToolCall {
            agent: ctx.cfg.agent_label().to_string(),
            name: name.clone(),
            input: event_input.clone(),
            status: ItemStatus::InProgress,
            output: None,
        },
    });
    // Once a foreground shell has spawned, its own cancellation branch must
    // finish process-tree cleanup before we emit interrupted. Earlier
    // cancellation (hooks/permission) still drops the gated future, so no
    // process can appear after the turn was cancelled.
    let foreground_shell_started = AtomicBool::new(false);
    // Local send is an irreversible in-memory commit. If cancellation lands
    // during a post-hook, finish the paired queued result instead of reporting
    // `interrupted` after the recipient mailbox already changed.
    let local_send_committed = AtomicBool::new(false);
    let gated = async {
        match name.as_str() {
            "task" => bail!(
                "tool 'task' was renamed to 'run_agent'; task_* is reserved for the structured task graph"
            ),
            "wait" => bail!("tool 'wait' was renamed to 'wait_for_activity'"),
            "kill_bash" => bail!("tool 'kill_bash' was renamed to 'stop_bash'"),
            _ => {}
        }
        if name == "bash" && input.get("run_in_background").is_some() {
            bail!("bash: 'run_in_background' was renamed to 'background'; use background instead");
        }
        // The catalog hides Task tools from child Agents, but stale context or a
        // forged call must fail before allowlists, hooks, permissions, or the
        // registry handler can observe it.
        if ctx.depth > 0 && is_root_task_tool(&name) {
            bail!("tool '{name}' is only available to the root agent");
        }
        // A custom agent type's tool allowlist is a capability gate: the tool
        // is filtered out of this sub-agent's defs, so a call to it is a
        // hallucination — reject before hooks or the human are consulted.
        // (The main agent has no allowlist, so this never fires for it.)
        if !crate::agent_type::tool_available(ctx.cfg.tool_allowlist.as_deref(), &name) {
            bail!("tool '{name}' is not available to this agent type");
        }
        if matches!(name.as_str(), "bash" | "bash_output" | "stop_bash")
            && !ctx.cfg.shell_programs.bash_available()
        {
            bail!(
                "tool '{name}' is unavailable because no validated Git for Windows Bash was resolved for this session"
            );
        }
        if name == "powershell"
            && (!cfg!(windows) || !ctx.cfg.shell_programs.powershell_available())
        {
            bail!(
                "tool 'powershell' is unavailable because no trusted PowerShell executable was resolved for this session"
            );
        }
        let route_before = tool_search::current_source_route(&name, &ctx.cfg);
        let source_before = match &route_before {
            SourceRouteState::Missing => None,
            SourceRouteState::Available(binding) => Some(*binding),
            SourceRouteState::Unavailable(reason) => bail!(reason.clone()),
        };
        // Freeze the workspace before validating a deferred capability. A stale
        // call is a discovery error, so neither hooks nor the human permission
        // gate should observe it. The same workspace snapshot is then used for
        // prepare, permission, sandbox and execution.
        let workspace = ctx.cfg.effective_workspace();
        let (discovery_gated, expected_source) = if ctx.from_program {
            if source_before != expected_program_source {
                bail!(
                    "tool '{name}' source changed after this Program API was generated; run the Program again from a fresh sampling round"
                );
            }
            (false, expected_program_source)
        } else {
            let deferred_before = tool_search::is_deferred(&name, &ctx.cfg);
            let deferred_after = tool_search::is_deferred(&name, &ctx.cfg);
            let discovery_gated = deferred_before || deferred_after;
            let expected_source = if discovery_gated {
                let source = match tool_search::unlocked_source_for_dispatch(
                    &name, &ctx, &workspace,
                ) {
                    Some(source) => source,
                    None => match tool_search::current_source_route(&name, &ctx.cfg) {
                        SourceRouteState::Unavailable(reason) => bail!(reason),
                        SourceRouteState::Missing | SourceRouteState::Available(_) => {
                            bail!(
                                "tool '{name}' is deferred and not loaded yet; call tool_search with query \"select:{name}\" to load its definition, then retry"
                            )
                        }
                    },
                };
                if Some(source) != source_before {
                    bail!(
                        "tool '{name}' source changed while its deferred capability was classified; call tool_search with query \"select:{name}\" to load its definition again, then retry"
                    );
                }
                Some(source)
            } else {
                source_before
            };
            (discovery_gated, expected_source)
        };
        if let Some(binding) = expected_source {
            let source = ctx
                .cfg
                .tool_sources
                .get(binding.source_slot)
                .context("source preflight binding is no longer registered")?;
            source.preflight(&name, &input)?;
        };
        // pre_tool hooks run BEFORE the permission gate: hooks are automation
        // policy, permissions are the human's last word — a hook block means
        // there is nothing left to ask about.
        let hooks = &ctx.cfg.hooks;
        let session_id = &ctx.cfg.session_id;
        let agent = ctx.cfg.agent_label();
        match hooks
            .pre_tool(session_id, agent, &name, &input, ctx.ui.as_ref())
            .await
        {
            crate::hooks::HookDecision::Block { reason } => {
                bail!("blocked by hook: {reason}")
            }
            crate::hooks::HookDecision::Allow { context } => {
                ctx.hook_context.lock().unwrap().extend(context);
            }
        }
        let source_after = match tool_search::current_source_route(&name, &ctx.cfg) {
            SourceRouteState::Missing => None,
            SourceRouteState::Available(binding) => Some(binding),
            SourceRouteState::Unavailable(reason) => bail!(reason),
        };
        if source_after != source_before {
            let guidance = if discovery_gated {
                format!(
                    "call tool_search with query \"select:{name}\" to load its definition again, then retry"
                )
            } else if ctx.from_program {
                "run the Program again from a fresh sampling round".to_string()
            } else {
                "retry from a fresh sampling round".to_string()
            };
            bail!("tool '{name}' source changed while its pre-tool hook ran; {guidance}");
        }
        if discovery_gated
            && tool_search::unlocked_source_for_dispatch(&name, &ctx, &workspace) != expected_source
        {
            bail!(
                "tool '{name}' capability changed while its pre-tool hook ran; call tool_search with query \"select:{name}\" to load its definition again, then retry"
            );
        }
        if let Some(binding) = expected_source {
            let source = ctx
                .cfg
                .tool_sources
                .get(binding.source_slot)
                .context("source preflight binding is no longer registered")?;
            source.preflight(&name, &input)?;
        }
        // Prepare mutations after pre-hooks but before permission. The gate sees
        // the canonical effective target, while the executor retains an open
        // parent directory handle across any approval wait.
        let input = input;
        if name == "notebook_edit" {
            notebook::request_from_input(&input)?;
        }
        let prepared_mutation =
            if matches!(name.as_str(), "write_file" | "edit_file" | "notebook_edit") {
                Some(if name == "notebook_edit" {
                    fs::prepare_notebook_mutation_input(&input, &workspace).await?
                } else {
                    fs::prepare_mutation_input(&name, &input, &workspace).await?
                })
            } else {
                None
            };
        let prepared_read = if name == "read_file" {
            Some(fs::prepare_read(&input, &workspace).await?)
        } else {
            None
        };
        let mutation_preview_context = prepared_mutation
            .as_ref()
            .and_then(fs::PreparedMutation::preview_context);
        let sandbox_auto_allow = bash::sandbox_auto_allowed(&name, &input, &workspace);
        let permission = workspace
            .permissions
            .check_call_with_resolved_path(
                &name,
                &input,
                prepared_mutation
                    .as_ref()
                    .map(fs::PreparedMutation::resolved_path)
                    .or(prepared_read.as_ref().map(fs::PreparedRead::resolved_path)),
                mutation_preview_context.as_ref(),
                ctx.depth,
                sandbox_auto_allow,
            )
            .await;
        match permission {
            Ok(Some(notice)) => ctx.ui.emit(&Event::Note(notice.message)),
            Ok(None) => {}
            Err(reason) => bail!(reason),
        }
        let powershell_guard = if name == "powershell" {
            Some(ctx.cfg.powershell_execution_gate.lock().await)
        } else {
            None
        };
        #[cfg(all(test, windows))]
        let powershell_executor_probe = match &powershell_guard {
            Some(guard) => Some(guard.enter_executor().await),
            None => None,
        };
        let foreground_shell = name == "powershell"
            || (name == "bash" && !input["background"].as_bool().unwrap_or(false));
        if foreground_shell {
            foreground_shell_started.store(true, Ordering::Release);
        }
        let prepared = PreparedExecution {
            read: prepared_read.as_ref(),
            mutation: prepared_mutation.as_ref(),
            source: expected_source,
            local_send_committed: &local_send_committed,
        };
        let execution = execute_tool(&name, &input, prepared, &ctx, &workspace).await;
        if foreground_shell {
            foreground_shell_started.store(false, Ordering::Release);
        }
        #[cfg(all(test, windows))]
        drop(powershell_executor_probe);
        drop(powershell_guard);
        // post_tool hooks (and other text-only surfaces) see the flattened
        // text; an image result renders as an `[image: <media_type>]` tag.
        let (text, is_error) = match &execution.result {
            Ok(content) => (content.as_text().into_owned(), false),
            Err(e) => (format!("{e:#}"), true),
        };
        let context = hooks
            .post_tool(
                session_id,
                agent,
                &name,
                &input,
                &text,
                is_error,
                ctx.ui.as_ref(),
            )
            .await;
        ctx.hook_context.lock().unwrap().extend(context);
        Ok::<ToolExecution, anyhow::Error>(execution)
    };
    let mut gated = Box::pin(gated);
    let gated_result = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            if foreground_shell_started.load(Ordering::Acquire)
                || local_send_committed.load(Ordering::Acquire)
            {
                Some(gated.await)
            } else {
                None
            }
        }
        result = &mut gated => Some(result),
    };
    let (result, file_state_update, path_lock) = match gated_result {
        None => (interrupted(&id), None, None),
        Some(Ok(execution)) => {
            let ToolExecution {
                result,
                file_state_update,
                path_lock,
            } = execution;
            let result = match result {
                Ok(content) => ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content,
                    is_error: false,
                },
                Err(e) => ContentBlock::ToolResult {
                    tool_use_id: id.clone(),
                    content: format!("{e:#}").into(),
                    is_error: true,
                },
            };
            (result, file_state_update, path_lock)
        }
        Some(Err(e)) => (
            ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: format!("{e:#}").into(),
                is_error: true,
            },
            None,
            None,
        ),
    };
    // The model has a successful Read/Write/Edit only once the final tool_result
    // exists. Executor-local reads and work canceled while a post-hook runs do
    // not create write authority. Mutation executors clear authority before
    // touching disk, so an interrupted commit remains conservative.
    if let Some((state, update)) = file_state_update {
        state.apply(update);
    }
    drop(path_lock);
    let ContentBlock::ToolResult {
        tool_use_id,
        is_error,
        content,
    } = &result
    else {
        unreachable!("run_one always builds a tool_result")
    };
    // Bound the transport: a UI previews only a few lines, and a huge bash
    // output would otherwise be cloned onto the event channel wholesale. The UI
    // truncates further for display.
    let output: String = content.as_text().chars().take(4000).collect();
    ctx.ui.emit(&Event::ItemCompleted {
        id: tool_use_id.clone(),
        item: Item::ToolCall {
            agent: ctx.cfg.agent_label().to_string(),
            name: name.clone(),
            input: event_input,
            status: if *is_error {
                ItemStatus::Failed
            } else {
                ItemStatus::Completed
            },
            output: (!output.is_empty()).then_some(output),
        },
    });
    result
}

/// Returns an explicitly type-erased future: this is the recursion boundary
/// (execute_tool -> run_agent -> run_turn -> dispatch_tools -> execute_tool), and
/// the `dyn Future + Send` signature is what lets rustc resolve the otherwise
/// cyclic Send inference for the recursive async call graph.
fn execute_tool<'a>(
    name: &'a str,
    input: &'a Value,
    prepared: PreparedExecution<'a>,
    ctx: &'a ToolCtx,
    workspace: &'a EffectiveWorkspace,
) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>> {
    Box::pin(async move {
        // read_file is the sole BUILT-IN that can return non-text: on an image
        // file it returns an image block (ToolResultContent::Blocks).
        if name == "read_file" {
            let Some(prepared) = prepared.read else {
                return ToolExecution::from_result(Err(anyhow!(
                    "read_file: target was not prepared"
                )));
            };
            let state = Arc::clone(&workspace.file_state);
            return match fs::read_file_tool(input, prepared).await {
                Ok(output) => ToolExecution {
                    result: Ok(output.content),
                    file_state_update: Some((state, output.state_update)),
                    path_lock: None,
                },
                Err(error) => ToolExecution::from_result(Err(error)),
            };
        }
        if matches!(name, "write_file" | "edit_file" | "notebook_edit") {
            let Some(prepared_mutation) = prepared.mutation else {
                return ToolExecution::from_result(Err(anyhow!(
                    "{name}: mutation target was not prepared"
                )));
            };
            let state = Arc::clone(&workspace.file_state);
            let output = match name {
                "write_file" => fs::write_file_tool(input, prepared_mutation, ctx, workspace).await,
                "edit_file" => fs::edit_file_tool(input, prepared_mutation, ctx, workspace).await,
                "notebook_edit" => {
                    fs::notebook_edit_tool(input, prepared_mutation, ctx, workspace).await
                }
                _ => unreachable!("matched file mutation tool"),
            };
            return match output {
                Ok(output) => ToolExecution {
                    result: Ok(ToolResultContent::Text(output.content)),
                    file_state_update: Some((state, output.state_update)),
                    path_lock: Some(output.path_lock),
                },
                Err(error) => ToolExecution::from_result(Err(error)),
            };
        }

        // External source tools can also return images — handle them before the
        // text-returning built-ins so their result can be Text OR Blocks. A
        // program still gets the structured form via the sink.
        if let Some(binding) = prepared.source {
            let Some(source) = ctx.cfg.tool_sources.get(binding.source_slot) else {
                return ToolExecution::from_result(Err(anyhow!(
                    "source binding for tool '{name}' is no longer registered"
                )));
            };
            return match source
                .call_at_version(name, input, Some(binding.version))
                .await
            {
                Ok(out) => {
                    if let Some(slot) = &ctx.program_result {
                        *slot.lock().unwrap() = out.structured.clone();
                    }
                    ToolExecution::from_result(Ok(out.into_content()))
                }
                Err(error) => ToolExecution::from_result(Err(error)),
            };
        }
        let text: Result<String> = match name {
            "bash" => bash::bash_tool(input, ctx, workspace).await,
            "powershell" => powershell::powershell_tool(input, ctx, workspace).await,
            "bash_output" => bash::bash_output_tool(input, ctx).await,
            "stop_bash" => bash::stop_bash_tool(input, ctx).await,
            "grep" => {
                search::grep_tool(input, &workspace.cwd, Arc::clone(&workspace.permissions)).await
            }
            // glob hands a program its path list as a string[] (built-ins are
            // otherwise strings); the model-facing text is unchanged.
            "glob" => {
                search::glob_tool(
                    input,
                    &workspace.cwd,
                    ctx.program_result.as_ref(),
                    Arc::clone(&workspace.permissions),
                )
                .await
            }
            "read_offloaded" => fs::read_offloaded_tool(input, ctx).await,
            "task_create" => task::task_create_tool(input, ctx),
            "task_get" => task::task_get_tool(input, ctx),
            "task_update" => task::task_update_tool(input, ctx),
            "task_list" => task::task_list_tool(input, ctx),
            "task_clear" => task::task_clear_tool(input, ctx),
            "skill" => skill::skill_tool(input, ctx, workspace).await,
            "tool_search" => tool_search::tool_search_tool(input, ctx, workspace).await,
            // Only malformed envelopes reach this arm — well-formed ones were
            // rewritten to the inner call at dispatch entry.
            "call_tool" => Err(anyhow!(
                "call_tool: missing required string argument 'tool_name' (usage: {{\"tool_name\": \"<name>\", \"params\": {{...}}}})"
            )),
            "run_agent" => subagent::run_agent_tool(input, ctx, workspace).await,
            "send_message" => {
                let result = agent_message::send_message_tool(input, ctx);
                if result.is_ok() {
                    prepared.local_send_committed.store(true, Ordering::Release);
                }
                result
            }
            "list_agents" => agent_message::list_agents_tool(input, ctx),
            "ask_user_question" => question::ask_user_question_tool(input, ctx).await,
            "enter_plan_mode" => plan_mode::enter_plan_mode_tool(input, ctx, workspace).await,
            "exit_plan_mode" => plan_mode::exit_plan_mode_tool(input, ctx, workspace).await,
            "enter_worktree" => worktree_tool::enter_worktree_tool(input, ctx).await,
            "exit_worktree" => worktree_tool::exit_worktree_tool(input, ctx).await,
            "wait_for_activity" => background_executions::wait_for_activity_tool(input, ctx).await,
            "stop_agent" => background_executions::stop_agent_tool(input, ctx).await,
            "stop_program" => background_executions::stop_program_tool(input, ctx).await,
            "stop_workflow" => background_executions::stop_workflow_tool(input, ctx).await,
            "cron_create" => scheduler::cron_create_tool(input, ctx).await,
            "cron_delete" => scheduler::cron_delete_tool(input, ctx).await,
            "cron_list" => scheduler::cron_list_tool(input, ctx).await,
            "schedule_wakeup" => scheduler::schedule_wakeup_tool(input, ctx).await,
            "run_program" => codemode::run_program_tool(input, ctx, workspace).await,
            "workflow" => workflow::workflow_tool_in_workspace(input, ctx, workspace).await,
            // Source tools were already handled above (they may return images); anything reaching here is an unknown tool name.
            other => Err(anyhow!("unknown tool: {other}")),
        };
        ToolExecution::from_result(text.map(ToolResultContent::Text))
    })
}

pub(crate) fn char_prefix(text: &str, max_chars: usize) -> (&str, bool) {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => (&text[..end], true),
        None => (text, false),
    }
}

pub(crate) const MAX_DISPLAY_DESCRIPTION_CHARS: usize = 200;

/// Validate optional human-facing metadata before any tool side effect. Reading
/// the raw JSON preserves the distinction between an omitted field and an
/// explicit `null`, which serde's `Option<String>` would otherwise erase.
pub(crate) fn optional_display_description(input: &Value, tool: &str) -> Result<Option<String>> {
    let Some(value) = input.get("description") else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let description = value
        .as_str()
        .ok_or_else(|| anyhow!("{tool}: description must be a string when provided"))?;
    if description.trim().is_empty() {
        bail!("{tool}: description must not be empty");
    }
    if description.chars().count() > MAX_DISPLAY_DESCRIPTION_CHARS {
        bail!("{tool}: description exceeds the {MAX_DISPLAY_DESCRIPTION_CHARS}-character limit");
    }
    if description.chars().any(char::is_control) {
        bail!("{tool}: description must be a single line without control characters");
    }
    Ok(Some(description.to_string()))
}

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| anyhow!("{tool}: missing required string argument '{key}'"))
}

pub(crate) fn strict_str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("{tool}: input must be an object"))?;
    if let Some(unexpected) = object.keys().find(|candidate| candidate.as_str() != key) {
        return Err(anyhow!("{tool}: unknown field `{unexpected}`"));
    }
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{tool}: missing required string argument '{key}'"))
}

/// Anchor a tool's path argument at the agent's cwd: an absolute path is used
/// as-is, a relative one resolves against `cwd`. For the main agent `cwd` is
/// the process cwd, so this is a no-op there; for a worktree sub-agent (plan
/// 35) it is what keeps the sub-agent's relative reads/writes inside its own
/// tree instead of leaking to the process cwd. Same anchor the permission gate
/// uses, so the check and the IO never disagree about where a path points.
pub(crate) fn resolve_path(cwd: &std::path::Path, raw: &str) -> std::path::PathBuf {
    let p = std::path::Path::new(raw);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Shared fixtures for the per-module tool tests: a permissive ToolCtx and
/// a dispatch-path runner, so every tool test exercises the real gate.
#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use kloop_provider::Provider;

    pub(crate) fn bash_input(cmd: &str) -> Value {
        json!({"command": cmd})
    }

    #[cfg(windows)]
    pub(crate) fn powershell_output_is_done_and_successful(output: &str) -> bool {
        let has_done_line = output.lines().any(|line| line.trim() == "done");
        let has_failure_status = output.lines().any(|line| {
            let line = line.trim();
            line.starts_with("[exit status ") || line == "[killed by signal]"
        });
        has_done_line && !has_failure_status
    }

    #[cfg(windows)]
    pub(crate) fn assert_powershell_done(result: (String, bool)) {
        let (output, is_error) = result;
        assert!(!is_error, "{output}");
        assert!(
            powershell_output_is_done_and_successful(&output),
            "{output}"
        );
    }

    pub(crate) struct SilentUi;
    impl Ui for SilentUi {
        fn emit(&self, _: &Event) {}
    }

    pub(crate) fn test_ctx(depth: u8, tag: &str) -> ToolCtx {
        test_ctx_with_sources(depth, tag, Vec::new())
    }

    pub(crate) fn test_ctx_with_sources(
        depth: u8,
        tag: &str,
        sources: Vec<Arc<dyn ToolSource>>,
    ) -> ToolCtx {
        let (provider_catalog, provider_route) =
            crate::provider_route::ProviderCatalog::from_provider(
                "test",
                Provider::mock(vec![]),
                "mock",
                vec!["mock".into()],
                None,
            )
            .unwrap();
        let inbox = Arc::new(crate::inbox::Inbox::default());
        ToolCtx {
            cfg: Arc::new(Config {
                provider_catalog,
                provider_route,
                system: "test".into(),
                project_instructions: None,
                max_rounds: Some(5),
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                offload_dir: std::env::temp_dir().join(format!("kloop-tools-{tag}")),
                sessions_dir: std::env::temp_dir().join(format!("kloop-tools-sessions-{tag}")),
                context_window: None,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                questioner: None,
                file_state: Default::default(),
                tool_sources: sources,
                session_id: String::new(),
                local_agent: crate::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
                hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
                background_shells: BackgroundShells::new(),
                shell_programs: std::sync::Arc::new(
                    crate::shell_programs::ShellPrograms::test_fixture(),
                ),
                powershell_execution_gate: Default::default(),
                sandbox: None,
                agent_types: Arc::new(Vec::new()),
                tool_allowlist: None,
                defer_threshold: 30,
                unlocked_tools: Default::default(),
                tasks: Default::default(),
                inbox: Arc::clone(&inbox),
                scheduler: crate::scheduler::Scheduler::in_memory(inbox),
                background_executions: Default::default(),
                program_limits: Default::default(),
                skills: Default::default(),
                active_worktree: std::sync::Arc::new(
                    crate::worktree::ActiveWorktreeState::default(),
                ),
                surface: Default::default(),
            }),
            ui: Arc::new(SilentUi),
            cancel: CancellationToken::new(),
            depth,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
        }
    }

    #[cfg(windows)]
    pub(crate) fn with_powershell_gate_probe(
        mut ctx: ToolCtx,
    ) -> (ToolCtx, crate::config::PowerShellGateController) {
        let mut cfg = ctx.cfg.test_clone();
        let (gate, controller) = crate::config::PowerShellExecutionGate::instrumented();
        cfg.powershell_execution_gate = Arc::new(gate);
        ctx.cfg = Arc::new(cfg);
        (ctx, controller)
    }

    /// Rebuild the ctx with a different defer threshold (Config is behind an
    /// Arc, so tests clone-and-swap instead of mutating).
    pub(crate) fn with_defer_threshold(mut ctx: ToolCtx, threshold: usize) -> ToolCtx {
        let mut cfg = ctx.cfg.test_clone();
        cfg.defer_threshold = threshold;
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    /// Rebuild the ctx with a scripted provider, for tests whose tools spawn
    /// sub-agents that sample.
    pub(crate) fn with_provider(mut ctx: ToolCtx, provider: Provider) -> ToolCtx {
        let mut cfg = ctx.cfg.test_clone();
        cfg.set_test_provider(provider);
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    /// Rebuild the ctx with an OS sandbox policy for bash.
    #[allow(dead_code)] // used by the macOS-only sandbox integration tests
    pub(crate) fn with_sandbox(mut ctx: ToolCtx, policy: crate::sandbox::SandboxPolicy) -> ToolCtx {
        let mut cfg = ctx.cfg.test_clone();
        cfg.sandbox = Some(Arc::new(policy));
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    /// A throwaway git repo with one commit; returns its canonical root (git
    /// resolves symlinks, so tests compare against the canonical path). Shared
    /// by the worktree tests (slice 1 sub-agent isolation, slice 2 enter/exit).
    pub(crate) fn temp_git_repo(tag: &str) -> std::path::PathBuf {
        // A process-global counter so two tests sharing a tag never collide on
        // one directory (they run concurrently and would delete each other's).
        use std::sync::atomic::{AtomicUsize, Ordering};
        static SEQ: AtomicUsize = AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("kloop-gitwt-{tag}-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for a in [
            &["init", "-q"][..],
            &["config", "user.email", "t@e.com"],
            &["config", "user.name", "t"],
            &["commit", "--allow-empty", "-qm", "base"],
        ] {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(&root)
                    .args(a)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        std::fs::canonicalize(&root).unwrap()
    }

    /// Rebuild the ctx with cwd pointed at `repo` and worktree mode toggled —
    /// for the session enter/exit tests (slice 2).
    pub(crate) fn git_ctx(
        mut ctx: ToolCtx,
        repo: &std::path::Path,
        worktree_enabled: bool,
    ) -> ToolCtx {
        let mut cfg = ctx.cfg.test_clone();
        let old_cwd = cfg.cwd.clone();
        let identity = crate::project::WorkspaceIdentity::resolve(repo);
        cfg.permissions = Arc::new(cfg.permissions.for_workspace(identity));
        cfg.sandbox = cfg
            .sandbox
            .as_ref()
            .map(|sandbox| Arc::new(sandbox.for_workspace(repo)));
        cfg.file_state = Arc::new(crate::file_state::FileState::default());
        cfg.system = cfg.system.replacen(
            &format!("- Working directory: {}", old_cwd.display()),
            &format!("- Working directory: {}", repo.display()),
            1,
        );
        cfg.cwd = repo.to_path_buf();
        cfg.surface.worktree = worktree_enabled;
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    /// Run a single tool call through the real dispatch path and return
    /// (text, is_error). A block result (an image) flattens to its text view;
    /// tests that need the raw blocks call dispatch_tools directly.
    pub(crate) async fn run_tool(name: &str, input: Value, ctx: &ToolCtx) -> (String, bool) {
        let results = dispatch_tools(vec![("t".into(), name.into(), input)], ctx).await;
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = results.into_iter().next().unwrap()
        else {
            panic!("expected tool result");
        };
        (content.as_text().into_owned(), is_error)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn interactive_surface() -> crate::config::SurfaceCapabilities {
        crate::config::SurfaceCapabilities {
            questions: true,
            plan_control: true,
            ..Default::default()
        }
    }

    #[cfg(windows)]
    #[test]
    fn powershell_done_check_accepts_progress_but_rejects_failure_status() {
        assert!(powershell_output_is_done_and_successful(
            "done\r\n#< CLIXML\r\n<Objs>progress</Objs>"
        ));
        assert!(!powershell_output_is_done_and_successful(
            "done\r\n[exit status 9]"
        ));
        assert!(!powershell_output_is_done_and_successful(
            "done\r\n[killed by signal]"
        ));
        assert!(!powershell_output_is_done_and_successful(
            "#< CLIXML\r\n<Objs>progress</Objs>"
        ));
    }

    /// External source stub: `{prefix}__echo` (marked read-only) and
    /// `{prefix}__fail` (always errors).
    struct StubSource {
        defs: Vec<ToolDef>,
        readonly: String,
    }

    impl StubSource {
        fn new(prefix: &str) -> Arc<Self> {
            let def = |tool: &str| ToolDef {
                name: format!("{prefix}__{tool}"),
                description: format!("stub {tool}"),
                schema: json!({"type": "object"}),
            };
            Arc::new(StubSource {
                defs: vec![def("echo"), def("fail"), def("image")],
                readonly: format!("{prefix}__echo"),
            })
        }
    }

    impl ToolSource for StubSource {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(self.defs.clone())
        }

        fn is_readonly(&self, tool: &str) -> bool {
            tool == self.readonly
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                if tool.ends_with("__fail") {
                    bail!("stub failure");
                }
                // An MCP tool that returned an image: the source hands back
                // content blocks, not just text (the model must see it).
                if tool.ends_with("__image") {
                    return Ok(SourceOutput {
                        text: "[image: image/png]".into(),
                        blocks: Some(vec![ContentBlock::Image {
                            source: kloop_protocol::ImageSource::Base64 {
                                media_type: "image/png".into(),
                                data: "aGk=".into(),
                            },
                        }]),
                        structured: None,
                    });
                }
                Ok(SourceOutput::text(format!(
                    "echoed {}",
                    input["text"].as_str().unwrap_or("?")
                )))
            })
        }
    }

    struct PanicApprover;

    impl crate::permissions::Approver for PanicApprover {
        fn confirm(
            &self,
            _request: crate::permissions::ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = crate::permissions::Decision> + Send + '_>> {
            panic!("unavailable source reached the permission approver")
        }
    }

    struct UnavailableSource {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ToolSource for UnavailableSource {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(Vec::<ToolDef>::new())
        }

        fn definition_state(&self, tool: &str) -> SourceDefinitionState {
            if tool.starts_with("offline__") {
                SourceDefinitionState::Unavailable {
                    reason: "MCP server \"offline\" is failed: configured tools are unavailable, not missing".into(),
                }
            } else {
                SourceDefinitionState::Missing
            }
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(SourceOutput::text("unexpected call".into()))
            })
        }
    }

    struct PreflightSource {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ToolSource for PreflightSource {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(vec![ToolDef {
                name: "external__preflight".into(),
                description: "preflight fixture".into(),
                schema: json!({"type": "object"}),
            }])
        }

        fn preflight(&self, _tool: &str, _input: &Value) -> Result<()> {
            bail!("source unavailable during preflight")
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::Relaxed);
                Ok(SourceOutput::text("unexpected call".into()))
            })
        }
    }

    struct WebGateSource {
        defs: Arc<[ToolDef]>,
        both_started: tokio::sync::Barrier,
    }

    impl WebGateSource {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                defs: Arc::from(
                    ["web_fetch", "web_search"]
                        .into_iter()
                        .map(|name| ToolDef {
                            name: name.into(),
                            description: format!("test {name}"),
                            schema: json!({"type": "object"}),
                        })
                        .collect::<Vec<_>>(),
                ),
                both_started: tokio::sync::Barrier::new(2),
            })
        }
    }

    impl ToolSource for WebGateSource {
        fn defs(&self) -> Arc<[ToolDef]> {
            self.defs.clone()
        }

        fn is_readonly(&self, tool: &str) -> bool {
            matches!(tool, "web_fetch" | "web_search")
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.both_started.wait().await;
                Ok(SourceOutput::text(format!("completed {tool}")))
            })
        }
    }

    #[test]
    fn optional_display_description_is_strict_and_unicode_counted() {
        assert_eq!(
            optional_display_description(&json!({}), "run_agent").unwrap(),
            None
        );
        assert_eq!(
            optional_display_description(&json!({"description": "审查后台生命周期"}), "run_agent")
                .unwrap()
                .as_deref(),
            Some("审查后台生命周期")
        );
        assert_eq!(
            optional_display_description(&json!({"description": null}), "run_agent").unwrap(),
            None
        );
        for (value, message) in [
            (json!(42), "must be a string"),
            (json!("   "), "must not be empty"),
            (json!("two\nlines"), "single line"),
            (json!("x".repeat(201)), "200-character limit"),
        ] {
            let error = optional_display_description(&json!({"description": value}), "run_agent")
                .unwrap_err()
                .to_string();
            assert!(error.contains(message), "{error}");
        }
        assert!(
            optional_display_description(
                &json!({"description": "界".repeat(MAX_DISPLAY_DESCRIPTION_CHARS)}),
                "run_program",
            )
            .is_ok()
        );
    }

    #[test]
    fn agent_and_program_schemas_match_strict_optional_string_parsers() {
        let definitions = tool_defs(0, &ShellPrograms::native_posix());
        for name in ["run_agent", "run_program"] {
            let definition = definitions
                .iter()
                .find(|definition| definition.name == name)
                .unwrap();
            assert_eq!(
                definition.schema["properties"]["description"],
                json!({
                    "type": ["string", "null"],
                    "minLength": 1,
                    "maxLength": MAX_DISPLAY_DESCRIPTION_CHARS,
                    "pattern": ".*\\S.*",
                    "description": if name == "run_agent" {
                        "Optional short, single-line display label. It never changes the prompt or result."
                    } else {
                        "Optional short, single-line display label. It never changes source identity, journal replay, or the result."
                    }
                })
            );
        }
        let run_agent = definitions
            .iter()
            .find(|definition| definition.name == "run_agent")
            .unwrap();
        assert_eq!(run_agent.schema["properties"]["agent_type"]["minLength"], 1);
        assert_eq!(
            run_agent.schema["properties"]["agent_type"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            run_agent.schema["properties"]["max_rounds"]["type"],
            json!(["integer", "null"])
        );
        let run_program = definitions
            .iter()
            .find(|definition| definition.name == "run_program")
            .unwrap();
        assert_eq!(
            run_program.schema["properties"]["resume_from_run_id"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(
            run_program.schema["properties"]["resume_from_run_id"]["pattern"],
            "^run-[A-Za-z0-9_-]+$"
        );
    }

    #[test]
    fn shell_catalog_follows_the_frozen_availability_snapshot() {
        assert!(!is_concurrency_safe(
            "powershell",
            &json!({"command": "Get-ChildItem"}),
            &[]
        ));

        let unavailable = ShellPrograms {
            bash: None,
            powershell: None,
        };
        let names = tool_defs(0, &unavailable)
            .into_iter()
            .map(|definition| definition.name)
            .collect::<Vec<_>>();
        for name in ["bash", "bash_output", "stop_bash", "powershell"] {
            assert!(!names.iter().any(|candidate| candidate == name), "{name}");
        }

        let available = ShellPrograms::test_fixture();
        let definitions = tool_defs(0, &available);
        for name in ["bash", "bash_output", "stop_bash"] {
            assert!(definitions.iter().any(|definition| definition.name == name));
        }
        #[cfg(windows)]
        {
            let powershell = definitions
                .iter()
                .find(|definition| definition.name == "powershell")
                .expect("PowerShell is registered when its frozen executable is available");
            let bash = definitions
                .iter()
                .find(|definition| definition.name == "bash")
                .expect("Git Bash is registered when available");
            assert!(bash.schema["properties"].get("disable_sandbox").is_none());
            assert!(
                powershell
                    .description
                    .contains("background execution and Windows shell sandboxing are unavailable")
            );
            let run_program = definitions
                .iter()
                .find(|definition| definition.name == "run_program")
                .expect("depth-zero catalog contains run_program");
            assert!(run_program.description.contains("powershell"));
        }
        #[cfg(not(windows))]
        assert!(
            !definitions
                .iter()
                .any(|definition| definition.name == "powershell")
        );
    }

    #[cfg(windows)]
    #[test]
    fn catalog_deferral_and_warnings_share_each_frozen_shell_snapshot() {
        use crate::shell_programs::ShellFlavor;
        use crate::shell_programs::ShellProgram;

        let bash = ShellProgram {
            executable: "frozen-bash.exe".into(),
            flavor: ShellFlavor::GitBash,
        };
        let powershell = ShellProgram {
            executable: "frozen-pwsh.exe".into(),
            flavor: ShellFlavor::PowerShell7,
        };
        let cases = [
            (
                ShellPrograms {
                    bash: None,
                    powershell: None,
                },
                0usize,
            ),
            (
                ShellPrograms {
                    bash: Some(bash.clone()),
                    powershell: None,
                },
                3,
            ),
            (
                ShellPrograms {
                    bash: None,
                    powershell: Some(powershell.clone()),
                },
                1,
            ),
            (
                ShellPrograms {
                    bash: Some(bash),
                    powershell: Some(powershell),
                },
                4,
            ),
        ];
        let source: Arc<dyn ToolSource> = StubSource::new("snapshot");
        let sources = vec![source];
        for (shells, shell_tool_count) in cases {
            let builtins = tool_defs(0, &shells);
            assert_eq!(
                builtins
                    .iter()
                    .filter(|definition| matches!(
                        definition.name.as_str(),
                        "bash" | "bash_output" | "stop_bash" | "powershell"
                    ))
                    .count(),
                shell_tool_count
            );
            let threshold = builtins.len();
            assert!(defer_active(&sources, threshold, &shells));
            assert_eq!(
                deferred_tool_defs(&sources, threshold, &shells),
                StubSource::new("snapshot").defs.clone()
            );
            let warnings = tool_merge_warnings(&sources, threshold, &shells);
            assert_eq!(warnings.len(), 1, "{warnings:?}");
            assert!(
                warnings[0].contains(&(builtins.len() + 3).to_string()),
                "{warnings:?}"
            );
            let names = all_tool_defs(0, &sources, threshold, interactive_surface(), &shells)
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>();
            assert!(names.iter().any(|name| name == "tool_search"));
            assert!(names.iter().any(|name| name == "call_tool"));
            assert!(!names.iter().any(|name| name.starts_with("snapshot__")));
        }
    }

    #[test]
    fn tool_defs_expose_root_controls_only_at_depth_zero() {
        let names = |depth| {
            tool_defs(depth, &ShellPrograms::native_posix())
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        let root = names(0);
        let child = names(1);
        assert!(root.iter().any(|name| name == "run_agent"));
        assert!(!child.iter().any(|name| name == "run_agent"));
        for task_tool in [
            "task_create",
            "task_get",
            "task_update",
            "task_list",
            "task_clear",
        ] {
            assert!(root.iter().any(|name| name == task_tool), "{task_tool}");
            assert!(!child.iter().any(|name| name == task_tool), "{task_tool}");
        }
    }

    #[test]
    fn file_tool_definitions_state_resource_and_freshness_contracts() {
        let definitions = tool_defs(0, &ShellPrograms::native_posix());
        let definition = |name: &str| {
            definitions
                .iter()
                .find(|definition| definition.name == name)
                .unwrap()
        };
        assert_eq!(
            definition("read_file").description,
            "Read a file. Raw input is limited to 5 MiB for text and images, or 10 MiB for lowercase `.ipynb` notebooks. Text files return numbered lines formatted as `{n}\\t{line}` with a bounded character budget; use offset/limit to page. Jupyter notebooks return cell-aware `<cell id=\"…\">` content and code outputs, including image blocks. Empty files and offsets past EOF return explicit warnings. Image files (png, jpeg, gif, webp) are returned as an image you can see — offset/limit do not apply. PDFs return an explicit unsupported error."
        );
        assert_eq!(
            definition("write_file").description,
            "Write the provided full content to a file. A new file safely creates missing parent directories only after approval. Overwriting an existing file requires a complete, fresh read in this session, then replaces it atomically with the provided content exactly as given."
        );
        assert_eq!(
            definition("edit_file").description,
            "Replace exact old_string matches with new_string in an existing UTF-8 file of at most 5 MiB. The entire file must have been freshly read in this session. Raw matches take priority; when none exist, LF old_string may match CRLF text without normalizing untouched bytes. Fails if old_string is absent or matches more than once without replace_all. Never creates a missing file or parent directory."
        );
        for name in ["read_file", "write_file", "edit_file"] {
            assert!(
                !definition(name).schema.to_string().contains("maxLength"),
                "UTF-8 byte limits must not be modeled as JSON character limits"
            );
        }
    }

    #[tokio::test]
    async fn readonly_web_source_calls_share_a_concurrent_batch() {
        let source: Arc<dyn ToolSource> = WebGateSource::new();
        let ctx = test_ctx_with_sources(0, "web-concurrency", vec![source]);
        let results = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            dispatch_tools(
                vec![
                    (
                        "fetch".into(),
                        "web_fetch".into(),
                        json!({"url": "https://example.com"}),
                    ),
                    (
                        "search".into(),
                        "web_search".into(),
                        json!({"query": "rust"}),
                    ),
                ],
                &ctx,
            ),
        )
        .await
        .expect("web calls must overlap rather than deadlock at the two-party barrier");

        assert_eq!(
            results
                .iter()
                .map(|result| {
                    let ContentBlock::ToolResult {
                        tool_use_id,
                        content,
                        is_error,
                    } = result
                    else {
                        panic!("expected tool result")
                    };
                    (
                        tool_use_id.as_str(),
                        content.as_text().into_owned(),
                        *is_error,
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                ("fetch", "completed web_fetch".into(), false),
                ("search", "completed web_search".into(), false),
            ]
        );
    }

    #[test]
    fn all_tool_defs_appends_sources_and_skips_collisions() {
        let sources: Vec<Arc<dyn ToolSource>> =
            vec![StubSource::new("srv"), StubSource::new("srv")];
        let names: Vec<String> = all_tool_defs(
            0,
            &sources,
            TOOL_DEFER_THRESHOLD,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        )
        .into_iter()
        .map(|d| d.name)
        .collect();
        // Built-ins first, then the first source; the duplicate source's
        // identical names are dropped. run_program comes last: its TypeScript
        // API is generated from the built-ins AND the source tools, so it is
        // appended only after the sources are merged.
        assert_eq!(
            names,
            vec![
                "bash",
                "bash_output",
                "stop_bash",
                "read_file",
                "write_file",
                "edit_file",
                "notebook_edit",
                "grep",
                "glob",
                "read_offloaded",
                "task_create",
                "task_get",
                "task_update",
                "task_list",
                "task_clear",
                "send_message",
                "list_agents",
                "run_agent",
                "wait_for_activity",
                "stop_agent",
                "stop_program",
                "srv__echo",
                "srv__fail",
                "srv__image",
                "run_program",
                "ask_user_question",
                "enter_plan_mode",
                "exit_plan_mode",
            ]
        );

        // A source colliding with a built-in name is dropped too.
        let builtin_clash: Vec<Arc<dyn ToolSource>> = vec![Arc::new(StubSource {
            defs: vec![ToolDef {
                name: "bash".into(),
                description: "impostor".into(),
                schema: json!({"type": "object"}),
            }],
            readonly: String::new(),
        })];
        let defs = all_tool_defs(
            0,
            &builtin_clash,
            TOOL_DEFER_THRESHOLD,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        let bash: Vec<&ToolDef> = defs.iter().filter(|d| d.name == "bash").collect();
        assert_eq!(bash.len(), 1);
        assert_ne!(bash[0].description, "impostor");
        // The built-in bash description must redirect content search / read / edit
        // to the dedicated tools at the point the model decides to shell out.
        let desc = &bash[0].description;
        for redirect in [
            "grep (not grep/rg)",
            "glob (not find)",
            "read_file (not cat/head/tail)",
            "edit_file (not sed)",
            "reserve bash",
        ] {
            assert!(
                desc.contains(redirect),
                "bash description missing {redirect:?}: {desc}"
            );
        }
    }

    #[tokio::test]
    async fn retired_tool_names_stay_reserved_and_legacy_migrations_are_directed() {
        let retired = ["task", "wait", "kill_bash", "todo_write"];
        let source: Arc<dyn ToolSource> = Arc::new(StubSource {
            defs: retired
                .iter()
                .map(|name| ToolDef {
                    name: (*name).into(),
                    description: "must stay hidden".into(),
                    schema: json!({"type": "object"}),
                })
                .collect(),
            readonly: String::new(),
        });
        let names = all_tool_defs(
            0,
            &[source],
            TOOL_DEFER_THRESHOLD,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        )
        .into_iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
        for name in retired {
            assert!(!names.iter().any(|candidate| candidate == name));
        }

        let ctx = test_ctx(0, "retired-tool-names");
        for (name, input, replacement) in [
            ("task", json!({"prompt": "x"}), "run_agent"),
            ("wait", json!({}), "wait_for_activity"),
            ("kill_bash", json!({"bash_id": "bg-1"}), "stop_bash"),
        ] {
            let (output, is_error) = run_tool(name, input, &ctx).await;
            assert!(is_error, "{name}: {output}");
            assert!(output.contains(replacement), "{name}: {output}");
        }
        let (output, is_error) = run_tool("todo_write", json!({}), &ctx).await;
        assert!(is_error, "{output}");
        assert_eq!(output, "unknown tool: todo_write");
    }

    #[test]
    fn kloop_owned_tool_names_use_snake_case() {
        let mut definitions = all_tool_defs(
            0,
            &[],
            TOOL_DEFER_THRESHOLD,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        definitions.push(skill_tool_def());
        definitions.push(crate::structured_output::tool_def(&json!({"type": "null"})));
        for definition in definitions {
            assert!(
                !definition.name.is_empty()
                    && definition.name.bytes().all(|byte| byte.is_ascii_lowercase()
                        || byte.is_ascii_digit()
                        || byte == b'_'),
                "kloop-owned tool name must be snake_case: {}",
                definition.name
            );
        }
    }

    #[test]
    fn source_can_force_only_selected_tools_to_defer() {
        struct ForcedSource {
            defs: Vec<ToolDef>,
        }
        impl ToolSource for ForcedSource {
            fn defs(&self) -> Arc<[ToolDef]> {
                Arc::from(self.defs.clone())
            }
            fn should_defer(&self, tool: &str) -> bool {
                tool == "srv__resource_helper"
            }
            fn is_readonly(&self, _tool: &str) -> bool {
                true
            }
            fn call<'a>(
                &'a self,
                _tool: &'a str,
                _input: &'a Value,
            ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
                Box::pin(async { Ok(SourceOutput::text("ok".into())) })
            }
        }
        let source: Arc<dyn ToolSource> = Arc::new(ForcedSource {
            defs: vec![
                ToolDef {
                    name: "srv__inline".into(),
                    description: "inline".into(),
                    schema: json!({"type": "object"}),
                },
                ToolDef {
                    name: "srv__resource_helper".into(),
                    description: "deferred".into(),
                    schema: json!({"type": "object"}),
                },
            ],
        });
        let sources = vec![source];
        let defs = all_tool_defs(
            0,
            &sources,
            usize::MAX,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        assert!(names.contains(&"srv__inline"));
        assert!(names.contains(&"tool_search"));
        assert!(names.contains(&"call_tool"));
        assert!(!names.contains(&"srv__resource_helper"));
        assert_eq!(
            deferred_tool_defs(&sources, usize::MAX, &ShellPrograms::native_posix()),
            vec![ToolDef {
                name: "srv__resource_helper".into(),
                description: "deferred".into(),
                schema: json!({"type": "object"}),
            }]
        );
    }

    #[tokio::test]
    async fn builtins_shadow_colliding_sources_at_dispatch() {
        let source: Arc<dyn ToolSource> = Arc::new(StubSource {
            defs: vec![ToolDef {
                name: "bash".into(),
                description: "impostor".into(),
                schema: json!({"type": "object"}),
            }],
            readonly: "bash".into(),
        });
        let ctx = test_ctx_with_sources(0, "builtin-shadow", vec![source]);
        let (direct, direct_error) =
            run_tool("bash", json!({"command": "printf BUILTIN-54"}), &ctx).await;
        assert!(!direct_error, "{direct}");
        assert_eq!(direct, "BUILTIN-54");

        let (wrapped, wrapped_error) = run_tool(
            "call_tool",
            json!({
                "tool_name": "bash",
                "params": {"command": "printf WRAPPED-BUILTIN-54"}
            }),
            &ctx,
        )
        .await;
        assert!(!wrapped_error, "{wrapped}");
        assert_eq!(wrapped, "WRAPPED-BUILTIN-54");
    }

    #[test]
    fn tool_merge_warnings_flags_collisions_and_oversized_lists() {
        assert_eq!(
            tool_merge_warnings(&[], TOOL_DEFER_THRESHOLD, &ShellPrograms::native_posix(),),
            Vec::<String>::new()
        );

        let colliding: Vec<Arc<dyn ToolSource>> =
            vec![StubSource::new("srv"), StubSource::new("srv")];
        let warnings = tool_merge_warnings(
            &colliding,
            TOOL_DEFER_THRESHOLD,
            &ShellPrograms::native_posix(),
        );
        assert_eq!(warnings.len(), 3, "one per duplicated name: {warnings:?}");
        assert!(warnings[0].contains("srv__echo"));
        assert!(warnings[1].contains("srv__fail"));
        assert!(warnings[2].contains("srv__image"));

        let many: Vec<ToolDef> = (0..40)
            .map(|i| ToolDef {
                name: format!("srv__tool{i}"),
                description: String::new(),
                schema: json!({"type": "object"}),
            })
            .collect();
        let big: Vec<Arc<dyn ToolSource>> = vec![Arc::new(StubSource {
            defs: many,
            readonly: String::new(),
        })];
        let warnings =
            tool_merge_warnings(&big, TOOL_DEFER_THRESHOLD, &ShellPrograms::native_posix());
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0]
                .contains(&(tool_defs(0, &ShellPrograms::native_posix()).len() + 40).to_string()),
            "got: {warnings:?}"
        );
        assert!(warnings[0].contains("tool_search"), "got: {warnings:?}");
    }

    /// The defer regime flips on the threshold: at or under, source tools
    /// are inline exactly as before and tool_search does not exist; past it,
    /// the defs shrink to built-ins + tool_search and the source tools move
    /// to the deferred set.
    #[test]
    fn defer_kicks_in_past_threshold() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        let builtin_count = tool_defs(0, &ShellPrograms::native_posix()).len();

        // Exactly at the threshold (built-ins + the stub's 3 tools): everything
        // inline, no tool_search.
        let inline = all_tool_defs(
            0,
            &sources,
            builtin_count + 3,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        assert!(inline.iter().any(|d| d.name == "srv__echo"));
        assert!(inline.iter().all(|d| d.name != "tool_search"));
        assert!(
            deferred_tool_defs(&sources, builtin_count + 3, &ShellPrograms::native_posix(),)
                .is_empty()
        );

        // One past it: built-ins + tool_search + call_tool only; sources
        // deferred. The three always-present depth-0 interaction controls are
        // appended after run_program and do not count toward the threshold.
        let deferred_regime = all_tool_defs(
            0,
            &sources,
            builtin_count + 2,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        let names: Vec<&str> = deferred_regime.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"tool_search"));
        assert!(names.contains(&"call_tool"));
        assert!(names.contains(&"ask_user_question"));
        assert!(names.contains(&"enter_plan_mode"));
        assert!(names.contains(&"exit_plan_mode"));
        assert!(!names.contains(&"srv__echo"));
        assert_eq!(deferred_regime.len(), builtin_count + 5);
        let deferred: Vec<String> =
            deferred_tool_defs(&sources, builtin_count + 2, &ShellPrograms::native_posix())
                .into_iter()
                .map(|d| d.name)
                .collect();
        assert_eq!(deferred, vec!["srv__echo", "srv__fail", "srv__image"]);
    }

    /// Slice 1: inline (below threshold) source tools get a full typed
    /// declaration in run_program's TypeScript API.
    #[test]
    fn run_program_def_declares_inline_source_tools() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        let defs = all_tool_defs(
            0,
            &sources,
            TOOL_DEFER_THRESHOLD,
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        let rp = defs.iter().find(|d| d.name == "run_program").unwrap();
        assert!(
            rp.description.contains("srv__echo(args:"),
            "expected a typed declaration: {}",
            rp.description
        );
        assert!(
            !rp.description.contains("- tools.srv__echo:"),
            "inline tools must not fall back to the manifest: {}",
            rp.description
        );
    }

    /// The typed manifest carries a capped label, not a second copy of the tool
    /// catalog: every declared tool is already in the same request's `tools`
    /// array with its full description, so repeating it verbatim beside the
    /// signature cost ~3.1 KB per request and told the model nothing new.
    /// Deferred tools are the deliberate exception — they have no catalog entry.
    #[test]
    fn typed_manifest_labels_are_capped_but_deferred_lines_stay_whole() {
        let defs = tool_defs(0, &ShellPrograms::native_posix());
        let rp = defs.iter().find(|d| d.name == "run_program").unwrap();
        let labels: Vec<&str> = rp
            .description
            .lines()
            .map(str::trim)
            .filter(|line| line.starts_with("/**"))
            .collect();
        assert!(!labels.is_empty(), "expected typed labels in the manifest");
        for label in &labels {
            let body = label
                .trim_start_matches("/**")
                .trim_end_matches("*/")
                .trim();
            assert!(
                body.chars().count() <= crate::tools::codemode::MANIFEST_SUMMARY_CHARS + 1,
                "label exceeded the manifest cap: {label}"
            );
        }
        // read_file's own description is far longer than the cap, so its label
        // must be the elided prefix rather than the whole paragraph.
        let read_file = defs.iter().find(|d| d.name == "read_file").unwrap();
        assert!(
            read_file.description.chars().count() > crate::tools::codemode::MANIFEST_SUMMARY_CHARS,
            "fixture assumption: read_file has a long description"
        );
        assert!(
            labels.iter().any(|label| label.contains('…')),
            "a description past the cap must be marked elided: {labels:?}"
        );

        // The deferred manifest is the exception: no catalog entry backs it, so
        // its line keeps the whole description.
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        let deferred = all_tool_defs(
            0,
            &sources,
            tool_defs(0, &ShellPrograms::native_posix()).len(),
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        let rp = deferred.iter().find(|d| d.name == "run_program").unwrap();
        let echo = sources[0]
            .defs()
            .iter()
            .find(|d| d.name == "srv__echo")
            .unwrap()
            .description
            .clone();
        assert!(
            rp.description
                .contains(&format!("- tools.srv__echo: {echo}")),
            "deferred manifest line must keep the full description: {}",
            rp.description
        );
    }

    /// Slice 2: past the threshold source tools degrade to a compact name +
    /// description manifest in run_program's description — no full signatures —
    /// with guidance that they stay callable from a program.
    #[test]
    fn run_program_def_lists_deferred_source_tools_as_a_manifest() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        // One source (2 tools) past the built-in count forces the defer regime.
        let defs = all_tool_defs(
            0,
            &sources,
            tool_defs(0, &ShellPrograms::native_posix()).len(),
            interactive_surface(),
            &ShellPrograms::native_posix(),
        );
        let rp = defs.iter().find(|d| d.name == "run_program").unwrap();
        assert!(
            rp.description.contains("- tools.srv__echo:"),
            "expected a manifest line: {}",
            rp.description
        );
        assert!(
            !rp.description.contains("srv__echo(args:"),
            "deferred tools must not be typed in full: {}",
            rp.description
        );
        assert!(
            rp.description
                .contains("cannot call tool_search from inside a program"),
            "expected the program-path guidance: {}",
            rp.description
        );
    }

    /// A source def colliding with a built-in is not callable, so it must
    /// not become discoverable either.
    #[test]
    fn collision_skipped_defs_are_not_deferred() {
        let clash: Vec<Arc<dyn ToolSource>> = vec![Arc::new(StubSource {
            defs: vec![ToolDef {
                name: "bash".into(),
                description: "impostor".into(),
                schema: json!({"type": "object"}),
            }],
            readonly: String::new(),
        })];
        assert!(deferred_tool_defs(&clash, 0, &ShellPrograms::native_posix()).is_empty());
    }

    #[tokio::test]
    async fn dispatch_routes_external_tools_and_maps_errors() {
        let ctx = test_ctx_with_sources(0, "ext", vec![StubSource::new("srv")]);

        let (out, is_error) = run_tool("srv__echo", json!({"text": "hi"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "echoed hi");

        let (out, is_error) = run_tool("srv__fail", json!({}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("stub failure"));

        // Unclaimed names still fail as unknown.
        let (out, is_error) = run_tool("other__tool", json!({}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("unknown tool"));
    }

    #[tokio::test]
    async fn unavailable_source_route_fails_before_call_and_is_not_unknown() {
        let source = Arc::new(UnavailableSource {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let base = test_ctx_with_sources(0, "unavailable-source", vec![source.clone()]);
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules::default(),
            std::env::temp_dir(),
            Some(Arc::new(PanicApprover)),
        )
        .unwrap();
        let mut cfg = base.cfg.test_clone();
        cfg.permissions = Arc::new(permissions);
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (output, is_error) = run_tool("offline__echo", json!({}), &ctx).await;
        assert!(is_error, "{output}");
        assert!(output.contains("unavailable, not missing"), "{output}");
        assert!(!output.contains("unknown tool"), "{output}");
        assert_eq!(source.calls.load(Ordering::Relaxed), 0);

        let (search, is_error) = run_tool(
            "tool_search",
            json!({"query": "select:offline__echo"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{search}");
        assert!(search.contains("unavailable, not missing"), "{search}");
        assert!(!search.contains("no deferred tool named"), "{search}");

        let ready_ctx = test_ctx_with_sources(
            0,
            "unavailable-shadow",
            vec![source, StubSource::new("offline")],
        );
        let (output, is_error) =
            run_tool("offline__echo", json!({"text": "ready"}), &ready_ctx).await;
        assert!(!is_error, "{output}");
        assert_eq!(output, "echoed ready");
    }

    #[tokio::test]
    async fn source_preflight_fails_before_hooks_permission_and_call() {
        let source = Arc::new(PreflightSource {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let base = test_ctx_with_sources(0, "source-preflight", vec![source.clone()]);
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules::default(),
            std::env::temp_dir(),
            Some(Arc::new(PanicApprover)),
        )
        .unwrap();
        let mut cfg = base.cfg.test_clone();
        cfg.permissions = Arc::new(permissions);
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (output, is_error) = run_tool("external__preflight", json!({}), &ctx).await;
        assert!(is_error, "{output}");
        assert!(output.contains("unavailable during preflight"), "{output}");
        assert_eq!(source.calls.load(Ordering::Relaxed), 0);
    }

    /// An external (MCP) tool that returns an image lands a Blocks tool_result,
    /// not flattened text — the model sees the picture. (run_tool would flatten
    /// it, so dispatch is driven directly to inspect the raw content.)
    #[tokio::test]
    async fn external_tool_image_result_is_a_blocks_tool_result() {
        let ctx = test_ctx_with_sources(0, "extimg", vec![StubSource::new("srv")]);
        let results =
            dispatch_tools(vec![("t".into(), "srv__image".into(), json!({}))], &ctx).await;
        let ContentBlock::ToolResult {
            content, is_error, ..
        } = &results[0]
        else {
            panic!("expected tool result");
        };
        assert!(!is_error);
        assert_eq!(
            content,
            &ToolResultContent::Blocks(vec![ContentBlock::Image {
                source: kloop_protocol::ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGk=".into(),
                },
            }])
        );
    }

    /// A sub-agent with a tool allowlist has calls to tools outside it
    /// rejected at dispatch (defense in depth — the defs are already
    /// filtered), while read_offloaded stays available regardless.
    #[tokio::test]
    async fn tool_allowlist_rejects_tools_outside_the_set() {
        let base = test_ctx(1, "allowlist");
        let mut cfg = base.cfg.test_clone();
        cfg.tool_allowlist = Some(Arc::new(["grep".to_string()].into_iter().collect()));
        let ctx = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (out, is_error) = run_tool("bash", bash_input("echo hi"), &ctx).await;
        assert!(is_error);
        assert!(out.contains("not available to this agent type"), "{out}");

        // Whitelisted tool is not blocked by the allowlist.
        let (out, _) = run_tool("grep", json!({"pattern": "x"}), &ctx).await;
        assert!(!out.contains("not available to this agent type"), "{out}");

        // read_offloaded is the infra exception: never blocked by the list.
        let (out, _) = run_tool("read_offloaded", json!({"id": "off-9999"}), &ctx).await;
        assert!(!out.contains("not available to this agent type"), "{out}");
    }

    /// A child cannot gain root Task graph capability through an explicit custom
    /// allowlist, a forged call, or the deferred call_tool envelope.
    #[tokio::test]
    async fn child_task_calls_fail_before_the_registry_even_when_allowlisted() {
        let root = test_ctx(0, "root-task-gate");
        let (created, is_error) = run_tool(
            "task_create",
            json!({"subject":"root work","description":"owned by root"}),
            &root,
        )
        .await;
        assert!(!is_error, "{created}");

        let task_tools = [
            "task_create",
            "task_get",
            "task_update",
            "task_list",
            "task_clear",
        ];
        let mut cfg = root.cfg.test_clone();
        cfg.tool_allowlist = Some(Arc::new(
            task_tools.into_iter().map(str::to_string).collect(),
        ));
        let child = ToolCtx {
            cfg: Arc::new(cfg),
            depth: 1,
            ..root.clone()
        };
        for (name, input) in [
            (
                "task_create",
                json!({"subject":"forged","description":"must not exist"}),
            ),
            ("task_get", json!({"task_id":"1"})),
            ("task_update", json!({"task_id":"1","status":"completed"})),
            ("task_list", json!({})),
            ("task_clear", json!({})),
        ] {
            let (output, is_error) = run_tool(name, input, &child).await;
            assert!(is_error, "{name}: {output}");
            assert_eq!(
                output,
                format!("tool '{name}' is only available to the root agent")
            );
        }
        let (output, is_error) = run_tool(
            "call_tool",
            json!({"tool_name":"task_list","params":{}}),
            &child,
        )
        .await;
        assert!(is_error, "{output}");
        assert_eq!(
            output,
            "tool 'task_list' is only available to the root agent"
        );

        let (task, is_error) = run_tool("task_get", json!({"task_id":"1"}), &root).await;
        assert!(!is_error, "{task}");
        let task: Value = serde_json::from_str(&task).unwrap();
        assert_eq!(task["task"]["status"], "pending");
        assert!(task["task"].get("owner").is_none());
        let (listed, is_error) = run_tool("task_list", json!({}), &root).await;
        assert!(!is_error, "{listed}");
        let listed: Value = serde_json::from_str(&listed).unwrap();
        assert_eq!(listed["tasks"].as_array().unwrap().len(), 1);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_task_gate_runs_before_pre_tool_hooks() {
        let marker =
            std::env::temp_dir().join(format!("kloop-child-task-hook-{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let base = test_ctx(1, "root-task-hook-order");
        let mut cfg = base.cfg.test_clone();
        cfg.hooks = Arc::new(crate::hooks::Hooks {
            defs: vec![crate::hooks::HookDef {
                event: crate::hooks::HookEvent::PreTool,
                command: vec![
                    "sh".into(),
                    "-c".into(),
                    format!("printf ran > '{}'", marker.display()),
                ],
                matcher: Some("task_list".into()),
                timeout_ms: crate::hooks::DEFAULT_TIMEOUT_MS,
            }],
        });
        let child = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (output, is_error) = run_tool("task_list", json!({}), &child).await;
        assert!(is_error, "{output}");
        assert_eq!(
            output,
            "tool 'task_list' is only available to the root agent"
        );
        assert!(!marker.exists(), "pre-tool hook ran before the root gate");
    }

    #[test]
    fn external_tools_are_serial_unless_marked_readonly() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        assert!(is_concurrency_safe("srv__echo", &json!({}), &sources));
        assert!(!is_concurrency_safe("srv__fail", &json!({}), &sources));
        // Unknown to every source: not safe.
        assert!(!is_concurrency_safe("other__tool", &json!({}), &sources));
        // Without sources nothing external is safe.
        assert!(!is_concurrency_safe("srv__echo", &json!({}), &[]));
    }

    #[tokio::test]
    async fn cancellation_mid_execution_interrupts_the_call() {
        let ctx = test_ctx(0, "midcancel");
        let cancel = ctx.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });
        let started = std::time::Instant::now();
        let (out, is_error) = run_tool("bash", bash_input("sleep 30"), &ctx).await;
        assert!(is_error);
        assert_eq!(out, "interrupted");
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
    }

    struct PendingApprover {
        entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }

    impl crate::permissions::Approver for PendingApprover {
        fn confirm(
            &self,
            _request: crate::permissions::ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = crate::permissions::Decision> + Send + '_>> {
            if let Some(entered) = self.entered.lock().unwrap().take() {
                let _ = entered.send(());
            }
            Box::pin(std::future::pending())
        }
    }

    #[tokio::test]
    async fn cancellation_while_bash_approval_is_pending_never_spawns() {
        let marker = std::env::temp_dir().join(format!(
            "kloop-pending-bash-approval-{}.txt",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&marker);
        let (entered, pending) = tokio::sync::oneshot::channel();
        let permissions = crate::permissions::Permissions::new(
            crate::permissions::Mode::Manual,
            &crate::permissions::PermissionRules::default(),
            std::env::current_dir().unwrap(),
            Some(Arc::new(PendingApprover {
                entered: std::sync::Mutex::new(Some(entered)),
            })),
        )
        .unwrap();
        let base = test_ctx(0, "pending-bash-approval");
        let mut config = base.cfg.test_clone();
        config.permissions = Arc::new(permissions);
        let ctx = ToolCtx {
            cfg: Arc::new(config),
            ..base
        };
        let command = format!("printf spawned > '{}'", marker.display());
        let execution = run_tool("bash", bash_input(&command), &ctx);
        let cancel = async {
            pending.await.expect("approval prompt was not entered");
            ctx.cancel.cancel();
        };
        let (result, ()) = tokio::join!(execution, cancel);
        let (out, is_error) = result;
        assert!(is_error);
        assert_eq!(out, "interrupted");
        assert!(!marker.exists(), "cancelled approval still spawned Bash");
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn concurrent_readonly_bash_calls_overlap_and_cancel_together() {
        let root = std::env::temp_dir().join(format!(
            "kloop-concurrent-bash-cancel-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let first = root.join("first.fifo");
        let second = root.join("second.fifo");
        for path in [&first, &second] {
            assert!(
                std::process::Command::new("mkfifo")
                    .arg(path)
                    .status()
                    .unwrap()
                    .success()
            );
        }

        let ctx = test_ctx(0, "concurrent-bash-cancel");
        let calls = vec![
            (
                "bash-overlap-a".into(),
                "bash".into(),
                bash_input(&format!("cat '{}'", first.display())),
            ),
            (
                "bash-overlap-b".into(),
                "bash".into(),
                bash_input(&format!("cat '{}'", second.display())),
            ),
        ];
        let execution = dispatch_tools(calls, &ctx);
        let first_writer = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().write(true).open(first)
        });
        let second_writer = tokio::task::spawn_blocking(move || {
            std::fs::OpenOptions::new().write(true).open(second)
        });
        let control = async {
            let (first, second) = tokio::join!(first_writer, second_writer);
            let first = first.unwrap().unwrap();
            let second = second.unwrap().unwrap();
            ctx.cancel.cancel();
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            drop((first, second));
        };
        let (results, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(execution, control)
        })
        .await
        .expect("readonly Bash calls did not overlap");
        assert_eq!(results.len(), 2);
        for (result, expected_id) in results.iter().zip(["bash-overlap-a", "bash-overlap-b"]) {
            let ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
            } = result
            else {
                panic!("expected tool result")
            };
            assert_eq!(tool_use_id, expected_id);
            assert_eq!(content.as_text().as_ref(), "interrupted");
            assert!(is_error);
        }
        let _ = std::fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn concurrent_powershell_calls_share_the_session_gate() {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let (ctx, mut gate) =
                with_powershell_gate_probe(test_ctx(0, "powershell-session-gate"));
            let first_ctx = ctx.clone();
            let first = tokio::spawn(async move {
                run_tool(
                    "powershell",
                    json!({"command": "Write-Output done"}),
                    &first_ctx,
                )
                .await
            });
            gate.wait_attempted().await;
            gate.wait_entered().await;
            assert_eq!(
                gate.snapshot(),
                crate::config::PowerShellGateSnapshot {
                    active: 1,
                    max_active: 1,
                    entries: 1,
                }
            );

            let second_ctx = ctx.clone();
            let second = tokio::spawn(async move {
                run_tool(
                    "powershell",
                    json!({"command": "Write-Output done"}),
                    &second_ctx,
                )
                .await
            });
            gate.wait_attempted().await;
            assert_eq!(
                gate.snapshot(),
                crate::config::PowerShellGateSnapshot {
                    active: 1,
                    max_active: 1,
                    entries: 1,
                }
            );

            gate.release_one();
            gate.wait_entered().await;
            assert_eq!(
                gate.snapshot(),
                crate::config::PowerShellGateSnapshot {
                    active: 1,
                    max_active: 1,
                    entries: 2,
                }
            );
            gate.release_one();

            let (first, second) = tokio::join!(first, second);
            assert_powershell_done(first.unwrap());
            assert_powershell_done(second.unwrap());
            assert_eq!(
                gate.snapshot(),
                crate::config::PowerShellGateSnapshot {
                    active: 0,
                    max_active: 1,
                    entries: 2,
                }
            );
        })
        .await
        .expect("PowerShell session gate test stalled");
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn cancelling_while_waiting_for_powershell_gate_never_spawns() {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            let (ctx, mut gate) = with_powershell_gate_probe(test_ctx(0, "powershell-gate-cancel"));
            let guard = ctx.cfg.powershell_execution_gate.lock_without_probe().await;
            let worker_ctx = ctx.clone();
            let worker = tokio::spawn(async move {
                run_tool(
                    "powershell",
                    json!({"command": "Write-Output should-not-spawn"}),
                    &worker_ctx,
                )
                .await
            });
            gate.wait_attempted().await;
            assert_eq!(gate.snapshot(), Default::default());

            ctx.cancel.cancel();
            let result = worker.await.unwrap();
            assert_eq!(result, ("interrupted".into(), true));
            assert_eq!(gate.snapshot(), Default::default());
            drop(guard);
        })
        .await
        .expect("PowerShell gate waiter ignored cancellation");
    }

    #[tokio::test]
    async fn dispatch_preserves_request_order_across_mixed_batches() {
        let ctx = test_ctx(0, "order");
        let results = dispatch_tools(
            vec![
                ("t1".into(), "bash".into(), bash_input("echo a")),
                ("t2".into(), "bash".into(), bash_input("true")), // unsafe
                ("t3".into(), "bash".into(), bash_input("echo c")),
            ],
            &ctx,
        )
        .await;
        let ids: Vec<&str> = results
            .iter()
            .map(|r| match r {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
                _ => panic!(),
            })
            .collect();
        assert_eq!(ids, vec!["t1", "t2", "t3"]);
    }

    #[test]
    fn concurrency_safety_by_name_and_input() {
        fn is_concurrency_safe(name: &str, input: &Value) -> bool {
            super::is_concurrency_safe(name, input, &[])
        }
        assert!(is_concurrency_safe("read_file", &json!({"path": "x"})));
        assert!(is_concurrency_safe(
            "read_offloaded",
            &json!({"id": "off-0001"})
        ));
        assert!(is_concurrency_safe("grep", &json!({"pattern": "x"})));
        assert!(is_concurrency_safe("glob", &json!({"pattern": "*.rs"})));
        assert!(is_concurrency_safe(
            "bash_output",
            &json!({"bash_id": "bg-1"})
        ));
        assert!(is_concurrency_safe(
            "stop_bash",
            &json!({"bash_id": "bg-1"})
        ));
        assert!(is_concurrency_safe("task_get", &json!({"task_id":"1"})));
        assert!(is_concurrency_safe("task_list", &json!({})));
        assert!(!is_concurrency_safe("task_clear", &json!({})));
        assert!(!is_concurrency_safe(
            "task_create",
            &json!({"subject":"x","description":"y"})
        ));
        assert!(!is_concurrency_safe(
            "task_update",
            &json!({"task_id":"1","status":"completed"})
        ));
        assert!(!is_concurrency_safe(
            "write_file",
            &json!({"path": "x", "content": ""})
        ));
        assert!(!is_concurrency_safe("edit_file", &json!({})));
        // Consecutive run_agent calls run as parallel sub-agents.
        assert!(is_concurrency_safe("run_agent", &json!({"prompt": "x"})));
        assert!(is_concurrency_safe(
            "stop_program",
            &json!({"program_id": "program-1"})
        ));
        assert!(!is_concurrency_safe("wait_for_activity", &json!({})));

        // read-only commands, incl. pipes and chains of safe segments
        assert!(is_concurrency_safe("bash", &bash_input("ls -la")));
        assert!(is_concurrency_safe(
            "bash",
            &bash_input("cat a.txt | grep foo")
        ));
        assert!(is_concurrency_safe(
            "bash",
            &bash_input("pwd && git status; wc -l f")
        ));
        assert!(is_concurrency_safe("bash", &bash_input("git log -5")));

        // unsafe: substitution, newline/background chaining, redirect,
        // unknown command, unsafe git subcommand, empty
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("cat $(rm -rf /tmp/x)")
        ));
        assert!(!is_concurrency_safe("bash", &bash_input("ls `evil`")));
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("ls\nrm -rf /tmp/x")
        ));
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("ls & rm -rf /tmp/x")
        ));
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("echo hi > out.txt")
        ));
        assert!(!is_concurrency_safe("bash", &bash_input("rm -rf /tmp/x")));
        assert!(!is_concurrency_safe(
            "bash",
            &bash_input("ls && make build")
        ));
        assert!(!is_concurrency_safe("bash", &bash_input("git push")));
        assert!(!is_concurrency_safe("bash", &bash_input("   ")));
        assert!(!is_concurrency_safe("bash", &json!({})));
    }

    #[test]
    fn char_prefix_never_splits_utf8() {
        assert_eq!(char_prefix("a界b", 2), ("a界", true));
        assert_eq!(char_prefix("a界b", 3), ("a界b", false));
        assert_eq!(char_prefix("a界b", 99), ("a界b", false));
    }

    #[tokio::test]
    async fn cancelled_dispatch_patches_every_tool_use() {
        use kloop_provider::Provider;

        struct NullUi;
        impl Ui for NullUi {
            fn emit(&self, _: &Event) {}
        }

        let (provider_catalog, provider_route) =
            crate::provider_route::ProviderCatalog::from_provider(
                "test",
                Provider::mock(vec![]),
                "mock",
                vec!["mock".into()],
                None,
            )
            .unwrap();
        let cancel = CancellationToken::new();
        cancel.cancel();
        let inbox = Arc::new(crate::inbox::Inbox::default());
        let ctx = ToolCtx {
            cfg: Arc::new(Config {
                provider_catalog,
                provider_route,
                system: "test".into(),
                project_instructions: None,
                max_rounds: Some(5),
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                offload_dir: std::env::temp_dir().join("kloop-test-cancel"),
                sessions_dir: std::env::temp_dir().join("kloop-test-cancel-sessions"),
                context_window: None,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                questioner: None,
                file_state: Default::default(),
                tool_sources: Vec::new(),
                session_id: String::new(),
                local_agent: crate::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
                hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
                background_shells: BackgroundShells::new(),
                shell_programs: std::sync::Arc::new(
                    crate::shell_programs::ShellPrograms::test_fixture(),
                ),
                powershell_execution_gate: Default::default(),
                sandbox: None,
                agent_types: Arc::new(Vec::new()),
                tool_allowlist: None,
                defer_threshold: 30,
                unlocked_tools: Default::default(),
                tasks: Default::default(),
                inbox: Arc::clone(&inbox),
                scheduler: crate::scheduler::Scheduler::in_memory(inbox),
                background_executions: Default::default(),
                program_limits: Default::default(),
                skills: Default::default(),
                active_worktree: std::sync::Arc::new(
                    crate::worktree::ActiveWorktreeState::default(),
                ),
                surface: Default::default(),
            }),
            ui: Arc::new(NullUi),
            cancel,
            depth: 0,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
        };
        let results = dispatch_tools(
            vec![
                ("t1".into(), "bash".into(), bash_input("ls")),
                (
                    "t2".into(),
                    "write_file".into(),
                    json!({"path": "x", "content": "y"}),
                ),
            ],
            &ctx,
        )
        .await;
        assert_eq!(
            results,
            vec![
                ContentBlock::ToolResult {
                    tool_use_id: "t1".into(),
                    content: "interrupted".into(),
                    is_error: true,
                },
                ContentBlock::ToolResult {
                    tool_use_id: "t2".into(),
                    content: "interrupted".into(),
                    is_error: true,
                },
            ]
        );
    }
}
