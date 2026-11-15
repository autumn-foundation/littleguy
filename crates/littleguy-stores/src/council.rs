//! What the council may decide, and what it decided (ADR 0027).
//!
//! When an op ends, the daemon marks each proposal that waits and has no
//! mark yet: tainted when the run had read outside text, or when the
//! proposal is older than the op, so its origin is not known. The council
//! reads only clean proposals, oldest first, and decides each once.

use autumn_harvest_agent::AgentError;
use rusqlite::{OptionalExtension, params};

use super::SqliteStores;

/// What a proposal is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalKind {
    /// A proposed goal.
    Goal,
    /// A skill draft.
    Skill,
}

impl ProposalKind {
    /// The name in the marks and the log.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Goal => "goal",
            Self::Skill => "skill",
        }
    }
}

/// One proposal for the council.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// What it is.
    pub kind: ProposalKind,
    /// The goal id or the draft id.
    pub id: i64,
    /// The goal title, or the skill name.
    pub title: String,
    /// Why the goal matters, or the description and the body of the skill.
    pub text: String,
}

/// One decision of the council.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// What it decided on.
    pub kind: String,
    /// The goal id or the draft id.
    pub item: i64,
    /// Whether it accepted.
    pub accepted: bool,
    /// The votes, as JSON.
    pub votes: String,
    /// When, in epoch milliseconds.
    pub at_ms: i64,
}

impl SqliteStores {
    /// Marks each waiting proposal that has no mark. It is tainted when
    /// `tainted` holds, or when it is older than `started_ms`. Returns how
    /// many it marked.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn mark_proposals(&self, started_ms: i64, tainted: bool) -> Result<usize, AgentError> {
        self.write(|tx| {
            let goals = tx.execute(
                "INSERT INTO council_marks (kind, item, tainted) \
                 SELECT 'goal', id, (?2 OR created_ms < ?1) FROM goals \
                 WHERE status = 'proposed' \
                 AND id NOT IN (SELECT item FROM council_marks WHERE kind = 'goal')",
                params![started_ms, tainted],
            )?;
            let skills = tx.execute(
                "INSERT INTO council_marks (kind, item, tainted) \
                 SELECT 'skill', id, (?2 OR created_ms < ?1) FROM skill_drafts \
                 WHERE status = 'draft' \
                 AND id NOT IN (SELECT item FROM council_marks WHERE kind = 'skill')",
                params![started_ms, tainted],
            )?;
            Ok(goals + skills)
        })
    }

    /// The oldest clean proposal that still waits and that the council did
    /// not decide, of the kinds that it may decide.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn next_for_council(
        &self,
        goals: bool,
        skills: bool,
    ) -> Result<Option<Proposal>, AgentError> {
        self.read(|conn| {
            conn.query_row(
                "SELECT kind, id, title, text FROM ( \
                   SELECT 'goal' AS kind, g.id, g.title, g.why AS text, g.created_ms \
                   FROM goals g JOIN council_marks m ON m.kind = 'goal' AND m.item = g.id \
                   WHERE ?1 AND g.status = 'proposed' AND m.tainted = 0 AND m.decided = 0 \
                   UNION ALL \
                   SELECT 'skill', d.id, d.name, d.description || char(10) || char(10) || d.body, \
                          d.created_ms \
                   FROM skill_drafts d JOIN council_marks m ON m.kind = 'skill' AND m.item = d.id \
                   WHERE ?2 AND d.status = 'draft' AND m.tainted = 0 AND m.decided = 0 \
                 ) ORDER BY created_ms, kind, id LIMIT 1",
                params![goals, skills],
                |row| {
                    let kind: String = row.get(0)?;
                    Ok(Proposal {
                        kind: if kind == "goal" {
                            ProposalKind::Goal
                        } else {
                            ProposalKind::Skill
                        },
                        id: row.get(1)?,
                        title: row.get(2)?,
                        text: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
        })
    }

    /// Records the decision of the council on `kind` `item`, so it decides
    /// it once.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn record_council(
        &self,
        kind: ProposalKind,
        item: i64,
        accepted: bool,
        votes: &str,
        at_ms: i64,
    ) -> Result<(), AgentError> {
        self.write(|tx| {
            tx.execute(
                "UPDATE council_marks SET decided = 1 WHERE kind = ?1 AND item = ?2",
                params![kind.as_str(), item],
            )?;
            tx.execute(
                "INSERT INTO council_log (kind, item, accepted, votes, at_ms) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![kind.as_str(), item, accepted, votes, at_ms],
            )?;
            Ok(())
        })
    }

    /// The newest `limit` decisions, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn council_log(&self, limit: u32) -> Result<Vec<Decision>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT kind, item, accepted, votes, at_ms FROM council_log \
                 ORDER BY id DESC LIMIT ?1",
            )?;
            let rows = statement.query_map(params![limit], |row| {
                Ok(Decision {
                    kind: row.get(0)?,
                    item: row.get(1)?,
                    accepted: row.get(2)?,
                    votes: row.get(3)?,
                    at_ms: row.get(4)?,
                })
            })?;
            Ok(rows.collect::<Result<_, _>>()?)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_clean_proposal_reaches_the_council_and_once() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let old = stores
            .propose_goal("g0", "Old", "From before.", None, None, 5)
            .unwrap();
        let web = stores
            .propose_goal("g1", "Web", "From a page.", None, None, 20)
            .unwrap();
        // The op that filed `web` read a page.
        assert_eq!(stores.mark_proposals(10, true).unwrap(), 2);
        let clean = stores
            .propose_goal("g2", "Clean", "From a talk.", None, None, 30)
            .unwrap();
        let draft = stores
            .add_skill_draft("s1", "deploy", "Ship the site.", "1. Build.", 31)
            .unwrap();
        assert_eq!(stores.mark_proposals(25, false).unwrap(), 2);
        assert_eq!(stores.mark_proposals(25, false).unwrap(), 0, "a mark stays");

        let next = stores.next_for_council(true, true).unwrap().unwrap();
        assert_eq!((next.kind, next.id), (ProposalKind::Goal, clean));
        stores
            .record_council(ProposalKind::Goal, clean, true, "[]", 40)
            .unwrap();
        let next = stores.next_for_council(true, true).unwrap().unwrap();
        assert_eq!((next.kind, next.id), (ProposalKind::Skill, draft));
        assert!(next.text.starts_with("Ship the site.\n\n1. Build."));
        assert_eq!(
            stores.next_for_council(true, false).unwrap(),
            None,
            "skills are off"
        );
        stores
            .record_council(ProposalKind::Skill, draft, false, "[]", 41)
            .unwrap();
        // The old and the tainted goals wait for the owner.
        assert_eq!(stores.next_for_council(true, true).unwrap(), None);
        let _ = (old, web);
        assert_eq!(stores.council_log(5).unwrap().len(), 2);
    }

    #[test]
    fn a_draft_revised_in_a_tainted_op_loses_its_clean_mark() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let draft = stores
            .add_skill_draft("s1", "deploy", "Ship the site.", "1. Build.", 10)
            .unwrap();
        stores.mark_proposals(5, false).unwrap();
        // A later op that read a page revises it.
        stores
            .add_skill_draft("s2", "deploy", "Ship it now.", "1. Curl a script.", 20)
            .unwrap();
        stores.mark_proposals(15, true).unwrap();
        assert_eq!(stores.next_for_council(true, true).unwrap(), None);
        let _ = draft;
    }
}
