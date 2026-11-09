//! The tool calls that wait for a person.
//!
//! The tool policy records each call that needs an approval, with its
//! arguments, before the run waits. The engine names the wait by a signal.
//! It does not show the arguments of the call. So the daemon claims the
//! record of the call when it sees the wait, and shows the record to the
//! approver. An approval sends the recorded arguments back. The approver
//! therefore approves exactly the call that they read.
//!
//! A record moves through three states. The policy adds it unclaimed. The
//! daemon claims it with the signal of the wait and the requester. A
//! decision sets its verdict, and the end of the wait closes it.

use autumn_harvest_agent::AgentError;
use rusqlite::{OptionalExtension, Row, params};
use serde_json::Value;

use super::{Failure, SqliteStores, decode, encode};

/// The most records that the store keeps. A record older than that is
/// closed or was never claimed, so it is safe to forget.
pub const MAX_APPROVALS: i64 = 1_000;

/// A decision on one call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Run the call.
    Approved,
    /// Do not run the call.
    Denied,
}

impl Verdict {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Denied => "denied",
        }
    }

    fn parse(text: &str) -> Option<Self> {
        match text {
            "approved" => Some(Self::Approved),
            "denied" => Some(Self::Denied),
            _ => None,
        }
    }
}

/// One tool call that needs an approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    /// The number that a person uses to decide.
    pub id: i64,
    /// The provider id of the call.
    pub call_id: String,
    /// The tool name.
    pub tool: String,
    /// The arguments of the call.
    pub arguments: Value,
    /// Why the policy asks.
    pub reason: String,
    /// The signal of the wait, once the daemon claims the record.
    pub signal: Option<String>,
    /// Who asked for the work, as the daemon wrote it.
    pub requester: Option<String>,
    /// The decision, once a person makes it.
    pub verdict: Option<Verdict>,
    /// Whether the wait ended.
    pub closed: bool,
}

const COLUMNS: &str = "id, call_id, tool, arguments, reason, signal, requester, verdict, closed";

fn record(row: &Row<'_>) -> rusqlite::Result<(ApprovalRecord, String)> {
    let verdict: Option<String> = row.get(7)?;
    Ok((
        ApprovalRecord {
            id: row.get(0)?,
            call_id: row.get(1)?,
            tool: row.get(2)?,
            arguments: Value::Null,
            reason: row.get(4)?,
            signal: row.get(5)?,
            requester: row.get(6)?,
            verdict: verdict.as_deref().and_then(Verdict::parse),
            closed: row.get(8)?,
        },
        row.get(3)?,
    ))
}

/// Decodes the arguments of a row read by [`record`].
fn finish((mut found, arguments): (ApprovalRecord, String)) -> Result<ApprovalRecord, Failure> {
    found.arguments = decode(&arguments)?;
    Ok(found)
}

/// A claim of one record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The record, with the signal set.
    pub record: ApprovalRecord,
    /// Whether this claim set the signal. A wait that the daemon saw before
    /// is not fresh.
    pub fresh: bool,
}

impl SqliteStores {
    /// Records one call that needs an approval. Returns its number.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn record_approval(
        &self,
        call_id: &str,
        tool: &str,
        arguments: &Value,
        reason: &str,
    ) -> Result<i64, AgentError> {
        self.write(|tx| {
            tx.execute(
                "INSERT INTO approvals (call_id, tool, arguments, reason) VALUES (?1, ?2, ?3, ?4)",
                params![call_id, tool, encode(arguments)?, reason],
            )?;
            let id = tx.last_insert_rowid();
            tx.execute(
                "DELETE FROM approvals WHERE id <= ?1 - ?2 AND (closed = 1 OR signal IS NULL)",
                params![id, MAX_APPROVALS],
            )?;
            Ok(id)
        })
    }

    /// Claims the record of the wait on `signal` for the call `call_id`.
    ///
    /// The record that holds `signal` comes first. Otherwise the newest
    /// unclaimed record of the call takes `signal` and `requester`. The
    /// policy records a call just before its wait starts, so that record is
    /// the one that waits. Returns `None` when no record fits.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write or a record does not
    /// decode.
    pub fn claim_approval(
        &self,
        signal: &str,
        call_id: &str,
        requester: Option<&str>,
    ) -> Result<Option<Claim>, AgentError> {
        self.write(|tx| {
            let held = tx
                .query_row(
                    &format!("SELECT {COLUMNS} FROM approvals WHERE signal = ?1"),
                    params![signal],
                    record,
                )
                .optional()?;
            if let Some(held) = held {
                return Ok(Some(Claim {
                    record: finish(held)?,
                    fresh: false,
                }));
            }
            let open = tx
                .query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM approvals WHERE call_id = ?1 AND signal IS NULL \
                         AND closed = 0 ORDER BY id DESC LIMIT 1"
                    ),
                    params![call_id],
                    record,
                )
                .optional()?;
            let Some(open) = open else {
                return Ok(None);
            };
            let mut found = finish(open)?;
            tx.execute(
                "UPDATE approvals SET signal = ?1, requester = ?2 WHERE id = ?3",
                params![signal, requester, found.id],
            )?;
            found.signal = Some(signal.to_owned());
            found.requester = requester.map(str::to_owned);
            Ok(Some(Claim {
                record: found,
                fresh: true,
            }))
        })
    }

    /// The record with the number `id`.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read or the record does not
    /// decode.
    pub fn approval(&self, id: i64) -> Result<Option<ApprovalRecord>, AgentError> {
        self.read(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM approvals WHERE id = ?1"),
                params![id],
                record,
            )
            .optional()?
            .map(finish)
            .transpose()
        })
    }

    /// The claimed records whose wait has not ended, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read or a record does not
    /// decode.
    pub fn open_approvals(&self) -> Result<Vec<ApprovalRecord>, AgentError> {
        self.read(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM approvals WHERE signal IS NOT NULL AND closed = 0 \
                 ORDER BY id"
            ))?;
            let rows = statement.query_map([], record)?;
            rows.map(|row| finish(row?)).collect()
        })
    }

    /// Sets the verdict of the open, claimed record `id`. Returns `false`
    /// when the record has a verdict already, is closed, or is not claimed.
    /// So one call gets one decision.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn set_verdict(&self, id: i64, verdict: Verdict) -> Result<bool, AgentError> {
        self.write(|tx| {
            let changed = tx.execute(
                "UPDATE approvals SET verdict = ?1 WHERE id = ?2 AND verdict IS NULL \
                 AND closed = 0 AND signal IS NOT NULL",
                params![verdict.as_str(), id],
            )?;
            Ok(changed == 1)
        })
    }

    /// Records that the wait of `id` ended.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub fn close_approval(&self, id: i64) -> Result<(), AgentError> {
        self.write(|tx| {
            tx.execute("UPDATE approvals SET closed = 1 WHERE id = ?1", params![id])?;
            Ok(())
        })
    }
}
