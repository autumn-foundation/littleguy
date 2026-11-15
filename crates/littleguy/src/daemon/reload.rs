//! A reload of `lg.toml` while the daemon runs (ADR 0018).
//!
//! The daemon keeps the parts of its start that no config holds: the
//! model, the sandbox runtime, the MCP tools and the open stores. A reload
//! builds the assistant again from those parts and the new config, then
//! puts the new harness in the runtime. The runtime replaces each workflow
//! and activity by its name, so the next op runs on the new harness. A
//! reload runs between two jobs of the job loop, so no op is in flight.
//!
//! The pure kernel `littleguy_core::reload::merge` decides which sections
//! apply now and which wait for a restart.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use autumn_harvest_agent::{AgentHarness, AgentModel, LiveSink, sqlite};
use chrono::Utc;
use littleguy_channels::Service;
use littleguy_core::reload::{Merge, merge};
use littleguy_core::{Config, SenderId};
use littleguy_stores::SqliteStores;

use super::{Daemon, harness, routing, warn_unreachable};
use crate::brief::BriefDelivery;
use crate::chats::Chats;
use crate::protocol::Response;
#[cfg(feature = "heartbeat")]
use crate::wake::Due;
use crate::wake::Wake;

/// What a reload needs that the config does not hold, and the config that
/// runs.
pub(super) struct Kit {
    /// The runtime database.
    pub(super) db: PathBuf,
    /// The model of the assistant.
    pub(super) model: Arc<dyn AgentModel>,
    /// The runtime of the `run_code` tool, started at start.
    pub(super) sandbox: Option<crate::sandbox::Runtime>,
    /// The tools of the MCP servers, started at start.
    pub(super) mcp: Vec<Arc<dyn autumn_harvest_agent::Tool>>,
    /// The Google API of the owner.
    #[cfg(feature = "google")]
    pub(super) google: Option<Arc<dyn littleguy_tools::google::GoogleApi>>,
    /// The tenant database of a fleet, which holds the stores.
    pub(super) tenant: Option<PathBuf>,
    /// The chat services that take messages.
    pub(super) services: Vec<Service>,
    /// The memory store, once a part needs it. It opens once.
    pub(super) stores: Option<Arc<SqliteStores>>,
    /// The config that runs.
    pub(super) config: Config,
    /// The config file that a reload reads. `None` refuses a reload.
    pub(super) file: Option<PathBuf>,
    /// How often the daemon looks at the config file.
    pub(super) watch_every: Duration,
    /// The readers of live answers. Every harness sends its text there.
    pub(super) live: Arc<super::live::Live>,
    /// Whether the wake plan armed a brief. The brief delivery reads it.
    pub(super) brief_armed: Arc<std::sync::atomic::AtomicBool>,
    /// What the wake tools share with the daemon.
    pub(super) wake_links: crate::plans::WakeLinks,
    /// What the precheck decided for the last heartbeat, for the tick log.
    #[cfg(feature = "heartbeat")]
    pub(super) verdict: Arc<crate::wake::Verdict>,
}

/// The parts of the daemon that a config shapes.
pub(super) struct Assembled {
    pub(super) harness: AgentHarness,
    pub(super) wake: Option<Wake>,
    pub(super) approvals: Option<Arc<SqliteStores>>,
    pub(super) approver: Option<SenderId>,
    #[cfg(feature = "refine")]
    pub(super) refine: Option<Box<crate::refine::Refine>>,
}

impl Kit {
    /// The memory store. It opens at the first call.
    fn stores(&mut self) -> Result<Arc<SqliteStores>> {
        if let Some(stores) = &self.stores {
            return Ok(Arc::clone(stores));
        }
        let stores = crate::memory::open(&self.db, self.tenant.as_deref())?;
        self.stores = Some(Arc::clone(&stores));
        Ok(stores)
    }

    /// Builds the assistant and the parts around it from `config`.
    pub(super) fn assemble(&mut self, config: &Config, chats: &Arc<Chats>) -> Result<Assembled> {
        let db = self.db.clone();
        let wake = Wake::new(
            config.brief.as_ref(),
            config.heartbeat.as_ref(),
            Arc::clone(&self.brief_armed),
            Utc::now(),
        )?;
        let runtime =
            crate::sandbox::share(self.sandbox.as_ref().filter(|_| config.sandbox.is_some()));
        #[cfg(feature = "google")]
        let google = crate::tools::google_tools(config.google.as_ref(), self.google.clone());
        #[cfg(not(feature = "google"))]
        let google: Vec<Arc<dyn autumn_harvest_agent::Tool>> = Vec::new();
        let google_on = !google.is_empty();
        let mut tools = crate::tools::Tools::new(
            &config.tools,
            &db,
            crate::tools::search_key_from_env().as_deref(),
        )
        .with(self.mcp.clone())
        .with(google);
        // The goals and ideas live in the memory store.
        if config.tools.plans {
            tools = tools.with(crate::plans::tools(&self.stores()?, &self.wake_links));
        }
        tracing::info!(tools = ?tools.names(), "the assistant tools");
        let flow = config.flow.rules();
        let asks = crate::guard::Guard::asks(&flow, tools.all(), runtime.is_some() || google_on);
        // The brief keeps its sent reports in the memory store, and the
        // guard keeps its calls that wait. So each opens the store even
        // when memory is off.
        let stores =
            if config.memory.enabled || config.brief.is_some() || asks || config.tools.plans {
                Some(self.stores()?)
            } else {
                None
            };
        // The configured block sizes, for new scopes and the stored ones.
        if let Some(stores) = &stores {
            stores.set_limits(&[
                ("memory", config.memory.memory_chars),
                ("user", config.memory.user_chars),
            ])?;
        }
        let delivery = config
            .brief
            .as_ref()
            .zip(stores.as_ref())
            .map(|(settings, stores)| {
                Arc::new(BriefDelivery::new(
                    settings,
                    Arc::clone(stores),
                    Arc::clone(chats),
                    Arc::clone(&self.brief_armed),
                ))
            });
        let approvals = stores.clone().filter(|_| asks);
        let guard = approvals.clone().map(|stores| crate::guard::Guard {
            stores,
            timeout: Duration::from_secs(config.approvals.timeout_secs),
            flow,
            sandbox: runtime,
            google: google_on,
        });
        let approver = config.approvals.approver.clone();
        warn_unreachable(approver.as_ref(), &self.services);
        // A heartbeat tick asks first whether a goal is due or an idea waits.
        #[cfg(feature = "heartbeat")]
        let precheck = config.heartbeat.as_ref().map(|beat| Due {
            plans: stores.clone().filter(|_| config.tools.plans),
            research: beat.research,
            verdict: Arc::clone(&self.verdict),
        });
        let memory = stores
            .filter(|_| config.memory.enabled)
            .map(|stores| (stores, db.as_path()));
        let model = Arc::clone(&self.model);
        let assistant = harness(config, Arc::clone(&model), memory, delivery, tools, guard);
        #[cfg(feature = "refine")]
        let (assistant, refine) = super::refine::attach(assistant, config, &db, model)?;
        #[cfg(not(feature = "refine"))]
        let _ = model;
        let assistant = assistant.live_sink(Arc::clone(&self.live) as Arc<dyn LiveSink>);
        #[cfg(feature = "heartbeat")]
        let assistant = match precheck {
            Some(due) => assistant.precheck(Arc::new(due)),
            None => assistant,
        };
        Ok(Assembled {
            harness: assistant,
            wake,
            approvals,
            approver,
            #[cfg(feature = "refine")]
            refine,
        })
    }
}

/// The senders that the config allows, which an unpair keeps.
pub(super) fn config_allowed(config: &Config) -> BTreeSet<SenderId> {
    config
        .discord
        .allowed
        .iter()
        .chain(&config.slack.allowed)
        .cloned()
        .collect()
}

/// The answer to a reload.
pub(super) fn reloaded(merge: &Merge) -> Response {
    let names = |list: &[&str]| list.iter().map(|name| (*name).to_owned()).collect();
    Response::Reloaded {
        applied: names(&merge.applied),
        restart: names(&merge.restart),
        profile: names(&merge.profile),
    }
}

impl Daemon {
    /// The config that runs.
    #[cfg(test)]
    #[must_use]
    pub const fn config(&self) -> &Config {
        &self.kit.config
    }

    /// Sets the config file that a reload reads.
    #[must_use]
    pub fn with_config_file(mut self, file: PathBuf) -> Self {
        self.kit.file = Some(file);
        self
    }

    /// Looks at the config file every `every`, for a test.
    #[cfg(all(test, feature = "camel"))]
    #[must_use]
    pub const fn with_watch_every(mut self, every: Duration) -> Self {
        self.kit.watch_every = every;
        self
    }

    /// The config file that a reload reads, if any.
    #[must_use]
    pub fn config_file(&self) -> Option<&Path> {
        self.kit.file.as_deref()
    }

    /// Reads the config file again, and applies it.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon has no config file, the file does
    /// not parse, or the new parts do not build. The old config then runs
    /// on.
    pub fn reload_file(&mut self) -> Result<Merge> {
        let file = self
            .kit
            .file
            .clone()
            .context("this daemon runs without a config file, so it cannot reload one")?;
        let next = Config::load(&file).with_context(|| format!("reading {}", file.display()))?;
        self.reload(next)
    }

    /// Applies `next`. Each live section takes its new value. Each section
    /// that starts something at start keeps its old value until a restart.
    ///
    /// # Errors
    ///
    /// Returns an error when the new parts do not build, for example a bad
    /// brief schedule. Nothing changes then.
    pub fn reload(&mut self, next: Config) -> Result<Merge> {
        let merge = merge(&self.kit.config, next);
        if merge.is_empty() {
            if !merge.restart.is_empty() {
                tracing::warn!(restart = ?merge.restart, "these sections apply at the next start");
            }
            return Ok(merge);
        }
        let config = merge.config.clone();
        let built = self.kit.assemble(&config, &self.chats)?;
        let table = routing(&config, self.owner_uid, &self.store, &self.kit.services)?;
        crate::memory::schedule(&mut self.rt, &config.memory)?;
        sqlite::register(&mut self.rt, Arc::new(built.harness));
        self.routing = table;
        self.config_allowed = config_allowed(&config);
        // A plan that did not change keeps its arm, so a wake that went out
        // does not go out again. A plan that went away turns the timer off.
        if config.brief != self.kit.config.brief || config.heartbeat != self.kit.config.heartbeat {
            // A reflection that waits for its arm survives the new plan.
            let pending = self
                .wake
                .as_ref()
                .and_then(Wake::pending_reflection)
                .map(str::to_owned);
            self.wake = built.wake;
            if let Some(pass) = pending {
                let brief_armed = Arc::clone(&self.kit.brief_armed);
                self.wake
                    .get_or_insert_with(|| Wake::idle(brief_armed))
                    .reflect(&pass);
            }
            self.disarm = self.wake.is_none();
            // The new plan reads the booked wakes again.
            self.kit
                .wake_links
                .changed
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        self.approvals = built.approvals;
        self.approver = built.approver;
        #[cfg(feature = "refine")]
        {
            self.refine = built.refine;
        }
        self.kit.config = config;
        // The refine loop or the skills folder may have changed.
        self.skills_stamp = None;
        tracing::info!(applied = ?merge.applied, profile = ?merge.profile, restart = ?merge.restart, "the config reloaded");
        Ok(merge)
    }
}

/// How often the daemon looks at its config file.
pub(super) const WATCH_EVERY: Duration = Duration::from_secs(2);

/// The change stamp of `file`: its modification time and its length.
fn stamp(file: &Path) -> Option<(std::time::SystemTime, u64)> {
    let meta = std::fs::metadata(file).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

/// Watches `file` every `every`, and asks the job loop for a reload when
/// it changes. A file that goes away changes nothing, and a file that
/// comes back reloads. It ends when the job loop ends.
pub(super) async fn watch(
    file: PathBuf,
    every: Duration,
    jobs: tokio::sync::mpsc::WeakSender<super::Job>,
    uid: u32,
) {
    let mut seen = stamp(&file);
    let mut ticker = tokio::time::interval(every);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let now = stamp(&file);
        if now == seen {
            continue;
        }
        seen = now;
        if now.is_none() {
            continue;
        }
        let Some(tx) = jobs.upgrade() else { return };
        let (reply, answer) = tokio::sync::oneshot::channel();
        let job = super::Job::Socket {
            uid,
            request: crate::protocol::Request::Reload,
            reply,
        };
        if tx.send(job).await.is_err() {
            return;
        }
        drop(tx);
        if answer.await.is_err() {
            return;
        }
    }
}

/// Runs a reload for the job loop: the daemon first, then the camel
/// routes. Only the owner may.
#[cfg_attr(
    not(feature = "camel"),
    expect(
        clippy::unused_async,
        clippy::needless_pass_by_ref_mut,
        reason = "the camel routes await and change the context, and the job loop has one signature"
    )
)]
pub(super) async fn reload_job(
    daemon: &mut Daemon,
    camel: &mut Option<crate::camel::Camel>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    jobs: &tokio::sync::mpsc::WeakSender<super::Job>,
    uid: u32,
) -> Response {
    if uid != daemon.owner_uid {
        tracing::warn!(uid, "refused a reload");
        return Response::Error {
            message: String::from("only the owner of the daemon may do that"),
        };
    }
    #[cfg(feature = "camel")]
    {
        // The routes are checked first, so a bad route changes nothing.
        let next = daemon
            .kit
            .file
            .as_deref()
            .map(Config::load)
            .transpose()
            .ok()
            .flatten();
        if let Some(next) = next
            && let Err(error) = crate::camel::check_routes(&next.camel)
        {
            return Response::Error {
                message: format!("{error:#}"),
            };
        }
    }
    let merge = match daemon.reload_file() {
        Ok(merge) => merge,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "the reload failed");
            return Response::Error {
                message: format!("{error:#}"),
            };
        }
    };
    #[cfg(feature = "camel")]
    if merge.applied.contains(&"camel")
        && let Err(error) = reload_camel(
            daemon.camel_mapper(&daemon.kit.config),
            daemon.kit.db.clone(),
            camel,
            tasks,
            jobs,
        )
        .await
    {
        return Response::Error {
            message: format!("the config applied, but the camel routes did not: {error:#}"),
        };
    }
    #[cfg(not(feature = "camel"))]
    let _ = (camel, tasks, jobs);
    reloaded(&merge)
}

/// Runs the camel routes of `mapper`. The first route starts camel, with
/// its dead letters in `db`. The daemon is not `Sync`, so this future
/// holds no borrow of it.
#[cfg(feature = "camel")]
async fn reload_camel(
    mapper: crate::camel::Mapper,
    db: PathBuf,
    camel: &mut Option<crate::camel::Camel>,
    tasks: &mut Vec<tokio::task::JoinHandle<()>>,
    jobs: &tokio::sync::mpsc::WeakSender<super::Job>,
) -> Result<()> {
    if let Some(running) = camel {
        return running.reload(mapper).await;
    }
    if mapper.routes.is_empty() {
        return Ok(());
    }
    let started = crate::camel::Camel::start(mapper, &db).await?;
    if let Some(tx) = jobs.upgrade() {
        let pump = crate::camel::pump(started.inbound(), tx, super::Job::Camel);
        tasks.push(tokio::spawn(pump));
    }
    *camel = Some(started);
    Ok(())
}

#[cfg(test)]
mod tests;
