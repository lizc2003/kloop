//! kloop-web — network implementations for web_fetch / web_search. The
//! agent-facing names and schemas live in `kloop-core::tools::web`; the CLI
//! glues these operations into core's `ToolSource` seam. Search backends are
//! pluggable behind [`SearchBackend`].

mod fetch;
mod html;
mod limits;
mod search;

use std::time::Duration;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde::Deserialize;
use serde_json::Value;

pub use search::Brave;
pub use search::SearchBackend;
pub use search::SearchHit;
pub use search::Tavily;

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const SEARCH_RESULTS_DEFAULT: usize = 5;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchRequest {
    url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchRequest {
    query: String,
    #[serde(default)]
    allowed_domains: Vec<String>,
    #[serde(default)]
    blocked_domains: Vec<String>,
}

pub struct WebTools {
    client: reqwest::Client,
    search: Option<Box<dyn SearchBackend>>,
}

impl WebTools {
    pub fn new(search: Option<Box<dyn SearchBackend>>) -> Result<Self> {
        Self::with_timeout(search, FETCH_TIMEOUT)
    }

    fn with_timeout(search: Option<Box<dyn SearchBackend>>, timeout: Duration) -> Result<Self> {
        let client = reqwest::Client::builder()
            // Redirects are followed manually so every hop passes the SSRF
            // guard (a public host can 302 to an internal address).
            .redirect(reqwest::redirect::Policy::none())
            .timeout(timeout)
            .user_agent("kloop/0.1")
            .build()
            .context("building http client")?;
        Ok(WebTools { client, search })
    }

    pub fn search_backend_name(&self) -> Option<&'static str> {
        self.search.as_ref().map(|search| search.name())
    }

    pub async fn fetch(&self, input: &Value) -> Result<String> {
        let request: FetchRequest = parse_input(input, "web_fetch")?;
        fetch::fetch_url(&self.client, &request.url, /*allow_private*/ false).await
    }

    pub async fn search(&self, input: &Value) -> Result<String> {
        let Some(search) = &self.search else {
            bail!("web_search is not enabled (no search backend configured)");
        };
        let request: SearchRequest = parse_input(input, "web_search")?;
        if request.query.chars().count() < 2 {
            bail!("web_search: query must contain at least 2 characters");
        }
        if !request.allowed_domains.is_empty() && !request.blocked_domains.is_empty() {
            bail!(
                "web_search: cannot specify both allowed_domains and blocked_domains in the same request"
            );
        }
        let hits = search
            .search(&self.client, &request.query, SEARCH_RESULTS_DEFAULT)
            .await?;
        let hits = search::filter_hits(hits, &request.allowed_domains, &request.blocked_domains);
        Ok(search::format_hits(&hits))
    }
}

fn parse_input<'de, T: Deserialize<'de>>(input: &'de Value, tool: &str) -> Result<T> {
    T::deserialize(input).with_context(|| format!("{tool}: invalid input"))
}

#[cfg(test)]
pub(crate) mod testutil {
    /// A WebTools whose fetch skips the SSRF guard so wiremock (loopback)
    /// is reachable. The guard itself is unit-tested directly.
    pub(crate) async fn fetch_private(url: &str) -> anyhow::Result<String> {
        let tools = super::WebTools::new(None).unwrap();
        super::fetch::fetch_url(&tools.client, url, /*allow_private*/ true).await
    }

    pub(crate) fn client() -> reqwest::Client {
        super::WebTools::new(None).unwrap().client
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::Mutex;

    use serde_json::json;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    use super::*;

    type SearchCalls = Arc<Mutex<Vec<(String, usize)>>>;

    struct StaticSearch {
        hits: Vec<SearchHit>,
        calls: SearchCalls,
    }

    impl SearchBackend for StaticSearch {
        fn name(&self) -> &'static str {
            "static"
        }

        fn search<'a>(
            &'a self,
            _client: &'a reqwest::Client,
            query: &'a str,
            count: usize,
        ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchHit>>> + Send + 'a>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((query.to_string(), count));
                Ok(self.hits.clone())
            })
        }
    }

    fn static_tools(hits: Vec<SearchHit>) -> (WebTools, SearchCalls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let tools = WebTools::new(Some(Box::new(StaticSearch {
            hits,
            calls: calls.clone(),
        })))
        .unwrap();
        (tools, calls)
    }

    #[test]
    fn reports_configured_search_backend() {
        let fetch_only = WebTools::new(None).unwrap();
        assert_eq!(fetch_only.search_backend_name(), None);

        let with_search = WebTools::new(Some(Box::new(Brave::new("test-key".into())))).unwrap();
        assert_eq!(with_search.search_backend_name(), Some("brave"));
    }

    #[tokio::test]
    async fn inputs_are_strict_and_validate_before_network() {
        let tools = WebTools::new(None).unwrap();
        let err = tools.fetch(&json!({})).await.unwrap_err();
        assert!(format!("{err:#}").contains("missing field `url`"));

        let err = tools
            .fetch(&json!({"url": "https://example.com", "prompt": "ignored"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("unknown field `prompt`"));

        let (tools, calls) = static_tools(Vec::new());
        for input in [
            json!({"query": "x"}),
            json!({"query": "rust", "max_results": 2}),
            json!({"query": 48}),
            json!({
                "query": "rust",
                "allowed_domains": ["example.com"],
                "blocked_domains": ["blocked.example.com"]
            }),
        ] {
            assert!(tools.search(&input).await.is_err(), "{input}");
        }
        assert!(calls.lock().unwrap().is_empty());

        let disabled = WebTools::new(None).unwrap();
        let err = disabled
            .search(&json!({"query": "rust"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not enabled"));
    }

    #[tokio::test]
    async fn production_fetch_rejects_loopback_without_a_request() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/private"))
            .respond_with(ResponseTemplate::new(200).set_body_string("must not be reached"))
            .mount(&server)
            .await;

        let tools = WebTools::new(None).unwrap();
        let err = tools
            .fetch(&json!({"url": format!("{}/private", server.uri())}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("SSRF guard"));
        assert_eq!(server.received_requests().await.unwrap().len(), 0);
    }

    #[tokio::test]
    async fn search_filters_domains_and_uses_internal_result_limit() {
        let hits = vec![
            SearchHit {
                title: "Allowed".into(),
                url: "https://example.com/a".into(),
                snippet: "A".into(),
            },
            SearchHit {
                title: "Blocked subdomain".into(),
                url: "https://blocked.example.com/b".into(),
                snippet: "B".into(),
            },
            SearchHit {
                title: "Suffix trap".into(),
                url: "https://badexample.com/c".into(),
                snippet: "C".into(),
            },
            SearchHit {
                title: "Invalid".into(),
                url: "javascript:alert(1)".into(),
                snippet: "D".into(),
            },
        ];
        let (tools, calls) = static_tools(hits);
        let output = tools
            .search(&json!({
                "query": "rust agent",
                "allowed_domains": ["EXAMPLE.com."]
            }))
            .await
            .unwrap();

        assert_eq!(
            output,
            "1. Allowed\n   https://example.com/a\n   A\n2. Blocked subdomain\n   https://blocked.example.com/b\n   B"
        );
        assert_eq!(
            *calls.lock().unwrap(),
            vec![("rust agent".into(), SEARCH_RESULTS_DEFAULT)]
        );
    }

    #[tokio::test]
    async fn search_timeout_is_bounded_by_the_http_client() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_millis(100))
                    .set_body_json(json!({"results": []})),
            )
            .mount(&server)
            .await;
        let tools = WebTools::with_timeout(
            Some(Box::new(Tavily::with_base("test".into(), server.uri()))),
            Duration::from_millis(20),
        )
        .unwrap();

        let err = tools
            .search(&json!({"query": "timeout"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("request to tavily failed"));
    }
}
