//! Web tools glue: `[web]` config parsing and the `ToolSource` adapter over
//! `kloop-web`. Core's `tools::web` module owns the agent-facing contracts;
//! this layer selects network backends and binds execution without adding a
//! network dependency to core. web_fetch is always on (except --mock);
//! web_search needs the selected backend's key and degrades to a warning
//! without one.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use serde_json::Value;

use kloop_core::tools::SourceOutput;
use kloop_core::tools::ToolSource;
use kloop_core::tools::web;
use kloop_core::tools::web::WEB_FETCH;
use kloop_core::tools::web::WEB_SEARCH;
use kloop_protocol::ToolDef;
use kloop_web::Brave;
use kloop_web::SearchBackend;
use kloop_web::Tavily;
use kloop_web::WebTools;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebConfig {
    /// Which SearchBackend to construct: "tavily" (default; free tier needs
    /// no card) or "brave".
    pub search_provider: String,
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            search_provider: "tavily".into(),
        }
    }
}

/// Parse the optional `[web]` table from the global user config. A missing
/// section uses defaults; unknown keys and malformed values are errors.
pub fn load_web_config(root: &toml::Table) -> Result<WebConfig> {
    let Some(web) = root.get("web") else {
        return Ok(WebConfig::default());
    };
    let web = web.as_table().context("[web] must be a table")?;
    let mut cfg = WebConfig::default();
    for (key, val) in web {
        match key.as_str() {
            "search_provider" => {
                cfg.search_provider = val
                    .as_str()
                    .context("[web].search_provider must be a string")?
                    .to_string();
            }
            other => bail!("[web] has unknown key '{other}' (search_provider)"),
        }
    }
    Ok(cfg)
}

/// Build the web ToolSource. Search backend selection degrades to
/// fetch-only with a warning (missing key, unknown provider) — web tools
/// never block startup.
pub fn build_web_source(cfg: &WebConfig, warn: &dyn Fn(&str)) -> Option<Arc<dyn ToolSource>> {
    let backend = |key: String| -> Option<Box<dyn SearchBackend>> {
        match cfg.search_provider.as_str() {
            "tavily" => Some(Box::new(Tavily::new(key))),
            "brave" => Some(Box::new(Brave::new(key))),
            _ => None,
        }
    };
    let key_env = match cfg.search_provider.as_str() {
        "tavily" => Some("TAVILY_API_KEY"),
        "brave" => Some("BRAVE_API_KEY"),
        _ => None,
    };
    let search: Option<Box<dyn SearchBackend>> = match key_env {
        None => {
            warn(&format!(
                "web_search disabled: unknown [web].search_provider '{}' (supported: tavily, brave)",
                cfg.search_provider
            ));
            None
        }
        Some(env) => match std::env::var(env) {
            Ok(key) if !key.is_empty() => backend(key),
            _ => {
                warn(&format!(
                    "web_search disabled: {env} not set (web_fetch still available)"
                ));
                None
            }
        },
    };
    match WebTools::new(search) {
        Ok(tools) => Some(web_source(tools)),
        Err(e) => {
            warn(&format!("web tools unavailable, skipped: {e:#}"));
            None
        }
    }
}

fn web_source(tools: WebTools) -> Arc<dyn ToolSource> {
    let defs = Arc::from(web::tool_defs(tools.search_backend_name()));
    Arc::new(WebToolSource { tools, defs })
}

struct WebToolSource {
    tools: WebTools,
    defs: Arc<[ToolDef]>,
}

impl ToolSource for WebToolSource {
    fn defs(&self) -> Arc<[ToolDef]> {
        self.defs.clone()
    }

    /// Both tools only read the network — eligible for concurrent dispatch.
    /// (The permission gate is separate and still asks for unknown tool
    /// names unless an allow rule covers them.)
    fn is_readonly(&self, _tool: &str) -> bool {
        true
    }

    fn call<'a>(
        &'a self,
        tool: &'a str,
        input: &'a Value,
    ) -> Pin<Box<dyn Future<Output = Result<SourceOutput>> + Send + 'a>> {
        Box::pin(async move {
            let text = match tool {
                WEB_FETCH => self.tools.fetch(input).await?,
                WEB_SEARCH => self.tools.search(input).await?,
                other => bail!("unknown web tool: {other}"),
            };
            Ok(SourceOutput::text(text))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(content: &str) -> toml::Table {
        content.parse().unwrap()
    }

    #[test]
    fn load_web_config_defaults_and_override() {
        assert_eq!(
            load_web_config(&toml::Table::new()).unwrap(),
            WebConfig::default()
        );

        let root = config("[permissions]\nallow = []\n");
        assert_eq!(load_web_config(&root).unwrap(), WebConfig::default());

        let root = config("[web]\nsearch_provider = \"brave\"\n");
        assert_eq!(
            load_web_config(&root).unwrap(),
            WebConfig {
                search_provider: "brave".into()
            }
        );
    }

    #[test]
    fn load_web_config_rejects_unknown_keys_and_bad_types() {
        for (tag, bad) in [
            ("unknown", "[web]\nprovider = \"brave\"\n"),
            ("badtype", "[web]\nsearch_provider = 3\n"),
            ("section", "web = 3\n"),
        ] {
            let root = config(bad);
            assert!(load_web_config(&root).is_err(), "{tag} should fail");
        }
    }

    #[test]
    fn build_web_source_degrades_search_by_provider() {
        // Unknown provider: fetch-only source plus a warning.
        let warnings = std::sync::Mutex::new(Vec::<String>::new());
        let warn = |s: &str| warnings.lock().unwrap().push(s.to_string());
        let cfg = WebConfig {
            search_provider: "duckduckgo".into(),
        };
        let source = build_web_source(&cfg, &warn).expect("fetch-only source");
        let names: Vec<String> = source.defs().iter().map(|def| def.name.clone()).collect();
        assert_eq!(names, vec!["web_fetch".to_string()]);
        assert!(source.is_readonly("web_fetch"));
        let warnings = warnings.into_inner().unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].contains("unknown [web].search_provider"),
            "{warnings:?}"
        );
    }

    struct FakeSearch;

    impl SearchBackend for FakeSearch {
        fn name(&self) -> &'static str {
            "fake"
        }

        fn search<'a>(
            &'a self,
            _client: &'a reqwest::Client,
            query: &'a str,
            count: usize,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<kloop_web::SearchHit>>> + Send + 'a>> {
            Box::pin(async move {
                assert_eq!(query, "rust agent");
                assert_eq!(count, 5);
                Ok(vec![kloop_web::SearchHit {
                    title: "Rust".into(),
                    url: "https://www.rust-lang.org/".into(),
                    snippet: "A language".into(),
                }])
            })
        }
    }

    #[tokio::test]
    async fn source_binds_defs_calls_and_errors() {
        let tools = WebTools::new(Some(Box::new(FakeSearch))).unwrap();
        let source = web_source(tools);
        let defs = source.defs();
        let names: Vec<&str> = defs.iter().map(|def| def.name.as_str()).collect();
        assert_eq!(names, vec!["web_fetch", "web_search"]);
        assert!(source.is_readonly("web_fetch"));
        assert!(source.is_readonly("web_search"));

        let input = serde_json::json!({
            "query": "rust agent",
            "allowed_domains": ["rust-lang.org"]
        });
        let output = source.call("web_search", &input).await.unwrap();
        assert_eq!(
            output.text,
            "1. Rust\n   https://www.rust-lang.org/\n   A language"
        );
        assert!(output.blocks.is_none());
        assert!(output.structured.is_none());

        let err = match source.call("web_unknown", &serde_json::json!({})).await {
            Ok(_) => panic!("unknown tool should fail"),
            Err(err) => err,
        };
        assert_eq!(format!("{err:#}"), "unknown web tool: web_unknown");
    }
}
