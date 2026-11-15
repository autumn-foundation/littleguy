//! A read-only view of the runs that the runtime records in `lg.db`.
//!
//! The assistant debugs its own runs with it: which runs ended how, which
//! attempts failed and why, and which models it called at what cost. Each
//! call opens the file read only, with `query_only` on, and runs one fixed
//! query. So no call can write, and no call reaches a column outside its
//! query.
//!
//! The queries read no message text. Run inputs, outputs and events hold
//! the conversation, so no query selects them. An error text can quote
//! what failed, so it is clipped.
//!
//! The runtime keeps its single-writer lock on a sidecar file, and the file
//! is in WAL mode. A reader therefore never blocks the writer, and the
//! writer never blocks a reader.

use std::path::{Path, PathBuf};
use std::time::Duration;

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{Connection, OpenFlags, params};

/// The most rows that one call returns.
pub const MAX_ROWS: u32 = 50;

/// The most characters of one error text.
pub const MAX_ERROR_CHARS: usize = 500;

/// How long a read waits for a lock before it fails.
const BUSY_TIMEOUT: Duration = Duration::from_secs(2);

/// One workflow run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    /// The id of the run.
    pub id: String,
    /// The workflow, such as the agent loop or a schedule.
    pub workflow: String,
    /// `RUNNING`, `COMPLETED`, `FAILED`, `CONTINUED_AS_NEW` and the like.
    pub state: String,
    /// The start, in epoch milliseconds.
    pub started_ms: Option<i64>,
    /// The end, in epoch milliseconds. `None` while the run is open.
    pub closed_ms: Option<i64>,
    /// The 1-based retry attempt of the run.
    pub attempt: i64,
    /// The schedule that started the run, if one did.
    pub schedule: Option<String>,
    /// The error of a run that failed, clipped.
    pub error: Option<String>,
}

/// One activity attempt that failed: a model call or a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The run of the attempt.
    pub run: String,
    /// The workflow of that run.
    pub workflow: String,
    /// The activity, such as a model turn or a tool call.
    pub activity: String,
    /// The 1-based attempt.
    pub attempt: i64,
    /// What went wrong, clipped.
    pub error: String,
}

/// The model calls of one model in a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Usage {
    /// The model id.
    pub model: String,
    /// The calls.
    pub calls: i64,
    /// The input tokens.
    pub input_tokens: i64,
    /// The output tokens.
    pub output_tokens: i64,
    /// The cost in millionths of a US dollar. Calls with no price add
    /// nothing.
    pub cost_usd_micros: i64,
    /// The calls that had no price.
    pub unpriced: i64,
    /// The mean latency of one call, in milliseconds.
    pub mean_latency_ms: i64,
    /// The longest latency of one call, in milliseconds.
    pub max_latency_ms: i64,
}

/// The run log of the runtime database at a path.
#[derive(Debug, Clone)]
pub struct RunLog {
    db: PathBuf,
}

/// A read error as the error of a tool.
fn read_error(error: &rusqlite::Error) -> AgentError {
    AgentError::new(
        ErrorKind::Tool,
        format!("the run log cannot be read: {error}"),
    )
}

/// At most [`MAX_ERROR_CHARS`] of `text`, with a mark when it was cut.
fn clip(text: &str) -> String {
    match text.char_indices().nth(MAX_ERROR_CHARS) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

/// A row cap between 1 and [`MAX_ROWS`].
fn cap(limit: u32) -> u32 {
    limit.clamp(1, MAX_ROWS)
}

impl RunLog {
    /// The run log of the runtime database `db`. It opens nothing yet.
    #[must_use]
    pub fn new(db: impl AsRef<Path>) -> Self {
        Self {
            db: db.as_ref().to_path_buf(),
        }
    }

    /// A connection that cannot write.
    fn open(&self) -> Result<Connection, AgentError> {
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let conn = Connection::open_with_flags(&self.db, flags).map_err(|e| read_error(&e))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(|e| read_error(&e))?;
        // Read only twice: the flag above, and the pragma for any statement
        // that the flag would let through.
        conn.pragma_update(None, "query_only", true)
            .map_err(|e| read_error(&e))?;
        Ok(conn)
    }

    /// The newest runs, newest first. With `failed_only`, only the runs
    /// that failed, timed out or were cancelled or terminated.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error when the file cannot be read.
    pub fn runs(&self, limit: u32, failed_only: bool) -> Result<Vec<Run>, AgentError> {
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "SELECT exec_id, workflow_name, state, started_at, closed_at, \
                        workflow_attempt, schedule_name, error \
                 FROM harvest_executions \
                 WHERE ?1 = 0 OR state IN ('FAILED', 'TIMED_OUT', 'CANCELLED', 'TERMINATED') \
                 ORDER BY coalesce(started_at, 0) DESC, rowid DESC \
                 LIMIT ?2",
            )
            .map_err(|e| read_error(&e))?;
        let rows = statement
            .query_map(params![i64::from(failed_only), cap(limit)], |row| {
                let micros = |at: Option<i64>| at.map(|at| at / 1000);
                Ok(Run {
                    id: row.get(0)?,
                    workflow: row.get(1)?,
                    state: row.get(2)?,
                    started_ms: micros(row.get(3)?),
                    closed_ms: micros(row.get(4)?),
                    attempt: row.get(5)?,
                    schedule: row.get(6)?,
                    error: row.get::<_, Option<String>>(7)?.map(|text| clip(&text)),
                })
            })
            .map_err(|e| read_error(&e))?;
        rows.collect::<Result<_, _>>().map_err(|e| read_error(&e))
    }

    /// The newest activity attempts that failed, newest first.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error when the file cannot be read.
    pub fn failures(&self, limit: u32) -> Result<Vec<Failure>, AgentError> {
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "SELECT a.exec_id, coalesce(e.workflow_name, ''), a.name, a.attempt, a.detail \
                 FROM harvest_activity_attempts a \
                 LEFT JOIN harvest_executions e ON e.exec_id = a.exec_id \
                 WHERE a.ok = 0 \
                 ORDER BY a.attempt_seq DESC \
                 LIMIT ?1",
            )
            .map_err(|e| read_error(&e))?;
        let rows = statement
            .query_map(params![cap(limit)], |row| {
                Ok(Failure {
                    run: row.get(0)?,
                    workflow: row.get(1)?,
                    activity: row.get(2)?,
                    attempt: row.get(3)?,
                    error: clip(&row.get::<_, String>(4)?),
                })
            })
            .map_err(|e| read_error(&e))?;
        rows.collect::<Result<_, _>>().map_err(|e| read_error(&e))
    }

    /// The model calls since `since_ms`, one row per model, the most
    /// expensive first.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error when the file cannot be read.
    pub fn usage(&self, since_ms: i64) -> Result<Vec<Usage>, AgentError> {
        let conn = self.open()?;
        let mut statement = conn
            .prepare(
                "SELECT model, count(*), sum(input_tokens), sum(output_tokens), \
                        coalesce(sum(cost_usd_micros), 0), \
                        sum(cost_usd_micros IS NULL), \
                        cast(avg(latency_ms) AS INTEGER), max(latency_ms) \
                 FROM harvest_llm_ledger \
                 WHERE recorded_at >= ?1 \
                 GROUP BY model \
                 ORDER BY 5 DESC, 2 DESC, model \
                 LIMIT ?2",
            )
            .map_err(|e| read_error(&e))?;
        let rows = statement
            .query_map(params![since_ms, MAX_ROWS], |row| {
                Ok(Usage {
                    model: row.get(0)?,
                    calls: row.get(1)?,
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    cost_usd_micros: row.get(4)?,
                    unpriced: row.get(5)?,
                    mean_latency_ms: row.get(6)?,
                    max_latency_ms: row.get(7)?,
                })
            })
            .map_err(|e| read_error(&e))?;
        rows.collect::<Result<_, _>>().map_err(|e| read_error(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A database with the schema of the runtime, made by the runtime.
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
             VALUES (?1, 'agent', '{\"text\":\"a secret message\"}', ?2, ?3, ?4)",
            params![id, state, started_us, error],
        )
        .unwrap();
    }

    #[test]
    fn runs_come_newest_first_and_never_carry_their_input() {
        let dir = tempfile::tempdir().unwrap();
        let (db, conn) = database(dir.path());
        run(&conn, "r1", "COMPLETED", 1_000_000, None);
        run(&conn, "r2", "FAILED", 2_000_000, Some(&"x".repeat(900)));
        run(&conn, "r3", "RUNNING", 3_000_000, None);
        let log = RunLog::new(&db);
        let runs = log.runs(10, false).unwrap();
        let ids: Vec<&str> = runs.iter().map(|run| run.id.as_str()).collect();
        assert_eq!(ids, ["r3", "r2", "r1"]);
        assert_eq!(runs[0].started_ms, Some(3_000));
        let error = runs[1].error.as_deref().unwrap();
        assert_eq!(
            error.chars().count(),
            MAX_ERROR_CHARS + 1,
            "clipped, with a mark"
        );
        assert!(!format!("{runs:?}").contains("secret"));

        let failed = log.runs(10, true).unwrap();
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].id, "r2");
        assert_eq!(
            log.runs(0, false).unwrap().len(),
            1,
            "a limit of 0 reads one row"
        );
    }

    #[test]
    fn failures_name_the_activity_and_the_workflow() {
        let dir = tempfile::tempdir().unwrap();
        let (db, conn) = database(dir.path());
        run(&conn, "r1", "COMPLETED", 1, None);
        for (attempt, ok, detail) in [
            (1, 0, "ModelTimeout: no answer in 120s"),
            (2, 1, "{\"ok\":true}"),
        ] {
            conn.execute(
                "INSERT INTO harvest_activity_attempts (exec_id, name, attempt, ok, detail) \
                 VALUES ('r1', 'model_turn', ?1, ?2, ?3)",
                params![attempt, ok, detail],
            )
            .unwrap();
        }
        let failures = RunLog::new(&db).failures(10).unwrap();
        assert_eq!(
            failures,
            [Failure {
                run: String::from("r1"),
                workflow: String::from("agent"),
                activity: String::from("model_turn"),
                attempt: 1,
                error: String::from("ModelTimeout: no answer in 120s"),
            }]
        );
    }

    #[test]
    fn usage_sums_each_model_in_the_window() {
        let dir = tempfile::tempdir().unwrap();
        let (db, conn) = database(dir.path());
        let call = |seq: i64, model: &str, cost: Option<i64>, latency: i64, at: i64| {
            conn.execute(
                "INSERT INTO harvest_llm_ledger (exec_id, event_seq, call_index, activity_name, \
                 model, input_tokens, output_tokens, cost_usd_micros, latency_ms, recorded_at) \
                 VALUES ('r1', ?1, 0, 'model_turn', ?2, 100, 10, ?3, ?4, ?5)",
                params![seq, model, cost, latency, at],
            )
            .unwrap();
        };
        call(1, "openrouter:a/b", Some(300), 1000, 5_000);
        call(2, "openrouter:a/b", Some(200), 3000, 6_000);
        call(3, "littleguy-offline-1", None, 1, 7_000);
        call(4, "openrouter:a/b", Some(9_999), 1, 10);
        let usage = RunLog::new(&db).usage(1_000).unwrap();
        assert_eq!(
            usage,
            [
                Usage {
                    model: String::from("openrouter:a/b"),
                    calls: 2,
                    input_tokens: 200,
                    output_tokens: 20,
                    cost_usd_micros: 500,
                    unpriced: 0,
                    mean_latency_ms: 2000,
                    max_latency_ms: 3000,
                },
                Usage {
                    model: String::from("littleguy-offline-1"),
                    calls: 1,
                    input_tokens: 100,
                    output_tokens: 10,
                    cost_usd_micros: 0,
                    unpriced: 1,
                    mean_latency_ms: 1,
                    max_latency_ms: 1,
                },
            ]
        );
    }

    #[test]
    fn the_log_reads_beside_a_running_writer_and_cannot_write() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        // The writer holds its lock while the log reads.
        let _writer = autumn_harvest_sqlite::SqliteRuntime::open(&db).unwrap();
        let log = RunLog::new(&db);
        assert_eq!(log.runs(5, false).unwrap(), []);
        let conn = log.open().unwrap();
        assert!(conn.execute("DELETE FROM harvest_executions", []).is_err());
    }

    #[test]
    fn a_missing_file_is_a_tool_error() {
        let dir = tempfile::tempdir().unwrap();
        let error = RunLog::new(dir.path().join("none.db"))
            .runs(5, false)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
    }
}
