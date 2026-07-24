//! kloop-web — network implementations for web_fetch / web_search. The
//! agent-facing names and schemas live in `kloop-core::tools::web`; the CLI
//! glues these operations into core's `ToolSource` seam. Search backends are
//! pluggable behind [`SearchBackend`].

mod fetch;
mod html;
mod search;

use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

pub use search::Brave;
pub use search::SearchBackend;
pub use search::SearchHit;
pub use search::Tavily;

const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const SEARCH_RESULTS_DEFAULT: usize = 5;
const SEARCH_RESULTS_MAX: usize = 10;

pub struct WebTools {
    client: reqwest::Client,
    search: Option<Box<dyn SearchBackend>>,
}

impl WebTools {
    pub fn new(search: Option<Box<dyn SearchBackend>>) -> Result<Self> {
        let client = reqwest::Client::builder()
            // Redirects are followed manually so every hop passes the SSRF
            // guard (a public host can 302 to an internal address).
            .redirect(reqwest::redirect::Policy::none())
            .timeout(FETCH_TIMEOUT)
            .user_agent("kloop/0.1")
            .build()
            .context("building http client")?;
        Ok(WebTools { client, search })
    }

    pub fn search_backend_name(&self) -> Option<&'static str> {
        self.search.as_ref().map(|search| search.name())
    }

    pub async fn fetch(&self, input: &Value) -> Result<String> {
        let url = str_arg(input, "url", "web_fetch")?;
        fetch::fetch_url(&self.client, url, /*allow_private*/ false).await
    }

    pub async fn search(&self, input: &Value) -> Result<String> {
        let Some(search) = &self.search else {
            bail!("web_search is not enabled (no search backend configured)");
        };
        let query = str_arg(input, "query", "web_search")?;
        let count = input["max_results"]
            .as_u64()
            .map_or(SEARCH_RESULTS_DEFAULT, |n| {
                (n as usize).clamp(1, SEARCH_RESULTS_MAX)
            });
        let hits = search.search(&self.client, query, count).await?;
        Ok(search::format_hits(&hits))
    }
}

fn str_arg<'a>(input: &'a Value, key: &str, tool: &str) -> Result<&'a str> {
    input[key]
        .as_str()
        .ok_or_else(|| anyhow!("{tool}: missing required string argument '{key}'"))
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
    use super::*;

    #[test]
    fn reports_configured_search_backend() {
        let fetch_only = WebTools::new(None).unwrap();
        assert_eq!(fetch_only.search_backend_name(), None);

        let with_search = WebTools::new(Some(Box::new(Brave::new("test-key".into())))).unwrap();
        assert_eq!(with_search.search_backend_name(), Some("brave"));
    }

    #[tokio::test]
    async fn call_validates_arguments() {
        let tools = WebTools::new(None).unwrap();
        let err = tools.fetch(&serde_json::json!({})).await.unwrap_err();
        assert!(format!("{err:#}").contains("missing required string argument 'url'"));

        let err = tools
            .search(&serde_json::json!({"query": "x"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not enabled"));
    }
}
