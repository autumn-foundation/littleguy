//! The Slack hooks of the Events API, against a real daemon and the fake
//! Slack.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autumn_plugin_slack::testing::signed_headers;
use autumn_web::test::TestResponse;
use littleguy_channels::testing::slack::{FakeSlack, dm_event};
use serde_json::{Value, json};

use super::*;

pub(super) use littleguy_channels::testing::slack::BOT_TOKEN;

/// The signing secret of the tests.
pub(super) const SECRET: &str = "8f742231b10e8888abcd99yyyzzz85a5";

const EVENTS: &str = "/hooks/slack/events";
const INTERACTIONS: &str = "/hooks/slack/interactions";

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// Posts `body` to `path`, signed with `secret` at `at`.
async fn signed(rig: &Rig, path: &str, body: &str, secret: &str, at: u64) -> TestResponse {
    let mut request = rig.client.post(path).body(body.to_owned());
    for (name, value) in signed_headers(secret, at, body.as_bytes()) {
        request = request.header(name, &value);
    }
    request.send().await
}

/// Posts `event` to the events hook, as Slack does.
async fn event(rig: &Rig, event: Value) -> TestResponse {
    let body = json!({"type": "event_callback", "event_id": "Ev1", "event": event});
    signed(rig, EVENTS, &body.to_string(), SECRET, now()).await
}

fn code_in(text: &str) -> String {
    let start = text.find("lg pair ").unwrap() + "lg pair ".len();
    text[start..].split('`').next().unwrap().to_owned()
}

async fn slack_rig() -> (Rig, FakeSlack) {
    let fake = FakeSlack::start().await.unwrap();
    let rig = rig_with(&hooked("0.0.0.0:8080"), Some(&fake.api_base)).await;
    (rig, fake)
}

#[tokio::test(flavor = "multi_thread")]
async fn the_events_hook_answers_the_challenge_with_a_good_signature_only() {
    let (rig, _fake) = slack_rig().await;
    let challenge = json!({"type": "url_verification", "challenge": "c-123"}).to_string();
    let answer = signed(&rig, EVENTS, &challenge, SECRET, now()).await;
    answer.assert_status(200);
    let body: Value = answer.json();
    assert_eq!(body["challenge"], "c-123");

    rig.client
        .post(EVENTS)
        .body(challenge.clone())
        .send()
        .await
        .assert_status(401);
    signed(&rig, EVENTS, &challenge, "another-secret", now())
        .await
        .assert_status(401);
    // A replay of an old request fails, though its signature is good.
    signed(&rig, EVENTS, &challenge, SECRET, now() - 3600)
        .await
        .assert_status(401);
    signed(
        &rig,
        INTERACTIONS,
        "payload=%7B%7D",
        "another-secret",
        now(),
    )
    .await
    .assert_status(401);

    signed(&rig, EVENTS, "not json", SECRET, now())
        .await
        .assert_status(400);
    signed(
        &rig,
        EVENTS,
        r#"{"type":"url_verification"}"#,
        SECRET,
        now(),
    )
    .await
    .assert_status(400);
    signed(
        &rig,
        EVENTS,
        r#"{"type":"app_rate_limited"}"#,
        SECRET,
        now(),
    )
    .await
    .assert_status(200);
    signed(&rig, INTERACTIONS, "no=payload", SECRET, now())
        .await
        .assert_status(400);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_direct_message_over_the_hook_gets_its_reply_once() {
    let (rig, fake) = slack_rig().await;
    event(&rig, dm_event("5001.000001", "D555", "U555", "hello"))
        .await
        .assert_status(200);
    assert!(fake.wait_for(10, |f| f.posted().len() == 1).await);
    let code = code_in(&fake.posted()[0].text);
    let paired = rig.handle.ask(Request::Pair { code }).await.unwrap();
    assert!(matches!(paired, Response::Paired { .. }), "pairing failed");

    event(&rig, dm_event("5002.000001", "D555", "U555", "hi"))
        .await
        .assert_status(200);
    assert!(fake.wait_for(10, |f| f.posted().len() == 2).await);
    assert!(fake.posted()[1].text.contains("you said"));
    assert_eq!(fake.posted()[1].channel, "D555");

    // The cursor moves just after the reply goes out. The job queue runs in
    // order, so an answer to a later request means the cursor moved.
    tokio::time::sleep(Duration::from_millis(200)).await;
    rig.handle.ask(Request::Pairs).await.unwrap();
    // Slack sends an event again when the answer is late. The message id
    // keys the op, so the retry runs nothing new.
    event(&rig, dm_event("5002.000001", "D555", "U555", "hi"))
        .await
        .assert_status(200);
    // The bot hears its own reply too, and lets it go.
    let mut own = dm_event("5002.000002", "D555", "U0", "my reply");
    own["bot_id"] = json!("B1");
    event(&rig, own).await.assert_status(200);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fake.posted().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_button_press_over_the_hook_gets_its_answer() {
    let (rig, fake) = slack_rig().await;
    let payload = json!({
        "type": "block_actions",
        "user": {"id": "U9"},
        "channel": {"id": "DU9"},
        "container": {"type": "message", "message_ts": "6.000000", "channel_id": "DU9"},
        "actions": [{"action_id": "lg:approve:1", "type": "button"}],
    });
    let body: String = form_urlencoded::Serializer::new(String::new())
        .append_pair("payload", &payload.to_string())
        .finish();
    signed(&rig, INTERACTIONS, &body, SECRET, now())
        .await
        .assert_status(200);
    assert!(fake.wait_for(10, |f| f.answers().len() == 1).await);
    let answer = &fake.answers()[0];
    assert_eq!((answer.kind, answer.target.as_str()), ("ephemeral", "U9"));

    // Another kind of interaction is not a press, and starts nothing.
    let other = form_urlencoded::Serializer::new(String::new())
        .append_pair("payload", r#"{"type":"view_submission"}"#)
        .finish();
    signed(&rig, INTERACTIONS, &other, SECRET, now())
        .await
        .assert_status(200);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_events_api_the_slack_hooks_do_not_exist() {
    let hooks_only = rig(&hooked("0.0.0.0:8080")).await;
    let challenge = json!({"type": "url_verification", "challenge": "c"}).to_string();
    signed(&hooks_only, EVENTS, &challenge, SECRET, now())
        .await
        .assert_status(404);
    let no_hooks = rig(&config(false)).await;
    signed(&no_hooks, EVENTS, &challenge, SECRET, now())
        .await
        .assert_status(404);
}
