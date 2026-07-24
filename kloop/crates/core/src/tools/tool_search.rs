//! Deferred-tool discovery: the tool_search tool, the injected notice that
//! tells the model which tools exist but are not loaded, and the dispatch
//! gate for locked tools. Unlocking is deliberately one-way and only ever
//! flows through a search hit — the tool defs sent to the model never change
//! (see `Config.unlocked_tools`).

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
        description: "Search the deferred tools (listed by name in the context). Matching tools' full definitions are returned in the result and those tools become directly callable from then on. Use \"select:<name>[,<name>...]\" to fetch exact tools by name, or keywords to search names and descriptions.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "\"select:<name>[,<name>...]\" for exact selection, or keywords"},
                "max_results": {"type": "integer", "description": "Max keyword matches returned (default 5)"}
            },
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
        description: "Invoke a tool that was loaded via tool_search. Prefer calling loaded tools directly by their own name; use this wrapper only if your runtime rejects such direct calls. The tool must have been loaded by a tool_search first.".into(),
        schema: json!({
            "type": "object",
            "properties": {
                "tool_name": {"type": "string", "description": "Name of the loaded tool to invoke"},
                "params": {"type": "object", "description": "Arguments for that tool, matching its returned schema"}
            },
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
/// deferred name for the whole session regardless of unlock state, so the
/// injected message — like the tool defs — stays byte-stable for the prompt
/// cache. None when deferral is inactive.
pub fn deferred_notice(cfg: &Config) -> Option<String> {
    let defs = deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold);
    if defs.is_empty() {
        return None;
    }
    let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    Some(format!(
        "<system-reminder>\nThe following tools exist but are deferred — their definitions are not loaded and calling them before loading fails:\n{}\nTo use one, first call tool_search (query \"select:<name>\" for an exact pick, or keywords to search). Matching definitions are returned in the result and those tools become directly callable. If your runtime rejects direct calls to loaded tools, invoke them through call_tool instead.\n</system-reminder>",
        names.join("\n")
    ))
}

/// True when `name` is a deferred tool that has not been unlocked yet —
/// checked at the top of dispatch, before hooks and the permission gate:
/// a locked call is a protocol error to bounce back at the model, not
/// something to ask the human about.
pub(super) fn locked(name: &str, cfg: &Config) -> bool {
    deferred_tool_defs(&cfg.tool_sources, cfg.defer_threshold)
        .iter()
        .any(|d| d.name == name)
        && !cfg.unlocked_tools.read().unwrap().contains(name)
}

pub(super) async fn tool_search_tool(input: &Value, ctx: &ToolCtx) -> Result<String> {
    let query = str_arg(input, "query", "tool_search")?.trim().to_string();
    if query.is_empty() {
        bail!("tool_search: query must not be empty");
    }
    let deferred = deferred_tool_defs(&ctx.cfg.tool_sources, ctx.cfg.defer_threshold);

    let mut found: Vec<ToolDef> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    if let Some(rest) = query.strip_prefix("select:") {
        let loaded = super::all_tool_defs(
            ctx.depth,
            &ctx.cfg.tool_sources,
            ctx.cfg.defer_threshold,
            ctx.cfg.worktree_enabled,
        );
        for name in rest.split(',').map(str::trim).filter(|n| !n.is_empty()) {
            if let Some(def) = deferred.iter().find(|d| d.name == name) {
                found.push(def.clone());
            } else if loaded.iter().any(|d| d.name == name) {
                notes.push(format!("'{name}' is already loaded; call it directly"));
            } else {
                notes.push(format!("no deferred tool named '{name}'"));
            }
        }
    } else {
        let max_results = match input["max_results"].as_u64() {
            Some(0) => bail!("tool_search: max_results must be greater than zero"),
            Some(n) => n as usize,
            None => DEFAULT_MAX_RESULTS,
        };
        let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut scored: Vec<(u32, &ToolDef)> = deferred
            .iter()
            .map(|def| (keyword_score(def, &terms), def))
            .filter(|(score, _)| *score > 0)
            .collect();
        // Name hits outrank description hits; ties break alphabetically so
        // results are deterministic.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        found.extend(scored.into_iter().take(max_results).map(|(_, d)| d.clone()));
    }

    if !found.is_empty() {
        let mut unlocked = ctx.cfg.unlocked_tools.write().unwrap();
        for def in &found {
            unlocked.insert(def.name.clone());
        }
    }
    Ok(render(&found, &notes, deferred.len()))
}

fn keyword_score(def: &ToolDef, terms: &[String]) -> u32 {
    let name = def.name.to_lowercase();
    let description = def.description.to_lowercase();
    terms
        .iter()
        .map(|t| {
            if name.contains(t.as_str()) {
                10
            } else if description.contains(t.as_str()) {
                2
            } else {
                0
            }
        })
        .sum()
}

fn render(found: &[ToolDef], notes: &[String], total_deferred: usize) -> String {
    let mut out = String::new();
    if found.is_empty() && notes.is_empty() {
        return format!("No matching deferred tools found ({total_deferred} deferred tools exist; their names are listed in the context).");
    }
    if !found.is_empty() {
        out.push_str(&format!(
            "Found {} tool(s); they are now loaded. Even though they were not in your original tool list, invoke them like any other tool — emit a regular tool call with the name and parameters below. If your runtime rejects that, invoke via call_tool({{\"tool_name\": \"<name>\", \"params\": {{...}}}}) instead. Do not delegate this or fall back to other tools:\n",
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
        fn defs(&self) -> &[ToolDef] {
            &self.defs
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
        let mut names: Vec<String> = cfg.unlocked_tools.read().unwrap().iter().cloned().collect();
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
            out.contains("max_results must be greater than zero"),
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
            .insert("srv__web_search".into());
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
