//! The past conversations of the assistant, for the owner (ADR 0039).
//!
//! The runtime database keeps each op until the purge of its run, so the
//! history needs no store of its own (`littleguy_stores::conversations`).
//! The web and the CLI see only their own turns: the ops whose key starts
//! with [`crate::client::MESSAGE_ID_PREFIX`]. A Slack or Discord message
//! shares the transcript, but it is no turn here.
//!
//! The turns of an op come from the state snapshot after it: the
//! transcript from the user message of the op to its end. That holds each
//! text of the assistant and each tool call with its result, as the live
//! chat had them. An op that adds more messages than the window keeps
//! loses its start, and then only the answer shows.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use autumn_harvest_agent::{AgentId, AgentState, ChatMessage, ChatRole, ContentPart};
use littleguy_core::Config;
use littleguy_stores::conversations::{self, Agent, PastOp};

use crate::protocol::{ConversationEntry, PastCall, PastTurn, Response, Speaker};

/// The agent of the history: the assistant, and the turns of the web and
/// the CLI.
fn agent(id: &AgentId) -> Agent<'_> {
    Agent {
        workflow: autumn_harvest_agent::AGENT_WORKFLOW_NAME,
        id: id.as_str(),
        key_prefix: crate::client::MESSAGE_ID_PREFIX,
        snapshot: autumn_harvest::entity::ENTITY_STATE_SIDE_EFFECT,
    }
}

/// The past conversations of `assistant` in the runtime database `db`.
///
/// # Errors
///
/// Returns an error when the file cannot be read.
pub(super) fn listing(config: &Config, db: &Path, assistant: &AgentId) -> Result<Response> {
    let found = conversations::conversations(db, agent(assistant))?;
    Ok(Response::Conversations {
        entries: found
            .conversations
            .into_iter()
            .map(|found| ConversationEntry {
                id: found.id,
                started_ms: found.started_ms,
                last_ms: found.last_ms,
                turns: found.turns,
                first_text: found.first_text,
                live: found.live,
            })
            .collect(),
        keep_days: config.runs.keep_days,
        partial: found.partial,
    })
}

/// The turns of conversation `id`.
///
/// # Errors
///
/// Returns an error when the file cannot be read or a snapshot does not
/// decode.
pub(super) fn conversation(db: &Path, assistant: &AgentId, id: i64) -> Result<Response> {
    let Some(ops) = conversations::conversation(db, agent(assistant), id)? else {
        return Ok(Response::Error {
            message: format!("no past conversation has the id {id}"),
        });
    };
    let mut turns = Vec::new();
    let mut failures = None;
    for op in ops {
        turns.extend(op_turns(&op, &mut failures)?);
    }
    Ok(Response::Conversation { id, turns })
}

/// The turns of one op. `failures` is the failure count of the state
/// before it, when known.
fn op_turns(op: &PastOp, failures: &mut Option<u64>) -> Result<Vec<PastTurn>> {
    let mut turns = vec![PastTurn {
        from: Speaker::Person,
        text: op.text.clone(),
        calls: Vec::new(),
        failed: false,
    }];
    let Some(after) = op.after.as_deref() else {
        return Ok(turns);
    };
    let state: AgentState = serde_json::from_str(after).context("decoding a state snapshot")?;
    let before = failures.replace(state.failures);
    let start = state.transcript.iter().rposition(|message| {
        message.role == ChatRole::User && super::message_text(message) == op.text.trim()
    });
    match start {
        Some(start) => turns.extend(answer_turns(&state.transcript[start + 1..])),
        // A failed op keeps the state before it, so its message is not in
        // the transcript, and the failure count grew.
        None if before.is_some_and(|before| state.failures > before) => turns.push(PastTurn {
            from: Speaker::Assistant,
            text: state
                .last_error
                .unwrap_or_else(|| String::from("this message failed")),
            calls: Vec::new(),
            failed: true,
        }),
        // The window dropped the start of the op. The answer stays.
        None => turns.extend(state.last_answer.map(|text| PastTurn {
            from: Speaker::Assistant,
            text,
            calls: Vec::new(),
            failed: false,
        })),
    }
    Ok(turns)
}

/// The turns of the assistant in `messages`: its text and its tool calls,
/// each call with its result.
fn answer_turns(messages: &[ChatMessage]) -> Vec<PastTurn> {
    let results: HashMap<&str, &str> = messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|part| match part {
            ContentPart::ToolResult {
                tool_call_id,
                content,
            } => Some((tool_call_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();
    messages
        .iter()
        .filter(|message| message.role == ChatRole::Assistant)
        .filter_map(|message| {
            let calls: Vec<PastCall> = message
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::ToolCall {
                        id,
                        name,
                        arguments,
                    } => Some(PastCall {
                        tool: name.clone(),
                        arguments: serde_json::to_string_pretty(arguments)
                            .unwrap_or_else(|_| arguments.to_string()),
                        result: results.get(id.as_str()).map(|text| (*text).to_owned()),
                    }),
                    _ => None,
                })
                .collect();
            let text = super::message_text(message);
            (!text.is_empty() || !calls.is_empty()).then_some(PastTurn {
                from: Speaker::Assistant,
                text,
                calls,
                failed: false,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_turn_folds_each_call_with_its_result() {
        let messages = vec![
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentPart::Text(String::from("let me look")),
                    ContentPart::ToolCall {
                        id: String::from("c1"),
                        name: String::from("web_fetch"),
                        arguments: json!({"url": "https://x.org"}),
                    },
                ],
            },
            ChatMessage {
                role: ChatRole::Tool,
                content: vec![ContentPart::ToolResult {
                    tool_call_id: String::from("c1"),
                    content: String::from("a page"),
                }],
            },
            ChatMessage::text(ChatRole::Assistant, "it says hi"),
        ];
        let turns = answer_turns(&messages);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].text, "let me look");
        assert_eq!(turns[0].calls[0].tool, "web_fetch");
        assert_eq!(turns[0].calls[0].result.as_deref(), Some("a page"));
        assert_eq!(turns[1].text, "it says hi");
        assert_eq!(turns[1].calls, Vec::new());
    }
}
