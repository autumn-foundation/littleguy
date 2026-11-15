//! The answers that a page started, until the browser opens their stream.
//!
//! A chat goes in two requests. The `POST` sends the message, which htmx
//! guards with the CSRF token, and starts the answer at once. The browser
//! then opens the stream of that answer with a `GET`, which carries no
//! token. So the `GET` only reads an answer that a `POST` started. It never
//! sends a message itself, and each answer streams once.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::daemon::live::Piece;

/// How long an answer waits for its stream. Then it goes, and its chat ends
/// in the history only.
pub const WAIT: Duration = Duration::from_secs(300);

/// Each answer that waits, with the time it started, keyed by a random id.
type Waiting = HashMap<String, (Instant, mpsc::Receiver<Piece>)>;

/// The answers that wait for their stream.
#[derive(Debug, Clone, Default)]
pub struct Streams {
    waiting: Arc<Mutex<Waiting>>,
}

impl Streams {
    fn waiting(&self) -> MutexGuard<'_, Waiting> {
        // A panic while the lock is held leaves a map that is still whole.
        self.waiting.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keeps `pieces` for one stream, and returns its id. Answers that
    /// waited longer than [`WAIT`] go now.
    pub fn put(&self, pieces: mpsc::Receiver<Piece>) -> String {
        let id = super::logins::fresh_token();
        let mut waiting = self.waiting();
        waiting.retain(|_, (since, _)| since.elapsed() < WAIT);
        waiting.insert(id.clone(), (Instant::now(), pieces));
        id
    }

    /// The pieces of `id`, once.
    pub fn take(&self, id: &str) -> Option<mpsc::Receiver<Piece>> {
        self.waiting().remove(id).map(|(_, pieces)| pieces)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answer_streams_once_under_an_id_that_no_one_guesses() {
        let streams = Streams::default();
        let (_tx, rx) = mpsc::channel(1);
        let id = streams.put(rx);
        assert_eq!(id.len(), 64);
        assert!(streams.take("0").is_none());
        assert!(streams.take(&id).is_some());
        assert!(streams.take(&id).is_none());
    }
}
