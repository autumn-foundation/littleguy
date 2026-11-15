//! The wakes that the assistant books for itself (ADR 0023).
//!
//! A wake says "look at this again at that time". The daemon aims the
//! engine timer at the next pending wake. The tick of a wake starts with a
//! fixed prompt, and the text of the wake stays here, where the assistant
//! reads it with `list_plans`. So a text that the model wrote never becomes
//! a user message.
//!
//! Three bounds keep a loop of wakes small: [`MAX_PENDING`] wakes at once,
//! [`MAX_AHEAD`] into the future, and [`MAX_CHAIN`] booked since the last
//! message of a person.

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{OptionalExtension, params};

use super::SqliteStores;

/// The most wakes that wait at once.
pub const MAX_PENDING: i64 = 20;

/// The most wakes that the assistant books since the last message of a
/// person, as the engine caps a chain of follow-ups.
pub const MAX_CHAIN: i64 = 10;

/// The furthest ahead that a wake may be, in milliseconds: seven days.
pub const MAX_AHEAD_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// The nearest that a wake may be, in milliseconds: one minute, the
/// engine minimum.
pub const MIN_AHEAD_MS: i64 = 60 * 1000;

/// The furthest ahead, for messages.
pub const MAX_AHEAD: &str = "seven days";

/// The longest text of a wake, in characters.
pub const MAX_ABOUT_CHARS: usize = 2_000;

/// One wake that waits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingWake {
    /// Its number.
    pub id: i64,
    /// When it is due, in epoch milliseconds.
    pub at_ms: i64,
    /// What to look at.
    pub about: String,
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

impl SqliteStores {
    /// Books a wake at `at_ms` for the call `key`. Returns its id.
    /// `since_person_ms` is the time of the last message of a person.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for an empty or long text, a time too near or
    /// too far, or a full list or chain. Returns an error when the store
    /// fails.
    pub fn add_wake(
        &self,
        key: &str,
        at_ms: i64,
        about: &str,
        now_ms: i64,
        since_person_ms: i64,
    ) -> Result<i64, AgentError> {
        let about = about.trim();
        if about.is_empty() {
            return Err(refuse("say what to look at"));
        }
        if about.chars().count() > MAX_ABOUT_CHARS {
            return Err(refuse(format!(
                "the text is over {MAX_ABOUT_CHARS} characters"
            )));
        }
        if at_ms < now_ms.saturating_add(MIN_AHEAD_MS) {
            return Err(refuse("a wake is at least one minute ahead"));
        }
        if at_ms > now_ms.saturating_add(MAX_AHEAD_MS) {
            return Err(refuse(format!("a wake is at most {MAX_AHEAD} ahead")));
        }
        self.write(|tx| {
            if let Some(id) = tx
                .query_row("SELECT id FROM wakes WHERE key = ?1", params![key], |row| {
                    row.get(0)
                })
                .optional()?
            {
                return Ok(Ok(id));
            }
            let pending: i64 = tx.query_row(
                "SELECT COUNT(*) FROM wakes WHERE status = 'pending'",
                [],
                |row| row.get(0),
            )?;
            if pending >= MAX_PENDING {
                return Ok(Err(refuse(format!(
                    "{MAX_PENDING} wakes wait already. Cancel one first"
                ))));
            }
            let chained: i64 = tx.query_row(
                "SELECT COUNT(*) FROM wakes WHERE created_ms > ?1",
                params![since_person_ms],
                |row| row.get(0),
            )?;
            if chained >= MAX_CHAIN {
                return Ok(Err(refuse(format!(
                    "you booked {MAX_CHAIN} wakes since the owner last wrote. Wait for them"
                ))));
            }
            tx.execute(
                "INSERT INTO wakes (at_ms, about, created_ms, key) VALUES (?1, ?2, ?3, ?4)",
                params![at_ms, about, now_ms, key],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        })?
    }

    /// Cancels the pending wake `id`. Returns whether it was pending.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn cancel_wake(&self, id: i64) -> Result<bool, AgentError> {
        self.write(|tx| {
            Ok(tx.execute(
                "UPDATE wakes SET status = 'cancelled' WHERE id = ?1 AND status = 'pending'",
                params![id],
            )? == 1)
        })
    }

    /// Marks every pending wake due at or before `until_ms` as done: a tick
    /// ran after it. Returns how many.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn finish_wakes(&self, until_ms: i64) -> Result<usize, AgentError> {
        self.write(|tx| {
            Ok(tx.execute(
                "UPDATE wakes SET status = 'done' WHERE status = 'pending' AND at_ms <= ?1",
                params![until_ms],
            )?)
        })
    }

    /// The pending wakes, the first due first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn pending_wakes(&self) -> Result<Vec<PendingWake>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, at_ms, about FROM wakes WHERE status = 'pending' ORDER BY at_ms, id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(PendingWake {
                    id: row.get(0)?,
                    at_ms: row.get(1)?,
                    about: row.get(2)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: i64 = 60 * 1000;

    fn stores() -> SqliteStores {
        SqliteStores::open_in_memory().unwrap()
    }

    fn refused<T: std::fmt::Debug>(result: Result<T, AgentError>) {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Tool);
    }

    #[test]
    fn a_wake_waits_until_a_tick_runs_after_it() {
        let stores = stores();
        let later = stores
            .add_wake("b", 30 * MINUTE, "check the deploy", 0, 0)
            .unwrap();
        let sooner = stores.add_wake("a", 10 * MINUTE, "check CI", 0, 0).unwrap();
        let ids: Vec<i64> = stores
            .pending_wakes()
            .unwrap()
            .iter()
            .map(|wake| wake.id)
            .collect();
        assert_eq!(ids, [sooner, later], "the first due first");
        assert_eq!(stores.finish_wakes(10 * MINUTE).unwrap(), 1);
        assert_eq!(stores.pending_wakes().unwrap()[0].id, later);
        assert!(stores.cancel_wake(later).unwrap());
        assert!(!stores.cancel_wake(later).unwrap());
        assert_eq!(stores.pending_wakes().unwrap(), []);
    }

    #[test]
    fn a_retried_call_books_one_wake() {
        let stores = stores();
        let first = stores.add_wake("k", 5 * MINUTE, "x", 0, 0).unwrap();
        assert_eq!(stores.add_wake("k", 5 * MINUTE, "x", 0, 0).unwrap(), first);
        assert_eq!(stores.pending_wakes().unwrap().len(), 1);
    }

    #[test]
    fn bad_times_texts_and_full_lists_are_refused() {
        let stores = stores();
        refused(stores.add_wake("a", MINUTE - 1, "x", 0, 0));
        refused(stores.add_wake("b", MAX_AHEAD_MS + 1, "x", 0, 0));
        refused(stores.add_wake("c", 5 * MINUTE, "  ", 0, 0));
        refused(stores.add_wake("d", 5 * MINUTE, &"x".repeat(MAX_ABOUT_CHARS + 1), 0, 0));
        for n in 0..MAX_PENDING {
            // Each comes after a message of a person, so no chain forms.
            stores
                .add_wake(&format!("p{n}"), 5 * MINUTE, "x", n, n)
                .unwrap();
        }
        refused(stores.add_wake("over", 5 * MINUTE, "x", 100, 100));
    }

    #[test]
    fn a_chain_stops_until_a_person_writes() {
        let stores = stores();
        for n in 0..MAX_CHAIN {
            stores
                .add_wake(&format!("c{n}"), (n + 2) * MINUTE, "x", n + 1, 0)
                .unwrap();
            stores.finish_wakes((n + 2) * MINUTE).unwrap();
        }
        refused(stores.add_wake("more", 99 * MINUTE, "x", 50, 0));
        // A message from a person starts a new chain.
        stores.add_wake("more", 99 * MINUTE, "x", 50, 40).unwrap();
    }
}
