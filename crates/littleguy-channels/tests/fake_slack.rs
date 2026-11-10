//! The Slack channel against the fake Slack.

use std::error::Error;
use std::time::Duration;

use littleguy_channels::slack::{
    Button, ButtonStyle, ChannelError, MAX_MESSAGE_CHARS, SlackApi, SlackEvent, SlackPress,
    run_socket,
};
use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack, Posted, dm_event};
use tokio::sync::mpsc;

type TestResult = Result<(), Box<dyn Error>>;

fn api(fake: &FakeSlack) -> SlackApi {
    SlackApi::new(&fake.api_base, BOT_TOKEN, APP_TOKEN)
}

#[tokio::test]
async fn the_api_posts_escaped_text_opens_a_dm_and_reads_history() -> TestResult {
    let fake = FakeSlack::start().await?;
    let api = api(&fake);
    api.send("D1", "hi <!channel> & you").await?;
    let long = "x".repeat(MAX_MESSAGE_CHARS + 5);
    api.send("D1", &long).await?;
    let posted = fake.posted();
    assert_eq!(posted.len(), 3);
    assert_eq!(
        posted[0],
        Posted {
            channel: "D1".into(),
            text: "hi &lt;!channel&gt; &amp; you".into(),
            buttons: Vec::new(),
        }
    );
    assert_eq!(posted[2].text, "xxxxx");

    assert_eq!(api.open_dm("U42").await?, "DU42");

    fake.add_history(dm_event("10.000001", "D1", "U42", "old"));
    fake.add_history(dm_event("12.000000", "D1", "U42", "newest"));
    fake.add_history(dm_event("11.500000", "D1", "U42", "newer"));
    let mut own = dm_event("13.000000", "D1", "U0", "my own reply");
    own["bot_id"] = serde_json::json!("B1");
    fake.add_history(own);
    let after: Vec<String> = api
        .messages_after("D1", "10.000001")
        .await?
        .into_iter()
        .map(|m| m.content)
        .collect();
    assert_eq!(after, ["newer", "newest"]);
    Ok(())
}

#[tokio::test]
async fn buttons_go_under_the_last_piece_and_a_press_is_answered() -> TestResult {
    let fake = FakeSlack::start().await?;
    let api = api(&fake);
    let buttons = [
        Button {
            custom_id: "lg:approve:1".into(),
            label: "Approve".into(),
            style: ButtonStyle::Success,
        },
        Button {
            custom_id: "lg:deny:1".into(),
            label: "Deny".into(),
            style: ButtonStyle::Danger,
        },
    ];
    api.send_with_buttons("D1", "run this?", &buttons).await?;
    // Escapes make this piece too long for a section block.
    api.send_with_buttons("D1", &"<".repeat(MAX_MESSAGE_CHARS), &buttons)
        .await?;
    let posted = fake.posted();
    assert_eq!(posted.len(), 3);
    assert_eq!(posted[0].text, "run this?");
    assert_eq!(posted[0].buttons, ["lg:approve:1", "lg:deny:1"]);
    assert!(posted[1].buttons.is_empty() && posted[1].text.starts_with("&lt;"));
    assert_eq!(posted[2].buttons, ["lg:approve:1", "lg:deny:1"]);
    assert_ne!(posted[2].text, posted[1].text, "the long text goes once");

    let press = SlackPress {
        user_id: "U1".into(),
        custom_id: "lg:approve:1".into(),
        channel_id: "D1".into(),
        message_ts: "1001.000000".into(),
    };
    api.update_pressed(&press, "Approved <now>").await?;
    api.answer_privately(&press, "only you").await?;
    let answers = fake.answers();
    assert_eq!(answers.len(), 2);
    assert_eq!(
        (answers[0].kind, answers[0].target.as_str()),
        ("update", "1001.000000")
    );
    assert_eq!(answers[0].text, "Approved &lt;now&gt;");
    assert_eq!(
        answers[0].blocks.as_deref(),
        Some("[]"),
        "the buttons go away"
    );
    assert_eq!(
        (answers[1].kind, answers[1].target.as_str()),
        ("ephemeral", "U1")
    );
    Ok(())
}

#[tokio::test]
async fn errors_are_errors_and_one_rate_limit_is_waited_out() -> TestResult {
    let fake = FakeSlack::start().await?;
    let api = api(&fake);
    fake.rate_limit(1);
    api.send("D1", "after a wait").await?;
    assert_eq!(fake.posted().len(), 1);

    fake.rate_limit(2);
    let error = api.send("D1", "twice").await.err().ok_or("no error")?;
    assert!(
        matches!(error, ChannelError::Http { status: 429, .. }),
        "{error}"
    );

    fake.fail_posts(1);
    let error = api.send("D1", "boom").await.err().ok_or("no error")?;
    assert!(error.to_string().contains("HTTP 500"), "{error}");

    let wrong = SlackApi::new(&fake.api_base, "xoxb-wrong", APP_TOKEN);
    let error = wrong.send("D1", "x").await.err().ok_or("no error")?;
    assert!(
        matches!(error, ChannelError::Api(ref code) if code == "invalid_auth"),
        "{error}"
    );
    let swapped = SlackApi::new(&fake.api_base, BOT_TOKEN, BOT_TOKEN);
    assert!(
        swapped.socket_url().await.is_err(),
        "the socket needs the app token"
    );
    assert_eq!(fake.posted().len(), 1);
    Ok(())
}

async fn next(
    events: &mut mpsc::UnboundedReceiver<SlackEvent>,
) -> Result<SlackEvent, Box<dyn Error>> {
    let event = tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await?
        .ok_or("the socket ended")?;
    Ok(event)
}

#[tokio::test]
async fn the_socket_acknowledges_each_envelope_and_connects_again() -> TestResult {
    let fake = FakeSlack::start().await?;
    let (tx, mut events) = mpsc::unbounded_channel();
    let socket = tokio::spawn(run_socket(api(&fake), tx));
    assert_eq!(next(&mut events).await?, SlackEvent::Ready);

    fake.push_dm("5.000001", "D1", "U1", "hello &amp; bye");
    let SlackEvent::Message(message) = next(&mut events).await? else {
        return Err("not a message".into());
    };
    assert_eq!(message.content, "hello & bye");
    assert!(fake.wait_for(10, |f| f.acks() == ["env-1"]).await);

    fake.push_press("U1", "lg:deny:2", "6.000000");
    let SlackEvent::Press(press) = next(&mut events).await? else {
        return Err("not a press".into());
    };
    assert_eq!(
        (press.user_id.as_str(), press.channel_id.as_str()),
        ("U1", "DU1")
    );

    let ignored = fake.push_envelope("slash_commands", &serde_json::json!({}));
    assert!(fake.wait_for(10, |f| f.acks().contains(&ignored)).await);

    fake.push_frame(serde_json::json!({"type": "disconnect", "reason": "refresh_requested"}));
    assert_eq!(next(&mut events).await?, SlackEvent::Ready);
    assert_eq!(fake.connects(), 2);

    fake.drop_connection();
    assert_eq!(next(&mut events).await?, SlackEvent::Ready);
    assert_eq!(fake.connects(), 3);

    drop(events);
    fake.push_dm("7.000000", "D1", "U1", "nobody listens");
    tokio::time::timeout(Duration::from_secs(10), socket).await??;
    Ok(())
}
