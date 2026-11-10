//! A fake Slack for tests: a Web API server and a Socket Mode server on
//! localhost.
//!
//! The Web API records each message that the bot posts and serves the
//! history that a test puts in. It checks the token of each call: the app
//! token opens the socket, and the bot token does the rest. The socket says
//! `hello`, then sends each envelope that a test pushes, and records each
//! acknowledgement.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

use super::{read_request, write_json};

/// The bot token that the fake takes.
pub const BOT_TOKEN: &str = "xoxb-test";

/// The app token that the fake takes.
pub const APP_TOKEN: &str = "xapp-test";

/// One message that the bot posted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Posted {
    /// The channel it went to.
    pub channel: String,
    /// Its text, as Slack got it.
    pub text: String,
    /// The action id of each button under it.
    pub buttons: Vec<String>,
}

/// One `chat.update` or `chat.postEphemeral` of the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// `update` or `ephemeral`.
    pub kind: &'static str,
    /// The channel.
    pub channel: String,
    /// The `ts` of an update, or the user of an ephemeral message.
    pub target: String,
    /// The text.
    pub text: String,
    /// The blocks parameter, as sent.
    pub blocks: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    posted: Vec<Posted>,
    answers: Vec<Answer>,
    history: BTreeMap<String, Vec<Value>>,
    acks: Vec<String>,
    connects: usize,
    failing_posts: usize,
    rate_limited: usize,
    next_envelope: u64,
}

/// What a test pushes into the socket.
enum Push {
    Frame(Value),
    Drop,
}

/// A running fake Slack.
#[derive(Clone)]
pub struct FakeSlack {
    /// The Web API base, for `SlackApi::new`.
    pub api_base: String,
    state: Arc<Mutex<State>>,
    push: mpsc::UnboundedSender<Push>,
    changed: Arc<Notify>,
    socket_url: String,
}

impl std::fmt::Debug for FakeSlack {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeSlack")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A `message` event of a direct message.
#[must_use]
pub fn dm_event(ts: &str, channel: &str, user: &str, text: &str) -> Value {
    json!({"type": "message", "channel": channel, "channel_type": "im", "user": user, "text": text, "ts": ts})
}

impl FakeSlack {
    /// Starts the Web API server and the socket server.
    ///
    /// # Errors
    ///
    /// Returns an error when a listener does not bind.
    pub async fn start() -> std::io::Result<Self> {
        let api = TcpListener::bind("127.0.0.1:0").await?;
        let socket = TcpListener::bind("127.0.0.1:0").await?;
        let (push, pushed) = mpsc::unbounded_channel();
        let fake = Self {
            api_base: format!("http://{}/api", api.local_addr()?),
            socket_url: format!("ws://{}/link/?ticket=t", socket.local_addr()?),
            state: Arc::new(Mutex::new(State::default())),
            push,
            changed: Arc::new(Notify::new()),
        };
        tokio::spawn(serve_api(api, fake.clone()));
        tokio::spawn(serve_socket(socket, fake.clone(), pushed));
        Ok(fake)
    }

    /// Pushes an envelope of `kind` with `payload`. Returns its id.
    #[must_use]
    pub fn push_envelope(&self, kind: &str, payload: &Value) -> String {
        self.envelope(kind, payload)
    }

    fn envelope(&self, kind: &str, payload: &Value) -> String {
        let next = {
            let mut state = lock(&self.state);
            state.next_envelope += 1;
            state.next_envelope
        };
        let id = format!("env-{next}");
        let frame = json!({"envelope_id": id, "type": kind, "payload": payload});
        let _ = self.push.send(Push::Frame(frame));
        id
    }

    /// Pushes a direct message from `user` in `channel`.
    pub fn push_dm(&self, ts: &str, channel: &str, user: &str, text: &str) {
        let payload = json!({"type": "event_callback", "event": dm_event(ts, channel, user, text)});
        self.envelope("events_api", &payload);
    }

    /// Pushes a press of the button `action_id` by `user` on the message
    /// `message_ts` in the DM channel `D<user>`.
    pub fn push_press(&self, user: &str, action_id: &str, message_ts: &str) {
        let channel = format!("D{user}");
        let payload = json!({
            "type": "block_actions",
            "user": {"id": user},
            "channel": {"id": channel},
            "container": {"type": "message", "message_ts": message_ts, "channel_id": channel},
            "actions": [{"action_id": action_id, "type": "button"}],
        });
        self.envelope("interactive", &payload);
    }

    /// Pushes any frame, such as a `disconnect`.
    pub fn push_frame(&self, frame: Value) {
        let _ = self.push.send(Push::Frame(frame));
    }

    /// Drops the socket, as a network fault would.
    pub fn drop_connection(&self) {
        let _ = self.push.send(Push::Drop);
    }

    /// Adds a message to the history of its channel.
    pub fn add_history(&self, event: Value) {
        let channel = event["channel"].as_str().unwrap_or_default().to_owned();
        lock(&self.state)
            .history
            .entry(channel)
            .or_default()
            .push(event);
    }

    /// Makes the next `n` posts fail with a 500.
    pub fn fail_posts(&self, n: usize) {
        lock(&self.state).failing_posts = n;
    }

    /// Makes the next `n` calls answer 429 with `Retry-After: 0`.
    pub fn rate_limit(&self, n: usize) {
        lock(&self.state).rate_limited = n;
    }

    /// Every message that the bot posted, in order.
    #[must_use]
    pub fn posted(&self) -> Vec<Posted> {
        lock(&self.state).posted.clone()
    }

    /// Every update and ephemeral message of the bot, in order.
    #[must_use]
    pub fn answers(&self) -> Vec<Answer> {
        lock(&self.state).answers.clone()
    }

    /// Every envelope id that the bot acknowledged, in order.
    #[must_use]
    pub fn acks(&self) -> Vec<String> {
        lock(&self.state).acks.clone()
    }

    /// The number of socket connections so far.
    #[must_use]
    pub fn connects(&self) -> usize {
        lock(&self.state).connects
    }

    /// Waits until `done` holds, for at most `secs` seconds. Returns
    /// whether it held.
    pub async fn wait_for(&self, secs: u64, done: impl Fn(&Self) -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            if done(self) {
                return true;
            }
            let notified = self.changed.notified();
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return done(self);
            }
        }
    }

    fn touch(&self) {
        self.changed.notify_waiters();
    }
}

/// The value of one `%XX` and `+` encoded form field.
fn decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'+' => out.push(b' '),
            b'%' if at + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[at + 1..at + 3]).unwrap_or("");
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    out.push(byte);
                    at += 2;
                } else {
                    out.push(b'%');
                }
            }
            byte => out.push(byte),
        }
        at += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The fields of a form body.
fn form(body: &[u8]) -> BTreeMap<String, String> {
    String::from_utf8_lossy(body)
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(key), decode(value))
        })
        .collect()
}

async fn serve_api(listener: TcpListener, fake: FakeSlack) {
    while let Ok((socket, _)) = listener.accept().await {
        let fake = fake.clone();
        tokio::spawn(async move {
            let _ = answer(socket, &fake).await;
        });
    }
}

/// Reads one call and writes one answer, then closes.
async fn answer(mut socket: TcpStream, fake: &FakeSlack) -> std::io::Result<()> {
    let Some((head, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };
    let target = head
        .lines()
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .unwrap_or_default();
    let method = target.trim_start_matches("/api/").to_owned();
    let token = head.lines().find_map(|line| {
        let lower = line.to_ascii_lowercase();
        lower
            .starts_with("authorization: bearer ")
            .then(|| line["authorization: bearer ".len()..].trim().to_owned())
    });
    let limited = {
        let mut state = lock(&fake.state);
        let limited = state.rate_limited > 0;
        state.rate_limited = state.rate_limited.saturating_sub(1);
        limited
    };
    if limited {
        let reply = json!({"ok": false, "error": "ratelimited"});
        return write_json(&mut socket, 429, &reply, "retry-after: 0\r\n").await;
    }
    let wanted = if method == "apps.connections.open" {
        APP_TOKEN
    } else {
        BOT_TOKEN
    };
    let (status, reply) = if token.as_deref() == Some(wanted) {
        route(fake, &method, &form(&body))
    } else {
        (200, json!({"ok": false, "error": "invalid_auth"}))
    };
    fake.touch();
    write_json(&mut socket, status, &reply, "").await
}

fn route(fake: &FakeSlack, method: &str, fields: &BTreeMap<String, String>) -> (u16, Value) {
    let field = |name: &str| fields.get(name).cloned().unwrap_or_default();
    match method {
        "apps.connections.open" => (200, json!({"ok": true, "url": fake.socket_url})),
        "conversations.open" => (
            200,
            json!({"ok": true, "channel": {"id": format!("D{}", field("users"))}}),
        ),
        "chat.postMessage" => record_post(&fake.state, fields),
        "chat.update" | "chat.postEphemeral" => {
            let (kind, target) = if method == "chat.update" {
                ("update", field("ts"))
            } else {
                ("ephemeral", field("user"))
            };
            lock(&fake.state).answers.push(Answer {
                kind,
                channel: field("channel"),
                target,
                text: field("text"),
                blocks: fields.get("blocks").cloned(),
            });
            (200, json!({"ok": true}))
        }
        "conversations.history" => {
            let channel = field("channel");
            let oldest = field("oldest");
            let mut newer: Vec<Value> = lock(&fake.state)
                .history
                .get(&channel)
                .into_iter()
                .flatten()
                .filter(|m| crate::chat::is_after(m["ts"].as_str().unwrap_or("0"), &oldest))
                .map(|m| {
                    let mut entry = m.clone();
                    // Slack leaves the channel out of each history entry.
                    if let Some(object) = entry.as_object_mut() {
                        object.remove("channel");
                        object.remove("channel_type");
                    }
                    entry
                })
                .collect();
            newer.reverse();
            (200, json!({"ok": true, "messages": newer}))
        }
        _ => (200, json!({"ok": false, "error": "unknown_method"})),
    }
}

/// Records one posted message, or fails it while failures are queued.
fn record_post(state: &Mutex<State>, fields: &BTreeMap<String, String>) -> (u16, Value) {
    let blocks: Value = fields
        .get("blocks")
        .and_then(|b| serde_json::from_str(b).ok())
        .unwrap_or(Value::Null);
    let buttons = blocks
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "actions")
        .flat_map(|block| block["elements"].as_array().into_iter().flatten())
        .filter_map(|button| button["action_id"].as_str().map(str::to_owned))
        .collect();
    let posted = Posted {
        channel: fields.get("channel").cloned().unwrap_or_default(),
        text: fields.get("text").cloned().unwrap_or_default(),
        buttons,
    };
    let count = {
        let mut state = lock(state);
        if state.failing_posts > 0 {
            state.failing_posts -= 1;
            None
        } else {
            state.posted.push(posted);
            Some(state.posted.len())
        }
    };
    count.map_or_else(
        || (500, json!({"ok": false, "error": "fatal_error"})),
        |count| {
            (
                200,
                json!({"ok": true, "ts": format!("{}.000000", 1_000 + count)}),
            )
        },
    )
}

async fn serve_socket(
    listener: TcpListener,
    fake: FakeSlack,
    mut pushed: mpsc::UnboundedReceiver<Push>,
) {
    while let Ok((socket, _)) = listener.accept().await {
        let Ok(ws) = tokio_tungstenite::accept_async(socket).await else {
            continue;
        };
        lock(&fake.state).connects += 1;
        fake.touch();
        let _ = session(ws, &fake, &mut pushed).await;
    }
}

async fn session(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    fake: &FakeSlack,
    pushed: &mut mpsc::UnboundedReceiver<Push>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let (mut sink, mut stream) = ws.split();
    let hello = json!({"type": "hello", "num_connections": 1});
    sink.send(Message::Text(hello.to_string().into())).await?;
    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { return Ok(()) };
                let Message::Text(text) = frame? else { continue };
                let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                if let Some(id) = value["envelope_id"].as_str() {
                    lock(&fake.state).acks.push(id.to_owned());
                    fake.touch();
                }
            }
            push = pushed.recv() => {
                match push {
                    Some(Push::Frame(frame)) => {
                        let disconnect = frame["type"] == "disconnect";
                        sink.send(Message::Text(frame.to_string().into())).await?;
                        if disconnect {
                            // Slack closes the old connection after a disconnect.
                            let _ = sink.close().await;
                            return Ok(());
                        }
                    }
                    Some(Push::Drop) | None => return Ok(()),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_form_body_decodes() {
        let fields = form(b"a=1&text=hi+there%21&blocks=%5B%5D&bad=%zz&empty=");
        assert_eq!(fields["text"], "hi there!");
        assert_eq!(fields["blocks"], "[]");
        assert_eq!(fields["bad"], "%zz");
        assert_eq!(fields["empty"], "");
        assert_eq!(decode("%"), "%");
        assert_eq!(decode("%4"), "%4");
    }
}
