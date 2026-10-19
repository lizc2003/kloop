use kloop_web::{Brave, Tavily, WebTools};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn tavily_forwards_constraints_and_preserves_source_dates() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [
                {"title": "Select", "url": "https://docs.rs/tokio/select", "content": "Cancellation", "published_date": "2026-10-01"},
                {"title": "Outside", "url": "https://bad-docs.rs/select", "content": "Ignored"}
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;
    let tools = WebTools::new(Some(Box::new(Tavily::with_base(
        "test".into(),
        server.uri(),
    ))))
    .unwrap();
    let output = tools
        .search(&json!({
            "query": "tokio \"cancellation safety\"",
            "allowed_domains": ["*.DOCS.rs."],
            "start_date": "2026-01-01",
            "end_date": "2026-10-19",
            "exact_match": true
        }))
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().unwrap(),
        json!({
            "query": "tokio \"cancellation safety\"",
            "max_results": 5,
            "search_depth": "basic",
            "include_domains": ["docs.rs"],
            "include_domains_mode": "restrict",
            "start_date": "2026-01-01",
            "end_date": "2026-10-19",
            "filter_by_published_date": true,
            "include_published_date": true,
            "exact_match": true
        })
    );
    assert_eq!(
        output,
        "1. Select\n   https://docs.rs/tokio/select\n   Cancellation\n   Date (published or updated; provider estimate): 2026-10-01"
    );
}

#[tokio::test]
async fn tavily_sends_domain_restrictions_before_retrieval() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .expect(1)
        .mount(&server)
        .await;
    let tools = WebTools::new(Some(Box::new(Tavily::with_base(
        "test".into(),
        server.uri(),
    ))))
    .unwrap();
    assert_eq!(
        tools
            .search(&json!({"query": "rust", "allowed_domains": ["DOCS.rs"]}))
            .await
            .unwrap(),
        "No results found"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0].body_json::<serde_json::Value>().unwrap()["include_domains"],
        json!(["docs.rs"])
    );
}

#[tokio::test]
async fn malformed_success_is_not_a_search_with_no_results() {
    for body in [
        json!({}),
        json!({"results": null}),
        json!({"results": {}}),
        json!({"results": [null]}),
        json!({"results": [{"url": 42}]}),
        json!({"results": [{"url": "https://example.com", "title": []}]}),
        json!({"results": [{"url": "https://example.com", "content": {}}]}),
        json!({"results": [{"url": "https://example.com", "published_date": 123}]}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let tools = WebTools::new(Some(Box::new(Tavily::with_base(
            "test".into(),
            server.uri(),
        ))))
        .unwrap();
        let error = tools.search(&json!({"query": "rust"})).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid search response"),
            "{body}: {error:#}"
        );
    }
}

#[tokio::test]
async fn invalid_constraints_do_not_send_any_requests() {
    let server = MockServer::start().await;
    let tools = WebTools::new(Some(Box::new(Tavily::with_base(
        "test".into(),
        server.uri(),
    ))))
    .unwrap();
    for input in [
        json!({"query": "  "}),
        json!({"query": "rust", "start_date": "2025-02-29"}),
        json!({"query": "rust", "start_date": "2024-2-29"}),
        json!({"query": "rust", "end_date": "2026-13-01"}),
        json!({"query": "rust", "start_date": "2026-10-19", "end_date": "2026-10-01"}),
        json!({"query": "rust", "exact_match": true}),
        json!({"query": "rust \"\"", "exact_match": true}),
        json!({"query": "rust \"unterminated", "exact_match": true}),
        json!({"query": "rust", "allowed_domains": [""]}),
        json!({"query": "rust", "blocked_domains": ["not a domain"]}),
        json!({"query": "rust", "allowed_domains": ["https://user:pass@example.com"]}),
        json!({"query": "rust", "allowed_domains": ["file:///tmp/private"]}),
    ] {
        assert!(tools.search(&input).await.is_err(), "{input}");
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn blocked_domains_and_one_sided_dates_are_forwarded_to_tavily() {
    for field in ["start_date", "end_date"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
                {"url": "https://blocked.example.com/a", "title": "Blocked"},
                {"url": "https://example.com/b", "title": "Kept", "published_date": null}
            ]})))
            .expect(1)
            .mount(&server)
            .await;
        let tools = WebTools::new(Some(Box::new(Tavily::with_base(
            "test".into(),
            server.uri(),
        ))))
        .unwrap();
        let mut input =
            json!({"query": "rust", "blocked_domains": ["https://BLOCKED.example.com/path"]});
        input[field] = "2024-02-29".into();
        assert_eq!(
            tools.search(&input).await.unwrap(),
            "1. Kept\n   https://example.com/b\n   "
        );
        let requests = server.received_requests().await.unwrap();
        let mut expected = json!({
            "query": "rust", "max_results": 5, "search_depth": "basic",
            "include_published_date": true, "exact_match": false,
            "exclude_domains": ["blocked.example.com"], "filter_by_published_date": true
        });
        expected[field] = "2024-02-29".into();
        assert_eq!(
            requests[0].body_json::<serde_json::Value>().unwrap(),
            expected
        );
    }
}

#[tokio::test]
async fn locally_removed_candidates_are_distinct_from_no_results() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"url": "https://badexample.com/a"}, {"url": "javascript:alert(1)"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    let tools = WebTools::new(Some(Box::new(Tavily::with_base(
        "test".into(),
        server.uri(),
    ))))
    .unwrap();
    assert_eq!(
        tools
            .search(&json!({"query": "rust", "allowed_domains": ["example.com"]}))
            .await
            .unwrap(),
        "No results remain: all 2 returned candidates were removed by local URL/domain validation. This does not establish that the requested sites have no matches."
    );
}

#[tokio::test]
async fn brave_forwards_domains_dates_and_quoted_phrases() {
    let server = MockServer::start().await;
    Mock::given(method("GET")).and(path("/res/v1/web/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"web": {"results": [
            {"title": "Rust", "url": "https://doc.rust-lang.org/book", "description": "Book", "page_age": "2026-10-01"}
        ]}}))).expect(1).mount(&server).await;
    let tools = WebTools::new(Some(Box::new(Brave::with_base(
        "test".into(),
        server.uri(),
    ))))
    .unwrap();
    let output = tools
        .search(&json!({
            "query": "rust \"borrow checker\"", "allowed_domains": ["RUST-lang.org", "docs.rs"],
            "start_date": "2026-01-01", "end_date": "2026-10-19", "exact_match": true
        }))
        .await
        .unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests[0].url.query_pairs().into_owned().collect::<Vec<_>>(), vec![
        ("q".into(), "rust \"borrow checker\" site:rust-lang.org OR rust \"borrow checker\" site:docs.rs".into()),
        ("count".into(), "5".into()), ("operators".into(), "true".into()),
        ("freshness".into(), "2026-01-01to2026-10-19".into())
    ]);
    assert_eq!(
        output,
        "1. Rust\n   https://doc.rust-lang.org/book\n   Book\n   Date (published or updated; provider estimate): 2026-10-01"
    );
}

#[tokio::test]
async fn brave_excludes_domains_and_rejects_one_sided_date_before_network() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/res/v1/web/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"web": {"results": []}})))
        .expect(1)
        .mount(&server)
        .await;
    let tools = WebTools::new(Some(Box::new(Brave::with_base(
        "test".into(),
        server.uri(),
    ))))
    .unwrap();
    for field in ["start_date", "end_date"] {
        let mut input = json!({"query": "rust"});
        input[field] = "2026-10-01".into();
        let error = tools.search(&input).await.unwrap_err();
        assert!(format!("{error:#}").contains("brave requires both start_date and end_date"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
    assert_eq!(
        tools
            .search(&json!({"query": "rust", "blocked_domains": ["example.com", "docs.rs"]}))
            .await
            .unwrap(),
        "No results found"
    );
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests[0]
            .url
            .query_pairs()
            .find(|(name, _)| name == "q")
            .unwrap()
            .1,
        "rust NOT site:example.com NOT site:docs.rs"
    );
}

#[tokio::test]
async fn brave_malformed_results_are_errors() {
    for body in [
        json!({}),
        json!({"web": {}}),
        json!({"web": {"results": "wrong"}}),
        json!({"web": {"results": [42]}}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/res/v1/web/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let tools = WebTools::new(Some(Box::new(Brave::with_base(
            "test".into(),
            server.uri(),
        ))))
        .unwrap();
        let error = tools.search(&json!({"query": "rust"})).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("invalid search response"),
            "{body}: {error:#}"
        );
    }
}
