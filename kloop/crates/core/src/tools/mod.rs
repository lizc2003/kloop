//! The tool seam: definitions, per-input concurrency classification, and
//! the gated dispatch loop (hooks → permissions → execution). Individual
//! tool implementations live in the sibling modules; this file is what the
//! agent loop and the frontends depend on.

mod background_tasks;
mod bash;
mod codemode;
mod fs;
mod inject;
#[cfg(test)]
mod plan49_parity_tests;
mod plan_mode;
mod search;
mod skill;
mod task;
mod todo;
mod tool_search;
pub mod web;
mod worktree_tool;

pub use background_tasks::BackgroundTasks;
pub use background_tasks::TaskStatus;
pub use bash::BackgroundShells;
// Slash-path prompt injections (`!cmd` / `@file`) for `/name` commands; the
// dispatch layer (`crate::commands`) calls this before running the turn.
pub(crate) use inject::expand_slash_injections;
// Registered on the task tool's peer set only at depth 0 with skills loaded
// (see `turn_rounds`); the pure skill logic it drives lives in `crate::skills`.
pub(crate) use skill::skill_tool_def;
pub use tool_search::deferred_notice;
// The skills module (`crate::skills`) dispatches a `context: fork` skill here,
// reusing the task sub-agent machinery.
pub(crate) use task::fork_skill;
pub use todo::parse_todos;
pub use todo::TodoItem;
pub use todo::TodoStatus;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::Ui;
use crate::config::Config;
use crate::event::Event;
use crate::event::Item;
use crate::event::ItemStatus;
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

struct ToolExecution {
    result: Result<ToolResultContent>,
    file_state_update: Option<(
        Arc<crate::file_state::FileState>,
        crate::file_state::FileStateUpdate,
    )>,
    path_lock: Option<tokio::sync::OwnedMutexGuard<()>>,
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
    fn defs(&self) -> &[ToolDef];
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
    /// Id of the parent session's line that this round's assistant message was
    /// recorded as (`{stem}#{seq}`), or None for an in-memory-only session. The
    /// task tool stamps it as the spawned sub-agent's `subagent_of`
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
/// search results. The returned array is then stable for the whole session —
/// unlocking never mutates it (prompt-cache friendliness is the point).
pub fn all_tool_defs(
    depth: u8,
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    worktree_enabled: bool,
) -> Vec<ToolDef> {
    let mut defs = builtin_defs(depth);
    let deferred_regime = defer_active(sources, defer_threshold);
    // Source tools the program's TypeScript API declares in full (inline
    // regime) — empty when deferred, where they degrade to a compact name +
    // description list instead (see `run_program_def`).
    let inline_sources = if deferred_regime {
        defs.push(tool_search::tool_search_def());
        defs.push(tool_search::call_tool_def());
        Vec::new()
    } else {
        let merged = merged_source_defs(sources);
        defs.extend(merged.iter().cloned());
        merged
    };
    // run_program is depth-0 only (like task). Now that sources are visible, its
    // TypeScript API can list them: full declarations for inline source tools
    // (typed `Promise<CallToolResult>`), or a compact manifest for deferred
    // ones — both callable at runtime.
    if depth == 0 {
        let deferred = if deferred_regime {
            deferred_tool_defs(sources, defer_threshold)
        } else {
            Vec::new()
        };
        defs.push(codemode::run_program_def(
            &builtin_defs(0),
            &inline_sources,
            &deferred,
        ));
        // Plan mode (plan 37): the model presents its plan for approval and, on
        // approval, leaves plan mode. Session-control like the worktree tools —
        // depth-0 only, kept out of run_program's TS API and the defer count.
        // Advertised unconditionally so the tool array stays byte-stable while
        // the mode toggles (shift+Tab / exit) within the session; it errors
        // cleanly when called outside plan mode.
        defs.push(plan_mode::exit_plan_mode_def());
        // Session worktree tools (plan 35 slice 2): only when the front-end
        // enables worktree mode (CLI/TUI/plain — not server threads or --mock),
        // and only top-level (a sub-agent isolates via task {isolation}). Kept
        // out of run_program's TS API and the deferral count on purpose.
        if worktree_enabled {
            defs.push(worktree_tool::enter_worktree_def());
            defs.push(worktree_tool::exit_worktree_def());
        }
    }
    defs
}

/// Whether the deferred-tools regime is on. The verdict is computed from the
/// depth-0 view (the most built-ins) and holds session-wide, so parent and
/// sub-agents never disagree about which tools are deferred.
pub fn defer_active(sources: &[Arc<dyn ToolSource>], defer_threshold: usize) -> bool {
    tool_defs(0).len() + merged_source_defs(sources).len() > defer_threshold
}

/// The source tools hidden behind tool_search: every merged source def when
/// deferral is active, none otherwise. Collision-skipped defs are excluded —
/// they are not callable, so they must not be discoverable either.
pub fn deferred_tool_defs(sources: &[Arc<dyn ToolSource>], defer_threshold: usize) -> Vec<ToolDef> {
    if defer_active(sources, defer_threshold) {
        merged_source_defs(sources)
    } else {
        Vec::new()
    }
}

/// Source defs deduplicated against the depth-0 built-ins and earlier
/// sources — the same merge order [`all_tool_defs`] uses.
fn merged_source_defs(sources: &[Arc<dyn ToolSource>]) -> Vec<ToolDef> {
    let mut seen: std::collections::HashSet<String> =
        tool_defs(0).into_iter().map(|d| d.name).collect();
    let mut defs = Vec::new();
    for source in sources {
        for def in source.defs() {
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
pub fn tool_merge_warnings(sources: &[Arc<dyn ToolSource>], defer_threshold: usize) -> Vec<String> {
    let mut warnings = Vec::new();
    let mut seen: std::collections::HashSet<String> =
        tool_defs(0).into_iter().map(|d| d.name).collect();
    for source in sources {
        for def in source.defs() {
            if !seen.insert(def.name.clone()) {
                warnings.push(format!(
                    "tool name collision: '{}' is already registered; the later definition is skipped",
                    def.name
                ));
            }
        }
    }
    let total = seen.len();
    if total > defer_threshold {
        warnings.push(format!(
            "{total} tools registered (> {defer_threshold}); MCP tool definitions are deferred — the model loads them on demand via tool_search"
        ));
    }
    warnings
}

fn find_source<'a>(
    sources: &'a [Arc<dyn ToolSource>],
    name: &str,
) -> Option<&'a Arc<dyn ToolSource>> {
    // First source claiming the name wins, mirroring the merge order.
    sources
        .iter()
        .find(|s| s.defs().iter().any(|d| d.name == name))
}

/// The built-in tool defs (bash, file, search, todo, and — at depth 0 —
/// `task`). This is the set `run_program` derives its TypeScript API from, so
/// it deliberately excludes `run_program` itself: no self-reference, and no
/// throwaway description regeneration when only counting is needed.
fn builtin_defs(depth: u8) -> Vec<ToolDef> {
    let mut defs = vec![
        ToolDef {
            name: "bash".into(),
            description: "Run a shell command with `sh -lc`. stdout and stderr are merged; a non-zero exit status is appended. Default timeout 60s. For long-running commands (dev servers, watches, slow builds) set run_in_background instead of appending '&'. When OS sandboxing is active, commands run with file writes limited to the workspace and temp directories and no network access; a failure that looks sandbox-caused is annotated in the result.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The command to run"},
                    "timeout_ms": {"type": "integer", "description": "Timeout in milliseconds (default 60000); ignored when run_in_background is set"},
                    "run_in_background": {"type": "boolean", "description": "Run in the background: returns immediately with an ID and an output file path. Check on it later with bash_output or by reading the output file; stop it with kill_bash."},
                    "disable_sandbox": {"type": "boolean", "description": "Run without the OS sandbox. Only set this after a command failed from sandbox restrictions (writes outside the workspace, network access) and that access is genuinely needed — never preemptively; the unsandboxed run requires user approval."}
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: "bash_output".into(),
            description: "Retrieve the status and output of a background bash command. Blocks until it finishes by default (up to timeout_ms); pass block=false to peek without waiting. Returns the tail of the output; read the output file for the rest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "ID from a run_in_background bash call, e.g. bg-1"},
                    "block": {"type": "boolean", "description": "Wait for completion (default true)"},
                    "timeout_ms": {"type": "integer", "description": "Max wait when blocking (default 30000, max 600000)"}
                },
                "required": ["bash_id"]
            }),
        },
        ToolDef {
            name: "kill_bash".into(),
            description: "Stop a running background bash command by ID; kills its whole process group.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "bash_id": {"type": "string", "description": "ID from a run_in_background bash call, e.g. bg-1"}
                },
                "required": ["bash_id"]
            }),
        },
        ToolDef {
            name: "read_file".into(),
            description: "Read a file. Text files return numbered lines formatted as `{n}\\t{line}` with a bounded character budget; use offset/limit to page. Empty files and offsets past EOF return explicit warnings. Image files (png, jpeg, gif, webp; up to 5 MiB) are returned as an image you can see — offset/limit do not apply. PDFs return an explicit unsupported error.".into(),
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
            description: "Write content to a file, creating parent directories as needed. Overwrites if the file exists.".into(),
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
            description: "Replace old_string with new_string in a file. Fails if old_string is not found, or matches more than once without replace_all.".into(),
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
    // Available at every depth (sub-agents plan too); task is depth-0 only.
    defs.push(todo::todo_write_def());
    if depth == 0 {
        defs.push(ToolDef {
            name: "task".into(),
            description: "Spawn a sub-agent with a fresh history to work on a self-contained prompt. By default this blocks and returns the sub-agent's final text; consecutive task calls in one response run as parallel sub-agents — use that for independent subtasks. Pass background=true to fire-and-forget instead: it returns immediately with an agent id (agent-N) and the sub-agent's result is delivered to you as a message when it finishes — use this to keep working while a long subtask runs, then block for it with the wait tool. Sub-agents cannot spawn further sub-agents. Pass agent_type to use a configured specialized agent (see below); omit it for a general-purpose sub-agent.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Complete standalone task description"},
                    "agent_type": {"type": "string", "description": "Name of a configured agent type to use (its own system prompt, model, and tools); omit for a general-purpose sub-agent"},
                    "background": {"type": "boolean", "description": "Fire-and-forget: return an agent id immediately and deliver the result as a message when it finishes, instead of blocking (default false)"},
                    "max_rounds": {"type": "integer", "minimum": 1, "description": "Optional round cap for the sub-agent; omitted means no round limit"},
                    "isolation": {"type": "string", "enum": ["shared", "worktree"], "description": "Where the sub-agent works. \"shared\" (default) uses the current working directory. \"worktree\" gives it a private git worktree on its own branch, so parallel sub-agents can edit the same files without conflicting; unmerged changes are reported back on their branch for you to merge. Requires a git repository."}
                },
                "required": ["prompt"]
            }),
        });
        defs.push(ToolDef {
            name: "wait".into(),
            description: "Block until a background sub-agent (dispatched with task background=true) finishes, or new input arrives, or the timeout passes. Returns a short status; the finished sub-agent's result is delivered separately as a message. Only useful when background sub-agents are running.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "timeout_ms": {"type": "integer", "description": "Max wait (default 30000, min 10000, max 3600000)"}
                }
            }),
        });
        defs.push(ToolDef {
            name: "stop_agent".into(),
            description: "Stop a running background sub-agent by id (agent-N). It ends without reporting a result.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "agent_id": {"type": "string", "description": "The agent id from a background task call, e.g. agent-2"}
                },
                "required": ["agent_id"]
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
pub fn tool_defs(depth: u8) -> Vec<ToolDef> {
    let mut defs = builtin_defs(depth);
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
        // bash_output only reads registry state; kill_bash only signals
        // processes this agent itself started (cc marks both concurrency-safe).
        "bash_output" | "kill_bash" => true,
        // tool_search reads defs and grows the unlock set — monotonic,
        // order-independent state, safe to batch.
        "tool_search" => true,
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
        // task is always safe to batch (cc shape): consecutive task calls run
        // as parallel sub-agents. Their own tool calls are gated individually
        // — a sub-agent's write still faces hooks and the permission gate.
        "task" => true,
        // stop_agent only signals a sub-agent's own cancel token — like
        // kill_bash, nothing the batch could race on. wait BLOCKS, so it must
        // run alone (batching it would stall its siblings behind the deadline).
        "stop_agent" => true,
        "wait" => false,
        "write_file" | "edit_file" => false,
        other => find_source(sources, other).is_some_and(|s| s.is_readonly(other)),
    }
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
    let tool_uses: Vec<(String, String, Value)> = tool_uses
        .into_iter()
        .map(|(id, name, input)| {
            let (name, input) = tool_search::unwrap_call_tool(name, input);
            (id, name, input)
        })
        .collect();
    let sources = &ctx.cfg.tool_sources;
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
                run_one(id.clone(), name.clone(), input.clone(), ctx.clone())
            });
            results.extend(futures::future::join_all(futs).await);
        } else {
            for (id, name, input) in batch {
                if ctx.cancel.is_cancelled() {
                    results.push(interrupted(id));
                } else {
                    results
                        .push(run_one(id.clone(), name.clone(), input.clone(), ctx.clone()).await);
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

async fn run_one(id: String, name: String, input: Value, ctx: ToolCtx) -> ContentBlock {
    // todo_write surfaces only as its `Todo` item (emitted from the tool itself),
    // never as a generic tool row: cc renders the checklist in the call's place.
    // Suppressing the ToolCall events here means no front-end needs a skip.
    let tool_row = name != "todo_write";
    if tool_row {
        ctx.ui.emit(&Event::ItemStarted {
            id: id.clone(),
            item: Item::ToolCall {
                agent: ctx.cfg.agent_label.clone(),
                name: name.clone(),
                input: input.clone(),
                status: ItemStatus::InProgress,
                output: None,
            },
        });
    }
    let event_input = input.clone();
    let gated = async {
        // A custom agent type's tool allowlist is a capability gate: the tool
        // is filtered out of this sub-agent's defs, so a call to it is a
        // hallucination — reject before hooks or the human are consulted.
        // (The main agent has no allowlist, so this never fires for it.)
        if !crate::agent_type::tool_available(ctx.cfg.tool_allowlist.as_deref(), &name) {
            bail!("tool '{name}' is not available to this agent type");
        }
        // Locked deferred tools bounce before hooks and permissions: the
        // model skipped tool_search, and neither automation policy nor the
        // human should be consulted about a call that cannot run. This is
        // also the only rejection that does NOT unlock — unlocking flows
        // exclusively through a tool_search hit. A program bypasses this gate:
        // its `tools` object already exposes the tool, so it is loaded for the
        // program (the top-level model still must tool_search to direct-call).
        if !ctx.from_program && tool_search::locked(&name, &ctx.cfg) {
            bail!(
                "tool '{name}' is deferred and not loaded yet; call tool_search with query \"select:{name}\" to load its definition, then retry"
            );
        }
        // pre_tool hooks run BEFORE the permission gate: hooks are automation
        // policy, permissions are the human's last word — a hook block means
        // there is nothing left to ask about.
        let hooks = &ctx.cfg.hooks;
        let session_id = &ctx.cfg.session_id;
        let agent = &ctx.cfg.agent_label;
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
        // Prepare mutations after pre-hooks but before permission. The gate sees
        // the canonical effective target, while the executor retains an open
        // parent directory handle across any approval wait.
        let input = input;
        let prepared_mutation = if matches!(name.as_str(), "write_file" | "edit_file") {
            Some(fs::prepare_mutation_input(&name, &input, &ctx).await?)
        } else {
            None
        };
        let prepared_read = if name == "read_file" {
            Some(fs::prepare_read(&input, &ctx).await?)
        } else {
            None
        };
        let sandbox_auto_allow = bash::sandbox_auto_allowed(&name, &input, &ctx);
        // effective_*: gate on the active worktree's re-anchored permissions
        // when the session entered one (plan 35 slice 2), else the base gate.
        if let Err(reason) = ctx
            .cfg
            .effective_permissions()
            .check_call_with_resolved_path(
                &name,
                &input,
                prepared_mutation
                    .as_ref()
                    .map(fs::PreparedMutation::resolved_path)
                    .or(prepared_read.as_ref().map(fs::PreparedRead::resolved_path)),
                ctx.depth,
                sandbox_auto_allow,
            )
            .await
        {
            bail!(reason);
        }
        let execution = execute_tool(
            &name,
            &input,
            prepared_read.as_ref(),
            prepared_mutation.as_ref(),
            &ctx,
        )
        .await;
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
    let (result, file_state_update, path_lock) = tokio::select! {
        _ = ctx.cancel.cancelled() => (interrupted(&id), None, None),
        r = gated => match r {
            Ok(execution) => {
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
            Err(e) => (ContentBlock::ToolResult {
                tool_use_id: id.clone(),
                content: format!("{e:#}").into(),
                is_error: true,
            }, None, None),
        },
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
    if tool_row {
        ctx.ui.emit(&Event::ItemCompleted {
            id: tool_use_id.clone(),
            item: Item::ToolCall {
                agent: ctx.cfg.agent_label.clone(),
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
    }
    result
}

/// Returns an explicitly type-erased future: this is the recursion boundary
/// (execute_tool -> task -> run_turn -> dispatch_tools -> execute_tool), and
/// the `dyn Future + Send` signature is what lets rustc resolve the otherwise
/// cyclic Send inference for the recursive async call graph.
fn execute_tool<'a>(
    name: &'a str,
    input: &'a Value,
    prepared_read: Option<&'a fs::PreparedRead>,
    prepared_mutation: Option<&'a fs::PreparedMutation>,
    ctx: &'a ToolCtx,
) -> Pin<Box<dyn Future<Output = ToolExecution> + Send + 'a>> {
    Box::pin(async move {
        // read_file is the sole BUILT-IN that can return non-text: on an image
        // file it returns an image block (ToolResultContent::Blocks).
        if name == "read_file" {
            let Some(prepared) = prepared_read else {
                return ToolExecution::from_result(Err(anyhow!(
                    "read_file: target was not prepared"
                )));
            };
            let state = ctx.cfg.effective_file_state();
            return match fs::read_file_tool(input, prepared).await {
                Ok(output) => ToolExecution {
                    result: Ok(output.content),
                    file_state_update: Some((state, output.state_update)),
                    path_lock: None,
                },
                Err(error) => ToolExecution::from_result(Err(error)),
            };
        }
        if name == "write_file" || name == "edit_file" {
            let Some(prepared) = prepared_mutation else {
                return ToolExecution::from_result(Err(anyhow!(
                    "{name}: mutation target was not prepared"
                )));
            };
            let state = ctx.cfg.effective_file_state();
            let output = if name == "write_file" {
                fs::write_file_tool(input, prepared, ctx).await
            } else {
                fs::edit_file_tool(input, prepared, ctx).await
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
        if let Some(source) = find_source(&ctx.cfg.tool_sources, name) {
            return match source.call(name, input).await {
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
            "bash" => bash::bash_tool(input, ctx).await,
            "bash_output" => bash::bash_output_tool(input, ctx).await,
            "kill_bash" => bash::kill_bash_tool(input, ctx).await,
            "grep" => {
                search::grep_tool(input, &ctx.cfg.effective_cwd(), ctx.cfg.effective_permissions())
                    .await
            }
            // glob hands a program its path list as a string[] (built-ins are
            // otherwise strings); the model-facing text is unchanged.
            "glob" => {
                search::glob_tool(
                    input,
                    &ctx.cfg.effective_cwd(),
                    ctx.program_result.as_ref(),
                    ctx.cfg.effective_permissions(),
                )
                .await
            }
            "read_offloaded" => fs::read_offloaded_tool(input, ctx).await,
            "todo_write" => todo::todo_write_tool(input, ctx).await,
            "skill" => skill::skill_tool(input, ctx).await,
            "tool_search" => tool_search::tool_search_tool(input, ctx).await,
            // Only malformed envelopes reach this arm — well-formed ones were
            // rewritten to the inner call at dispatch entry.
            "call_tool" => Err(anyhow!(
                "call_tool: missing required string argument 'tool_name' (usage: {{\"tool_name\": \"<name>\", \"params\": {{...}}}})"
            )),
            "task" => task::task_tool(input, ctx).await,
            "exit_plan_mode" => plan_mode::exit_plan_mode_tool(input, ctx).await,
            "enter_worktree" => worktree_tool::enter_worktree_tool(input, ctx).await,
            "exit_worktree" => worktree_tool::exit_worktree_tool(input, ctx).await,
            "wait" => background_tasks::wait_tool(input, ctx).await,
            "stop_agent" => background_tasks::stop_agent_tool(input, ctx).await,
            "run_program" => codemode::run_program_tool(input, ctx).await,
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

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key]
        .as_str()
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
        ToolCtx {
            cfg: Arc::new(Config {
                provider: Arc::new(Provider::mock(vec![])),
                model: "mock".into(),
                system: "test".into(),
                project_instructions: None,
                max_rounds: Some(5),
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                offload_dir: std::env::temp_dir().join(format!("kloop-tools-{tag}")),
                sessions_dir: std::env::temp_dir().join(format!("kloop-tools-sessions-{tag}")),
                context_window: None,
                fallback_model: None,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                file_state: Default::default(),
                tool_sources: sources,
                session_id: String::new(),
                agent_label: String::new(),
                hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
                background_shells: BackgroundShells::new(),
                sandbox: None,
                agent_types: Arc::new(Vec::new()),
                tool_allowlist: None,
                defer_threshold: 30,
                unlocked_tools: Default::default(),
                todos: Default::default(),
                inbox: Default::default(),
                background_tasks: Default::default(),
                program_limits: Default::default(),
                skills: Default::default(),
                active_worktree: std::sync::Arc::new(std::sync::RwLock::new(None)),
                worktree_enabled: false,
            }),
            ui: Arc::new(SilentUi),
            cancel: CancellationToken::new(),
            depth,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            parent_rollout_id: None,
            program_result: None,
        }
    }

    /// Rebuild the ctx with a different defer threshold (Config is behind an
    /// Arc, so tests clone-and-swap instead of mutating).
    pub(crate) fn with_defer_threshold(mut ctx: ToolCtx, threshold: usize) -> ToolCtx {
        let mut cfg = (*ctx.cfg).clone();
        cfg.defer_threshold = threshold;
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    /// Rebuild the ctx with a scripted provider, for tests whose tools spawn
    /// sub-agents that sample.
    pub(crate) fn with_provider(mut ctx: ToolCtx, provider: Provider) -> ToolCtx {
        let mut cfg = (*ctx.cfg).clone();
        cfg.provider = Arc::new(provider);
        ctx.cfg = Arc::new(cfg);
        ctx
    }

    /// Rebuild the ctx with an OS sandbox policy for bash.
    #[allow(dead_code)] // used by the macOS-only sandbox integration tests
    pub(crate) fn with_sandbox(mut ctx: ToolCtx, policy: crate::sandbox::SandboxPolicy) -> ToolCtx {
        let mut cfg = (*ctx.cfg).clone();
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
            assert!(std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(a)
                .status()
                .unwrap()
                .success());
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
        let mut cfg = (*ctx.cfg).clone();
        cfg.cwd = repo.to_path_buf();
        cfg.worktree_enabled = worktree_enabled;
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
        fn defs(&self) -> &[ToolDef] {
            &self.defs
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

    #[test]
    fn tool_defs_expose_task_only_at_depth_zero() {
        let names = |depth| {
            tool_defs(depth)
                .into_iter()
                .map(|t| t.name)
                .collect::<Vec<_>>()
        };
        assert!(names(0).iter().any(|n| n == "task"));
        assert!(!names(1).iter().any(|n| n == "task"));
    }

    #[test]
    fn all_tool_defs_appends_sources_and_skips_collisions() {
        let sources: Vec<Arc<dyn ToolSource>> =
            vec![StubSource::new("srv"), StubSource::new("srv")];
        let names: Vec<String> = all_tool_defs(0, &sources, TOOL_DEFER_THRESHOLD, false)
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
                "kill_bash",
                "read_file",
                "write_file",
                "edit_file",
                "grep",
                "glob",
                "read_offloaded",
                "todo_write",
                "task",
                "wait",
                "stop_agent",
                "srv__echo",
                "srv__fail",
                "srv__image",
                "run_program",
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
        let defs = all_tool_defs(0, &builtin_clash, TOOL_DEFER_THRESHOLD, false);
        let bash: Vec<&ToolDef> = defs.iter().filter(|d| d.name == "bash").collect();
        assert_eq!(bash.len(), 1);
        assert_ne!(bash[0].description, "impostor");
    }

    #[test]
    fn tool_merge_warnings_flags_collisions_and_oversized_lists() {
        assert_eq!(
            tool_merge_warnings(&[], TOOL_DEFER_THRESHOLD),
            Vec::<String>::new()
        );

        let colliding: Vec<Arc<dyn ToolSource>> =
            vec![StubSource::new("srv"), StubSource::new("srv")];
        let warnings = tool_merge_warnings(&colliding, TOOL_DEFER_THRESHOLD);
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
        let warnings = tool_merge_warnings(&big, TOOL_DEFER_THRESHOLD);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("54 tools"), "got: {warnings:?}");
        assert!(warnings[0].contains("tool_search"), "got: {warnings:?}");
    }

    /// The defer regime flips on the threshold: at or under, source tools
    /// are inline exactly as before and tool_search does not exist; past it,
    /// the defs shrink to built-ins + tool_search and the source tools move
    /// to the deferred set.
    #[test]
    fn defer_kicks_in_past_threshold() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        let builtin_count = tool_defs(0).len();

        // Exactly at the threshold (built-ins + the stub's 3 tools): everything
        // inline, no tool_search.
        let inline = all_tool_defs(0, &sources, builtin_count + 3, false);
        assert!(inline.iter().any(|d| d.name == "srv__echo"));
        assert!(inline.iter().all(|d| d.name != "tool_search"));
        assert!(deferred_tool_defs(&sources, builtin_count + 3).is_empty());

        // One past it: built-ins + tool_search + call_tool only; sources
        // deferred. (+1 for the always-present depth-0 exit_plan_mode tool.)
        let deferred_regime = all_tool_defs(0, &sources, builtin_count + 2, false);
        let names: Vec<&str> = deferred_regime.iter().map(|d| d.name.as_str()).collect();
        assert!(names.contains(&"tool_search"));
        assert!(names.contains(&"call_tool"));
        assert!(names.contains(&"exit_plan_mode"));
        assert!(!names.contains(&"srv__echo"));
        assert_eq!(deferred_regime.len(), builtin_count + 3);
        let deferred: Vec<String> = deferred_tool_defs(&sources, builtin_count + 2)
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
        let defs = all_tool_defs(0, &sources, TOOL_DEFER_THRESHOLD, false);
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

    /// Slice 2: past the threshold source tools degrade to a compact name +
    /// description manifest in run_program's description — no full signatures —
    /// with guidance that they stay callable from a program.
    #[test]
    fn run_program_def_lists_deferred_source_tools_as_a_manifest() {
        let sources: Vec<Arc<dyn ToolSource>> = vec![StubSource::new("srv")];
        // One source (2 tools) past the built-in count forces the defer regime.
        let defs = all_tool_defs(0, &sources, tool_defs(0).len(), false);
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
        assert!(deferred_tool_defs(&clash, 0).is_empty());
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
        let mut cfg = (*base.cfg).clone();
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
            "kill_bash",
            &json!({"bash_id": "bg-1"})
        ));
        assert!(!is_concurrency_safe(
            "write_file",
            &json!({"path": "x", "content": ""})
        ));
        assert!(!is_concurrency_safe("edit_file", &json!({})));
        // Consecutive task calls run as parallel sub-agents (cc shape).
        assert!(is_concurrency_safe("task", &json!({"prompt": "x"})));

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

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolCtx {
            cfg: Arc::new(Config {
                provider: Arc::new(Provider::mock(vec![])),
                model: "mock".into(),
                system: "test".into(),
                project_instructions: None,
                max_rounds: Some(5),
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                offload_dir: std::env::temp_dir().join("kloop-test-cancel"),
                sessions_dir: std::env::temp_dir().join("kloop-test-cancel-sessions"),
                context_window: None,
                fallback_model: None,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                file_state: Default::default(),
                tool_sources: Vec::new(),
                session_id: String::new(),
                agent_label: String::new(),
                hooks: std::sync::Arc::new(crate::hooks::Hooks::none()),
                background_shells: BackgroundShells::new(),
                sandbox: None,
                agent_types: Arc::new(Vec::new()),
                tool_allowlist: None,
                defer_threshold: 30,
                unlocked_tools: Default::default(),
                todos: Default::default(),
                inbox: Default::default(),
                background_tasks: Default::default(),
                program_limits: Default::default(),
                skills: Default::default(),
                active_worktree: std::sync::Arc::new(std::sync::RwLock::new(None)),
                worktree_enabled: false,
            }),
            ui: Arc::new(NullUi),
            cancel,
            depth: 0,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
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
