//! Calls to a provider API that wait for a person, end to end on the
//! daemon and San Mateo (ADR 0031).

use littleguy_core::ApiConfig;
use san_mateo::MemoryTransport;
use san_mateo::transport::HttpResponse;

use super::*;
use crate::oauth::tests::{import, section, start};

/// The base URL of the `acme` provider of the test catalog.
const ACME: &str = "https://api.acme.example.com";

fn api_config() -> Config {
    let mut config = config();
    config.tools = littleguy_core::ToolsConfig {
        web_fetch: false,
        web_search: false,
        files: false,
        runs: false,
        plans: false,
        ..littleguy_core::ToolsConfig::default()
    };
    config.san_mateo = Some(section("acme", None));
    config.api = Some(ApiConfig {
        integrations: vec![String::from("acme")],
        max_chars: 1_000,
    });
    config
}

async fn open_with_api(dir: &Path, fake: &MemoryTransport) -> Daemon {
    let db = dir.join("lg.db");
    let config = api_config();
    let connections = start(&db, config.san_mateo.as_ref().unwrap(), fake)
        .await
        .unwrap();
    import(&connections, "acme", "at-acme", "mark").await;
    let (proxy, integrations) = connections.api(&[String::from("acme")]);
    let extras = Extras {
        api: Some((
            Arc::new(proxy) as Arc<dyn littleguy_tools::api::ProviderApi>,
            integrations,
        )),
        ..Extras::default()
    };
    Daemon::open_with(
        &db,
        &config,
        Arc::new(MemoryModel::default()),
        UID,
        false,
        extras,
    )
    .unwrap()
    .with_connections(connections)
}

const SEND: &str = r#"use api_send {"integration":"acme","method":"POST","path":"/v1/items","body":{"name":"tea"}}"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_get_runs_at_once_and_a_send_waits_then_goes_out_once() {
    let dir = tempfile::tempdir().unwrap();
    let fake = MemoryTransport::new();
    let items = format!("{ACME}/v1/items");
    fake.reply(
        "GET",
        &items,
        HttpResponse::json(200, &serde_json::json!({"items": []})),
    );
    fake.reply(
        "POST",
        &items,
        HttpResponse::json(201, &serde_json::json!({"id": "i1"})),
    );
    let mut daemon = open_with_api(dir.path(), &fake).await;

    let read = daemon
        .handle_request(
            UID,
            request(
                "a-1",
                r#"use api_get {"integration":"acme","path":"/v1/items"}"#,
            ),
        )
        .await;
    assert!(
        matches!(read, Response::Reply { .. }),
        "a read does not wait"
    );
    assert_eq!(fake.requests_to(&items).len(), 1);

    let waiting = daemon.handle_request(UID, request("a-2", SEND)).await;
    let Response::Waiting { approval: id } = waiting else {
        panic!("the send does not wait");
    };
    let posts = || {
        fake.requests_to(&items)
            .into_iter()
            .filter(|r| r.method == "POST")
            .count()
    };
    assert_eq!(posts(), 0, "the call went out before approval");
    let listed = daemon.handle_request(UID, Request::Approvals).await;
    let Response::Approvals { entries } = listed else {
        panic!("the response is not a list of approvals");
    };
    assert_eq!(entries[0].tool, "api_send");
    assert!(
        entries[0]
            .text
            .starts_with("Call: POST /v1/items\nIntegration: acme"),
        "the approver sees the call first"
    );
    let answer = daemon.handle_request(UID, Request::Approve { id }).await;
    assert!(matches!(answer, Response::Reply { .. }));
    assert_eq!(posts(), 1, "one approval sends one call");
    let sent = fake
        .requests_to(&items)
        .into_iter()
        .find(|r| r.method == "POST")
        .unwrap();
    assert_eq!(sent.header_value("authorization"), Some("Bearer at-acme"));
    assert_eq!(sent.body, br#"{"name":"tea"}"#);
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
}
