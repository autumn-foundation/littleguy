//! The log of the ticks that ran: heartbeats, briefs, reflections and
//! booked wakes (ADR 0025).
//!
//! The assistant runs every op inside one long-lived run, so its run log
//! shows no tick. The daemon writes one row here for each tick that it saw
//! end: what kind of tick, when it was due, when the daemon saw it end, and
//! how it went. A row holds no message text.

use autumn_harvest_agent::AgentError;
use rusqlite::params;

use super::SqliteStores;

/// The most rows that the log keeps. Older rows go.
pub const MAX_TICKS: i64 = 500;

/// One tick that ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tick {
    /// `heartbeat`, `brief`, `reflection` or `follow-up`.
    pub kind: String,
    /// When it was due, in epoch milliseconds.
    pub due_ms: i64,
    /// When the daemon saw it end, in epoch milliseconds.
    pub seen_ms: i64,
    /// How it went, in a few words.
    pub outcome: String,
}

impl SqliteStores {
    /// Records one tick, and keeps the newest [`MAX_TICKS`].
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn record_tick(&self, tick: &Tick) -> Result<(), AgentError> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO tick_log (kind, due_ms, seen_ms, outcome) VALUES (?1, ?2, ?3, ?4)",
                params![tick.kind, tick.due_ms, tick.seen_ms, tick.outcome],
            )?;
            tx.execute(
                "DELETE FROM tick_log WHERE id <= (SELECT MAX(id) FROM tick_log) - ?1",
                params![MAX_TICKS],
            )?;
            Ok(())
        })
    }

    /// The newest `limit` ticks, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn recent_ticks(&self, limit: u32) -> Result<Vec<Tick>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT kind, due_ms, seen_ms, outcome FROM tick_log ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit], |row| {
                Ok(Tick {
                    kind: row.get(0)?,
                    due_ms: row.get(1)?,
                    seen_ms: row.get(2)?,
                    outcome: row.get(3)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tick(n: i64) -> Tick {
        Tick {
            kind: String::from("heartbeat"),
            due_ms: n,
            seen_ms: n + 1,
            outcome: format!("ran {n}"),
        }
    }

    #[test]
    fn the_newest_ticks_come_first_and_the_log_keeps_its_size() {
        let stores = SqliteStores::open_in_memory().unwrap();
        for n in 0..=MAX_TICKS {
            stores.record_tick(&tick(n)).unwrap();
        }
        let recent = stores.recent_ticks(3).unwrap();
        assert_eq!(recent[0], tick(MAX_TICKS));
        assert_eq!(recent[2], tick(MAX_TICKS - 2));
        let all = stores.recent_ticks(u32::MAX).unwrap();
        assert_eq!(all.len(), usize::try_from(MAX_TICKS).unwrap());
        assert_eq!(all.last().unwrap(), &tick(1), "the oldest row went");
    }
}
