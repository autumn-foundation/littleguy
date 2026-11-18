//! The patches that the assistant proposes (ADR 0042).
//!
//! A patch is a change to one repository that waits for the owner. The
//! owner opens it, and the daemon pushes it to a branch and opens a draft
//! PR. No tool of the assistant can open one. The store keeps the edits,
//! the diff that the owner reads, and the digest of that diff. The open
//! compares the digest, so the owner opens exactly what the owner read.
//!
//! The open has several steps on GitHub. The store keeps the result of
//! each step, so a restart resumes where it stopped and never opens a
//! second PR.

use autumn_harvest_agent::{AgentError, ErrorKind};
use rusqlite::{OptionalExtension, Row, params};

use super::SqliteStores;

/// The most patches that wait for the owner at once. It bounds the
/// assistant, not the owner, as for skill drafts.
pub const MAX_WAITING: i64 = 20;

/// The most closed patches that a listing shows.
pub const MAX_LISTED_CLOSED: i64 = 20;

/// Where a patch stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchStatus {
    /// It waits for the owner.
    Waiting,
    /// The owner declined it.
    Declined,
    /// The assistant withdrew it.
    Withdrawn,
    /// It is a draft PR.
    Opened,
    /// Its PR merged.
    Merged,
    /// Its PR closed without a merge.
    Closed,
}

impl PatchStatus {
    /// The stored name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Waiting => "waiting",
            Self::Declined => "declined",
            Self::Withdrawn => "withdrawn",
            Self::Opened => "opened",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    /// The status of the stored name `name`.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        [
            Self::Waiting,
            Self::Declined,
            Self::Withdrawn,
            Self::Opened,
            Self::Merged,
            Self::Closed,
        ]
        .into_iter()
        .find(|status| status.as_str() == name)
    }
}

/// What a plan item a patch serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchFor {
    /// A goal, by id.
    Goal(i64),
    /// An idea, by id.
    Idea(i64),
}

/// A new patch, before the store numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPatch {
    /// The PR title.
    pub title: String,
    /// The PR body.
    pub body: String,
    /// The commit that the edits are written against.
    pub base: String,
    /// The edits, as JSON.
    pub edits: String,
    /// The unified diff that the edits make at `base`.
    pub diff: String,
    /// The digest of the title, the body, the base and the diff.
    pub digest: String,
    /// The opened patch whose PR this one revises.
    pub revises: Option<i64>,
    /// The goal or idea that it serves.
    pub serves: Option<PatchFor>,
}

/// One stored patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patch {
    /// Its number.
    pub id: i64,
    /// The text that the assistant proposed.
    pub new: NewPatch,
    /// Where it stands.
    pub status: PatchStatus,
    /// Why the owner declined it, when the owner said.
    pub reason: Option<String>,
    /// The branch that holds its commit, once pushed.
    pub branch: Option<String>,
    /// Its commit, once made.
    pub commit: Option<String>,
    /// Its PR number, once opened.
    pub pr: Option<i64>,
    /// When the assistant proposed it, in epoch milliseconds.
    pub created_ms: i64,
    /// When its status last changed, in epoch milliseconds.
    pub updated_ms: i64,
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

const COLUMNS: &str = "id, title, body, base, edits, diff, digest, revises, for_goal, \
     for_idea, status, reason, branch, commit_sha, pr, created_ms, updated_ms";

fn patch_of(row: &Row<'_>) -> rusqlite::Result<Patch> {
    let status: String = row.get(10)?;
    let goal: Option<i64> = row.get(8)?;
    let idea: Option<i64> = row.get(9)?;
    Ok(Patch {
        id: row.get(0)?,
        new: NewPatch {
            title: row.get(1)?,
            body: row.get(2)?,
            base: row.get(3)?,
            edits: row.get(4)?,
            diff: row.get(5)?,
            digest: row.get(6)?,
            revises: row.get(7)?,
            serves: goal
                .map(PatchFor::Goal)
                .or_else(|| idea.map(PatchFor::Idea)),
        },
        // A status that this build does not know reads as closed, so it
        // never opens.
        status: PatchStatus::parse(&status).unwrap_or(PatchStatus::Closed),
        reason: row.get(11)?,
        branch: row.get(12)?,
        commit: row.get(13)?,
        pr: row.get(14)?,
        created_ms: row.get(15)?,
        updated_ms: row.get(16)?,
    })
}

impl SqliteStores {
    /// Stores `patch` for the call `key`. Returns its id. A retried call
    /// gets the id of its first row.
    ///
    /// # Errors
    ///
    /// Returns a `Tool` error when [`MAX_WAITING`] patches wait, or when
    /// `revises` names a patch that is not an open PR. Returns an error
    /// when the store fails.
    pub fn add_patch(&self, key: &str, patch: &NewPatch, now_ms: i64) -> Result<i64, AgentError> {
        self.write(|tx| {
            if let Some(id) = tx
                .query_row(
                    "SELECT id FROM patches WHERE key = ?1",
                    params![key],
                    |row| row.get(0),
                )
                .optional()?
            {
                return Ok(Ok(id));
            }
            if let Some(revised) = patch.revises {
                let status: Option<String> = tx
                    .query_row(
                        "SELECT status FROM patches WHERE id = ?1 AND pr IS NOT NULL",
                        params![revised],
                        |row| row.get(0),
                    )
                    .optional()?;
                if status.as_deref() != Some(PatchStatus::Opened.as_str()) {
                    return Ok(Err(refuse(format!(
                        "patch {revised} is not an open PR, so it cannot be revised"
                    ))));
                }
            }
            let waiting: i64 = tx.query_row(
                "SELECT COUNT(*) FROM patches WHERE status = 'waiting'",
                [],
                |row| row.get(0),
            )?;
            if waiting >= MAX_WAITING {
                return Ok(Err(refuse(format!(
                    "{MAX_WAITING} patches wait for the owner. Wait until they decide, or \
                     withdraw one"
                ))));
            }
            let (goal, idea) = match patch.serves {
                Some(PatchFor::Goal(id)) => (Some(id), None),
                Some(PatchFor::Idea(id)) => (None, Some(id)),
                None => (None, None),
            };
            tx.execute(
                "INSERT INTO patches (title, body, base, edits, diff, digest, revises, for_goal, \
                 for_idea, created_ms, updated_ms, key) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10, ?11)",
                params![
                    patch.title,
                    patch.body,
                    patch.base,
                    patch.edits,
                    patch.diff,
                    patch.digest,
                    patch.revises,
                    goal,
                    idea,
                    now_ms,
                    key
                ],
            )?;
            Ok(Ok(tx.last_insert_rowid()))
        })?
    }

    /// The patch `id`, in any status.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn patch(&self, id: i64) -> Result<Option<Patch>, AgentError> {
        self.read(|conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM patches WHERE id = ?1"),
                    params![id],
                    patch_of,
                )
                .optional()?)
        })
    }

    /// The patches that wait or are open PRs, oldest first, then the newest
    /// [`MAX_LISTED_CLOSED`] that ended, newest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn patches(&self) -> Result<Vec<Patch>, AgentError> {
        self.read(|conn| {
            let mut live = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM patches WHERE status IN ('waiting', 'opened') ORDER BY id"
            ))?;
            let mut all: Vec<Patch> = live
                .query_map([], patch_of)?
                .collect::<rusqlite::Result<_>>()?;
            let mut ended = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM patches WHERE status NOT IN ('waiting', 'opened') \
                 ORDER BY updated_ms DESC, id DESC LIMIT ?1"
            ))?;
            all.extend(
                ended
                    .query_map(params![MAX_LISTED_CLOSED], patch_of)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            Ok(all)
        })
    }

    /// Moves the waiting patch `id` to `status`, with `reason`. Returns
    /// whether it waited.
    fn close_waiting(
        &self,
        id: i64,
        status: PatchStatus,
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<bool, AgentError> {
        self.write(|tx| {
            Ok(tx.execute(
                "UPDATE patches SET status = ?2, reason = ?3, updated_ms = ?4 \
                 WHERE id = ?1 AND status = 'waiting' AND commit_sha IS NULL",
                params![id, status.as_str(), reason, now_ms],
            )? == 1)
        })
    }

    /// The assistant withdraws the waiting patch `id`. Returns whether it
    /// waited. A patch whose open has started cannot be withdrawn.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn withdraw_patch(&self, id: i64, now_ms: i64) -> Result<bool, AgentError> {
        self.close_waiting(id, PatchStatus::Withdrawn, None, now_ms)
    }

    /// The owner declines the waiting patch `id`, with `reason`. Returns
    /// whether it waited.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn decline_patch(
        &self,
        id: i64,
        reason: Option<&str>,
        now_ms: i64,
    ) -> Result<bool, AgentError> {
        self.close_waiting(id, PatchStatus::Declined, reason, now_ms)
    }

    /// Records the commit of the waiting patch `id` on `branch`. The open
    /// resumes from it after a restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails, or the patch does not wait.
    pub fn record_patch_commit(
        &self,
        id: i64,
        branch: &str,
        commit: &str,
        now_ms: i64,
    ) -> Result<(), AgentError> {
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE patches SET branch = ?2, commit_sha = ?3, updated_ms = ?4 \
                 WHERE id = ?1 AND status = 'waiting'",
                params![id, branch, commit, now_ms],
            )?;
            Ok(if changed == 1 {
                Ok(())
            } else {
                Err(refuse(format!("patch {id} does not wait")))
            })
        })?
    }

    /// Records the PR `pr` of the waiting patch `id`, which is now open.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails, or the patch does not wait.
    pub fn record_patch_opened(&self, id: i64, pr: i64, now_ms: i64) -> Result<(), AgentError> {
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE patches SET status = 'opened', pr = ?2, updated_ms = ?3 \
                 WHERE id = ?1 AND status = 'waiting' AND commit_sha IS NOT NULL",
                params![id, pr, now_ms],
            )?;
            Ok(if changed == 1 {
                Ok(())
            } else {
                Err(refuse(format!("patch {id} has no commit that waits")))
            })
        })?
    }

    /// The numbers of the PRs that are open, each once, lowest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn open_prs(&self) -> Result<Vec<i64>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT pr FROM patches WHERE status = 'opened' AND pr IS NOT NULL \
                 ORDER BY pr",
            )?;
            let prs = statement.query_map([], |row| row.get(0))?;
            Ok(prs.collect::<rusqlite::Result<_>>()?)
        })
    }

    /// Ends each open patch of the PR `pr` as merged or closed. Returns the
    /// patches that it ended.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn end_pr(&self, pr: i64, merged: bool, now_ms: i64) -> Result<Vec<Patch>, AgentError> {
        let status = if merged {
            PatchStatus::Merged
        } else {
            PatchStatus::Closed
        };
        self.write(|tx| {
            let ended: Vec<Patch> = {
                let mut statement = tx.prepare(&format!(
                    "SELECT {COLUMNS} FROM patches WHERE pr = ?1 AND status = 'opened' ORDER BY id"
                ))?;
                let rows = statement.query_map(params![pr], patch_of)?;
                rows.collect::<rusqlite::Result<_>>()?
            };
            tx.execute(
                "UPDATE patches SET status = ?2, updated_ms = ?3 WHERE pr = ?1 AND status = 'opened'",
                params![pr, status.as_str(), now_ms],
            )?;
            Ok(ended)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new(title: &str) -> NewPatch {
        NewPatch {
            title: title.to_owned(),
            body: String::from("why"),
            base: String::from("abc123"),
            edits: String::from("[]"),
            diff: String::from("--- a/x\n+++ b/x\n"),
            digest: String::from("d1"),
            revises: None,
            serves: Some(PatchFor::Idea(24)),
        }
    }

    #[test]
    fn a_patch_waits_once_and_opens_once() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let id = stores.add_patch("k", &new("fix"), 1).unwrap();
        assert_eq!(
            stores.add_patch("k", &new("again"), 2).unwrap(),
            id,
            "a retried call stores once"
        );
        let first = stores.patch(id).unwrap().unwrap();
        assert_eq!(first.status, PatchStatus::Waiting);
        assert_eq!(first.new, new("fix"));

        // Without a commit there is nothing to open.
        assert!(stores.record_patch_opened(id, 7, 3).is_err());
        stores
            .record_patch_commit(id, "littleguy/patch-1", "c1", 3)
            .unwrap();
        // A patch whose open started is no longer the assistant's to withdraw.
        assert!(!stores.withdraw_patch(id, 4).unwrap());
        stores.record_patch_opened(id, 7, 4).unwrap();
        assert!(stores.record_patch_opened(id, 7, 5).is_err());
        let opened = stores.patch(id).unwrap().unwrap();
        assert_eq!(opened.status, PatchStatus::Opened);
        assert_eq!(opened.pr, Some(7));
        assert_eq!(opened.branch.as_deref(), Some("littleguy/patch-1"));
        assert_eq!(stores.open_prs().unwrap(), [7]);

        let ended = stores.end_pr(7, true, 6).unwrap();
        assert_eq!(ended.len(), 1);
        assert_eq!(
            stores.patch(id).unwrap().unwrap().status,
            PatchStatus::Merged
        );
        assert_eq!(stores.end_pr(7, false, 7).unwrap().len(), 0);
        assert_eq!(stores.open_prs().unwrap(), [] as [i64; 0]);
    }

    #[test]
    fn a_revision_needs_an_open_pr() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let first = stores.add_patch("a", &new("fix"), 1).unwrap();
        let revision = NewPatch {
            revises: Some(first),
            ..new("fix the fix")
        };
        let refused = stores.add_patch("b", &revision, 2).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Tool);
        stores.record_patch_commit(first, "b", "c", 3).unwrap();
        stores.record_patch_opened(first, 9, 3).unwrap();
        let id = stores.add_patch("b", &revision, 4).unwrap();
        assert_eq!(stores.patch(id).unwrap().unwrap().new.revises, Some(first));
    }

    #[test]
    fn withdraw_and_decline_close_a_waiting_patch_only() {
        let stores = SqliteStores::open_in_memory().unwrap();
        let one = stores.add_patch("a", &new("one"), 1).unwrap();
        let two = stores.add_patch("b", &new("two"), 2).unwrap();
        assert!(stores.withdraw_patch(one, 3).unwrap());
        assert!(!stores.withdraw_patch(one, 3).unwrap());
        assert!(stores.decline_patch(two, Some("not now"), 4).unwrap());
        assert!(!stores.decline_patch(two, None, 5).unwrap());
        let declined = stores.patch(two).unwrap().unwrap();
        assert_eq!(declined.status, PatchStatus::Declined);
        assert_eq!(declined.reason.as_deref(), Some("not now"));
        // The ended ones list newest first, after the live ones.
        let listed: Vec<i64> = stores.patches().unwrap().iter().map(|p| p.id).collect();
        assert_eq!(listed, [two, one]);
    }

    #[test]
    fn a_full_queue_refuses_a_new_patch() {
        let stores = SqliteStores::open_in_memory().unwrap();
        for n in 0..MAX_WAITING {
            stores.add_patch(&format!("k{n}"), &new("p"), 0).unwrap();
        }
        let refused = stores.add_patch("over", &new("p"), 0).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::Tool);
        assert!(stores.withdraw_patch(1, 1).unwrap());
        stores.add_patch("over", &new("p"), 2).unwrap();
    }

    #[test]
    fn each_status_reads_back() {
        for status in [
            PatchStatus::Waiting,
            PatchStatus::Declined,
            PatchStatus::Withdrawn,
            PatchStatus::Opened,
            PatchStatus::Merged,
            PatchStatus::Closed,
        ] {
            assert_eq!(PatchStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(PatchStatus::parse("gone"), None);
    }
}
