//! The client side of the socket: `lg chat`, `lg pair`, `lg pairs` and
//! `lg unpair` send one request each and print the answer.

use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::protocol::{ConnectionEntry, PairEntry, Request, Response};

/// A fresh message id: the clock in nanoseconds and the process id.
#[must_use]
pub fn fresh_message_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("cli-{nanos}-{}", std::process::id())
}

/// Sends `request` to the daemon on `socket` and returns its answer.
///
/// # Errors
///
/// Returns an error when no daemon answers, or the answer is not valid.
pub async fn send(socket: &Path, request: &Request) -> Result<Response> {
    let stream = UnixStream::connect(socket).await.with_context(|| {
        format!(
            "no daemon answers on {}. Start one with `lg serve`",
            socket.display()
        )
    })?;
    let (read, mut write) = stream.into_split();
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    write
        .write_all(&line)
        .await
        .context("sending the message")?;
    let mut answer = String::new();
    BufReader::new(read)
        .read_line(&mut answer)
        .await
        .context("reading the answer")?;
    if answer.is_empty() {
        bail!("the daemon closed the connection without an answer");
    }
    serde_json::from_str(&answer).context("the answer is not valid")
}

/// The text that `lg chat` prints, and whether the command succeeded.
#[must_use]
pub fn render(response: &Response) -> (String, bool) {
    match response {
        Response::Reply { text } => (text.clone(), true),
        Response::Duplicate { last_answer } => (
            format!(
                "(already delivered) {}",
                last_answer.as_deref().unwrap_or("no answer yet")
            ),
            true,
        ),
        Response::NoAnswer { reason } => (format!("no answer: {reason}"), false),
        Response::Pairing { .. } => (String::from("this sender is not paired"), false),
        Response::Dropped => (String::from("the daemon dropped the message"), false),
        Response::Paired { sender } => (format!("paired {sender}"), true),
        Response::Pairs { entries } => (render_pairs(entries), true),
        Response::Unpaired { removed: true } => (String::from("unpaired"), true),
        Response::ConnectLink { link } => (
            format!("Open this link in a browser on this machine:\n{link}"),
            true,
        ),
        Response::Connections { entries } => (render_connections(entries), true),
        Response::Disconnected { removed: true } => (String::from("disconnected"), true),
        Response::Disconnected { removed: false } => {
            (String::from("no connection has that id"), false)
        }
        Response::Unpaired { removed: false } => (
            String::from("no paired sender has that id. A sender from the config file stays"),
            false,
        ),
        Response::Error { message } => (format!("error: {message}"), false),
    }
}

/// One line per connection: integration, connection id, provider, date and
/// whether it needs a new connect.
fn render_connections(entries: &[ConnectionEntry]) -> String {
    if entries.is_empty() {
        return String::from("no connections. Run `lg connect <integration>`");
    }
    entries
        .iter()
        .map(|e| {
            let state = if e.refresh_exhausted {
                "connect again"
            } else {
                "ok"
            };
            format!(
                "{}\t{}\t{}\t{}\t{state}",
                e.integration, e.connection, e.provider, e.created_at
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// One line per allowed sender: channel, sender and source.
fn render_pairs(entries: &[PairEntry]) -> String {
    if entries.is_empty() {
        return String::from("no allowed senders");
    }
    entries
        .iter()
        .map(|e| format!("{}\t{}\t{}", e.channel, e.sender, e.source))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_ids_differ_and_are_valid() {
        let a = fresh_message_id();
        let b = fresh_message_id();
        assert_ne!(a, b);
        assert!(littleguy_core::MessageId::new(a).is_ok());
    }

    #[test]
    fn only_an_answer_or_a_duplicate_succeeds() {
        let reply = Response::Reply {
            text: String::from("hi"),
        };
        assert_eq!(render(&reply), (String::from("hi"), true));
        assert!(render(&Response::Duplicate { last_answer: None }).1);
        assert!(!render(&Response::Dropped).1);
        assert!(!render(&Response::Pairing { code: None }).1);
        assert_eq!(
            render(&Response::Paired {
                sender: String::from("discord:1")
            }),
            (String::from("paired discord:1"), true)
        );
        assert!(render(&Response::Unpaired { removed: true }).1);
        assert!(!render(&Response::Unpaired { removed: false }).1);
        assert!(
            !render(&Response::NoAnswer {
                reason: String::from("x")
            })
            .1
        );
        assert!(
            !render(&Response::Error {
                message: String::from("x")
            })
            .1
        );
    }

    #[test]
    fn pairs_print_one_line_each() {
        assert_eq!(render_pairs(&[]), "no allowed senders");
        let entries = vec![
            PairEntry {
                channel: String::from("cli"),
                sender: String::from("uid:1"),
                source: String::from("owner"),
            },
            PairEntry {
                channel: String::from("discord"),
                sender: String::from("discord:2"),
                source: String::from("paired"),
            },
        ];
        assert_eq!(
            render(&Response::Pairs { entries }).0,
            "cli\tuid:1\towner\ndiscord\tdiscord:2\tpaired"
        );
    }

    #[test]
    fn connections_print_one_line_each() {
        assert!(render_connections(&[]).contains("lg connect"));
        let entry = |exhausted| ConnectionEntry {
            integration: String::from("google"),
            connection: String::from("c-1"),
            provider: String::from("google"),
            created_at: String::from("2026-11-08T00:00:00+00:00"),
            refresh_exhausted: exhausted,
        };
        let (text, ok) = render(&Response::Connections {
            entries: vec![entry(false), entry(true)],
        });
        assert!(ok);
        assert_eq!(
            text,
            "google\tc-1\tgoogle\t2026-11-08T00:00:00+00:00\tok\n\
             google\tc-1\tgoogle\t2026-11-08T00:00:00+00:00\tconnect again"
        );
        let (text, ok) = render(&Response::ConnectLink {
            link: String::from("http://x"),
        });
        assert!(ok && text.ends_with("http://x"));
        assert!(render(&Response::Disconnected { removed: true }).1);
        assert!(!render(&Response::Disconnected { removed: false }).1);
    }

    #[tokio::test]
    async fn no_daemon_names_the_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("none.sock");
        let request = Request::Chat {
            message_id: String::from("m"),
            text: String::from("t"),
        };
        let error = send(&socket, &request).await.unwrap_err().to_string();
        assert!(error.contains("none.sock"), "{error}");
    }
}
