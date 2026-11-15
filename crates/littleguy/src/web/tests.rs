//! Tests of the pages, against a real daemon on the offline model.

use std::sync::Arc;

use autumn_web::test::{TestApp, TestClient};
use littleguy_core::{Config, HooksConfig, OwnerName, UiConfig};
use littleguy_models::OfflineModel;
use tokio::sync::Notify;

use super::*;
use crate::daemon::{self, Daemon, Links};
use crate::protocol::{Request, Response};

const UID: u32 = 1000;

/// A hook token for the tests.
const TOKEN: &str = "0123456789abcdef0123456789abcdef";

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
    rig_with(config, None).await
}

/// A rig whose Slack runs over the Events API against the fake Slack at
/// `slack`, when there is one.
async fn rig_with(config: &Config, slack: Option<&str>) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lg.db");
    let plan = Plan::of(config);
    let logins = Logins::new(base_url(plan.bind));
    let extras = daemon::Extras {
        slack: slack.is_some(),
        ..daemon::Extras::default()
    };
    let model = Arc::new(OfflineModel::new());
    let daemon = Daemon::open_with(&db, config, model, UID, false, extras)
        .unwrap()
        .with_logins(logins.clone());
    let listener = daemon::bind(&dir.path().join("lg.sock")).await.unwrap();
    let (handle, jobs) = daemon::jobs(UID);
    // As `lg serve` does, so a chat streams.
    let handle = handle.with_live(daemon.live());
    #[cfg(feature = "slack")]
    let (links, slack) = slack_links(&daemon, &handle, slack);
    #[cfg(not(feature = "slack"))]
    let links = {
        assert!(slack.is_none(), "this build has no Slack");
        Links::default()
    };
    let stop = Arc::new(Notify::new());
    let stopped = Arc::clone(&stop);
    tokio::spawn(async move {
        let until = async move { stopped.notified().await };
        daemon::run(daemon, listener, links, None, jobs, until).await
    });
    let hooks = plan
        .hooks
        .then(|| HookToken::new("T", Some(TOKEN.to_owned())).unwrap());
    let web = Web {
        handle: handle.clone(),
        logins: logins.clone(),
        streams: crate::web::Streams::default(),
        hooks,
        #[cfg(feature = "slack")]
        slack,
    };
    let mut app = TestApp::new().config(settings(&plan)).routes(routes(&plan));
    if plan.pages {
        app = app.plugin(pages::Assets);
    }
    let client = app
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

/// The links and the Slack hooks of a rig with the fake Slack at `base`.
#[cfg(feature = "slack")]
fn slack_links(daemon: &Daemon, handle: &Handle, base: Option<&str>) -> (Links, Option<SlackHook>) {
    let Some(base) = base else {
        return (Links::default(), None);
    };
    let api = littleguy_channels::SlackApi::web_only(base, slack_tests::BOT_TOKEN);
    let secret = littleguy_channels::SigningSecret::new(slack_tests::SECRET).unwrap();
    let hook = SlackHook::new(&secret, daemon.slack_inbox(api.clone(), handle));
    let links = Links {
        #[cfg(feature = "discord")]
        discord: None,
        slack: Some(daemon::SlackLink::Events { api, secret }),
    };
    (links, Some(hook))
}

/// Signs the client in with a fresh link, and returns the CSRF token of
/// the home page.
async fn sign_in(rig: &Rig) -> String {
    let link = rig.logins.link();
    let path = &link[link.find("/login").unwrap()..];
    rig.client.get(path).send().await.assert_status(303);
    let home = rig.client.get("/").send().await;
    home.assert_status(200);
    let tokens = home.selector_attr("input[name=_csrf]", "value");
    tokens.into_iter().flatten().next().unwrap()
}

/// The events of a markstream stream body, as `(name, data)`.
fn events(body: &str) -> Vec<(String, String)> {
    body.split("\n\n")
        .filter_map(|block| {
            let mut name = None;
            let mut data = None;
            for line in block.lines() {
                if let Some(value) = line.strip_prefix("event: ") {
                    name = Some(value.to_owned());
                } else if let Some(value) = line.strip_prefix("data: ") {
                    data = Some(value.to_owned());
                }
            }
            Some((name?, data?))
        })
        .collect()
}

/// The stream URL in the reply of a chat.
fn stream_src(reply: &autumn_web::test::TestResponse) -> String {
    let sources = reply.selector_attr("[data-markstream-src]", "data-markstream-src");
    sources.into_iter().flatten().next().unwrap()
}

/// Sends `text` as the owner, and reads its answer stream to the end.
async fn chat(rig: &Rig, token: &str, text: &str) -> Vec<(String, String)> {
    let reply = rig
        .client
        .post("/chat")
        .header("X-CSRF-Token", token)
        .form(&format!("text={text}"))
        .send()
        .await;
    reply.assert_status(200);
    let stream = rig.client.get(&stream_src(&reply)).send().await;
    stream.assert_status(200);
    stream.assert_header_contains("content-type", "text/event-stream");
    events(&stream.text())
}

/// The text that the events leave in the answer, as markstream applies them.
fn shown(events: &[(String, String)]) -> String {
    let mut text = String::new();
    for (name, data) in events {
        match name.as_str() {
            "chunk" => text.push_str(&serde_json::from_str::<String>(data).unwrap()),
            "replace" => text = serde_json::from_str(data).unwrap(),
            _ => {}
        }
    }
    text
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_streams_as_the_model_writes_then_settles() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    let events = chat(&rig, &token, "hi").await;
    let names: Vec<&str> = events.iter().map(|(name, _)| name.as_str()).collect();
    let answer = "littleguy (offline): you said 'hi'";
    // A model call starts, its text comes, then the answer takes its place.
    assert_eq!(names.first(), Some(&"replace"));
    assert!(names.contains(&"chunk"), "{names:?}");
    assert_eq!(names[names.len() - 2..], ["replace", "done"]);
    let live: Vec<_> = events[..events.len() - 2].to_vec();
    assert_eq!(shown(&live), answer);
    assert_eq!(shown(&events), answer);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_answer_streams_once_and_only_to_the_owner() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    let reply = rig
        .client
        .post("/chat")
        .header("X-CSRF-Token", &token)
        .form("text=hi")
        .send()
        .await;
    let src = stream_src(&reply);
    assert_eq!(
        shown(&events(&rig.client.get(&src).send().await.text())),
        "littleguy (offline): you said 'hi'"
    );
    let again = rig.client.get(&src).send().await;
    again.assert_status(200);
    assert_eq!(
        events(&again.text()),
        [(String::from("error"), String::from("null"))]
    );
    rig.client
        .get("/chat/stream/unknown")
        .send()
        .await
        .assert_body_contains("event: error");
    rig.client.log_out();
    let stranger = rig.client.get("/").send().await;
    stranger.assert_status(401);
    rig.client.get(&src).send().await.assert_status(401);
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
    let path = link.trim_start_matches(&base_url(UiConfig::default().bind));
    let first = rig.client.get(path).send().await;
    first.assert_status(303);
    first.assert_header("location", "/");
    rig.client.get(path).send().await.assert_status(403);
    let home = rig.client.get("/").send().await;
    home.assert_status(200);
    home.assert_selector_count("form#composer", 1);
    let manage = rig.client.get("/manage").send().await;
    manage.assert_status(200);
    for section in ["Approvals", "Who can talk to it", "Refine", "Connections"] {
        manage.assert_body_contains(section);
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
    let stream = rig.client.get(&stream_src(&answer)).send().await;
    assert!(shown(&events(&stream.text())).contains("you said 'hello'"));
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
    let plan = Plan {
        bind: ui.bind,
        pages: true,
        hooks: false,
    };
    let settings = settings(&plan);
    assert_eq!(settings.server.host, "::1");
    assert_eq!(settings.server.port, 4000);
    assert!(settings.security.csrf.enabled);
    assert_eq!(settings.server.prestop_grace_secs, 0);
    assert!(!settings.aeo.enabled);
    assert!(settings.session.http_only);
    assert_eq!(settings.session.same_site, "Strict");
    assert_eq!(settings.session.cookie_name, COOKIE);
    assert_eq!(settings.security.csrf.exempt_paths, ["/hooks"]);
    assert_eq!(base_url(ui.bind), "http://[::1]:4000");
}

#[tokio::test]
async fn a_stopped_daemon_answers_with_an_error() {
    let (handle, jobs) = daemon::jobs(UID);
    drop(jobs);
    assert!(handle.ask(Request::Pairs).await.is_err());
}

/// A config with a `[hooks]` section at `bind`.
fn hooked(bind: &str) -> Config {
    let mut config = config(false);
    config.hooks = Some(HooksConfig {
        bind: bind.parse().unwrap(),
        ..HooksConfig::default()
    });
    config
}

#[test]
fn the_plan_serves_pages_only_on_loopback() {
    let plain = Plan::of(&config(false));
    assert_eq!(plain.bind, UiConfig::default().bind);
    assert!(plain.pages && !plain.hooks);
    let public = Plan::of(&hooked("0.0.0.0:8080"));
    assert_eq!(public.bind.to_string(), "0.0.0.0:8080");
    assert!(!public.pages && public.hooks);
    let local = Plan::of(&hooked("127.0.0.1:8080"));
    assert!(local.pages && local.hooks);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_public_hook_ticks_with_its_token_and_serves_no_pages() {
    let rig = rig(&hooked("0.0.0.0:8080")).await;
    rig.client.get("/").send().await.assert_status(404);
    rig.client
        .get("/login?token=x")
        .send()
        .await
        .assert_status(404);
    rig.client
        .post("/hooks/tick")
        .send()
        .await
        .assert_status(401);
    rig.client
        .post("/hooks/tick")
        .header("Authorization", "Bearer wrong")
        .send()
        .await
        .assert_status(401);
    let ticked = rig
        .client
        .post("/hooks/tick")
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send()
        .await;
    ticked.assert_status(200);
    ticked.assert_body_contains("ticked");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_loopback_hook_serves_the_pages_too() {
    let rig = rig(&hooked("127.0.0.1:0")).await;
    sign_in(&rig).await;
    rig.client
        .post("/hooks/tick")
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send()
        .await
        .assert_status(200);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_hooks_the_tick_route_does_not_exist() {
    let rig = rig(&config(false)).await;
    rig.client
        .post("/hooks/tick")
        .header("Authorization", &format!("Bearer {TOKEN}"))
        .send()
        .await
        .assert_status(404);
}

#[tokio::test]
async fn a_tick_of_a_stopped_daemon_is_an_error() {
    let (handle, jobs) = daemon::jobs(UID);
    drop(jobs);
    assert!(handle.tick().await.is_err());
}

#[cfg(feature = "slack")]
mod slack_tests;

#[tokio::test(flavor = "multi_thread")]
async fn the_home_page_shows_the_conversation() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    rig.client
        .get("/")
        .send()
        .await
        .assert_selector_count("#thread .msg", 0);
    let answer = rig
        .client
        .post("/chat")
        .header("X-CSRF-Token", &token)
        .form("text=**hello**")
        .send()
        .await;
    answer.assert_status(200);
    answer.assert_selector_count(".msg.lg [data-markstream][data-markstream-src]", 1);
    let stream = rig.client.get(&stream_src(&answer)).send().await;
    assert_eq!(
        shown(&events(&stream.text())),
        "littleguy (offline): you said '**hello**'"
    );

    let home = rig.client.get("/").send().await;
    home.assert_status(200);
    home.assert_selector_count("#thread .msg.you", 1);
    home.assert_selector_count("#thread .msg.lg [data-markstream]", 1);
    home.assert_text_contains("#thread .msg.you", "**hello**");
    home.assert_selector_count("form#composer[hx-target='#thread']", 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pages_load_their_script_and_style_from_the_binary() {
    let rig = rig(&config(false)).await;
    sign_in(&rig).await;
    let home = rig.client.get("/").send().await;
    for (selector, attr, kind) in [
        ("script[src*='chat']", "src", "javascript"),
        ("link[rel=stylesheet][href*='lg']", "href", "css"),
    ] {
        let url = home
            .selector_attr(selector, attr)
            .into_iter()
            .flatten()
            .next()
            .unwrap_or_else(|| panic!("no {selector}"));
        home.assert_selector_count(&format!("{selector}[integrity]"), 1);
        let file = rig.client.get(&url).send().await;
        file.assert_status(200);
        file.assert_header_contains("content-type", kind);
    }
    home.assert_selector_count("script[type=module][src*='markstream']", 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_manage_page_holds_the_sections() {
    let rig = rig(&config(false)).await;
    rig.client.get("/manage").send().await.assert_status(401);
    sign_in(&rig).await;
    let manage = rig.client.get("/manage").send().await;
    manage.assert_status(200);
    for id in ["approvals", "pairs", "refine", "connections"] {
        manage.assert_selector_count(&format!("section#{id}"), 1);
    }
    rig.client
        .get("/")
        .send()
        .await
        .assert_selector_count("section#approvals", 0);
}
