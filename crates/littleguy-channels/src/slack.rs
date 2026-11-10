//! Slack: direct messages over Socket Mode, replies over the Web API.
//!
//! `autumn-plugin-slack` 0.4 serves the Events API on a public HTTPS route,
//! and it has no Socket Mode. A personal daemon has no public URL. So this
//! module opens the Socket Mode WebSocket itself. [`run_socket`] asks
//! `apps.connections.open` for a URL with the app token, connects, and
//! connects again after each drop, with a backoff.
//!
//! Slack resends an envelope that gets no acknowledgement. So the socket
//! acknowledges each envelope before it hands the event on. A message that
//! arrives twice has one message id, so its op runs once.
//!
//! After each `hello`, the daemon reads the history it missed with
//! [`SlackApi::messages_after`], as it does for Discord.
//!
//! The model writes the replies. Slack reads `<!channel>` and `<@U1>` in a
//! text as a mention, so each reply escapes `&`, `<` and `>`. No reply can
//! ping anyone.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

pub use crate::chat::is_after;
use crate::chat::{BACKOFF, http_client, next_backoff, ts_parts};
pub use crate::chat::{Button, ButtonStyle, ChannelError, DmMessage};

/// The Web API base of Slack.
pub const API_BASE: &str = "https://slack.com/api";

/// The longest text of one message, in characters. A section block takes
/// at most 3,000, and escapes can make a text longer.
pub const MAX_MESSAGE_CHARS: usize = 2_500;

/// The longest text that a section block takes.
const MAX_SECTION_CHARS: usize = 3_000;

/// The text above buttons whose message a section cannot hold.
const CHOOSE: &str = "Choose one:";

/// The most messages that one history read returns.
pub const HISTORY_PAGE: usize = 100;

/// The longest wait that a rate limit may ask for before the one retry.
const MAX_RETRY_WAIT: Duration = Duration::from_secs(10);

/// One press of a message button.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlackPress {
    /// The user who pressed the button.
    pub user_id: String,
    /// The action id of the button.
    pub custom_id: String,
    /// The channel of the message.
    pub channel_id: String,
    /// The `ts` of the message that holds the button.
    pub message_ts: String,
}

/// One event of the socket that the daemon acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackEvent {
    /// A new connection is ready. Read the history that was missed.
    Ready,
    /// A direct message arrived.
    Message(DmMessage),
    /// A user pressed a button.
    Press(SlackPress),
}

/// What one Socket Mode frame means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// The connection is ready.
    Hello,
    /// Slack ends this connection. Connect again now.
    Disconnect,
    /// An envelope. Acknowledge `envelope_id`, then act on `event`.
    Envelope {
        /// The id that the acknowledgement carries.
        envelope_id: String,
        /// The event, when it is one that the daemon acts on.
        event: Option<SlackEvent>,
    },
    /// A frame that needs nothing.
    Other,
}

/// Reads one Socket Mode frame.
#[must_use]
pub fn parse_frame(frame: &Value) -> Frame {
    match frame["type"].as_str() {
        Some("hello") => return Frame::Hello,
        Some("disconnect") => return Frame::Disconnect,
        _ => {}
    }
    let Some(envelope_id) = frame["envelope_id"].as_str() else {
        return Frame::Other;
    };
    let payload = &frame["payload"];
    let event = match frame["type"].as_str() {
        Some("events_api") => parse_dm(&payload["event"]).map(SlackEvent::Message),
        Some("interactive") => parse_press(payload).map(SlackEvent::Press),
        _ => None,
    };
    Frame::Envelope {
        envelope_id: envelope_id.to_owned(),
        event,
    }
}

/// Reads a direct message from a `message` event or a history entry.
///
/// A message outside a DM, a message from a bot (this one included), an
/// edit or any other subtype, and a message with no text yield `None`. So
/// the bot never answers itself.
#[must_use]
pub fn parse_dm(event: &Value) -> Option<DmMessage> {
    if event["type"].as_str() != Some("message") {
        return None;
    }
    // A history entry carries no channel type. Its channel is a DM.
    if event
        .get("channel_type")
        .is_some_and(|kind| kind.as_str() != Some("im"))
    {
        return None;
    }
    if event.get("subtype").is_some_and(|s| !s.is_null())
        || event.get("bot_id").is_some_and(|b| !b.is_null())
    {
        return None;
    }
    let content = event["text"].as_str()?.trim();
    if content.is_empty() {
        return None;
    }
    Some(DmMessage {
        id: event["ts"].as_str()?.to_owned(),
        channel_id: event["channel"].as_str()?.to_owned(),
        author_id: event["user"].as_str()?.to_owned(),
        content: unescape(content),
    })
}

/// Reads a button press from a `block_actions` payload. Another payload
/// yields `None`.
#[must_use]
pub fn parse_press(payload: &Value) -> Option<SlackPress> {
    if payload["type"].as_str() != Some("block_actions") {
        return None;
    }
    let channel = payload["channel"]["id"]
        .as_str()
        .or_else(|| payload["container"]["channel_id"].as_str())?;
    Some(SlackPress {
        user_id: payload["user"]["id"].as_str()?.to_owned(),
        custom_id: payload["actions"][0]["action_id"].as_str()?.to_owned(),
        channel_id: channel.to_owned(),
        message_ts: payload["container"]["message_ts"].as_str()?.to_owned(),
    })
}

/// `text` with `&`, `<` and `>` escaped, so Slack shows it as written and
/// reads no mention or link in it.
#[must_use]
pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// The text of a message as the person wrote it. Slack escapes the same
/// three characters in what it sends.
#[must_use]
pub fn unescape(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// The Slack block of `button`.
fn button_json(button: &Button) -> Value {
    let style = match button.style {
        ButtonStyle::Success => "primary",
        ButtonStyle::Danger => "danger",
    };
    json!({
        "type": "button",
        "text": {"type": "plain_text", "text": button.label},
        "action_id": button.custom_id,
        "value": button.custom_id,
        "style": style,
    })
}

/// The Web API calls that littleguy makes.
#[derive(Clone)]
pub struct SlackApi {
    base: String,
    bot_token: String,
    app_token: String,
    client: reqwest::Client,
}

/// The tokens stay out of every log line.
impl std::fmt::Debug for SlackApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SlackApi")
            .field("base", &self.base)
            .field("tokens", &"<redacted>")
            .finish_non_exhaustive()
    }
}

/// Which token a call sends.
#[derive(Clone, Copy)]
enum Token {
    /// The bot token (`xoxb-`), for the Web API.
    Bot,
    /// The app token (`xapp-`), for `apps.connections.open` only.
    App,
}

impl SlackApi {
    /// A client against `base`, such as [`API_BASE`], with the bot token
    /// and the app-level token.
    #[must_use]
    pub fn new(
        base: impl Into<String>,
        bot_token: impl Into<String>,
        app_token: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into().trim_end_matches('/').to_owned(),
            bot_token: bot_token.into(),
            app_token: app_token.into(),
            client: http_client(),
        }
    }

    /// Sends `text` to `channel`, split into pieces that Slack takes.
    ///
    /// # Errors
    ///
    /// Returns the error of the first piece that fails. The pieces before it
    /// stay sent.
    pub async fn send(&self, channel: &str, text: &str) -> Result<(), ChannelError> {
        self.send_with_buttons(channel, text, &[]).await
    }

    /// Sends `text` to `channel` with `buttons` under it. A long text
    /// splits, and the buttons go under its last piece.
    ///
    /// # Errors
    ///
    /// Returns the error of the first piece that fails.
    pub async fn send_with_buttons(
        &self,
        channel: &str,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), ChannelError> {
        let pieces = crate::chat::split_message(text, MAX_MESSAGE_CHARS);
        let last = pieces.len().saturating_sub(1);
        for (index, piece) in pieces.iter().enumerate() {
            let text = escape(piece);
            let mut params = vec![("channel", channel.to_owned())];
            if index == last && !buttons.is_empty() {
                let row: Vec<Value> = buttons.iter().map(button_json).collect();
                let mut blocks = Vec::new();
                let shown = if (1..=MAX_SECTION_CHARS).contains(&text.chars().count()) {
                    blocks
                        .push(json!({"type": "section", "text": {"type": "mrkdwn", "text": text}}));
                    text
                } else {
                    // A section cannot hold it, so the text goes alone first.
                    self.post(vec![("channel", channel.to_owned()), ("text", text)])
                        .await?;
                    String::from(CHOOSE)
                };
                blocks.push(json!({"type": "actions", "elements": row}));
                params.push(("blocks", Value::Array(blocks).to_string()));
                params.push(("text", shown));
            } else {
                params.push(("text", text));
            }
            self.post(params).await?;
        }
        Ok(())
    }

    /// One `chat.postMessage`. Links and media do not unfurl.
    async fn post(&self, mut params: Vec<(&str, String)>) -> Result<(), ChannelError> {
        params.push(("unfurl_links", String::from("false")));
        params.push(("unfurl_media", String::from("false")));
        self.call("chat.postMessage", &params, Token::Bot)
            .await
            .map(drop)
    }

    /// Replaces the message `ts` in `channel` with `text`, with no buttons.
    /// So nobody can press them again.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn update_pressed(&self, press: &SlackPress, text: &str) -> Result<(), ChannelError> {
        let text: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        let params = [
            ("channel", press.channel_id.clone()),
            ("ts", press.message_ts.clone()),
            ("text", escape(&text)),
            ("blocks", String::from("[]")),
        ];
        self.call("chat.update", &params, Token::Bot)
            .await
            .map(drop)
    }

    /// Answers `press` with `text` that only the presser sees. The message
    /// and its buttons stay.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn answer_privately(
        &self,
        press: &SlackPress,
        text: &str,
    ) -> Result<(), ChannelError> {
        let text: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        let params = [
            ("channel", press.channel_id.clone()),
            ("user", press.user_id.clone()),
            ("text", escape(&text)),
        ];
        self.call("chat.postEphemeral", &params, Token::Bot)
            .await
            .map(drop)
    }

    /// The DM channel with `user_id`. Slack returns the same channel for the
    /// same user, so a second call opens nothing new.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the answer has no id.
    pub async fn open_dm(&self, user_id: &str) -> Result<String, ChannelError> {
        let answer = self
            .call(
                "conversations.open",
                &[("users", user_id.to_owned())],
                Token::Bot,
            )
            .await?;
        answer["channel"]["id"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ChannelError::Decode(String::from("the DM channel has no id")))
    }

    /// The direct messages in `channel` after the `ts` `after`, oldest
    /// first, at most [`HISTORY_PAGE`] of them.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the answer has no list.
    pub async fn messages_after(
        &self,
        channel: &str,
        after: &str,
    ) -> Result<Vec<DmMessage>, ChannelError> {
        let params = [
            ("channel", channel.to_owned()),
            ("oldest", after.to_owned()),
            ("inclusive", String::from("false")),
            ("limit", HISTORY_PAGE.to_string()),
        ];
        let answer = self
            .call("conversations.history", &params, Token::Bot)
            .await?;
        let list = answer["messages"]
            .as_array()
            .ok_or_else(|| ChannelError::Decode(String::from("the history has no list")))?;
        let mut messages: Vec<DmMessage> = list
            .iter()
            .filter_map(|entry| {
                let mut entry = entry.clone();
                entry["channel"] = json!(channel);
                parse_dm(&entry)
            })
            .filter(|message| is_after(&message.id, after))
            .collect();
        messages.sort_by_key(|a| ts_parts(&a.id));
        Ok(messages)
    }

    /// A fresh Socket Mode URL. Each URL serves one connection.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails or the answer has no URL.
    pub async fn socket_url(&self) -> Result<String, ChannelError> {
        let answer = self.call("apps.connections.open", &[], Token::App).await?;
        answer["url"]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| ChannelError::Decode(String::from("the socket answer has no url")))
    }

    /// One form-encoded call, with one retry after a rate limit. An answer
    /// with `ok: false` is an error.
    async fn call(
        &self,
        method: &str,
        params: &[(&str, String)],
        token: Token,
    ) -> Result<Value, ChannelError> {
        let token = match token {
            Token::Bot => &self.bot_token,
            Token::App => &self.app_token,
        };
        let mut retried = false;
        loop {
            let response = self
                .client
                .post(format!("{}/{method}", self.base))
                .bearer_auth(token)
                .form(params)
                .send()
                .await
                .map_err(|e| ChannelError::Transport(e.without_url().to_string()))?;
            let status = response.status().as_u16();
            let wait = retry_wait(
                response
                    .headers()
                    .get("retry-after")
                    .and_then(|v| v.to_str().ok()),
            );
            let text = response
                .text()
                .await
                .map_err(|e| ChannelError::Transport(e.without_url().to_string()))?;
            if status == 429 && !retried {
                retried = true;
                tokio::time::sleep(wait).await;
                continue;
            }
            if !(200..300).contains(&status) {
                return Err(ChannelError::Http {
                    status,
                    body: text.chars().take(300).collect(),
                });
            }
            let answer: Value =
                serde_json::from_str(&text).map_err(|e| ChannelError::Decode(e.to_string()))?;
            if answer["ok"].as_bool() != Some(true) {
                let code = answer["error"].as_str().unwrap_or("unknown_error");
                return Err(ChannelError::Api(code.chars().take(100).collect()));
            }
            return Ok(answer);
        }
    }
}

/// The wait that a `Retry-After` header asks for, capped at
/// [`MAX_RETRY_WAIT`].
fn retry_wait(header: Option<&str>) -> Duration {
    header
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(Duration::from_secs(1), Duration::from_secs)
        .min(MAX_RETRY_WAIT)
}

/// How one connection ended.
enum Ended {
    /// Slack asked for a new connection.
    Refresh,
    /// The connection dropped or failed.
    Dropped,
    /// The daemon stopped listening.
    Stopped,
}

/// Runs Socket Mode until `events` closes, and connects again after each
/// drop or failed connect.
///
/// The pause between two connects starts at one second and doubles up to a
/// minute. A connection that said `hello` resets it. A `disconnect` from
/// Slack connects again at once.
pub async fn run_socket(api: SlackApi, events: mpsc::UnboundedSender<SlackEvent>) {
    let mut pause = BACKOFF.0;
    loop {
        let (ended, live) = session(&api, &events).await;
        if live {
            pause = BACKOFF.0;
        }
        match ended {
            Ended::Stopped => return,
            Ended::Refresh => continue,
            Ended::Dropped => {}
        }
        tracing::warn!(pause_secs = pause.as_secs(), "the slack socket dropped");
        tokio::time::sleep(pause).await;
        pause = next_backoff(pause);
    }
}

/// One connection. Returns how it ended, and whether it said `hello`.
async fn session(api: &SlackApi, events: &mpsc::UnboundedSender<SlackEvent>) -> (Ended, bool) {
    if events.is_closed() {
        return (Ended::Stopped, false);
    }
    let url = match api.socket_url().await {
        Ok(url) => url,
        Err(error) => {
            tracing::warn!(%error, "no slack socket url");
            return (Ended::Dropped, false);
        }
    };
    let (ws, _) = match tokio_tungstenite::connect_async(url.as_str()).await {
        Ok(connected) => connected,
        Err(error) => {
            tracing::warn!(%error, "the slack socket did not connect");
            return (Ended::Dropped, false);
        }
    };
    let (mut sink, mut stream) = ws.split();
    let mut live = false;
    while let Some(frame) = stream.next().await {
        let text = match frame {
            Ok(Message::Text(text)) => text,
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(_) => continue,
        };
        let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let event = match parse_frame(&value) {
            Frame::Hello => {
                live = true;
                Some(SlackEvent::Ready)
            }
            Frame::Disconnect => return (Ended::Refresh, live),
            Frame::Envelope { envelope_id, event } => {
                let ack = json!({"envelope_id": envelope_id}).to_string();
                if sink.send(Message::Text(ack.into())).await.is_err() {
                    break;
                }
                event
            }
            Frame::Other => None,
        };
        if let Some(event) = event
            && events.send(event).is_err()
        {
            return (Ended::Stopped, live);
        }
    }
    (Ended::Dropped, live)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dm(ts: &str, text: &str) -> Value {
        json!({"type": "message", "channel": "D1", "channel_type": "im", "user": "U1", "text": text, "ts": ts})
    }

    #[test]
    fn a_direct_message_parses_and_is_unescaped() {
        let message = parse_dm(&dm("1.000100", "a &lt;b&gt; &amp; c")).unwrap();
        assert_eq!(
            message,
            DmMessage {
                id: String::from("1.000100"),
                channel_id: String::from("D1"),
                author_id: String::from("U1"),
                content: String::from("a <b> & c"),
            }
        );
    }

    #[test]
    fn a_channel_message_a_bot_an_edit_or_an_empty_text_is_skipped() {
        let mut channel = dm("1.0", "hi");
        channel["channel_type"] = json!("channel");
        assert_eq!(parse_dm(&channel), None);
        let mut bot = dm("1.0", "hi");
        bot["bot_id"] = json!("B1");
        assert_eq!(parse_dm(&bot), None);
        let mut edit = dm("1.0", "hi");
        edit["subtype"] = json!("message_changed");
        assert_eq!(parse_dm(&edit), None);
        assert_eq!(parse_dm(&dm("1.0", "  ")), None);
        assert_eq!(parse_dm(&json!({"type": "reaction_added"})), None);
        let mut history = dm("1.0", "hi");
        history.as_object_mut().unwrap().remove("channel_type");
        assert!(parse_dm(&history).is_some(), "a history entry has no type");
    }

    #[test]
    fn frames_parse_to_what_the_daemon_needs() {
        assert_eq!(parse_frame(&json!({"type": "hello"})), Frame::Hello);
        assert_eq!(
            parse_frame(&json!({"type": "disconnect", "reason": "refresh_requested"})),
            Frame::Disconnect
        );
        assert_eq!(parse_frame(&json!({"type": "nothing"})), Frame::Other);
        let event = json!({"envelope_id": "e1", "type": "events_api",
            "payload": {"type": "event_callback", "event": dm("2.0", "hey")}});
        let Frame::Envelope { envelope_id, event } = parse_frame(&event) else {
            panic!("not an envelope");
        };
        assert_eq!(envelope_id, "e1");
        assert!(matches!(event, Some(SlackEvent::Message(m)) if m.content == "hey"));
        let press = json!({"envelope_id": "e2", "type": "interactive", "payload": {
            "type": "block_actions", "user": {"id": "U1"}, "channel": {"id": "D1"},
            "container": {"message_ts": "3.0", "channel_id": "D1"},
            "actions": [{"action_id": "lg:approve:3"}]}});
        assert_eq!(
            parse_frame(&press),
            Frame::Envelope {
                envelope_id: String::from("e2"),
                event: Some(SlackEvent::Press(SlackPress {
                    user_id: String::from("U1"),
                    custom_id: String::from("lg:approve:3"),
                    channel_id: String::from("D1"),
                    message_ts: String::from("3.0"),
                })),
            }
        );
        let other = json!({"envelope_id": "e3", "type": "slash_commands", "payload": {}});
        assert_eq!(
            parse_frame(&other),
            Frame::Envelope {
                envelope_id: String::from("e3"),
                event: None
            },
            "an envelope the daemon ignores is still acknowledged"
        );
        assert_eq!(parse_press(&json!({"type": "view_submission"})), None);
    }

    #[test]
    fn a_reply_cannot_mention_anyone() {
        assert_eq!(
            escape("<!channel> & <@U1>"),
            "&lt;!channel&gt; &amp; &lt;@U1&gt;"
        );
        assert_eq!(unescape(&escape("a<&>b")), "a<&>b");
    }

    #[test]
    fn timestamps_compare_as_numbers() {
        assert!(is_after("10.000001", "9.999999"));
        assert!(is_after("1.000002", "1.000001"));
        assert!(!is_after("1.000001", "1.000001"));
        assert!(is_after("1.0", "x"));
        assert!(!is_after("x", "1.0"));
        assert!(is_after("2", "1.5"));
    }

    #[test]
    fn a_rate_limit_wait_is_read_and_capped() {
        assert_eq!(retry_wait(Some("3")), Duration::from_secs(3));
        assert_eq!(retry_wait(Some("600")), MAX_RETRY_WAIT);
        assert_eq!(retry_wait(Some("soon")), Duration::from_secs(1));
        assert_eq!(retry_wait(None), Duration::from_secs(1));
    }

    #[test]
    fn a_button_has_the_documented_shape() {
        let button = Button {
            custom_id: String::from("lg:deny:1"),
            label: String::from("Deny"),
            style: ButtonStyle::Danger,
        };
        let block = button_json(&button);
        assert_eq!(block["type"], "button");
        assert_eq!(block["action_id"], "lg:deny:1");
        assert_eq!(block["style"], "danger");
        assert_eq!(block["text"]["text"], "Deny");
    }

    #[test]
    fn the_tokens_stay_out_of_debug_output() {
        let api = SlackApi::new(API_BASE, "xoxb-secret", "xapp-secret");
        let debug = format!("{api:?}");
        assert!(!debug.contains("secret"), "{debug}");
    }
}
