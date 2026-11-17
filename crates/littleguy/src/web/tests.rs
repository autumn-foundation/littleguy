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

/// The text of a reply, or nothing for any other response. A check prints
/// only this text, never the whole response: some responses carry a
/// secret, such as the token of a link.
fn reply_text(response: &Response) -> &str {
    match response {
        Response::Reply { text, .. } => text,
        _ => "",
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
    rig_on(config, slack, Arc::new(OfflineModel::new())).await
}

/// A rig whose assistant runs on `model`.
async fn rig_on(
    config: &Config,
    slack: Option<&str>,
    model: Arc<dyn autumn_harvest_agent::AgentModel>,
) -> Rig {
    let extras = daemon::Extras {
        slack: slack.is_some(),
        ..daemon::Extras::default()
    };
    rig_extras(config, slack, model, extras).await
}

/// A rig whose daemon opens with `extras`.
async fn rig_extras(
    config: &Config,
    slack: Option<&str>,
    model: Arc<dyn autumn_harvest_agent::AgentModel>,
    extras: daemon::Extras,
) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("lg.db");
    let plan = Plan::of(config);
    let logins = Logins::new(base_url(plan.bind));
    let daemon = Daemon::open_with(&db, config, model, UID, false, extras)
        .unwrap()
        .with_logins(logins.clone());
    #[cfg(feature = "skills")]
    let daemon = daemon.with_skills_dir(dir.path().join("skills"));
    let listener = daemon::bind(&dir.path().join("lg.sock")).await.unwrap();
    let (handle, jobs) = daemon::jobs(UID);
    // As `lg serve` does, so a chat streams.
    let handle = handle.with_live(daemon.live()).with_reads(daemon.reads());
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
        decisions: crate::web::Decisions::default(),
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
    for (button, done) in [("approve", "Approved"), ("deny", "Denied")] {
        let pressed = rig
            .client
            .post(&format!("/approvals/7/{button}"))
            .header("X-CSRF-Token", &token)
            .send()
            .await;
        // The press answers at once. The section asks for the result.
        pressed.assert_status(200);
        pressed.assert_body_contains(&format!("{done} request #7. It runs now"));
        pressed.assert_body_contains("hx-get=\"/approvals/7/result\"");
        let result = result_of(&rig, 7).await;
        // The failed decision is red. The list is off, so it is muted.
        assert_eq!(result.selector_count("section#approvals p.error"), 1);
        assert_eq!(result.selector_count("section#approvals p.off"), 1);
        let again = rig.client.get("/approvals/7/result").send().await;
        assert_eq!(
            again.selector_count("section#approvals p.error"),
            0,
            "a result is read once"
        );
    }
    rig.client.log_out();
    rig.client
        .get("/approvals/7/result")
        .send()
        .await
        .assert_status(401);
}

/// The result of the decision on call `id`, once its run answered.
async fn result_of(rig: &Rig, id: i64) -> autumn_web::test::TestResponse {
    for _ in 0..200 {
        let result = rig
            .client
            .get(&format!("/approvals/{id}/result"))
            .send()
            .await;
        if result.status == 200 {
            return result;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("the decision on #{id} never answered");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_fresh_daemon_shows_no_red_on_the_manage_page() {
    // No approvals, no refine section, and maybe no OAuth: each is off,
    // not broken.
    let rig = rig(&config(false)).await;
    sign_in(&rig).await;
    let manage = rig.client.get("/manage").send().await;
    manage.assert_status(200);
    manage.assert_selector_count("p.error", 0);
    manage.assert_selector_count("section#approvals p.off .tag", 1);
    manage.assert_selector_count("section#refine p.off", 1);
    manage.assert_selector_count("section#connections p.off", 1);
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

/// Saves a file for `save <path> <text>`, with the file tool, and calls any
/// tool for `use <tool> <json>`. Then it says `saved`. Any other message
/// gets `ok`.
#[derive(Debug)]
struct Saver;

impl autumn_harvest_agent::AgentModel for Saver {
    fn chat<'a>(
        &'a self,
        request: &'a autumn_harvest_agent::ChatRequest,
    ) -> autumn_harvest_agent::BoxFuture<
        'a,
        Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
    > {
        use autumn_harvest_agent::{ChatResponse, ContentPart, StopReason, TokenUsage};
        let last = request.messages.last().map(|message| &message.content);
        let answered = last
            .into_iter()
            .flatten()
            .any(|part| matches!(part, ContentPart::ToolResult { .. }));
        let text = last.into_iter().flatten().find_map(|part| match part {
            ContentPart::Text(text) => Some(text.clone()),
            _ => None,
        });
        let save = text
            .as_deref()
            .and_then(|text| text.strip_prefix("save "))
            .and_then(|rest| rest.split_once(' '))
            .map(|(path, text)| {
                (
                    "write_file",
                    serde_json::json!({"path": path, "text": text}),
                )
            });
        // `use <tool> <json>` calls any tool.
        let call = save.or_else(|| {
            let (tool, input) = text.as_deref()?.strip_prefix("use ")?.split_once(' ')?;
            Some((tool, serde_json::from_str(input).ok()?))
        });
        let (content, stop_reason) = match call {
            Some((name, arguments)) if !answered => (
                ContentPart::ToolCall {
                    id: String::from("call-1"),
                    name: name.to_owned(),
                    arguments,
                },
                StopReason::ToolUse,
            ),
            _ if answered => (
                ContentPart::Text(String::from("saved")),
                StopReason::EndTurn,
            ),
            _ => (ContentPart::Text(String::from("ok")), StopReason::EndTurn),
        };
        Box::pin(async move {
            Ok(ChatResponse {
                content: vec![content],
                stop_reason,
                usage: TokenUsage::new(1, 1),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_workspace_page_lists_the_files_and_opens_one() {
    let rig = rig_on(&config(false), None, Arc::new(Saver)).await;
    sign_in(&rig).await;
    let empty = rig.client.get("/workspace").send().await;
    empty.assert_status(200);
    empty.assert_body_contains("The workspace is empty");
    empty.assert_selector_count(
        "header.bar nav a[href='/workspace'][aria-current='page']",
        1,
    );

    let saved = rig
        .handle
        .ask(Request::Chat {
            message_id: String::from("w-1"),
            text: String::from("save notes/tea.md **green** tea"),
        })
        .await
        .unwrap();
    assert_eq!(reply_text(&saved), "saved");
    rig.handle
        .ask(Request::Chat {
            message_id: String::from("w-2"),
            text: String::from("save plan.txt a <b>plan</b>"),
        })
        .await
        .unwrap();

    let listed = rig.client.get("/workspace").send().await;
    listed.assert_selector_count(".files li", 2);
    listed.assert_selector_count(
        ".files a[href='/workspace?path=notes%2Ftea.md'][hx-select='#file']",
        1,
    );
    listed.assert_text_contains("#file", "Pick a file");

    // Markdown renders, and the link of the open file is marked.
    let markdown = rig
        .client
        .get("/workspace?path=notes%2Ftea.md")
        .send()
        .await;
    markdown.assert_status(200);
    markdown.assert_selector_count(".files a[aria-current='page']", 1);
    markdown.assert_selector_count("#file [data-markstream]", 1);
    markdown.assert_text_contains("#file", "**green** tea");

    // Other text shows as it is, and HTML in it stays text.
    let text = rig.client.get("/workspace?path=plan.txt").send().await;
    text.assert_selector_count("#file pre", 1);
    text.assert_selector_count("#file b", 0);
    text.assert_text_contains("#file pre", "a <b>plan</b>");

    let missing = rig.client.get("/workspace?path=gone.md").send().await;
    missing.assert_text_contains("#file", "the workspace has no file gone.md");

    rig.client.log_out();
    rig.client.get("/workspace").send().await.assert_status(401);
}

#[test]
fn markdown_is_known_by_its_extension_in_any_case() {
    for path in ["a.md", "notes/B.MD", "c.Markdown"] {
        assert!(pages::is_markdown(path), "{path}");
    }
    for path in ["a.txt", "md", "notes.md/plan"] {
        assert!(!pages::is_markdown(path), "{path}");
    }
}

#[cfg(not(feature = "plans"))]
#[tokio::test(flavor = "multi_thread")]
async fn the_goals_tab_says_the_build_has_no_plans() {
    let rig = rig_on(&config(false), None, Arc::new(Saver)).await;
    sign_in(&rig).await;
    for tab in ["goals", "ideas"] {
        rig.client
            .get(&format!("/workspace?tab={tab}"))
            .send()
            .await
            .assert_body_contains("`plans` feature");
    }
}

#[cfg(feature = "notes")]
#[tokio::test(flavor = "multi_thread")]
async fn the_owner_reads_the_notes_of_littleguy_on_the_notes_tab() {
    let rig = rig_on(&config(false), None, Arc::new(Saver)).await;
    sign_in(&rig).await;
    rig.client
        .get("/workspace?tab=notes")
        .send()
        .await
        .assert_text_contains("#plan-view", "No notes yet");
    let answer = rig
        .handle
        .ask(Request::Chat {
            message_id: String::from("n-1"),
            text: String::from(
                r##"use write_note {"name": "ui-punchlist", "text": "# Punch list\n\n- Tabs <b>count</b>."}"##,
            ),
        })
        .await
        .unwrap();
    assert_eq!(reply_text(&answer), "saved");
    let page = rig.client.get("/workspace?tab=notes").send().await;
    page.assert_text_contains("#tabs a[aria-current='page']", "Notes");
    page.assert_text_contains("#plan-view .files .path", "ui-punchlist");
    // The note opens as a document, and HTML in it stays text.
    page.assert_text_contains("#file .doc h2", "ui-punchlist");
    page.assert_text_contains("#file .doc", "Tabs <b>count</b>.");
    page.assert_selector_count("#file b", 0);
}

#[cfg(feature = "plans")]
#[tokio::test(flavor = "multi_thread")]
async fn the_owner_accepts_a_proposed_goal_on_the_goals_tab() {
    let rig = rig_on(&config(false), None, Arc::new(Saver)).await;
    let token = sign_in(&rig).await;
    let files = rig.client.get("/workspace").send().await;
    files.assert_text_contains("#tabs a[aria-current='page']", "Files");
    files.assert_selector_count("#tabs .count", 0);
    rig.client
        .get("/workspace?tab=goals")
        .send()
        .await
        .assert_text_contains("#plan-view", "No goals yet");
    rig.client
        .get("/workspace?tab=ideas")
        .send()
        .await
        .assert_text_contains("#plan-view", "No ideas yet");

    for (id, text) in [
        (
            "p-1",
            r#"use add_idea {"title": "Docs links", "body": "They <b>rot</b>."}"#,
        ),
        (
            "p-2",
            r#"use propose_goal {"title": "Fix links", "why": "Dead ends.", "check_every_minutes": 60, "from_idea": 1}"#,
        ),
    ] {
        let answer = rig
            .handle
            .ask(Request::Chat {
                message_id: id.to_owned(),
                text: text.to_owned(),
            })
            .await
            .unwrap();
        assert_eq!(reply_text(&answer), "saved");
    }

    // Every tab shows that a goal waits.
    let files = rig.client.get("/workspace").send().await;
    files.assert_text_contains("#tabs .count", "1 waiting");

    // The goals tab opens the first goal as a document, with the buttons.
    let goals = rig.client.get("/workspace?tab=goals").send().await;
    goals.assert_text_contains("#tabs a[aria-current='page']", "Goals");
    goals.assert_selector_count(
        "#plan-view .files a[href='/workspace?tab=goals&id=1'] .dot",
        1,
    );
    goals.assert_text_contains("#file .doc .tag", "waiting for you");
    goals.assert_selector_count("#file button[hx-post='/plans/accept/1']", 1);
    goals.assert_selector_count("#file button[hx-post='/plans/decline/1']", 1);
    goals.assert_selector_count("#file a[href='/workspace?tab=ideas&id=1']", 1);

    // The idea is a document too. HTML in it stays text.
    let idea = rig.client.get("/workspace?tab=ideas&id=1").send().await;
    idea.assert_selector_count("#file button[hx-post='/plans/drop-idea/1']", 1);
    idea.assert_text_contains("#file .doc", "They <b>rot</b>.");
    idea.assert_selector_count("#file b", 0);

    rig.client
        .post("/plans/accept/1")
        .send()
        .await
        .assert_status(403);
    let accepted = rig
        .client
        .post("/plans/accept/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    accepted.assert_status(200);
    accepted.assert_selector_count("#plan-view .tag.active", 1);
    accepted.assert_selector_count("#plan-view button[hx-post='/plans/drop-goal/1']", 1);
    accepted.assert_text_contains("#plan-view .meta", "checks every 60 min");
    // The tabs come along out of band, and nothing waits any more.
    accepted.assert_selector_count("#tabs[hx-swap-oob='true']", 1);
    accepted.assert_selector_count("#tabs .count", 0);
    rig.client
        .get("/workspace?tab=ideas")
        .send()
        .await
        .assert_text_contains("#plan-view", "No ideas yet");

    let again = rig
        .client
        .post("/plans/accept/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    again.assert_text_contains("#plan-view .error", "1 is not a proposed goal");
    rig.client
        .post("/plans/bogus/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .assert_status(404);

    rig.client.log_out();
    rig.client
        .post("/plans/drop-goal/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .assert_status(401);
}

#[cfg(feature = "skills")]
#[tokio::test(flavor = "multi_thread")]
async fn the_owner_accepts_and_retires_a_skill_on_the_skills_tab() {
    let rig = rig_on(&config(true), None, Arc::new(Saver)).await;
    let token = sign_in(&rig).await;
    rig.client
        .get("/workspace?tab=skills")
        .send()
        .await
        .assert_text_contains("#plan-view", "No skills yet");

    let drafted = rig
        .handle
        .ask(Request::Chat {
            message_id: String::from("k-1"),
            text: String::from(
                r#"use propose_skill {"name": "weekly-digest", "description": "Write the Friday digest.", "body": "1. Read <b>the</b> week."}"#,
            ),
        })
        .await
        .unwrap();
    assert_eq!(reply_text(&drafted), "saved");

    // Every tab counts the draft. The skills tab opens it as a document.
    let files = rig.client.get("/workspace").send().await;
    files.assert_text_contains("#tabs a[href='/workspace?tab=skills'] .count", "1 waiting");
    let skills = rig.client.get("/workspace?tab=skills").send().await;
    skills.assert_text_contains("#file .doc .tag", "waiting for you");
    skills.assert_selector_count("#file button[hx-post='/skills/accept/1']", 1);
    skills.assert_selector_count("#file button[hx-post='/skills/decline/1']", 1);
    skills.assert_selector_count("#file b", 0);

    rig.client
        .post("/skills/accept/1")
        .send()
        .await
        .assert_status(403);
    let accepted = rig
        .client
        .post("/skills/accept/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    accepted.assert_status(200);
    accepted.assert_selector_count("#plan-view .tag.active", 1);
    accepted.assert_text_contains("#plan-view .meta", "by littleguy");
    accepted.assert_text_contains("#plan-view #file .doc h2", "weekly-digest");
    accepted.assert_selector_count(
        "#plan-view button[hx-post='/skills/retire/weekly-digest']",
        1,
    );
    accepted.assert_selector_count("#tabs[hx-swap-oob='true']", 1);
    accepted.assert_selector_count("#tabs .count", 0);

    let retired = rig
        .client
        .post("/skills/retire/weekly-digest")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    retired.assert_text_contains("#plan-view", "No skills yet");
    let again = rig
        .client
        .post("/skills/retire/weekly-digest")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    again.assert_text_contains("#plan-view .error", "no skill is named weekly-digest");
    rig.client
        .post("/skills/bogus/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .assert_status(404);
    rig.client.log_out();
    rig.client
        .post("/skills/accept/1")
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .assert_status(401);
}

#[tokio::test(flavor = "multi_thread")]
async fn new_conversation_starts_the_thread_over() {
    let rig = rig(&config(false)).await;
    let token = sign_in(&rig).await;
    chat(&rig, &token, "hi").await;
    let home = rig.client.get("/").send().await;
    home.assert_body_contains("you said");
    assert_eq!(home.selector_count("button[hx-post='/new']"), 1);

    let cleared = rig
        .client
        .post("/new")
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    cleared.assert_status(200);
    cleared.assert_body_contains("Say hi to littleguy.");
    let home = rig.client.get("/").send().await;
    home.assert_body_contains("Say hi to littleguy.");
    assert!(!home.text().contains("you said"), "the old turns are gone");

    rig.client.log_out();
    rig.client
        .post("/new")
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .assert_status(401);
}

/// A sandbox backend that runs nothing and prints `hi`.
#[cfg(feature = "sandbox")]
#[derive(Debug)]
struct Echo;

#[cfg(feature = "sandbox")]
impl autumn_harvest::sandbox::SandboxBackend for Echo {
    fn isolation(&self) -> autumn_harvest::sandbox::IsolationLevel {
        autumn_harvest::sandbox::IsolationLevel::Process
    }

    fn execute(
        &self,
        _job: autumn_harvest::sandbox::SandboxJob,
        _cancel: tokio_util::sync::CancellationToken,
    ) -> autumn_harvest::sandbox::runtime::BoxFuture<
        '_,
        Result<autumn_harvest::sandbox::SandboxOutcome, autumn_harvest::sandbox::SandboxError>,
    > {
        use autumn_harvest::sandbox::{IsolationLevel, RunStatus, SandboxOutcome};
        Box::pin(std::future::ready(Ok(SandboxOutcome {
            status: RunStatus::Exited { code: 0 },
            stdout: String::from("hi\n"),
            stderr: String::new(),
            stdout_bytes: 3,
            stderr_bytes: 0,
            stdout_truncated: false,
            stderr_truncated: false,
            outputs: Vec::new(),
            skipped_outputs: Vec::new(),
            duration_ms: 1,
            isolation: IsolationLevel::Process,
            image: None,
        })))
    }
}

#[cfg(feature = "sandbox")]
#[tokio::test(flavor = "multi_thread")]
async fn a_call_that_waits_is_decided_in_the_chat() {
    let mut config = config(false);
    config.tools.files = false;
    config.sandbox = Some(littleguy_core::SandboxConfig {
        backend: littleguy_core::SandboxBackend::Process,
        network: false,
        approve: littleguy_core::SandboxApprove::Always,
    });
    let runtime = crate::sandbox::runtime_on(
        Arc::new(Echo) as Arc<dyn autumn_harvest::sandbox::SandboxBackend>,
        config.sandbox.as_ref().unwrap(),
    )
    .unwrap();
    let extras = daemon::Extras {
        sandbox: Some(runtime),
        ..daemon::Extras::default()
    };
    let rig = rig_extras(&config, None, Arc::new(Saver), extras).await;
    let token = sign_in(&rig).await;

    // No call waits: the chat shows no card.
    let home = rig.client.get("/").send().await;
    assert_eq!(home.selector_count("#pending .approval"), 0);

    let events = chat(
        &rig,
        &token,
        r#"use run_code {"runtime": "sh", "code": "echo hi"}"#,
    )
    .await;
    assert!(
        shown(&events).contains("Decide it below"),
        "the answer points to the card"
    );
    let pending = rig.client.get("/chat/pending").send().await;
    assert_eq!(pending.selector_count("#pending .approval"), 1);
    pending.assert_body_contains("echo hi");
    let approve = pending.selector_attr("button.approve", "hx-post");
    let approve = approve.into_iter().flatten().next().unwrap();
    // The page shows the card too, after a reload.
    let home = rig.client.get("/").send().await;
    assert_eq!(home.selector_count("#pending .approval"), 1);

    let pressed = rig
        .client
        .post(&approve)
        .header("X-CSRF-Token", &token)
        .send()
        .await;
    pressed.assert_status(200);
    pressed.assert_body_contains("littleguy goes on");
    let result = pressed.selector_attr(".approval.decided", "hx-get");
    let result = result.into_iter().flatten().next().unwrap();
    let mut answer = None;
    for _ in 0..200 {
        let polled = rig.client.get(&result).send().await;
        if polled.status.as_u16() == 200 {
            answer = Some(polled);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let answer = answer.unwrap();
    // The answer joins the thread, and no card is left.
    answer.assert_body_contains("beforeend:#thread");
    answer.assert_body_contains("saved");
    assert_eq!(answer.selector_count("#pending .approval"), 0);

    rig.client.log_out();
    rig.client
        .get("/chat/pending")
        .send()
        .await
        .assert_status(401);
    rig.client
        .post(&approve)
        .header("X-CSRF-Token", &token)
        .send()
        .await
        .assert_status(401);
}

/// A model that holds each answer until the test opens its gate.
#[derive(Debug)]
struct Gated {
    /// Notified when a call starts.
    entered: Arc<Notify>,
    /// Holds the answer.
    gate: Arc<Notify>,
}

impl autumn_harvest_agent::AgentModel for Gated {
    fn chat<'a>(
        &'a self,
        _request: &'a autumn_harvest_agent::ChatRequest,
    ) -> autumn_harvest_agent::BoxFuture<
        'a,
        Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
    > {
        use autumn_harvest_agent::{ChatResponse, ContentPart, StopReason, TokenUsage};
        Box::pin(async move {
            self.entered.notify_one();
            self.gate.notified().await;
            Ok(ChatResponse {
                content: vec![ContentPart::Text(String::from("done"))],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(1, 1),
            })
        })
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_pages_open_while_the_assistant_thinks() {
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let model = Gated {
        entered: Arc::clone(&entered),
        gate: Arc::clone(&gate),
    };
    let rig = rig_on(&config(true), None, Arc::new(model)).await;
    sign_in(&rig).await;
    let handle = rig.handle.clone();
    let chat = tokio::spawn(async move {
        handle
            .ask(Request::Chat {
                message_id: String::from("cli-1-1"),
                text: String::from("think hard"),
            })
            .await
    });
    // The run holds the loop of the daemon now.
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.notified())
        .await
        .unwrap();
    for path in ["/", "/workspace", "/manage"] {
        let page = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rig.client.get(path).send(),
        )
        .await
        .unwrap_or_else(|_| panic!("{path} waited for the run"));
        page.assert_status(200);
    }
    gate.notify_one();
    let answer = chat.await.unwrap().unwrap();
    assert!(
        matches!(answer, Response::Reply { .. }),
        "the chat answers once the gate opens"
    );
}

#[cfg(feature = "history")]
#[tokio::test(flavor = "multi_thread")]
async fn the_history_lists_past_conversations_and_opens_one_read_only() {
    let rig = rig(&config(false)).await;
    sign_in(&rig).await;
    let empty = rig.client.get("/history").send().await;
    empty.assert_status(200);
    empty.assert_body_contains("for 30 days ([runs] keep_days)");
    empty.assert_body_contains("No conversation yet.");

    for (id, text) in [("cli-1-1", "green tea"), ("cli-2-1", "oolong")] {
        rig.handle
            .ask(Request::Chat {
                message_id: String::from(id),
                text: String::from(text),
            })
            .await
            .unwrap();
    }
    rig.handle.ask(Request::NewConversation).await.unwrap();
    rig.handle
        .ask(Request::Chat {
            message_id: String::from("cli-3-1"),
            text: String::from("coffee"),
        })
        .await
        .unwrap();

    let listing = rig.client.get("/history").send().await;
    listing.assert_status(200);
    assert_eq!(listing.selector_count("ul.conversations li"), 2);
    let links: Vec<String> = listing
        .selector_attr("ul.conversations a", "href")
        .into_iter()
        .flatten()
        .collect();
    // The live conversation goes to the chat. The old one opens here.
    assert_eq!(links[0], "/");
    assert!(
        links[1].starts_with("/history/"),
        "an old conversation opens on its own page"
    );
    listing.assert_body_contains("green tea");

    let old = rig.client.get(&links[1]).send().await;
    old.assert_status(200);
    old.assert_body_contains("you said 'oolong'");
    assert_eq!(old.selector_count("#thread .msg.you"), 2);
    assert_eq!(old.selector_count("#composer"), 0, "read only");

    let missing = rig.client.get("/history/99999").send().await;
    missing.assert_body_contains("no past conversation has the id 99999");
}
