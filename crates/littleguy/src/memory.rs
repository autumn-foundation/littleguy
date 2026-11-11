//! The memory of the assistant and its nightly consolidation.
//!
//! The assistant keeps notes in two memory blocks, `memory` and `user`. A
//! run reads them once, at its start, and edits them with the `memory`
//! tool. Each op that ends adds one episode to a log. A consolidation pass
//! reads the new episodes, asks the model for facts, and adds each fact to
//! the `memory` block.
//!
//! The notes and the log live in `memory.db` beside `lg.db`
//! (`littleguy-stores`), so the runtime stays the one writer of `lg.db`.
//! With a `[fleet]`, they live in the tenant database of the owner instead
//! (ADR 0017). The first start on a fleet moves the rows of `memory.db`
//! into it. An
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

/// The name that `memory.db` takes once its rows moved into a tenant
/// database. The file stays, so an owner can go back.
pub const IMPORTED_FILE: &str = "memory.db.imported";

/// Opens the memory store: in the `tenant` database of a fleet, or beside
/// `db`.
///
/// On a fleet, the rows of `memory.db` move into an empty tenant database
/// once. The old file then takes the name [`IMPORTED_FILE`].
///
/// # Errors
///
/// Returns an error when a file does not open, or the move fails.
pub fn open(db: &Path, tenant: Option<&Path>) -> Result<Arc<SqliteStores>> {
    let Some(tenant) = tenant else {
        let path = path_for(db);
        return Ok(Arc::new(
            SqliteStores::open(&path).with_context(|| format!("opening {}", path.display()))?,
        ));
    };
    let stores = SqliteStores::open_shared(tenant)
        .with_context(|| format!("opening the stores in {}", tenant.display()))?;
    let old = path_for(db);
    if old.exists() {
        if stores
            .import(&old)
            .with_context(|| format!("moving {} into the tenant database", old.display()))?
        {
            let imported = old.with_file_name(IMPORTED_FILE);
            std::fs::rename(&old, &imported)
                .with_context(|| format!("renaming {}", old.display()))?;
            tracing::info!(from = %old.display(), "the memory moved into the tenant database");
        } else {
            tracing::warn!(
                file = %old.display(),
                "the tenant database holds a memory already, so this file stays unread"
            );
        }
    }
    Ok(Arc::new(stores))
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A tenant file in WAL mode that another connection keeps open, as
    /// the fleet does.
    fn tenant(path: &Path) -> rusqlite::Connection {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.query_row("PRAGMA journal_mode = wal", [], |_| Ok(()))
            .unwrap();
        conn.execute_batch("CREATE TABLE keeper (x INTEGER)")
            .unwrap();
        conn
    }

    #[test]
    fn the_first_open_on_a_fleet_moves_memory_db_into_the_tenant() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        open(&db, None).unwrap().mark_sent("brief:1").unwrap();
        let file = dir.path().join("mark.db");
        let keeper = tenant(&file);

        let stores = open(&db, Some(&file)).unwrap();
        assert!(stores.was_sent("brief:1").unwrap());
        assert!(!path_for(&db).exists());
        assert!(dir.path().join(IMPORTED_FILE).exists());

        // A later open finds no memory.db, and keeps the rows.
        drop(stores);
        assert!(open(&db, Some(&file)).unwrap().was_sent("brief:1").unwrap());

        // A memory.db that comes back stays unread beside a full tenant.
        open(&db, None).unwrap().mark_sent("brief:2").unwrap();
        let stores = open(&db, Some(&file)).unwrap();
        assert!(!stores.was_sent("brief:2").unwrap());
        assert!(path_for(&db).exists());
        drop(keeper);
    }
}
