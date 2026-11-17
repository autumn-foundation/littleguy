//! The newest state of an entity in the runtime database `lg.db`.
//!
//! The daemon answers the reads of the owner beside its loop (ADR 0038).
//! The state of the assistant is one of them. The engine records a state
//! snapshot at the start of each run and after each op. This module reads
//! the newest one over a connection that cannot write, as the run log
//! does. The file is in WAL mode, so the read never blocks the writer.

use std::path::Path;
use std::time::Duration;

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};

/// How long a read waits for a lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// The newest state of one entity, as the runtime database holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EntityState {
    /// The entity has no run.
    NoRun,
    /// The value of the newest state snapshot of the run.
    Snapshot(serde_json::Value),
    /// The run has no snapshot yet.
    Unknown,
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

/// The newest committed state of the entity `workflow_id` of
/// `workflow_name`: the value of the newest side effect named
/// `snapshot` in its running run.
///
/// The engine records a state snapshot at the start of each run and
/// after each op. While an op runs, the newest one is the state before
/// it. The walk goes from the newest event of the run, so it stops at
/// the first snapshot.
///
/// # Errors
///
/// Returns a `Tool` error when the file cannot be read or a snapshot is
/// not JSON.
pub fn entity_state(
    db: &Path,
    workflow_name: &str,
    workflow_id: &str,
    snapshot: &str,
) -> Result<EntityState, AgentError> {
    let conn = open(db)?;
    let exec: Option<String> = conn
        .query_row(
            "SELECT exec_id FROM harvest_executions \
             WHERE workflow_name = ?1 AND workflow_id = ?2 AND state = 'RUNNING' \
             ORDER BY rowid DESC LIMIT 1",
            params![workflow_name, workflow_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| read_error(&e))?;
    let Some(exec) = exec else {
        return Ok(EntityState::NoRun);
    };
    let value: Option<String> = conn
        .query_row(
            "SELECT json_extract(event_json, '$.data.value') FROM harvest_events \
             WHERE exec_id = ?1 \
               AND json_extract(event_json, '$.type') = 'SideEffectRecorded' \
               AND json_extract(event_json, '$.data.name') = ?2 \
             ORDER BY seq DESC LIMIT 1",
            params![exec, snapshot],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| read_error(&e))?
        .flatten();
    let Some(value) = value else {
        return Ok(EntityState::Unknown);
    };
    let value = serde_json::from_str(&value).map_err(|e| {
        AgentError::new(
            ErrorKind::Tool,
            format!("a state snapshot is not JSON: {e}"),
        )
    })?;
    Ok(EntityState::Snapshot(value))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn database(dir: &Path) -> (PathBuf, Connection) {
        let db = dir.join("lg.db");
        drop(autumn_harvest_sqlite::SqliteRuntime::open(&db).unwrap());
        let conn = Connection::open(&db).unwrap();
        (db, conn)
    }

    fn run(conn: &Connection, id: &str, state: &str, started_us: i64, error: Option<&str>) {
        conn.execute(
            "INSERT INTO harvest_executions \
             (exec_id, workflow_name, input_json, state, started_at, error) \
             VALUES (?1, 'agent', '{}', ?2, ?3, ?4)",
            params![id, state, started_us, error],
        )
        .unwrap();
    }

    #[test]
    fn an_entity_with_no_run_or_no_snapshot_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (db, conn) = database(dir.path());
        let state = |id: &str| entity_state(&db, "agent", id, "harvest.entity.state").unwrap();
        assert_eq!(state("assistant/mark"), EntityState::NoRun);
        // A running run whose first snapshot is not written yet.
        run(&conn, "r1", "RUNNING", 1, None);
        assert_eq!(state(""), EntityState::Unknown);
        conn.execute(
            "INSERT INTO harvest_events (exec_id, seq, event_json) VALUES ('r1', 1, ?1)",
            params![
                r#"{"type":"SideEffectRecorded","data":{"name":"harvest.entity.state","value":{"state":{"segments":4}}}}"#
            ],
        )
        .unwrap();
        assert_eq!(
            state(""),
            EntityState::Snapshot(serde_json::json!({"state": {"segments": 4}}))
        );
    }
}
