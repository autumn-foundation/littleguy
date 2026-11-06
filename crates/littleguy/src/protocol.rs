//! The wire form between the `lg` commands and `lg serve`.
//!
//! One connection carries one request line and one response line, each a
//! JSON object tagged by `kind`. The socket is local and private, so the
//! protocol has no version field yet. The client and the daemon ship in one
//! binary.

use serde::{Deserialize, Serialize};

/// The largest request line, in bytes. A longer line is refused unread.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// One request to the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Request {
    /// A message for the assistant, from `lg chat`.
    Chat {
        /// The id of the message. A retry with the same id delivers it once.
        message_id: String,
        /// The text.
        text: String,
    },
    /// Redeem a pairing code. Only the owner may.
    Pair {
        /// The code, as the stranger read it out.
        code: String,
    },
    /// List the allowed senders of every channel. Only the owner may.
    Pairs,
    /// Take a paired sender off its channel. Only the owner may.
    Unpair {
        /// The sender id, such as `discord:123`.
        sender: String,
    },
}

/// One allowed sender.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairEntry {
    /// The channel, such as `discord`.
    pub channel: String,
    /// The sender id.
    pub sender: String,
    /// `config` for a sender that the config file allows, `paired` for one
    /// that a pairing code added, `owner` for the local user.
    pub source: String,
}

/// The answer of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// The assistant answered.
    Reply {
        /// The answer text.
        text: String,
    },
    /// The message id was delivered before. Nothing ran again.
    Duplicate {
        /// The last answer of the assistant, if it has one.
        last_answer: Option<String>,
    },
    /// The op ran, but it ended without an answer.
    NoAnswer {
        /// Why, from the agent state.
        reason: String,
    },
    /// The sender is not paired with this daemon.
    Pairing {
        /// The code to give the owner. `None` when too many codes are live.
        code: Option<String>,
    },
    /// The daemon dropped the message.
    Dropped,
    /// A pairing code was redeemed.
    Paired {
        /// The sender that may talk to the assistant now.
        sender: String,
    },
    /// The allowed senders.
    Pairs {
        /// Each allowed sender, by channel.
        entries: Vec<PairEntry>,
    },
    /// The answer to an unpair request.
    Unpaired {
        /// Whether a paired sender was removed.
        removed: bool,
    },
    /// The request was not valid, or the daemon failed.
    Error {
        /// What went wrong.
        message: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_response_carries_its_kind() {
        let json = serde_json::to_value(Response::Reply {
            text: String::from("hi"),
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"kind": "reply", "text": "hi"}));
        let back: Response = serde_json::from_value(json).unwrap();
        assert_eq!(
            back,
            Response::Reply {
                text: String::from("hi")
            }
        );
    }

    #[test]
    fn every_request_round_trips() {
        for request in [
            Request::Chat {
                message_id: String::from("m-1"),
                text: String::from("hello"),
            },
            Request::Pair {
                code: String::from("ABCD-1234"),
            },
            Request::Pairs,
            Request::Unpair {
                sender: String::from("discord:1"),
            },
        ] {
            let line = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        }
        let chat = serde_json::to_value(Request::Pairs).unwrap();
        assert_eq!(chat, serde_json::json!({"kind": "pairs"}));
    }
}
