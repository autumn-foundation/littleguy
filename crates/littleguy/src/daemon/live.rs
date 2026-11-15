//! The text of an answer while the model writes it.
//!
//! The harness sends each piece of model text to a [`LiveSink`]. A drive
//! can run more than the op of a message: a tick that comes due during the
//! op runs right after it, in the same drive. So [`Live`] marks the message
//! of the drive and the run id of its op, and it forwards only the pieces of
//! that run to the one reader that watches the message. Any other piece goes
//! nowhere. Nothing here is durable: the answer that the daemon returns is
//! still the answer that counts.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard, PoisonError};

use autumn_harvest_agent::{LiveEvent, LiveSink};
use tokio::sync::mpsc;

use crate::protocol::Response;

/// How many pieces wait for a slow reader. The harness joins small deltas,
/// so this holds many seconds of text. A piece that does not fit is
/// dropped, and the final answer puts the whole text back.
pub const LIVE_PIECES: usize = 256;

/// One piece of a live answer, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Piece {
    /// A model call starts. The text of an earlier call of the turn goes.
    Restart,
    /// Text that the model wrote.
    Text(String),
    /// The answer of the daemon. It is the last piece.
    Answer(Response),
}

/// The readers of live answers, keyed by message id.
#[derive(Debug, Default)]
pub struct Live {
    taps: Mutex<Taps>,
}

#[derive(Debug, Default)]
struct Taps {
    /// The reader of each watched message.
    readers: HashMap<String, mpsc::Sender<Piece>>,
    /// The message of the drive that runs now.
    current: Option<String>,
    /// Which run is the op of that message.
    run: Run,
}

/// Which run is the op of the current message.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
enum Run {
    /// Not known yet: no piece goes out.
    #[default]
    Unknown,
    /// The first run that writes, when the assistant had no run before.
    First,
    /// This run id.
    Id(String),
}

impl Live {
    fn taps(&self) -> MutexGuard<'_, Taps> {
        // A panic while the lock is held leaves a map that is still whole.
        self.taps.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sends the live pieces of `message_id` to `reader`.
    pub fn watch(&self, message_id: &str, reader: mpsc::Sender<Piece>) {
        self.taps().readers.insert(message_id.to_owned(), reader);
    }

    /// Stops the pieces of `message_id`.
    pub fn forget(&self, message_id: &str) {
        self.taps().readers.remove(message_id);
    }

    /// Marks `message_id` as the message of the drive that starts.
    pub(super) fn begin(&self, message_id: &str) {
        let mut taps = self.taps();
        taps.current = Some(message_id.to_owned());
        taps.run = Run::Unknown;
    }

    /// Names `run_id` as the run of the op of the current message.
    pub(super) fn expect(&self, run_id: &str) {
        self.taps().run = Run::Id(run_id.to_owned());
    }

    /// Takes the first run that writes as the run of the op of the current
    /// message.
    pub(super) fn expect_first(&self) {
        self.taps().run = Run::First;
    }

    /// Ends the drive of the current message.
    pub(super) fn end(&self) {
        let mut taps = self.taps();
        taps.current = None;
        taps.run = Run::Unknown;
    }
}

impl LiveSink for Live {
    fn send(&self, event: &LiveEvent) {
        let (LiveEvent::Start { run_id, .. } | LiveEvent::Delta { run_id, .. }) = event;
        let reader = {
            let mut taps = self.taps();
            if taps.run == Run::First {
                taps.run = Run::Id(run_id.clone());
            }
            let ours = taps.run == Run::Id(run_id.clone());
            taps.current
                .as_ref()
                .filter(|_| ours)
                .and_then(|current| taps.readers.get(current))
                .cloned()
        };
        let Some(reader) = reader else {
            return;
        };
        let piece = match event {
            LiveEvent::Start { .. } => Piece::Restart,
            LiveEvent::Delta { text, .. } => Piece::Text(text.clone()),
        };
        // A sink must not block. A full or closed reader loses the piece.
        let _ = reader.try_send(piece);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delta(text: &str) -> LiveEvent {
        LiveEvent::Delta {
            run_id: String::from("r"),
            step: 0,
            text: text.to_owned(),
        }
    }

    #[test]
    fn only_the_watched_message_of_the_drive_gets_its_pieces() {
        let live = Live::default();
        let (tx, mut rx) = mpsc::channel(8);
        live.watch("m-1", tx);
        live.send(&delta("before the drive"));
        live.begin("m-2");
        live.expect("r");
        live.send(&delta("another message"));
        live.end();
        live.begin("m-1");
        live.send(&delta("before its run is known"));
        live.expect("r");
        live.send(&LiveEvent::Start {
            run_id: String::from("r"),
            step: 0,
        });
        live.send(&delta("hello"));
        // A tick that runs next in the same drive has its own run.
        live.send(&LiveEvent::Start {
            run_id: String::from("tick"),
            step: 0,
        });
        live.send(&LiveEvent::Delta {
            run_id: String::from("tick"),
            step: 0,
            text: String::from("a tick"),
        });
        live.end();
        live.send(&delta("after the drive"));
        assert_eq!(rx.try_recv(), Ok(Piece::Restart));
        assert_eq!(rx.try_recv(), Ok(Piece::Text(String::from("hello"))));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn with_no_run_before_the_first_run_that_writes_is_the_op() {
        let live = Live::default();
        let (tx, mut rx) = mpsc::channel(8);
        live.watch("m-1", tx);
        live.begin("m-1");
        live.expect_first();
        live.send(&delta("first"));
        live.send(&LiveEvent::Delta {
            run_id: String::from("other"),
            step: 0,
            text: String::from("not ours"),
        });
        assert_eq!(rx.try_recv(), Ok(Piece::Text(String::from("first"))));
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn a_full_or_forgotten_reader_loses_pieces_and_the_sink_never_blocks() {
        let live = Live::default();
        let (tx, mut rx) = mpsc::channel(1);
        live.watch("m-1", tx);
        live.begin("m-1");
        live.expect("r");
        live.send(&delta("a"));
        live.send(&delta("b"));
        live.forget("m-1");
        live.send(&delta("c"));
        assert_eq!(rx.try_recv(), Ok(Piece::Text(String::from("a"))));
        // The map held the last sender, so the channel is closed now.
        assert_eq!(rx.try_recv(), Err(mpsc::error::TryRecvError::Disconnected));
    }
}
