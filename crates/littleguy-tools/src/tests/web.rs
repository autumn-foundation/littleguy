//! The web tools against a local site and a local search service.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{ErrorKind, Tool, ToolEffect};
use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use serde_json::json;

use super::ctx;
use crate::{FetchTool, MAX_BODY_BYTES, MAX_REDIRECTS, SearchTool};

const LOCAL: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// Serves `router` on a free local port.
async fn serve(router: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await });
    addr
}

fn html(body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], body).into_response()
}

async fn site() -> SocketAddr {
    let router = Router::new()
        .route(
            "/page",
            get(|| async { html("<h1>Tea</h1><p>Green &amp; black.</p><script>x()</script>") }),
        )
        .route("/plain", get(|| async { "just text" }))
        .route(
            "/json",
            get(|| async { ([(header::CONTENT_TYPE, "application/json")], "{\"a\":1}") }),
        )
        .route(
            "/image",
            get(|| async { ([(header::CONTENT_TYPE, "image/png")], vec![0_u8; 8]) }),
        )
        .route(
            "/missing",
            get(|| async { (StatusCode::NOT_FOUND, html("<p>gone</p>")) }),
        )
        .route("/hop", get(|| async { Redirect::temporary("/page") }))
        .route("/loop", get(|| async { Redirect::temporary("/loop") }))
        .route(
            "/away",
            get(|| async { Redirect::temporary("http://127.0.0.2:9/secret") }),
        )
        .route(
            "/big",
            get(|| async {
                let body = "a".repeat(MAX_BODY_BYTES + 10);
                ([(header::CONTENT_TYPE, "text/plain")], body)
            }),
        );
    serve(router).await
}

fn local_fetch(max_chars: usize) -> FetchTool {
    FetchTool::new(max_chars).allow(LOCAL)
}

#[tokio::test]
async fn a_page_comes_back_as_text() {
    let addr = site().await;
    let page = local_fetch(1000)
        .execute(json!({"url": format!("http://{addr}/page")}), &ctx(None))
        .await
        .unwrap();
    assert_eq!(page["text"], "Tea\nGreen & black.");
    assert_eq!(page["status"], 200);
    assert_eq!(page["truncated"], false);
    assert!(
        page["content_type"]
            .as_str()
            .unwrap()
            .starts_with("text/html")
    );
}

#[tokio::test]
async fn text_and_json_pass_and_a_binary_body_is_refused() {
    let addr = site().await;
    let tool = local_fetch(1000);
    let plain = tool.fetch(&format!("http://{addr}/plain")).await.unwrap();
    assert_eq!(plain["text"], "just text");
    let data = tool.fetch(&format!("http://{addr}/json")).await.unwrap();
    assert_eq!(data["text"], "{\"a\":1}");
    let image = tool
        .fetch(&format!("http://{addr}/image"))
        .await
        .unwrap_err();
    assert_eq!(image.kind(), ErrorKind::Tool);
    assert!(image.to_string().contains("not text"));
}

#[tokio::test]
async fn an_error_page_is_a_result_with_its_status() {
    let addr = site().await;
    let page = local_fetch(1000)
        .fetch(&format!("http://{addr}/missing"))
        .await
        .unwrap();
    assert_eq!(page["status"], 404);
    assert_eq!(page["text"], "gone");
}

#[tokio::test]
async fn redirects_are_followed_and_bounded() {
    let addr = site().await;
    let tool = local_fetch(1000);
    let page = tool.fetch(&format!("http://{addr}/hop")).await.unwrap();
    assert_eq!(page["url"], format!("http://{addr}/page"));
    assert_eq!(page["text"], "Tea\nGreen & black.");
    let error = tool
        .fetch(&format!("http://{addr}/loop"))
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains(&format!("more than {MAX_REDIRECTS}"))
    );
}

#[tokio::test]
async fn a_redirect_to_a_private_address_is_refused() {
    let addr = site().await;
    let error = local_fetch(1000)
        .fetch(&format!("http://{addr}/away"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Tool);
    assert!(error.to_string().contains("127.0.0.2"));
    assert!(error.to_string().contains("not a public address"));
}

#[tokio::test]
async fn long_pages_are_cut() {
    let addr = site().await;
    let tool = local_fetch(10);
    let page = tool.fetch(&format!("http://{addr}/page")).await.unwrap();
    assert_eq!(page["text"], "Tea\nGreen ");
    assert_eq!(page["truncated"], true);
    let big = local_fetch(usize::MAX)
        .fetch(&format!("http://{addr}/big"))
        .await
        .unwrap();
    assert_eq!(big["text"].as_str().unwrap().len(), MAX_BODY_BYTES);
    assert_eq!(big["truncated"], true);
}

#[tokio::test]
async fn private_and_odd_urls_are_refused_before_any_request() {
    let tool = FetchTool::new(1000);
    for (url, words) in [
        ("http://127.0.0.1:9/", "not a public address"),
        ("http://localhost:9/", "not a public address"),
        ("http://[::1]:9/", "not a public address"),
        (
            "http://169.254.169.254/latest/meta-data",
            "not a public address",
        ),
        ("http://10.0.0.1/", "not a public address"),
        ("http://192.168.1.1/admin", "not a public address"),
        ("http://[::ffff:127.0.0.1]/", "not a public address"),
        ("file:///etc/passwd", "only http and https"),
        ("ftp://example.com/", "only http and https"),
        ("http://user:pw@example.com/", "user name or password"),
        ("not a url", "is not a URL"),
    ] {
        let error = tool.fetch(url).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
        assert!(error.to_string().contains(words), "{url}: {error}");
    }
}

#[tokio::test]
async fn the_fetch_tool_reads_only_and_marks_its_result_untrusted() {
    let tool = FetchTool::new(10);
    assert_eq!(tool.name(), "web_fetch");
    assert_eq!(tool.effect(), ToolEffect::ReadOnly);
    assert_eq!(tool.labels(), [String::from(labels::UNTRUSTED_INPUT)]);
    assert!(tool.interruptible());
    assert_eq!(tool.input_schema()["required"], json!(["url"]));
    assert!(tool.description().contains("untrusted"));
    let error = tool.execute(json!({}), &ctx(None)).await.unwrap_err();
    assert!(error.to_string().contains("`url`"));
}

/// What the fake search service saw.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<(String, String, String)>>>);

async fn search_service(status: StatusCode) -> (SocketAddr, Seen) {
    let seen = Seen::default();
    let router = Router::new()
        .route(
            "/search",
            get(
                move |State(seen): State<Seen>,
                      Query(query): Query<std::collections::HashMap<String, String>>,
                      headers: HeaderMap| async move {
                    let key = headers
                        .get("X-Subscription-Token")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    seen.0.lock().unwrap().push((
                        query.get("q").cloned().unwrap_or_default(),
                        query.get("count").cloned().unwrap_or_default(),
                        key,
                    ));
                    let body = json!({"web": {"results": [
                        {"title": "<strong>Tea</strong> guide", "url": "https://tea.example/",
                         "description": "All about <strong>tea</strong> &amp; more."},
                        {"title": "Second", "url": "https://two.example/"}
                    ]}});
                    (status, axum::Json(body))
                },
            ),
        )
        .with_state(seen.clone());
    (serve(router).await, seen)
}

#[tokio::test]
async fn a_search_returns_clean_results() {
    let (addr, seen) = search_service(StatusCode::OK).await;
    let tool = SearchTool::with_endpoint("secret-key", format!("http://{addr}/search"));
    let found = tool
        .execute(json!({"query": "green tea", "count": 50}), &ctx(None))
        .await
        .unwrap();
    assert_eq!(found["query"], "green tea");
    let results = found["results"].as_array().unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0]["title"], "Tea guide");
    assert_eq!(results[0]["description"], "All about tea & more.");
    assert_eq!(results[1]["description"], "");
    let seen = seen.0.lock().unwrap().clone();
    let sent_key = seen.first().is_some_and(|(_, _, key)| key == "secret-key");
    assert!(sent_key);
    assert_eq!(seen[0].0, "green tea");
    // The count is capped at ten.
    assert_eq!(seen[0].1, "10");
}

#[tokio::test]
async fn search_failures_map_to_their_kinds() {
    for (status, kind) in [
        (StatusCode::TOO_MANY_REQUESTS, ErrorKind::RateLimited),
        (StatusCode::UNAUTHORIZED, ErrorKind::Authentication),
        (StatusCode::BAD_GATEWAY, ErrorKind::Unavailable),
        (StatusCode::BAD_REQUEST, ErrorKind::Tool),
    ] {
        let (addr, _) = search_service(status).await;
        let tool = SearchTool::with_endpoint("k", format!("http://{addr}/search"));
        let error = tool.search("tea", 3).await.unwrap_err();
        assert_eq!(error.kind(), kind, "{status}");
    }
    let tool = SearchTool::with_endpoint("k", "http://127.0.0.1:9/search");
    assert_eq!(
        tool.search("tea", 3).await.unwrap_err().kind(),
        ErrorKind::Transport
    );
    let empty = tool
        .execute(json!({"query": "  "}), &ctx(None))
        .await
        .unwrap_err();
    assert!(empty.to_string().contains("empty"));
}

#[test]
fn the_search_tool_hides_its_key() {
    let tool = SearchTool::new("secret-key");
    let shown = format!("{tool:?}");
    let hidden = !shown.contains("secret-key");
    assert!(hidden);
    assert!(shown.contains(crate::BRAVE_ENDPOINT));
    assert_eq!(tool.name(), "web_search");
    assert_eq!(tool.effect(), ToolEffect::ReadOnly);
    assert_eq!(tool.labels(), [String::from(labels::UNTRUSTED_INPUT)]);
    assert!(tool.interruptible());
    assert!(tool.description().contains("untrusted"));
    assert_eq!(tool.input_schema()["required"], json!(["query"]));
}
