//! The tenant database of the owner, replicated (ADR 0017). Feature `fleet`.
//!
//! `lg serve` keeps the memory, the episode log and the approvals in
//! `<root>/<owner>.db`, and the replicator of autumn-web ships its WAL to the
//! replica. The replicator is the one checkpointer of the file. So the
//! stores open a short connection per call with the checkpoint off
//! (`SqliteStores::open_shared`).
//!
//! A new volume has no tenant file. With `restore_missing`, the file comes
//! back from the replica before the daemon opens it. Nothing fences another
//! host yet, so only one host may serve the owner (F5).
//!
//! The `DatabaseFleet` of autumn-web 0.10 needs its `sqlite` feature. That
//! feature changes the pool type of the whole app, and autumn-harvest-plugin
//! 0.10 then fails to build. So F1 runs the replicator of one file. F2 moves
//! to the fleet with autumn-harvest 0.11.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use autumn_web::config::{ReplicationConfig, ReplicationS3Config};
use autumn_web::replication::RestoreError;
use autumn_web::replication::restore::restore;
use autumn_web::time::{ClockSource, SystemClock};
use littleguy_core::{FleetConfig, OwnerName, Replica};
use littleguy_stores::SqliteStores;
use tokio_util::sync::CancellationToken;

/// The directory of the tenant databases when the config names none,
/// beside the database.
pub const FLEET_DIR: &str = "fleet";

/// The key prefix of each tenant replica. The owner name follows it.
const REPLICA_PREFIX: &str = "littleguy/tenants";

/// The open tenant database of the owner, and the thread that ships it.
#[derive(Debug)]
pub struct Tenant {
    path: PathBuf,
    stop: CancellationToken,
    shipper: Option<std::thread::JoinHandle<()>>,
}

impl Tenant {
    /// Opens the tenant database of `owner`. It lives in `config.root`, or
    /// in `fleet` beside `db`.
    ///
    /// # Errors
    ///
    /// Returns an error when the owner name is no tenant name, the replica
    /// is not usable, for example a missing key variable, or the file does
    /// not restore or open.
    pub async fn open(config: &FleetConfig, db: &Path, owner: &OwnerName) -> Result<Self> {
        let name = owner.as_str();
        if !is_tenant_name(name) {
            bail!(
                "owner `{name}` is not a tenant name: use lowercase letters, digits, `-` and `_`"
            );
        }
        let root = root_of(config, db);
        std::fs::create_dir_all(&root)
            .with_context(|| format!("creating the fleet {}", root.display()))?;
        let path = root.join(format!("{name}.db"));
        let settings = replication_config(config);
        let url = format!("sqlite://{}", path.display());
        let owner_name = name.to_owned();
        let restore_missing = config.restore_missing;
        let file = path.clone();
        // The S3 destination uses a blocking client, so all of it runs off
        // the async threads.
        let (mut replicator, restored) = tokio::task::spawn_blocking(move || {
            let clock: Arc<dyn ClockSource> = Arc::new(SystemClock);
            let runtime = autumn_web::replication::build(&settings, &url, &owner_name, None, clock)
                .map_err(|error| anyhow!("the fleet replica is not usable: {error}"))?;
            let mut restored = false;
            if restore_missing && !file.exists() {
                match restore(
                    runtime.replicator.destination().as_ref(),
                    &runtime.settings.root,
                    None,
                    &file,
                ) {
                    Ok(_) => restored = true,
                    Err(RestoreError::NoReplica { .. }) => {}
                    Err(error) => bail!("restoring {} failed: {error}", file.display()),
                }
            }
            SqliteStores::create_shared(&file)
                .with_context(|| format!("opening {}", file.display()))?;
            Ok::<_, anyhow::Error>((runtime.replicator, restored))
        })
        .await
        .context("opening the tenant database")??;
        if restored {
            tracing::info!(path = %path.display(), "the tenant database came back from the replica");
        }
        // The first tick opens the connection of the replicator. It holds
        // the WAL open from now on, so no close of a store is the last one.
        let first = tokio::task::spawn_blocking(move || {
            let tick = replicator.tick();
            (replicator, tick)
        })
        .await
        .context("the first ship")?;
        let (replicator, tick) = first;
        if let Err(error) = tick {
            tracing::warn!(%error, "the first ship failed; the replicator tries again");
        }
        let stop = CancellationToken::new();
        let token = stop.clone();
        let shipper = std::thread::Builder::new()
            .name(String::from("lg-tenant-replication"))
            .spawn(move || replicator.run(&token))
            .context("starting the replication thread")?;
        tracing::info!(path = %path.display(), "the tenant database is open");
        Ok(Self {
            path,
            stop,
            shipper: Some(shipper),
        })
    }

    /// The file of the tenant database.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops the replicator. It ships the last frames before it stops.
    pub async fn close(mut self) {
        self.stop.cancel();
        if let Some(shipper) = self.shipper.take() {
            let _ = tokio::task::spawn_blocking(move || shipper.join()).await;
        }
    }
}

/// Whether `name` is a tenant name: lowercase ASCII letters, digits, `-`
/// and `_`, starting with a letter or a digit. The fleet of autumn-web has
/// the same rule, so F2 keeps each file name.
fn is_tenant_name(name: &str) -> bool {
    name.len() <= 128
        && name
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

/// The directory of the tenant databases.
fn root_of(config: &FleetConfig, db: &Path) -> PathBuf {
    config.root.clone().unwrap_or_else(|| {
        db.parent()
            .unwrap_or_else(|| Path::new("."))
            .join(FLEET_DIR)
    })
}

/// The `[replication]` settings of autumn-web for `config`.
fn replication_config(config: &FleetConfig) -> ReplicationConfig {
    let mut settings = ReplicationConfig {
        enabled: true,
        rpo_secs: config.rpo_secs,
        prefix: Some(String::from(REPLICA_PREFIX)),
        ..ReplicationConfig::default()
    };
    match &config.replica {
        Replica::Path(path) => settings.path = Some(path.display().to_string()),
        Replica::S3(s3) => {
            settings.s3 = Some(ReplicationS3Config {
                bucket: Some(s3.bucket.clone()),
                region: s3.region.clone(),
                endpoint: s3.endpoint.clone(),
                access_key_id_env: Some(s3.access_key_id_env.clone()),
                secret_access_key_env: Some(s3.secret_access_key_env.clone()),
                force_path_style: s3.force_path_style,
            });
        }
    }
    settings
}

#[cfg(test)]
mod tests;
