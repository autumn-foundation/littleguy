//! The skills that the assistant drafts for itself (ADR 0024).
//!
//! A draft is a skill that waits for the owner. The owner accepts it, and
//! the daemon writes it as a `SKILL.md` in the skills folder, or declines
//! it. A skill is standing instructions to the assistant, so no tool of the
//! assistant can accept one.

use autumn_harvest_agent::{AgentError, ErrorKind, SkillText};
use rusqlite::{OptionalExtension, params};

use super::SqliteStores;

/// The most drafts that wait for the owner at once.
pub const MAX_DRAFTS: i64 = 20;

/// One skill that waits for the owner.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDraft {
    /// Its number.
    pub id: i64,
    /// The name that the model would load it by.
    pub name: String,
    /// When to load it.
    pub description: String,
    /// The know-how.
    pub body: String,
    /// When it was drafted, in epoch milliseconds.
    pub created_ms: i64,
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

impl SqliteStores {
    /// Stores a draft for the call `key`. Returns its id.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for a skill that the engine refuses, or when
    /// [`MAX_DRAFTS`] wait. Returns an error when the store fails.
    pub fn add_skill_draft(
        &self,
        key: &str,
        name: &str,
        description: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<i64, AgentError> {
        let (name, description, body) = (name.trim(), description.trim(), body.trim());
        autumn_harvest_agent::skill::check(name, &SkillText::new(description, body))
            .map_err(|reason| refuse(format!("the skill is not valid: {reason}")))?;
        self.write(|tx| {
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM skill_drafts WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?
            {
                return Ok(Ok(id));
            }
            // A draft with the name of a waiting draft revises it, so one
            // name waits once.
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM skill_drafts WHERE name = ?1 AND status = 'draft'",
                    params![name],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            {
                tx.execute(
                    "UPDATE skill_drafts SET description = ?1, body = ?2, created_ms = ?3, \
                     key = ?4 WHERE id = ?5",
                    params![description, body, now_ms, key, id],
                )?;
                return Ok(Ok(id));
            }
            let waiting: i64 = tx.query_row(
                "SELECT COUNT(*) FROM skill_drafts WHERE status = 'draft'",
                [],
                |row| row.get(0),
            )?;
            if waiting >= MAX_DRAFTS {
                return Ok(Err(refuse(format!(
                    "{MAX_DRAFTS} skill drafts wait for the owner. Wait until they decide"
                ))));
            }
            tx.execute(
                "INSERT INTO skill_drafts (name, description, body, created_ms, key) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, description, body, now_ms, key],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        })?
    }

    /// The drafts that wait, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn skill_drafts(&self) -> Result<Vec<SkillDraft>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT id, name, description, body, created_ms FROM skill_drafts \
                 WHERE status = 'draft' ORDER BY id",
            )?;
            let rows = statement.query_map([], |row| {
                Ok(SkillDraft {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    body: row.get(3)?,
                    created_ms: row.get(4)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The draft `id`, if it waits.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn skill_draft(&self, id: i64) -> Result<Option<SkillDraft>, AgentError> {
        Ok(self
            .skill_drafts()?
            .into_iter()
            .find(|draft| draft.id == id))
    }

    /// Closes the waiting draft `id` as accepted or declined. Returns
    /// whether it waited.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn close_skill_draft(&self, id: i64, accepted: bool) -> Result<bool, AgentError> {
        let status = if accepted { "accepted" } else { "declined" };
        self.write(|tx| {
            Ok(tx.execute(
                "UPDATE skill_drafts SET status = ?2 WHERE id = ?1 AND status = 'draft'",
                params![id, status],
            )? == 1)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_draft_with_the_name_of_a_waiting_draft_replaces_it() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let first = stores
            .add_skill_draft("k1", "deploy", "Ship the site.", "1. Build.", 1)
            .unwrap();
        let again = stores
            .add_skill_draft(
                "k2",
                "deploy",
                "Ship the site, then tag.",
                "1. Build.\n2. Tag.",
                2,
            )
            .unwrap();
        assert_eq!(again, first, "the waiting draft is revised in place");
        let drafts = stores.skill_drafts().unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(drafts[0].description, "Ship the site, then tag.");
        // A retry of the second call finds its row.
        assert_eq!(
            stores
                .add_skill_draft("k2", "deploy", "Ship the site, then tag.", "x", 3)
                .unwrap(),
            first
        );
    }

    #[test]
    fn a_draft_waits_once_and_closes_once() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let id = stores
            .add_skill_draft("k", "deploy", "Ship the site.", "1. Build.", 1)
            .unwrap();
        assert_eq!(
            stores
                .add_skill_draft("k", "deploy", "Ship the site.", "1. Build.", 2)
                .unwrap(),
            id,
            "a retried call drafts once"
        );
        assert_eq!(stores.skill_draft(id).unwrap().unwrap().name, "deploy");
        assert!(stores.close_skill_draft(id, true).unwrap());
        assert!(!stores.close_skill_draft(id, false).unwrap());
        assert_eq!(stores.skill_drafts().unwrap(), []);
    }

    #[test]
    fn a_bad_skill_or_a_full_list_is_refused() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let refused = |result: Result<i64, AgentError>| {
            assert_eq!(result.unwrap_err().kind(), ErrorKind::Tool);
        };
        refused(stores.add_skill_draft("a", "bad name!", "d", "b", 0));
        refused(stores.add_skill_draft("b", "ok", "two\nlines", "b", 0));
        for n in 0..MAX_DRAFTS {
            stores
                .add_skill_draft(&format!("d{n}"), &format!("s{n}"), "d", "b", 0)
                .unwrap();
        }
        refused(stores.add_skill_draft("over", "more", "d", "b", 0));
    }
}
