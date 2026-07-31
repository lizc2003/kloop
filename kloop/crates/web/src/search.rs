//! Pluggable search backends. Adding a provider = implement [`SearchBackend`]
//! and add an arm to the CLI's provider match; the tool shape stays fixed.

use std::future::Future;
use std::pin::Pin;

use anyhow::bail;
use anyhow::Context;
use anyhow::Result;
use reqwest::Url;
use serde_json::Value;

use crate::limits;

#[derive(Clone, Debug, PartialEq, Eq)]
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
    let rendered = hits
        .iter()
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
        .join("\n");
    let (mut bounded, truncated) = limits::truncate_chars(rendered, limits::MAX_TEXT_CHARS);
    if truncated {
        bounded.push_str("\n\n[search results truncated at 50000 characters]");
    }
    bounded
}

/// Keep only valid HTTP(S) result URLs that satisfy the model-provided domain
/// filters. A blocked domain always wins when both lists match.
pub(crate) fn filter_hits(
    hits: Vec<SearchHit>,
    allowed_domains: &[String],
    blocked_domains: &[String],
) -> Vec<SearchHit> {
    let restrict_to_allowed = !allowed_domains.is_empty();
    let allowed_domains = normalize_domains(allowed_domains);
    let blocked_domains = normalize_domains(blocked_domains);
    hits.into_iter()
        .filter(|hit| {
            let Ok(url) = Url::parse(&hit.url) else {
                return false;
            };
            if !matches!(url.scheme(), "http" | "https") {
                return false;
            }
            let Some(host) = url
                .host_str()
                .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
            else {
                return false;
            };
            let allowed = !restrict_to_allowed
                || allowed_domains
                    .iter()
                    .any(|domain| domain_matches(&host, domain));
            allowed
                && !blocked_domains
                    .iter()
                    .any(|domain| domain_matches(&host, domain))
        })
        .collect()
}

fn normalize_domains(raw_domains: &[String]) -> Vec<String> {
    raw_domains
        .iter()
        .filter_map(|raw_domain| {
            let raw_domain = raw_domain.trim().trim_start_matches("*.");
            if raw_domain.is_empty() {
                return None;
            }
            let parsed = if raw_domain.contains("://") {
                Url::parse(raw_domain).ok()
            } else {
                Url::parse(&format!("https://{raw_domain}")).ok()
            }?;
            parsed
                .host_str()
                .map(|domain| domain.trim_end_matches('.').to_ascii_lowercase())
                .filter(|domain| !domain.is_empty())
        })
        .collect()
}

fn domain_matches(host: &str, domain: &str) -> bool {
    host == domain
        || host
            .strip_suffix(domain)
            .is_some_and(|prefix| prefix.ends_with('.'))
}

/// Send a backend's built request, validate the status, and parse the body —
/// the skeleton every backend shares. `name` labels the errors.
async fn send_and_parse(req: reqwest::RequestBuilder, name: &str) -> Result<Value> {
    let mut resp = req
        .send()
        .await
        .with_context(|| format!("web_search: request to {name} failed"))?;
    let status = resp.status();
    let mut body = Vec::new();
    while let Some(chunk) = resp
        .chunk()
        .await
        .with_context(|| format!("web_search: reading {name} response failed"))?
    {
        if body.len().saturating_add(chunk.len()) > limits::MAX_DOWNLOAD_BYTES {
            bail!("web_search: {name} response exceeded 5MB");
        }
        body.extend_from_slice(&chunk);
    }
    if !status.is_success() {
        let head: String = String::from_utf8_lossy(&body).chars().take(200).collect();
        bail!("web_search: {name} returned HTTP {status}: {head}");
    }
    serde_json::from_slice(&body)
        .with_context(|| format!("web_search: {name} returned invalid JSON"))
}

/// Map a backend's result array to hits. `snippet_field` is the per-backend
/// key holding the summary ("description" for Brave, "content" for Tavily).
fn hits_from(results: &[Value], count: usize, snippet_field: &str) -> Vec<SearchHit> {
    results
        .iter()
        .take(count)
        .map(|r| SearchHit {
            title: crate::html::strip_inline_tags(r["title"].as_str().unwrap_or("(untitled)")),
            url: r["url"].as_str().unwrap_or("").to_string(),
            snippet: crate::html::strip_inline_tags(r[snippet_field].as_str().unwrap_or("")),
        })
        .collect()
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
            let req = client
                .get(format!("{}/res/v1/web/search", self.base))
                .query(&[("q", query), ("count", &count.to_string())])
                .header("X-Subscription-Token", &self.key)
                .header("Accept", "application/json");
            let json = send_and_parse(req, "brave").await?;
            let results = json["web"]["results"].as_array().map(Vec::as_slice);
            Ok(hits_from(results.unwrap_or_default(), count, "description"))
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
            let req = client
                .post(format!("{}/search", self.base))
                .bearer_auth(&self.key)
                .json(&serde_json::json!({
                    "query": query,
                    "max_results": count,
                    "search_depth": "basic",
                }));
            let json = send_and_parse(req, "tavily").await?;
            let results = json["results"].as_array().map(Vec::as_slice);
            Ok(hits_from(results.unwrap_or_default(), count, "content"))
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

    #[test]
    fn domain_filters_use_host_boundaries_and_blocked_wins() {
        let hit = |url: &str| SearchHit {
            title: url.into(),
            url: url.into(),
            snippet: String::new(),
        };
        let hits = vec![
            hit("https://example.com/a"),
            hit("https://docs.example.com/b"),
            hit("https://blocked.example.com/c"),
            hit("https://badexample.com/d"),
            hit("file:///tmp/not-web"),
        ];
        let filtered = filter_hits(
            hits,
            &["*.EXAMPLE.com.".into()],
            &["https://blocked.example.com/path".into()],
        );
        assert_eq!(
            filtered
                .iter()
                .map(|hit| hit.url.as_str())
                .collect::<Vec<_>>(),
            vec!["https://example.com/a", "https://docs.example.com/b"]
        );
        assert!(filter_hits(vec![hit("https://example.com/a")], &[String::new()], &[]).is_empty());
    }

    #[test]
    fn formatted_results_have_a_unicode_safe_total_cap() {
        let output = format_hits(&[SearchHit {
            title: "界".repeat(60_000),
            url: "https://example.com".into(),
            snippet: "tail".into(),
        }]);
        assert!(
            output.ends_with("[search results truncated at 50000 characters]"),
            "{output:?}"
        );
        assert!(output.chars().count() < 50_100);
    }

    #[tokio::test]
    async fn backend_response_body_is_bounded_and_json_must_be_valid() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                vec![b'x'; limits::MAX_DOWNLOAD_BYTES + 1],
                "application/json",
            ))
            .mount(&server)
            .await;
        let client = crate::testutil::client();
        let err = Tavily::with_base("k".into(), server.uri())
            .search(&client, "oversized", 5)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("response exceeded 5MB"));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let err = Tavily::with_base("k".into(), server.uri())
            .search(&client, "invalid", 5)
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON"));
    }
}
