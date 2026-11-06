//! `ClaudeModel` against a local HTTP server that plays the Messages API.
//!
//! The server answers each connection with the next canned response and
//! records the request. So the tests check the wire, with no key and no
//! network.

use std::error::Error;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex};

use autumn_harvest_agent::{
    AgentModel, ChatMessage, ChatRequest, ChatRole, ContentPart, ErrorKind, StopReason,
};
use littleguy_models::ClaudeModel;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type TestResult<T = ()> = Result<T, Box<dyn Error>>;

/// One canned answer: a status, a content type and a body.
struct Canned {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Canned {
    fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            content_type: "application/json",
            body: body.to_string(),
        }
    }
}

/// One request that the server read.
#[derive(Debug, Clone)]
struct Seen {
    head: String,
    body: Value,
}

impl Seen {
    fn header(&self, name: &str) -> Option<String> {
        let prefix = format!("{name}:");
        self.head.lines().find_map(|line| {
            let lower = line.to_ascii_lowercase();
            lower
                .starts_with(&prefix)
                .then(|| line[prefix.len()..].trim().to_owned())
        })
    }
}

/// Serves `answers` in order, one per connection. Returns the endpoint and
/// the requests that it saw.
async fn serve(answers: Vec<Canned>) -> TestResult<(String, Arc<Mutex<Vec<Seen>>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/messages", listener.local_addr()?);
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    tokio::spawn(async move {
        for answer in answers {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let Ok(request) = read_request(&mut socket).await else {
                return;
            };
            if let Ok(mut log) = log.lock() {
                log.push(request);
            }
            let reply = format!(
                "HTTP/1.1 {} X\r\ncontent-type: {}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                answer.status,
                answer.content_type,
                answer.body.len(),
                answer.body
            );
            let _ = socket.write_all(reply.as_bytes()).await;
            let _ = socket.shutdown().await;
        }
    });
    Ok((endpoint, seen))
}

/// Reads one request with a `content-length` body.
async fn read_request(socket: &mut tokio::net::TcpStream) -> TestResult<Seen> {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let split = loop {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Err("the client closed early".into());
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
                .map(|v| v.trim().to_owned())
        })
        .ok_or("no content-length")?
        .parse()?;
    let mut body = bytes[split + 4..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await?;
        if n == 0 {
            return Err("the body is short".into());
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Ok(Seen {
        head,
        body: serde_json::from_slice(&body)?,
    })
}

fn seen(log: &Mutex<Vec<Seen>>) -> TestResult<Vec<Seen>> {
    Ok(log.lock().map_err(|e| e.to_string())?.clone())
}

fn ask(text: &str) -> ChatRequest {
    ChatRequest {
        messages: vec![
            ChatMessage::text(ChatRole::System, "You are littleguy."),
            ChatMessage::text(ChatRole::User, text),
        ],
        tools: Vec::new(),
        max_tokens: None,
        temperature: None,
        draw: None,
    }
}

fn answer(text: &str) -> Value {
    json!({
        "type": "message",
        "role": "assistant",
        "content": [
            {"type": "thinking", "thinking": "", "signature": "sig-1"},
            {"type": "text", "text": text},
        ],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 10, "output_tokens": 4, "cache_read_input_tokens": 2},
    })
}

#[tokio::test]
async fn a_reply_comes_back_and_its_thinking_block_is_replayed() -> TestResult {
    let (endpoint, log) = serve(vec![
        Canned::json(200, &answer("hello")),
        Canned::json(200, &answer("again")),
    ])
    .await?;
    let model = ClaudeModel::new("sk-test").endpoint(endpoint);
    let first = model.chat(&ask("hi")).await?;
    assert_eq!(first.content, vec![ContentPart::Text("hello".into())]);
    assert_eq!(first.stop_reason, StopReason::EndTurn);
    assert_eq!(first.usage.input_tokens, 12);
    assert_eq!(first.usage.output_tokens, 4);

    let mut next = ask("hi");
    next.messages
        .push(ChatMessage::text(ChatRole::Assistant, "hello"));
    next.messages
        .push(ChatMessage::text(ChatRole::User, "more"));
    model.chat(&next).await?;

    let requests = seen(&log)?;
    assert_eq!(requests.len(), 2);
    let head = &requests[0];
    assert_eq!(head.header("x-api-key").as_deref(), Some("sk-test"));
    assert!(head.header("anthropic-version").is_some());
    assert_eq!(head.body["model"], "claude-opus-5-5");
    assert_eq!(head.body["system"], "You are littleguy.");
    // The second request sends the first turn back with its thinking block.
    let replayed = &requests[1].body["messages"][1];
    assert_eq!(replayed["role"], "assistant");
    assert_eq!(replayed["content"][0]["type"], "thinking");
    assert_eq!(replayed["content"][0]["signature"], "sig-1");
    assert_eq!(replayed["content"][1]["text"], "hello");
    Ok(())
}

#[tokio::test]
async fn an_http_error_maps_to_its_kind() -> TestResult {
    let error = json!({"type": "error", "error": {"type": "x", "message": "slow down"}});
    let (endpoint, _) = serve(vec![
        Canned::json(429, &error),
        Canned::json(401, &error),
        Canned::json(529, &error),
        Canned::json(400, &error),
    ])
    .await?;
    let model = ClaudeModel::new("sk-test").endpoint(endpoint);
    for kind in [
        ErrorKind::RateLimited,
        ErrorKind::Authentication,
        ErrorKind::Unavailable,
        ErrorKind::Provider,
    ] {
        let Err(failure) = model.chat(&ask("hi")).await else {
            return Err("an error status gave a reply".into());
        };
        assert_eq!(failure.kind(), kind);
        assert!(
            failure.message().contains("slow down"),
            "{}",
            failure.message()
        );
    }
    Ok(())
}

#[tokio::test]
async fn a_body_that_is_not_json_is_a_decode_error() -> TestResult {
    let (endpoint, _) = serve(vec![Canned {
        status: 200,
        content_type: "text/plain",
        body: "not json".into(),
    }])
    .await?;
    let model = ClaudeModel::new("sk-test").endpoint(endpoint);
    let Err(failure) = model.chat(&ask("hi")).await else {
        return Err("text gave a reply".into());
    };
    assert_eq!(failure.kind(), ErrorKind::Decode);
    Ok(())
}

#[tokio::test]
async fn no_server_is_a_transport_error() -> TestResult {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}/v1/messages", listener.local_addr()?);
    drop(listener);
    let model = ClaudeModel::new("sk-test").endpoint(endpoint);
    let Err(failure) = model.chat(&ask("hi")).await else {
        return Err("no server gave a reply".into());
    };
    assert_eq!(failure.kind(), ErrorKind::Transport);
    Ok(())
}

#[tokio::test]
async fn a_stream_passes_each_text_delta_and_returns_the_message() -> TestResult {
    let events = [
        json!({"type": "message_start", "message": {"type": "message", "role": "assistant", "content": [], "usage": {"input_tokens": 3}}}),
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "hel"}}),
        json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "lo"}}),
        json!({"type": "content_block_stop", "index": 0}),
        json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 2}}),
        json!({"type": "message_stop"}),
    ];
    let body = events.iter().fold(String::new(), |mut body, event| {
        let _ = write!(body, "event: x\ndata: {event}\n\n");
        body
    });
    let (endpoint, log) = serve(vec![Canned {
        status: 200,
        content_type: "text/event-stream",
        body,
    }])
    .await?;
    let model = ClaudeModel::new("sk-test").endpoint(endpoint);
    let pieces = Arc::new(Mutex::new(String::new()));
    let sink = Arc::clone(&pieces);
    let on_delta = move |piece: &str| {
        if let Ok(mut text) = sink.lock() {
            text.push_str(piece);
        }
    };
    let response = model.chat_stream(&ask("hi"), &on_delta).await?;
    assert_eq!(response.content, vec![ContentPart::Text("hello".into())]);
    assert_eq!(response.usage.output_tokens, 2);
    assert_eq!(pieces.lock().map_err(|e| e.to_string())?.as_str(), "hello");
    assert_eq!(seen(&log)?[0].body["stream"], true);
    Ok(())
}

#[test]
fn the_model_id_names_the_settings() {
    let model = ClaudeModel::new("sk-test");
    assert_eq!(
        model.model_id(),
        Some("claude-opus-5-5+effort=high+fallbacks=default")
    );
    let other = ClaudeModel::new("sk-test").model("claude-sonnet-5-5");
    assert_eq!(other.model_id(), Some("claude-sonnet-5-5"));
}
