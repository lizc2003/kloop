//! The tool seam: the `ToolCtx` every call carries, the source contract, and
//! the entry points the agent loop and the frontends depend on.
//!
//! Three sibling modules carry the machinery:
//! - `catalog` — what a request offers and what it withholds;
//! - `dispatch` — one round of calls: batching, gating, hooks, deadlines;
//! - `paths` — the path anchor the tools and the permission gate share.
//!
//! Individual tool implementations live in the other sibling modules.

mod agent_gate;
mod agent_message;
pub(crate) mod args;
mod background_executions;
mod background_registry;
mod bash;
mod builtin;
mod catalog;
mod codemode;
mod contract;
mod dispatch;
mod fs;
mod inject;
pub(crate) mod notebook;
mod paths;
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
pub(crate) use bash::truncate_incomplete_utf8;
pub(crate) use builtin::Builtin;
pub use builtin::builtin_tool_names;
pub use catalog::all_tool_defs;
pub use catalog::defer_active;
pub use catalog::deferred_tool_defs;
pub use catalog::is_concurrency_safe;
pub use catalog::tool_defs;
pub use catalog::tool_merge_warnings;
pub(crate) use codemode::ProgramToolManifest;
pub(crate) use codemode::capture_program_tool_manifest;
pub use dispatch::dispatch_tools;
pub(crate) use paths::resolve_path;
pub use tool_search::DeferredToolUnlocks;
// Slash-path prompt injections (`!cmd` / `@file`) for `/name` commands; the
// dispatch layer (`crate::commands`) calls this before running the turn.
pub(crate) use inject::expand_slash_injections;
// Registered on the run_agent peer set only at depth 0 with skills loaded
// (see `turn_rounds`); the pure skill logic it drives lives in `crate::skills`.
pub(crate) use skill::skill_tool_def;
// Recorded at the round boundary by `agent.rs` (plan 197), not on a tool result;
// the async entry point is what that round boundary calls (plan 265). The
// synchronous one stays for the tests that exercise the judgement on its own.
#[cfg(test)]
pub(crate) use fs::changed_reads_reminder;
pub(crate) use fs::changed_reads_reminder_async;
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
// Siblings reach the split modules through these private bindings, exactly as
// they reached the definitions when everything lived here.
use catalog::analyze_shell_call;
use catalog::builtin_defs;
#[cfg(test)]
pub(crate) use catalog::builtin_defs_builds;
use catalog::is_concurrency_safe_with;
#[cfg(test)]
use catalog::may_have_effects;
use catalog::merged_source_defs;
#[cfg(test)]
use catalog::reserved_names;
use catalog::source_definition_result;
use catalog::source_definition_snapshot;
use catalog::source_route_state;
use catalog::source_tool_deferral;
use catalog::tool_name_is_inline;
use dispatch::NOT_RUN_STOPPED;
use dispatch::run_one;
use dispatch::run_pre_tool_hook;
use dispatch::unless_stopped;

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::Ordering;
use std::time::Duration;

use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::agent::Ui;
use crate::config::Config;
use crate::event::Event;
#[cfg(test)]
use crate::permissions::user_stop;
#[cfg(test)]
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

/// Cap on a failure text reinjected into the model's context: room for a stack
/// trace and the frames that matter, short enough not to buy a turn of context.
const MAX_REINJECT_ERROR_CHARS: usize = 3600;

/// Clip a reinjected failure to [`MAX_REINJECT_ERROR_CHARS`], keeping the head —
/// the error class and first frames — and saying so when it cut.
fn clip_error(text: &str) -> String {
    kloop_protocol::clip_chars(text, MAX_REINJECT_ERROR_CHARS, "… (error truncated)").into_owned()
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

/// What one call reports to its turn for the session file. The file belongs to
/// the turn's `History`, which `run_one` cannot reach, so the report travels to
/// the turn and the call waits until the line is written.
pub(crate) enum JournalNote {
    /// A call that may have side effects is about to run (plan 204): the
    /// rollout says "started" only before the effect can begin.
    Started { tool_use_id: String },
    /// A permission question about this call, or a memory that answered one
    /// (plan 283). Only read back offline, to count questions.
    Approval {
        tool_use_id: String,
        record: crate::permissions::ApprovalRecord,
    },
}

/// The turn's end of a [`ToolJournal`].
pub(crate) type ToolJournalReceiver =
    tokio::sync::mpsc::UnboundedReceiver<(JournalNote, tokio::sync::oneshot::Sender<()>)>;

/// A call's way to the turn's session file. `run_one` binds it to the call's
/// id, so a note sent from anywhere below (the gate, the bash escalation) says
/// which call it is about. A question asked after the round ended — a
/// background Program's call — goes to the history's backlog instead.
#[derive(Clone)]
pub(crate) struct ToolJournal {
    tx: tokio::sync::mpsc::UnboundedSender<(JournalNote, tokio::sync::oneshot::Sender<()>)>,
    backlog: crate::history::ApprovalBacklog,
    /// False for calls a program fires: the program's own call is the one the
    /// session's pairing repair has to judge, so theirs are not journaled as
    /// started. Their questions still are.
    starts: bool,
    call_id: Option<Arc<str>>,
}

impl ToolJournal {
    pub(crate) fn channel(backlog: crate::history::ApprovalBacklog) -> (Self, ToolJournalReceiver) {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            Self {
                tx,
                backlog,
                starts: true,
                call_id: None,
            },
            rx,
        )
    }

    pub(crate) fn without_starts(self) -> Self {
        Self {
            starts: false,
            ..self
        }
    }

    fn for_call(self, id: &str) -> Self {
        Self {
            call_id: Some(Arc::from(id)),
            ..self
        }
    }

    async fn started(&self) {
        if !self.starts {
            return;
        }
        if let Some(id) = &self.call_id {
            self.send(JournalNote::Started {
                tool_use_id: id.to_string(),
            })
            .await;
        }
    }

    pub(crate) async fn approval(&self, record: crate::permissions::ApprovalRecord) {
        let Some(id) = &self.call_id else {
            return;
        };
        let note = JournalNote::Approval {
            tool_use_id: id.to_string(),
            record: record.clone(),
        };
        if !self.send(note).await {
            self.backlog.lock().unwrap().push((id.to_string(), record));
        }
    }

    /// Returns once the turn has written the line, or at once if the turn is
    /// no longer listening — a note nobody can write must not block the call.
    /// Whether the line was written.
    async fn send(&self, note: JournalNote) -> bool {
        let (ack, written) = tokio::sync::oneshot::channel();
        self.tx.send((note, ack)).is_ok() && written.await.is_ok()
    }
}

/// Everything a tool execution needs; cheap to clone into spawned futures.
#[derive(Clone)]
pub struct ToolCtx {
    pub cfg: Arc<Config>,
    pub ui: Arc<dyn Ui>,
    pub cancel: CancellationToken,
    /// The user answered an approval No (plan 216). Unlike `cancel` nothing is
    /// interrupted: calls already running finish, calls not yet started do not
    /// run, and the turn ends after this batch. Shared down the foreground turn
    /// tree — a No inside a foreground sub-agent stops its parent too — while a
    /// background agent or program carries its own.
    pub stop: CancellationToken,
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
    /// Set only by a turn whose calls answer to a session file; None for
    /// harness commands. Calls a program fires carry it without "started"
    /// notes ([`ToolJournal::without_starts`]).
    pub(crate) journal: Option<ToolJournal>,
}

impl ToolCtx {
    /// Write a permission question into the session file, when this call
    /// answers to one.
    pub(crate) async fn journal_approval(
        &self,
        records: impl IntoIterator<Item = crate::permissions::ApprovalRecord>,
    ) {
        if let Some(journal) = &self.journal {
            for record in records {
                journal.approval(record).await;
            }
        }
    }
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
            stop: CancellationToken::new(),
            depth: 0,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
            journal: None,
        }
    }
}

struct SilentHarnessUi;
impl Ui for SilentHarnessUi {
    fn emit(&self, _ev: &Event) {}
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
            if name == "ask_user_question" {
                question::normalize_arguments(&mut input);
            }
            (id, name, input)
        })
        .collect()
}

pub(crate) use kloop_protocol::char_prefix;

pub(crate) const MAX_DISPLAY_DESCRIPTION_CHARS: usize = 200;

/// Validate optional human-facing metadata before any tool side effect. An
/// explicit `null` is omission, the same rule the shared argument helpers use.
pub(crate) fn optional_display_description(input: &Value, tool: &str) -> Result<Option<String>> {
    let Some(description) = args::optional_str(input, "description", tool)? else {
        return Ok(None);
    };
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

pub(crate) use args::required_str as str_arg;

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
        login_env: Option<crate::shell_env::ShellLoginEnv>,
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
                login_env: None,
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

        /// Stand in for a captured login environment: the shell runs non-login
        /// with these variables. Only the Unix bash tests capture one.
        #[cfg(unix)]
        pub(crate) fn login_env(mut self, login_env: crate::shell_env::ShellLoginEnv) -> Self {
            self.login_env = Some(login_env);
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
                shell_login_env: Arc::new(self.login_env.unwrap_or_default()),
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
                cleanup_tasks: Default::default(),
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
            stop: CancellationToken::new(),
            depth,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
            journal: None,
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
                crate::git::hermetic_command(&root)
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

    /// Plan 283 review: a question asked after its round ended — a background
    /// Program keeps calling tools once the dispatch that started it returned,
    /// and the round's channel with it — is not lost. It waits in the
    /// history's backlog and is written before the next line.
    #[tokio::test]
    async fn a_question_after_the_round_ended_is_written_before_the_next_line() {
        let dir = std::env::temp_dir()
            .join(crate::resource_id::fresh("kloop-plan283-backlog-").expect("fresh id"));
        std::fs::create_dir_all(&dir).unwrap();
        let session = dir.join("session.jsonl");
        let mut history = crate::history::History::new(dir.join("offload"));
        history.attach_rollout(crate::rollout::Rollout::new(session.clone()));
        let (journal, notes) = ToolJournal::channel(history.approval_backlog());
        drop(notes);
        let record = crate::permissions::ApprovalRecord {
            door: crate::permissions::ApprovalDoor::Gate,
            tool: "bash".into(),
            reason: crate::permissions::AskReason::Uncontained,
            detail: "make install".into(),
            keys: vec!["bash:make install".into()],
            rules: vec!["bash(make install *)".into()],
            offered: vec![crate::permissions::ApprovalScope::Once],
            answer: crate::permissions::ApprovalAnswer::Allowed {
                scope: crate::permissions::ApprovalScope::Once,
            },
        };
        journal
            .for_call("program-call")
            .approval(record.clone())
            .await;
        history.note_tool_started("next-call");

        let lines: Vec<Value> = std::fs::read_to_string(&session)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .filter(|line| matches!(line["type"].as_str(), Some("approval" | "tool_started")))
            .collect();
        let kinds: Vec<(&str, &str)> = lines
            .iter()
            .map(|line| {
                (
                    line["type"].as_str().unwrap(),
                    line["tool_use_id"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            kinds,
            vec![("approval", "program-call"), ("tool_started", "next-call")]
        );
        let mut written = lines[0].clone();
        for envelope in [
            "type",
            "tool_use_id",
            "id",
            "parent",
            "ts",
            "format_version",
        ] {
            written.as_object_mut().unwrap().remove(envelope);
        }
        assert_eq!(
            serde_json::from_value::<crate::permissions::ApprovalRecord>(written).unwrap(),
            record
        );
        let _ = std::fs::remove_dir_all(dir);
    }

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

    /// Plan 266: one foreground bash call parses its command exactly once.
    /// Batching, the gate and execution read the dispatch's facts; before
    /// this, the same text went through tree-sitter five to seven times.
    #[tokio::test]
    async fn one_foreground_bash_call_parses_its_command_once() {
        let cfg = (*TestConfig::new("plan266-one-parse").build()).clone();
        let cfg = Config {
            permissions: Arc::new(
                crate::permissions::Permissions::new(
                    crate::permissions::Mode::Manual,
                    &crate::permissions::PermissionRules::default(),
                    cfg.cwd.clone(),
                    None,
                )
                .expect("empty rules parse"),
            ),
            ..cfg
        };
        let ctx = test_ctx_with_cfg(0, Arc::new(cfg));
        crate::shell::reset_bash_analyses();
        let results = dispatch_tools(
            vec![("t".into(), "bash".into(), bash_input("echo kloop-plan266"))],
            &ctx,
        )
        .await;
        assert!(
            results.iter().all(|block| matches!(
                block,
                ContentBlock::ToolResult {
                    is_error: false,
                    ..
                }
            )),
            "{results:?}"
        );
        assert_eq!(crate::shell::bash_analyses(), 1);
    }

    /// The batching verdict must not change when it reads the parse dispatch
    /// already made instead of paying for its own.
    #[test]
    fn plan266_batching_decides_from_the_shared_parse() {
        for command in ["echo hi", "cargo build", "cat x > out.txt"] {
            let input = bash_input(command);
            let facts = analyze_shell_call("bash", &input).expect("bash has a command");
            assert_eq!(
                is_concurrency_safe("bash", &input, &[]),
                is_concurrency_safe_with("bash", &input, &[], Some(&facts)),
                "{command}"
            );
        }
    }

    /// Plan 204: the orchestrators are batched as concurrency-safe, but what
    /// they spawn acts on the world, so a crash inside one is not "nothing
    /// happened".
    #[test]
    fn orchestrators_may_have_effects_though_batched_as_safe() {
        let effects = |name: &str, input: Value| super::may_have_effects(name, &input, &[], None);
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

    /// A skill activation is not "just reading a file": the body's `!cmd` /
    /// `@file` injections expand inside the call (each gated on its own), and a
    /// `fork` skill runs a sub-agent. So it is neither batched beside a
    /// concurrent read nor treated as having done nothing after a crash.
    #[test]
    fn skill_activation_is_effectful() {
        let input = json!({"name": "x"});
        assert!(!super::is_concurrency_safe("skill", &input, &[]));
        assert!(super::may_have_effects("skill", &input, &[], None));
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
            stop: CancellationToken::new(),
            depth: 0,
            enclosing_execution: None,
            hook_context: Arc::new(std::sync::Mutex::new(Vec::new())),
            from_program: false,
            program_tool_manifest: None,
            parent_rollout_id: None,
            program_result: None,
            journal: None,
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

/// Plan 216: a person's No on an approval ends the turn once its batch is done.
#[cfg(test)]
mod stop_tests {
    use std::sync::atomic::AtomicUsize;

    use serde_json::json;
    use tokio::sync::mpsc;
    use tokio::sync::oneshot;

    use super::testutil::*;
    use super::*;
    use crate::permissions::Approver;
    use crate::permissions::ConfirmRequest;
    use crate::permissions::Decision;
    use crate::permissions::Mode;
    use crate::permissions::PermissionRules;
    use crate::permissions::Permissions;

    /// Answers every approval with one decision and counts the asks.
    struct Answering {
        decision: Decision,
        asked: AtomicUsize,
    }

    impl Approver for Answering {
        fn confirm(
            &self,
            _: ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            let decision = self.decision;
            Box::pin(async move { decision })
        }
    }

    /// Hands each approval to the test, which answers it (or doesn't).
    struct Handing(mpsc::UnboundedSender<oneshot::Sender<Decision>>);

    impl Approver for Handing {
        fn confirm(
            &self,
            _: ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = Decision> + Send + '_>> {
            let (reply, answer) = oneshot::channel();
            self.0.send(reply).unwrap();
            Box::pin(async move { answer.await.unwrap_or(Decision::Deny) })
        }
    }

    /// A manual-mode gate over a fresh directory that is also the cwd, so a
    /// write inside it is contained (never asks) while an opaque bash script
    /// always does.
    fn gated_ctx(tag: &str, approver: Arc<dyn Approver>) -> (ToolCtx, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!("kloop-plan216-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let ctx = test_ctx(0, tag);
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = dir.clone();
        cfg.permissions = Arc::new(
            Permissions::new(
                Mode::Manual,
                &PermissionRules::default(),
                dir.clone(),
                Some(approver),
            )
            .unwrap(),
        );
        (
            ToolCtx {
                cfg: Arc::new(cfg),
                ..ctx
            },
            dir,
        )
    }

    fn result(id: &str, content: &str) -> ContentBlock {
        ContentBlock::ToolResult {
            tool_use_id: id.into(),
            content: content.into(),
            is_error: true,
        }
    }

    /// Records which calls started.
    #[derive(Default)]
    struct Started(std::sync::Mutex<Vec<String>>);

    impl Ui for Started {
        fn emit(&self, ev: &Event) {
            if let Event::ItemStarted { id, .. } = ev {
                self.0.lock().unwrap().push(id.clone());
            }
        }
    }

    /// The No answers its own call and stops the turn; nothing after it in the
    /// batch runs, including a write that would never have asked — and none
    /// of those even starts (no row, no pre-tool hook).
    #[tokio::test]
    async fn a_no_leaves_the_rest_of_the_batch_unrun() {
        let approver = Arc::new(Answering {
            decision: Decision::Stop,
            asked: AtomicUsize::new(0),
        });
        let (mut ctx, dir) = gated_ctx("batch", approver.clone());
        let started = Arc::new(Started::default());
        ctx.ui = started.clone();
        let results = dispatch_tools(
            vec![
                (
                    "b1".into(),
                    "bash".into(),
                    json!({"command": "printf x > one.txt"}),
                ),
                (
                    "w1".into(),
                    "write_file".into(),
                    json!({"path": dir.join("two.txt"), "content": "x"}),
                ),
                (
                    "b2".into(),
                    "bash".into(),
                    json!({"command": "printf y > three.txt"}),
                ),
            ],
            &ctx,
        )
        .await;
        assert_eq!(
            results,
            vec![
                result("b1", &user_stop("bash")),
                result("w1", NOT_RUN_STOPPED),
                result("b2", NOT_RUN_STOPPED),
            ]
        );
        assert_eq!(approver.asked.load(Ordering::SeqCst), 1, "asked only once");
        assert_eq!(*started.0.lock().unwrap(), vec!["b1".to_string()]);
        assert!(ctx.stop.is_cancelled());
        assert!(!ctx.cancel.is_cancelled(), "a stop is not an interrupt");
        assert!(!dir.join("two.txt").exists());
    }

    /// A plain Deny — what headless answers — refuses the call and nothing
    /// more: the turn goes on and the next call in the batch runs.
    #[tokio::test]
    async fn a_deny_refuses_one_call_and_the_batch_goes_on() {
        let approver = Arc::new(Answering {
            decision: Decision::Deny,
            asked: AtomicUsize::new(0),
        });
        let (ctx, dir) = gated_ctx("deny", approver.clone());
        let results = dispatch_tools(
            vec![
                (
                    "b1".into(),
                    "bash".into(),
                    json!({"command": "printf x > one.txt"}),
                ),
                (
                    "w1".into(),
                    "write_file".into(),
                    json!({"path": dir.join("two.txt"), "content": "x"}),
                ),
            ],
            &ctx,
        )
        .await;
        assert_eq!(
            results[0],
            result(
                "b1",
                "The user declined this bash call. Do not retry the same call; take a different approach, or ask the user how to proceed."
            )
        );
        assert!(
            matches!(
                &results[1],
                ContentBlock::ToolResult {
                    is_error: false,
                    ..
                }
            ),
            "{results:?}"
        );
        assert!(!ctx.stop.is_cancelled());
        assert!(dir.join("two.txt").exists());
    }

    /// An approval still waiting when another call's No stops the turn is
    /// withdrawn: the call does not run, and the reply channel closes, which
    /// is how a front-end knows to take the question down.
    #[tokio::test]
    async fn a_waiting_approval_is_withdrawn_when_the_turn_stops() {
        let (asks, mut asked) = mpsc::unbounded_channel();
        let (ctx, _dir) = gated_ctx("withdraw", Arc::new(Handing(asks)));
        let call = tokio::spawn({
            let ctx = ctx.clone();
            async move { run_tool("bash", json!({"command": "printf x > f.txt"}), &ctx).await }
        });
        let reply = asked.recv().await.expect("the call asks");
        assert!(!reply.is_closed());

        ctx.stop.cancel();
        let settled = tokio::time::timeout(Duration::from_secs(5), call)
            .await
            .expect("the stop withdraws the wait instead of leaving it hanging");
        assert_eq!(settled.unwrap(), (NOT_RUN_STOPPED.to_string(), true));
        assert!(reply.is_closed(), "nobody is waiting for this answer now");
    }

    /// The sub-agent and the Program share one cap and one suffix, so a failure
    /// reads the same whichever producer reinjected it.
    #[test]
    fn clip_error_caps_only_long_failures() {
        assert_eq!(clip_error("short"), "short");
        let long = "x".repeat(MAX_REINJECT_ERROR_CHARS + 500);
        let out = clip_error(&long);
        assert!(out.ends_with("… (error truncated)"));
        assert!(out.chars().count() < long.chars().count());
    }
}
