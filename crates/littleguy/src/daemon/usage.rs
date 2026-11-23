//! What one op spends: the rows of the cost ledger that its drive records
//! (issue #61).
//!
//! The runtime has one writer, and [`Daemon::handle`](super::Daemon::handle)
//! drives one op at a time. So the ledger rows that appear during the drive
//! belong to the op. A mark before the drive notes the run of the
//! assistant, the last event with a model call and the newest run. After
//! the drive, the usage sums the rows of that run past the mark. Each op
//! ends its run (ADR 0035), so the drive starts new runs too. The usage
//! adds every row of each run that is newer than the mark, in any
//! workflow. So a `sweep` call counts too: each source is a child run with
//! its own ledger rows (ADR 0044).

use anyhow::Result;
use autumn_harvest_agent::{AgentId, sqlite};
use autumn_harvest_sqlite::{ExecutionFilter, ExecutionId, LlmLedgerRow, SqliteRuntime};

use crate::protocol::Usage;

/// Where the ledger of an assistant stands before a drive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Mark {
    /// The run of the assistant, if it has one.
    run: Option<ExecutionId>,
    /// The last event of that run with a model call, if any.
    seq: Option<i64>,
    /// The newest run of any workflow in any state, if any.
    newest: Option<ExecutionId>,
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
    let filter = ExecutionFilter {
        limit: Some(1),
        ..ExecutionFilter::new()
    };
    let newest = rt
        .list_executions(&filter)?
        .executions
        .iter()
        .find_map(autumn_harvest_sqlite::ExecutionSummary::execution_id);
    Ok(Mark { run, seq, newest })
}

/// What the drive since `mark` spent.
///
/// # Errors
///
/// Returns an error when the runtime cannot list the runs or read the
/// ledger.
pub(super) fn since(rt: &SqliteRuntime, mark: Mark) -> Result<Usage> {
    let mut usage = Usage::default();
    if let Some(exec) = mark.run {
        for row in rt.llm_ledger(exec)? {
            if mark.seq.is_none_or(|seq| row.event_seq > seq) {
                add(&mut usage, &row);
            }
        }
    }
    for exec in newer_than(rt, mark.newest)? {
        if Some(exec) != mark.run {
            for row in rt.llm_ledger(exec)? {
                add(&mut usage, &row);
            }
        }
    }
    Ok(usage)
}

/// The runs of any workflow that are newer than `newest`. A list reads the
/// newest run first. With no `newest`, every run is newer.
fn newer_than(rt: &SqliteRuntime, newest: Option<ExecutionId>) -> Result<Vec<ExecutionId>> {
    let mut filter = ExecutionFilter::new();
    let mut runs = Vec::new();
    loop {
        let page = rt.list_executions(&filter)?;
        for exec in page
            .executions
            .iter()
            .filter_map(autumn_harvest_sqlite::ExecutionSummary::execution_id)
        {
            if Some(exec) == newest {
                return Ok(runs);
            }
            runs.push(exec);
        }
        match page.next {
            Some(cursor) => filter.before = Some(cursor),
            None => return Ok(runs),
        }
    }
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
