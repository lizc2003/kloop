//! Web tools glue: `[web]` config parsing and the `ToolSource` adapter over
//! `kloop-web`. Core's `tools::web` module owns the agent-facing contracts;
//! this layer selects network backends and binds execution without adding a
//! network dependency to core. web_fetch is always on (except --mock);
//! web_search needs `[web].api_key` and degrades to a warning without it.

use std::fmt;
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

#[derive(Clone, PartialEq, Eq)]
pub struct WebConfig {
    /// Which SearchBackend to construct: "tavily" (default; free tier needs
    /// no card) or "brave".
    pub search_provider: String,
    /// The selected backend's key, and `[web].api_key` in the global config
    /// is the only place it can come from. No environment variable: every
    /// other credential kloop uses is written in that one 0600 file, and a
    /// key that only an exported variable can carry works for whoever already
    /// knows the variable's name and silently degrades to fetch-only for
    /// everyone else — including the user who configured the provider right
    /// there in the file.
    pub api_key: Option<String>,
}

/// Renders the key as `<redacted>`: this struct holds a credential, and both
/// assertions and any future logging reach for `{:?}`.
impl fmt::Debug for WebConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WebConfig")
            .field("search_provider", &self.search_provider)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl Default for WebConfig {
    fn default() -> Self {
        WebConfig {
            search_provider: "tavily".into(),
            api_key: None,
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
            "api_key" => {
                let key = val.as_str().context("[web].api_key must be a string")?;
                if key.is_empty() {
                    bail!("[web].api_key must not be empty");
                }
                cfg.api_key = Some(key.to_string());
            }
            other => bail!("[web] has unknown key '{other}' (search_provider, api_key)"),
        }
    }
    Ok(cfg)
}

/// Build the web ToolSource. Search backend selection degrades to
/// fetch-only with a warning (missing key, unknown provider) — web tools
/// never block startup.
pub fn build_web_source(cfg: &WebConfig, warn: &dyn Fn(&str)) -> Option<Arc<dyn ToolSource>> {
    // An unknown provider is reported ahead of a missing key: the name is the
    // more basic mistake, and naming the key to add would send the user off to
    // buy one for a backend kloop cannot build.
    let search: Option<Box<dyn SearchBackend>> = match cfg.search_provider.as_str() {
        "tavily" => keyed(cfg, warn, |key| Box::new(Tavily::new(key))),
        "brave" => keyed(cfg, warn, |key| Box::new(Brave::new(key))),
        other => {
            warn(&format!(
                "web_search disabled: unknown [web].search_provider '{other}' \
                 (supported: tavily, brave)"
            ));
            None
        }
    };
    match WebTools::new(search) {
        Ok(tools) => Some(web_source(tools)),
        Err(e) => {
            warn(&format!("web tools unavailable, skipped: {e:#}"));
            None
        }
    }
}

/// A known provider still needs its key, and `[web].api_key` is the one place
/// that carries it.
fn keyed(
    cfg: &WebConfig,
    warn: &dyn Fn(&str),
    build: impl FnOnce(String) -> Box<dyn SearchBackend>,
) -> Option<Box<dyn SearchBackend>> {
    match &cfg.api_key {
        Some(key) => Some(build(key.clone())),
        None => {
            warn(
                "web_search disabled: no [web].api_key in ~/.kloop/config.toml \
                 (web_fetch still available)",
            );
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

        let root = config("[web]\nsearch_provider = \"brave\"\napi_key = \"bsk\"\n");
        assert_eq!(
            load_web_config(&root).unwrap(),
            WebConfig {
                search_provider: "brave".into(),
                api_key: Some("bsk".into()),
            }
        );

        // The key alone is a complete config: the provider keeps its default.
        let root = config("[web]\napi_key = \"tvly-x\"\n");
        assert_eq!(
            load_web_config(&root).unwrap(),
            WebConfig {
                search_provider: "tavily".into(),
                api_key: Some("tvly-x".into()),
            }
        );
    }

    /// The struct carries a credential, so the derived Debug had to go.
    #[test]
    fn debug_never_prints_the_key() {
        let cfg = WebConfig {
            search_provider: "tavily".into(),
            api_key: Some("tvly-secret".into()),
        };
        assert_eq!(
            format!("{cfg:?}"),
            "WebConfig { search_provider: \"tavily\", api_key: Some(\"<redacted>\") }"
        );
        assert_eq!(
            format!("{:?}", WebConfig::default()),
            "WebConfig { search_provider: \"tavily\", api_key: None }"
        );
    }

    #[test]
    fn load_web_config_rejects_unknown_keys_and_bad_types() {
        for (tag, bad) in [
            ("unknown", "[web]\nprovider = \"brave\"\n"),
            ("badtype", "[web]\nsearch_provider = 3\n"),
            ("keytype", "[web]\napi_key = 3\n"),
            // Written-but-empty is a typo, not a request for fetch-only —
            // same call as providers.x.auth_header makes.
            ("keyempty", "[web]\napi_key = \"\"\n"),
            ("section", "web = 3\n"),
        ] {
            let root = config(bad);
            assert!(load_web_config(&root).is_err(), "{tag} should fail");
        }

        // A key misspelled into an unknown one must not echo the secret.
        let root = config("[web]\napi_keys = \"tvly-sentinel\"\n");
        let error = load_web_config(&root).unwrap_err().to_string();
        assert_eq!(
            error,
            "[web] has unknown key 'api_keys' (search_provider, api_key)"
        );
    }

    /// Registration is a pure function of the config now that no environment
    /// variable takes part, so all three outcomes are assertable here.
    #[test]
    fn build_web_source_registers_search_only_with_a_known_provider_and_a_key() {
        let build = |cfg: WebConfig| {
            let warnings = std::sync::Mutex::new(Vec::<String>::new());
            let names = {
                let warn = |s: &str| warnings.lock().unwrap().push(s.to_string());
                let source = build_web_source(&cfg, &warn).expect("web source");
                assert!(source.is_readonly("web_fetch"));
                source
                    .defs()
                    .iter()
                    .map(|def| def.name.clone())
                    .collect::<Vec<String>>()
            };
            (names, warnings.into_inner().unwrap())
        };

        let (names, warnings) = build(WebConfig {
            search_provider: "tavily".into(),
            api_key: Some("tvly-k".into()),
        });
        assert_eq!(names, ["web_fetch", "web_search"]);
        assert_eq!(warnings, [] as [String; 0]);

        let (names, warnings) = build(WebConfig::default());
        assert_eq!(names, ["web_fetch"]);
        assert_eq!(
            warnings,
            [
                "web_search disabled: no [web].api_key in ~/.kloop/config.toml \
              (web_fetch still available)"
            ]
        );

        // The provider name is the more basic mistake: it is reported even
        // when the key is missing too.
        let (names, warnings) = build(WebConfig {
            search_provider: "duckduckgo".into(),
            api_key: None,
        });
        assert_eq!(names, ["web_fetch"]);
        assert_eq!(
            warnings,
            [
                "web_search disabled: unknown [web].search_provider 'duckduckgo' \
              (supported: tavily, brave)"
            ]
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
