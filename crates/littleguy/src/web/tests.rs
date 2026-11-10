//! Tests of the pages, against a real daemon on the offline model.

use std::sync::Arc;

use autumn_web::test::{TestApp, TestClient};
use littleguy_core::{Config, OwnerName, UiConfig};
use littleguy_models::OfflineModel;
use tokio::sync::Notify;

use super::*;
use crate::daemon::{self, Daemon, Links};
use crate::protocol::{Request, Response};

const UID: u32 = 1000;

/// A daemon that runs its job loop until the test ends, and the pages over
/// it.
struct Rig {
    client: TestClient,
    handle: Handle,
    logins: Logins,
    stop: Arc<Notify>,
    _dir: tempfile::TempDir,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.stop.notify_one();
    }
}

fn config(refine: bool) -> Config {
    let mut config = Config::for_owner(OwnerName::new("mark").unwrap());
    if refine {
        config.refine = Some(littleguy_core::RefineConfig::default());
    }
    config
}

async fn rig(config: &Config) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lg.db");
    let ui = UiConfig::default();
    let logins = Logins::new(base_url(&ui));
    let daemon = Daemon::open(&db, config, Arc::new(OfflineModel::new()), UID, false)
        .unwrap()
        .with_logins(logins.clone());
    let listener = daemon::bind(&dir.path().join("lg.sock")).await.unwrap();
    let (handle, jobs) = daemon::jobs(UID);
    let stop = Arc::new(Notify::new());
    let stopped = Arc::clone(&stop);
    tokio::spawn(async move {
        let until = async move { stopped.notified().await };
        daemon::run(daemon, listener, Links::default(), None, jobs, until).await
    });
    let web = Web {
        handle: handle.clone(),
        logins: logins.clone(),
    };
    let client = TestApp::new()
        .config(settings(&ui))
        .routes(pages::routes())
        .state_initializer(move |state| state.insert_extension(web))
        .build();
    Rig {
        client,
        handle,
        logins,
        stop,
        _dir: dir,
    }
}

/// Signs the client in with a fresh link, and returns the CSRF token of
/// the home page.
async fn sign_in(rig: &Rig) -> String {
    let link = rig.logins.link();
    let path = link.trim_start_matches(&base_url(&UiConfig::default()));
    rig.client.get(path).send().await.assert_status(303);
    let home = rig.client.get("/").send().await;
    home.assert_status(200);
    let tokens = home.selector_attr("input[name=_csrf]", "value");
    tokens.into_iter().flatten().next().unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_browser_without_a_session_sees_how_to_sign_in() {
    let rig = rig(&config(false)).await;
    rig.client.get("/llms.txt").send().await.assert_status(404);
    let home = rig.client.get("/").send().await;
    home.assert_status(401);
    home.assert_body_contains("lg ui");
    rig.client.get("/approvals").send().await.assert_status(401);
    rig.client.get("/refine/1").send().await.assert_status(401);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sign_in_link_works_once() {
    let rig = rig(&config(false)).await;
    rig.client
        .get("/login?token=nothing")
        .send()
        .await
        .assert_status(403);
    let link = rig.logins.link();
    let path = link.trim_start_matches(&base_url(&UiConfig::default()));
    let first = rig.client.get(path).send().await;
    first.assert_status(303);
    first.assert_header("location", "/");
    rig.client.get(path).send().await.assert_status(403);
    let home = rig.client.get("/").send().await;
    home.assert_status(200);
    for section in [
        "Chat",
        "Approvals",
        "Who can talk to it",
        "Refine",
        "Connections",
    ] {
        home.assert_body_contains(section);
    }
    assert_eq!(
        home.selector_count("script[src='/static/js/htmx.min.js']"),
        1
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn an_action_needs_the_csrf_token() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    rig.client
        .post("/chat")
        .form("text=hello")
        .send()
        .await
        .assert_status(403);
    let answer = rig
        .client
        .post("/chat")
        .header("X-CSRF-Token", &token)
        .form("text=hello")
        .send()
        .await;
    answer.assert_status(200);
    answer.assert_body_contains("you said 'hello'");
}

#[tokio::test(flavor = "multi_thread")]
async fn sign_out_ends_the_session() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    rig.client
        .post("/logout")
        .form(&format!("_csrf={token}"))
        .send()
        .await
        .assert_status(303);
    rig.client.get("/").send().await.assert_status(401);
}

#[cfg(feature = "refine")]
#[tokio::test(flavor = "multi_thread")]
async fn the_refine_section_activates_a_version() {
    let rig = rig(&config(true)).await;
    let token = sign_in(&rig).await;
    let stored = rig
        .handle
        .ask(Request::OverlayPut {
            overlay: String::from("{\"guardrails\": \"Be brief.\"}"),
        })
        .await
        .unwrap();
    assert!(matches!(stored, Response::OverlayStored { version: 1 }));
    let section = rig.client.get("/refine").send().await;
    section.assert_status(200);
    section.assert_body_contains("v1");
    assert_eq!(
        section.selector_count("button[hx-post='/refine/1/activate']"),
        1
    );
    let active = rig
        .client
        .post("/refine/1/activate")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    active.assert_status(200);
    active.assert_body_contains("runs on overlay version 1");
    assert_eq!(
        active.selector_count("button[hx-post='/refine/rollback']"),
        1
    );
    let shown = rig.client.get("/refine/1").send().await;
    shown.assert_status(200);
    shown.assert_body_contains("Be brief.");
    let proposed = rig
        .client
        .post("/refine/propose")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    proposed.assert_body_contains("no transcript");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_refine_section_says_when_refine_is_off() {
    // Without the feature, the daemon names the missing feature instead.
    let off = if cfg!(feature = "refine") {
        "add a [refine] section"
    } else {
        "built without it"
    };
    let rig = rig(&config(false)).await;
    sign_in(&rig).await;
    rig.client
        .get("/refine")
        .send()
        .await
        .assert_body_contains(off);
    rig.client
        .get("/refine/1")
        .send()
        .await
        .assert_body_contains(off);
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_and_approvals_answer_in_their_sections() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    let paired = rig
        .client
        .post("/pairs/pair")
        .header("X-CSRF-Token", &token)
        .form("code=ABCD-1234")
        .send()
        .await;
    paired.assert_status(200);
    assert_eq!(paired.selector_count("section#pairs p.error"), 1);
    let unpaired = rig
        .client
        .post("/pairs/unpair")
        .header("X-CSRF-Token", &token)
        .form("sender=discord%3A123")
        .send()
        .await;
    unpaired.assert_body_contains("no paired sender has that id");
    let approvals = rig.client.get("/approvals").send().await;
    approvals.assert_body_contains("no call waits for approval in this daemon");
    let approved = rig
        .client
        .post("/approvals/7/approve")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    // The notice and the list both say that this daemon has no approvals.
    assert_eq!(approved.selector_count("section#approvals p.error"), 2);
    let denied = rig
        .client
        .post("/approvals/7/deny")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    assert_eq!(denied.selector_count("section#approvals p.error"), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_gets_a_link_from_the_daemon() {
    let rig = rig(&config(false)).await;
    let Response::ConnectLink { link } = rig.handle.ask(Request::UiLink).await.unwrap() else {
        panic!("expected a link");
    };
    assert!(link.starts_with("http://127.0.0.1:3004/login?token="));
    let path = link.trim_start_matches("http://127.0.0.1:3004");
    rig.client.get(path).send().await.assert_status(303);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_without_pages_refuses_a_link() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config(false),
        Arc::new(OfflineModel::new()),
        UID,
        false,
    )
    .unwrap();
    let answer = daemon.handle_request(UID, Request::UiLink).await;
    assert!(matches!(answer, Response::Error { .. }));
    let other = daemon.handle_request(UID + 1, Request::UiLink).await;
    assert!(matches!(other, Response::Error { .. }));
}

#[test]
fn the_settings_keep_the_pages_local_and_guarded() {
    let ui = UiConfig {
        bind: "[::1]:4000".parse().unwrap(),
    };
    let settings = settings(&ui);
    assert_eq!(settings.server.host, "::1");
    assert_eq!(settings.server.port, 4000);
    assert!(settings.security.csrf.enabled);
    assert_eq!(settings.server.prestop_grace_secs, 0);
    assert!(!settings.aeo.enabled);
    assert!(settings.session.http_only);
    assert_eq!(settings.session.same_site, "Strict");
    assert_eq!(settings.session.cookie_name, COOKIE);
    assert_eq!(base_url(&ui), "http://[::1]:4000");
}

#[tokio::test]
async fn a_stopped_daemon_answers_with_an_error() {
    let (handle, jobs) = daemon::jobs(UID);
    drop(jobs);
    assert!(handle.ask(Request::Pairs).await.is_err());
}
