//! The memory of the assistant and its nightly consolidation.
//!
//! The assistant keeps notes in two memory blocks, `memory` and `user`. A
//! run reads them once, at its start, and edits them with the `memory`
//! tool. Each op that ends adds one episode to a log. A consolidation pass
//! reads the new episodes, asks the model for facts, and adds each fact to
//! the `memory` block.
//!
//! The notes and the log live in the AletheiaDB directory
//! `memory.aletheia` beside `lg.db`, with the `aletheia` feature (ADR
//! 0047). The first start there moves the memory rows of `memory.db` into
//! it. `[memory] backend = "sqlite"` keeps them in `memory.db`
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
use littleguy_core::{Config, MemoryBackend, MemoryConfig};
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

/// The store of the memory blocks and the episode log.
#[derive(Debug, Clone)]
pub enum Stores {
    /// `memory.db`, or the tenant database of a fleet.
    Sqlite(Arc<SqliteStores>),
    /// The AletheiaDB directory `memory.aletheia` (ADR 0047).
    #[cfg(feature = "aletheia")]
    Aletheia(Arc<littleguy_stores::aletheia::AletheiaStores>),
}

/// The store that `config` asks for. With no `backend`, a build with the
/// `aletheia` feature takes AletheiaDB, unless a `[fleet]` keeps the memory
/// in its tenant database (ADR 0017). A build without it takes SQLite.
#[must_use]
pub fn backend(config: &Config) -> MemoryBackend {
    config
        .memory
        .backend
        .unwrap_or(if cfg!(feature = "aletheia") && config.fleet.is_none() {
            MemoryBackend::Aletheia
        } else {
            MemoryBackend::Sqlite
        })
}

/// Gives the harness the stores, and the agents of `db` a directory, so the
/// consolidation dispatcher finds the assistant.
#[must_use]
pub fn harness(harness: AgentHarness, stores: Stores, db: &Path) -> AgentHarness {
    let harness = match stores {
        Stores::Sqlite(stores) => harness.memory(stores.clone()).episodes(stores),
        #[cfg(feature = "aletheia")]
        Stores::Aletheia(stores) => harness.memory(stores.clone()).episodes(stores),
    };
    harness.directory(Arc::new(autumn_harvest_agent::sqlite::Directory::open(db)))
}

/// The embedder of `config`: the endpoint of `[memory.embeddings]`, or the
/// local hashing embedder. `key` reads the bearer token by its name.
///
/// # Errors
///
/// Returns an error when the section names an endpoint and this build has
/// no `openai` feature, or the house key that it names is not set.
#[cfg(feature = "aletheia")]
pub fn embedder(
    config: &MemoryConfig,
    key: impl Fn(&str) -> Option<String>,
) -> Result<Arc<dyn littleguy_core::embed::Embedder>> {
    let Some(section) = &config.embeddings else {
        return Ok(Arc::new(littleguy_core::embed::HashingEmbedder::default()));
    };
    let token = match &section.key_env {
        Some(name) => Some(key(name).filter(|value| !value.trim().is_empty()).with_context(|| {
            format!("memory.embeddings.key_env names {name}, and neither the house keys nor the environment set it")
        })?),
        None => None,
    };
    #[cfg(feature = "openai")]
    {
        Ok(Arc::new(littleguy_models::OpenAiEmbedder::new(
            section.url.clone(),
            section.model.clone(),
            section.dimensions,
            token,
        )))
    }
    #[cfg(not(feature = "openai"))]
    {
        let _ = token;
        anyhow::bail!(crate::features::missing("[memory.embeddings]", "openai"))
    }
}

/// Opens the AletheiaDB store beside `db`. The first start moves the memory
/// rows of `memory.db` into it once, and the rows stay in `memory.db`. Each
/// start then embeds the facts and episodes that wait for an embedding.
///
/// # Errors
///
/// Returns an error when another process holds the directory, it holds
/// vectors of other dimensions, or the move fails.
#[cfg(feature = "aletheia")]
pub async fn open_aletheia(
    db: &Path,
    config: &MemoryConfig,
    key: impl Fn(&str) -> Option<String>,
) -> Result<Arc<littleguy_stores::aletheia::AletheiaStores>> {
    use littleguy_stores::aletheia::{self, AletheiaStores};
    let dir = aletheia::path_for(db);
    let stores = AletheiaStores::open(&dir, embedder(config, key)?)
        .await
        .with_context(|| format!("opening {}", dir.display()))?;
    if !stores.imported().await? {
        let old = path_for(db);
        if old.exists() {
            let rows = SqliteStores::open(&old)
                .with_context(|| format!("opening {}", old.display()))?
                .memory_rows()?;
            if stores.import(rows).await? {
                tracing::info!(from = %old.display(), to = %dir.display(), "the memory moved into AletheiaDB");
            }
        } else {
            stores.mark_imported().await?;
        }
    }
    match stores.embed_nodes(false).await {
        Ok(0) => {}
        Ok(count) => tracing::info!(count, "embedded the facts and episodes that waited"),
        Err(error) => tracing::warn!(%error, "the embeddings wait for the next start"),
    }
    Ok(Arc::new(stores))
}

/// Keeps the memory of the assistant in the store, and an episode log that
/// the scheduled pass reads.
#[must_use]
pub fn profile(profile: AgentProfile, config: &MemoryConfig) -> AgentProfile {
    let mut episodes = EpisodeSettings::default();
    // A batch that fits the output cap of a pass (ADR 0030).
    episodes.max_pass_episodes = config.pass_episodes;
    // A pass goes with no cap unless `[memory] pass_tokens` sets one (ADR
    // 0034). The engine takes the cap as a setting since 0.11.
    episodes.max_pass_output_tokens = config.pass_tokens;
    profile.memory(AgentMemory::Store).episodes(episodes)
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

    #[cfg(feature = "aletheia")]
    #[tokio::test]
    async fn the_first_start_moves_memory_db_into_aletheia_once() {
        use autumn_harvest_agent::memory::{EditKey, MemoryOp, MemoryScope, MemoryStore};
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let scope = MemoryScope::new("agent:assistant/mark");
        let add = |text: &str| MemoryOp::Add {
            block: "memory".into(),
            text: text.into(),
        };
        {
            let old = open(&db, None).unwrap();
            old.apply(&scope, add("Mark likes tea"), &EditKey::new("k1"))
                .await
                .unwrap();
        }
        let config = MemoryConfig::default();
        let entries = |blocks: Vec<autumn_harvest_agent::memory::MemoryBlock>| {
            blocks
                .into_iter()
                .find(|b| b.label == "memory")
                .map(|b| b.entries)
                .unwrap_or_default()
        };
        {
            let stores = open_aletheia(&db, &config, |_| None).await.unwrap();
            let blocks = MemoryStore::load(&*stores, &scope).await.unwrap();
            assert_eq!(entries(blocks), vec![String::from("Mark likes tea")]);
            stores
                .apply(&scope, add("Mark lives in Porto"), &EditKey::new("k2"))
                .await
                .unwrap();
        }
        // A row that memory.db gains later does not move: the move ran once.
        {
            let old = open(&db, None).unwrap();
            old.apply(&scope, add("stale"), &EditKey::new("k3"))
                .await
                .unwrap();
        }
        let stores = open_aletheia(&db, &config, |_| None).await.unwrap();
        let blocks = MemoryStore::load(&*stores, &scope).await.unwrap();
        assert_eq!(
            entries(blocks),
            vec![
                String::from("Mark likes tea"),
                String::from("Mark lives in Porto")
            ]
        );
        assert!(
            path_for(&db).exists(),
            "memory.db stays, so an owner can go back"
        );
    }

    #[cfg(feature = "aletheia")]
    #[test]
    fn the_embedder_follows_the_config() {
        let local = embedder(&MemoryConfig::default(), |_| None).unwrap();
        assert_eq!(local.name(), littleguy_core::embed::HASHING_NAME);
        let mut config = MemoryConfig::default();
        config.embeddings = Some(littleguy_core::EmbeddingsConfig {
            url: "https://api.openai.com/v1".into(),
            model: "text-embedding-3-small".into(),
            dimensions: 1536,
            key_env: Some("OPENAI_API_KEY".into()),
        });
        let missing = embedder(&config, |_| None).unwrap_err().to_string();
        assert!(missing.contains("OPENAI_API_KEY"), "{missing}");
        let remote = embedder(&config, |_| Some("sk-test".into()));
        if cfg!(feature = "openai") {
            let remote = remote.unwrap();
            assert_eq!(remote.name(), "text-embedding-3-small");
            assert_eq!(remote.dimensions(), 1536);
        } else {
            assert!(remote.unwrap_err().to_string().contains("openai"));
        }
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
