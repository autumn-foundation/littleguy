//! `lg serve` as an autumn-web app (M5.2, ADR 0014).
//!
//! autumn-web serves the pages, owns the signals and the graceful shutdown,
//! and runs the job loop of the daemon as a task. The pages are maud markup
//! with htmx. They reach the daemon through a [`Handle`], so each action is
//! one owner request, as `lg` sends over the socket.
//!
//! The engine runtime stays in the job loop. `HarvestPlugin` cannot run the
//! durable agent on SQLite yet, so the app does not mount it.

pub mod logins;
mod pages;

use std::future::Future;
use std::sync::{Arc, Mutex, PoisonError};

use autumn_web::app::AppBuilder;
use autumn_web::config::{AutumnConfig, ConfigError, ConfigLoader};
use autumn_web::config::{LogConfig, TelemetryConfig};
use autumn_web::telemetry::{TelemetryGuard, TelemetryInitError, TelemetryProvider};
use littleguy_core::UiConfig;
use tokio::sync::Notify;
use tokio::task::JoinHandle;

pub use logins::Logins;

use crate::daemon::Handle;

/// The session cookie of the pages.
pub const COOKIE: &str = "lg.sid";

/// What the pages need from the daemon.
#[derive(Debug, Clone)]
pub struct Web {
    /// The way into the job loop, as the owner.
    pub handle: Handle,
    /// The sign-in links.
    pub logins: Logins,
}

/// The base address of the pages at `ui`.
#[must_use]
pub fn base_url(ui: &UiConfig) -> String {
    format!("http://{}", ui.bind)
}

/// The autumn-web settings of the pages at `ui`.
///
/// Every mutating request needs the CSRF token. The session cookie is
/// HTTP-only and same-site strict. It is not `Secure`, because the pages
/// serve plain HTTP on a loopback address.
///
/// No load balancer sits in front, so shutdown skips the pre-stop grace.
/// The pages are private, so the answer-engine documents, such as
/// `llms.txt`, are off.
#[must_use]
pub fn settings(ui: &UiConfig) -> AutumnConfig {
    let mut config = AutumnConfig::default();
    config.server.host = ui.bind.ip().to_string();
    config.server.port = ui.bind.port();
    config.server.prestop_grace_secs = 0;
    config.aeo.enabled = false;
    config.security.csrf.enabled = true;
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

/// The app of the pages, with no lifecycle hooks. Tests build it too.
#[must_use]
pub fn app(web: Web, ui: &UiConfig) -> AppBuilder {
    autumn_web::app()
        .with_config_loader(Fixed(settings(ui)))
        .with_telemetry_provider(Quiet)
        .routes(pages::routes())
        .state_initializer(move |state| state.insert_extension(web))
}

/// Runs `daemon` as a task of the app, and the app until a signal stops
/// it. The app stops the daemon first, so the runtime closes cleanly.
///
/// # Errors
///
/// Returns an error when the job loop fails.
pub async fn serve<F>(web: Web, ui: &UiConfig, daemon: F) -> anyhow::Result<()>
where
    F: FnOnce(Arc<Notify>) -> JoinHandle<anyhow::Result<()>>,
{
    let stop = Arc::new(Notify::new());
    let task = Arc::new(Mutex::new(Some(daemon(Arc::clone(&stop)))));
    let (hook_stop, hook_task) = (Arc::clone(&stop), Arc::clone(&task));
    tracing::info!(url = %base_url(ui), "the pages serve; run `lg ui` to sign in");
    Box::pin(
        app(web, ui)
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
