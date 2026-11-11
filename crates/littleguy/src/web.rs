//! `lg serve` as an autumn-web app (M5.2, ADR 0014).
//!
//! autumn-web serves the pages, owns the signals and the graceful shutdown,
//! and runs the job loop of the daemon as a task. The pages are maud markup
//! with htmx. They reach the daemon through a [`Handle`], so each action is
//! one owner request, as `lg` sends over the socket.
//!
//! The engine runtime stays in the job loop. `HarvestPlugin` cannot run the
//! durable agent on SQLite yet, so the app does not mount it.

pub mod hooks;
pub mod logins;
mod pages;
#[cfg(feature = "slack")]
pub mod slack;

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use autumn_web::app::AppBuilder;
use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader};
use autumn_web::config::{LogConfig, TelemetryConfig};
use autumn_web::telemetry::{TelemetryGuard, TelemetryInitError, TelemetryProvider};
use std::net::SocketAddr;

use littleguy_core::Config;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub use hooks::HookToken;
pub use logins::Logins;
#[cfg(feature = "slack")]
pub use slack::SlackHook;

use crate::daemon::Handle;

/// The session cookie of the pages.
pub const COOKIE: &str = "lg.sid";

/// What the routes need from the daemon.
#[derive(Debug, Clone)]
pub struct Web {
    /// The way into the job loop, as the owner.
    pub handle: Handle,
    /// The sign-in links.
    pub logins: Logins,
    /// The bearer token of the hooks, when the hooks serve.
    pub hooks: Option<HookToken>,
    /// The Slack hooks, when Slack runs over the Events API.
    #[cfg(feature = "slack")]
    pub slack: Option<SlackHook>,
}

/// Where the app serves, and what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Plan {
    /// The address of the app.
    pub bind: SocketAddr,
    /// Whether the pages serve. They serve only on a loopback address.
    pub pages: bool,
    /// Whether the hooks serve.
    pub hooks: bool,
}

impl Plan {
    /// The plan of `config`. With `[hooks]`, the app serves on the hooks
    /// address, and the pages serve only when that address is loopback.
    /// Without it, the pages serve on the ui address.
    #[must_use]
    pub fn of(config: &Config) -> Self {
        config.hooks.as_ref().map_or(
            Self {
                bind: config.ui.bind,
                pages: true,
                hooks: false,
            },
            |hooks| Self {
                bind: hooks.bind,
                pages: hooks.bind.ip().is_loopback(),
                hooks: true,
            },
        )
    }
}

/// The base address of the app at `bind`.
#[must_use]
pub fn base_url(bind: SocketAddr) -> String {
    format!("http://{bind}")
}

/// The autumn-web settings of the app at `plan`.
///
/// Every mutating request needs the CSRF token, except a hook. A hook
/// comes from a machine, with a bearer token instead. The session cookie
/// is HTTP-only and same-site strict. It is not `Secure`, because the
/// pages serve plain HTTP on a loopback address only.
///
/// No load balancer sits in front, so shutdown skips the pre-stop grace.
/// The routes are private, so the answer-engine documents, such as
/// `llms.txt`, are off.
#[must_use]
pub fn settings(plan: &Plan) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.server.host = plan.bind.ip().to_string();
    config.server.port = plan.bind.port();
    config.server.prestop_grace_secs = 0;
    config.aeo.enabled = false;
    config.security.csrf.enabled = true;
    config.security.csrf.exempt_paths = vec![String::from("/hooks")];
    config.session.cookie_name = String::from(COOKIE);
    config.session.http_only = true;
    config.session.secure = false;
    config.session.same_site = String::from("Strict");
    config
}

/// A config loader that gives one fixed config. lg reads `lg.toml`, never
/// `autumn.toml`.
struct Fixed(AutumnConfig);

impl ConfigLoader for Fixed {
    fn load(&self) -> impl Future<Output = Result<AutumnConfig, ConfigError>> + Send {
        let config = self.0.clone();
        async move { Ok(config) }
    }
}

/// A telemetry provider that installs nothing. `lg` sets up its own
/// subscriber before the app starts, with `LG_LOG`.
struct Quiet;

impl TelemetryProvider for Quiet {
    fn init(
        &self,
        _log: &LogConfig,
        _telemetry: &TelemetryConfig,
        _profile: Option<&str>,
    ) -> Result<TelemetryGuard, TelemetryInitError> {
        Ok(TelemetryGuard::disabled())
    }
}

/// The routes of `plan`.
#[must_use]
pub fn routes(plan: &Plan) -> Vec<autumn_web::Route> {
    let mut routes = Vec::new();
    if plan.pages {
        routes.extend(pages::routes());
    }
    if plan.hooks {
        routes.extend(hooks::routes());
        #[cfg(feature = "slack")]
        routes.extend(slack::routes());
    }
    routes
}

/// The app of `plan`, with no lifecycle hooks. Tests build it too.
#[must_use]
pub fn app(web: Web, plan: &Plan) -> AppBuilder {
    autumn_web::app()
        .with_config_loader(Fixed(settings(plan)))
        .with_telemetry_provider(Quiet)
        .routes(routes(plan))
        .state_initializer(move |state| state.insert_extension(web))
}

/// Runs `daemon` as a task of the app, and the app until a signal stops
/// it. The app stops the daemon first, so the runtime closes cleanly.
///
/// # Errors
///
/// Returns an error when the job loop fails.
pub async fn serve<F>(web: Web, plan: &Plan, daemon: F) -> anyhow::Result<()>
where
    F: FnOnce(Arc<Notify>) -> JoinHandle<anyhow::Result<()>>,
{
    let stop = Arc::new(Notify::new());
    let task = Arc::new(Mutex::new(Some(daemon(Arc::clone(&stop)))));
    let (hook_stop, hook_task) = (Arc::clone(&stop), Arc::clone(&task));
    if plan.pages {
        tracing::info!(url = %base_url(plan.bind), "the pages serve; run `lg ui` to sign in");
    }
    if plan.hooks {
        tracing::info!(url = %base_url(plan.bind), "the hooks serve");
    }
    Box::pin(
        app(web, plan)
            .on_shutdown(move || {
                let stop = Arc::clone(&hook_stop);
                let task = Arc::clone(&hook_task);
                async move {
                    stop.notify_one();
                    let running = task.lock().unwrap_or_else(PoisonError::into_inner).take();
                    if let Some(running) = running {
                        finish(running).await;
                    }
                }
            })
            .run(),
    )
    .await;
    // The app can also end without its hooks, as when no route serves.
    stop.notify_one();
    let running = task.lock().unwrap_or_else(PoisonError::into_inner).take();
    match running {
        Some(running) => running.await?,
        None => Ok(()),
    }
}

/// Waits for the job loop, and logs how it ended.
async fn finish(running: JoinHandle<anyhow::Result<()>>) {
    match running.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(error = %format!("{error:#}"), "the daemon failed"),
        Err(error) => tracing::error!(%error, "the daemon task ended early"),
    }
}

#[cfg(test)]
mod tests;
