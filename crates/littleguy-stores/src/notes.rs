//! The notes of the assistant (ADR 0029).
//!
//! A note is text under a name, such as `ui-punchlist`. The assistant keeps
//! what it learns here, beside its ideas and goals in `memory.db`. A tick
//! may write a note: the workspace is read-only on a tick, but this store
//! is the assistant's own, as its ideas are.

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{OptionalExtension, params};

use super::SqliteStores;

/// A refusal that the model reads.
fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

/// One note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// The name.
    pub name: String,
    /// The text.
    pub text: String,
    /// When it last changed, in epoch milliseconds.
    pub updated_ms: i64,
}

/// `name`, trimmed, when it is a name: one line, not empty. The store
/// bounds neither a name, nor a note, nor the count of notes (ADR 0034).
fn checked(name: &str) -> Result<&str, AgentError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(refuse("a note name is not empty"));
    }
    if name.chars().any(char::is_control) {
        return Err(refuse("a note name is one line"));
    }
    Ok(name)
}

impl SqliteStores {
    /// Writes the note `name`, in place of one with that name. Returns
    /// whether the note is new.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for a bad name. Returns an error when the
    /// store fails.
    pub fn write_note(&self, name: &str, text: &str, now_ms: i64) -> Result<bool, AgentError> {
        let name = checked(name)?;
        self.write(|tx| {
            let exists: bool = tx
                .query_row("SELECT 1 FROM notes WHERE name = ?1", params![name], |_| {
                    Ok(())
                })
                .optional()?
                .is_some();
            tx.execute(
                "INSERT INTO notes (name, text, updated_ms) VALUES (?1, ?2, ?3) \
                 ON CONFLICT(name) DO UPDATE SET text = ?2, updated_ms = ?3",
                params![name, text, now_ms],
            )?;
            Ok(!exists)
        })
    }

    /// The note `name`, if there is one.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn note(&self, name: &str) -> Result<Option<Note>, AgentError> {
        let name = name.trim().to_owned();
        self.read(|conn| {
            conn.query_row(
                "SELECT name, text, updated_ms FROM notes WHERE name = ?1",
                params![name],
                |row| {
                    Ok(Note {
                        name: row.get(0)?,
                        text: row.get(1)?,
                        updated_ms: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    /// Every note, by name.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn notes(&self) -> Result<Vec<Note>, AgentError> {
        self.read(|conn| {
            let mut statement =
                conn.prepare("SELECT name, text, updated_ms FROM notes ORDER BY name")?;
            let rows = statement.query_map([], |row| {
                Ok(Note {
                    name: row.get(0)?,
                    text: row.get(1)?,
                    updated_ms: row.get(2)?,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }

    /// Deletes the note `name`. Returns whether there was one.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn delete_note(&self, name: &str) -> Result<bool, AgentError> {
        let name = name.trim().to_owned();
        self.write(|tx| Ok(tx.execute("DELETE FROM notes WHERE name = ?1", params![name])? > 0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_is_written_replaced_listed_and_deleted() {
        let stores = SqliteStores::open_in_memory().unwrap();
        assert!(stores.write_note("ui-punchlist", "1. Tabs.", 1).unwrap());
        assert!(
            !stores
                .write_note(" ui-punchlist ", "1. Tabs.\n2. Counts.", 2)
                .unwrap()
        );
        let note = stores.note("ui-punchlist").unwrap().unwrap();
        assert_eq!(note.text, "1. Tabs.\n2. Counts.");
        assert_eq!(note.updated_ms, 2);
        stores.write_note("a-first", "x", 3).unwrap();
        let names: Vec<String> = stores
            .notes()
            .unwrap()
            .into_iter()
            .map(|n| n.name)
            .collect();
        assert_eq!(names, ["a-first", "ui-punchlist"]);
        assert!(stores.delete_note("a-first").unwrap());
        assert!(!stores.delete_note("a-first").unwrap());
        assert_eq!(stores.note("a-first").unwrap(), None);
    }

    #[test]
    fn a_bad_name_is_refused() {
        let stores = SqliteStores::open_in_memory().unwrap();
        for name in ["", "   ", "two\nlines"] {
            assert!(stores.write_note(name, "x", 1).is_err(), "{name:?}");
        }
        assert_eq!(stores.notes().unwrap(), []);
    }

    #[test]
    fn the_store_bounds_no_note_and_no_count() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let long = "x".repeat(1_000_000);
        assert!(stores.write_note("big", &long, 1).unwrap());
        assert_eq!(stores.note("big").unwrap().unwrap().text.len(), 1_000_000);
        let name = "n".repeat(500);
        assert!(stores.write_note(&name, "x", 1).unwrap());
        for n in 0..1001 {
            stores.write_note(&format!("note-{n}"), "x", 1).unwrap();
        }
        assert_eq!(stores.notes().unwrap().len(), 1003);
    }
}
