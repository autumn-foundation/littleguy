//! The wire form between the `lg` commands and `lg serve`.
//!
//! One connection carries one request line and one response line, each a
//! JSON object tagged by `kind`. The socket is local and private, so the
//! protocol has no version field yet. The client and the daemon ship in one
//! binary.
//!
//! Other programs of the same user read it too, such as a table that runs
//! each character as an `lg serve`. So a new field is optional: an older
//! reader ignores it, and a reply from an older daemon leaves it out.
//!
//! One request has more than one response line: `chat_stream` gets a
//! [`Piece`] line for each piece of text while the model writes, then the
//! response line (issue #67).

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
    /// A message for the assistant, as `chat`, with the text of the answer
    /// while the model writes it. The daemon writes a [`Piece`] line for
    /// each piece, then the response line.
    ChatStream {
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
    /// Ask the model for a profile overlay from the assistant transcript.
    /// Only the owner may.
    RefinePropose,
    /// List the overlay versions of the assistant. Only the owner may.
    Overlays,
    /// Show one overlay version. Only the owner may.
    OverlayShow {
        /// The version.
        version: u64,
    },
    /// Store an overlay as a new version. Only the owner may.
    OverlayPut {
        /// The overlay JSON.
        overlay: String,
    },
    /// Make one version the active one. Only the owner may.
    OverlayActivate {
        /// The version.
        version: u64,
    },
    /// Activate the version that was active before. Only the owner may.
    OverlayRollback,
    /// A one-time link that signs a browser in to the web pages. Only the
    /// owner may.
    UiLink,
    /// Read the config file again, and apply it. Only the owner may.
    Reload,
    /// The text turns of the conversation, oldest first. Only the owner
    /// may.
    History,
    /// The files of the assistant workspace. Only the owner may.
    Workspace,
    /// One file of the assistant workspace. Only the owner may.
    WorkspaceFile {
        /// The path of the file in the workspace.
        path: String,
    },
    /// The goals and the ideas of the assistant. Only the owner may.
    Plans,
    /// The skills of the skills folder and the drafts of the assistant.
    /// Only the owner may.
    Skills,
    /// Write the draft `draft` as a `SKILL.md`. Only the owner may.
    AcceptSkill {
        /// The draft.
        draft: i64,
    },
    /// Decline the draft `draft`. Only the owner may.
    DeclineSkill {
        /// The draft.
        draft: i64,
    },
    /// Move the skill `name` to the retired folder. Only the owner may.
    RetireSkill {
        /// The skill.
        name: String,
    },
    /// Accept a proposed goal, or decline it, drop an active goal, or drop
    /// an open idea. Only the owner may.
    Decide {
        /// What to do.
        decision: PlanDecision,
        /// The goal or the idea.
        id: i64,
    },
}

/// One skill of the skills folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// The name that the model loads it by.
    pub name: String,
    /// When to load it.
    pub description: String,
    /// The know-how, in markdown.
    pub body: String,
    /// Who wrote it, when the file says.
    pub author: Option<String>,
    /// The `SKILL.md` file.
    pub path: String,
}

/// A `SKILL.md` that does not load, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidSkill {
    /// The file.
    pub path: String,
    /// Why it does not load.
    pub reason: String,
}

/// A skill that the assistant drafted, which waits for the owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillDraftEntry {
    /// Its number.
    pub id: i64,
    /// The name that the model would load it by.
    pub name: String,
    /// When to load it.
    pub description: String,
    /// The know-how, in markdown.
    pub body: String,
    /// When it was drafted, in epoch milliseconds.
    pub created_ms: i64,
    /// Whether a skill of that name exists, so that accepting replaces it.
    pub revises: bool,
}

/// What the owner decides about a goal or an idea.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanDecision {
    /// Accept a proposed goal.
    Accept,
    /// Decline a proposed goal.
    Decline,
    /// Drop an active goal.
    DropGoal,
    /// Drop an open idea.
    DropIdea,
}

/// One note of a goal or an idea.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanNote {
    /// When, in epoch milliseconds.
    pub at_ms: i64,
    /// The text.
    pub text: String,
}

/// One goal that waits for the owner or is active.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoalEntry {
    /// Its number.
    pub id: i64,
    /// `proposed` or `active`.
    pub status: String,
    /// A short name.
    pub title: String,
    /// Why it matters.
    pub why: String,
    /// How often it is checked, in minutes.
    pub check_every_minutes: Option<i64>,
    /// When the next check is due, in epoch milliseconds.
    pub next_check_ms: Option<i64>,
    /// The idea that it came from.
    pub idea: Option<i64>,
    /// The newest notes, oldest first.
    pub notes: Vec<PlanNote>,
}

/// One open idea.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdeaEntry {
    /// Its number.
    pub id: i64,
    /// A short name.
    pub title: String,
    /// What it is.
    pub body: String,
    /// When it was filed, in epoch milliseconds.
    pub created_ms: i64,
    /// The newest research notes, oldest first.
    pub notes: Vec<PlanNote>,
}

/// One file of the assistant workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceEntry {
    /// The path in the workspace.
    pub path: String,
    /// The size, in bytes.
    pub bytes: u64,
}

/// Who wrote one turn of the conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Speaker {
    /// A person, on any channel.
    Person,
    /// The assistant.
    Assistant,
    /// The assistant on its own: the answer of a brief or a heartbeat.
    Unprompted,
}

/// One text turn of the conversation. Tool calls and tool results are not
/// turns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Turn {
    /// Who wrote it.
    pub from: Speaker,
    /// The text.
    pub text: String,
}

/// One overlay version of the assistant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayEntry {
    /// The version.
    pub version: u64,
    /// Who wrote it, such as `operator:lg-refine`.
    pub author: String,
    /// When it was stored, in RFC 3339.
    pub created_at: String,
    /// Whether the assistant runs on it.
    pub active: bool,
    /// The parts that it sets, such as `guardrails`.
    pub parts: Vec<String>,
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

/// A piece of an answer while the model writes it: a line before the
/// response line of a `chat_stream`. The pieces are a preview. The response
/// line is the answer that counts, and a piece that a slow reader misses is
/// lost.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Piece {
    /// A model call starts. The text of an earlier call of the turn goes.
    Restart,
    /// Text that the model wrote.
    Delta {
        /// The text.
        text: String,
    },
}

/// What one op spent: the model calls that its drive recorded in the cost
/// ledger.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// The model calls.
    pub calls: u64,
    /// The input tokens.
    pub input_tokens: u64,
    /// The output tokens.
    pub output_tokens: u64,
    /// The cost of the priced calls, in millionths of a US dollar.
    pub cost_usd_micros: u64,
    /// The calls with no price. When it is not zero, the cost is a floor.
    pub unpriced_calls: u64,
}

/// The answer of the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Response {
    /// The assistant answered.
    Reply {
        /// The answer text.
        text: String,
        /// What the op spent, from the cost ledger. A daemon that does not
        /// report it, and a reply after an approval, leave it out.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        usage: Option<Usage>,
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
    /// The proposer wrote an overlay. It waits for `lg refine activate`.
    Proposed {
        /// The stored version.
        version: u64,
        /// The overlay JSON.
        overlay: String,
    },
    /// The proposer kept the active overlay.
    Kept,
    /// The overlay versions.
    Overlays {
        /// The active version, if any.
        active: Option<u64>,
        /// Each version, oldest first.
        entries: Vec<OverlayEntry>,
    },
    /// One overlay version.
    Overlay {
        /// The version.
        entry: OverlayEntry,
        /// The overlay JSON.
        overlay: String,
    },
    /// An overlay was stored. It is not active.
    OverlayStored {
        /// The version.
        version: u64,
    },
    /// The assistant runs on `version` from its next op.
    OverlayActive {
        /// The active version.
        version: u64,
        /// The version that was active before, if any.
        previous: Option<u64>,
    },
    /// The daemon read its config file again.
    Reloaded {
        /// The sections that changed and apply now.
        applied: Vec<String>,
        /// The sections that changed and apply at the next start.
        restart: Vec<String>,
        /// The sections that changed and shape the agent profile. The
        /// running assistant keeps its profile (autumn-harvest #2658).
        #[serde(default)]
        profile: Vec<String>,
    },
    /// The text turns of the conversation, oldest first.
    History {
        /// The turns.
        turns: Vec<Turn>,
    },
    /// The files of the assistant workspace, by path.
    Workspace {
        /// The files.
        files: Vec<WorkspaceEntry>,
    },
    /// One file of the assistant workspace.
    WorkspaceFile {
        /// The path in the workspace.
        path: String,
        /// The size, in bytes.
        bytes: u64,
        /// The text. `None` when the file is not UTF-8 text, or is too
        /// large to show.
        text: Option<String>,
    },
    /// The goals and the ideas of the assistant.
    Plans {
        /// The active goals, then the proposed ones.
        goals: Vec<GoalEntry>,
        /// The open ideas, oldest first.
        ideas: Vec<IdeaEntry>,
    },
    /// The skills of the skills folder, the files that do not load, and
    /// the drafts that wait.
    Skills {
        /// The skills that load, by name.
        skills: Vec<SkillEntry>,
        /// The files that do not load.
        invalid: Vec<InvalidSkill>,
        /// The drafts of the assistant, oldest first.
        drafts: Vec<SkillDraftEntry>,
    },
    /// What a listing asked for is not set up in this daemon: a section is
    /// missing, or this build lacks the feature. It is not a failure. The
    /// message says how to turn it on.
    Off {
        /// What is off, and how to turn it on.
        message: String,
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
            usage: None,
        })
        .unwrap();
        assert_eq!(json, serde_json::json!({"kind": "reply", "text": "hi"}));
        let back: Response = serde_json::from_value(json).unwrap();
        assert_eq!(
            back,
            Response::Reply {
                text: String::from("hi"),
                usage: None,
            }
        );
    }

    #[test]
    fn a_piece_carries_its_kind() {
        assert_eq!(
            serde_json::to_value(Piece::Delta {
                text: String::from("I try")
            })
            .unwrap(),
            serde_json::json!({"kind": "delta", "text": "I try"})
        );
        assert_eq!(
            serde_json::to_value(Piece::Restart).unwrap(),
            serde_json::json!({"kind": "restart"})
        );
        let request: Request = serde_json::from_value(serde_json::json!({
            "kind": "chat_stream", "message_id": "m-1", "text": "hello",
        }))
        .unwrap();
        assert!(matches!(request, Request::ChatStream { .. }));
    }

    #[test]
    fn a_reply_carries_its_usage_on_the_wire() {
        let usage = Usage {
            calls: 2,
            input_tokens: 1830,
            output_tokens: 212,
            cost_usd_micros: 4100,
            unpriced_calls: 0,
        };
        let json = serde_json::to_value(Response::Reply {
            text: String::from("hi"),
            usage: Some(usage),
        })
        .unwrap();
        assert_eq!(
            json,
            serde_json::json!({"kind": "reply", "text": "hi", "usage": {
                "calls": 2, "input_tokens": 1830, "output_tokens": 212,
                "cost_usd_micros": 4100, "unpriced_calls": 0,
            }})
        );
        let back: Response = serde_json::from_value(json).unwrap();
        assert!(matches!(back, Response::Reply { usage: Some(read), .. } if read == usage));
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
            Request::Reload,
            Request::History,
            Request::Skills,
            Request::AcceptSkill { draft: 2 },
            Request::DeclineSkill { draft: 3 },
            Request::RetireSkill {
                name: String::from("deploy"),
            },
            Request::Plans,
            Request::Decide {
                decision: PlanDecision::DropIdea,
                id: 4,
            },
            Request::Workspace,
            Request::WorkspaceFile {
                path: String::from("notes/a.md"),
            },
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
