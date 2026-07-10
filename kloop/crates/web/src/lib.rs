//! kloop-web — web_fetch / web_search implementations. Depends on protocol
//! only (plus reqwest); the CLI glues this into core's `ToolSource` seam the
//! same way it glues MCP, keeping core network-free. Search backends are
//! pluggable behind [`SearchBackend`]; Brave is the first implementation.

mod fetch;
mod html;
mod search;

use std::time::Duration;

use anyhow::anyhow;
use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::json;
use serde_json::Value;

use kloop_protocol::ToolDef;

pub use search::Brave;
pub use search::SearchBackend;
pub use search::SearchHit;

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

    pub fn defs(&self) -> Vec<ToolDef> {
        let mut defs = vec![ToolDef {
            name: "web_fetch".into(),
            description: "Fetch a URL and return its content as plain text (HTML is converted, tags stripped). HTTP is upgraded to HTTPS. Same-host redirects are followed; a cross-host redirect is reported back so you can fetch the new URL explicitly. Refuses private/internal addresses. Long pages are truncated.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "Full URL to fetch (http or https)"}
                },
                "required": ["url"]
            }),
        }];
        if let Some(search) = &self.search {
            defs.push(ToolDef {
                name: "web_search".into(),
                description: format!(
                    "Search the web (via {}). Returns the top results as title, URL and snippet; fetch a result with web_fetch for the full page.",
                    search.name()
                ),
                schema: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "The search query"},
                        "max_results": {"type": "integer", "description": "Number of results (1-10, default 5)"}
                    },
                    "required": ["query"]
                }),
            });
        }
        defs
    }

    pub async fn call(&self, tool: &str, input: &Value) -> Result<String> {
        match tool {
            "web_fetch" => {
                let url = str_arg(input, "url", "web_fetch")?;
                fetch::fetch_url(&self.client, url, /*allow_private*/ false).await
            }
            "web_search" => {
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
            other => Err(anyhow!("unknown web tool: {other}")),
        }
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
    fn defs_expose_search_only_with_a_backend() {
        let fetch_only = WebTools::new(None).unwrap();
        let names: Vec<String> = fetch_only.defs().into_iter().map(|d| d.name).collect();
        assert_eq!(names, vec!["web_fetch"]);

        let with_search = WebTools::new(Some(Box::new(Brave::new("test-key".into())))).unwrap();
        let defs = with_search.defs();
        let names: Vec<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["web_fetch", "web_search"]);
        assert!(
            defs[1].description.contains("brave"),
            "backend named in description"
        );
    }

    #[tokio::test]
    async fn call_validates_arguments() {
        let tools = WebTools::new(None).unwrap();
        let err = tools.call("web_fetch", &json!({})).await.unwrap_err();
        assert!(format!("{err:#}").contains("missing required string argument 'url'"));

        let err = tools
            .call("web_search", &json!({"query": "x"}))
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("not enabled"));

        let err = tools.call("nope", &json!({})).await.unwrap_err();
        assert!(format!("{err:#}").contains("unknown web tool"));
    }
}
