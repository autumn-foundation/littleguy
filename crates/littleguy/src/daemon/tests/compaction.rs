//! Compaction end to end on the daemon (ADR 0043).
//!
//! A scripted model pads each tool round, so the window grows toward the
//! request cap. With `[compaction]`, the engine asks for a save turn, the
//! model saves its notes, and the window resets to the task and the notes.
//! The model then answers from its notes. Without the section, the same op
//! stops at the cap.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_harvest_agent::context_reset::{RESET_NOTICE, RESTORED_CALL_ID, SAVE_PROGRESS_TOOL};
use autumn_harvest_agent::{
    AgentError, BoxFuture, ChatMessage, ChatRequest, ChatResponse, ChatRole, ContentPart,
    StopReason, TokenUsage,
};

use super::*;

/// The notes that the model saves.
const NOTES: &str = "Found so far: the answer is 42. Next: tell the owner.";

/// The text that pads each tool round.
const PAD: usize = 15_000;

/// A model that works in padded tool rounds until its notes come back.
#[derive(Debug, Default)]
struct Worker {
    rounds: AtomicU64,
    notices: AtomicU64,
    saves: AtomicU64,
    /// Whether each request offered `save_progress`, and whether it showed
    /// the restored notes.
    seen: Mutex<Vec<(bool, bool)>>,
}

fn text_of(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The notes of the restored `save_progress` call in `messages`.
fn restored(messages: &[ChatMessage]) -> Option<String> {
    messages.iter().find_map(|message| {
        message.content.iter().find_map(|part| match part {
            ContentPart::ToolCall { id, arguments, .. } if id == RESTORED_CALL_ID => {
                arguments["notes"].as_str().map(str::to_owned)
            }
            _ => None,
        })
    })
}

impl Worker {
    fn reply(&self, request: &ChatRequest) -> Vec<ContentPart> {
        let offered = request
            .tools
            .iter()
            .any(|tool| tool.name == SAVE_PROGRESS_TOOL);
        let notes = restored(&request.messages);
        self.seen.lock().unwrap().push((offered, notes.is_some()));
        let last = request.messages.last();
        if last.is_some_and(|m| m.role == ChatRole::User && text_of(m) == RESET_NOTICE) {
            self.notices.fetch_add(1, Ordering::SeqCst);
            self.saves.fetch_add(1, Ordering::SeqCst);
            return vec![ContentPart::ToolCall {
                id: String::from("save-1"),
                name: String::from(SAVE_PROGRESS_TOOL),
                arguments: serde_json::json!({ "notes": NOTES }),
            }];
        }
        if let Some(notes) = notes {
            return vec![ContentPart::Text(format!("From my notes: {notes}"))];
        }
        let n = self.rounds.fetch_add(1, Ordering::SeqCst);
        vec![
            ContentPart::Text("x".repeat(PAD)),
            ContentPart::ToolCall {
                id: format!("c{n}"),
                name: String::from("read_file"),
                arguments: serde_json::json!({"path": format!("notes/{n}.md")}),
            },
        ]
    }
}

impl AgentModel for Worker {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            let content = self.reply(request);
            let stop_reason = if content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolCall { .. }))
            {
                StopReason::ToolUse
            } else {
                StopReason::EndTurn
            };
            Ok(ChatResponse {
                content,
                stop_reason,
                usage: TokenUsage::new(1, 1),
            })
        })
    }
}

fn capped(section: &str) -> Config {
    Config::parse(&format!(
        "owner = \"mark\"\nmax_request_bytes = 200000\n{section}"
    ))
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_op_saves_its_notes_resets_and_answers_from_them() {
    let dir = tempfile::tempdir().unwrap();
    let worker = Arc::new(Worker::default());
    let config = capped("[compaction]\nat_percent = 50\nnotes_bytes = 2000");
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::clone(&worker) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap();

    let response = daemon.handle_request(UID, request("m-1", "dig")).await;
    assert_eq!(reply_text(&response), format!("From my notes: {NOTES}"));
    assert_eq!(worker.notices.load(Ordering::SeqCst), 1, "one save turn");
    assert_eq!(worker.saves.load(Ordering::SeqCst), 1);
    assert!(
        worker.rounds.load(Ordering::SeqCst) >= 2,
        "the window grew over several rounds first"
    );
    {
        let seen = worker.seen.lock().unwrap();
        assert!(
            seen.iter().all(|(offered, _)| *offered),
            "each request offers save_progress"
        );
        assert_eq!(
            seen.last(),
            Some(&(true, true)),
            "the last request holds the notes"
        );
    }
    // The agent keeps the notes past the op.
    let state = daemon.state().await.unwrap().unwrap();
    assert_eq!(
        state.notes.map(|notes| notes.text),
        Some(String::from(NOTES))
    );

    // The next message sees the notes again, and needs no reset.
    let next = daemon.handle_request(UID, request("m-2", "and now?")).await;
    assert_eq!(reply_text(&next), format!("From my notes: {NOTES}"));
    assert_eq!(worker.notices.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_compaction_the_same_op_stops_at_the_cap() {
    let dir = tempfile::tempdir().unwrap();
    let worker = Arc::new(Worker::default());
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &capped(""),
        Arc::clone(&worker) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap();

    let response = daemon.handle_request(UID, request("m-1", "dig")).await;
    // The messages name no part of the response, so no reply text reaches
    // the test log.
    let stopped_at_the_cap = matches!(
        &response,
        Response::NoAnswer { reason } if reason.contains("too long")
    );
    assert!(stopped_at_the_cap, "the op stops early at the cap");
    assert_eq!(worker.notices.load(Ordering::SeqCst), 0);
    assert!(
        worker
            .seen
            .lock()
            .unwrap()
            .iter()
            .all(|(offered, _)| !offered),
        "no save_progress without [compaction]"
    );
}
