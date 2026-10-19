use kloop_web::{DeepSeek, SearchBackend, SearchRequest, WebTools};
use serde_json::{Value, json};
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tools(server: &MockServer) -> WebTools {
    WebTools::new(Some(Box::new(DeepSeek::with_base(
        "test-key".into(),
        server.uri(),
    ))))
    .unwrap()
}

#[tokio::test]
async fn forwards_constraints_auth_and_preserves_model_summary_and_date() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tools/search"))
        .and(header("Authorization", "Bearer test-key"))
        .and(body_json(json!({
            "query": "\"Rust\" after:2025-01-01 before:2026-01-01 site:docs.rs OR \"Rust\" after:2025-01-01 before:2026-01-01 site:rust-lang.org",
            "count": 5,
            "need_content": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": [
            {"title":"The <b>Rust</b> Book", "url":"https://doc.rust-lang.org/book/",
             "summary":"Model <b>summary</b>", "snippet":"Short display snippet",
             "content":"Full page not requested", "date":"2025-09-18T00:00:00+00:00"},
            {"title":"Outside", "url":"https://baddocs.rs/page", "summary":"Filtered"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        tools(&server)
            .search(&json!({
                "query":"\"Rust\"", "allowed_domains":["*.DOCS.rs.", "rust-lang.org"],
                "start_date":"2025-01-01", "end_date":"2026-01-01", "exact_match":true
            }))
            .await
            .unwrap(),
        "1. The Rust Book\n   https://doc.rust-lang.org/book/\n   Model summary\n   Date (published or updated; provider estimate): 2025-09-18T00:00:00+00:00"
    );
}

#[tokio::test]
async fn forwards_exclusions_and_each_one_sided_date() {
    for (date, operator) in [
        (None, ""),
        (Some("start_date"), " after:2025-01-01"),
        (Some("end_date"), " before:2025-01-01"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .and(body_json(json!({
                "query":format!("rust -site:example.com -site:docs.rs{operator}"),
                "count":5, "need_content":false
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
            .expect(1)
            .mount(&server)
            .await;
        let mut input =
            json!({"query":"rust", "blocked_domains":["https://EXAMPLE.com/path", "docs.rs"]});
        if let Some(date) = date {
            input[date] = "2025-01-01".into();
        }
        assert_eq!(
            tools(&server).search(&input).await.unwrap(),
            "No results found"
        );
    }
}

#[tokio::test]
async fn missing_dates_are_not_invented_and_results_remain_bounded() {
    let server = MockServer::start().await;
    let mut results: Vec<Value> = (0..6)
        .map(|i| {
            json!({
                "url":format!("https://example.com/{i}"), "title":format!("Result {i}"),
                "summary":"Long summary", "date":null
            })
        })
        .collect();
    results[0].as_object_mut().unwrap().remove("date");
    Mock::given(method("POST"))
        .and(path("/v1/tools/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":results})))
        .expect(1)
        .mount(&server)
        .await;
    let output = tools(&server)
        .search(&json!({"query":"rust"}))
        .await
        .unwrap();
    let expected = (0..5)
        .map(|i| {
            format!(
                "{}. Result {i}\n   https://example.com/{i}\n   Long summary",
                i + 1
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert_eq!(output, expected);
}

#[tokio::test]
async fn locally_rejected_candidates_are_not_reported_as_no_matches() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/tools/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[
            {"url":"javascript:alert(1)", "summary":"Unsafe URL"},
            {"url":"https://badexample.com", "summary":"Wrong host"}
        ]})))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        tools(&server)
            .search(&json!({"query":"rust", "allowed_domains":["example.com"]}))
            .await
            .unwrap(),
        "No results remain: all 2 returned candidates were removed by local URL/domain validation. This does not establish that the requested sites have no matches."
    );
}

#[tokio::test]
async fn malformed_success_responses_are_errors() {
    for body in [
        json!({}),
        json!({"results":null}),
        json!({"results":{}}),
        json!({"results":[null]}),
        json!({"results":[{"url":42}]}),
        json!({"results":[{"url":"https://example.com", "title":[]}]}),
        json!({"results":[{"url":"https://example.com", "summary":false}]}),
        json!({"results":[{"url":"https://example.com", "date":42}]}),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
            .expect(1)
            .mount(&server)
            .await;
        let error = tools(&server)
            .search(&json!({"query":"rust"}))
            .await
            .unwrap_err();
        assert!(
            format!("{error:#}").contains("deepseek returned invalid search response"),
            "{body}: {error:#}"
        );
    }
}

#[tokio::test]
async fn http_and_json_errors_are_not_empty_results() {
    for (status, body, expected) in [
        (401, "unauthorized", "deepseek returned HTTP 401"),
        (429, "too many requests", "deepseek returned HTTP 429"),
        (200, "not JSON", "deepseek returned invalid JSON"),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/tools/search"))
            .respond_with(ResponseTemplate::new(status).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        let error = tools(&server)
            .search(&json!({"query":"rust"}))
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
}

#[tokio::test]
async fn query_limit_counts_unicode_and_includes_generated_operators() {
    let server = MockServer::start().await;
    let query = "界".repeat(128);
    Mock::given(method("POST"))
        .and(path("/v1/tools/search"))
        .and(body_json(
            json!({"query":query, "count":5, "need_content":false}),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":[]})))
        .expect(1)
        .mount(&server)
        .await;
    let tools = tools(&server);
    assert_eq!(
        tools.search(&json!({"query":query})).await.unwrap(),
        "No results found"
    );
    for input in [
        json!({"query":"界".repeat(129)}),
        json!({"query":"r".repeat(120), "allowed_domains":["example.com"]}),
        json!({"query":"r".repeat(120), "start_date":"2025-01-01"}),
    ] {
        let error = tools.search(&input).await.unwrap_err();
        assert!(
            format!("{error:#}").contains("128 Unicode characters including search operators"),
            "{error:#}"
        );
    }
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_backend_counts_are_rejected_before_network() {
    let server = MockServer::start().await;
    let backend = DeepSeek::with_base("test-key".into(), server.uri());
    let client = reqwest::Client::new();
    let request = SearchRequest {
        query: "rust".into(),
        ..Default::default()
    };
    for count in [0, 51] {
        let error = backend.search(&client, &request, count).await.unwrap_err();
        assert!(format!("{error:#}").contains("deepseek result count must be between 1 and 50"));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
