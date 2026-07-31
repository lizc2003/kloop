//! Deferred-tool discovery: the tool_search tool, the injected notice that
//! tells the model which tools exist but are not loaded, and the dispatch
//! gate for locked tools. An unlock is bound to the source definition
//! generation returned by a search hit; catalog refresh invalidates it. The
//! provider tool array does not change merely because a tool was unlocked.

use anyhow::bail;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use super::deferred_tool_defs;
use super::str_arg;
use super::ToolCtx;
use crate::config::Config;
use kloop_protocol::ToolDef;

const DEFAULT_MAX_RESULTS: usize = 5;

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
    let defs = deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold);
    if defs.is_empty() {
        return None;
    }
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    Some(format!(
        "<system-reminder>\nThe following tools exist but are deferred — their definitions are not loaded and calling them before loading fails:\n{}\nTo use one, first call tool_search (query \"select:<name>\" for an exact pick, or keywords to search), then invoke it through call_tool with the returned schema. A direct call by the loaded tool's own name is only a provider-compatibility optimization.\n</system-reminder>",
        names.join("\n")
    ))
}

/// True when `name` is a deferred tool that has not been unlocked yet —
/// checked at the top of dispatch, before hooks and the permission gate:
/// a locked call is a protocol error to bounce back at the model, not
/// something to ask the human about.
pub(super) fn locked(name: &str, cfg: &Config) -> bool {
    let deferred = deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold);
    if !deferred.iter().any(|def| def.name == name) {
        return false;
    }
    let Some(generation) = super::source_definition_generation(&cfg.tool_sources, name) else {
        return true;
    };
    cfg.unlocked_tools.read().unwrap().get(name).copied() != Some(generation)
}

pub(super) fn unlocked_generation_for_dispatch(name: &str, cfg: &Config) -> Option<u64> {
    deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold)
        .iter()
        .any(|def| def.name == name)
        .then(|| cfg.unlocked_tools.read().unwrap().get(name).copied())
        .flatten()
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

pub(super) async fn tool_search_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let query = str_arg(input, "query", "tool_search")?.trim().to_string();
    if query.is_empty() {
        bail!("tool_search: query must not be empty");
    }
    let deferred = deferred_tool_defs(&ctx.cfg.tool_sources, ctx.cfg.defer_threshold);
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
            } else {
                notes.push(format!("no deferred tool named '{name}'"));
            }
        }
        if !selected_any {
            bail!("tool_search: select: requires at least one tool name");
        }
    } else {
        if let Some(def) = deferred
            .iter()
            .find(|def| def.name.eq_ignore_ascii_case(&query))
        {
            found.push(def.clone());
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
    }

    let mut current_found = Vec::with_capacity(found.len());
    for def in found {
        if let Some((current, generation)) =
            super::source_definition_snapshot(&ctx.cfg.tool_sources, &def.name)
        {
            current_found.push((current, generation));
        }
    }
    if !current_found.is_empty() {
        let mut unlocked = ctx.cfg.unlocked_tools.write().unwrap();
        for (def, generation) in &current_found {
            unlocked.insert(def.name.clone(), *generation);
        }
    }
    let found: Vec<ToolDef> = current_found
        .into_iter()
        .map(|(def, _generation)| def)
        .collect();
    Ok(render(&found, &notes, deferred.len()))
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
        return format!("No matching deferred tools found ({total_deferred} deferred tools exist; their names are listed in the context).");
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
    use crate::tools::testutil::*;
    use crate::tools::ToolSource;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;

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

    fn deferred_ctx(tag: &str) -> crate::tools::ToolCtx {
        with_defer_threshold(test_ctx_with_sources(0, tag, vec![srv()]), 0)
    }

    fn unlocked(cfg: &Config) -> Vec<String> {
        let mut names: Vec<String> = cfg.unlocked_tools.read().unwrap().keys().cloned().collect();
        names.sort();
        names
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
        let ctx = with_defer_threshold(test_ctx_with_sources(0, "snapshot-race", vec![source]), 0);
        let (out, is_error) =
            run_tool("tool_search", json!({"query": "select:srv__racing"}), &ctx).await;
        assert!(!is_error, "{out}");
        assert!(out.contains(r#""new""#), "{out}");
        assert!(!out.contains(r#""old""#), "{out}");
        assert_eq!(
            ctx.cfg.unlocked_tools.read().unwrap().get("srv__racing"),
            Some(&1)
        );
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
        let ctx = with_defer_threshold(
            test_ctx_with_sources(0, "generation", vec![source.clone()]),
            0,
        );

        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "select:srv__changing"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains(r#""old""#), "{out}");
        let (out, is_error) = run_tool("srv__changing", json!({"old": "x"}), &ctx).await;
        assert!(!is_error, "{out}");

        source.replace_schema("new");
        let (out, is_error) = run_tool("srv__changing", json!({"old": "x"}), &ctx).await;
        assert!(is_error, "{out}");
        assert!(out.contains("deferred and not loaded yet"), "{out}");

        let (out, is_error) = run_tool(
            "tool_search",
            json!({"query": "select:srv__changing"}),
            &ctx,
        )
        .await;
        assert!(!is_error, "{out}");
        assert!(out.contains(r#""new""#), "{out}");
        let (out, is_error) = run_tool("srv__changing", json!({"new": "x"}), &ctx).await;
        assert!(!is_error, "{out}");
    }

    /// Below the threshold nothing is deferred: source tools dispatch
    /// directly and tool_search does not exist.
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
        let notice = deferred_notice(&ctx.cfg).expect("regime active");
        assert!(notice.starts_with("<system-reminder>"), "{notice}");
        assert!(
            notice.contains("srv__web_search\nsrv__page_fetch"),
            "{notice}"
        );
        assert!(notice.contains("tool_search"), "{notice}");
        // Unlocking must not change the injected text (prompt-cache stability).
        ctx.cfg
            .unlocked_tools
            .write()
            .unwrap()
            .insert("srv__web_search".into(), 0);
        assert_eq!(deferred_notice(&ctx.cfg), Some(notice));
    }

    /// Sub-agent configs are `..parent.clone()` spreads: the unlock set is
    /// the same Arc, so a parent's discoveries carry over (and vice versa).
    #[test]
    fn unlock_set_is_shared_into_cloned_configs() {
        let ctx = deferred_ctx("shared");
        let sub = Config {
            max_rounds: Some(1),
            ..(*ctx.cfg).clone()
        };
        assert!(Arc::ptr_eq(&ctx.cfg.unlocked_tools, &sub.unlocked_tools));
    }

    #[test]
    fn tool_search_is_concurrency_safe() {
        assert!(crate::tools::is_concurrency_safe(
            "tool_search",
            &json!({"query": "x"}),
            &[]
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
