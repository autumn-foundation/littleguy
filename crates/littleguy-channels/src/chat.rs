//! What every chat service shares: the service names, the errors, a direct
//! message, a button, and the split of a long reply.

use thiserror::Error;

/// One chat service that littleguy talks on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Service {
    /// Discord, over the gateway.
    Discord,
    /// Slack, over Socket Mode.
    Slack,
}

impl Service {
    /// Every service, in a fixed order.
    pub const ALL: [Self; 2] = [Self::Discord, Self::Slack];

    /// The channel id of the service. A sender id starts with it and `:`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Discord => "discord",
            Self::Slack => "slack",
        }
    }

    /// The service of `name`, or `None`.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|service| service.name() == name)
    }

    /// The service and the user id of a sender id such as `slack:U1`.
    #[must_use]
    pub fn split_sender(sender: &str) -> Option<(Self, &str)> {
        let (name, user) = sender.split_once(':')?;
        Self::from_name(name)
            .filter(|_| !user.is_empty())
            .map(|service| (service, user))
    }
}

impl std::fmt::Display for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

/// Why a chat call failed.
#[derive(Debug, Error)]
pub enum ChannelError {
    /// The request did not reach the service, or the answer was lost.
    #[error("the chat request failed: {0}")]
    Transport(String),
    /// The service answered with an error status.
    #[error("the chat service answered HTTP {status}: {body}")]
    Http {
        /// The HTTP status.
        status: u16,
        /// The start of the body.
        body: String,
    },
    /// The answer is not what the API documents.
    #[error("the chat service sent an answer that does not parse: {0}")]
    Decode(String),
    /// Slack answered `ok: false` with this error code.
    #[error("slack answered `{0}`")]
    Api(String),
    /// The service is not on in this daemon.
    #[error("{0} is off in this daemon")]
    Off(Service),
}

/// One direct message to the bot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DmMessage {
    /// The message id: a Discord snowflake, or a Slack `ts`.
    pub id: String,
    /// The DM channel id. A reply goes here.
    pub channel_id: String,
    /// The author's user id.
    pub author_id: String,
    /// The text.
    pub content: String,
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

/// The two numbers of a message id: a Discord snowflake, or the seconds
/// and the microseconds of a Slack `ts`.
pub(crate) fn ts_parts(id: &str) -> Option<(u64, u64)> {
    let (secs, micros) = id.split_once('.').unwrap_or((id, "0"));
    Some((secs.parse().ok()?, micros.parse().ok()?))
}

/// Whether the message id `a` comes after `b`, on Discord or Slack. An id
/// that does not parse comes before every other one, so it never moves a
/// cursor.
#[must_use]
pub fn is_after(a: &str, b: &str) -> bool {
    match (ts_parts(a), ts_parts(b)) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

#[cfg(any(feature = "discord", feature = "slack"))]
/// A reqwest client with a 30 second bound, on the ring provider.
pub(crate) fn http_client() -> reqwest::Client {
    // Reqwest panics without a provider. An `Err` means that one is installed.
    let _ = rustls::crypto::ring::default_provider().install_default();
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

#[cfg(any(feature = "discord", feature = "slack"))]
/// The first and the longest pause between two connects.
pub(crate) const BACKOFF: (std::time::Duration, std::time::Duration) = (
    std::time::Duration::from_secs(1),
    std::time::Duration::from_secs(60),
);

#[cfg(any(feature = "discord", feature = "slack"))]
/// The pause after `current`: twice as long, up to the longest.
pub(crate) fn next_backoff(current: std::time::Duration) -> std::time::Duration {
    (current * 2).min(BACKOFF.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sender_names_its_service() {
        assert_eq!(
            Service::split_sender("slack:U1"),
            Some((Service::Slack, "U1"))
        );
        assert_eq!(
            Service::split_sender("discord:42"),
            Some((Service::Discord, "42"))
        );
        assert_eq!(Service::split_sender("slack:"), None);
        assert_eq!(Service::split_sender("uid:1000"), None);
        assert_eq!(Service::split_sender("plain"), None);
        assert_eq!(Service::Slack.to_string(), "slack");
        assert!(is_after("18446744073709551615", "18446744073709551614"));
        assert!(is_after("2.000001", "2"));
        assert_eq!(
            ChannelError::Off(Service::Slack).to_string(),
            "slack is off in this daemon"
        );
    }
}
