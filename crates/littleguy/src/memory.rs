//! The memory of the assistant and its nightly consolidation.
//!
//! The assistant keeps notes in two memory blocks, `memory` and `user`. A
//! run reads them once, at its start, and edits them with the `memory`
//! tool. Each op that ends adds one episode to a log. A consolidation pass
//! reads the new episodes, asks the model for facts, and adds each fact to
//! the `memory` block.
//!
//! The notes and the log live in `memory.db` beside `lg.db`
//! (`littleguy-stores`), so the runtime stays the one writer of `lg.db`. An
//! engine schedule in `lg.db` starts the pass each night. The daemon tick
//! fires it, so a pass runs only while the daemon runs. After a gap the
//! schedule fires once, then counts on from then.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use autumn_harvest::policy::Schedule;
use autumn_harvest_agent::consolidate;
use autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME;
use autumn_harvest_agent::{
    AgentHarness, AgentMemory, AgentProfile, ConsolidateAll, EpisodeSettings,
};
use autumn_harvest_sqlite::{SqliteError, SqliteRuntime};
use littleguy_core::MemoryConfig;
use littleguy_stores::SqliteStores;

/// The file name of the memory store, beside the database.
pub const MEMORY_FILE: &str = "memory.db";

/// The memory store beside the database `db`.
#[must_use]
pub fn path_for(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(MEMORY_FILE)
}

/// Opens the memory store beside `db`.
///
/// # Errors
///
/// Returns an error when the file does not open.
pub fn open(db: &Path) -> Result<Arc<SqliteStores>> {
    let path = path_for(db);
    Ok(Arc::new(
        SqliteStores::open(&path).with_context(|| format!("opening {}", path.display()))?,
    ))
}

/// Gives the harness the stores, and the agents of `db` a directory, so the
/// consolidation dispatcher finds the assistant.
#[must_use]
pub fn harness(harness: AgentHarness, stores: Arc<SqliteStores>, db: &Path) -> AgentHarness {
    harness
        .memory(stores.clone())
        .episodes(stores)
        .directory(Arc::new(autumn_harvest_agent::sqlite::Directory::open(db)))
}

/// Keeps the memory of the assistant in the store, and an episode log that
/// the scheduled pass reads.
#[must_use]
pub fn profile(profile: AgentProfile) -> AgentProfile {
    profile
        .memory(AgentMemory::Store)
        .episodes(EpisodeSettings::default())
}

/// Puts the consolidation schedule in the runtime, or takes it out when
/// `config` keeps no memory.
///
/// # Errors
///
/// Returns an error when the schedule does not validate, for example a bad
/// cron field or an unknown time zone, or the runtime cannot write it.
pub fn schedule(rt: &mut SqliteRuntime, config: &MemoryConfig) -> Result<()> {
    if !config.enabled {
        return match rt.delete_schedule(CONSOLIDATE_ALL_WORKFLOW_NAME) {
            Ok(()) | Err(SqliteError::ScheduleNotFound(_)) => Ok(()),
            Err(error) => Err(error.into()),
        };
    }
    let when = Schedule::CronInTimezone {
        expr: config.consolidate.clone(),
        tz: config.timezone.clone(),
    };
    let schedule = consolidate::schedule(when, &ConsolidateAll::new())?;
    rt.upsert_schedule(&schedule).with_context(|| {
        format!(
            "memory.consolidate `{}` in `{}` is not a schedule",
            config.consolidate, config.timezone
        )
    })?;
    Ok(())
}
