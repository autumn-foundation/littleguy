//! The past conversations of an agent, read from the runtime database.
//!
//! The engine keeps each op as a signal row of its run, and the row stays
//! until the purge of the run. So the history needs no store of its own.
//!
//! - **The boundary.** A conversation is the stretch of op signals between
//!   two clears of the transcript, in `signal_seq` order. A clear is the op
//!   whose payload has `op.action = "clear_transcript"`.
//! - **The turns.** A turn is a user message whose op key starts with the
//!   prefix that the caller names. Messages of another source, such as a
//!   chat service, set no boundary of their own and count as no turn.
//! - **The id.** The id of a conversation is the `signal_seq` of its first
//!   turn. It never changes, and no later row reuses it.
//!
//! Each read opens the file read only, as the run log does.

use std::path::Path;
use std::time::Duration;

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// How long a read waits for a lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// The name of the signal that carries an entity op.
const OP_SIGNAL: &str = "harvest.entity.op";

/// One past conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conversation {
    /// The `signal_seq` of its first turn.
    pub id: i64,
    /// When its first turn arrived, in epoch milliseconds.
    pub started_ms: i64,
    /// When its last turn arrived, in epoch milliseconds.
    pub last_ms: i64,
    /// Its turns.
    pub turns: i64,
    /// The text of its first turn.
    pub first_text: String,
    /// Whether it is the conversation that goes on now: no clear follows it.
    pub live: bool,
}

/// The conversations of an agent, newest first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// The conversations.
    pub conversations: Vec<Conversation>,
    /// Whether the oldest one began before the oldest run that the file
    /// keeps, so its first turns are gone.
    pub partial: bool,
}

/// One turn of a past conversation, and the state after its op.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastOp {
    /// The `signal_seq` of the turn.
    pub seq: i64,
    /// The text of the user message.
    pub text: String,
    /// The JSON of the state that the engine recorded after the op, when
    /// the history holds it.
    pub after: Option<String>,
}

/// Which agent to read: its workflow, its id and the prefix of the op keys
/// that count as turns.
#[derive(Debug, Clone, Copy)]
pub struct Agent<'a> {
    /// The workflow name, such as `agent`.
    pub workflow: &'a str,
    /// The workflow id of the agent.
    pub id: &'a str,
    /// The prefix of the op keys of the turns to show.
    pub key_prefix: &'a str,
    /// The name of the state snapshot side effect.
    pub snapshot: &'a str,
}

/// A read error as a store error.
fn read_error(error: &rusqlite::Error) -> AgentError {
    AgentError::new(
        ErrorKind::Tool,
        format!("the runtime database cannot be read: {error}"),
    )
}

/// A connection to `db` that cannot write.
fn open(db: &Path) -> Result<Connection, AgentError> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn = Connection::open_with_flags(db, flags).map_err(|e| read_error(&e))?;
    conn.busy_timeout(BUSY_TIMEOUT)
        .map_err(|e| read_error(&e))?;
    conn.pragma_update(None, "query_only", true)
        .map_err(|e| read_error(&e))?;
    Ok(conn)
}

/// The `LIKE` pattern of the keys that start with `prefix`.
fn like(prefix: &str) -> String {
    let escaped = prefix
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("{escaped}%")
}

/// The op signals of `agent`, each with its part: the number of clears
/// before it. `{turn}` is 1 for a turn of the prefix.
const MARKED: &str = "\
    WITH ops AS ( \
      SELECT s.signal_seq, s.received_at, s.payload_json, \
             (json_extract(s.payload_json, '$.op.type') = 'user_message' \
              AND s.idempotency_key LIKE ?3 ESCAPE '\\') AS turn, \
             coalesce(json_extract(s.payload_json, '$.op.action'), '') = 'clear_transcript' \
               AS clear \
      FROM harvest_signals AS s \
      JOIN harvest_executions AS e ON e.exec_id = s.exec_id \
      WHERE s.name = ?4 AND e.workflow_name = ?1 AND e.workflow_id = ?2 \
    ), \
    marked AS ( \
      SELECT *, sum(clear) OVER (ORDER BY signal_seq) AS part FROM ops \
    )";

/// The conversations of `agent` in `db`, newest first. A file with no run
/// has none.
///
/// # Errors
///
/// Returns a `Tool` error when the file cannot be read.
pub fn conversations(db: &Path, agent: Agent<'_>) -> Result<Listing, AgentError> {
    if !db.exists() {
        return Ok(Listing {
            conversations: Vec::new(),
            partial: false,
        });
    }
    let conn = open(db)?;
    let sql = format!(
        "{MARKED}, \
         parts AS ( \
           SELECT part, \
                  min(signal_seq) FILTER (WHERE turn) AS id, \
                  min(received_at) FILTER (WHERE turn) AS started_ms, \
                  max(received_at) FILTER (WHERE turn) AS last_ms, \
                  count(*) FILTER (WHERE turn) AS turns \
           FROM marked GROUP BY part \
         ) \
         SELECT p.id, p.started_ms, p.last_ms, p.turns, \
                coalesce(json_extract(s.payload_json, '$.op.text'), ''), \
                p.part = (SELECT max(part) FROM marked) \
         FROM parts AS p JOIN harvest_signals AS s ON s.signal_seq = p.id \
         WHERE p.turns > 0 \
         ORDER BY p.id DESC"
    );
    let mut statement = conn.prepare(&sql).map_err(|e| read_error(&e))?;
    let rows = statement
        .query_map(
            params![agent.workflow, agent.id, like(agent.key_prefix), OP_SIGNAL],
            |row| {
                Ok(Conversation {
                    id: row.get(0)?,
                    started_ms: row.get(1)?,
                    last_ms: row.get(2)?,
                    turns: row.get(3)?,
                    first_text: row.get(4)?,
                    live: row.get(5)?,
                })
            },
        )
        .map_err(|e| read_error(&e))?;
    let conversations = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| read_error(&e))?;
    // The oldest run that the file keeps started with turns that it does
    // not hold: a purge took the runs before it.
    let partial: bool = conn
        .query_row(
            "SELECT coalesce(json_array_length(input_json, '$.state.transcript'), 0) > 0 \
             FROM harvest_executions WHERE workflow_name = ?1 AND workflow_id = ?2 \
             ORDER BY rowid LIMIT 1",
            params![agent.workflow, agent.id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| read_error(&e))?
        .unwrap_or(false);
    Ok(Listing {
        conversations,
        partial,
    })
}

/// The turns of conversation `id` of `agent` in `db`, oldest first, each
/// with the state after its op. `None` when no conversation has the id.
///
/// The state after an op is the first snapshot in its run after the
/// signal of the op arrived. The snapshot that opens a run does not count:
/// a signal that starts a run arrives before it, and it holds the state
/// before the op. An op that a run carried to the next one in its
/// checkpoint has no signal event there, so it shows with no state.
///
/// # Errors
///
/// Returns a `Tool` error when the file cannot be read.
pub fn conversation(
    db: &Path,
    agent: Agent<'_>,
    id: i64,
) -> Result<Option<Vec<PastOp>>, AgentError> {
    if !db.exists() {
        return Ok(None);
    }
    let conn = open(db)?;
    // The id must be the first turn of its part.
    let sql = format!(
        "{MARKED} \
         SELECT signal_seq FROM marked \
         WHERE turn AND part = (SELECT part FROM marked WHERE signal_seq = ?5) \
         ORDER BY signal_seq LIMIT 1"
    );
    let first: Option<i64> = conn
        .query_row(
            &sql,
            params![
                agent.workflow,
                agent.id,
                like(agent.key_prefix),
                OP_SIGNAL,
                id
            ],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| read_error(&e))?;
    if first != Some(id) {
        return Ok(None);
    }
    let sql = format!(
        "{MARKED} \
         SELECT m.signal_seq, coalesce(json_extract(m.payload_json, '$.op.text'), ''), \
                (SELECT json_extract(after.event_json, '$.data.value.state') \
                 FROM harvest_events AS got \
                 JOIN harvest_events AS after \
                   ON after.exec_id = got.exec_id AND after.seq > got.seq \
                 WHERE got.exec_id = s.exec_id \
                   AND json_extract(got.event_json, '$.type') = 'SignalReceived' \
                   AND json_extract(got.event_json, '$.data.payload.op.message_id') \
                       = s.idempotency_key \
                   AND json_extract(after.event_json, '$.type') = 'SideEffectRecorded' \
                   AND json_extract(after.event_json, '$.data.name') = ?6 \
                   AND after.seq > (SELECT min(opening.seq) FROM harvest_events AS opening \
                     WHERE opening.exec_id = got.exec_id \
                       AND json_extract(opening.event_json, '$.type') = 'SideEffectRecorded' \
                       AND json_extract(opening.event_json, '$.data.name') = ?6) \
                 ORDER BY after.seq LIMIT 1) \
         FROM marked AS m JOIN harvest_signals AS s ON s.signal_seq = m.signal_seq \
         WHERE m.turn AND m.part = (SELECT part FROM marked WHERE signal_seq = ?5) \
         ORDER BY m.signal_seq"
    );
    let mut statement = conn.prepare(&sql).map_err(|e| read_error(&e))?;
    let rows = statement
        .query_map(
            params![
                agent.workflow,
                agent.id,
                like(agent.key_prefix),
                OP_SIGNAL,
                id,
                agent.snapshot
            ],
            |row| {
                Ok(PastOp {
                    seq: row.get(0)?,
                    text: row.get(1)?,
                    after: row.get(2)?,
                })
            },
        )
        .map_err(|e| read_error(&e))?;
    let ops = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| read_error(&e))?;
    Ok(Some(ops))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const AGENT: Agent<'static> = Agent {
        workflow: "agent",
        id: "assistant/mark",
        key_prefix: "cli-",
        snapshot: "harvest.entity.state",
    };

    fn database(dir: &Path) -> (PathBuf, Connection) {
        let db = dir.join("lg.db");
        drop(autumn_harvest_sqlite::SqliteRuntime::open(&db).unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO harvest_executions (exec_id, workflow_name, workflow_id, input_json, state) \
             VALUES ('r1', 'agent', 'assistant/mark', '{}', 'RUNNING')",
            [],
        )
        .unwrap();
        (db, conn)
    }

    /// Adds an op signal with `key` and `op`, at `at` ms.
    fn op(conn: &Connection, key: &str, op: &serde_json::Value, at: i64) {
        let payload = serde_json::json!({"kind": "op", "op": op});
        conn.execute(
            "INSERT INTO harvest_signals \
             (exec_id, name, payload_json, delivered, received_at, idempotency_key) \
             VALUES ('r1', 'harvest.entity.op', ?1, 1, ?2, ?3)",
            params![payload.to_string(), at, key],
        )
        .unwrap();
    }

    fn say(conn: &Connection, key: &str, text: &str, at: i64) {
        op(
            conn,
            key,
            &serde_json::json!({"type": "user_message", "text": text, "message_id": key}),
            at,
        );
    }

    fn clear(conn: &Connection, key: &str, at: i64) {
        op(
            conn,
            key,
            &serde_json::json!({"type": "control", "action": "clear_transcript"}),
            at,
        );
    }

    #[test]
    fn a_clear_splits_the_turns_and_other_sources_count_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let (db, conn) = database(dir.path());
        // A control op that is no clear must not split, also before any
        // clear (a sum over no clear is NULL, not 0).
        op(
            &conn,
            "heartbeat:1",
            &serde_json::json!({"type": "control", "action": "set_heartbeat"}),
            5,
        );
        say(&conn, "cli-1", "first", 10);
        say(&conn, "slack:D1:1.0", "from slack", 15);
        say(&conn, "cli-2", "second", 20);
        clear(&conn, "new-conversation:2", 30);
        say(&conn, "discord:9", "only discord", 35);
        clear(&conn, "new-conversation:3", 40);
        say(&conn, "cli-3", "third", 50);
        let listing = conversations(&db, AGENT).unwrap();
        let shape: Vec<(i64, i64, &str, bool)> = listing
            .conversations
            .iter()
            .map(|c| (c.turns, c.started_ms, c.first_text.as_str(), c.live))
            .collect();
        assert_eq!(shape, [(1, 50, "third", true), (2, 10, "first", false)]);
        assert_eq!(listing.conversations[1].last_ms, 20);
        assert!(!listing.partial);

        let old = listing.conversations[1].id;
        let ops = conversation(&db, AGENT, old).unwrap().unwrap();
        let texts: Vec<&str> = ops.iter().map(|op| op.text.as_str()).collect();
        assert_eq!(texts, ["first", "second"]);
        assert_eq!(ops[0].after, None, "no history, no state");
        // Only the first turn of a conversation is its id.
        assert_eq!(conversation(&db, AGENT, ops[1].seq).unwrap(), None);
        assert_eq!(conversation(&db, AGENT, 999).unwrap(), None);
    }

    #[test]
    fn a_purged_start_marks_the_oldest_conversation_partial() {
        let dir = tempfile::tempdir().unwrap();
        let (db, conn) = database(dir.path());
        conn.execute(
            "UPDATE harvest_executions SET input_json = \
             '{\"state\": {\"transcript\": [{\"role\": \"user\", \"content\": []}]}}'",
            [],
        )
        .unwrap();
        say(&conn, "cli-1", "late", 10);
        assert!(conversations(&db, AGENT).unwrap().partial);
    }

    #[test]
    fn a_missing_file_has_no_conversations() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("none.db");
        assert_eq!(conversations(&db, AGENT).unwrap().conversations, Vec::new());
        assert_eq!(conversation(&db, AGENT, 1).unwrap(), None);
    }

    #[test]
    fn a_prefix_is_matched_literally() {
        assert_eq!(like("cli-"), "cli-%");
        assert_eq!(like("a_b%"), "a\\_b\\%%");
    }
}
