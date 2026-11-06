//! The wire form between `lg chat` and `lg serve`.
//!
//! One connection carries one request line and one response line, each a
//! JSON object. The socket is local and private, so the protocol has no
//! version field yet. The client and the daemon ship in one binary.

use serde::{Deserialize, Serialize};

/// The largest request line, in bytes. A longer line is refused unread.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;

/// One message from `lg chat`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The id of the message. A retry with the same id delivers it once.
    pub message_id: String,
    /// The text.
    pub text: String,
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
    Pairing,
    /// The daemon dropped the message.
    Dropped,
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
    fn a_request_round_trips() {
        let request = Request {
            message_id: String::from("m-1"),
            text: String::from("hello"),
        };
        let line = serde_json::to_string(&request).unwrap();
        assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
    }
}
