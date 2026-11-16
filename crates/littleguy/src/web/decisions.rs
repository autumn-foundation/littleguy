//! The approvals that a page decided, until their run answers.
//!
//! A decision drives the run on: the code runs, and the model takes its next
//! turn. That takes seconds. So the `POST` of a button only starts the
//! decision and answers at once. The page then asks for the result with a
//! `GET`, which only reads a decision that a `POST` started. The `GET` never
//! decides anything itself, and each result is read once.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::protocol::Response;

/// How long a result waits for its page. Then it goes, and the answer is in
/// the history only.
pub const KEEP: Duration = Duration::from_secs(300);

/// Where one decision is.
#[derive(Debug)]
enum Slot {
    /// The run still goes on.
    Running,
    /// The run answered.
    Done(Response),
}

/// What a page reads about one decision.
#[derive(Debug, PartialEq, Eq)]
pub enum Poll {
    /// No page started this decision, or its result was read.
    Unknown,
    /// The run still goes on.
    Running,
    /// The answer of the run. It is read once.
    Done(Response),
}

/// Each decision with the time that it started, by approval number.
type Slots = HashMap<i64, (Instant, Slot)>;

/// The decisions that pages started.
#[derive(Debug, Clone, Default)]
pub struct Decisions {
    slots: Arc<Mutex<Slots>>,
}

impl Decisions {
    fn slots(&self) -> MutexGuard<'_, Slots> {
        // A panic while the lock is held leaves a map that is still whole.
        self.slots.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Starts the decision on approval `id`: `decide` sends it and drives
    /// the run. A second press while the first still runs starts nothing.
    /// Results older than [`KEEP`] go now.
    pub fn start<F>(&self, id: i64, decide: F)
    where
        F: Future<Output = Response> + Send + 'static,
    {
        {
            let mut slots = self.slots();
            slots.retain(|_, (started, _)| started.elapsed() < KEEP);
            if matches!(slots.get(&id), Some((_, Slot::Running))) {
                return;
            }
            slots.insert(id, (Instant::now(), Slot::Running));
        }
        let decisions = self.clone();
        tokio::spawn(async move {
            let answer = decide.await;
            decisions
                .slots()
                .insert(id, (Instant::now(), Slot::Done(answer)));
        });
    }

    /// Where the decision on approval `id` is. A result is read once.
    #[must_use]
    pub fn take(&self, id: i64) -> Poll {
        let mut slots = self.slots();
        match slots.get(&id) {
            None => Poll::Unknown,
            Some((_, Slot::Running)) => Poll::Running,
            Some((_, Slot::Done(_))) => match slots.remove(&id) {
                Some((_, Slot::Done(answer))) => Poll::Done(answer),
                _ => Poll::Unknown,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn a_decision_runs_on_while_the_page_has_its_answer() {
        let decisions = Decisions::default();
        assert_eq!(decisions.take(1), Poll::Unknown);
        let (finish, finished) = oneshot::channel::<()>();
        decisions.start(1, async move {
            let _ = finished.await;
            Response::Decided { approval: 1 }
        });
        assert_eq!(decisions.take(1), Poll::Running, "the press did not wait");
        decisions.start(1, async { Response::Decided { approval: 99 } });
        finish.send(()).unwrap();
        let answer = loop {
            match decisions.take(1) {
                Poll::Running => tokio::task::yield_now().await,
                other => break other,
            }
        };
        assert_eq!(
            answer,
            Poll::Done(Response::Decided { approval: 1 }),
            "a second press while the first runs starts nothing"
        );
        assert_eq!(decisions.take(1), Poll::Unknown, "a result is read once");
    }
}
