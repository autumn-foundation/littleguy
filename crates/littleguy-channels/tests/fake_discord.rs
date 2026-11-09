//! The Discord channel against the fake Discord.

use std::error::Error;

use littleguy_channels::discord::{
    Button, ButtonStyle, DIRECT_MESSAGES, DiscordRest, GatewayEvent, MAX_MESSAGE_CHARS, run_gateway,
};
use littleguy_channels::testing::{Callback, FakeDiscord, Sent, dm_event};
use tokio::sync::mpsc;

type TestResult = Result<(), Box<dyn Error>>;

#[tokio::test]
async fn the_rest_client_sends_opens_a_dm_and_reads_history() -> TestResult {
    let fake = FakeDiscord::start().await?;
    let rest = DiscordRest::new(&fake.api_base, "token");
    rest.send("900", "hello").await?;
    let long = "x".repeat(MAX_MESSAGE_CHARS + 5);
    rest.send("900", &long).await?;
    let sent = fake.sent();
    assert_eq!(sent.len(), 3);
    assert_eq!(
        sent[0],
        Sent {
            channel_id: "900".into(),
            content: "hello".into(),
            buttons: Vec::new(),
        }
    );
    assert_eq!(sent[2].content, "xxxxx");

    assert_eq!(rest.open_dm("42").await?, "dm42");

    fake.add_history(dm_event("10", "900", "42", "old"));
    fake.add_history(dm_event("12", "900", "42", "newest"));
    fake.add_history(dm_event("11", "900", "42", "newer"));
    let mut own = dm_event("13", "900", "1", "my own reply");
    own["author"]["bot"] = serde_json::json!(true);
    fake.add_history(own);
    let after: Vec<String> = rest
        .messages_after("900", "10")
        .await?
        .into_iter()
        .map(|m| m.content)
        .collect();
    assert_eq!(after, ["newer", "newest"]);
    Ok(())
}

#[tokio::test]
async fn an_error_status_is_an_error_and_a_bad_token_is_refused() -> TestResult {
    let fake = FakeDiscord::start().await?;
    let rest = DiscordRest::new(&fake.api_base, "token");
    fake.fail_sends(1);
    let error = rest.send("900", "hello").await.err().ok_or("no error")?;
    assert!(error.to_string().contains("HTTP 500"), "{error}");
    rest.send("900", "again").await?;
    assert_eq!(fake.sent().len(), 1);

    let missing = rest.messages_after("900", "0").await?;
    assert_eq!(missing, Vec::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let dead = DiscordRest::new(format!("http://{}", listener.local_addr()?), "token");
    drop(listener);
    let error = dead.send("1", "x").await.err().ok_or("no error")?;
    assert!(error.to_string().contains("request failed"), "{error}");
    Ok(())
}

#[tokio::test]
async fn the_gateway_identifies_forwards_messages_and_reconnects() -> TestResult {
    let fake = FakeDiscord::start().await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let gateway = tokio::spawn(run_gateway(
        String::from("bot-token"),
        fake.gateway_url.clone(),
        tx,
    ));
    assert_eq!(rx.recv().await, Some(GatewayEvent::Ready));
    let identify = &fake.identifies()[0];
    assert_eq!(identify["token"], "bot-token");
    assert_eq!(identify["intents"], DIRECT_MESSAGES);

    fake.push_dm("20", "900", "42", "hi there");
    let Some(GatewayEvent::Message(message)) = rx.recv().await else {
        return Err("no message".into());
    };
    assert_eq!(message.content, "hi there");
    assert_eq!(message.author_id, "42");

    fake.drop_connection();
    assert_eq!(rx.recv().await, Some(GatewayEvent::Ready));
    assert_eq!(fake.connects(), 2);
    drop(rx);
    fake.push_dm("21", "900", "42", "after close");
    tokio::time::timeout(std::time::Duration::from_secs(10), gateway).await??;
    Ok(())
}

#[tokio::test]
async fn buttons_go_out_and_a_press_comes_back_and_gets_its_answer() -> TestResult {
    let fake = FakeDiscord::start().await?;
    let rest = DiscordRest::new(&fake.api_base, "token");
    let buttons = [
        Button {
            custom_id: String::from("lg:approve:1"),
            label: String::from("Approve"),
            style: ButtonStyle::Success,
        },
        Button {
            custom_id: String::from("lg:deny:1"),
            label: String::from("Deny"),
            style: ButtonStyle::Danger,
        },
    ];
    let long = format!("{}\nend", "y".repeat(MAX_MESSAGE_CHARS));
    rest.send_with_buttons("900", &long, &buttons).await?;
    let sent = fake.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].buttons.len(), 0);
    assert_eq!(sent[1].buttons, ["lg:approve:1", "lg:deny:1"]);

    let (tx, mut rx) = mpsc::unbounded_channel();
    let gateway = tokio::spawn(run_gateway(
        String::from("token"),
        fake.gateway_url.clone(),
        tx,
    ));
    assert_eq!(rx.recv().await, Some(GatewayEvent::Ready));
    fake.push_press("55", "42", "lg:approve:1");
    let Some(GatewayEvent::Press(press)) = rx.recv().await else {
        return Err("no press".into());
    };
    assert_eq!(press.custom_id, "lg:approve:1");
    rest.update_pressed(&press, "Approved.").await?;
    rest.answer_privately(&press, "Only for you.").await?;
    assert_eq!(
        fake.callbacks(),
        [
            Callback {
                interaction_id: String::from("55"),
                kind: 7,
                content: String::from("Approved."),
            },
            Callback {
                interaction_id: String::from("55"),
                kind: 4,
                content: String::from("Only for you."),
            },
        ]
    );
    // A forged token is refused.
    let forged = littleguy_channels::discord::ButtonPress {
        token: String::from("wrong"),
        ..press
    };
    let error = rest
        .update_pressed(&forged, "x")
        .await
        .err()
        .ok_or("no error")?;
    assert!(error.to_string().contains("HTTP 401"), "{error}");
    gateway.abort();
    Ok(())
}
