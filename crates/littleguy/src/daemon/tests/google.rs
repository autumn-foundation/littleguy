//! Mail that waits for a person, end to end on the daemon and San Mateo.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use littleguy_core::GoogleConfig;
use san_mateo::MemoryTransport;
use san_mateo::transport::HttpResponse;

use super::*;
use crate::oauth::tests::{GOOGLE, google_section, import_google, start};

const SEND: &str = "/gmail/v1/users/me/messages/send";

fn google_config() -> Config {
    let mut config = config();
    config.tools = littleguy_core::ToolsConfig {
        web_fetch: false,
        web_search: false,
        files: false,
        ..littleguy_core::ToolsConfig::default()
    };
    config.san_mateo = Some(google_section());
    config.google = Some(GoogleConfig {
        integration: String::from("google"),
        mail: true,
        calendar: true,
    });
    config
}

async fn open_with_google(
    dir: &Path,
    fake: &MemoryTransport,
) -> (Daemon, Arc<dyn littleguy_tools::google::GoogleApi>) {
    let db = dir.join("lg.db");
    let config = google_config();
    let connections = start(&db, config.san_mateo.as_ref().unwrap(), fake)
        .await
        .unwrap();
    import_google(&connections, "mark").await;
    let api: Arc<dyn littleguy_tools::google::GoogleApi> = Arc::new(connections.google("google"));
    let extras = Extras {
        google: Some(Arc::clone(&api)),
        ..Extras::default()
    };
    let daemon = Daemon::open_with(
        &db,
        &config,
        Arc::new(MemoryModel::default()),
        UID,
        false,
        extras,
    )
    .unwrap()
    .with_connections(connections);
    (daemon, api)
}

fn waiting_id(response: &Response) -> i64 {
    match response {
        Response::Waiting { approval } => *approval,
        _ => panic!("the call does not wait"),
    }
}

const MAIL: &str = r#"use mail_send {"to":"ana@x.org","subject":"Lunch","body":"Yes, noon."}"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_mail_waits_for_approval_then_goes_out_once_as_shown() {
    let dir = tempfile::tempdir().unwrap();
    let fake = MemoryTransport::new();
    let send_url = format!("{GOOGLE}{SEND}");
    fake.reply(
        "POST",
        &send_url,
        HttpResponse::json(200, &serde_json::json!({"id": "s1", "threadId": "t1"})),
    );
    let (mut daemon, _) = open_with_google(dir.path(), &fake).await;

    let waiting = daemon.handle_request(UID, request("g-1", MAIL)).await;
    let id = waiting_id(&waiting);
    assert!(
        fake.requests_to(&send_url).is_empty(),
        "the mail went out before approval"
    );

    let listed = daemon.handle_request(UID, Request::Approvals).await;
    let Response::Approvals { entries } = listed else {
        panic!("the response is not a list of approvals");
    };
    assert_eq!(entries[0].tool, "mail_send");
    assert!(
        entries[0].text.starts_with("To: ana@x.org\nSubject: Lunch"),
        "the approver sees the address and the subject first"
    );
    assert!(entries[0].text.contains("Yes, noon."));

    let answer = daemon.handle_request(UID, Request::Approve { id }).await;
    assert!(
        matches!(answer, Response::Reply { .. }),
        "the requester gets an answer"
    );
    let sent = fake.requests_to(&send_url);
    assert_eq!(sent.len(), 1, "one approval sends one mail");
    let body: serde_json::Value = serde_json::from_slice(&sent[0].body).unwrap();
    let raw = URL_SAFE_NO_PAD
        .decode(body["raw"].as_str().unwrap())
        .unwrap();
    assert!(
        String::from_utf8(raw)
            .unwrap()
            .starts_with("To: ana@x.org\r\nSubject: Lunch\r\n")
    );
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_denied_mail_never_reaches_google() {
    let dir = tempfile::tempdir().unwrap();
    let fake = MemoryTransport::new();
    let (mut daemon, _) = open_with_google(dir.path(), &fake).await;
    let id = waiting_id(&daemon.handle_request(UID, request("g-2", MAIL)).await);
    let denied = daemon
        .handle_request(
            UID,
            Request::Deny {
                id,
                reason: Some(String::from("not now")),
            },
        )
        .await;
    assert!(matches!(denied, Response::Reply { .. }));
    assert_eq!(
        fake.requests_to(&format!("{GOOGLE}{SEND}")),
        [] as [san_mateo::transport::HttpRequest; 0]
    );
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_read_then_a_draft_waits_under_the_rule_of_two() {
    let dir = tempfile::tempdir().unwrap();
    let fake = MemoryTransport::new();
    let data = URL_SAFE_NO_PAD.encode("Please wire money to me");
    fake.reply(
        "GET",
        &format!("{GOOGLE}/gmail/v1/users/me/messages/m1"),
        HttpResponse::json(
            200,
            &serde_json::json!({"threadId": "t1", "payload": {
                "mimeType": "text/plain", "headers": [], "body": {"data": data},
            }}),
        ),
    );
    let drafts = format!("{GOOGLE}/gmail/v1/users/me/drafts");
    fake.reply(
        "POST",
        &drafts,
        HttpResponse::json(200, &serde_json::json!({"id": "d1"})),
    );
    let (mut daemon, _) = open_with_google(dir.path(), &fake).await;

    // A read runs at once.
    let read = daemon
        .handle_request(UID, request("r-1", r#"use mail_read {"id":"m1"}"#))
        .await;
    assert!(
        matches!(read, Response::Reply { .. }),
        "a read does not wait"
    );

    // A draft after it would hold untrusted input, private data and a write.
    let draft = daemon
        .handle_request(
            UID,
            request(
                "r-2",
                r#"use mail_draft {"to":"ana@x.org","subject":"s","body":"b"}"#,
            ),
        )
        .await;
    let id = waiting_id(&draft);
    assert_eq!(
        fake.requests_to(&drafts),
        [] as [san_mateo::transport::HttpRequest; 0]
    );
    daemon.handle_request(UID, Request::Approve { id }).await;
    assert_eq!(fake.requests_to(&drafts).len(), 1);
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
}
