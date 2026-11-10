//! A fake Discord for tests: a REST server and a gateway on localhost.
//! The [`slack`] module holds a fake Slack.
//!
//! The REST server records each message that the bot sends and serves the
//! history that a test puts in. The gateway says hello, takes the identify,
//! sends `READY`, then sends each event that a test pushes. A test can drop
//! the connection to check that the bot connects again.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Notify, mpsc};
use tokio_tungstenite::tungstenite::Message;

pub mod slack;

pub use slack::FakeSlack;

/// One message that the bot sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    /// The channel it went to.
    pub channel_id: String,
    /// Its text.
    pub content: String,
    /// The custom id of each button under it.
    pub buttons: Vec<String>,
}

/// One answer of the bot to a button press.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Callback {
    /// The interaction it answers.
    pub interaction_id: String,
    /// The callback type: 7 updates the message, 4 answers privately.
    pub kind: u64,
    /// Its text.
    pub content: String,
}

#[derive(Debug, Default)]
struct State {
    sent: Vec<Sent>,
    callbacks: Vec<Callback>,
    history: BTreeMap<String, Vec<Value>>,
    identifies: Vec<Value>,
    connects: usize,
    failing_sends: usize,
}

/// What a test pushes into the gateway.
enum Push {
    Event(String, Value),
    Drop,
}

/// A running fake Discord.
#[derive(Clone)]
pub struct FakeDiscord {
    /// The REST base, for `DiscordRest::new`.
    pub api_base: String,
    /// The gateway URL, for `run_gateway`.
    pub gateway_url: String,
    state: Arc<Mutex<State>>,
    push: mpsc::UnboundedSender<Push>,
    changed: Arc<Notify>,
}

impl std::fmt::Debug for FakeDiscord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeDiscord")
            .field("api_base", &self.api_base)
            .finish_non_exhaustive()
    }
}

fn lock(state: &Mutex<State>) -> std::sync::MutexGuard<'_, State> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A `MESSAGE_CREATE` payload of a direct message.
#[must_use]
pub fn dm_event(id: &str, channel_id: &str, author_id: &str, content: &str) -> Value {
    json!({
        "id": id,
        "channel_id": channel_id,
        "author": {"id": author_id, "username": format!("user{author_id}")},
        "content": content,
    })
}

impl FakeDiscord {
    /// Starts the REST server and the gateway.
    ///
    /// # Errors
    ///
    /// Returns an error when a listener does not bind.
    pub async fn start() -> std::io::Result<Self> {
        let rest = TcpListener::bind("127.0.0.1:0").await?;
        let gateway = TcpListener::bind("127.0.0.1:0").await?;
        let (push, pushed) = mpsc::unbounded_channel();
        let fake = Self {
            api_base: format!("http://{}/api/v10", rest.local_addr()?),
            gateway_url: format!("ws://{}/?v=10&encoding=json", gateway.local_addr()?),
            state: Arc::new(Mutex::new(State::default())),
            push,
            changed: Arc::new(Notify::new()),
        };
        tokio::spawn(serve_rest(rest, fake.clone()));
        tokio::spawn(serve_gateway(gateway, fake.clone(), pushed));
        Ok(fake)
    }

    /// Sends a `MESSAGE_CREATE` of a direct message on the gateway.
    pub fn push_dm(&self, id: &str, channel_id: &str, author_id: &str, content: &str) {
        let event = dm_event(id, channel_id, author_id, content);
        let _ = self
            .push
            .send(Push::Event(String::from("MESSAGE_CREATE"), event));
    }

    /// Sends any dispatch on the gateway.
    pub fn push_event(&self, name: &str, data: Value) {
        let _ = self.push.send(Push::Event(name.to_owned(), data));
    }

    /// Sends an `INTERACTION_CREATE` of a button press by `user_id` on the
    /// gateway. The interaction id is `interaction_id`.
    pub fn push_press(&self, interaction_id: &str, user_id: &str, custom_id: &str) {
        let event = json!({
            "id": interaction_id,
            "token": format!("token-{interaction_id}"),
            "type": 3,
            "channel_id": format!("dm{user_id}"),
            "user": {"id": user_id},
            "data": {"custom_id": custom_id, "component_type": 2},
        });
        let _ = self
            .push
            .send(Push::Event(String::from("INTERACTION_CREATE"), event));
    }

    /// Every answer to a button press, in order.
    #[must_use]
    pub fn callbacks(&self) -> Vec<Callback> {
        lock(&self.state).callbacks.clone()
    }

    /// Drops the gateway connection, as a network fault would.
    pub fn drop_connection(&self) {
        let _ = self.push.send(Push::Drop);
    }

    /// Adds a message to the history of `channel_id`.
    pub fn add_history(&self, event: Value) {
        let channel = event["channel_id"].as_str().unwrap_or_default().to_owned();
        lock(&self.state)
            .history
            .entry(channel)
            .or_default()
            .push(event);
    }

    /// Makes the next `n` sends fail with a 500.
    pub fn fail_sends(&self, n: usize) {
        lock(&self.state).failing_sends = n;
    }

    /// Every message that the bot sent, in order.
    #[must_use]
    pub fn sent(&self) -> Vec<Sent> {
        lock(&self.state).sent.clone()
    }

    /// Every identify payload that the gateway took.
    #[must_use]
    pub fn identifies(&self) -> Vec<Value> {
        lock(&self.state).identifies.clone()
    }

    /// The number of gateway connections so far.
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

async fn serve_rest(listener: TcpListener, fake: FakeDiscord) {
    while let Ok((socket, _)) = listener.accept().await {
        let fake = fake.clone();
        tokio::spawn(async move {
            let _ = answer(socket, &fake).await;
        });
    }
}

/// Reads one HTTP request: its head and its body. `None` when the peer
/// closed first.
pub(crate) async fn read_request(
    socket: &mut TcpStream,
) -> std::io::Result<Option<(String, Vec<u8>)>> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let split = loop {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Ok(None);
        }
        bytes.extend_from_slice(&chunk[..n]);
        if let Some(at) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
            break at;
        }
    };
    let head = String::from_utf8_lossy(&bytes[..split]).into_owned();
    let length: usize = head
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .and_then(|v| v.trim().parse().ok())
        })
        .unwrap_or(0);
    let mut body = bytes[split + 4..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(Some((head, body)))
}

/// Writes one JSON answer with `status`, then closes.
pub(crate) async fn write_json(
    socket: &mut TcpStream,
    status: u16,
    reply: &Value,
    headers: &str,
) -> std::io::Result<()> {
    let text = reply.to_string();
    let response = format!(
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{headers}connection: close\r\n\r\n{text}",
        text.len()
    );
    socket.write_all(response.as_bytes()).await?;
    socket.shutdown().await
}

/// Reads one request and writes one answer, then closes.
async fn answer(mut socket: TcpStream, fake: &FakeDiscord) -> std::io::Result<()> {
    let Some((head, body)) = read_request(&mut socket).await? else {
        return Ok(());
    };
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let line = head.lines().next().unwrap_or_default();
    let mut parts = line.split(' ');
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let authorized = head
        .lines()
        .any(|l| l.to_ascii_lowercase().starts_with("authorization: bot "));
    let interaction = target.contains("/interactions/");
    let (status, reply) = if interaction {
        // The press token authorizes a callback, so it carries no bot token.
        if authorized {
            (400, json!({"message": "a callback takes no bot token"}))
        } else {
            record_callback(&fake.state, target, &body)
        }
    } else if authorized {
        route(fake, method, target, &body)
    } else {
        (401, json!({"message": "401: Unauthorized"}))
    };
    fake.touch();
    write_json(&mut socket, status, &reply, "").await
}

fn route(fake: &FakeDiscord, method: &str, target: &str, body: &Value) -> (u16, Value) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = path.strip_prefix("/api/v10").unwrap_or(path);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match (method, segments.as_slice()) {
        ("POST", ["channels", channel, "messages"]) => record_send(&fake.state, channel, body),
        ("POST", ["users", "@me", "channels"]) => {
            let user = body["recipient_id"].as_str().unwrap_or_default();
            (200, json!({"id": format!("dm{user}")}))
        }
        ("GET", ["channels", channel, "messages"]) => {
            let after = query
                .split('&')
                .find_map(|kv| kv.strip_prefix("after="))
                .unwrap_or("0");
            (
                200,
                Value::Array(history_after(&fake.state, channel, after)),
            )
        }
        _ => (404, json!({"message": "404: Not Found"})),
    }
}

/// Records one sent message, or fails it while failures are queued.
fn record_send(state: &Mutex<State>, channel: &str, body: &Value) -> (u16, Value) {
    let mut state = lock(state);
    if state.failing_sends > 0 {
        state.failing_sends -= 1;
        return (500, json!({"message": "boom"}));
    }
    let buttons = body["components"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|row| row["components"].as_array().into_iter().flatten())
        .filter_map(|button| button["custom_id"].as_str().map(str::to_owned))
        .collect();
    state.sent.push(Sent {
        channel_id: channel.to_owned(),
        content: body["content"].as_str().unwrap_or_default().to_owned(),
        buttons,
    });
    (200, json!({"id": state.sent.len().to_string()}))
}

/// Records one interaction callback. A token that does not match its
/// interaction is refused, as Discord refuses it.
fn record_callback(state: &Mutex<State>, target: &str, body: &Value) -> (u16, Value) {
    let path = target.strip_prefix("/api/v10").unwrap_or(target);
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["interactions", id, token, "callback"] = segments.as_slice() else {
        return (404, json!({"message": "404: Not Found"}));
    };
    if *token != format!("token-{id}") {
        return (401, json!({"message": "Invalid interaction token"}));
    }
    lock(state).callbacks.push(Callback {
        interaction_id: (*id).to_owned(),
        kind: body["type"].as_u64().unwrap_or_default(),
        content: body["data"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
    });
    (204, Value::Null)
}

/// The history of `channel` after `after`, newest first, as Discord sends it.
fn history_after(state: &Mutex<State>, channel: &str, after: &str) -> Vec<Value> {
    let mut newer: Vec<Value> = lock(state)
        .history
        .get(channel)
        .into_iter()
        .flatten()
        .filter(|m| crate::chat::is_after(m["id"].as_str().unwrap_or("0"), after))
        .cloned()
        .collect();
    newer.reverse();
    newer
}

async fn serve_gateway(
    listener: TcpListener,
    fake: FakeDiscord,
    mut pushed: mpsc::UnboundedReceiver<Push>,
) {
    while let Ok((socket, _)) = listener.accept().await {
        let Ok(ws) = tokio_tungstenite::accept_async(socket).await else {
            continue;
        };
        lock(&fake.state).connects += 1;
        fake.touch();
        // One connection at a time, as Discord gives one session.
        let _ = gateway_session(ws, &fake, &mut pushed).await;
    }
}

async fn gateway_session(
    ws: tokio_tungstenite::WebSocketStream<TcpStream>,
    fake: &FakeDiscord,
    pushed: &mut mpsc::UnboundedReceiver<Push>,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let (mut sink, mut stream) = ws.split();
    let hello = json!({"op": 10, "d": {"heartbeat_interval": 45_000}});
    sink.send(Message::Text(hello.to_string().into())).await?;
    let mut seq = 0_u64;
    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { return Ok(()) };
                let Message::Text(text) = frame? else { continue };
                let value: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
                match value["op"].as_u64() {
                    Some(2) => {
                        lock(&fake.state).identifies.push(value["d"].clone());
                        seq += 1;
                        let ready = json!({"op": 0, "t": "READY", "s": seq, "d": {"session_id": "s"}});
                        sink.send(Message::Text(ready.to_string().into())).await?;
                        fake.touch();
                    }
                    Some(1) => {
                        let ack = json!({"op": 11});
                        sink.send(Message::Text(ack.to_string().into())).await?;
                    }
                    _ => {}
                }
            }
            push = pushed.recv() => {
                match push {
                    Some(Push::Event(name, data)) => {
                        seq += 1;
                        let event = json!({"op": 0, "t": name, "s": seq, "d": data});
                        sink.send(Message::Text(event.to_string().into())).await?;
                    }
                    // A drop without a close frame, as a network fault looks.
                    Some(Push::Drop) | None => return Ok(()),
                }
            }
        }
    }
}
