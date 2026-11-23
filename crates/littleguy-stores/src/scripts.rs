//! The saved scripts of the assistant (ADR 0045).
//!
//! The assistant drafts a script, and the draft waits for the owner. The
//! owner accepts it, and the daemon writes the code to the scripts folder
//! and records its SHA-256 here. `run_script` runs only a script with a
//! row here, and only when the file still has the recorded hash. No tool
//! of the assistant can accept a draft.
//!
//! The run log keeps what ran and with what arguments. A tick changes
//! nothing except through a blessed script, so the log makes that
//! exception visible on the Scripts tab.

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{OptionalExtension, Row, params};

use super::SqliteStores;

/// The most drafts that wait for the owner at once. It bounds the
/// assistant, not the owner, as for skill drafts.
pub const MAX_DRAFTS: i64 = 20;

/// The most rows that the run log keeps. Older rows go, as in the tick log.
pub const MAX_RUNS: i64 = 500;

/// One script that waits for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptDraft {
    /// Its number.
    pub id: i64,
    /// The name that `run_script` takes.
    pub name: String,
    /// The sandbox runtime, such as `python3`.
    pub runtime: String,
    /// What it does, and what its arguments and stdin are.
    pub description: String,
    /// The program.
    pub code: String,
    /// When it was drafted, in epoch milliseconds.
    pub created_ms: i64,
}

/// One script that the owner accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Script {
    /// The name that `run_script` takes.
    pub name: String,
    /// The sandbox runtime, such as `python3`.
    pub runtime: String,
    /// What it does, and what its arguments and stdin are.
    pub description: String,
    /// The file name in the scripts folder.
    pub file: String,
    /// The SHA-256 of the file at acceptance, in lower-case hex.
    pub sha256: String,
    /// When the owner accepted it, in epoch milliseconds.
    pub accepted_ms: i64,
}

/// One row of the run log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScriptRun {
    /// Its number.
    pub id: i64,
    /// The script.
    pub name: String,
    /// The arguments, as given.
    pub args: Vec<String>,
    /// The stdin, as given.
    pub stdin: Option<String>,
    /// The run of the assistant that called it.
    pub run_id: String,
    /// What happened: `exited 0`, `refused: ...` and so on.
    pub outcome: String,
    /// How long the code ran, in milliseconds.
    pub duration_ms: i64,
    /// When it ran, in epoch milliseconds.
    pub at_ms: i64,
}

/// A run for the log, before the store numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewScriptRun<'a> {
    /// The key of the tool call. A retried call logs once.
    pub key: &'a str,
    /// The script.
    pub name: &'a str,
    /// The arguments, as given.
    pub args: &'a [String],
    /// The stdin, as given.
    pub stdin: Option<&'a str>,
    /// The run of the assistant that called it.
    pub run_id: &'a str,
    /// What happened.
    pub outcome: &'a str,
    /// How long the code ran, in milliseconds.
    pub duration_ms: i64,
    /// When it ran, in epoch milliseconds.
    pub at_ms: i64,
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

fn draft_of(row: &Row<'_>) -> rusqlite::Result<ScriptDraft> {
    Ok(ScriptDraft {
        id: row.get(0)?,
        name: row.get(1)?,
        runtime: row.get(2)?,
        description: row.get(3)?,
        code: row.get(4)?,
        created_ms: row.get(5)?,
    })
}

fn script_of(row: &Row<'_>) -> rusqlite::Result<Script> {
    Ok(Script {
        name: row.get(0)?,
        runtime: row.get(1)?,
        description: row.get(2)?,
        file: row.get(3)?,
        sha256: row.get(4)?,
        accepted_ms: row.get(5)?,
    })
}

const DRAFT_COLUMNS: &str = "id, name, runtime, description, code, created_ms";
const SCRIPT_COLUMNS: &str = "name, runtime, description, file, sha256, accepted_ms";

impl SqliteStores {
    /// Stores a draft for the call `key`. Returns its id. A draft with the
    /// name of a waiting draft revises it, so one name waits once. The
    /// caller checks the name and the runtime.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for empty code, or when [`MAX_DRAFTS`] wait.
    /// Returns an error when the store fails.
    pub fn add_script_draft(
        &self,
        key: &str,
        draft: &ScriptDraft,
        now_ms: i64,
    ) -> Result<i64, AgentError> {
        let (name, runtime, description) = (
            draft.name.trim(),
            draft.runtime.trim(),
            draft.description.trim(),
        );
        if draft.code.trim().is_empty() {
            return Err(refuse("the script has no code"));
        }
        let code = draft.code.as_str();
        self.write(|tx| {
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM script_drafts WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?
            {
                return Ok(Ok(id));
            }
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM script_drafts WHERE name = ?1 AND status = 'draft'",
                    params![name],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            {
                tx.execute(
                    "UPDATE script_drafts SET runtime = ?1, description = ?2, code = ?3, \
                     created_ms = ?4, key = ?5 WHERE id = ?6",
                    params![runtime, description, code, now_ms, key, id],
                )?;
                return Ok(Ok(id));
            }
            let waiting: i64 = tx.query_row(
                "SELECT COUNT(*) FROM script_drafts WHERE status = 'draft'",
                [],
                |row| row.get(0),
            )?;
            if waiting >= MAX_DRAFTS {
                return Ok(Err(refuse(format!(
                    "{MAX_DRAFTS} script drafts wait for the owner. Wait until they decide"
                ))));
            }
            tx.execute(
                "INSERT INTO script_drafts (name, runtime, description, code, created_ms, key) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![name, runtime, description, code, now_ms, key],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        })?
    }

    /// The drafts that wait, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn script_drafts(&self) -> Result<Vec<ScriptDraft>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {DRAFT_COLUMNS} FROM script_drafts WHERE status = 'draft' ORDER BY id"
            ))?;
            let rows = statement.query_map([], draft_of)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The draft `id`, if it waits.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn script_draft(&self, id: i64) -> Result<Option<ScriptDraft>, AgentError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    &format!(
                        "SELECT {DRAFT_COLUMNS} FROM script_drafts \
                         WHERE id = ?1 AND status = 'draft'"
                    ),
                    params![id],
                    draft_of,
                )
                .optional()?)
        })
    }

    /// Declines the waiting draft `id`. Returns whether it waited.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn decline_script_draft(&self, id: i64) -> Result<bool, AgentError> {
        self.write(|tx| {
            Ok(tx.execute(
                "UPDATE script_drafts SET status = 'declined' WHERE id = ?1 AND status = 'draft'",
                params![id],
            )? == 1)
        })
    }

    /// Accepts the waiting draft `id` as `script`, in one transaction: the
    /// draft closes, and the script replaces any script of its name.
    /// Returns whether the draft waited. The daemon writes the file first,
    /// so a row never names a file that was not written.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn accept_script_draft(&self, id: i64, script: &Script) -> Result<bool, AgentError> {
        self.write(|tx| {
            let closed = tx.execute(
                "UPDATE script_drafts SET status = 'accepted' WHERE id = ?1 AND status = 'draft'",
                params![id],
            )? == 1;
            if closed {
                tx.execute(
                    "INSERT INTO scripts (name, runtime, description, file, sha256, accepted_ms, \
                     draft) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) ON CONFLICT (name) DO UPDATE SET \
                     runtime = excluded.runtime, description = excluded.description, \
                     file = excluded.file, sha256 = excluded.sha256, \
                     accepted_ms = excluded.accepted_ms, draft = excluded.draft",
                    params![
                        script.name,
                        script.runtime,
                        script.description,
                        script.file,
                        script.sha256,
                        script.accepted_ms,
                        id
                    ],
                )?;
            }
            Ok(closed)
        })
    }

    /// The accepted scripts, by name.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn scripts(&self) -> Result<Vec<Script>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {SCRIPT_COLUMNS} FROM scripts ORDER BY name"
            ))?;
            let rows = statement.query_map([], script_of)?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The accepted script `name`, if any.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn script(&self, name: &str) -> Result<Option<Script>, AgentError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {SCRIPT_COLUMNS} FROM scripts WHERE name = ?1"),
                    params![name],
                    script_of,
                )
                .optional()?)
        })
    }

    /// Removes the accepted script `name`, so `run_script` refuses it.
    /// Returns whether it was there.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn retire_script(&self, name: &str) -> Result<bool, AgentError> {
        self.write(|tx| Ok(tx.execute("DELETE FROM scripts WHERE name = ?1", params![name])? == 1))
    }

    /// Logs one run once for its key, and keeps the newest [`MAX_RUNS`].
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails or the arguments do not
    /// encode.
    pub fn record_script_run(&self, run: &NewScriptRun<'_>) -> Result<(), AgentError> {
        let args = serde_json::to_string(run.args).map_err(|error| {
            AgentError::new(
                ErrorKind::Decode,
                format!("the arguments do not encode: {error}"),
            )
        })?;
        self.write(|tx| {
            tx.execute(
                "INSERT INTO script_runs (name, args, stdin, run_id, outcome, duration_ms, at_ms, \
                 key) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8) ON CONFLICT (key) DO NOTHING",
                params![
                    run.name,
                    args,
                    run.stdin,
                    run.run_id,
                    run.outcome,
                    run.duration_ms,
                    run.at_ms,
                    run.key
                ],
            )?;
            tx.execute(
                "DELETE FROM script_runs WHERE id <= (SELECT MAX(id) FROM script_runs) - ?1",
                params![MAX_RUNS],
            )?;
            Ok(())
        })
    }

    /// The newest `limit` runs, the newest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails or a row does not decode.
    pub fn script_runs(&self, limit: u32) -> Result<Vec<ScriptRun>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, name, args, stdin, run_id, outcome, duration_ms, at_ms \
                 FROM script_runs ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            })?;
            let mut runs = Vec::new();
            for row in rows {
                let (id, name, args, stdin, run_id, outcome, duration_ms, at_ms) = row?;
                runs.push(ScriptRun {
                    id,
                    name,
                    args: super::decode(&args)?,
                    stdin,
                    run_id,
                    outcome,
                    duration_ms,
                    at_ms,
                });
            }
            Ok(runs)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft(name: &str, code: &str) -> ScriptDraft {
        ScriptDraft {
            id: 0,
            name: name.to_owned(),
            runtime: String::from("python3"),
            description: String::from("Walk a repository."),
            code: code.to_owned(),
            created_ms: 0,
        }
    }

    fn script(name: &str, sha: &str) -> Script {
        Script {
            name: name.to_owned(),
            runtime: String::from("python3"),
            description: String::from("Walk a repository."),
            file: format!("{name}.py"),
            sha256: sha.to_owned(),
            accepted_ms: 5,
        }
    }

    #[test]
    fn a_draft_waits_once_and_a_retry_or_a_revision_keeps_its_id() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let id = stores
            .add_script_draft("k1", &draft("walk", "print(1)"), 1)
            .unwrap();
        assert_eq!(
            stores
                .add_script_draft("k1", &draft("walk", "print(9)"), 2)
                .unwrap(),
            id,
            "a retried call drafts once"
        );
        assert_eq!(stores.script_draft(id).unwrap().unwrap().code, "print(1)");
        let revised = stores
            .add_script_draft("k2", &draft("walk", "print(2)"), 3)
            .unwrap();
        assert_eq!(revised, id, "the waiting draft is revised in place");
        let drafts = stores.script_drafts().unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].code, "print(2)");
        assert_eq!(drafts[0].created_ms, 3);
    }

    #[test]
    fn empty_code_or_a_full_list_is_refused() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let error = stores
            .add_script_draft("e", &draft("walk", "  \n"), 0)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
        for n in 0..MAX_DRAFTS {
            stores
                .add_script_draft(&format!("d{n}"), &draft(&format!("s{n}"), "x"), 0)
                .unwrap();
        }
        let error = stores
            .add_script_draft("over", &draft("more", "x"), 0)
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
        // A revision of a waiting draft still fits.
        stores
            .add_script_draft("again", &draft("s0", "y"), 0)
            .unwrap();
    }

    #[test]
    fn acceptance_closes_the_draft_and_pins_the_script_once() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let id = stores
            .add_script_draft("k", &draft("walk", "print(1)"), 1)
            .unwrap();
        assert!(
            stores
                .accept_script_draft(id, &script("walk", "aa"))
                .unwrap()
        );
        assert!(
            !stores
                .accept_script_draft(id, &script("walk", "bb"))
                .unwrap(),
            "a closed draft accepts once"
        );
        assert_eq!(stores.script_drafts().unwrap(), []);
        assert_eq!(stores.script("walk").unwrap().unwrap().sha256, "aa");
        // A revision of an accepted script replaces its row.
        let next = stores
            .add_script_draft("k2", &draft("walk", "print(2)"), 2)
            .unwrap();
        assert_ne!(next, id);
        assert!(
            stores
                .accept_script_draft(next, &script("walk", "cc"))
                .unwrap()
        );
        assert_eq!(stores.scripts().unwrap(), [script("walk", "cc")]);
        assert!(stores.retire_script("walk").unwrap());
        assert!(!stores.retire_script("walk").unwrap());
        assert_eq!(stores.script("walk").unwrap(), None);
    }

    #[test]
    fn a_declined_draft_leaves_the_queue_and_accepts_nothing() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let id = stores
            .add_script_draft("k", &draft("walk", "print(1)"), 1)
            .unwrap();
        assert!(stores.decline_script_draft(id).unwrap());
        assert!(!stores.decline_script_draft(id).unwrap());
        assert!(
            !stores
                .accept_script_draft(id, &script("walk", "aa"))
                .unwrap()
        );
        assert_eq!(stores.scripts().unwrap(), []);
    }

    #[test]
    fn the_run_log_keeps_each_call_once_and_the_newest_first() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let args = vec![String::from("a b"), String::from("$(x)")];
        let run = |key: &'static str, at_ms: i64| NewScriptRun {
            key,
            name: "walk",
            args: &args,
            stdin: Some("in"),
            run_id: "run-1",
            outcome: "exited 0",
            duration_ms: 7,
            at_ms,
        };
        stores.record_script_run(&run("k1", 1)).unwrap();
        stores.record_script_run(&run("k1", 1)).unwrap();
        stores.record_script_run(&run("k2", 2)).unwrap();
        let runs = stores.script_runs(10).unwrap();
        assert_eq!(runs.len(), 2, "a retried call logs once");
        assert_eq!(runs[0].at_ms, 2);
        assert_eq!(runs[1].args, args);
        assert_eq!(runs[1].stdin.as_deref(), Some("in"));
    }

    #[test]
    fn the_run_log_keeps_its_size() {
        let stores = SqliteStores::open_in_memory().unwrap();
        for n in 0..=MAX_RUNS {
            stores
                .record_script_run(&NewScriptRun {
                    key: &format!("k{n}"),
                    name: "walk",
                    args: &[],
                    stdin: None,
                    run_id: "r",
                    outcome: "exited 0",
                    duration_ms: 0,
                    at_ms: n,
                })
                .unwrap();
        }
        let runs = stores.script_runs(u32::MAX).unwrap();
        assert_eq!(i64::try_from(runs.len()).unwrap(), MAX_RUNS);
        assert_eq!(runs[0].at_ms, MAX_RUNS);
    }
}
