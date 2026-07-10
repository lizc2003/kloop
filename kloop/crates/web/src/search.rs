//! Pluggable search backends. Adding a provider = implement [`SearchBackend`]
//! and add an arm to the CLI's provider match; the tool shape stays fixed.

use std::future::Future;
use std::pin::Pin;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use serde_json::Value;

#[derive(Debug)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

pub trait SearchBackend: Send + Sync {
    /// Short lowercase name surfaced in the tool description ("brave").
    fn name(&self) -> &'static str;
    fn search<'a>(
        &'a self,
        client: &'a reqwest::Client,
        query: &'a str,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchHit>>> + Send + 'a>>;
}

pub fn format_hits(hits: &[SearchHit]) -> String {
    if hits.is_empty() {
        return "No results found".into();
    }
    hits.iter()
        .enumerate()
        .map(|(i, hit)| {
            format!(
                "{}. {}\n   {}\n   {}",
                i + 1,
                hit.title,
                hit.url,
                hit.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Brave Search API: one GET with a subscription-token header.
/// <https://api-dashboard.search.brave.com/app/documentation>
pub struct Brave {
    key: String,
    base: String,
}

impl Brave {
    pub fn new(key: String) -> Self {
        Brave {
            key,
            base: "https://api.search.brave.com".into(),
        }
    }

    /// Test seam: point at a mock server.
    pub fn with_base(key: String, base: String) -> Self {
        Brave { key, base }
    }
}

impl SearchBackend for Brave {
    fn name(&self) -> &'static str {
        "brave"
    }

    fn search<'a>(
        &'a self,
        client: &'a reqwest::Client,
        query: &'a str,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchHit>>> + Send + 'a>> {
        Box::pin(async move {
            let resp = client
                .get(format!("{}/res/v1/web/search", self.base))
                .query(&[("q", query), ("count", &count.to_string())])
                .header("X-Subscription-Token", &self.key)
                .header("Accept", "application/json")
                .send()
                .await
                .context("web_search: request to brave failed")?;
            let status = resp.status();
            let body = resp
                .text()
                .await
                .context("web_search: reading brave response failed")?;
            if !status.is_success() {
                let head: String = body.chars().take(200).collect();
                bail!("web_search: brave returned HTTP {status}: {head}");
            }
            let json: Value =
                serde_json::from_str(&body).context("web_search: brave returned invalid JSON")?;
            let results = json["web"]["results"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            Ok(results
                .iter()
                .take(count)
                .map(|r| SearchHit {
                    title: crate::html::strip_inline_tags(
                        r["title"].as_str().unwrap_or("(untitled)"),
                    ),
                    url: r["url"].as_str().unwrap_or("").to_string(),
                    snippet: crate::html::strip_inline_tags(
                        r["description"].as_str().unwrap_or(""),
                    ),
                })
                .collect())
        })
    }
}

/// Tavily Search API: one JSON POST with a bearer token. The default
/// backend — free tier needs no card, and it is the most common choice in
/// the agent ecosystem. <https://docs.tavily.com/documentation/api-reference/endpoint/search>
pub struct Tavily {
    key: String,
    base: String,
}

impl Tavily {
    pub fn new(key: String) -> Self {
        Tavily {
            key,
            base: "https://api.tavily.com".into(),
        }
    }

    /// Test seam: point at a mock server.
    pub fn with_base(key: String, base: String) -> Self {
        Tavily { key, base }
    }
}

impl SearchBackend for Tavily {
    fn name(&self) -> &'static str {
        "tavily"
    }

    fn search<'a>(
        &'a self,
        client: &'a reqwest::Client,
        query: &'a str,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchHit>>> + Send + 'a>> {
        Box::pin(async move {
            let resp = client
                .post(format!("{}/search", self.base))
                .bearer_auth(&self.key)
                .json(&serde_json::json!({
                    "query": query,
                    "max_results": count,
                    "search_depth": "basic",
                }))
                .send()
                .await
                .context("web_search: request to tavily failed")?;
            let status = resp.status();
            let body = resp
                .text()
                .await
                .context("web_search: reading tavily response failed")?;
            if !status.is_success() {
                let head: String = body.chars().take(200).collect();
                bail!("web_search: tavily returned HTTP {status}: {head}");
            }
            let json: Value =
                serde_json::from_str(&body).context("web_search: tavily returned invalid JSON")?;
            let results = json["results"].as_array().cloned().unwrap_or_default();
            Ok(results
                .iter()
                .take(count)
                .map(|r| SearchHit {
                    title: crate::html::strip_inline_tags(
                        r["title"].as_str().unwrap_or("(untitled)"),
                    ),
                    url: r["url"].as_str().unwrap_or("").to_string(),
                    snippet: crate::html::strip_inline_tags(r["content"].as_str().unwrap_or("")),
                })
                .collect())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;

    fn brave_at(server: &MockServer) -> Brave {
        Brave::with_base("test-key".into(), server.uri())
    }

    #[tokio::test]
    async fn brave_formats_query_and_parses_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "rust agent"))
            .and(query_param("count", "2"))
            .and(header("X-Subscription-Token", "test-key"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "web": {"results": [
                    {"title": "The <strong>Rust</strong> Book", "url": "https://doc.rust-lang.org/book/", "description": "Learn <strong>Rust</strong>"},
                    {"title": "rust-lang/rust", "url": "https://github.com/rust-lang/rust", "description": "The Rust repo"}
                ]}
            })))
            .mount(&server)
            .await;

        let client = crate::testutil::client();
        let hits = brave_at(&server)
            .search(&client, "rust agent", 2)
            .await
            .unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "The Rust Book", "highlight markup stripped");
        assert_eq!(hits[0].snippet, "Learn Rust");

        let text = format_hits(&hits);
        assert!(
            text.starts_with("1. The Rust Book\n   https://doc.rust-lang.org/book/\n   Learn Rust")
        );
        assert!(text.contains("2. rust-lang/rust"));
    }

    #[tokio::test]
    async fn tavily_posts_json_and_parses_results() {
        use wiremock::matchers::body_partial_json;
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("Authorization", "Bearer tvly-test"))
            .and(body_partial_json(json!({"query": "rust agent", "max_results": 2})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "query": "rust agent",
                "results": [
                    {"title": "The Rust Book", "url": "https://doc.rust-lang.org/book/", "content": "Learn Rust", "score": 0.9},
                    {"title": "rust-lang/rust", "url": "https://github.com/rust-lang/rust", "content": "The Rust repo", "score": 0.8}
                ]
            })))
            .mount(&server)
            .await;

        let client = crate::testutil::client();
        let tavily = Tavily::with_base("tvly-test".into(), server.uri());
        assert_eq!(tavily.name(), "tavily");
        let hits = tavily.search(&client, "rust agent", 2).await.unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].title, "The Rust Book");
        assert_eq!(hits[0].snippet, "Learn Rust");
        assert!(format_hits(&hits).contains("2. rust-lang/rust"));
    }

    #[tokio::test]
    async fn tavily_maps_errors_and_empty_results() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(401).set_body_string("invalid api key"))
            .mount(&server)
            .await;
        let client = crate::testutil::client();
        let err = Tavily::with_base("bad".into(), server.uri())
            .search(&client, "q", 5)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("HTTP 401") && msg.contains("invalid api key"),
            "{msg}"
        );

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
            .mount(&server)
            .await;
        let hits = Tavily::with_base("k".into(), server.uri())
            .search(&client, "nothing", 5)
            .await
            .unwrap();
        assert_eq!(format_hits(&hits), "No results found");
    }

    #[tokio::test]
    async fn brave_maps_errors_and_empty_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "quota"))
            .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .and(query_param("q", "nothing"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"web": {"results": []}})))
            .mount(&server)
            .await;

        let client = crate::testutil::client();
        let err = brave_at(&server)
            .search(&client, "quota", 5)
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("HTTP 429") && msg.contains("rate limited"),
            "{msg}"
        );

        let hits = brave_at(&server)
            .search(&client, "nothing", 5)
            .await
            .unwrap();
        assert_eq!(format_hits(&hits), "No results found");
    }
}
