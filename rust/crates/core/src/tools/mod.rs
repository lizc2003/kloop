//! The tool seam: definitions, per-input concurrency classification, and
//! the gated dispatch loop (hooks → permissions → execution). Individual
//! tool implementations live in the sibling modules; this file is what the
//! agent loop and the frontends depend on.

mod agent_message;
mod background_executions;
mod bash;
mod builtin;
mod codemode;
mod fs;
mod inject;
pub(crate) mod notebook;
#[cfg(test)]
mod plan151_acceptance_tests;
#[cfg(test)]
mod plan197_acceptance_tests;
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
mod todo;
mod tool_search;
pub mod web;
mod workflow;
mod worktree_tool;

pub use background_executions::BackgroundExecutions;
pub use background_executions::ExecutionStatus;
pub use bash::BackgroundShells;
pub(crate) use builtin::Builtin;
pub use builtin::builtin_tool_names;
pub(crate) use codemode::ProgramToolManifest;
pub(crate) use codemode::capture_program_tool_manifest;
pub use tool_search::DeferredToolUnlocks;
// Slash-path prompt injections (`!cmd` / `@file`) for `/name` commands; the
// dispatch layer (`crate::commands`) calls this before running the turn.
pub(crate) use inject::expand_slash_injections;
// Registered on the run_agent peer set only at depth 0 with skills loaded
// (see `turn_rounds`); the pure skill logic it drives lives in `crate::skills`.
pub(crate) use skill::skill_tool_def;
// Recorded at the round boundary by `agent.rs` (plan 197), not on a tool result.
pub(crate) use fs::changed_reads_reminder;
pub use tool_search::deferred_notice;
// The skills module (`crate::skills`) dispatches a `context: fork` skill here,
// reusing the run_agent sub-agent machinery.
pub(crate) use subagent::fork_skill;
#[cfg(test)]
pub(crate) use todo::REMINDER_STALE_ROUNDS;
pub use todo::TodoItem;
pub use todo::TodoRegistry;
pub use todo::TodoSnapshot;
pub use todo::TodoStatus;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use anyhow::bail;
use serde_json::Value;
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

/// Most calls that may be running at once inside one concurrent batch. The
/// batch is not split to enforce it: grouping stays "consecutive calls of the
/// same concurrency safety", with `tool_search` as the ordering barrier
/// ([`Builtin::concurrency_safe`]), and this only bounds how many of a group
/// run at the same moment.
///
/// One number for every tool, as in cc (`CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY`,
/// default 10) and deepseek-harness (`DEFAULT_MAX_PARALLEL_TOOL_CALLS`, also
/// 10). Splitting it by cost class — `read_file` against read-only `bash`
/// against `run_agent` — has no precedent in either: cc runs sub-agents under
/// this same one cap. The one reference that does split (grok) gives a private
/// budget to a handful of individually expensive tools (image/video
/// generation), which is a different mechanism for a tool kloop does not have.
pub const MAX_CONCURRENT_TOOL_CALLS: usize = 10;

/// How long a read-only built-in may run before its deadline asks it to stop.
///
/// Measured, and measured as never firing: across 742 real calls that had a
/// sampling round to themselves, `read_file` peaked at 0.03s, `glob` at 0.05s
/// and `grep` at 4.65s. (Calls that shared a round look far slower, but a batch
/// shares one result timestamp with the slowest call in it — a trap worth
/// naming here, because it reads as a 6 450-second grep.) So this is not a
/// budget real work runs into; it is a floor under a wedged network mount,
/// which a local corpus has nothing to say about. deepseek-harness declines to
/// give grep/glob a budget at all for exactly that reason. It is kept because
/// once the mechanism exists one more row costs nothing — but if it ever fires
/// on real work, the bug is here, not in the tool.
const READ_ONLY_TOOL_TIMEOUT: Duration = Duration::from_secs(60);

/// Default budget for a call into an external source — an MCP server, a web
/// provider — which a source may replace ([`ToolSource::call_timeout`]).
///
/// This is the hole the mechanism exists for. A built-in that hangs is a kloop
/// bug; an MCP server that stops answering is an ordinary Tuesday, and before
/// this it hung the session with no bound at all. The references bracket the
/// answer rather than supply it: cc's default MCP *tool* timeout is
/// `100_000_000` ms — 27.8 hours, effectively none, overridable through
/// `MCP_TOOL_TIMEOUT` — while deepseek-harness declares 30s, but only on its
/// two web tools, and passes every undeclared tool through untouched. Five
/// minutes sits between them: an order of magnitude above dsh's web budget, and
/// short enough that one wedged server costs a few minutes instead of a session.
const EXTERNAL_TOOL_TIMEOUT: Duration = Duration::from_secs(300);

/// Headroom between `bash`'s own `timeout_ms` and the outer deadline. The inner
/// bound must always win: it kills the process tree, which a cancellation
/// cannot, and it is the number the model asked for. Thirty seconds is enough
/// for process-tree teardown on a loaded machine.
const BASH_TIMEOUT_GRACE: Duration = Duration::from_secs(30);

/// How long a call that has been asked to cancel gets to actually stop before
/// its future is dropped. Capped at the budget itself, so a small budget is not
/// dwarfed by its own settle window.
const CANCEL_SETTLE_GRACE: Duration = Duration::from_secs(5);

/// The budget for one call, from whoever owns the tool: the built-in table, or
/// the source that advertised it. An unknown name gets none — it fails on the
/// next line anyway, and a deadline is not how that should be reported.
fn call_timeout(name: &str, input: &Value, ctx: &ToolCtx) -> Option<Duration> {
    match Builtin::from_name(name) {
        Some(builtin) => builtin.timeout(input),
        None => {
            find_source(&ctx.cfg.tool_sources, name).and_then(|source| source.call_timeout(name))
        }
    }
}

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
    /// How long one call into this source may run before the dispatcher cancels
    /// it and reports a timeout. `None` waits forever, which is what the seam
    /// did before there was a budget at all.
    ///
    /// Declared here rather than in a table keyed by tool name, following
    /// deepseek-harness: a name table is one typo away from silently not
    /// applying. A source that knows one of its tools is legitimately slow — a
    /// build, a long query — raises it for that tool by name.
    fn call_timeout(&self, _tool: &str) -> Option<Duration> {
        Some(EXTERNAL_TOOL_TIMEOUT)
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

/// Where a call that may have side effects reports "about to run" (plan 204).
/// The session file belongs to the turn's `History`, which `run_one` cannot
/// reach, so the report travels to the turn and the call waits until the line
/// is on disk: the rollout says "started" only before the effect can begin.
#[derive(Clone)]
pub(crate) struct ToolStartedSink(
    tokio::sync::mpsc::UnboundedSender<(String, tokio::sync::oneshot::Sender<()>)>,
);

/// The turn's end of a [`ToolStartedSink`].
pub(crate) type ToolStartedReceiver =
    tokio::sync::mpsc::UnboundedReceiver<(String, tokio::sync::oneshot::Sender<()>)>;

impl ToolStartedSink {
    pub(crate) fn channel() -> (Self, ToolStartedReceiver) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (Self(tx), rx)
    }

    /// Returns once the turn has written the line, or at once if the turn is
    /// no longer listening — a note nobody can write must not block the call.
    async fn note(&self, tool_use_id: &str) {
        let (ack, written) = tokio::sync::oneshot::channel();
        if self.0.send((tool_use_id.to_string(), ack)).is_ok() {
            let _ = written.await;
        }
    }
}

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
    /// Set only by a turn whose calls answer to a session file. None for
    /// harness commands, and for calls a program fires: the program's own
    /// call is the one the session's pairing repair has to judge.
    pub(crate) tool_started: Option<ToolStartedSink>,
}

impl ToolCtx {
    /// A top-level context for a command the harness runs on its own — a
    /// `/name` `!cmd` injection, a scheduled `check` — rather than one the
    /// model called. It streams nothing: the permission prompt, which rides the
    /// approver inside `cfg.permissions`, is its only surface.
    pub(crate) fn harness(cfg: Arc<Config>, cancel: CancellationToken) -> Self {
        Self {
            cfg,
            ui: Arc::new(SilentHarnessUi),
            cancel,
            depth: 0,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
            tool_started: None,
        }
    }
}

struct SilentHarnessUi;
impl Ui for SilentHarnessUi {
    fn emit(&self, _ev: &crate::event::Event) {}
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
    let (inline_sources, deferred) =
        partition_source_defs(sources, defer_threshold, shell_programs);
    if !deferred.is_empty() {
        defs.push(tool_search::tool_search_def());
        defs.push(tool_search::call_tool_def());
    }
    defs.extend(inline_sources.iter().cloned());
    // The surface-gated block is depth-0 only (like run_agent) and rides on
    // what the front-end enables. It is appended after the source defs because
    // run_program's generated TypeScript API lists them: full declarations for
    // inline source tools (typed `Promise<CallToolResult>`), a compact manifest
    // for deferred ones — both callable at runtime.
    //
    // Walking `builtin::ALL` rather than pushing group by group is what keeps
    // this honest: a new surface-gated tool is offered because it declared a
    // gate, not because someone remembered to add a `push` here. The walk order
    // is `ALL`'s order, which is the wire order — see [`builtin::ALL`].
    if depth == 0 {
        // Only run_program's definition reads the catalogs, and that surface is
        // off by default — rebuilding three dozen schemas for a request that
        // does not offer it would be pure waste.
        let program_builtins = if surface.program {
            builtin_defs(0, shell_programs)
        } else {
            Vec::new()
        };
        let cx = builtin::DefCx {
            builtins: &program_builtins,
            inline_sources: &inline_sources,
            deferred_sources: &deferred,
        };
        defs.extend(
            builtin::ALL
                .iter()
                .filter(|tool| tool.in_surface(surface))
                .map(|tool| tool.def(&cx)),
        );
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
    partition_source_defs(sources, defer_threshold, shell_programs).1
}

/// The merged source catalog split into `(inline, deferred)`, in merge order.
/// One walk: `all_tool_defs` needs both halves, and computing them as two
/// independent calls merged the whole catalog twice and then subtracted one
/// result from the other.
fn partition_source_defs(
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    shell_programs: &ShellPrograms,
) -> (Vec<ToolDef>, Vec<ToolDef>) {
    let merged = merged_source_defs(sources);
    if tool_defs(0, shell_programs).len() + merged.len() > defer_threshold {
        return (Vec::new(), merged);
    }
    merged.into_iter().partition(|def| {
        !find_source(sources, &def.name).is_some_and(|source| source.should_defer(&def.name))
    })
}

/// Every name an external source may not claim: every built-in ([`builtin::ALL`],
/// so it cannot drift from what is actually offered — including the
/// surface-gated ones a host may not enable and the shells a host may not
/// have) plus the structured-output protocol tool.
///
/// Retired names are **not** kept here. A name kloop no longer offers is not
/// kloop's to reserve: the list only ever grew, and what it bought was a
/// slightly better error for a call the model has no reason to make, since it
/// picks from the tool array it was just sent. An unknown name already fails
/// closed with `unknown tool: <name>`.
///
/// Derived once. The derivation is what keeps the list honest, but paying for
/// it per lookup meant rebuilding a dozen `json!` schemas just to read their
/// `name` — and `resolve_source` runs on every tool call.
fn reserved_names() -> &'static std::collections::HashSet<String> {
    static NAMES: std::sync::LazyLock<std::collections::HashSet<String>> =
        std::sync::LazyLock::new(|| {
            builtin::ALL
                .iter()
                .map(|tool| tool.name().to_string())
                .chain(
                    [
                        // An internal completion protocol, appended past every
                        // catalog and filter — never a configurable tool.
                        "structured_output",
                    ]
                    .into_iter()
                    .map(String::from),
                )
                .collect()
        });
    &NAMES
}

fn merged_source_defs(sources: &[Arc<dyn ToolSource>]) -> Vec<ToolDef> {
    let mut seen = reserved_names().clone();
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
    let mut seen = reserved_names().clone();
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
    if reserved_names().contains(name) {
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
    if reserved_names().contains(name) {
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

/// The built-in tool defs (bash, file, search, and — at depth 0 — todos plus
/// `run_agent`): every [`builtin::Builtin`] whose gate puts it in the catalog.
/// This is the set `run_program` derives its TypeScript API from, so it
/// excludes `run_program` itself — structurally, because that tool's gate is a
/// surface capability, not by a hand-kept omission that could rot: no
/// self-reference, and no throwaway description regeneration when only counting
/// is needed.
fn builtin_defs(depth: u8, shell_programs: &ShellPrograms) -> Vec<ToolDef> {
    let cx = builtin::DefCx::default();
    builtin::ALL
        .iter()
        .filter(|tool| tool.in_catalog(depth, shell_programs))
        .map(|tool| tool.def(&cx))
        .collect()
}

/// What tool-counting (`defer_active`, `tool_merge_warnings`) sees: the built-ins
/// only. Surface-gated depth-0 tools are excluded because they may not be sent at
/// all, and `run_program` now joins them
/// ([`crate::config::SurfaceCapabilities::program`]), so it no longer counts
/// toward the defer threshold — the treatment `workflow`, `worktree` and the rest
/// have always had. What is actually sent, whose TypeScript API also lists the
/// external source tools, is built in [`all_tool_defs`], which sees both the
/// sources and the surface.
pub fn tool_defs(depth: u8, shell_programs: &ShellPrograms) -> Vec<ToolDef> {
    builtin_defs(depth, shell_programs)
}

/// Concurrency safety by name AND input. Built-ins answer for themselves
/// ([`Builtin::concurrency_safe`]); external tools are safe only when their
/// source marks them read-only. Built-in names shadow sources here exactly as
/// they do in dispatch — a source cannot claim a reserved name in the first
/// place.
pub fn is_concurrency_safe(name: &str, input: &Value, sources: &[Arc<dyn ToolSource>]) -> bool {
    match Builtin::from_name(name) {
        Some(builtin) => builtin.concurrency_safe(input),
        None => find_source(sources, name).is_some_and(|source| source.is_readonly(name)),
    }
}

/// Whether a call may change something outside the conversation, so that a
/// crash while it ran leaves the world in an unknown state (plan 204).
/// Concurrency safety is almost that question, except for the orchestrators:
/// they are batched as safe because their children re-enter the gate, but
/// those children write and run things all the same.
fn may_have_effects(name: &str, input: &Value, sources: &[Arc<dyn ToolSource>]) -> bool {
    !is_concurrency_safe(name, input, sources)
        || matches!(
            Builtin::from_name(name),
            Some(Builtin::RunAgent | Builtin::Workflow | Builtin::RunProgram)
        )
}

/// The argument names other harnesses use for the same thing, and the one name
/// kloop implements.
///
/// Every mainstream harness spells the same file arguments differently, and a
/// model carries whichever spelling its training saw most: `file_path` for `path`
/// (claude-code, grok-build), and `old_str`/`new_str`/`file_text` for
/// `old_string`/`new_string`/`content` (the Anthropic text-editor lineage that
/// deepseek-harness follows — its path argument is already `path`). The fourth
/// case is the one kloop inflicts on itself: three file tools say `path` and
/// `notebook_edit` says `notebook_path`.
///
/// Translating a synonym is strictly better than refusing it — the edit the model
/// asked for is the edit that happens — and it advertises nothing new, because the
/// schema still carries exactly one name per argument. Only builtins with a fixed
/// schema appear here; an external tool's argument names are its own.
const ARGUMENT_SYNONYMS: &[(&str, &str, &[&str])] = &[
    ("read_file", "path", &["file_path"]),
    ("write_file", "path", &["file_path"]),
    ("write_file", "content", &["file_text"]),
    ("edit_file", "path", &["file_path"]),
    ("edit_file", "old_string", &["old_str"]),
    ("edit_file", "new_string", &["new_str"]),
    ("notebook_edit", "notebook_path", &["path", "file_path"]),
];

/// Rename a call's known synonyms onto the names the tool implements.
///
/// Only ever fills a gap, and never guesses:
/// - a canonical key that is present is left alone whatever its type, so a call
///   that sent the real name is never second-guessed by a stray synonym;
/// - synonyms that disagree are **not** resolved. The call passes through
///   untouched and the tool's own refusal explains it, naming both the argument it
///   wanted and the keys the call did carry — which is what that refusal is for.
///
/// Every consumed synonym is removed, so the object the gate, the preview and the
/// executor see matches the schema exactly, and an allow-list tool is not tripped
/// by the spelling it was just forgiven.
fn rename_argument_synonyms(tool: &str, input: &mut Value) {
    let Some(object) = input.as_object_mut() else {
        return;
    };
    for (owner, canonical, synonyms) in ARGUMENT_SYNONYMS {
        if *owner != tool || object.contains_key(*canonical) {
            continue;
        }
        let present: Vec<&str> = synonyms
            .iter()
            .copied()
            .filter(|synonym| object.contains_key(*synonym))
            .collect();
        let (Some(first), true) = (
            present.first(),
            present
                .windows(2)
                .all(|pair| object[pair[0]] == object[pair[1]]),
        ) else {
            continue;
        };
        let value = object[*first].clone();
        for synonym in &present {
            object.remove(*synonym);
        }
        object.insert((*canonical).to_string(), value);
    }
}

/// Normalize compatibility envelopes before any caller classifies a tool use.
/// Structured turns share this with the ordinary dispatcher so a deferred-mode
/// `call_tool` wrapper around the synthetic terminal tool is still intercepted.
///
/// Argument synonyms are renamed here for the same reason the wrapper is unwrapped
/// here: concurrency batching, hooks, the permission gate, the approval preview
/// and the executor must all read one shape. A path that arrived as `file_path`
/// has to be the path the gate matches its rules against — normalizing after the
/// gate would hand it a call with no path at all.
pub(crate) fn normalize_tool_uses(
    tool_uses: Vec<(String, String, Value)>,
) -> Vec<(String, String, Value)> {
    tool_uses
        .into_iter()
        .map(|(id, name, input)| {
            let (name, mut input) = tool_search::unwrap_call_tool(name, input);
            rename_argument_synonyms(&name, &mut input);
            (id, name, input)
        })
        .collect()
}

/// Execute one round of tool calls. Consecutive concurrency-safe calls run as
/// one concurrent batch, at most [`MAX_CONCURRENT_TOOL_CALLS`] of them at a
/// time; everything else runs sequentially. Every tool_use always gets a paired
/// tool_result: cancellation patches the remaining calls with is_error
/// "interrupted" results so history stays legal.
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
            // One permit per call in flight. join_all still yields results in
            // request order, and a finished call drops its permit at once, so
            // the cap counts calls that are actually running rather than slots
            // held by results waiting to be collected. A call that waits for a
            // permit through a cancellation never reaches its executor:
            // `run_one` observes the cancelled token first and settles as
            // interrupted.
            let limiter = tokio::sync::Semaphore::new(MAX_CONCURRENT_TOOL_CALLS);
            let futs = batch.iter().map(|(id, name, input)| {
                let call = run_one(
                    id.clone(),
                    name.clone(),
                    input.clone(),
                    ctx.clone(),
                    expected_program_source(name),
                );
                let limiter = &limiter;
                async move {
                    let _permit = limiter
                        .acquire()
                        .await
                        .expect("the batch limiter outlives its batch and is never closed");
                    call.await
                }
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

/// The in-process cancellation result: the call knows how far it got. A killed
/// session's orphans get their own two texts from rollout repair instead.
pub(crate) fn interrupted(tool_use_id: &str) -> ContentBlock {
    ContentBlock::ToolResult {
        tool_use_id: tool_use_id.into(),
        content: "interrupted".into(),
        is_error: true,
    }
}

/// A call to a tool that cannot run at all — a retired name, a control this
/// depth is not given, a capability this front-end does not offer, a shell this
/// host has none of. Rejected before hooks, the permission gate or the registry
/// handler can observe it: none of them has anything to decide about a call
/// that was never legal.
fn reject_unavailable(name: &str, ctx: &ToolCtx) -> Result<()> {
    // The same gate table the request's tool array was built from: root-owned
    // controls a sub-agent is not sent, front-end capabilities this session did
    // not enable, a shell this host resolved no interpreter for. The catalog
    // hides all three, but hiding is not refusing — stale context from before a
    // compaction, a resumed rollout and a forged call all reach dispatch
    // without passing a builder, and must fail before allowlists, hooks,
    // permissions or the registry handler can observe them.
    if let Some(reason) = Builtin::from_name(name)
        .and_then(|tool| tool.unoffered(ctx.depth, ctx.cfg.surface, &ctx.cfg.shell_programs))
    {
        bail!(reason.message(name));
    }
    // A custom agent type's tool allowlist is a capability gate: the tool
    // is filtered out of this sub-agent's defs, so a call to it is a
    // hallucination — reject before hooks or the human are consulted.
    // (The main agent has no allowlist, so this never fires for it.)
    if !crate::agent_type::tool_available(ctx.cfg.tool_allowlist.as_deref(), name) {
        bail!("tool '{name}' is not available to this agent type");
    }
    Ok(())
}

/// Which source this call is bound to, frozen before hooks run. Every later
/// re-check compares against this: a source that changes underneath a call is a
/// discovery error, never a silently re-routed execution.
struct SourceGate {
    /// The tool came through the deferred-capability gate, so it also has to
    /// still be unlocked after the hook.
    discovery_gated: bool,
    /// The binding the call must execute against.
    expected: Option<SourceCallBinding>,
    /// The binding as it read before the hook ran.
    before: Option<SourceCallBinding>,
}

/// Resolve the call's source and its deferred-capability standing.
fn classify_source(
    name: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
    expected_program_source: Option<SourceCallBinding>,
) -> Result<SourceGate> {
    let before = match tool_search::current_source_route(name, &ctx.cfg) {
        SourceRouteState::Missing => None,
        SourceRouteState::Available(binding) => Some(binding),
        SourceRouteState::Unavailable(reason) => bail!(reason.clone()),
    };
    if ctx.from_program {
        if before != expected_program_source {
            bail!(
                "tool '{name}' source changed after this Program API was generated; run the Program again from a fresh sampling round"
            );
        }
        return Ok(SourceGate {
            discovery_gated: false,
            expected: expected_program_source,
            before,
        });
    }
    // Two samples, deliberately. A dynamic source publishes a new
    // catalog between them, so a tool can be absent on the first look
    // and deferred on the second — and a tool that appeared at any point
    // during classification has never been through discovery. Either
    // observation therefore gates it: `||`, not the single read the
    // names invite. `catalog_appearance_during_classification_still_requires_discovery`
    // fails if this collapses into one call.
    let deferred_before = tool_search::is_deferred(name, &ctx.cfg);
    let deferred_after = tool_search::is_deferred(name, &ctx.cfg);
    let discovery_gated = deferred_before || deferred_after;
    if !discovery_gated {
        return Ok(SourceGate {
            discovery_gated,
            expected: before,
            before,
        });
    }
    let source = match tool_search::unlocked_source_for_dispatch(name, ctx, workspace) {
        Some(source) => source,
        None => match tool_search::current_source_route(name, &ctx.cfg) {
            SourceRouteState::Unavailable(reason) => bail!(reason),
            SourceRouteState::Missing | SourceRouteState::Available(_) => {
                bail!(
                    "tool '{name}' is deferred and not loaded yet; call tool_search with query \"select:{name}\" to load its definition, then retry"
                )
            }
        },
    };
    if Some(source) != before {
        bail!(
            "tool '{name}' source changed while its deferred capability was classified; call tool_search with query \"select:{name}\" to load its definition again, then retry"
        );
    }
    Ok(SourceGate {
        discovery_gated,
        expected: Some(source),
        before,
    })
}

/// Let the owning source inspect the call before it runs. Asked twice — once on
/// the frozen binding, once after the pre-tool hook — because a hook may have
/// changed what the source would say.
fn preflight_source(gate: &SourceGate, name: &str, input: &Value, ctx: &ToolCtx) -> Result<()> {
    let Some(binding) = gate.expected else {
        return Ok(());
    };
    let source = ctx
        .cfg
        .tool_sources
        .get(binding.source_slot)
        .context("source preflight binding is no longer registered")?;
    source.preflight(name, input)
}

/// pre_tool hooks run BEFORE the permission gate: hooks are automation policy,
/// permissions are the human's last word — a hook block means there is nothing
/// left to ask about.
async fn run_pre_tool_hook(name: &str, input: &Value, ctx: &ToolCtx) -> Result<()> {
    match ctx
        .cfg
        .hooks
        .pre_tool(
            &ctx.cfg.session_id,
            ctx.cfg.agent_label(),
            name,
            input,
            ctx.ui.as_ref(),
        )
        .await
    {
        crate::hooks::HookDecision::Block { reason } => bail!("blocked by hook: {reason}"),
        crate::hooks::HookDecision::Allow { context } => {
            ctx.hook_context.lock().unwrap().extend(context);
            Ok(())
        }
    }
}

/// A hook runs arbitrary code, and a source may republish while it does. The
/// call executes against the binding it was classified on or not at all.
fn recheck_source_after_hook(
    gate: &SourceGate,
    name: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<()> {
    let after = match tool_search::current_source_route(name, &ctx.cfg) {
        SourceRouteState::Missing => None,
        SourceRouteState::Available(binding) => Some(binding),
        SourceRouteState::Unavailable(reason) => bail!(reason),
    };
    if after != gate.before {
        let guidance = if gate.discovery_gated {
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
    if gate.discovery_gated
        && tool_search::unlocked_source_for_dispatch(name, ctx, workspace) != gate.expected
    {
        bail!(
            "tool '{name}' capability changed while its pre-tool hook ran; call tool_search with query \"select:{name}\" to load its definition again, then retry"
        );
    }
    Ok(())
}

/// What `prepare` resolved for this call. Prepared after pre-hooks but before
/// permission, so the gate sees the canonical effective target while the
/// executor retains an open parent directory handle across any approval wait.
struct PreparedCall {
    mutation: Option<fs::PreparedMutation>,
    read: Option<fs::PreparedRead>,
}

impl PreparedCall {
    async fn resolve(name: &str, input: &Value, workspace: &EffectiveWorkspace) -> Result<Self> {
        if name == "notebook_edit" {
            notebook::request_from_input(input)?;
        }
        let mutation = if matches!(name, "write_file" | "edit_file" | "notebook_edit") {
            Some(if name == "notebook_edit" {
                fs::prepare_notebook_mutation_input(input, workspace).await?
            } else {
                fs::prepare_mutation_input(name, input, workspace).await?
            })
        } else {
            None
        };
        let read = if name == "read_file" {
            Some(fs::prepare_read(input, workspace).await?)
        } else {
            None
        };
        Ok(Self { mutation, read })
    }

    /// The human's last word on this call, against the resolved target.
    async fn authorize(
        &self,
        name: &str,
        input: &Value,
        ctx: &ToolCtx,
        workspace: &EffectiveWorkspace,
    ) -> Result<()> {
        let preview_context = self
            .mutation
            .as_ref()
            .and_then(fs::PreparedMutation::preview_context);
        let sandbox_auto_allow = bash::sandbox_auto_allowed(name, input, workspace);
        let permission = workspace
            .permissions
            .check_call_with_resolved_path(
                name,
                input,
                self.mutation
                    .as_ref()
                    .map(fs::PreparedMutation::resolved_path)
                    .or(self.read.as_ref().map(fs::PreparedRead::resolved_path)),
                preview_context.as_ref(),
                ctx.depth,
                sandbox_auto_allow,
            )
            .await;
        match permission {
            Ok(Some(notice)) => ctx.ui.emit(&Event::Note(notice.message)),
            Ok(None) => {}
            Err(reason) => bail!(reason),
        }
        Ok(())
    }
}

/// post_tool hooks (and other text-only surfaces) see the flattened text; an
/// image result renders as an `[image: <media_type>]` tag.
async fn run_post_tool_hook(name: &str, input: &Value, execution: &ToolExecution, ctx: &ToolCtx) {
    let (text, is_error) = match &execution.result {
        Ok(content) => (content.as_text().into_owned(), false),
        Err(e) => (format!("{e:#}"), true),
    };
    let context = ctx
        .cfg
        .hooks
        .post_tool(
            &ctx.cfg.session_id,
            ctx.cfg.agent_label(),
            name,
            input,
            &text,
            is_error,
            ctx.ui.as_ref(),
        )
        .await;
    ctx.hook_context.lock().unwrap().extend(context);
}

/// Everything a cancelled turn is allowed to drop: the checks, the hooks, the
/// permission wait and the execution itself. Cancellation kills this future
/// outright unless one of the two flags says an irreversible commit is already
/// under way, in which case [`run_one`] lets it finish.
#[allow(clippy::too_many_arguments)]
async fn run_gated(
    id: &str,
    name: &str,
    input: &Value,
    ctx: &ToolCtx,
    expected_program_source: Option<SourceCallBinding>,
    foreground_shell_started: &AtomicBool,
    local_send_committed: &AtomicBool,
    deadline: &CallDeadline,
) -> Result<ToolExecution> {
    reject_unavailable(name, ctx)?;
    // Freeze the workspace before validating a deferred capability. A stale
    // call is a discovery error, so neither hooks nor the human permission
    // gate should observe it. The same workspace snapshot is then used for
    // prepare, permission, sandbox and execution.
    let workspace = ctx.cfg.effective_workspace();
    let gate = classify_source(name, ctx, &workspace, expected_program_source)?;
    preflight_source(&gate, name, input, ctx)?;
    run_pre_tool_hook(name, input, ctx).await?;
    recheck_source_after_hook(&gate, name, ctx, &workspace)?;
    preflight_source(&gate, name, input, ctx)?;
    let prepared = PreparedCall::resolve(name, input, &workspace).await?;
    prepared.authorize(name, input, ctx, &workspace).await?;
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
    // Every gate is behind us and nothing below can refuse the call, so this is
    // the one point where "started" is true and not yet stale.
    if let Some(sink) = &ctx.tool_started
        && may_have_effects(name, input, &ctx.cfg.tool_sources)
    {
        sink.note(id).await;
    }
    let foreground_shell =
        name == "powershell" || (name == "bash" && !input["background"].as_bool().unwrap_or(false));
    if foreground_shell {
        foreground_shell_started.store(true, Ordering::Release);
    }
    let executed = deadline
        .arm(
            &ctx.cancel,
            execute_tool(
                name,
                input,
                PreparedExecution {
                    read: prepared.read.as_ref(),
                    mutation: prepared.mutation.as_ref(),
                    source: gate.expected,
                    local_send_committed,
                },
                ctx,
                &workspace,
            ),
        )
        .await;
    // Dropped without stopping. There is no verdict to report and no aftermath
    // to release; `report_timeout` replaces this text with the honest one.
    let execution = executed
        .unwrap_or_else(|| ToolExecution::from_result(Err(anyhow!("{name}: did not stop"))));
    if foreground_shell {
        foreground_shell_started.store(false, Ordering::Release);
    }
    #[cfg(all(test, windows))]
    drop(powershell_executor_probe);
    drop(powershell_guard);
    run_post_tool_hook(name, input, &execution, ctx).await;
    Ok(execution)
}

/// One call's time budget, and whether it ran out.
///
/// Armed around the executor alone, not around the whole gated run. The budget
/// is for the work: pre-tool hooks, and above all a human deciding whether to
/// approve the call, are not the tool hanging. A read a reviewer took two
/// minutes over must not come back to the model as a timeout — and in this
/// repo's own logs the slowest calls by far are exactly that wait (`skill` at
/// 3 304s, `write_file` at 211s), so this is not a hypothetical ordering.
struct CallDeadline {
    budget: Option<Duration>,
    expired: AtomicBool,
}

impl CallDeadline {
    fn new(name: &str, input: &Value, ctx: &ToolCtx) -> Self {
        Self {
            budget: call_timeout(name, input, ctx),
            expired: AtomicBool::new(false),
        }
    }

    /// Run `work` under the budget, in two stages, and answer `None` if it never
    /// stopped.
    ///
    /// **Cancel first, and wait.** A tool that watches its token unwinds and
    /// hands back a real verdict — its process killed, its temp file removed,
    /// its lock released. Tearing the future up at the instant the budget
    /// expires would strand exactly those.
    ///
    /// **Then drop.** Waiting forever for a tool that is not listening is the
    /// hang this whole mechanism exists to end, and for the bucket that
    /// motivated it there is nothing else available: a `ToolSource` call gets no
    /// token at this seam, so dropping its future IS its cancellation — Rust
    /// unwinds the in-flight request through `Drop`, which the TypeScript
    /// harness this design comes from cannot do at any price. That is why the
    /// order matters rather than the choice: whoever can stop cleanly does, and
    /// only whoever cannot gets dropped.
    async fn arm<T>(&self, cancel: &CancellationToken, work: impl Future<Output = T>) -> Option<T> {
        let Some(budget) = self.budget else {
            return Some(work.await);
        };
        let mut work = std::pin::pin!(work);
        tokio::select! {
            done = &mut work => return Some(done),
            _ = tokio::time::sleep(budget) => {}
        }
        self.expired.store(true, Ordering::Release);
        cancel.cancel();
        // A call given 50ms to work gets 50ms to stop; one given five minutes
        // gets the full window. Scaling the smaller case is what keeps a test
        // that arms a millisecond budget from waiting seconds to observe it.
        tokio::time::timeout(budget.min(CANCEL_SETTLE_GRACE), work)
            .await
            .ok()
    }

    fn expired(&self) -> bool {
        self.expired.load(Ordering::Acquire)
    }
}

/// What a finished call leaves for its caller to release once the final
/// `tool_result` exists: the write authority it earned and the path lock it
/// held. A cancelled or failed call leaves neither.
#[derive(Default)]
struct Aftermath {
    file_state_update: Option<(
        Arc<crate::file_state::FileState>,
        crate::file_state::FileStateUpdate,
    )>,
    path_lock: Option<tokio::sync::OwnedMutexGuard<()>>,
}

/// The gated run's verdict as the turn's `tool_result`. `None` is a
/// cancellation that nothing irreversible had started.
fn settle_execution(id: &str, gated: Option<Result<ToolExecution>>) -> (ContentBlock, Aftermath) {
    let error_result = |e: anyhow::Error| ContentBlock::ToolResult {
        tool_use_id: id.into(),
        content: format!("{e:#}").into(),
        is_error: true,
    };
    match gated {
        None => (interrupted(id), Aftermath::default()),
        Some(Ok(execution)) => {
            let ToolExecution {
                result,
                file_state_update,
                path_lock,
            } = execution;
            let result = match result {
                Ok(content) => ContentBlock::ToolResult {
                    tool_use_id: id.into(),
                    content,
                    is_error: false,
                },
                Err(e) => error_result(e),
            };
            (
                result,
                Aftermath {
                    file_state_update,
                    path_lock,
                },
            )
        }
        Some(Err(e)) => (error_result(e), Aftermath::default()),
    }
}

/// Append a model-visible notice to a finished tool result. Appending to the
/// result rather than recording a history entry of its own is what keeps it
/// ordered against the round's other results and replayable from a rollout.
fn append_notice(result: &mut ContentBlock, notice: &str) {
    let ContentBlock::ToolResult { content, .. } = result else {
        return;
    };
    match content {
        ToolResultContent::Text(text) => {
            text.push('\n');
            text.push_str(notice);
        }
        ToolResultContent::Blocks(blocks) => blocks.push(ContentBlock::Text {
            text: notice.to_string(),
        }),
    }
}

/// Replace a timed-out call's verdict with one that says so.
///
/// Only when the call settled as a failure. A tool that honoured the
/// cancellation and still finished its work did the work, and handing the model
/// a timeout instead of the answer it has would be a lie that costs a round.
///
/// The wording is the honest part. This budget cancels; it does not kill. A
/// tool that ignores its token is running still, and the model is told that
/// rather than left to assume the process is gone — deepseek-harness documents
/// the same limitation for the same mechanism.
fn report_timeout(result: &mut ContentBlock, name: &str, deadline: Duration) {
    let ContentBlock::ToolResult {
        content, is_error, ..
    } = result
    else {
        return;
    };
    if !*is_error {
        return;
    }
    // `{:?}` on a Duration prints "60s" / "40ms" rather than rounding a
    // sub-second budget to the "0s" that `as_secs` would.
    *content = ToolResultContent::Text(format!(
        "{name}: timed out after {deadline:?}. The call was asked to cancel and this \
         result is what it settled to. Nothing killed it: a tool that does not honour \
         cancellation may still be running in the background, and its side effects \
         may still land."
    ));
}

async fn run_one(
    id: String,
    name: String,
    input: Value,
    ctx: ToolCtx,
    expected_program_source: Option<SourceCallBinding>,
) -> ContentBlock {
    // This call's own cancellation scope. Cancelling the parent still reaches
    // it, so an interrupted turn behaves exactly as before; what the child buys
    // is the other direction — the deadline can stop THIS call without
    // cancelling the rest of the batch, and the two are told apart by which
    // token fired rather than by a timer race.
    let call_cancel = ctx.cancel.child_token();
    let ctx = ToolCtx {
        cancel: call_cancel.clone(),
        ..ctx
    };
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
    let deadline = CallDeadline::new(&name, &input, &ctx);
    let mut gated = Box::pin(run_gated(
        &id,
        &name,
        &input,
        &ctx,
        expected_program_source,
        &foreground_shell_started,
        &local_send_committed,
        &deadline,
    ));
    let gated_result = tokio::select! {
        _ = ctx.cancel.cancelled() => {
            // `deadline.expired` is what keeps our own cancellation from being
            // read as the turn's. Without it this branch and the finished
            // `gated` are both ready at every timeout, and whichever select
            // picks decides whether the model hears "interrupted" or "timed
            // out" — the same scoping deepseek-harness needs so a nested outer
            // deadline reads as ordinary upstream cancellation.
            if deadline.expired()
                || foreground_shell_started.load(Ordering::Acquire)
                || local_send_committed.load(Ordering::Acquire)
            {
                Some(gated.await)
            } else {
                None
            }
        }
        result = &mut gated => Some(result),
    };
    let (mut result, aftermath) = settle_execution(&id, gated_result);
    if deadline.expired() {
        report_timeout(
            &mut result,
            &name,
            deadline.budget.expect("a budget expired"),
        );
    }
    // The model has a successful Read/Write/Edit only once the final tool_result
    // exists. Executor-local reads and work canceled while a post-hook runs do
    // not create write authority. Mutation executors clear authority before
    // touching disk, so an interrupted commit remains conservative.
    //
    // The reread advisory hangs off the same condition for the same reason: a
    // read whose result the model never sees must not count as one it has.
    if let Some((state, update)) = aftermath.file_state_update {
        if let Some(advisory) = fs::reread_advisory(&state, &update) {
            append_notice(&mut result, &advisory);
        }
        state.apply(update);
    }
    drop(aftermath.path_lock);
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
        let Some(builtin) = Builtin::from_name(name) else {
            // Not a built-in, so it is an external source tool — which may also
            // return images, hence the same non-text path the file reads take.
            // A source can never claim a built-in name (`reserved_names`), so
            // this branch and the match below cannot both be right for one call.
            let Some(binding) = prepared.source else {
                return ToolExecution::from_result(Err(anyhow!("unknown tool: {name}")));
            };
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
        };
        // Exhaustive on purpose: a new built-in that reaches dispatch without an
        // arm here is a compile error, not an `unknown tool` at runtime.
        let text: Result<String> = match builtin {
            // read_file is the sole BUILT-IN that can return non-text: on an
            // image file it returns an image block (ToolResultContent::Blocks).
            Builtin::ReadFile => {
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
            // The file mutations carry a state update and a path lock back out,
            // so they also return before the text path.
            Builtin::WriteFile | Builtin::EditFile | Builtin::NotebookEdit => {
                let Some(prepared_mutation) = prepared.mutation else {
                    return ToolExecution::from_result(Err(anyhow!(
                        "{name}: mutation target was not prepared"
                    )));
                };
                let state = Arc::clone(&workspace.file_state);
                let output = match builtin {
                    Builtin::WriteFile => {
                        fs::write_file_tool(input, prepared_mutation, ctx, workspace).await
                    }
                    Builtin::EditFile => {
                        fs::edit_file_tool(input, prepared_mutation, ctx, workspace).await
                    }
                    _ => fs::notebook_edit_tool(input, prepared_mutation, ctx, workspace).await,
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
            Builtin::Bash => bash::bash_tool(input, ctx, workspace).await,
            Builtin::PowerShell => powershell::powershell_tool(input, ctx, workspace).await,
            Builtin::BashOutput => bash::bash_output_tool(input, ctx).await,
            Builtin::StopBash => bash::stop_bash_tool(input, ctx).await,
            Builtin::Grep => {
                search::grep_tool(input, &workspace.cwd, Arc::clone(&workspace.permissions)).await
            }
            // glob hands a program its path list as a string[] (built-ins are
            // otherwise strings); the model-facing text is unchanged.
            Builtin::Glob => {
                search::glob_tool(
                    input,
                    &workspace.cwd,
                    ctx.program_result.as_ref(),
                    Arc::clone(&workspace.permissions),
                )
                .await
            }
            Builtin::TodoWrite => todo::todo_write_tool(input, ctx),
            Builtin::Skill => skill::skill_tool(input, ctx, workspace).await,
            Builtin::ToolSearch => tool_search::tool_search_tool(input, ctx, workspace).await,
            // Only malformed envelopes reach this arm — well-formed ones were
            // rewritten to the inner call at dispatch entry.
            Builtin::CallTool => Err(anyhow!(
                "call_tool: missing required string argument 'tool_name' (usage: {{\"tool_name\": \"<name>\", \"params\": {{...}}}})"
            )),
            Builtin::RunAgent => subagent::run_agent_tool(input, ctx, workspace).await,
            Builtin::SendMessage => {
                let result = agent_message::send_message_tool(input, ctx);
                if result.is_ok() {
                    prepared.local_send_committed.store(true, Ordering::Release);
                }
                result
            }
            Builtin::ListAgents => agent_message::list_agents_tool(input, ctx),
            Builtin::AskUserQuestion => question::ask_user_question_tool(input, ctx).await,
            Builtin::EnterPlanMode => plan_mode::enter_plan_mode_tool(input, ctx, workspace).await,
            Builtin::ExitPlanMode => plan_mode::exit_plan_mode_tool(input, ctx, workspace).await,
            Builtin::EnterWorktree => worktree_tool::enter_worktree_tool(input, ctx).await,
            Builtin::ExitWorktree => worktree_tool::exit_worktree_tool(input, ctx).await,
            Builtin::WaitForActivity => {
                background_executions::wait_for_activity_tool(input, ctx).await
            }
            Builtin::StopAgent => background_executions::stop_agent_tool(input, ctx).await,
            Builtin::StopProgram => background_executions::stop_program_tool(input, ctx).await,
            Builtin::StopWorkflow => background_executions::stop_workflow_tool(input, ctx).await,
            Builtin::CronCreate => scheduler::cron_create_tool(input, ctx).await,
            Builtin::CronDelete => scheduler::cron_delete_tool(input, ctx).await,
            Builtin::CronList => scheduler::cron_list_tool(input, ctx).await,
            Builtin::ScheduleWakeup => scheduler::schedule_wakeup_tool(input, ctx).await,
            Builtin::RunProgram => codemode::run_program_tool(input, ctx, workspace).await,
            Builtin::Workflow => workflow::workflow_tool_in_workspace(input, ctx, workspace).await,
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

/// How many other keys a missing-argument refusal lists, and how much of each.
const MAX_LISTED_KEYS: usize = 8;
const MAX_LISTED_KEY_BYTES: usize = 40;

/// What a missing-argument refusal can add: the other keys the call did carry.
///
/// The canonical name alone leaves a model that reached for another harness's
/// spelling — `file_path` for `path`, `old_str` for `old_string` — to work out on
/// its own that the key it sent went nowhere, and the tools reached through
/// [`str_arg`] take no allow-list, so an unknown key is dropped in silence and
/// nothing else mentions it. Naming both sides turns a blind retry into an
/// informed one, and it does so without teaching a second spelling: kloop
/// translates no synonyms. A measured session of 50 `edit_file` calls got the
/// parameter names wrong zero times, so a tolerance layer would be answering a
/// question nobody asked — and it would still owe an answer for two conflicting
/// keys. What this does buy unconditionally is that the *next* wrong name says so
/// in the transcript, which is what would make that measurement possible on a
/// model whose prior differs.
///
/// Silent when the missing key is the only one: an argument that is present but
/// not a string has nothing to point at, and the refusal already names it.
/// Keys are model-supplied, so they are sanitized and capped like any other
/// echoed input.
fn provided_keys(input: &Value, key: &str) -> String {
    let Some(object) = input.as_object() else {
        return String::new();
    };
    let mut listed: Vec<String> = object
        .keys()
        .filter(|name| name.as_str() != key)
        .take(MAX_LISTED_KEYS + 1)
        .map(|name| agent_message::bounded_diagnostic(name, MAX_LISTED_KEY_BYTES))
        .collect();
    if listed.is_empty() {
        return String::new();
    }
    let elided = listed.len() > MAX_LISTED_KEYS;
    listed.truncate(MAX_LISTED_KEYS);
    let names = listed.join(", ");
    let ellipsis = if elided { ", …" } else { "" };
    format!(" (got: {names}{ellipsis})")
}

pub(crate) fn str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key].as_str().ok_or_else(|| {
        let got = provided_keys(input, key);
        anyhow!("{tool}: missing required string argument '{key}'{got}")
    })
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
    use serde_json::json;

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

    /// The one test Config. A test names the handful of fields it actually
    /// cares about through the builder; the other two dozen get the same
    /// permissive defaults every hand-copied literal used to spell out.
    pub(crate) struct TestConfig {
        tag: String,
        provider: Provider,
        model: String,
        allowed_models: Vec<String>,
        max_rounds: Option<usize>,
        context_window: Option<u64>,
        tool_sources: Vec<Arc<dyn ToolSource>>,
        dirs: Option<std::path::PathBuf>,
        surface: crate::config::SurfaceCapabilities,
    }

    impl TestConfig {
        /// `tag` keeps one test's offload and session files off every other
        /// test's — it becomes the `kloop-{tag}` / `kloop-{tag}-sessions`
        /// directory pair under the system temp dir, so it must be unique
        /// across the crate.
        pub(crate) fn new(tag: &str) -> Self {
            Self {
                tag: tag.to_string(),
                provider: Provider::mock(vec![]),
                model: "mock".into(),
                allowed_models: vec!["mock".into()],
                max_rounds: Some(5),
                context_window: None,
                tool_sources: Vec::new(),
                dirs: None,
                // A test ctx models a front-end, and the one the tests want is
                // the fully-featured one: a tool whose surface is off is not
                // offered, so dispatch refuses it before the executor under
                // test ever runs. Tests of an *absent* capability say so with
                // [`with_surface`].
                surface: crate::config::SurfaceCapabilities {
                    questions: true,
                    plan_control: true,
                    program: true,
                    workflow: true,
                    worktree: true,
                    scheduler: true,
                },
            }
        }

        pub(crate) fn provider(mut self, provider: Provider) -> Self {
            self.provider = provider;
            self
        }

        /// Override the route's model names — for tests that read a model name
        /// back out, or that switch the route to a second allowed model.
        pub(crate) fn models(mut self, primary: &str, allowed: &[&str]) -> Self {
            self.model = primary.to_string();
            self.allowed_models = allowed.iter().map(|m| (*m).to_string()).collect();
            self
        }

        pub(crate) fn max_rounds(mut self, max_rounds: Option<usize>) -> Self {
            self.max_rounds = max_rounds;
            self
        }

        pub(crate) fn context_window(mut self, context_window: Option<u64>) -> Self {
            self.context_window = context_window;
            self
        }

        pub(crate) fn tool_sources(mut self, tool_sources: Vec<Arc<dyn ToolSource>>) -> Self {
            self.tool_sources = tool_sources;
            self
        }

        /// Put offload and session files in one caller-owned directory instead
        /// of the tag-derived pair — for tests that reopen the session file.
        pub(crate) fn dirs(mut self, dir: &std::path::Path) -> Self {
            self.dirs = Some(dir.to_path_buf());
            self
        }

        pub(crate) fn build(self) -> Arc<Config> {
            let (provider_catalog, provider_route) =
                crate::provider_route::ProviderCatalog::from_provider(
                    "test",
                    self.provider,
                    self.model,
                    self.allowed_models,
                )
                .expect("test provider route is valid");
            let (offload_dir, sessions_dir) = match self.dirs {
                Some(dir) => (dir.clone(), dir),
                None => (
                    std::env::temp_dir().join(format!("kloop-{}", self.tag)),
                    std::env::temp_dir().join(format!("kloop-{}-sessions", self.tag)),
                ),
            };
            let inbox = Arc::new(crate::inbox::Inbox::default());
            Arc::new(Config {
                provider_catalog,
                provider_route,
                context_budget: crate::config::ContextBudgetSource::Pinned,
                system: "test".into(),
                project_instructions: None,
                max_rounds: self.max_rounds,
                cwd: std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
                offload_dir,
                sessions_dir,
                context_window: self.context_window,
                permissions: Arc::new(crate::permissions::Permissions::allow_all()),
                questioner: None,
                file_state: Default::default(),
                tool_sources: self.tool_sources,
                session_id: String::new(),
                local_agent: crate::agent_mailbox::LocalAgentContext::root(Arc::clone(&inbox)),
                hooks: Arc::new(crate::hooks::Hooks::none()),
                background_shells: BackgroundShells::new(),
                shell_programs: Arc::new(crate::shell_programs::ShellPrograms::test_fixture()),
                powershell_execution_gate: Default::default(),
                sandbox: None,
                agent_types: Arc::new(Vec::new()),
                tool_allowlist: None,
                defer_threshold: 30,
                unlocked_tools: Default::default(),
                todos: Default::default(),
                inbox: Arc::clone(&inbox),
                scheduler: crate::scheduler::Scheduler::in_memory(inbox),
                background_executions: Default::default(),
                program_limits: Default::default(),
                request_reduction: true,
                skills: Default::default(),
                active_worktree: Arc::new(crate::worktree::ActiveWorktreeState::default()),
                surface: self.surface,
            })
        }
    }

    pub(crate) fn test_ctx(depth: u8, tag: &str) -> ToolCtx {
        test_ctx_with_sources(depth, tag, Vec::new())
    }

    pub(crate) fn test_ctx_with_sources(
        depth: u8,
        tag: &str,
        sources: Vec<Arc<dyn ToolSource>>,
    ) -> ToolCtx {
        test_ctx_with_cfg(
            depth,
            TestConfig::new(&format!("tools-{tag}"))
                .tool_sources(sources)
                .build(),
        )
    }

    /// A ctx over a Config the test built itself — for tests that must hold the
    /// same session state (file observations, history) that the ctx dispatches
    /// against, or that need a second ctx on a sub-agent's Config.
    pub(crate) fn test_ctx_with_cfg(depth: u8, cfg: Arc<Config>) -> ToolCtx {
        ToolCtx {
            cfg,
            ui: Arc::new(SilentUi),
            cancel: CancellationToken::new(),
            depth,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
            tool_started: None,
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

    /// Rebuild the ctx for a front-end that offers a different surface — for
    /// the tests that pin what a session WITHOUT a capability does, against
    /// the fully-featured front-end [`TestConfig`] hands out by default.
    pub(crate) fn with_surface(
        mut ctx: ToolCtx,
        surface: crate::config::SurfaceCapabilities,
    ) -> ToolCtx {
        let mut cfg = ctx.cfg.test_clone();
        cfg.surface = surface;
        ctx.cfg = Arc::new(cfg);
        ctx
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
mod reserved_name_tests {
    use super::*;

    /// The set is derived from `builtin::ALL`, so what needs pinning is what is
    /// NOT derived: a catalog tool that somehow is not a variant, the names no
    /// longer offered at all, and the fact that a source tool's name stays free.
    #[test]
    fn the_reserved_set_covers_the_catalog_the_surface_and_the_retired_names() {
        let reserved = reserved_names();

        // Every built-in in the widest catalog, on every host.
        for definition in tool_defs(0, &ShellPrograms::native_posix()) {
            assert!(
                reserved.contains(&definition.name),
                "built-in '{}' is not reserved",
                definition.name
            );
        }

        // The capability-gated session-control surface, absent from the
        // catalog above precisely because a frontend may not enable it.
        for name in [
            "skill",
            "tool_search",
            "call_tool",
            "ask_user_question",
            "enter_plan_mode",
            "exit_plan_mode",
            "workflow",
            "stop_workflow",
            "run_program",
            "stop_program",
            "enter_worktree",
            "exit_worktree",
            "structured_output",
            "cron_create",
            "cron_delete",
            "cron_list",
            "schedule_wakeup",
        ] {
            assert!(
                reserved.contains(name),
                "surface tool '{name}' is not reserved"
            );
        }

        // A name kloop retired is a name kloop released. This assertion is
        // here so the list cannot start growing again: it only ever grew, and
        // every entry bought a marginally better error for a call the model
        // has no reason to make.
        for name in ["task", "task_create"] {
            assert!(
                !reserved.contains(name),
                "retired tool '{name}' is reserved"
            );
        }

        // Both shells stay reserved even where the host offers neither.
        for name in ["bash", "bash_output", "stop_bash", "powershell"] {
            assert!(
                reserved.contains(name),
                "shell tool '{name}' is not reserved"
            );
        }

        // A source tool's own name is not.
        assert!(!reserved.contains("srv__x"));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testutil::*;
    use super::*;

    /// The two things the synonym rename must never do. It fills a gap and it
    /// does not guess: a canonical key that is present stays, whatever its type
    /// and whatever else the call carried, and synonyms that disagree are left
    /// alone for the tool's own refusal to explain — naming both the argument it
    /// wanted and the keys it got is exactly what that refusal is for.
    #[test]
    fn renaming_a_synonym_fills_a_gap_and_never_guesses() {
        let renamed = |tool: &str, input: Value| {
            let mut input = input;
            rename_argument_synonyms(tool, &mut input);
            input
        };

        assert_eq!(
            renamed(
                "edit_file",
                json!({"file_path": "a", "old_str": "b", "new_str": "c"})
            ),
            json!({"path": "a", "old_string": "b", "new_string": "c"})
        );
        // Present canonical wins, and the stray synonym is not merged in: a call
        // that used the real name is never second-guessed.
        assert_eq!(
            renamed("edit_file", json!({"path": "real", "file_path": "other"})),
            json!({"path": "real", "file_path": "other"})
        );
        assert_eq!(
            renamed("edit_file", json!({"path": 5, "file_path": "other"})),
            json!({"path": 5, "file_path": "other"})
        );
        // Two synonyms for one argument: consumed when they agree, untouched when
        // they do not.
        assert_eq!(
            renamed(
                "notebook_edit",
                json!({"path": "same", "file_path": "same"})
            ),
            json!({"notebook_path": "same"})
        );
        assert_eq!(
            renamed("notebook_edit", json!({"path": "a", "file_path": "b"})),
            json!({"path": "a", "file_path": "b"})
        );
        // Only the tools listed, and only their own arguments.
        assert_eq!(
            renamed("read_file", json!({"old_str": "x", "file_path": "a"})),
            json!({"old_str": "x", "path": "a"})
        );
        assert_eq!(
            renamed("srv__external", json!({"file_path": "a"})),
            json!({"file_path": "a"})
        );
        assert_eq!(
            renamed("edit_file", json!("not an object")),
            json!("not an object")
        );
    }

    /// Definitions as sent when the `program` surface is on — the branch that
    /// still ships `run_program`. Its own tests keep exercising it; plan 113 only
    /// changed which branch is the default.
    fn program_surface_defs() -> Vec<ToolDef> {
        all_tool_defs(
            0,
            &[],
            /*defer_threshold*/ 200,
            crate::config::SurfaceCapabilities {
                program: true,
                ..Default::default()
            },
            &ShellPrograms::native_posix(),
        )
    }

    fn interactive_surface() -> crate::config::SurfaceCapabilities {
        crate::config::SurfaceCapabilities {
            questions: true,
            program: true,
            plan_control: true,
            ..Default::default()
        }
    }

    /// Plan 113: the code engine keeps one model-facing door. Off — the default —
    /// neither `run_program` nor its `stop_program` reaches the model, and the
    /// count that drives the defer threshold drops with them. On restores both.
    /// Nothing is deleted either way; this pins both branches so flipping back is
    /// a config change, not a revival.
    #[test]
    fn the_program_surface_gates_run_program_and_its_stop_tool() {
        let names = |program: bool| {
            all_tool_defs(
                0,
                &[],
                /*defer_threshold*/ 200,
                crate::config::SurfaceCapabilities {
                    program,
                    workflow: true,
                    ..interactive_surface()
                },
                &ShellPrograms::native_posix(),
            )
            .into_iter()
            .map(|def| def.name)
            .collect::<Vec<_>>()
        };
        let off = names(false);
        assert!(!off.contains(&"run_program".to_string()), "{off:?}");
        assert!(!off.contains(&"stop_program".to_string()), "{off:?}");
        // The other door onto the same engine is untouched.
        assert!(off.contains(&"workflow".to_string()), "{off:?}");
        assert!(off.contains(&"run_agent".to_string()), "{off:?}");

        let on = names(true);
        assert!(on.contains(&"run_program".to_string()), "{on:?}");
        assert!(on.contains(&"stop_program".to_string()), "{on:?}");
        assert_eq!(on.len(), off.len() + 2);

        // Counting follows what is actually sent: a surface-gated tool is not in
        // the built-in count, exactly like workflow and worktree.
        let counted = tool_defs(0, &ShellPrograms::native_posix())
            .into_iter()
            .map(|def| def.name)
            .collect::<Vec<_>>();
        assert!(!counted.contains(&"run_program".to_string()), "{counted:?}");
        assert!(
            !counted.contains(&"stop_program".to_string()),
            "{counted:?}"
        );
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

    /// Read-only external source that counts how many of its calls overlap.
    /// Each call registers and then blocks on `gate` until the test releases
    /// it, so the peak is whatever the dispatcher allowed to run at once
    /// rather than a timing artifact of how fast the calls returned.
    struct ConcurrencyProbe {
        started: std::sync::atomic::AtomicUsize,
        active: std::sync::atomic::AtomicUsize,
        peak: std::sync::atomic::AtomicUsize,
        gate: tokio::sync::Semaphore,
    }

    impl ConcurrencyProbe {
        fn new() -> Arc<Self> {
            Arc::new(ConcurrencyProbe {
                started: Default::default(),
                active: Default::default(),
                peak: Default::default(),
                gate: tokio::sync::Semaphore::new(0),
            })
        }

        fn started(&self) -> usize {
            self.started.load(Ordering::SeqCst)
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        /// Let `calls` blocked calls finish. Permits granted before a call
        /// arrives wait for it.
        fn release(&self, calls: usize) {
            self.gate.add_permits(calls);
        }

        /// Hand the runtime back until `want` calls have started. Callers wrap
        /// this in a timeout: it never returns on its own if the dispatcher
        /// refuses to start that many.
        async fn wait_for_started(&self, want: usize) {
            while self.started() < want {
                tokio::task::yield_now().await;
            }
        }

        /// Hand the runtime back long enough for the dispatcher to start
        /// everything it is willing to start.
        async fn settle(&self) {
            for _ in 0..100 {
                tokio::task::yield_now().await;
            }
        }

        fn calls(&self, count: usize) -> Vec<(String, String, Value)> {
            (0..count)
                .map(|n| (format!("t{n}"), "probe__wait".into(), json!({"n": n})))
                .collect()
        }
    }

    impl ToolSource for ConcurrencyProbe {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(vec![ToolDef {
                name: "probe__wait".into(),
                description: "blocks until the test releases it".into(),
                schema: json!({"type": "object"}),
            }])
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.started.fetch_add(1, Ordering::SeqCst);
                let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
                self.peak.fetch_max(active, Ordering::SeqCst);
                self.gate
                    .acquire()
                    .await
                    .expect("the probe gate is never closed")
                    .forget();
                self.active.fetch_sub(1, Ordering::SeqCst);
                Ok(SourceOutput::text(format!("probe {}", input["n"])))
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
        let definitions = program_surface_defs();
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
        // The round cap is fixed in code, not offered to the model: it has no
        // basis for the number, and both wrong answers were measured (12/10 ran
        // out; unbounded reached 158 rounds and tripled the request count).
        assert!(run_agent.schema["properties"]["max_rounds"].is_null());
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

    /// The order of the tool array is part of the provider request's bytes, and
    /// the prompt cache keys on those bytes: a reshuffle that changes nothing
    /// semantically still invalidates every cached prefix. Since the catalog is
    /// now walked from `builtin::ALL` rather than pushed group by group, this
    /// pins the walk's output against the sequence the hand-written pushes
    /// produced.
    #[test]
    fn the_tool_array_keeps_its_wire_order() {
        let names = |depth, surface| {
            all_tool_defs(depth, &[], 200, surface, &ShellPrograms::native_posix())
                .into_iter()
                .map(|definition| definition.name)
                .collect::<Vec<_>>()
        };
        let everything = crate::config::SurfaceCapabilities {
            questions: true,
            plan_control: true,
            program: true,
            workflow: true,
            worktree: true,
            scheduler: true,
        };
        assert_eq!(
            names(0, everything),
            [
                "bash",
                "bash_output",
                "stop_bash",
                "read_file",
                "write_file",
                "edit_file",
                "notebook_edit",
                "grep",
                "glob",
                "todo_write",
                "send_message",
                "list_agents",
                "run_agent",
                "wait_for_activity",
                "stop_agent",
                "run_program",
                "stop_program",
                "cron_create",
                "cron_delete",
                "cron_list",
                "schedule_wakeup",
                "ask_user_question",
                "enter_plan_mode",
                "exit_plan_mode",
                "workflow",
                "stop_workflow",
                "enter_worktree",
                "exit_worktree",
            ]
        );
        // A sub-agent sees the depth-gated block drop out, and nothing else move.
        assert_eq!(
            names(1, everything),
            [
                "bash",
                "bash_output",
                "stop_bash",
                "read_file",
                "write_file",
                "edit_file",
                "notebook_edit",
                "grep",
                "glob",
                "send_message",
                "list_agents",
            ]
        );
        // Every surface off: the depth-0 tail goes with them, the rest stays put.
        assert_eq!(
            names(0, crate::config::SurfaceCapabilities::default()),
            [
                "bash",
                "bash_output",
                "stop_bash",
                "read_file",
                "write_file",
                "edit_file",
                "notebook_edit",
                "grep",
                "glob",
                "todo_write",
                "send_message",
                "list_agents",
                "run_agent",
                "wait_for_activity",
                "stop_agent",
            ]
        );
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
        // The whole background-agent surface, not just its spawn tool: listing
        // only `run_agent` here is how `stop_agent` and `wait_for_activity`
        // went unnoticed on the execution side for as long as they did.
        for root_only in ["run_agent", "wait_for_activity", "stop_agent", "todo_write"] {
            assert!(root.iter().any(|name| name == root_only), "{root_only}");
            assert!(!child.iter().any(|name| name == root_only), "{root_only}");
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
            "Replace exact old_string matches with new_string in an existing UTF-8 file of at most 5 MiB. The file must have been read in this session — any range qualifies; if it changed since that read the edit still applies and the result says so. Raw matches take priority; when none exist, LF old_string may match CRLF text without normalizing untouched bytes. Fails if old_string is absent or matches more than once without replace_all. Never creates a missing file or parent directory."
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
        // identical names are dropped. run_program comes after the sources
        // because its TypeScript API is generated from the built-ins AND the
        // source tools, and `stop_program` now rides beside it on the same
        // surface flag instead of sitting among the built-ins.
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
                "todo_write",
                "send_message",
                "list_agents",
                "run_agent",
                "wait_for_activity",
                "stop_agent",
                "srv__echo",
                "srv__fail",
                "srv__image",
                "run_program",
                "stop_program",
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

    /// A name the catalog does not carry fails closed with one answer, whether
    /// it was never a tool or used to be one. `task` and `task_create` are the
    /// probes because both once had a hand-written rename or reservation: this
    /// test is what keeps that kind of entry from coming back.
    #[tokio::test]
    async fn an_unknown_tool_name_fails_closed_with_no_rename_table() {
        let ctx = test_ctx(0, "unknown-tool-names");
        for name in ["task", "task_create", "never_was_a_tool"] {
            let (output, is_error) = run_tool(name, json!({}), &ctx).await;
            assert!(is_error, "{name}: {output}");
            assert_eq!(output, format!("unknown tool: {name}"));
        }
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
        // appended after run_program and do not count toward the threshold —
        // and neither do run_program and stop_program themselves any more, since
        // plan 113 put them behind the `program` surface (which this helper
        // enables, so they are present in the result even though absent from
        // `builtin_count`).
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
        assert_eq!(deferred_regime.len(), builtin_count + 7);
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
        let defs = program_surface_defs();
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
    /// filtered), while coordination tools stay available regardless.
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

        // Coordination is the infra exception: never blocked by the list.
        let (out, _) = run_tool("list_agents", json!({}), &ctx).await;
        assert!(!out.contains("not available to this agent type"), "{out}");
    }

    /// A child cannot gain the root task list through an explicit custom
    /// allowlist, a forged call, or the deferred call_tool envelope.
    #[tokio::test]
    async fn child_todo_calls_fail_before_the_registry_even_when_allowlisted() {
        let root = test_ctx(0, "root-task-gate");
        let (written, is_error) = run_tool(
            "todo_write",
            json!({"todos":[{"subject":"root work","status":"pending"}]}),
            &root,
        )
        .await;
        assert!(!is_error, "{written}");

        let mut cfg = root.cfg.test_clone();
        cfg.tool_allowlist = Some(Arc::new(
            ["todo_write"].into_iter().map(str::to_string).collect(),
        ));
        let child = ToolCtx {
            cfg: Arc::new(cfg),
            depth: 1,
            ..root.clone()
        };
        for input in [
            json!({"todos":[{"subject":"forged","status":"pending"}]}),
            json!({"todos":[]}),
        ] {
            let (output, is_error) = run_tool("todo_write", input, &child).await;
            assert!(is_error, "{output}");
            assert_eq!(
                output,
                "tool 'todo_write' is only available to the root agent"
            );
        }
        let (output, is_error) = run_tool(
            "call_tool",
            json!({"tool_name":"todo_write","params":{"todos":[]}}),
            &child,
        )
        .await;
        assert!(is_error, "{output}");
        assert_eq!(
            output,
            "tool 'todo_write' is only available to the root agent"
        );

        // The child shares the Arc; what it never reaches is the registry.
        assert_eq!(
            root.cfg.todos.snapshot().todos,
            vec![crate::tools::TodoItem {
                subject: "root work".into(),
                status: crate::tools::TodoStatus::Pending,
            }]
        );
    }

    /// The recurrence guard. Whatever a builder would leave out of the request,
    /// the door has to refuse — walked from `builtin::ALL` rather than from a
    /// list, because a hand-kept list is exactly what let `stop_agent` and
    /// `wait_for_activity` fall out of the door while staying out of the
    /// catalog. Add a `Depth0` or `Surface` variant and this test covers it the
    /// moment it exists.
    #[tokio::test]
    async fn the_door_refuses_every_builtin_the_catalog_would_have_withheld() {
        let root = with_surface(test_ctx(0, "gate-parity"), Default::default());
        let child = ToolCtx {
            depth: 1,
            ..root.clone()
        };
        let mut checked = 0;
        for tool in builtin::ALL {
            let name = tool.name();
            match tool.gate() {
                // A sub-agent is sent neither the root-only controls nor the
                // surface block, whatever its own Config says about the latter.
                builtin::Gate::Depth0 | builtin::Gate::Surface(_) => {
                    let (out, is_error) = run_tool(name, json!({}), &child).await;
                    assert!(is_error, "{name}: {out}");
                    assert_eq!(
                        out,
                        format!("tool '{name}' is only available to the root agent")
                    );
                    checked += 1;
                }
                builtin::Gate::Always | builtin::Gate::Shell(_) | builtin::Gate::Elsewhere => {}
            }
            // And at depth 0 a surface tool is still refused when the front-end
            // this session runs against does not enable it — `root` above runs
            // against one that enables nothing.
            if matches!(tool.gate(), builtin::Gate::Surface(_)) {
                let (out, is_error) = run_tool(name, json!({}), &root).await;
                assert!(is_error, "{name}: {out}");
                assert!(
                    out.starts_with(&format!(
                        "tool '{name}' is unavailable because this session's front-end"
                    )),
                    "{name}: {out}"
                );
                checked += 1;
            }
        }
        // 4 root-only controls, plus 13 surface-gated tools refused twice —
        // once for the depth, once for the front-end. A gate table that stopped
        // naming them would pass every assertion above without running one.
        assert_eq!(checked, 4 + 13 * 2);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn child_todo_gate_runs_before_pre_tool_hooks() {
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
                matcher: Some("todo_write".into()),
                timeout_ms: crate::hooks::DEFAULT_TIMEOUT_MS,
            }],
        });
        let child = ToolCtx {
            cfg: Arc::new(cfg),
            ..base
        };

        let (output, is_error) = run_tool("todo_write", json!({"todos": []}), &child).await;
        assert!(is_error, "{output}");
        assert_eq!(
            output,
            "tool 'todo_write' is only available to the root agent"
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

    /// `description` is display metadata and nothing else: the same command
    /// with a label and without one produces the same result byte for byte,
    /// and an unusable label is refused before the command runs instead of
    /// being silently dropped.
    #[tokio::test]
    async fn bash_description_is_display_only() {
        let ctx = test_ctx(1, "bash-description");
        let plain = run_tool("bash", bash_input("printf DESCRIBED"), &ctx).await;
        let labelled = run_tool(
            "bash",
            json!({"command": "printf DESCRIBED", "description": "Print the sentinel"}),
            &ctx,
        )
        .await;
        assert_eq!(plain, labelled);

        let (out, is_error) = run_tool(
            "bash",
            json!({"command": "printf NEVER", "description": "   "}),
            &ctx,
        )
        .await;
        assert_eq!(
            (out, is_error),
            ("bash: description must not be empty".to_string(), true)
        );
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

    fn result_ids(results: &[ContentBlock]) -> Vec<String> {
        results
            .iter()
            .map(|block| match block {
                ContentBlock::ToolResult { tool_use_id, .. } => tool_use_id.clone(),
                other => panic!("expected tool result, got {other:?}"),
            })
            .collect()
    }

    /// A batch is never split to enforce the cap, so a round of 25 read-only
    /// calls is still one batch — but only [`MAX_CONCURRENT_TOOL_CALLS`] of
    /// them ever run at once, and the results still come back in request order.
    #[tokio::test]
    async fn a_concurrent_batch_runs_at_most_the_call_limit_at_once() {
        let probe = ConcurrencyProbe::new();
        let sources: Vec<Arc<dyn ToolSource>> = vec![probe.clone()];
        let ctx = test_ctx_with_sources(0, "concurrency-cap", sources);
        let calls = MAX_CONCURRENT_TOOL_CALLS * 2 + 5;
        let uses = probe.calls(calls);
        let dispatch = tokio::spawn({
            let ctx = ctx.clone();
            async move { dispatch_tools(uses, &ctx).await }
        });

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            probe.wait_for_started(MAX_CONCURRENT_TOOL_CALLS),
        )
        .await
        .expect("the batch never reached the concurrency limit");
        probe.settle().await;
        assert_eq!(probe.started(), MAX_CONCURRENT_TOOL_CALLS);

        probe.release(calls);
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
            .await
            .expect("the batch stalled behind the limiter")
            .expect("dispatch task panicked");
        assert_eq!(probe.peak(), MAX_CONCURRENT_TOOL_CALLS);
        assert_eq!(
            result_ids(&results),
            (0..calls).map(|n| format!("t{n}")).collect::<Vec<_>>()
        );
    }

    /// `tool_search` is an ordering barrier: the read-only calls around it must
    /// stay in separate batches, so a following call cannot start until the
    /// preceding one has finished. The same two calls without it do overlap —
    /// that half is the negative control for the first.
    #[tokio::test]
    async fn tool_search_still_splits_the_batch_it_sits_between() {
        let probe = ConcurrencyProbe::new();
        let sources: Vec<Arc<dyn ToolSource>> = vec![probe.clone()];
        let ctx = test_ctx_with_sources(0, "concurrency-barrier", sources);
        let uses = vec![
            ("t0".into(), "probe__wait".into(), json!({"n": 0})),
            (
                "search".into(),
                "tool_search".into(),
                json!({"query": "probe", "max_results": 1}),
            ),
            ("t1".into(), "probe__wait".into(), json!({"n": 1})),
        ];
        let dispatch = tokio::spawn({
            let ctx = ctx.clone();
            async move { dispatch_tools(uses, &ctx).await }
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), probe.wait_for_started(1))
            .await
            .expect("the first call never started");
        probe.settle().await;
        assert_eq!(
            probe.started(),
            1,
            "the call after tool_search joined the batch before it"
        );

        probe.release(2);
        let results = tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
            .await
            .expect("the barred batch stalled")
            .expect("dispatch task panicked");
        assert_eq!(probe.peak(), 1);
        assert_eq!(result_ids(&results), vec!["t0", "search", "t1"]);

        let overlapping = ConcurrencyProbe::new();
        let sources: Vec<Arc<dyn ToolSource>> = vec![overlapping.clone()];
        let ctx = test_ctx_with_sources(0, "concurrency-no-barrier", sources);
        let uses = overlapping.calls(2);
        let dispatch = tokio::spawn({
            let ctx = ctx.clone();
            async move { dispatch_tools(uses, &ctx).await }
        });
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            overlapping.wait_for_started(2),
        )
        .await
        .expect("two read-only calls with nothing between them did not overlap");
        overlapping.release(2);
        tokio::time::timeout(std::time::Duration::from_secs(5), dispatch)
            .await
            .expect("the unbarred batch stalled")
            .expect("dispatch task panicked");
        assert_eq!(overlapping.peak(), 2);
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

    /// Plan 204: the orchestrators are batched as concurrency-safe, but what
    /// they spawn acts on the world, so a crash inside one is not "nothing
    /// happened".
    #[test]
    fn orchestrators_may_have_effects_though_batched_as_safe() {
        let effects = |name: &str, input: Value| super::may_have_effects(name, &input, &[]);
        assert_eq!(
            [
                effects("read_file", json!({"path": "x"})),
                effects("bash", bash_input("ls")),
                effects("bash_output", json!({"bash_id": "bg-1"})),
                effects("write_file", json!({"path": "x", "content": ""})),
                effects("bash", bash_input("rm x")),
                effects("run_agent", json!({"prompt": "x"})),
                effects("workflow", json!({})),
                effects("run_program", json!({})),
            ],
            [false, false, false, true, true, true, true, true]
        );
    }

    #[test]
    fn concurrency_safety_by_name_and_input() {
        fn is_concurrency_safe(name: &str, input: &Value) -> bool {
            super::is_concurrency_safe(name, input, &[])
        }
        assert!(is_concurrency_safe("read_file", &json!({"path": "x"})));
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
        assert!(!is_concurrency_safe("todo_write", &json!({"todos": []})));
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
        struct NullUi;
        impl Ui for NullUi {
            fn emit(&self, _: &Event) {}
        }

        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = ToolCtx {
            cfg: testutil::TestConfig::new("test-cancel").build(),
            ui: Arc::new(NullUi),
            cancel,
            depth: 0,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
            tool_started: None,
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
