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

/// The profile heartbeat carries the flag, grants no action, and never
/// books a tick of its own.
#[test]
fn the_profile_heartbeat_only_lets_a_tick_keep_notes() {
    use autumn_harvest_agent::AgentHeartbeat;
    use autumn_harvest_agent::agent::wake::next_heartbeat;

    let profile = super::super::keep_tick_notes(AgentProfile::new(ASSISTANT_KIND));
    let beat = profile.heartbeat.clone().unwrap();
    assert!(beat.allow_memory_writes, "a tick may keep notes");
    assert!(!beat.allow_actions, "a tick stays read-only");

    // Before the first control, the engine books the profile heartbeat. That
    // booking lies past the last instant of any timer, so it never fires.
    // It is later than every wake that the daemon plans.
    let now = Timestamp::now().as_millisecond();
    let booked = next_heartbeat(None, Some(beat.every_ms()), now, false).unwrap();
    assert!(booked > Timestamp::MAX.as_millisecond());

    // An armed tick keeps the flag and its own interval.
    let armed = crate::wake::heartbeat("tick", Duration::from_mins(30)).narrowed(Some(&profile));
    assert!(armed.allow_memory_writes && !armed.allow_actions);
    assert_eq!(armed.every_secs, 30 * 60);

    // A control without the flag clears it, whatever the profile says.
    let plain = AgentHeartbeat::every(Duration::from_mins(30)).narrowed(Some(&profile));
    assert!(!plain.allow_memory_writes);
    // A profile without the heartbeat clears the flag of an armed tick.
    let bare = crate::wake::heartbeat("tick", Duration::from_secs(60))
        .narrowed(Some(&AgentProfile::new(ASSISTANT_KIND)));
    assert!(!bare.allow_memory_writes);
}

/// Whether some request in `seen[from..]` offered `save_progress`.
fn offered_since(worker: &Worker, from: usize) -> bool {
    worker.seen.lock().unwrap()[from..]
        .iter()
        .any(|(offered, _)| *offered)
}

/// A long brief tick saves its notes, resets, and answers from them.
#[cfg(any(feature = "discord", feature = "sandbox"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_long_tick_saves_its_notes_and_resets() {
    let dir = tempfile::tempdir().unwrap();
    let worker = Arc::new(Worker::default());
    let mut config = capped("[compaction]\nat_percent = 50\nnotes_bytes = 2000");
    config.brief = Some(littleguy_core::BriefConfig {
        schedule: String::from("0 8 * * *"),
        timezone: String::from("UTC"),
        prompt: String::from("dig"),
        to: None,
    });
    let clock = HandClock::at("2026-11-14T07:00:00Z");
    let hand = clock.clone();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::clone(&worker) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap()
    .with_clock(move || hand.now());
    daemon.tick().await.unwrap();
    assert_eq!(
        worker.seen.lock().unwrap().len(),
        0,
        "no tick before the slot"
    );

    clock.set("2026-11-14T08:00:01Z");
    tick_until(&mut daemon, || worker.notices.load(Ordering::SeqCst) > 0).await;
    daemon.tick().await.unwrap();

    let state = daemon.state().await.unwrap().unwrap();
    assert_eq!(state.ticks, 1, "the brief ran as a tick");
    assert_eq!(worker.notices.load(Ordering::SeqCst), 1, "one save turn");
    assert!(offered_since(&worker, 0), "the tick offers save_progress");
    assert_eq!(
        state.notes.map(|notes| notes.text),
        Some(String::from(NOTES))
    );
    assert_eq!(
        state.last_answer.as_deref(),
        Some(format!("From my notes: {NOTES}").as_str())
    );
}

/// A tick armed without the flag gets no reset, so it stops at the cap. The
/// attended op on the same daemon still resets. This is the half-wired
/// patch: a profile heartbeat with no flag on the armed control.
#[cfg(any(feature = "discord", feature = "sandbox"))]
#[tokio::test(flavor = "multi_thread")]
async fn a_tick_without_the_flag_stops_at_the_cap_and_a_person_still_resets() {
    use autumn_harvest_agent::{AgentHeartbeat, AgentStop, HeartbeatSetting};

    let dir = tempfile::tempdir().unwrap();
    let worker = Arc::new(Worker::default());
    let clock = HandClock::at("2026-11-14T07:00:00Z");
    let hand = clock.clone();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &capped("[compaction]\nat_percent = 50\nnotes_bytes = 2000"),
        Arc::clone(&worker) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap()
    .with_clock(move || hand.now());
    daemon.tick().await.unwrap();

    // The heartbeat that `wake::heartbeat` built before ADR 0043 changed.
    let id = daemon.routing.assistant().clone();
    let op = AgentOp::Control(Control::SetHeartbeat {
        setting: HeartbeatSetting::Custom {
            heartbeat: AgentHeartbeat::every(Duration::from_secs(60)).prompt("dig"),
        },
    });
    sqlite::send(&mut daemon.rt, &id, &op, Some("plain-beat")).unwrap();
    daemon.tick().await.unwrap();

    clock.set("2026-11-14T07:01:01Z");
    tick_until(&mut daemon, || worker.rounds.load(Ordering::SeqCst) > 0).await;
    daemon.tick().await.unwrap();
    let state = daemon.state().await.unwrap().unwrap();
    assert_eq!(state.ticks, 1, "the tick ran");
    assert!(
        matches!(state.last_stop, Some(AgentStop::TranscriptFull)),
        "the tick stops at the cap: {:?}",
        state.last_stop
    );
    assert_eq!(worker.notices.load(Ordering::SeqCst), 0, "no save turn");
    assert!(
        !offered_since(&worker, 0),
        "the tick offers no save_progress"
    );
    assert!(state.notes.is_none());

    // Stop the timer, so only the person speaks now.
    let off = crate::wake::off(clock.now());
    sqlite::send(&mut daemon.rt, &id, &off.op, Some(&off.key)).unwrap();
    daemon.tick().await.unwrap();
    let from = worker.seen.lock().unwrap().len();
    let response = daemon.handle_request(UID, request("m-1", "dig")).await;
    assert_eq!(reply_text(&response), format!("From my notes: {NOTES}"));
    assert_eq!(worker.notices.load(Ordering::SeqCst), 1, "one save turn");
    assert!(
        offered_since(&worker, from),
        "the person gets save_progress"
    );
}
