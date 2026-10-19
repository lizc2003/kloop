//! Pluggable search backends. Adding a provider = implement [`SearchBackend`]
//! and add an arm to the CLI's provider match; the tool shape stays fixed.

use std::future::Future;
use std::pin::Pin;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use chrono::NaiveDate;
use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

use crate::limits;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default)]
    pub allowed_domains: Vec<String>,
    #[serde(default)]
    pub blocked_domains: Vec<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    #[serde(default)]
    pub exact_match: bool,
}

impl SearchRequest {
    pub(crate) fn validate(&mut self) -> Result<()> {
        if self.query.trim().chars().count() < 2 {
            bail!("web_search: query must contain at least 2 non-padding characters");
        }
        if !self.allowed_domains.is_empty() && !self.blocked_domains.is_empty() {
            bail!(
                "web_search: cannot specify both allowed_domains and blocked_domains in the same request"
            );
        }
        for (field, value) in [
            ("start_date", &self.start_date),
            ("end_date", &self.end_date),
        ] {
            if let Some(value) = value {
                let valid_shape = value.len() == 10
                    && value.bytes().enumerate().all(|(i, ch)| match i {
                        4 | 7 => ch == b'-',
                        _ => ch.is_ascii_digit(),
                    });
                if !valid_shape
                    || value.starts_with("0000")
                    || NaiveDate::parse_from_str(value, "%Y-%m-%d").is_err()
                {
                    bail!("web_search: {field} must be a valid YYYY-MM-DD date");
                }
            }
        }
        if let (Some(start), Some(end)) = (&self.start_date, &self.end_date)
            && start > end
        {
            bail!("web_search: start_date must not be after end_date");
        }
        if self.exact_match {
            let parts: Vec<_> = self.query.split('"').collect();
            if parts.len().is_multiple_of(2)
                || !parts
                    .iter()
                    .skip(1)
                    .step_by(2)
                    .any(|part| !part.trim().is_empty())
            {
                bail!(
                    "web_search: exact_match requires a non-empty phrase in paired double quotes in query"
                );
            }
        }
        self.allowed_domains = normalize_domains(&self.allowed_domains)?;
        self.blocked_domains = normalize_domains(&self.blocked_domains)?;
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub date: Option<String>,
}

pub trait SearchBackend: Send + Sync {
    /// Short lowercase name surfaced in the tool description ("brave").
    fn name(&self) -> &'static str;
    fn search<'a>(
        &'a self,
        client: &'a reqwest::Client,
        request: &'a SearchRequest,
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
            let mut text = format!(
                "{}. {}\n   {}\n   {}",
                i + 1,
                hit.title,
                hit.url,
                hit.snippet
            );
            if let Some(date) = &hit.date {
                text.push_str(&format!(
                    "\n   Date (published or updated; provider estimate): {date}"
                ));
            }
            text
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

fn normalize_domains(raw_domains: &[String]) -> Result<Vec<String>> {
    let mut normalized = Vec::new();
    for raw_domain in raw_domains {
        let raw_domain = raw_domain.trim().trim_start_matches("*.");
        let parsed = if raw_domain.contains("://") {
            Url::parse(raw_domain)
        } else {
            Url::parse(&format!("https://{raw_domain}"))
        }
        .context("web_search: invalid domain filter")?;
        if !matches!(parsed.scheme(), "http" | "https")
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            bail!("web_search: domain filters require an HTTP(S) host without credentials");
        }
        let host = parsed
            .host_str()
            .map(|domain| domain.trim_end_matches('.').to_ascii_lowercase())
            .filter(|domain| !domain.is_empty())
            .context("web_search: invalid domain filter")?;
        if !normalized.contains(&host) {
            normalized.push(host);
        }
    }
    Ok(normalized)
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
    loop {
        let next = resp
            .chunk()
            .await
            .with_context(|| format!("web_search: reading {name} response failed"))?;
        let Some(chunk) = next else {
            break;
        };
        if body.len().saturating_add(chunk.len()) > limits::MAX_SEARCH_DOWNLOAD_BYTES {
            let limit = limits::MAX_SEARCH_DOWNLOAD_BYTES;
            bail!("web_search: {name} response exceeded {limit} bytes");
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
fn hits_from(
    results: &Value,
    count: usize,
    snippet_field: &str,
    date_field: &str,
    name: &str,
) -> Result<Vec<SearchHit>> {
    let results = results.as_array().with_context(|| {
        format!("web_search: {name} returned invalid search response (expected results array)")
    })?;
    results
        .iter()
        .take(count)
        .map(|r| {
            let invalid = || {
                format!(
                    "web_search: {name} returned invalid search response (malformed result item)"
                )
            };
            let url = r["url"].as_str().with_context(invalid)?;
            let text = |field: &str| -> Result<Option<String>> {
                match r.get(field) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(value)) => Ok(Some(crate::html::strip_inline_tags(value))
                        .filter(|s| !s.trim().is_empty())),
                    _ => bail!("{}", invalid()),
                }
            };
            Ok(SearchHit {
                title: text("title")?.unwrap_or_else(|| "(untitled)".into()),
                url: url.to_string(),
                snippet: text(snippet_field)?.unwrap_or_default(),
                date: text(date_field)?,
            })
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
        request: &'a SearchRequest,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchHit>>> + Send + 'a>> {
        Box::pin(async move {
            let mut query = if request.allowed_domains.is_empty() {
                request.query.clone()
            } else {
                request
                    .allowed_domains
                    .iter()
                    .map(|domain| format!("{} site:{domain}", request.query))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            };
            for domain in &request.blocked_domains {
                query.push_str(&format!(" NOT site:{domain}"));
            }
            let mut req = client
                .get(format!("{}/res/v1/web/search", self.base))
                .query(&[
                    ("q", query.as_str()),
                    ("count", &count.to_string()),
                    ("operators", "true"),
                ])
                .header("X-Subscription-Token", &self.key)
                .header("Accept", "application/json");
            match (&request.start_date, &request.end_date) {
                (Some(start), Some(end)) => {
                    req = req.query(&[("freshness", format!("{start}to{end}"))])
                }
                (None, None) => {}
                (Some(_), None) | (None, Some(_)) => bail!(
                    "web_search: brave requires both start_date and end_date for a date range"
                ),
            }
            let json = send_and_parse(req, "brave").await?;
            hits_from(
                &json["web"]["results"],
                count,
                "description",
                "page_age",
                "brave",
            )
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
        request: &'a SearchRequest,
        count: usize,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<SearchHit>>> + Send + 'a>> {
        Box::pin(async move {
            let mut payload = serde_json::json!({
                "query": request.query,
                "max_results": count,
                "search_depth": "basic",
                "include_published_date": true,
                "exact_match": request.exact_match,
            });
            if !request.allowed_domains.is_empty() {
                payload["include_domains"] = serde_json::json!(request.allowed_domains);
                payload["include_domains_mode"] = "restrict".into();
            }
            if !request.blocked_domains.is_empty() {
                payload["exclude_domains"] = serde_json::json!(request.blocked_domains);
            }
            if let Some(start) = &request.start_date {
                payload["start_date"] = start.clone().into();
            }
            if let Some(end) = &request.end_date {
                payload["end_date"] = end.clone().into();
            }
            if request.start_date.is_some() || request.end_date.is_some() {
                payload["filter_by_published_date"] = true.into();
            }
            let req = client
                .post(format!("{}/search", self.base))
                .bearer_auth(&self.key)
                .json(&payload);
            let json = send_and_parse(req, "tavily").await?;
            hits_from(
                &json["results"],
                count,
                "content",
                "published_date",
                "tavily",
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;
    use wiremock::matchers::query_param;

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
            .search(
                &client,
                &SearchRequest {
                    query: "rust agent".into(),
                    ..Default::default()
                },
                2,
            )
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
        let hits = tavily
            .search(
                &client,
                &SearchRequest {
                    query: "rust agent".into(),
                    ..Default::default()
                },
                2,
            )
            .await
            .unwrap();
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
            .search(
                &client,
                &SearchRequest {
                    query: "q".into(),
                    ..Default::default()
                },
                5,
            )
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
            .search(
                &client,
                &SearchRequest {
                    query: "nothing".into(),
                    ..Default::default()
                },
                5,
            )
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
            .search(
                &client,
                &SearchRequest {
                    query: "quota".into(),
                    ..Default::default()
                },
                5,
            )
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("HTTP 429") && msg.contains("rate limited"),
            "{msg}"
        );

        let hits = brave_at(&server)
            .search(
                &client,
                &SearchRequest {
                    query: "nothing".into(),
                    ..Default::default()
                },
                5,
            )
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
            date: None,
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
            &normalize_domains(&["*.EXAMPLE.com.".into()]).unwrap(),
            &normalize_domains(&["https://blocked.example.com/path".into()]).unwrap(),
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
            date: None,
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
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(vec![b'x'; 5 * 1024 * 1024 + 1], "application/json"),
            )
            .mount(&server)
            .await;
        let client = crate::testutil::client();
        let err = Tavily::with_base("k".into(), server.uri())
            .search(
                &client,
                &SearchRequest {
                    query: "oversized".into(),
                    ..Default::default()
                },
                5,
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("response exceeded 5242880 bytes"));

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string("not-json"))
            .mount(&server)
            .await;
        let err = Tavily::with_base("k".into(), server.uri())
            .search(
                &client,
                &SearchRequest {
                    query: "invalid".into(),
                    ..Default::default()
                },
                5,
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("invalid JSON"));
    }
}
