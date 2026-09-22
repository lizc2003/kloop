//! Deferred-tool discovery: the tool_search tool, the injected notice that
//! tells the model which tools exist but are not loaded, and the dispatch
//! gate for locked tools. An unlock is a session-memory capability receipt
//! bound to one source snapshot, effective workspace, permission epoch, and
//! agent authority. Catalog or scope changes invalidate it; the provider tool
//! array does not change merely because a tool was unlocked.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;

use anyhow::Result;
use anyhow::bail;
use serde_json::Value;
use serde_json::json;

use super::SourceCallBinding;
use super::SourceRouteState;
use super::ToolCtx;
use super::deferred_tool_defs;
use super::str_arg;
use crate::config::Config;
use crate::config::EffectiveWorkspace;
use crate::permissions::PermissionCapabilityEpoch;
use kloop_protocol::LocalAgentId;
use kloop_protocol::ToolDef;

const DEFAULT_MAX_RESULTS: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct CapabilityBinding {
    workspace_id: crate::project::WorkspaceId,
    workspace_cwd: PathBuf,
    workspace_epoch: u64,
    permission_epoch: PermissionCapabilityEpoch,
    agent_id: LocalAgentId,
    depth: u8,
    tool_allowlist: Option<Vec<String>>,
}

impl CapabilityBinding {
    fn capture(ctx: &ToolCtx, workspace: &EffectiveWorkspace) -> Self {
        let tool_allowlist = ctx.cfg.tool_allowlist.as_ref().map(|allowlist| {
            let mut names: Vec<String> = allowlist.iter().cloned().collect();
            names.sort();
            names
        });
        Self {
            workspace_id: workspace.identity.workspace_id().clone(),
            workspace_cwd: workspace.cwd.clone(),
            workspace_epoch: workspace.workspace_epoch,
            permission_epoch: workspace.permissions.capability_epoch(),
            agent_id: ctx.cfg.agent_id().clone(),
            depth: ctx.depth,
            tool_allowlist,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct UnlockReceipt {
    tool_name: String,
    source: SourceCallBinding,
    capability: CapabilityBinding,
}

/// Session-memory capability receipts for deferred tools. The public façade is
/// needed only because external crates still construct Config directly; receipt
/// fields remain core-private and never enter protocol, history, or rollout.
#[derive(Default)]
pub struct DeferredToolUnlocks {
    receipts: RwLock<HashMap<(String, LocalAgentId, u8), UnlockReceipt>>,
}

impl DeferredToolUnlocks {
    fn key(receipt: &UnlockReceipt) -> (String, LocalAgentId, u8) {
        (
            receipt.tool_name.clone(),
            receipt.capability.agent_id.clone(),
            receipt.capability.depth,
        )
    }

    fn record(&self, receipt: UnlockReceipt) {
        self.receipts
            .write()
            .unwrap()
            .insert(Self::key(&receipt), receipt);
    }

    fn contains(&self, receipt: &UnlockReceipt) -> bool {
        self.receipts.read().unwrap().get(&Self::key(receipt)) == Some(receipt)
    }

    pub(crate) fn clear(&self) {
        self.receipts.write().unwrap().clear();
    }

    #[cfg(test)]
    fn tool_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .receipts
            .read()
            .unwrap()
            .values()
            .map(|receipt| receipt.tool_name.clone())
            .collect();
        names.sort();
        names.dedup();
        names
    }

    #[cfg(test)]
    fn replace_for_test(&self, receipt: UnlockReceipt) {
        let mut receipts = self.receipts.write().unwrap();
        receipts.clear();
        receipts.insert(Self::key(&receipt), receipt);
    }

    #[cfg(test)]
    fn receipts(&self) -> Vec<UnlockReceipt> {
        self.receipts.read().unwrap().values().cloned().collect()
    }
}

pub(super) fn tool_search_def() -> ToolDef {
    ToolDef {
        name: "tool_search".into(),
        description: "Search deferred tools and load their full definitions. Use \"select:<name>[,<name>...]\" for exact tools, or keywords (prefix a required term with `+`). After loading, invoke the tool through call_tool; direct calls remain a compatibility optimization for providers that permit undeclared names.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "\"select:<name>[,<name>...]\" for exact selection, or keywords"},
                "max_results": {"type": "integer", "minimum": 1, "description": "Max keyword matches returned (default 5)"}
            },
            "additionalProperties": false,
            "required": ["query"]
        }),
    }
}

/// Escape hatch for models that refuse to emit tool calls for names absent
/// from their declared tool list (observed: gpt-5.4-mini will search and
/// unlock but never direct-call). Dispatch unwraps the envelope up front, so
/// hooks, permissions, concurrency and the UI all see the inner tool name —
/// this wrapper never reaches the gates itself.
pub(super) fn call_tool_def() -> ToolDef {
    ToolDef {
        name: "call_tool".into(),
        description: "Invoke a tool whose definition was loaded via tool_search. This is the standard deferred-tool execution path because it works with providers that reject direct calls to names absent from the original tool array. The inner tool still passes through its normal hooks, permission and concurrency checks.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "tool_name": {"type": "string", "description": "Name of the loaded tool to invoke"},
                "params": {"type": "object", "description": "Arguments for that tool, matching its returned schema"}
            },
            "additionalProperties": false,
            "required": ["tool_name"]
        }),
    }
}

/// Rewrite a `call_tool` envelope into the inner call it wraps; every other
/// call passes through untouched. Runs at the top of dispatch, before
/// concurrency batching. A malformed envelope (missing tool_name) is left
/// as-is so the call_tool arm of execute_tool can report usage.
pub(super) fn unwrap_call_tool(name: String, input: Value) -> (String, Value) {
    if name != "call_tool" {
        return (name, input);
    }
    match input["tool_name"].as_str() {
        Some(inner) => {
            let params = match &input["params"] {
                Value::Null => json!({}),
                params => params.clone(),
            };
            (inner.to_string(), params)
        }
        None => (name, input),
    }
}

/// The synthetic-context block announcing deferred tools. Lists every
/// deferred name in the current source snapshot regardless of unlock state,
/// so searching does not perturb the prompt-cache prefix. A dynamic catalog
/// refresh may replace the list at the next sampling round. None when deferral
/// is inactive.
pub fn deferred_notice(cfg: &Config) -> Option<String> {
    let defs = deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold, &cfg.shell_programs);
    if defs.is_empty() {
        return None;
    }
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    Some(format!(
        "<system-reminder>\nThe following tools exist but are deferred — their definitions are not loaded and calling them before loading fails:\n{}\nTo use one, first call tool_search (query \"select:<name>\" for an exact pick, or keywords to search), then invoke it through call_tool with the returned schema. A direct call by the loaded tool's own name is only a provider-compatibility optimization.\n</system-reminder>",
        names.join("\n")
    ))
}

pub(super) fn is_deferred(name: &str, cfg: &Config) -> bool {
    deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold, &cfg.shell_programs)
        .iter()
        .any(|definition| definition.name == name)
}

fn receipt_for_capability(
    name: &str,
    source: SourceCallBinding,
    capability: &CapabilityBinding,
) -> UnlockReceipt {
    UnlockReceipt {
        tool_name: name.to_string(),
        source,
        capability: capability.clone(),
    }
}

pub(super) fn current_source_route(name: &str, cfg: &Config) -> SourceRouteState {
    super::source_route_state(&cfg.tool_sources, name)
}

pub(super) fn current_source_binding(name: &str, cfg: &Config) -> Option<SourceCallBinding> {
    match current_source_route(name, cfg) {
        SourceRouteState::Available(binding) => Some(binding),
        SourceRouteState::Missing | SourceRouteState::Unavailable(_) => None,
    }
}

pub(super) fn unavailable_source_reason(name: &str, cfg: &Config) -> Option<String> {
    match current_source_route(name, cfg) {
        SourceRouteState::Unavailable(reason) => Some(reason),
        SourceRouteState::Missing | SourceRouteState::Available(_) => None,
    }
}

/// Return the exact source owner/generation unlocked for this capability scope.
/// A missing receipt is the deferred locked verdict; callers reject it before
/// hooks or permission rather than asking the human about an undiscovered tool.
pub(super) fn unlocked_source_for_dispatch(
    name: &str,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Option<SourceCallBinding> {
    let source = current_source_binding(name, &ctx.cfg)?;
    let capability = CapabilityBinding::capture(ctx, workspace);
    let receipt = receipt_for_capability(name, source, &capability);
    ctx.cfg.unlocked_tools.contains(&receipt).then_some(source)
}

fn max_results(input: &Value) -> Result<usize> {
    match input.get("max_results") {
        None => Ok(DEFAULT_MAX_RESULTS),
        Some(Value::Number(value)) => value
            .as_u64()
            .and_then(|value| usize::try_from(value).ok())
            .filter(|value| *value > 0)
            .ok_or_else(|| anyhow::anyhow!("tool_search: max_results must be a positive integer")),
        Some(_) => bail!("tool_search: max_results must be a positive integer"),
    }
}

pub(super) async fn tool_search_tool(
    input: &Value,
    ctx: &ToolCtx,
    workspace: &EffectiveWorkspace,
) -> Result<String> {
    let query = str_arg(input, "query", "tool_search")?.trim().to_string();
    if query.is_empty() {
        bail!("tool_search: query must not be empty");
    }
    let deferred = deferred_tool_defs(
        &ctx.cfg.tool_sources,
        ctx.cfg.defer_threshold,
        &ctx.cfg.shell_programs,
    );
    let max_results = max_results(input)?;

    let mut found: Vec<ToolDef> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if query
        .get(.."select:".len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("select:"))
    {
        let rest = &query["select:".len()..];
        let loaded = super::all_tool_defs(
            ctx.depth,
            &ctx.cfg.tool_sources,
            ctx.cfg.defer_threshold,
            ctx.cfg.surface,
            &ctx.cfg.shell_programs,
        );
        let mut selected = std::collections::HashSet::new();
        let mut selected_any = false;
        for name in rest
            .split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
        {
            selected_any = true;
            if let Some(def) = deferred
                .iter()
                .find(|def| def.name.eq_ignore_ascii_case(name))
            {
                if selected.insert(def.name.clone()) {
                    found.push(def.clone());
                }
            } else if loaded.iter().any(|def| def.name.eq_ignore_ascii_case(name)) {
                notes.push(format!("'{name}' is already loaded; call it directly"));
            } else if let Some(reason) = unavailable_source_reason(name, &ctx.cfg) {
                notes.push(reason);
            } else {
                notes.push(format!("no deferred tool named '{name}'"));
            }
        }
        if !selected_any {
            bail!("tool_search: select: requires at least one tool name");
        }
    } else if let Some(def) = deferred
        .iter()
        .find(|def| def.name.eq_ignore_ascii_case(&query))
    {
        found.push(def.clone());
    } else if let Some(reason) = unavailable_source_reason(&query, &ctx.cfg) {
        notes.push(reason);
    } else {
        let query_lower = query.to_lowercase();
        let prefix_matches: Vec<ToolDef> = deferred
            .iter()
            .filter(|def| def.name.to_lowercase().starts_with(&query_lower))
            .take(max_results)
            .cloned()
            .collect();
        if query_lower.contains("__") && !prefix_matches.is_empty() {
            found = prefix_matches;
        } else {
            let terms: Vec<SearchTerm> = query
                .split_whitespace()
                .filter_map(SearchTerm::parse)
                .collect();
            let mut scored: Vec<(u32, &ToolDef)> = deferred
                .iter()
                .filter_map(|def| keyword_score(def, &terms).map(|score| (score, def)))
                .filter(|(score, _)| *score > 0)
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
            found.extend(
                scored
                    .into_iter()
                    .take(max_results)
                    .map(|(_, def)| def.clone()),
            );
        }
    }

    let capability = CapabilityBinding::capture(ctx, workspace);
    let mut published = Vec::new();
    for definition in found {
        match super::source_definition_result(&ctx.cfg.tool_sources, &definition.name) {
            Ok(Some(snapshot)) => {
                ctx.cfg.unlocked_tools.record(receipt_for_capability(
                    &snapshot.definition.name,
                    snapshot.binding,
                    &capability,
                ));
                published.push(snapshot.definition);
            }
            Ok(None) => notes.push(format!(
                "tool '{}' changed while search results were being published; search again",
                definition.name
            )),
            Err(reason) => notes.push(reason),
        }
    }
    Ok(render(&published, &notes, deferred.len()))
}

#[derive(Debug)]
struct SearchTerm {
    value: String,
    required: bool,
}

impl SearchTerm {
    fn parse(raw: &str) -> Option<Self> {
        let (required, value) = match raw.strip_prefix('+') {
            Some(value) => (true, value),
            None => (false, raw),
        };
        (!value.is_empty()).then(|| SearchTerm {
            value: value.to_lowercase(),
            required,
        })
    }
}

fn keyword_score(def: &ToolDef, terms: &[SearchTerm]) -> Option<u32> {
    let name = def.name.to_lowercase();
    let normalized_name = name.replace(['_', '-'], " ");
    let name_tokens: Vec<&str> = normalized_name.split_whitespace().collect();
    let description = def.description.to_lowercase();
    let schema = def.schema.to_string().to_lowercase();
    let mut score = 0;
    for term in terms {
        let name_exact = name_tokens.contains(&term.value.as_str());
        let name_substring = name_tokens
            .iter()
            .any(|token| token.contains(term.value.as_str()));
        let description_hit = description.contains(term.value.as_str());
        let schema_hit = schema.contains(term.value.as_str());
        if term.required && !(name_substring || description_hit || schema_hit) {
            return None;
        }
        score += if name_exact {
            10
        } else if name_substring || name.contains(term.value.as_str()) {
            5
        } else if description_hit {
            2
        } else if schema_hit {
            1
        } else {
            0
        };
    }
    Some(score)
}

fn render(found: &[ToolDef], notes: &[String], total_deferred: usize) -> String {
    let mut out = String::new();
    if found.is_empty() && notes.is_empty() {
        return format!(
            "No matching deferred tools found ({total_deferred} deferred tools exist; their names are listed in the context)."
        );
    }
    if !found.is_empty() {
        out.push_str(&format!(
            "Found {} tool(s); they are now loaded. Invoke each through call_tool({{\"tool_name\": \"<name>\", \"params\": {{...}}}}) using the schema below. A direct call by the loaded tool's own name is also accepted when the provider permits it:\n",
            found.len()
        ));
        for def in found {
            out.push_str(&format!(
                "\n## {}\n{}\nparameters (JSON schema): {}\n",
                def.name, def.description, def.schema
            ));
        }
    }
    for note in notes {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(note);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::history::History;
    use crate::tools::ToolSource;
    use crate::tools::testutil::*;
    use kloop_protocol::AssistantBlock;
    use kloop_protocol::ContentBlock;
    use kloop_protocol::Message;
    use kloop_provider::MockTurn;
    use kloop_provider::Provider;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    /// Two deferred-able tools with distinct names/descriptions so keyword
    /// scoring is observable. Calls echo the tool name.
    struct Srv {
        defs: Vec<ToolDef>,
    }

    fn srv() -> Arc<dyn ToolSource> {
        let def = |name: &str, description: &str| ToolDef {
            name: name.into(),
            description: description.into(),
            schema: json!({"type": "object", "properties": {"q": {"type": "string"}}}),
        };
        Arc::new(Srv {
            defs: vec![
                def("srv__web_search", "Query a search engine for pages"),
                def("srv__page_fetch", "Fetch a web page; supports search terms"),
            ],
        })
    }

    impl ToolSource for Srv {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(self.defs.clone())
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move { Ok(crate::tools::SourceOutput::text(format!("ran {tool}"))) })
        }
    }

    struct ChangingSrv {
        defs: std::sync::RwLock<Vec<ToolDef>>,
        generation: std::sync::atomic::AtomicU64,
    }

    impl ChangingSrv {
        fn new(field: &str) -> Self {
            Self {
                defs: std::sync::RwLock::new(vec![Self::definition(field)]),
                generation: std::sync::atomic::AtomicU64::new(0),
            }
        }

        fn definition(field: &str) -> ToolDef {
            ToolDef {
                name: "srv__changing".into(),
                description: "A tool whose schema changes".into(),
                schema: json!({
                    "type": "object",
                    "properties": {field: {"type": "string"}},
                    "required": [field]
                }),
            }
        }

        fn replace_schema(&self, field: &str) {
            *self.defs.write().unwrap() = vec![Self::definition(field)];
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    impl ToolSource for ChangingSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(self.defs.read().unwrap().clone())
        }

        fn definition_generation(&self, _tool: &str) -> u64 {
            self.generation.load(std::sync::atomic::Ordering::Acquire)
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move { Ok(crate::tools::SourceOutput::text(format!("ran {tool}"))) })
        }
    }

    struct ReadinessSrv {
        revision: std::sync::atomic::AtomicU64,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl ToolSource for ReadinessSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(vec![ToolDef {
                name: "srv__readiness".into(),
                description: "A tool whose readiness revision changes".into(),
                schema: json!({"type": "object"}),
            }])
        }

        fn readiness_revision(&self, _tool: &str) -> u64 {
            self.revision.load(std::sync::atomic::Ordering::Acquire)
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(crate::tools::SourceOutput::text("ready call".into()))
            })
        }
    }

    struct UnavailableDuringPublishSrv;

    impl ToolSource for UnavailableDuringPublishSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(vec![ToolDef {
                name: "srv__publish_race".into(),
                description: "becomes unavailable while publishing".into(),
                schema: json!({"type": "object"}),
            }])
        }

        fn definition_state(&self, tool: &str) -> crate::tools::SourceDefinitionState {
            if tool == "srv__publish_race" {
                crate::tools::SourceDefinitionState::Unavailable {
                    reason: "MCP server is stale: unavailable, not missing".into(),
                }
            } else {
                crate::tools::SourceDefinitionState::Missing
            }
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async { unreachable!("unavailable source must not be called") })
        }
    }

    struct SnapshotRaceSrv {
        defs_calls: std::sync::atomic::AtomicUsize,
    }

    impl SnapshotRaceSrv {
        fn definition(field: &str) -> ToolDef {
            ToolDef {
                name: "srv__racing".into(),
                description: "A tool refreshed during search".into(),
                schema: json!({
                    "type": "object",
                    "properties": {field: {"type": "string"}},
                    "required": [field]
                }),
            }
        }
    }

    impl ToolSource for SnapshotRaceSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            let call = self
                .defs_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Arc::from(vec![Self::definition(if call == 0 {
                "old"
            } else {
                "new"
            })])
        }

        fn definition_generation(&self, _tool: &str) -> u64 {
            1
        }

        fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
            (tool == "srv__racing").then(|| (Self::definition("new"), 1))
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move { Ok(crate::tools::SourceOutput::text(format!("ran {tool}"))) })
        }
    }

    struct AppearingSrv {
        defs_calls: std::sync::atomic::AtomicUsize,
        calls: std::sync::atomic::AtomicUsize,
    }

    impl AppearingSrv {
        fn definition() -> ToolDef {
            ToolDef {
                name: "srv__appearing".into(),
                description: "Appears during dispatch classification".into(),
                schema: json!({"type": "object"}),
            }
        }
    }

    impl ToolSource for AppearingSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            let call = self
                .defs_calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if call < 2 {
                Arc::from(Vec::<ToolDef>::new())
            } else {
                Arc::from(vec![Self::definition()])
            }
        }

        fn definition_generation(&self, _tool: &str) -> u64 {
            1
        }

        fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
            (tool == "srv__appearing").then(|| (Self::definition(), 1))
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            true
        }

        fn call<'a>(
            &'a self,
            _tool: &'a str,
            _input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(crate::tools::SourceOutput::text("unexpected call".into()))
            })
        }
    }

    struct OwnedSrv {
        definition: std::sync::RwLock<Option<ToolDef>>,
        generation: std::sync::atomic::AtomicU64,
        label: &'static str,
        calls: std::sync::atomic::AtomicUsize,
        seen_generations: std::sync::Mutex<Vec<Option<u64>>>,
    }

    impl OwnedSrv {
        fn new(name: &str, label: &'static str) -> Self {
            Self {
                definition: std::sync::RwLock::new(Some(ToolDef {
                    name: name.into(),
                    description: format!("owned by {label}"),
                    schema: json!({"type": "object"}),
                })),
                generation: std::sync::atomic::AtomicU64::new(0),
                label,
                calls: std::sync::atomic::AtomicUsize::new(0),
                seen_generations: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn remove_definition(&self) {
            *self.definition.write().unwrap() = None;
            self.generation
                .fetch_add(1, std::sync::atomic::Ordering::Release);
        }
    }

    impl ToolSource for OwnedSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(
                self.definition
                    .read()
                    .unwrap()
                    .iter()
                    .cloned()
                    .collect::<Vec<_>>(),
            )
        }

        fn definition_generation(&self, _tool: &str) -> u64 {
            self.generation.load(std::sync::atomic::Ordering::Acquire)
        }

        fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
            let definition = self.definition.read().unwrap();
            definition
                .as_ref()
                .filter(|definition| definition.name == tool)
                .cloned()
                .map(|definition| (definition, self.definition_generation(tool)))
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            self.call_at_generation(tool, input, None)
        }

        fn call_at_generation<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
            generation: Option<u64>,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.seen_generations.lock().unwrap().push(generation);
                let current = self.definition_generation(tool);
                if generation.is_some_and(|generation| generation != current) {
                    bail!("stale generation for {tool}");
                }
                if self
                    .definition
                    .read()
                    .unwrap()
                    .as_ref()
                    .is_none_or(|definition| definition.name != tool)
                {
                    bail!("owner no longer advertises {tool}");
                }
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(crate::tools::SourceOutput::text(self.label.into()))
            })
        }
    }

    struct WireRaceSrv {
        definition: ToolDef,
        generation: std::sync::atomic::AtomicU64,
        readiness_revision: std::sync::atomic::AtomicU64,
        calls: std::sync::atomic::AtomicUsize,
        barrier: Arc<tokio::sync::Barrier>,
    }

    impl ToolSource for WireRaceSrv {
        fn defs(&self) -> Arc<[ToolDef]> {
            Arc::from(vec![self.definition.clone()])
        }

        fn definition_generation(&self, _tool: &str) -> u64 {
            self.generation.load(std::sync::atomic::Ordering::Acquire)
        }

        fn definition_snapshot(&self, tool: &str) -> Option<(ToolDef, u64)> {
            (tool == self.definition.name)
                .then(|| (self.definition.clone(), self.definition_generation(tool)))
        }

        fn readiness_revision(&self, _tool: &str) -> u64 {
            self.readiness_revision
                .load(std::sync::atomic::Ordering::Acquire)
        }

        fn is_readonly(&self, _tool: &str) -> bool {
            false
        }

        fn call<'a>(
            &'a self,
            tool: &'a str,
            input: &'a Value,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            self.call_at_version(tool, input, None)
        }

        fn call_at_version<'a>(
            &'a self,
            tool: &'a str,
            _input: &'a Value,
            version: Option<crate::tools::SourceVersion>,
        ) -> Pin<Box<dyn Future<Output = Result<crate::tools::SourceOutput>> + Send + 'a>> {
            Box::pin(async move {
                self.barrier.wait().await;
                self.barrier.wait().await;
                let current = crate::tools::SourceVersion {
                    definition_generation: self.definition_generation(tool),
                    readiness_revision: self.readiness_revision(tool),
                };
                let Some(expected) = version else {
                    bail!("wire call is missing a source version for {tool}");
                };
                if expected.definition_generation != current.definition_generation {
                    bail!("wire rejected stale generation for {tool}");
                }
                if expected.readiness_revision != current.readiness_revision {
                    bail!("wire rejected stale readiness for {tool}");
                }
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                Ok(crate::tools::SourceOutput::text("wire call".into()))
            })
        }
    }

    struct WorkspaceSessionApprover {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl crate::permissions::Approver for WorkspaceSessionApprover {
        fn confirm(
            &self,
            _request: crate::permissions::ConfirmRequest,
        ) -> Pin<Box<dyn Future<Output = crate::permissions::Decision> + Send + '_>> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Box::pin(async {
                crate::permissions::Decision::Allow(
                    crate::permissions::ApprovalScope::WorkspaceSession,
                )
            })
        }
    }

    struct NoopProjectWriter;

    impl crate::permissions::ProjectPermissionWriter for NoopProjectWriter {
        fn append_allow(
            &self,
            _project_id: crate::project::ProjectId,
            _additions: crate::permissions::ProjectAllowRules,
        ) -> Pin<
            Box<
                dyn Future<
                        Output = std::result::Result<
                            crate::permissions::ProjectPolicySnapshot,
                            crate::permissions::ProjectPolicyStoreError,
                        >,
                    > + Send
                    + '_,
            >,
        > {
            Box::pin(async { unreachable!("writer is not used by this test") })
        }
    }

    fn deferred_ctx(tag: &str) -> crate::tools::ToolCtx {
        with_defer_threshold(test_ctx_with_sources(0, tag, vec![srv()]), 0)
    }

    fn deferred_ctx_with_source(tag: &str, source: Arc<dyn ToolSource>) -> crate::tools::ToolCtx {
        with_defer_threshold(test_ctx_with_sources(0, tag, vec![source]), 0)
    }

    async fn search_select(ctx: &crate::tools::ToolCtx, name: &str) -> String {
        let (output, is_error) = run_tool(
            "tool_search",
            json!({"query": format!("select:{name}")}),
            ctx,
        )
        .await;
        assert!(!is_error, "{output}");
        output
    }

    #[derive(Clone, Copy)]
    enum WireRacePath {
        Discovered,
        Program,
    }

    #[derive(Clone, Copy)]
    enum WireRaceAxis {
        Definition,
        Readiness,
    }

    impl WireRaceAxis {
        fn advance(self, source: &WireRaceSrv) {
            match self {
                Self::Definition => &source.generation,
                Self::Readiness => &source.readiness_revision,
            }
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        }

        fn error_fragment(self) -> &'static str {
            match self {
                Self::Definition => "wire rejected stale generation",
                Self::Readiness => "wire rejected stale readiness",
            }
        }
    }

    async fn assert_wire_refresh_race(
        tag: &str,
        name: &str,
        path: WireRacePath,
        axis: WireRaceAxis,
    ) {
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let source = Arc::new(WireRaceSrv {
            definition: ToolDef {
                name: name.into(),
                description: "refreshes after dispatch".into(),
                schema: json!({"type": "object"}),
            },
            generation: std::sync::atomic::AtomicU64::new(0),
            readiness_revision: std::sync::atomic::AtomicU64::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
            barrier: Arc::clone(&barrier),
        });
        let mut ctx = deferred_ctx_with_source(tag, source.clone());
        match path {
            WireRacePath::Discovered => {
                search_select(&ctx, name).await;
            }
            WireRacePath::Program => ctx.from_program = true,
        }
        let receipts_before =
            matches!(path, WireRacePath::Discovered).then(|| ctx.cfg.unlocked_tools.receipts());

        let call = tokio::spawn({
            let ctx = ctx.clone();
            let name = name.to_string();
            async move { run_tool(&name, json!({}), &ctx).await }
        });
        barrier.wait().await;
        axis.advance(&source);
        barrier.wait().await;
        let (output, is_error) = call.await.unwrap();
        assert!(is_error, "{output}");
        assert!(output.contains(axis.error_fragment()), "{output}");
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
        if let Some(receipts_before) = receipts_before {
            assert_eq!(
                ctx.cfg.unlocked_tools.receipts(),
                receipts_before,
                "a failed wire call must not mint a replacement receipt"
            );
        }
    }

    fn tool_surface(ctx: &crate::tools::ToolCtx) -> (Vec<ToolDef>, Option<String>) {
        (
            crate::tools::all_tool_defs(
                ctx.depth,
                &ctx.cfg.tool_sources,
                ctx.cfg.defer_threshold,
                ctx.cfg.surface,
                &ctx.cfg.shell_programs,
            ),
            deferred_notice(&ctx.cfg),
        )
    }

    fn unlocked(cfg: &Config) -> Vec<String> {
        cfg.unlocked_tools.tool_names()
    }

    #[tokio::test]
    async fn select_unlocks_and_returns_full_definition() {
        let ctx = deferred_ctx("select");
        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("## srv__web_search"), "{out}");
        assert!(out.contains("Query a search engine for pages"), "{out}");
        assert!(
            out.contains(r#""properties":{"q":{"type":"string"}}"#),
            "{out}"
        );
        assert_eq!(unlocked(&ctx.cfg), vec!["srv__web_search"]);
    }

    #[tokio::test]
    async fn search_returns_schema_and_generation_from_one_source_snapshot() {
        let source: Arc<dyn ToolSource> = Arc::new(SnapshotRaceSrv {
            defs_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = deferred_ctx_with_source("snapshot-race", source);
        let out = search_select(&ctx, "srv__racing").await;
        assert!(out.contains(r#""new""#), "{out}");
        assert!(!out.contains(r#""old""#), "{out}");
        let receipts = ctx.cfg.unlocked_tools.receipts();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].tool_name, "srv__racing");
        assert_eq!(receipts[0].source.version.definition_generation, 1);
    }

    #[tokio::test]
    async fn readiness_revision_invalidates_discovery_without_calling_source() {
        let source = Arc::new(ReadinessSrv {
            revision: std::sync::atomic::AtomicU64::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = deferred_ctx_with_source("readiness-revision", source.clone());
        search_select(&ctx, "srv__readiness").await;
        let receipts = ctx.cfg.unlocked_tools.receipts();
        assert_eq!(receipts[0].source.version.readiness_revision, 0);

        source
            .revision
            .store(1, std::sync::atomic::Ordering::Release);
        let (output, is_error) = run_tool("srv__readiness", json!({}), &ctx).await;
        assert!(is_error, "{output}");
        assert!(output.contains("deferred and not loaded yet"), "{output}");
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::Relaxed), 0);

        search_select(&ctx, "srv__readiness").await;
        let (output, is_error) = run_tool("srv__readiness", json!({}), &ctx).await;
        assert!(!is_error, "{output}");
        assert_eq!(output, "ready call");
    }

    #[tokio::test]
    async fn unavailable_during_publication_is_not_rendered_as_no_match() {
        let ctx = deferred_ctx_with_source(
            "unavailable-publication",
            Arc::new(UnavailableDuringPublishSrv),
        );
        for query in ["select:srv__publish_race", "srv__publish_race"] {
            let (output, is_error) = run_tool("tool_search", json!({"query": query}), &ctx).await;
            assert!(!is_error, "{output}");
            assert!(output.contains("unavailable, not missing"), "{output}");
            assert!(!output.contains("No matching deferred tools"), "{output}");
            assert!(!output.contains("no deferred tool named"), "{output}");
        }
        assert!(ctx.cfg.unlocked_tools.receipts().is_empty());
    }

    #[tokio::test]
    async fn catalog_appearance_during_classification_still_requires_discovery() {
        let source = Arc::new(AppearingSrv {
            defs_calls: std::sync::atomic::AtomicUsize::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let ctx = deferred_ctx_with_source("appearing-source", source.clone());

        let (output, is_error) = run_tool("srv__appearing", json!({}), &ctx).await;
        assert!(is_error, "{output}");
        assert!(output.contains("deferred and not loaded yet"), "{output}");
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn select_is_case_insensitive_and_deduplicates() {
        let ctx = deferred_ctx("select-case");
        let (out, is_error) = run_tool(
            "tool_search",
            json!({
                "query": "SeLeCt:SRV__WEB_SEARCH, srv__web_search, srv__page_fetch"
            }),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert_eq!(out.matches("## srv__web_search").count(), 1, "{out}");
        assert_eq!(out.matches("## srv__page_fetch").count(), 1, "{out}");
        assert_eq!(
            unlocked(&ctx.cfg),
            vec!["srv__page_fetch", "srv__web_search"]
        );
    }

    #[tokio::test]
    async fn keyword_search_supports_required_terms_exact_and_prefix_queries() {
        let ctx = deferred_ctx("keyword-required");
        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "+engine search", "max_results": 5}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("## srv__web_search"), "{out}");
        assert!(!out.contains("## srv__page_fetch"), "{out}");

        let ctx = deferred_ctx("exact-name");
        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "SRV__PAGE_FETCH", "max_results": 5}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("## srv__page_fetch"), "{out}");
        assert!(!out.contains("## srv__web_search"), "{out}");

        let ctx = deferred_ctx("prefix-name");
        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "srv__page", "max_results": 5}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains("## srv__page_fetch"), "{out}");
    }

    #[tokio::test]
    async fn select_reports_loaded_and_unknown_names_without_unlocking() {
        let ctx = deferred_ctx("select-notes");
        let (out, is_error) =
            run_tool("tool_search", json!({"query": "select:bash, nope"}), &ctx).await;
        assert!(!is_error);
        assert!(
            out.contains("'bash' is already loaded; call it directly"),
            "{out}"
        );
        assert!(out.contains("no deferred tool named 'nope'"), "{out}");
        assert_eq!(unlocked(&ctx.cfg), Vec::<String>::new());
    }

    #[tokio::test]
    async fn keyword_search_ranks_name_hits_first_and_caps_results() {
        let ctx = deferred_ctx("keyword");
        // "search" hits web_search's name (10) and page_fetch's description (2).
        let (out, is_error) = run_tool("tool_search", json!({"query": "search"}), &ctx).await;
        assert!(!is_error);
        let web = out.find("## srv__web_search").expect("name hit present");
        let fetch = out
            .find("## srv__page_fetch")
            .expect("description hit present");
        assert!(web < fetch, "name hit must rank first: {out}");
        assert_eq!(
            unlocked(&ctx.cfg),
            vec!["srv__page_fetch", "srv__web_search"]
        );

        // max_results caps to the best hit only.
        let ctx = deferred_ctx("keyword-cap");
        let (out, _) = run_tool(
            "tool_search",
            json!({"query": "search", "max_results": 1}),
            &ctx,
        )
        .await;
        assert!(out.contains("srv__web_search"));
        assert!(!out.contains("## srv__page_fetch"), "{out}");
        assert_eq!(unlocked(&ctx.cfg), vec!["srv__web_search"]);
    }

    #[tokio::test]
    async fn no_match_and_bad_input_shapes() {
        let ctx = deferred_ctx("no-match");
        let (out, is_error) = run_tool("tool_search", json!({"query": "zzz"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(
            out,
            "No matching deferred tools found (2 deferred tools exist; their names are listed in the context)."
        );

        let (out, is_error) = run_tool("tool_search", json!({"query": "  "}), &ctx).await;
        assert!(is_error);
        assert!(out.contains("query must not be empty"), "{out}");

        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "search", "max_results": 0}),
            &ctx,
        )
        .await;
        assert!(is_error);
        assert!(
            out.contains("max_results must be a positive integer"),
            "{out}"
        );

        for invalid in [json!(-1), json!("1"), json!(1.5), Value::Null, json!([])] {
            let (out, is_error) = run_tool(
                "tool_search",
                json!({"query": "search", "max_results": invalid}),
                &ctx,
            )
            .await;
            assert!(is_error, "{out}");
            assert!(
                out.contains("max_results must be a positive integer"),
                "{out}"
            );
        }

        let (out, is_error) = run_tool("tool_search", json!({"query": "select: , "}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(
            out.contains("select: requires at least one tool name"),
            "{out}"
        );
    }

    /// The dispatch gate: a locked deferred tool bounces with guidance and
    /// stays locked — only a tool_search hit unlocks. After the hit the same
    /// call flows through to the source.
    #[tokio::test]
    async fn locked_direct_call_bounces_and_search_opens_the_gate() {
        let ctx = deferred_ctx("gate");
        let (out, is_error) = run_tool("srv__web_search", json!({"q": "x"}), &ctx).await;
        assert!(is_error);
        assert_eq!(
            out,
            "tool 'srv__web_search' is deferred and not loaded yet; call tool_search with query \"select:srv__web_search\" to load its definition, then retry"
        );
        assert_eq!(
            unlocked(&ctx.cfg),
            Vec::<String>::new(),
            "a bounce must not unlock"
        );

        run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &ctx,
        )
        .await;
        let (out, is_error) = run_tool("srv__web_search", json!({"q": "x"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "ran srv__web_search");
    }

    #[tokio::test]
    async fn refreshed_schema_invalidates_the_previous_unlock() {
        let source = Arc::new(ChangingSrv::new("old"));
        let ctx = deferred_ctx_with_source("generation", source.clone());

        let out = search_select(&ctx, "srv__changing").await;
        assert!(out.contains(r#""old""#), "{out}");
        let (out, is_error) = run_tool("srv__changing", json!({"old": "x"}), &ctx).await;
        assert!(!is_error, "{out}");

        source.replace_schema("new");
        let (out, is_error) = run_tool("srv__changing", json!({"old": "x"}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");

        let out = search_select(&ctx, "srv__changing").await;
        assert!(out.contains(r#""new""#), "{out}");
        let (out, is_error) = run_tool("srv__changing", json!({"new": "x"}), &ctx).await;
        assert!(!is_error, "{out}");
    }

    #[tokio::test]
    async fn same_name_winner_change_cannot_reuse_or_hop_an_unlock() {
        let first = Arc::new(OwnedSrv::new("srv__same", "first"));
        let second = Arc::new(OwnedSrv::new("srv__same", "second"));
        let ctx = with_defer_threshold(
            test_ctx_with_sources(0, "source-owner", vec![first.clone(), second.clone()]),
            0,
        );

        let out = search_select(&ctx, "srv__same").await;
        assert!(out.contains("owned by first"), "{out}");
        assert_eq!(
            run_tool("srv__same", json!({}), &ctx).await,
            ("first".into(), false)
        );

        first.remove_definition();
        let (out, is_error) = run_tool("srv__same", json!({}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");
        assert_eq!(first.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(
            second.calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "a stale receipt must not hop to the later same-name source"
        );
    }

    #[tokio::test]
    async fn wire_call_rejects_refresh_after_dispatch_validation() {
        assert_wire_refresh_race(
            "wire-race",
            "srv__wire_race",
            WireRacePath::Discovered,
            WireRaceAxis::Definition,
        )
        .await;
    }

    #[tokio::test]
    async fn wire_call_rejects_readiness_refresh_after_dispatch_validation() {
        assert_wire_refresh_race(
            "readiness-wire-race",
            "srv__readiness_wire_race",
            WireRacePath::Discovered,
            WireRaceAxis::Readiness,
        )
        .await;
    }

    #[tokio::test]
    async fn program_bypasses_discovery_but_keeps_source_generation_binding() {
        let source = Arc::new(OwnedSrv::new("srv__program", "program"));
        let mut ctx = deferred_ctx_with_source("program-binding", source.clone());
        ctx.from_program = true;

        assert_eq!(
            run_tool("srv__program", json!({}), &ctx).await,
            ("program".into(), false)
        );
        assert_eq!(*source.seen_generations.lock().unwrap(), vec![Some(0)]);
        assert!(unlocked(&ctx.cfg).is_empty());
    }

    #[tokio::test]
    async fn program_refresh_race_still_fails_at_the_wire_gate() {
        assert_wire_refresh_race(
            "program-wire-race",
            "srv__program_race",
            WireRacePath::Program,
            WireRaceAxis::Definition,
        )
        .await;
    }

    #[tokio::test]
    async fn permission_mode_and_workspace_session_grants_invalidate_future_calls() {
        let source = Arc::new(OwnedSrv::new("srv__approval", "approved"));
        let approver = Arc::new(WorkspaceSessionApprover {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut ctx = with_defer_threshold(
            test_ctx_with_sources(0, "permission-epoch", vec![source.clone()]),
            0,
        );
        let mut cfg = ctx.cfg.test_clone();
        cfg.permissions = Arc::new(
            crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &Default::default(),
                cfg.cwd.clone(),
                Some(approver.clone()),
            )
            .unwrap(),
        );
        ctx.cfg = Arc::new(cfg);

        run_tool(
            "tool_search",
            json!({"query": "select:srv__approval"}),
            &ctx,
        )
        .await;
        assert_eq!(
            run_tool("srv__approval", json!({}), &ctx).await,
            ("approved".into(), false),
            "the call whose approval advances the cache epoch still completes"
        );
        assert_eq!(approver.calls.load(std::sync::atomic::Ordering::Relaxed), 1);

        let (out, is_error) = run_tool("srv__approval", json!({}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");
        assert_eq!(
            approver.calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "stale discovery must bounce before permission"
        );

        run_tool(
            "tool_search",
            json!({"query": "select:srv__approval"}),
            &ctx,
        )
        .await;
        assert_eq!(
            run_tool("srv__approval", json!({}), &ctx).await,
            ("approved".into(), false)
        );
        assert_eq!(
            approver.calls.load(std::sync::atomic::Ordering::Relaxed),
            1,
            "the remembered workspace grant remains the permission verdict"
        );

        ctx.cfg
            .permissions
            .set_mode(crate::permissions::Mode::Bypass);
        let (out, is_error) = run_tool("srv__approval", json!({}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");
    }

    #[tokio::test]
    async fn project_policy_refresh_and_revision_zero_invalidation_stale_receipts() {
        let root = temp_git_repo("tool-receipt-policy");
        let identity = crate::project::WorkspaceIdentity::resolve(&root);
        let project_id = identity.project_id().unwrap().clone();
        let registry = crate::permissions::ProjectPolicyRegistry::default();
        let writer = Arc::new(NoopProjectWriter);
        let project = registry.get_or_insert(
            project_id.clone(),
            crate::permissions::ProjectPolicySnapshot::empty(),
            writer.clone(),
        );
        let project_view = Arc::clone(&project);
        let source = Arc::new(OwnedSrv::new("srv__policy", "policy"));
        let mut ctx = with_defer_threshold(
            test_ctx_with_sources(0, "project-policy-epoch", vec![source]),
            0,
        );
        let mut cfg = ctx.cfg.test_clone();
        cfg.cwd = root.clone();
        cfg.permissions = Arc::new(crate::permissions::Permissions::from_layers(
            Arc::new(crate::permissions::GlobalPermissionPolicy::empty()),
            project,
            Arc::new(crate::permissions::PermissionSession::new(
                crate::permissions::Mode::Bypass,
                None,
            )),
            identity,
        ));
        ctx.cfg = Arc::new(cfg);

        run_tool("tool_search", json!({"query": "select:srv__policy"}), &ctx).await;
        assert_eq!(
            run_tool("srv__policy", json!({}), &ctx).await,
            ("policy".into(), false)
        );

        registry.get_or_insert(
            project_id.clone(),
            crate::permissions::ProjectPolicySnapshot {
                revision: 1,
                allow: crate::permissions::ProjectAllowRules::empty(),
            },
            writer,
        );
        let (out, is_error) = run_tool("srv__policy", json!({}), &ctx).await;
        assert!(is_error, "{out}");

        run_tool("tool_search", json!({"query": "select:srv__policy"}), &ctx).await;
        registry.invalidate(&project_id);
        assert_eq!(project_view.snapshot().revision, 0);
        let (out, is_error) = run_tool("srv__policy", json!({}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn receipt_field_mutations_fail_before_permission_and_source() {
        let source = Arc::new(OwnedSrv::new("srv__mutate", "mutated"));
        let approver = Arc::new(WorkspaceSessionApprover {
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let mut ctx = with_defer_threshold(
            test_ctx_with_sources(0, "receipt-mutation", vec![source.clone()]),
            0,
        );
        let mut cfg = ctx.cfg.test_clone();
        cfg.permissions = Arc::new(
            crate::permissions::Permissions::new(
                crate::permissions::Mode::Manual,
                &Default::default(),
                cfg.cwd.clone(),
                Some(approver.clone()),
            )
            .unwrap(),
        );
        ctx.cfg = Arc::new(cfg);
        run_tool("tool_search", json!({"query": "select:srv__mutate"}), &ctx).await;
        let receipt = ctx.cfg.unlocked_tools.receipts().pop().unwrap();
        let other_workspace = crate::project::WorkspaceIdentity::ephemeral(
            std::env::temp_dir().join("kloop-other-receipt-workspace"),
        );
        let mut mutations = Vec::new();
        let mut changed = receipt.clone();
        changed.tool_name = "srv__other".into();
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.source.source_slot += 1;
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.source.version.definition_generation += 1;
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.capability.workspace_id = other_workspace.workspace_id().clone();
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.capability.workspace_cwd.push("other");
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.capability.workspace_epoch += 1;
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.capability.agent_id = "agent-999".parse().unwrap();
        mutations.push(changed);
        let mut changed = receipt.clone();
        changed.capability.depth += 1;
        mutations.push(changed);
        let mut changed = receipt;
        changed.capability.tool_allowlist = Some(vec!["srv__mutate".into()]);
        mutations.push(changed);

        for mutation in mutations {
            ctx.cfg.unlocked_tools.replace_for_test(mutation);
            let (out, is_error) = run_tool("srv__mutate", json!({}), &ctx).await;
            assert!(is_error, "{out}");
            assert!(out.contains("deferred and not loaded yet"), "{out}");
        }
        assert_eq!(
            approver.calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "mutated receipts must fail before permission"
        );
        assert_eq!(
            source.calls.load(std::sync::atomic::Ordering::Relaxed),
            0,
            "mutated receipts must fail before source execution"
        );
    }

    #[tokio::test]
    async fn worktree_transitions_require_rediscovery_even_after_returning_to_base() {
        let root = temp_git_repo("tool-receipt-worktree");
        let ctx = git_ctx(deferred_ctx("worktree-scope"), &root, true);
        run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &ctx,
        )
        .await;
        assert_eq!(
            run_tool("srv__web_search", json!({}), &ctx).await,
            ("ran srv__web_search".into(), false)
        );

        crate::worktree::enter(&ctx.cfg, "receipt-scope")
            .await
            .unwrap();
        let (out, is_error) = run_tool("srv__web_search", json!({}), &ctx).await;
        assert!(is_error, "{out}");
        crate::worktree::exit(&ctx.cfg, crate::worktree::ExitAction::Remove, false)
            .await
            .unwrap();
        let (out, is_error) = run_tool("srv__web_search", json!({}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn inactive_regime_leaves_dispatch_untouched() {
        let ctx = test_ctx_with_sources(0, "inactive", vec![srv()]);
        let (out, is_error) = run_tool("srv__web_search", json!({"q": "x"}), &ctx).await;
        assert!(!is_error);
        assert_eq!(out, "ran srv__web_search");
        assert_eq!(deferred_notice(&ctx.cfg), None);
    }

    #[test]
    fn notice_lists_every_deferred_name_and_stays_static_across_unlocks() {
        let ctx = deferred_ctx("notice");
        let (tools_before, notice_before) = tool_surface(&ctx);
        let notice = notice_before.clone().expect("regime active");
        assert!(notice.starts_with("<system-reminder>"), "{notice}");
        assert!(
            notice.contains("srv__web_search\nsrv__page_fetch"),
            "{notice}"
        );
        assert!(notice.contains("tool_search"), "{notice}");
        // Unlocking must not change the injected text (prompt-cache stability).
        let workspace = ctx.cfg.effective_workspace();
        let source = current_source_binding("srv__web_search", &ctx.cfg).unwrap();
        let capability = CapabilityBinding::capture(&ctx, &workspace);
        ctx.cfg.unlocked_tools.record(receipt_for_capability(
            "srv__web_search",
            source,
            &capability,
        ));
        assert_eq!(
            tool_surface(&ctx),
            (tools_before, notice_before),
            "receipt churn must not alter the provider tool surface"
        );
    }

    /// Ordinary same-agent Config clones share one live capability store.
    #[test]
    fn unlock_set_is_shared_into_cloned_configs() {
        let ctx = deferred_ctx("shared");
        let sub = Config {
            max_rounds: Some(1),
            ..ctx.cfg.test_clone()
        };
        assert!(Arc::ptr_eq(&ctx.cfg.unlocked_tools, &sub.unlocked_tools));
        assert!(Arc::ptr_eq(
            &ctx.cfg.powershell_execution_gate,
            &sub.powershell_execution_gate
        ));
    }

    #[tokio::test]
    async fn same_session_compaction_preserves_deferred_unlock() {
        let source = Arc::new(OwnedSrv::new("srv__mutate", "mutated"));
        let ctx = with_provider(
            deferred_ctx_with_source("compaction-preserves-unlock", source.clone()),
            Provider::mock_scripted(vec![MockTurn::Blocks(vec![AssistantBlock::Text {
                text: "summary".into(),
            }])]),
        );
        search_select(&ctx, "srv__mutate").await;
        let receipts_before = ctx.cfg.unlocked_tools.receipts();
        let surface_before = tool_surface(&ctx);

        let mut history = History::new(ctx.cfg.offload_dir.clone());
        history.record(Message::user_text("old"));
        history.record(Message::assistant(vec![ContentBlock::Text {
            text: "old answer".into(),
        }]));
        history.record(Message::user_text("current"));
        let stats = crate::compact::run_compaction(
            &ctx.cfg,
            "mock",
            &mut history,
            &CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(stats.summarized, 1);
        assert_eq!(stats.kept, 2);
        assert_eq!(
            history.messages()[0],
            kloop_protocol::Message::injected(
                kloop_protocol::Injected::ContextSummary,
                format!("{}summary", crate::compact::SUMMARY_PREFIX),
            )
        );
        assert_eq!(
            ctx.cfg.unlocked_tools.receipts(),
            receipts_before,
            "same-session compaction must preserve the exact live receipt"
        );
        assert_eq!(
            tool_surface(&ctx),
            surface_before,
            "same-session compaction must not perturb the provider tool surface"
        );
        assert_eq!(
            run_tool("srv__mutate", json!({}), &ctx).await,
            ("mutated".into(), false)
        );
        assert_eq!(source.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn conversation_reset_revokes_live_receipts() {
        let ctx = deferred_ctx("conversation-reset");
        search_select(&ctx, "srv__web_search").await;
        assert_eq!(
            run_tool("srv__web_search", json!({}), &ctx).await,
            ("ran srv__web_search".into(), false)
        );

        ctx.cfg.reset_deferred_tool_capabilities();
        let (output, is_error) = run_tool("srv__web_search", json!({}), &ctx).await;
        assert!(is_error, "{output}");
        assert!(output.contains("deferred and not loaded yet"), "{output}");
    }

    #[tokio::test]
    async fn subagent_has_fresh_receipts_and_cannot_use_a_shared_parent_receipt() {
        let ctx = deferred_ctx("child-scope");
        run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &ctx,
        )
        .await;
        let workspace = ctx.cfg.effective_workspace();
        let child = ctx
            .cfg
            .subagent_from(&workspace, None, "agent-999".parse().unwrap());
        assert!(!Arc::ptr_eq(&ctx.cfg.unlocked_tools, &child.unlocked_tools));
        assert!(child.unlocked_tools.tool_names().is_empty());

        let mut child_ctx = ctx.clone();
        child_ctx.depth = 1;
        child_ctx.cfg = Arc::new(child);
        let (out, is_error) = run_tool("srv__web_search", json!({}), &child_ctx).await;
        assert!(is_error, "{out}");

        let mut forged = child_ctx.cfg.test_clone();
        forged.unlocked_tools = Arc::clone(&ctx.cfg.unlocked_tools);
        child_ctx.cfg = Arc::new(forged);
        let (out, is_error) = run_tool("srv__web_search", json!({}), &child_ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");
    }

    #[tokio::test]
    async fn allowlist_change_requires_a_scope_specific_receipt() {
        let ctx = deferred_ctx("allowlist-scope");
        run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &ctx,
        )
        .await;
        let mut restricted = ctx.cfg.test_clone();
        restricted.tool_allowlist = Some(Arc::new(
            ["tool_search".to_string(), "srv__web_search".to_string()]
                .into_iter()
                .collect(),
        ));
        let mut restricted_ctx = ctx.clone();
        restricted_ctx.cfg = Arc::new(restricted);

        let (out, is_error) = run_tool("srv__web_search", json!({}), &restricted_ctx).await;
        assert!(is_error, "{out}");
        run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &restricted_ctx,
        )
        .await;
        assert_eq!(
            run_tool("srv__web_search", json!({}), &restricted_ctx).await,
            ("ran srv__web_search".into(), false)
        );
    }

    #[test]
    fn independent_sessions_do_not_share_the_powershell_gate() {
        let first = deferred_ctx("powershell-gate-first");
        let second = deferred_ctx("powershell-gate-second");
        assert!(!Arc::ptr_eq(
            &first.cfg.powershell_execution_gate,
            &second.cfg.powershell_execution_gate
        ));
    }

    #[test]
    fn tool_search_is_an_ordering_barrier() {
        assert!(!crate::tools::is_concurrency_safe(
            "tool_search",
            &json!({"query": "x"}),
            &[]
        ));
    }

    #[tokio::test]
    async fn same_response_search_then_readonly_call_observes_request_order() {
        let ctx = deferred_ctx("same-response-search-first");
        let results = crate::tools::dispatch_tools(
            vec![
                (
                    "search".into(),
                    "tool_search".into(),
                    json!({"query": "select:srv__web_search"}),
                ),
                ("call".into(), "srv__web_search".into(), json!({"q": "x"})),
            ],
            &ctx,
        )
        .await;
        assert!(matches!(
            &results[0],
            kloop_protocol::ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
        assert_eq!(
            results[1],
            kloop_protocol::ContentBlock::ToolResult {
                tool_use_id: "call".into(),
                content: "ran srv__web_search".into(),
                is_error: false,
            }
        );

        let ctx = deferred_ctx("same-response-call-first");
        let results = crate::tools::dispatch_tools(
            vec![
                ("call".into(), "srv__web_search".into(), json!({"q": "x"})),
                (
                    "search".into(),
                    "tool_search".into(),
                    json!({"query": "select:srv__web_search"}),
                ),
            ],
            &ctx,
        )
        .await;
        assert!(matches!(
            &results[0],
            kloop_protocol::ContentBlock::ToolResult { is_error: true, .. }
        ));
        assert!(matches!(
            &results[1],
            kloop_protocol::ContentBlock::ToolResult {
                is_error: false,
                ..
            }
        ));
    }

    /// The call_tool envelope is transparent: the gate judges the inner
    /// name (locked bounce, then success after unlock), and a malformed
    /// envelope reports usage instead of dispatching.
    #[tokio::test]
    async fn call_tool_unwraps_to_the_inner_call() {
        let ctx = deferred_ctx("call-tool");
        let wrapped = json!({"tool_name": "srv__web_search", "params": {"q": "x"}});

        let (out, is_error) = run_tool("call_tool", wrapped.clone(), &ctx).await;
        assert!(is_error);
        assert!(out.contains("tool 'srv__web_search' is deferred"), "{out}");

        run_tool(
            "tool_search",
            json!({"query": "select:srv__web_search"}),
            &ctx,
        )
        .await;
        let (out, is_error) = run_tool("call_tool", wrapped, &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "ran srv__web_search");

        // params defaults to {} when omitted.
        let (out, is_error) =
            run_tool("call_tool", json!({"tool_name": "srv__web_search"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert_eq!(out, "ran srv__web_search");

        let (out, is_error) = run_tool("call_tool", json!({"params": {}}), &ctx).await;
        assert!(is_error);
        assert!(
            out.contains("missing required string argument 'tool_name'"),
            "{out}"
        );
    }

    /// Unwrapping happens before concurrency classification: a call_tool
    /// envelope around a read-only source tool batches as read-only.
    #[test]
    fn unwrap_happens_before_concurrency_classification() {
        let (name, input) = unwrap_call_tool(
            "call_tool".into(),
            json!({"tool_name": "srv__web_search", "params": {"q": "x"}}),
        );
        assert_eq!(name, "srv__web_search");
        assert_eq!(input, json!({"q": "x"}));

        // Non-envelopes and malformed envelopes pass through untouched.
        let (name, input) = unwrap_call_tool("bash".into(), json!({"command": "ls"}));
        assert_eq!((name.as_str(), &input), ("bash", &json!({"command": "ls"})));
        let (name, _) = unwrap_call_tool("call_tool".into(), json!({"params": {}}));
        assert_eq!(name, "call_tool");
    }
}
