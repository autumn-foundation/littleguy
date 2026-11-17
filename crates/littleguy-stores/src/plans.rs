//! Goals and ideas: what the assistant does, and what it plans.
//!
//! - An **idea** is a thing to plan and research. The assistant files one
//!   and adds research notes to it.
//! - A **goal** is a thing to do. The assistant proposes one, often from an
//!   idea. It does nothing until the owner accepts it, as a refine overlay
//!   does nothing until the owner activates it. An active goal can ask for
//!   a check every so often, and it keeps progress notes.
//!
//! So every change that the assistant makes is safe on a run with no person
//! present: it files, notes, proposes, finishes and concludes. Only the
//! owner accepts, declines, drops and reopens.
//!
//! An idea is `open`, `answered`, `promoted` or `dropped`. The assistant
//! concludes an open idea that its research answered, and the idea leaves
//! the open list (ADR 0041). Its row and its notes stay. The owner reopens
//! it when they disagree. An accepted goal promotes its idea, and the owner
//! drops an idea that is open or answered.
//!
//! Each insert of a tool call carries the key of that call. A retried call
//! therefore adds one row, and gets back the same id.

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{OptionalExtension, Transaction, params};

use super::{Failure, SqliteStores};

/// The most ideas that stay open at once. A run that files ideas in a loop
/// stops here.
pub const MAX_OPEN_IDEAS: i64 = 100;

/// The most goals that wait for the owner at once.
pub const MAX_PROPOSED_GOALS: i64 = 20;

/// The most goals that stay active at once.
pub const MAX_ACTIVE_GOALS: i64 = 50;

/// The notes of each item in a listing, newest last.
pub const LISTED_NOTES: i64 = 5;

/// The answered ideas in the owner's listing, the last concluded first.
pub const LISTED_ANSWERED: i64 = 20;

/// Where a goal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GoalStatus {
    /// The assistant proposed it. It waits for the owner.
    Proposed,
    /// The owner accepted it. The assistant works on it.
    Active,
    /// The assistant finished it.
    Done,
    /// The owner declined the proposal.
    Declined,
    /// The owner dropped the active goal.
    Dropped,
}

impl GoalStatus {
    /// The name in the store and on the wire.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Active => "active",
            Self::Done => "done",
            Self::Declined => "declined",
            Self::Dropped => "dropped",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        [
            Self::Proposed,
            Self::Active,
            Self::Done,
            Self::Declined,
            Self::Dropped,
        ]
        .into_iter()
        .find(|status| status.as_str() == text)
    }
}

/// One note of a goal or an idea.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// When it was written, in epoch milliseconds.
    pub at_ms: i64,
    /// The text.
    pub text: String,
}

/// One idea that is open or answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Idea {
    /// The number that the owner and the assistant use.
    pub id: i64,
    /// A short name.
    pub title: String,
    /// What the idea is.
    pub body: String,
    /// When it was filed, in epoch milliseconds.
    pub created_ms: i64,
    /// The newest research notes, oldest first.
    pub notes: Vec<Note>,
}

/// One goal that waits for the owner or is active.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Goal {
    /// The number that the owner and the assistant use.
    pub id: i64,
    /// Proposed or active.
    pub status: GoalStatus,
    /// A short name.
    pub title: String,
    /// Why it matters.
    pub why: String,
    /// The time between two checks, in seconds. `None`: no checks.
    pub check_every_secs: Option<i64>,
    /// When the next check is due, in epoch milliseconds. Only an active
    /// goal has one.
    pub next_check_ms: Option<i64>,
    /// The idea that it came from.
    pub idea: Option<i64>,
    /// When it was proposed, in epoch milliseconds.
    pub created_ms: i64,
    /// The newest progress notes, oldest first.
    pub notes: Vec<Note>,
}

/// The goals that wait or are active, the open ideas and the answered
/// ones.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Plans {
    /// The active goals, the first due first, then the proposed ones.
    pub goals: Vec<Goal>,
    /// The open ideas, the oldest first.
    pub ideas: Vec<Idea>,
    /// The newest [`LISTED_ANSWERED`] answered ideas, the last concluded
    /// first. Only the owner's listing shows them: the assistant's working
    /// set is the open ideas.
    pub answered: Vec<Idea>,
}

/// A refusal that the model can read and act on.
fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

/// `text` trimmed, refused when it is empty.
fn checked<'a>(what: &str, text: &'a str) -> Result<&'a str, AgentError> {
    let text = text.trim();
    if text.is_empty() {
        return Err(refuse(format!("the {what} is empty")));
    }
    Ok(text)
}

/// The id of the row of `table` that the call `key` added, if any.
fn keyed(tx: &Transaction<'_>, table: &str, key: &str) -> Result<Option<i64>, Failure> {
    Ok(tx
        .query_row(
            &format!("SELECT id FROM {table} WHERE key = ?1"),
            params![key],
            |row| row.get(0),
        )
        .optional()?)
}

/// The status of the row `id` of `table`, if the row exists.
fn status(tx: &Transaction<'_>, table: &str, id: i64) -> Result<Option<String>, Failure> {
    Ok(tx
        .query_row(
            &format!("SELECT status FROM {table} WHERE id = ?1"),
            params![id],
            |row| row.get(0),
        )
        .optional()?)
}

/// The rows of `table` with `status`.
fn count(tx: &Transaction<'_>, table: &str, status: &str) -> Result<i64, Failure> {
    Ok(tx.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE status = ?1"),
        params![status],
        |row| row.get(0),
    )?)
}

/// Adds a note to `owner` in `table`, once for `key`. Every note stays.
fn add_note(
    tx: &Transaction<'_>,
    table: &str,
    owner: i64,
    key: &str,
    text: &str,
    now_ms: i64,
) -> Result<(), Failure> {
    let column = if table == "goal_notes" {
        "goal"
    } else {
        "idea"
    };
    tx.execute(
        &format!(
            "INSERT INTO {table} ({column}, text, at_ms, key) VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT (key) DO NOTHING"
        ),
        params![owner, text, now_ms, key],
    )?;
    Ok(())
}

/// Moves goal `id` from `from` to `to`. Returns whether it was at `from`.
fn move_goal(
    tx: &Transaction<'_>,
    id: i64,
    from: GoalStatus,
    to: GoalStatus,
) -> Result<bool, Failure> {
    Ok(tx.execute(
        "UPDATE goals SET status = ?3, next_check_ms = NULL WHERE id = ?1 AND status = ?2",
        params![id, from.as_str(), to.as_str()],
    )? == 1)
}

impl SqliteStores {
    /// Files an idea for the call `key`. Returns its id.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for an empty text, or when
    /// [`MAX_OPEN_IDEAS`] are open. Returns an error when the store fails.
    pub fn add_idea(
        &self,
        key: &str,
        title: &str,
        body: &str,
        now_ms: i64,
    ) -> Result<i64, AgentError> {
        let title = checked("title", title)?;
        let body = checked("body", body)?;
        self.write(|tx| {
            if let Some(id) = keyed(tx, "ideas", key)? {
                return Ok(Ok(id));
            }
            if count(tx, "ideas", "open")? >= MAX_OPEN_IDEAS {
                return Ok(Err(refuse(format!(
                    "{MAX_OPEN_IDEAS} ideas are open. Conclude the ones that you answered, \
                     propose a goal from one, or ask the owner to drop some, before you file \
                     another"
                ))));
            }
            tx.execute(
                "INSERT INTO ideas (title, body, created_ms, key) VALUES (?1, ?2, ?3, ?4)",
                params![title, body, now_ms, key],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        })?
    }

    /// Adds a research note to the open idea `id`, for the call `key`.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for an empty note, or an idea that is
    /// not open. Returns an error when the store fails.
    pub fn research_idea(
        &self,
        key: &str,
        id: i64,
        note: &str,
        now_ms: i64,
    ) -> Result<(), AgentError> {
        let note = checked("note", note)?;
        self.write(|tx| {
            if status(tx, "ideas", id)?.as_deref() != Some("open") {
                return Ok(Err(refuse(format!("idea {id} is not open"))));
            }
            add_note(tx, "idea_notes", id, key, note, now_ms)?;
            Ok(Ok(()))
        })?
    }

    /// Concludes the open idea `id` with the closing `note`, for the call
    /// `key`. The idea is answered: it leaves the open list, and its row and
    /// notes stay. A retried call succeeds once more.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for an empty note, or an idea that is not
    /// open. Returns an error when the store fails.
    pub fn conclude_idea(
        &self,
        key: &str,
        id: i64,
        note: &str,
        now_ms: i64,
    ) -> Result<(), AgentError> {
        let note = checked("note", note)?;
        self.write(|tx| {
            // The note of this call exists: an earlier try concluded it.
            if keyed(tx, "idea_notes", key)?.is_some() {
                return Ok(Ok(()));
            }
            match status(tx, "ideas", id)?.as_deref() {
                Some("open") => {}
                Some("answered") => {
                    return Ok(Err(refuse(format!("idea {id} is answered already"))));
                }
                _ => return Ok(Err(refuse(format!("idea {id} is not open")))),
            }
            add_note(tx, "idea_notes", id, key, note, now_ms)?;
            tx.execute(
                "UPDATE ideas SET status = 'answered' WHERE id = ?1",
                params![id],
            )?;
            Ok(Ok(()))
        })?
    }

    /// Reopens the answered idea `id`. Returns whether it was answered.
    /// Only the owner reopens an idea.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error when [`MAX_OPEN_IDEAS`] are open. Returns an
    /// error when the store fails.
    pub fn reopen_idea(&self, id: i64) -> Result<bool, AgentError> {
        self.write(|tx| {
            if status(tx, "ideas", id)?.as_deref() != Some("answered") {
                return Ok(Ok(false));
            }
            if count(tx, "ideas", "open")? >= MAX_OPEN_IDEAS {
                return Ok(Err(refuse(format!(
                    "{MAX_OPEN_IDEAS} ideas are open. Drop one first"
                ))));
            }
            tx.execute(
                "UPDATE ideas SET status = 'open' WHERE id = ?1",
                params![id],
            )?;
            Ok(Ok(true))
        })?
    }

    /// Drops the open or answered idea `id`. Returns whether it was open or
    /// answered. Only the owner drops an idea.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn drop_idea(&self, id: i64) -> Result<bool, AgentError> {
        self.write(|tx| {
            Ok(tx.execute(
                "UPDATE ideas SET status = 'dropped' WHERE id = ?1 \
                 AND status IN ('open', 'answered')",
                params![id],
            )? == 1)
        })
    }

    /// Proposes a goal for the call `key`. Returns its id. With `idea`, it
    /// comes from that open idea. The goal waits for the owner.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for an empty text, an interval under
    /// a minute, an idea that is not open, or when [`MAX_PROPOSED_GOALS`]
    /// wait. Returns an error when the store fails.
    pub fn propose_goal(
        &self,
        key: &str,
        title: &str,
        why: &str,
        check_every_secs: Option<i64>,
        idea: Option<i64>,
        now_ms: i64,
    ) -> Result<i64, AgentError> {
        let title = checked("title", title)?;
        let why = checked("reason", why)?;
        if check_every_secs.is_some_and(|every| every < 60) {
            return Err(refuse("a check interval is at least one minute"));
        }
        self.write(|tx| {
            if let Some(id) = keyed(tx, "goals", key)? {
                return Ok(Ok(id));
            }
            if count(tx, "goals", GoalStatus::Proposed.as_str())? >= MAX_PROPOSED_GOALS {
                return Ok(Err(refuse(format!(
                    "{MAX_PROPOSED_GOALS} proposed goals wait for the owner. Wait until \
                     they decide"
                ))));
            }
            if let Some(idea) = idea
                && status(tx, "ideas", idea)?.as_deref() != Some("open")
            {
                return Ok(Err(refuse(format!("idea {idea} is not open"))));
            }
            tx.execute(
                "INSERT INTO goals (title, why, status, check_every_secs, idea, created_ms, key) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    title,
                    why,
                    GoalStatus::Proposed.as_str(),
                    check_every_secs,
                    idea,
                    now_ms,
                    key
                ],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        })?
    }

    /// Accepts the proposed goal `id` at `now_ms`. Its first check is due
    /// one interval later, and its idea is promoted. Returns whether it was
    /// proposed. Only the owner accepts.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error when [`MAX_ACTIVE_GOALS`] are active. Returns
    /// an error when the store fails.
    pub fn accept_goal(&self, id: i64, now_ms: i64) -> Result<bool, AgentError> {
        self.write(|tx| {
            if status(tx, "goals", id)?.as_deref() != Some(GoalStatus::Proposed.as_str()) {
                return Ok(Ok(false));
            }
            if count(tx, "goals", GoalStatus::Active.as_str())? >= MAX_ACTIVE_GOALS {
                return Ok(Err(refuse(format!(
                    "{MAX_ACTIVE_GOALS} goals are active. Drop one first"
                ))));
            }
            tx.execute(
                "UPDATE goals SET status = ?2, \
                 next_check_ms = ?3 + check_every_secs * 1000 WHERE id = ?1",
                params![id, GoalStatus::Active.as_str(), now_ms],
            )?;
            tx.execute(
                "UPDATE ideas SET status = 'promoted' WHERE status = 'open' \
                 AND id = (SELECT idea FROM goals WHERE id = ?1)",
                params![id],
            )?;
            Ok(Ok(true))
        })?
    }

    /// Declines the proposed goal `id`. Returns whether it was proposed.
    /// Its idea stays open. Only the owner declines.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn decline_goal(&self, id: i64) -> Result<bool, AgentError> {
        self.write(|tx| move_goal(tx, id, GoalStatus::Proposed, GoalStatus::Declined))
    }

    /// Drops the active goal `id`. Returns whether it was active. Only the
    /// owner drops.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn drop_goal(&self, id: i64) -> Result<bool, AgentError> {
        self.write(|tx| move_goal(tx, id, GoalStatus::Active, GoalStatus::Dropped))
    }

    /// Adds a progress note to the active goal `id`, for the call `key`. A
    /// goal with checks has its next check due one interval from now. With
    /// `finished`, the goal is done.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error for an empty note, or a goal that is
    /// not active. Returns an error when the store fails.
    pub fn goal_progress(
        &self,
        key: &str,
        id: i64,
        note: &str,
        finished: bool,
        now_ms: i64,
    ) -> Result<(), AgentError> {
        let note = checked("note", note)?;
        self.write(|tx| {
            if status(tx, "goals", id)?.as_deref() != Some(GoalStatus::Active.as_str()) {
                return Ok(Err(refuse(format!("goal {id} is not active"))));
            }
            add_note(tx, "goal_notes", id, key, note, now_ms)?;
            if finished {
                move_goal(tx, id, GoalStatus::Active, GoalStatus::Done)?;
            } else {
                tx.execute(
                    "UPDATE goals SET next_check_ms = ?2 + check_every_secs * 1000 \
                     WHERE id = ?1 AND check_every_secs IS NOT NULL",
                    params![id, now_ms],
                )?;
            }
            Ok(Ok(()))
        })?
    }

    /// The active goals whose check is due at `now_ms`.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn due_goals(&self, now_ms: i64) -> Result<Vec<i64>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT id FROM goals WHERE status = 'active' AND next_check_ms <= ?1 \
                 ORDER BY next_check_ms, id",
            )?;
            let ids = statement.query_map(params![now_ms], |row| row.get(0))?;
            Ok(ids.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// The active and proposed goals, the open ideas and the newest
    /// answered ones, each with its newest notes.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn plans(&self) -> Result<Plans, AgentError> {
        self.read(|conn| {
            let notes = |table: &str, column: &str, owner: i64| -> rusqlite::Result<Vec<Note>> {
                let mut statement = conn.prepare(&format!(
                    "SELECT at_ms, text FROM (SELECT id, at_ms, text FROM {table} \
                     WHERE {column} = ?1 ORDER BY id DESC LIMIT ?2) ORDER BY id"
                ))?;
                let rows = statement.query_map(params![owner, LISTED_NOTES], |row| {
                    Ok(Note {
                        at_ms: row.get(0)?,
                        text: row.get(1)?,
                    })
                })?;
                rows.collect()
            };
            let mut statement = conn.prepare(
                "SELECT id, status, title, why, check_every_secs, next_check_ms, idea, \
                 created_ms FROM goals WHERE status IN ('active', 'proposed') \
                 ORDER BY status = 'proposed', next_check_ms IS NULL, next_check_ms, id",
            )?;
            let goals = statement
                .query_map([], |row| {
                    let status: String = row.get(1)?;
                    Ok(Goal {
                        id: row.get(0)?,
                        status: GoalStatus::parse(&status).unwrap_or(GoalStatus::Proposed),
                        title: row.get(2)?,
                        why: row.get(3)?,
                        check_every_secs: row.get(4)?,
                        next_check_ms: row.get(5)?,
                        idea: row.get(6)?,
                        created_ms: row.get(7)?,
                        notes: Vec::new(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .map(|mut goal| {
                    goal.notes = notes("goal_notes", "goal", goal.id)?;
                    Ok(goal)
                })
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let ideas = |query: &str| -> rusqlite::Result<Vec<Idea>> {
                let mut statement = conn.prepare(query)?;
                statement
                    .query_map([], |row| {
                        Ok(Idea {
                            id: row.get(0)?,
                            title: row.get(1)?,
                            body: row.get(2)?,
                            created_ms: row.get(3)?,
                            notes: Vec::new(),
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .into_iter()
                    .map(|mut idea| {
                        idea.notes = notes("idea_notes", "idea", idea.id)?;
                        Ok(idea)
                    })
                    .collect()
            };
            let open = ideas(
                "SELECT id, title, body, created_ms FROM ideas WHERE status = 'open' ORDER BY id",
            )?;
            // The closing note is the newest note of an answered idea, so
            // its time orders the list.
            let answered = ideas(&format!(
                "SELECT id, title, body, created_ms FROM ideas WHERE status = 'answered' \
                 ORDER BY (SELECT MAX(id) FROM idea_notes WHERE idea = ideas.id) DESC, id DESC \
                 LIMIT {LISTED_ANSWERED}"
            ))?;
            Ok(Plans {
                goals,
                ideas: open,
                answered,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stores() -> SqliteStores {
        SqliteStores::open_in_memory().unwrap()
    }

    fn refused<T: std::fmt::Debug>(result: Result<T, AgentError>) {
        assert_eq!(result.unwrap_err().kind(), ErrorKind::Tool);
    }

    #[test]
    fn an_idea_is_researched_proposed_and_accepted() {
        let stores = stores();
        let idea = stores
            .add_idea("k1", "Watch the docs", "Links rot on the docs site.", 1_000)
            .unwrap();
        stores
            .research_idea("k2", idea, "Found 3 broken links.", 2_000)
            .unwrap();
        let goal = stores
            .propose_goal(
                "k3",
                "Fix the docs links",
                "Readers hit dead ends.",
                Some(3_600),
                Some(idea),
                3_000,
            )
            .unwrap();
        // A proposal changes nothing yet: the idea is open, nothing is due.
        let plans = stores.plans().unwrap();
        assert_eq!(plans.ideas.len(), 1);
        assert_eq!(
            plans.ideas[0].notes,
            [Note {
                at_ms: 2_000,
                text: String::from("Found 3 broken links."),
            }]
        );
        assert_eq!(plans.goals[0].status, GoalStatus::Proposed);
        assert_eq!(plans.goals[0].next_check_ms, None);
        assert_eq!(stores.due_goals(i64::MAX).unwrap(), Vec::<i64>::new());
        refused(stores.goal_progress("k4", goal, "early", false, 4_000));

        assert!(stores.accept_goal(goal, 10_000).unwrap());
        assert!(!stores.accept_goal(goal, 11_000).unwrap(), "once");
        let plans = stores.plans().unwrap();
        assert_eq!(plans.ideas, [], "an accepted goal promotes its idea");
        assert_eq!(plans.goals[0].status, GoalStatus::Active);
        assert_eq!(plans.goals[0].idea, Some(idea));
        assert_eq!(plans.goals[0].next_check_ms, Some(3_610_000));
        refused(stores.research_idea("k5", idea, "late", 12_000));
    }

    #[test]
    fn a_declined_goal_leaves_its_idea_open() {
        let stores = stores();
        let idea = stores.add_idea("i", "I", "b", 0).unwrap();
        let goal = stores
            .propose_goal("g", "G", "w", None, Some(idea), 0)
            .unwrap();
        assert!(stores.decline_goal(goal).unwrap());
        assert!(!stores.accept_goal(goal, 1).unwrap());
        let plans = stores.plans().unwrap();
        assert_eq!(plans.goals, []);
        assert_eq!(plans.ideas.len(), 1);
    }

    #[test]
    fn a_retried_call_adds_one_row() {
        let stores = stores();
        let first = stores.add_idea("same", "A", "b", 1).unwrap();
        assert_eq!(stores.add_idea("same", "A", "b", 2).unwrap(), first);
        stores.research_idea("note", first, "n", 3).unwrap();
        stores.research_idea("note", first, "n", 4).unwrap();
        let goal = stores.propose_goal("g", "G", "w", None, None, 5).unwrap();
        assert_eq!(
            stores.propose_goal("g", "G", "w", None, None, 6).unwrap(),
            goal
        );
        let plans = stores.plans().unwrap();
        assert_eq!(plans.ideas[0].notes.len(), 1);
        assert_eq!(plans.goals.len(), 1);
    }

    #[test]
    fn progress_moves_the_next_check_and_a_finished_goal_is_gone() {
        let stores = stores();
        let checked = stores
            .propose_goal("a", "Checked", "w", Some(60), None, 0)
            .unwrap();
        let plain = stores
            .propose_goal("b", "Plain", "w", None, None, 0)
            .unwrap();
        stores.accept_goal(checked, 0).unwrap();
        stores.accept_goal(plain, 0).unwrap();
        assert_eq!(stores.due_goals(59_999).unwrap(), Vec::<i64>::new());
        assert_eq!(stores.due_goals(60_000).unwrap(), [checked]);
        stores
            .goal_progress("c", checked, "all fine", false, 70_000)
            .unwrap();
        assert_eq!(stores.due_goals(129_999).unwrap(), Vec::<i64>::new());
        assert_eq!(stores.due_goals(130_000).unwrap(), [checked]);

        stores
            .goal_progress("d", checked, "shipped", true, 80_000)
            .unwrap();
        assert_eq!(stores.due_goals(i64::MAX).unwrap(), Vec::<i64>::new());
        refused(stores.goal_progress("e", checked, "late", false, 90_000));
        assert!(stores.drop_goal(plain).unwrap());
        assert!(!stores.drop_goal(plain).unwrap());
        assert_eq!(stores.plans().unwrap().goals, []);
    }

    #[test]
    fn a_concluded_idea_leaves_the_open_list_and_the_owner_can_reopen_it() {
        let stores = stores();
        let first = stores.add_idea("a", "First", "b", 0).unwrap();
        let second = stores.add_idea("b", "Second", "b", 0).unwrap();
        stores.research_idea("n", first, "Looked.", 1).unwrap();
        refused(stores.conclude_idea("c0", first, "  ", 2));
        stores
            .conclude_idea("c1", first, "Answered: no goal.", 2)
            .unwrap();
        // A retry of the same call succeeds and adds no note.
        stores
            .conclude_idea("c1", first, "Answered: no goal.", 3)
            .unwrap();
        let plans = stores.plans().unwrap();
        assert_eq!(
            plans.ideas.iter().map(|idea| idea.id).collect::<Vec<_>>(),
            [second]
        );
        assert_eq!(plans.answered.len(), 1);
        assert_eq!(plans.answered[0].id, first);
        assert_eq!(
            plans.answered[0]
                .notes
                .iter()
                .map(|note| note.text.as_str())
                .collect::<Vec<_>>(),
            ["Looked.", "Answered: no goal."]
        );

        // An answered idea takes no note, no second conclusion and no goal.
        refused(stores.research_idea("n2", first, "late", 4));
        let again = stores.conclude_idea("c2", first, "again", 4).unwrap_err();
        assert!(again.to_string().contains("answered already"), "{again}");
        refused(stores.propose_goal("g", "G", "w", None, Some(first), 4));
        refused(stores.conclude_idea("c3", 999, "none", 4));

        // The last concluded comes first.
        stores.conclude_idea("c4", second, "Done too.", 5).unwrap();
        let answered: Vec<i64> = stores
            .plans()
            .unwrap()
            .answered
            .iter()
            .map(|idea| idea.id)
            .collect();
        assert_eq!(answered, [second, first]);

        // The owner reopens it, and the assistant works on it again.
        assert!(stores.reopen_idea(first).unwrap());
        assert!(!stores.reopen_idea(first).unwrap(), "once");
        stores.research_idea("n3", first, "More.", 6).unwrap();
        let plans = stores.plans().unwrap();
        assert_eq!(plans.ideas[0].id, first);
        assert_eq!(plans.answered.len(), 1);

        // The owner drops an answered idea as well as an open one.
        assert!(stores.drop_idea(second).unwrap());
        assert!(!stores.reopen_idea(second).unwrap());
        assert_eq!(stores.plans().unwrap().answered, []);
    }

    #[test]
    fn an_answered_idea_frees_a_place_and_a_reopen_respects_the_cap() {
        let stores = stores();
        for n in 0..MAX_OPEN_IDEAS {
            stores.add_idea(&format!("i{n}"), "I", "b", 0).unwrap();
        }
        refused(stores.add_idea("over", "I", "b", 0));
        stores.conclude_idea("c", 1, "Answered.", 1).unwrap();
        stores.add_idea("room", "I", "b", 0).unwrap();
        refused(stores.reopen_idea(1));
        for n in 0..=LISTED_ANSWERED {
            stores
                .conclude_idea(&format!("d{n}"), n + 2, "Answered.", n + 2)
                .unwrap();
        }
        let answered = stores.plans().unwrap().answered;
        assert_eq!(answered.len(), usize::try_from(LISTED_ANSWERED).unwrap());
        assert_eq!(answered[0].id, LISTED_ANSWERED + 2, "the last first");
    }

    #[test]
    fn bad_text_and_full_lists_are_refused() {
        let stores = stores();
        refused(stores.add_idea("a", "  ", "b", 0));
        // A long text is no bound. A fresh store keeps the list empty.
        let long = "t".repeat(100_000);
        self::stores().add_idea("b", &long, &long, 0).unwrap();
        refused(stores.propose_goal("c", "G", "w", Some(59), None, 0));
        refused(stores.propose_goal("d", "G", "w", None, Some(999), 0));
        for n in 0..MAX_OPEN_IDEAS {
            stores.add_idea(&format!("i{n}"), "I", "b", 0).unwrap();
        }
        refused(stores.add_idea("over", "I", "b", 0));
        assert!(stores.drop_idea(1).unwrap());
        assert!(!stores.drop_idea(1).unwrap());
        stores.add_idea("room", "I", "b", 0).unwrap();
        for n in 0..MAX_PROPOSED_GOALS {
            stores
                .propose_goal(&format!("p{n}"), "G", "w", None, None, 0)
                .unwrap();
        }
        refused(stores.propose_goal("over", "G", "w", None, None, 0));

        for n in 0..200 {
            stores.research_idea(&format!("n{n}"), 2, "x", n).unwrap();
        }
        let kept: i64 = stores
            .read(|db| {
                Ok(db.query_row(
                    "SELECT COUNT(*) FROM idea_notes WHERE idea = 2",
                    [],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(kept, 200);
        let plans = stores.plans().unwrap();
        let idea = plans.ideas.iter().find(|idea| idea.id == 2).unwrap();
        assert_eq!(idea.notes.len(), usize::try_from(LISTED_NOTES).unwrap());
        assert_eq!(idea.notes.last().unwrap().at_ms, 199);
    }
}
