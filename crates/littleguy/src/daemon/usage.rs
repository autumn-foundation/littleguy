//! What one op spends: the rows of the cost ledger that its drive records
//! (issue #61).
//!
//! The runtime has one writer, and [`Daemon::handle`](super::Daemon::handle)
//! drives one op at a time. So the ledger rows that appear during the drive
//! belong to the op. A mark before the drive notes the run of the
//! assistant and the last event with a model call. After the drive, the
//! usage sums the rows of that run past the mark. When the drive starts
//! the run, or continues it as new, it adds every row of the new run.

use anyhow::Result;
use autumn_harvest_agent::{AgentId, sqlite};
use autumn_harvest_sqlite::{ExecutionId, LlmLedgerRow, SqliteRuntime};

use crate::protocol::Usage;

/// Where the ledger of an assistant stands before a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark {
    /// The run of the assistant, if it has one.
    run: Option<ExecutionId>,
    /// The last event of that run with a model call, if any.
    seq: Option<i64>,
}

/// Marks where the ledger of the assistant `id` stands.
///
/// # Errors
///
/// Returns an error when the runtime cannot list the runs or read the
/// ledger.
pub(super) fn mark(rt: &SqliteRuntime, id: &AgentId) -> Result<Mark> {
    let run = sqlite::current_run(rt, id)?;
    let seq = match run {
        Some(exec) => rt.llm_ledger(exec)?.iter().map(|row| row.event_seq).max(),
        None => None,
    };
    Ok(Mark { run, seq })
}

/// What the drive since `mark` spent.
///
/// # Errors
///
/// Returns an error when the runtime cannot list the runs or read the
/// ledger.
pub(super) fn since(rt: &SqliteRuntime, id: &AgentId, mark: Mark) -> Result<Usage> {
    let mut usage = Usage::default();
    if let Some(exec) = mark.run {
        for row in rt.llm_ledger(exec)? {
            if mark.seq.is_none_or(|seq| row.event_seq > seq) {
                add(&mut usage, &row);
            }
        }
    }
    let now = sqlite::current_run(rt, id)?;
    if let Some(exec) = now
        && now != mark.run
    {
        for row in rt.llm_ledger(exec)? {
            add(&mut usage, &row);
        }
    }
    Ok(usage)
}

/// Adds one model call to `usage`.
const fn add(usage: &mut Usage, row: &LlmLedgerRow) {
    usage.calls += 1;
    usage.input_tokens += row.input_tokens;
    usage.output_tokens += row.output_tokens;
    match row.cost_usd_micros {
        Some(cost) => usage.cost_usd_micros += cost,
        None => usage.unpriced_calls += 1,
    }
}
