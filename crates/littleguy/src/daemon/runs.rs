//! The runs of the assistant: a new run for each message, and the purge of
//! the runs that ended (ADR 0035).
//!
//! autumn-harvest 0.10 reads the profile of an agent once, at the start of
//! a run, and replay keeps that read (autumn-harvest #2658). So the
//! assistant ends its run after each op, at a checkpoint, and the next run
//! reads the profile of the harness that runs then. A run then holds the
//! history of one op, not of every op since the first.
//!
//! The next run starts at once after the checkpoint, and it reads the
//! profile before the next change. So after a start and after a reload,
//! the daemon sends one op that changes nothing: a `Resume` of an agent
//! that is not paused. That op ends the run too, and the run after it reads
//! the new profile. `Control::ReloadProfile` replaces it at the next engine
//! upgrade.
//!
//! An ended run stays in the database for `[runs] keep_days`, so the run log
//! tools can read it. A purge once a day deletes the older ones.

use std::time::Duration;

use anyhow::Result;
use autumn_harvest_agent::{AgentOp, Control, sqlite};
use autumn_harvest_sqlite::{PurgePolicy, PurgeReport};
use jiff::Timestamp;

use super::Daemon;

/// The ops of one run of the assistant.
pub(super) const OPS_PER_RUN: u64 = 1;

/// The time between two purges.
const PURGE_EVERY: jiff::SignedDuration = jiff::SignedDuration::from_hours(24);

/// The most batches of one purge call. A tick that does not finish goes on
/// at the next tick, so one tick never holds the write lock for long.
const PURGE_BATCHES: u32 = 4;

/// The seconds of one day.
const DAY_SECS: u64 = 24 * 60 * 60;

impl Daemon {
    /// Ends the run of the assistant when a start or a reload asked for
    /// it, so the next op reads the profile of the config that runs now.
    ///
    /// A call that waits for a decision holds the op of the run, so the end
    /// waits for the decision too.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot send the op or drive the
    /// run.
    pub(super) async fn roll(&mut self) -> Result<()> {
        if !self.roll_due || self.waiting.is_some() {
            return Ok(());
        }
        let id = self.routing.assistant().clone();
        // With no run, the first op starts one, and it reads the profile.
        if sqlite::current_run(&self.rt, &id)?.is_some() {
            self.rolls += 1;
            let key = format!("roll:{}:{}", (self.clock)().as_microsecond(), self.rolls);
            let op = AgentOp::Control(Control::Resume);
            sqlite::send(&mut self.rt, &id, &op, Some(&key))?;
            sqlite::drive(&mut self.rt, &id).await?;
        }
        self.roll_due = false;
        Ok(())
    }

    /// Deletes the runs that ended more than `[runs] keep_days` before
    /// `now`, once a day. A purge that stops at its batch limit goes on at
    /// the next call.
    ///
    /// # Errors
    ///
    /// Returns an error when a read or a delete of the database fails.
    pub(super) fn purge(&mut self, now: Timestamp) -> Result<Option<PurgeReport>> {
        if self.purged_at.is_some_and(|at| now - at < PURGE_EVERY) {
            return Ok(None);
        }
        let keep = Duration::from_secs(u64::from(self.kit.config.runs.keep_days) * DAY_SECS);
        let policy = PurgePolicy::new().max_age(keep).batch_limit(PURGE_BATCHES);
        let report = self.rt.purge_with_as_of(now, &policy)?;
        if report.complete {
            self.purged_at = Some(now);
        }
        if report.removed > 0 {
            tracing::info!(removed = report.removed, by_workflow = ?report.by_workflow, "the purge deleted the runs that ended long ago");
        }
        Ok(Some(report))
    }
}
