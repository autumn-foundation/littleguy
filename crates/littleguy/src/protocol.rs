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
    /// A link that connects one account of an integration. Only the owner
    /// may.
    Connect {
        /// The integration key from the config, such as `google`.
        integration: String,
    },
    /// List the OAuth connections. Only the owner may.
    Connections,
    /// Delete one OAuth connection. Only the owner may.
    Disconnect {
        /// The integration key.
        integration: String,
        /// The connection id.
        connection: String,
    },
    /// List the calls that wait for an approval. Only the owner may.
    Approvals,
    /// Run the call that waits as request `id`. Only the owner may.
    Approve {
        /// The request number.
        id: i64,
    },
    /// Refuse the call that waits as request `id`. Only the owner may.
    Deny {
        /// The request number.
        id: i64,
        /// Why, for the assistant.
        reason: Option<String>,
    },
}

/// One OAuth connection, with no credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectionEntry {
    /// The integration key.
    pub integration: String,
    /// The connection id.
    pub connection: String,
    /// The provider name.
    pub provider: String,
    /// When the account was connected, in RFC 3339.
    pub created_at: String,
    /// The refresh token stopped working, so the owner must connect again.
    pub refresh_exhausted: bool,
}

/// One call that waits for an approval.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalEntry {
    /// The request number.
    pub id: i64,
    /// The tool.
    pub tool: String,
    /// What the call does, as the approver sees it.
    pub text: String,
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
    /// The link to open in a browser on this machine.
    ConnectLink {
        /// The link. It holds a one-time session token.
        link: String,
    },
    /// The OAuth connections.
    Connections {
        /// Each connection.
        entries: Vec<ConnectionEntry>,
    },
    /// The answer to a disconnect request.
    Disconnected {
        /// Whether the connection existed.
        removed: bool,
    },
    /// The op waits until a person approves a call.
    Waiting {
        /// The request number.
        approval: i64,
    },
    /// A call waits for an approval, so the daemon took no new message.
    Busy {
        /// The request number of the call that waits.
        approval: i64,
    },
    /// The calls that wait for an approval.
    Approvals {
        /// Each call, oldest first.
        entries: Vec<ApprovalEntry>,
    },
    /// A decision went to the assistant. Its answer went to the requester.
    Decided {
        /// The request number.
        approval: i64,
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
            Request::Connect {
                integration: String::from("google"),
            },
            Request::Connections,
            Request::Disconnect {
                integration: String::from("google"),
                connection: String::from("c-1"),
            },
            Request::Approvals,
            Request::Approve { id: 3 },
            Request::Deny {
                id: 3,
                reason: Some(String::from("no")),
            },
        ] {
            let line = serde_json::to_string(&request).unwrap();
            assert_eq!(serde_json::from_str::<Request>(&line).unwrap(), request);
        }
        let chat = serde_json::to_value(Request::Pairs).unwrap();
        assert_eq!(chat, serde_json::json!({"kind": "pairs"}));
    }
}
