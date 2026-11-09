//! Discord: direct messages over the gateway, replies over the REST API.
//!
//! The gateway client comes from `autumn-plugin-discord`. It connects,
//! identifies and heartbeats. It does not resume a session, and a failed
//! connect ends its run. So [`run_gateway`] runs it under a supervisor that
//! connects again with a backoff. After each connect, Discord sends
//! `READY`. The daemon then reads the history it missed with
//! [`DiscordRest::messages_after`], so a message sent while littleguy was
//! down still gets an answer.
//!
//! The plugin has no call that posts a message to a channel or opens a DM.
//! [`DiscordRest`] adds both, with one retry after a rate limit.
//!
//! A message can carry buttons. A press of a button arrives on the gateway
//! as an `INTERACTION_CREATE` ([`ButtonPress`]). Discord waits three seconds
//! for its answer, so the daemon answers each press before it does slow
//! work.

use std::time::Duration;

use autumn_plugin_discord::gateway::{EventHandler, GATEWAY_URL, GatewayClient};
use serde_json::{Value, json};
use thiserror::Error;
use tokio::sync::mpsc;

/// The REST API base of Discord.
pub const API_BASE: &str = "https://discord.com/api/v10";

/// The gateway URL of Discord.
pub const GATEWAY: &str = GATEWAY_URL;

/// The gateway intent for direct messages. Discord sends the content of a
/// direct message without the privileged message-content intent.
pub const DIRECT_MESSAGES: u32 = 1 << 12;

/// The longest message that Discord takes, in characters.
pub const MAX_MESSAGE_CHARS: usize = 2000;

/// The most messages that one history read returns.
pub const HISTORY_PAGE: usize = 100;

/// The longest wait that a rate limit may ask for before the one retry.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);

/// The first and the longest pause between two gateway connects.
const BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// Why a Discord call failed.
#[derive(Debug, Error)]
pub enum ChannelError {
    /// The request did not reach Discord, or the answer was lost.
    #[error("discord request failed: {0}")]
    Transport(String),
    /// Discord answered with an error status.
    #[error("discord answered HTTP {status}: {body}")]
    Http {
        /// The HTTP status.
        status: u16,
        /// The start of the body.
        body: String,
    },
    /// The answer is not what the API documents.
    #[error("discord sent an answer that does not parse: {0}")]
    Decode(String),
}

/// One direct message to the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmMessage {
    /// The message snowflake.
    pub id: String,
    /// The DM channel snowflake. A reply goes here.
    pub channel_id: String,
    /// The author's user snowflake.
    pub author_id: String,
    /// The text.
    pub content: String,
}

/// One press of a message button.
#[derive(Clone, PartialEq, Eq)]
pub struct ButtonPress {
    /// The interaction snowflake.
    pub interaction_id: String,
    /// The token that answers the interaction. It stays out of each log.
    pub token: String,
    /// The user who pressed the button.
    pub user_id: String,
    /// The custom id of the button.
    pub custom_id: String,
    /// The channel of the message.
    pub channel_id: String,
}

/// The token stays out of every log line.
impl std::fmt::Debug for ButtonPress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ButtonPress")
            .field("interaction_id", &self.interaction_id)
            .field("user_id", &self.user_id)
            .field("custom_id", &self.custom_id)
            .field("channel_id", &self.channel_id)
            .finish_non_exhaustive()
    }
}

/// The interaction type of a message component.
const MESSAGE_COMPONENT: u64 = 3;

/// Reads a button press from an `INTERACTION_CREATE` payload.
///
/// Another interaction type yields `None`. In a DM the user is `user`. In a
/// server the user is `member.user`.
#[must_use]
pub fn parse_press(data: &Value) -> Option<ButtonPress> {
    if data["type"].as_u64() != Some(MESSAGE_COMPONENT) {
        return None;
    }
    let user = data["user"]["id"]
        .as_str()
        .or_else(|| data["member"]["user"]["id"].as_str())?;
    Some(ButtonPress {
        interaction_id: data["id"].as_str()?.to_owned(),
        token: data["token"].as_str()?.to_owned(),
        user_id: user.to_owned(),
        custom_id: data["data"]["custom_id"].as_str()?.to_owned(),
        channel_id: data["channel_id"].as_str()?.to_owned(),
    })
}

/// The look of a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonStyle {
    /// Green.
    Success,
    /// Red.
    Danger,
}

/// One button of a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Button {
    /// The id that a press carries back. At most 100 characters.
    pub custom_id: String,
    /// The label.
    pub label: String,
    /// The look.
    pub style: ButtonStyle,
}

impl Button {
    fn to_json(&self) -> Value {
        let style = match self.style {
            ButtonStyle::Success => 3,
            ButtonStyle::Danger => 4,
        };
        json!({"type": 2, "style": style, "label": self.label, "custom_id": self.custom_id})
    }
}

/// One event of the gateway that the daemon acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatewayEvent {
    /// A new session is ready. Read the history that was missed.
    Ready,
    /// A direct message arrived.
    Message(DmMessage),
    /// A user pressed a button.
    Press(ButtonPress),
}

/// Reads a direct message from a `MESSAGE_CREATE` payload or a history
/// entry.
///
/// A message in a server, a message from a bot (this one included), and a
/// message with no text yield `None`. So the bot never answers itself.
#[must_use]
pub fn parse_dm(data: &Value) -> Option<DmMessage> {
    if data.get("guild_id").is_some_and(|g| !g.is_null()) {
        return None;
    }
    if data["author"]["bot"].as_bool() == Some(true) {
        return None;
    }
    let content = data["content"].as_str()?.trim();
    if content.is_empty() {
        return None;
    }
    Some(DmMessage {
        id: data["id"].as_str()?.to_owned(),
        channel_id: data["channel_id"].as_str()?.to_owned(),
        author_id: data["author"]["id"].as_str()?.to_owned(),
        content: content.to_owned(),
    })
}

/// Whether snowflake `a` comes after snowflake `b`. A snowflake that does
/// not parse comes before every other one, so it never moves a cursor.
#[must_use]
pub fn is_after(a: &str, b: &str) -> bool {
    match (a.parse::<u64>(), b.parse::<u64>()) {
        (Ok(a), Ok(b)) => a > b,
        (Ok(_), Err(_)) => true,
        (Err(_), _) => false,
    }
}

/// Splits `text` into messages of at most `max` characters.
///
/// A cut falls at the last line break of a piece when it has one, so code
/// blocks and lists stay readable. A piece with no line break is cut at the
/// limit.
#[must_use]
pub fn split_message(text: &str, max: usize) -> Vec<String> {
    let max = max.max(1);
    let mut pieces = Vec::new();
    let mut rest = text;
    while rest.chars().count() > max {
        let limit = rest
            .char_indices()
            .nth(max)
            .map_or(rest.len(), |(at, _)| at);
        let head = &rest[..limit];
        let cut = head
            .rfind('\n')
            .filter(|at| *at > 0)
            .map_or(limit, |at| at + 1);
        pieces.push(rest[..cut].trim_end_matches('\n').to_owned());
        rest = &rest[cut..];
    }
    if !rest.is_empty() || pieces.is_empty() {
        pieces.push(rest.to_owned());
    }
    pieces
}

/// The REST calls that littleguy makes.
#[derive(Clone)]
pub struct DiscordRest {
    base: String,
    token: String,
    client: reqwest::Client,
}

/// The token stays out of every log line.
impl std::fmt::Debug for DiscordRest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordRest")
            .field("base", &self.base)
            .field("token", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl DiscordRest {
    /// A client for the bot `token` against `base`, such as [`API_BASE`].
    #[must_use]
    pub fn new(base: impl Into<String>, token: impl Into<String>) -> Self {
        // Reqwest panics without a provider. An `Err` means that one is installed.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            token: token.into(),
            client,
        }
    }

    /// Sends `text` to `channel_id`, split into pieces that Discord takes.
    ///
    /// # Errors
    ///
    /// Returns the error of the first piece that fails. The pieces before it
    /// stay sent.
    pub async fn send(&self, channel_id: &str, text: &str) -> Result<(), ChannelError> {
        let path = format!("/channels/{channel_id}/messages");
        for piece in split_message(text, MAX_MESSAGE_CHARS) {
            let body = json!({"content": piece, "allowed_mentions": {"parse": []}});
            self.call(reqwest::Method::POST, &path, Some(&body)).await?;
        }
        Ok(())
    }

    /// Sends `text` to `channel_id` with `buttons` under it. A long text
    /// splits, and the buttons go under its last piece.
    ///
    /// # Errors
    ///
    /// Returns the error of the first piece that fails.
    pub async fn send_with_buttons(
        &self,
        channel_id: &str,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), ChannelError> {
        let path = format!("/channels/{channel_id}/messages");
        let pieces = split_message(text, MAX_MESSAGE_CHARS);
        let last = pieces.len().saturating_sub(1);
        for (index, piece) in pieces.into_iter().enumerate() {
            let mut body = json!({"content": piece, "allowed_mentions": {"parse": []}});
            if index == last && !buttons.is_empty() {
                let row: Vec<Value> = buttons.iter().map(Button::to_json).collect();
                body["components"] = json!([{"type": 1, "components": row}]);
            }
            self.call(reqwest::Method::POST, &path, Some(&body)).await?;
        }
        Ok(())
    }

    /// Answers `press` by replacing its message with `text`, with no
    /// buttons. So nobody can press them again.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn update_pressed(
        &self,
        press: &ButtonPress,
        text: &str,
    ) -> Result<(), ChannelError> {
        let content: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        let body = json!({
            "type": 7,
            "data": {"content": content, "components": [], "allowed_mentions": {"parse": []}},
        });
        self.callback(press, &body).await
    }

    /// Answers `press` with `text` that only the presser sees. The message
    /// and its buttons stay.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn answer_privately(
        &self,
        press: &ButtonPress,
        text: &str,
    ) -> Result<(), ChannelError> {
        let content: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        let body = json!({
            "type": 4,
            "data": {"content": content, "flags": 64, "allowed_mentions": {"parse": []}},
        });
        self.callback(press, &body).await
    }

    /// One interaction callback. The token of the press authorizes it, so
    /// the call sends no bot token.
    async fn callback(&self, press: &ButtonPress, body: &Value) -> Result<(), ChannelError> {
        let path = format!(
            "/interactions/{}/{}/callback",
            press.interaction_id, press.token
        );
        self.send_once(reqwest::Method::POST, &path, Some(body), false)
            .await
            .map(drop)
    }

    /// The DM channel with `user_id`. Discord returns the same channel for
    /// the same user, so a second call opens nothing new.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the answer has no id.
    pub async fn open_dm(&self, user_id: &str) -> Result<String, ChannelError> {
        let body = json!({"recipient_id": user_id});
        let answer = self
            .call(reqwest::Method::POST, "/users/@me/channels", Some(&body))
            .await?;
        answer["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ChannelError::Decode(String::from("the DM channel has no id")))
    }

    /// The direct messages in `channel_id` after the snowflake `after`,
    /// oldest first, at most [`HISTORY_PAGE`] of them.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the answer is not a list.
    pub async fn messages_after(
        &self,
        channel_id: &str,
        after: &str,
    ) -> Result<Vec<DmMessage>, ChannelError> {
        let path = format!("/channels/{channel_id}/messages?after={after}&limit={HISTORY_PAGE}");
        let answer = self.call(reqwest::Method::GET, &path, None).await?;
        let list = answer
            .as_array()
            .ok_or_else(|| ChannelError::Decode(String::from("the history is not a list")))?;
        let mut messages: Vec<DmMessage> = list.iter().filter_map(parse_dm).collect();
        messages.sort_by(|a, b| {
            if is_after(&a.id, &b.id) {
                std::cmp::Ordering::Greater
            } else if is_after(&b.id, &a.id) {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });
        Ok(messages)
    }

    /// One call with the bot token, with one retry after a rate limit.
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, ChannelError> {
        self.send_once(method, path, body, true).await
    }

    /// One call, with one retry after a rate limit. `bot` adds the token.
    async fn send_once(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&Value>,
        bot: bool,
    ) -> Result<Value, ChannelError> {
        let mut retried = false;
        loop {
            let mut request = self
                .client
                .request(method.clone(), format!("{}{path}", self.base));
            if bot {
                request = request.header("Authorization", format!("Bot {}", self.token));
            }
            if let Some(body) = body {
                request = request.json(body);
            }
            let response = request
                .send()
                .await
                .map_err(|e| ChannelError::Transport(e.without_url().to_string()))?;
            let status = response.status().as_u16();
            let text = response
                .text()
                .await
                .map_err(|e| ChannelError::Transport(e.without_url().to_string()))?;
            if status == 429 && !retried {
                retried = true;
                tokio::time::sleep(retry_wait(&text)).await;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(ChannelError::Http {
                    status,
                    body: text.chars().take(300).collect(),
                });
            }
            if text.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&text).map_err(|e| ChannelError::Decode(e.to_string()));
        }
    }
}

/// The wait that a 429 body asks for, capped at [`MAX_RETRY_WAIT`].
fn retry_wait(body: &str) -> Duration {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| v["retry_after"].as_f64())
        .filter(|secs| secs.is_finite() && *secs >= 0.0)
        .map_or(Duration::from_secs(1), Duration::from_secs_f64)
        .min(MAX_RETRY_WAIT)
}

/// Passes the events that matter to the daemon.
struct Forward(mpsc::UnboundedSender<GatewayEvent>);

impl EventHandler for Forward {
    fn on_event(&self, name: &str, data: &Value) {
        let event = match name {
            "READY" => Some(GatewayEvent::Ready),
            "MESSAGE_CREATE" => parse_dm(data).map(GatewayEvent::Message),
            "INTERACTION_CREATE" => parse_press(data).map(GatewayEvent::Press),
            _ => None,
        };
        if let Some(event) = event {
            // A closed receiver means the daemon stops. The supervisor ends next.
            let _ = self.0.send(event);
        }
    }
}

/// The next pause of the supervisor.
fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF.1)
}

/// Runs the gateway at `url` until `events` closes, and connects again after
/// each drop or failed connect.
///
/// The pause between two connects starts at one second and doubles up to a
/// minute. A connection that delivered an event resets it.
pub async fn run_gateway(token: String, url: String, events: mpsc::UnboundedSender<GatewayEvent>) {
    let client = GatewayClient::new(token, DIRECT_MESSAGES);
    let mut pause = BACKOFF.0;
    while !events.is_closed() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let forward = Forward(tx);
        let run = client.run_with_url(&url, &forward);
        tokio::pin!(run);
        let mut delivered = false;
        let outcome = loop {
            tokio::select! {
                outcome = &mut run => break outcome,
                Some(event) = rx.recv() => {
                    delivered = true;
                    if events.send(event).is_err() {
                        return;
                    }
                }
            }
        };
        while let Ok(event) = rx.try_recv() {
            delivered = true;
            if events.send(event).is_err() {
                return;
            }
        }
        match outcome {
            Ok(()) => tracing::warn!("the discord gateway closed the connection"),
            Err(error) => tracing::warn!(%error, "the discord gateway failed"),
        }
        if delivered {
            pause = BACKOFF.0;
        }
        tokio::time::sleep(pause).await;
        pause = next_backoff(pause);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dm(id: &str) -> Value {
        json!({
            "id": id,
            "channel_id": "900",
            "author": {"id": "42", "username": "mark"},
            "content": " hello ",
        })
    }

    #[test]
    fn a_direct_message_parses() {
        let message = parse_dm(&dm("10")).unwrap();
        assert_eq!(
            message,
            DmMessage {
                id: "10".into(),
                channel_id: "900".into(),
                author_id: "42".into(),
                content: "hello".into(),
            }
        );
    }

    #[test]
    fn a_server_message_a_bot_message_or_an_empty_one_is_skipped() {
        let mut guild = dm("1");
        guild["guild_id"] = json!("5");
        assert_eq!(parse_dm(&guild), None);
        let mut null_guild = dm("1");
        null_guild["guild_id"] = Value::Null;
        assert!(parse_dm(&null_guild).is_some());
        let mut bot = dm("1");
        bot["author"]["bot"] = json!(true);
        assert_eq!(parse_dm(&bot), None);
        let mut empty = dm("1");
        empty["content"] = json!("   ");
        assert_eq!(parse_dm(&empty), None);
        assert_eq!(parse_dm(&json!({"content": "x"})), None);
    }

    #[test]
    fn snowflakes_compare_as_numbers() {
        assert!(is_after("10", "9"));
        assert!(!is_after("9", "10"));
        assert!(!is_after("9", "9"));
        assert!(is_after("1", "not a snowflake"));
        assert!(!is_after("x", "1"));
    }

    #[test]
    fn a_long_reply_splits_at_line_breaks_and_at_the_limit() {
        assert_eq!(split_message("short", 10), ["short"]);
        assert_eq!(split_message("", 10), [""]);
        assert_eq!(split_message("aaaa\nbbbb\ncc", 10), ["aaaa\nbbbb", "cc"]);
        assert_eq!(split_message("abcdefghij12", 10), ["abcdefghij", "12"]);
        let wide = "é".repeat(25);
        let pieces = split_message(&wide, 10);
        assert_eq!(pieces.len(), 3);
        assert!(pieces.iter().all(|p| p.chars().count() <= 10));
        assert_eq!(pieces.concat(), wide);
        assert_eq!(split_message("x", 0), ["x"]);
    }

    #[test]
    fn every_piece_fits_and_no_text_is_lost() {
        let text = "line one\n".repeat(400);
        let pieces = split_message(&text, MAX_MESSAGE_CHARS);
        assert!(pieces.len() > 1);
        assert!(
            pieces
                .iter()
                .all(|p| p.chars().count() <= MAX_MESSAGE_CHARS)
        );
        let joined: String = pieces.join("\n");
        assert_eq!(joined.replace('\n', ""), text.replace('\n', ""));
    }

    #[test]
    fn a_rate_limit_wait_is_read_and_capped() {
        assert_eq!(
            retry_wait(r#"{"retry_after": 0.5}"#),
            Duration::from_millis(500)
        );
        assert_eq!(retry_wait(r#"{"retry_after": 600}"#), MAX_RETRY_WAIT);
        assert_eq!(retry_wait("nope"), Duration::from_secs(1));
        assert_eq!(retry_wait(r#"{"retry_after": -1}"#), Duration::from_secs(1));
    }

    #[test]
    fn the_backoff_doubles_up_to_a_minute() {
        assert_eq!(next_backoff(Duration::from_secs(1)), Duration::from_secs(2));
        assert_eq!(
            next_backoff(Duration::from_secs(40)),
            Duration::from_secs(60)
        );
    }

    #[test]
    fn the_token_stays_out_of_debug_output() {
        let rest = DiscordRest::new(API_BASE, "secret-token");
        assert!(!format!("{rest:?}").contains("secret-token"));
    }

    fn press(kind: u64) -> Value {
        json!({
            "id": "77",
            "token": "secret-press-token",
            "type": kind,
            "channel_id": "900",
            "user": {"id": "42"},
            "data": {"custom_id": "lg:approve:3", "component_type": 2},
        })
    }

    #[test]
    fn a_button_press_parses_in_a_dm_and_in_a_server() {
        let parsed = parse_press(&press(3)).unwrap();
        assert_eq!(parsed.interaction_id, "77");
        assert_eq!(parsed.user_id, "42");
        assert_eq!(parsed.custom_id, "lg:approve:3");
        assert_eq!(parsed.channel_id, "900");
        assert!(!format!("{parsed:?}").contains("secret-press-token"));

        let mut server = press(3);
        server.as_object_mut().unwrap().remove("user");
        server["member"] = json!({"user": {"id": "43"}});
        assert_eq!(parse_press(&server).unwrap().user_id, "43");

        assert_eq!(parse_press(&press(2)), None);
        let mut no_id = press(3);
        no_id["data"] = json!({});
        assert_eq!(parse_press(&no_id), None);
    }

    #[test]
    fn a_button_has_the_documented_shape() {
        let approve = Button {
            custom_id: String::from("a"),
            label: String::from("Approve"),
            style: ButtonStyle::Success,
        };
        assert_eq!(approve.to_json()["style"], 3);
        assert_eq!(approve.to_json()["type"], 2);
        let deny = Button {
            style: ButtonStyle::Danger,
            ..approve
        };
        assert_eq!(deny.to_json()["style"], 4);
    }

    #[test]
    fn the_forwarder_passes_ready_and_direct_messages_only() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let forward = Forward(tx);
        forward.on_event("READY", &json!({}));
        forward.on_event("MESSAGE_CREATE", &dm("7"));
        forward.on_event("TYPING_START", &json!({}));
        let mut guild = dm("8");
        guild["guild_id"] = json!("1");
        forward.on_event("MESSAGE_CREATE", &guild);
        assert_eq!(rx.try_recv().unwrap(), GatewayEvent::Ready);
        assert!(matches!(rx.try_recv().unwrap(), GatewayEvent::Message(m) if m.id == "7"));
        forward.on_event("INTERACTION_CREATE", &press(3));
        forward.on_event("INTERACTION_CREATE", &press(2));
        assert!(matches!(rx.try_recv().unwrap(), GatewayEvent::Press(p) if p.user_id == "42"));
        assert!(rx.try_recv().is_err());
    }
}
