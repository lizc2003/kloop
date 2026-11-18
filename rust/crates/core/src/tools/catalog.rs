//! The tool catalog: what a request offers, what is withheld behind
//! `tool_search`, and the name rules that keep external sources off built-ins.
//!
//! Every function here answers a question about the set of tools. None of them
//! executes a call — that is [`super::dispatch`]'s job.

use std::sync::Arc;

use kloop_protocol::ToolDef;
use serde_json::Value;

use super::Builtin;
use super::SourceCallBinding;
use super::SourceDefinitionSnapshot;
use super::SourceDefinitionState;
use super::SourceRouteState;
use super::SourceVersion;
use super::ToolSource;
use super::builtin;
use super::tool_search;
use crate::shell_programs::ShellPrograms;

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
    let catalog = builtin_defs(depth, shell_programs);
    let (inline_sources, deferred) =
        partition_source_defs(sources, defer_threshold, shell_programs);
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
    let surface_defs = if depth == 0 {
        // Only run_program's definition reads the catalogs, and that surface is
        // off by default — rebuilding three dozen schemas for a request that
        // does not offer it would be pure waste. When it is on, `catalog` is
        // already exactly the depth-0 built-ins the Program API lists.
        let no_builtins: Vec<ToolDef> = Vec::new();
        let cx = builtin::DefCx {
            builtins: if surface.program {
                &catalog
            } else {
                &no_builtins
            },
            inline_sources: &inline_sources,
            deferred_sources: &deferred,
        };
        builtin::ALL
            .iter()
            .filter(|tool| tool.in_surface(surface))
            .map(|tool| tool.def(&cx))
            .collect()
    } else {
        Vec::new()
    };
    let mut defs = catalog;
    if !deferred.is_empty() {
        defs.push(tool_search::tool_search_def());
        defs.push(tool_search::call_tool_def());
    }
    defs.extend(inline_sources);
    defs.extend(surface_defs);
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

/// Whether an external source offers `name` and the merge withholds it behind
/// tool_search. `None` means no source offers it; a built-in name is reserved
/// away before the sources are consulted and therefore also answers `None`.
///
/// One name, not the whole split: the dispatch gate asks this for every call,
/// and building both halves used to clone every source's ToolDef — full JSON
/// schemas included — to answer a single lookup.
pub(super) fn source_tool_deferral(
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    shell_programs: &ShellPrograms,
    name: &str,
) -> Option<bool> {
    if reserved_names().contains(name) {
        return None;
    }
    // First source claiming the name wins, mirroring the merge.
    for source in sources {
        let source_defs = source.defs();
        if let Some(def) = source_defs.iter().find(|def| def.name == name) {
            // Past the threshold every merged source tool is deferred, so the
            // per-tool answer is not needed — and the count is what keeps this
            // path from cloning the catalog.
            let over =
                builtin_count(0, shell_programs) + merged_source_count(sources) > defer_threshold;
            return Some(over || source.should_defer(&def.name));
        }
    }
    None
}

/// Whether the request built now carries `name` outside the deferred set, used
/// by `select:` to tell "already loaded" from "unknown".
///
/// The caller has already ruled out the deferred catalog, so any source match
/// here is inline by construction. Built-ins and the deferred-era
/// `tool_search`/`call_tool` pair are decided by name: materializing the defs
/// rebuilt run_program's whole TypeScript API just to compare strings.
pub(super) fn tool_name_is_inline(
    name: &str,
    depth: u8,
    sources: &[Arc<dyn ToolSource>],
    surface: crate::config::SurfaceCapabilities,
    shell_programs: &ShellPrograms,
    deferred_regime: bool,
) -> bool {
    if deferred_regime
        && (name.eq_ignore_ascii_case("tool_search") || name.eq_ignore_ascii_case("call_tool"))
    {
        return true;
    }
    if builtin::ALL.iter().any(|tool| {
        tool.name().eq_ignore_ascii_case(name)
            && (tool.in_catalog(depth, shell_programs) || (depth == 0 && tool.in_surface(surface)))
    }) {
        return true;
    }
    sources.iter().any(|source| {
        source
            .defs()
            .iter()
            .any(|def| def.name.eq_ignore_ascii_case(name) && !reserved_names().contains(&def.name))
    })
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
    if builtin_count(0, shell_programs) + merged.len() > defer_threshold {
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
pub(super) fn reserved_names() -> &'static std::collections::HashSet<String> {
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

pub(super) fn merged_source_defs(sources: &[Arc<dyn ToolSource>]) -> Vec<ToolDef> {
    merge_source_defs(sources).0
}

/// The merged catalog plus one message per dropped definition. Startup
/// diagnostics and the request builder have to agree on what "merged" means,
/// so both come from here.
fn merge_source_defs(sources: &[Arc<dyn ToolSource>]) -> (Vec<ToolDef>, Vec<String>) {
    let mut seen = reserved_names().clone();
    let mut defs = Vec::new();
    let mut collisions = Vec::new();
    for source in sources {
        let source_defs = source.defs();
        for def in source_defs.iter() {
            if seen.insert(def.name.clone()) {
                defs.push(def.clone());
            } else {
                collisions.push(format!(
                    "tool name collision: '{}' is already registered; the later definition is skipped",
                    def.name
                ));
            }
        }
    }
    (defs, collisions)
}

/// How many definitions the merge would produce, without cloning any of them.
/// The defer threshold only needs the count.
fn merged_source_count(sources: &[Arc<dyn ToolSource>]) -> usize {
    let mut seen = reserved_names().clone();
    let mut count = 0;
    for source in sources {
        let source_defs = source.defs();
        for def in source_defs.iter() {
            if seen.insert(def.name.clone()) {
                count += 1;
            }
        }
    }
    count
}

/// Startup diagnostics for the merged tool set: name collisions (the later
/// definition is skipped) and a note when the deferred-tools regime kicked
/// in. Depth 0 is the authoritative view (it has the most built-ins).
pub fn tool_merge_warnings(
    sources: &[Arc<dyn ToolSource>],
    defer_threshold: usize,
    shell_programs: &ShellPrograms,
) -> Vec<String> {
    let (merged, mut warnings) = merge_source_defs(sources);
    let total = builtin_count(0, shell_programs) + merged.len();
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

pub(super) fn find_source<'a>(
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

pub(super) fn source_definition_result(
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

pub(super) fn source_definition_snapshot(
    sources: &[Arc<dyn ToolSource>],
    name: &str,
) -> Option<SourceDefinitionSnapshot> {
    source_definition_result(sources, name).ok().flatten()
}

pub(super) fn source_route_state(sources: &[Arc<dyn ToolSource>], name: &str) -> SourceRouteState {
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
pub(super) fn builtin_defs(depth: u8, shell_programs: &ShellPrograms) -> Vec<ToolDef> {
    #[cfg(test)]
    BUILTIN_DEFS_BUILDS.with(|builds| builds.set(builds.get() + 1));
    let cx = builtin::DefCx::default();
    builtin::ALL
        .iter()
        .filter(|tool| tool.in_catalog(depth, shell_programs))
        .map(|tool| tool.def(&cx))
        .collect()
}

/// How many built-ins a request at `depth` carries in its catalog. Counting
/// walks the same gate as [`builtin_defs`] but builds no schemas.
fn builtin_count(depth: u8, shell_programs: &ShellPrograms) -> usize {
    builtin::ALL
        .iter()
        .filter(|tool| tool.in_catalog(depth, shell_programs))
        .count()
}

// Thread-local: tests in one binary run in parallel, and a shared counter
// would count another test's catalog build.
#[cfg(test)]
thread_local! {
    static BUILTIN_DEFS_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn builtin_defs_builds() -> usize {
    BUILTIN_DEFS_BUILDS.with(std::cell::Cell::get)
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
    is_concurrency_safe_with(name, input, sources, None)
}

/// The batching decision over a parse dispatch already made for bash. Passing
/// the same facts here is what keeps one call's command analysed once: the
/// batch loop, the gate and execution all read this one value.
pub(super) fn is_concurrency_safe_with(
    name: &str,
    input: &Value,
    sources: &[Arc<dyn ToolSource>],
    facts: Option<&crate::shell::BashFacts>,
) -> bool {
    match Builtin::from_name(name) {
        Some(builtin) => builtin.concurrency_safe_with(input, facts),
        None => find_source(sources, name).is_some_and(|source| source.is_readonly(name)),
    }
}

/// Parse a bash call's command for the whole call. Every other tool has no
/// shell analysis to share, and a bash call without a string command is
/// rejected by its own parser later.
pub(super) fn analyze_shell_call(
    name: &str,
    input: &Value,
) -> Option<Arc<crate::shell::BashFacts>> {
    if name != "bash" {
        return None;
    }
    input["command"]
        .as_str()
        .map(|command| Arc::new(crate::shell::analyze_bash_facts(command)))
}

/// Whether a call may change something outside the conversation, so that a
/// crash while it ran leaves the world in an unknown state (plan 204).
/// Concurrency safety is almost that question, except for the orchestrators:
/// they are batched as safe because their children re-enter the gate, but
/// those children write and run things all the same.
pub(super) fn may_have_effects(
    name: &str,
    input: &Value,
    sources: &[Arc<dyn ToolSource>],
    facts: Option<&crate::shell::BashFacts>,
) -> bool {
    !is_concurrency_safe_with(name, input, sources, facts)
        || matches!(
            Builtin::from_name(name),
            Some(Builtin::RunAgent | Builtin::Workflow | Builtin::RunProgram)
        )
}
