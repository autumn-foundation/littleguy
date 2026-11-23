//! The owner's side of saved scripts (ADR 0046): list them with the run
//! log, accept or decline a draft, or retire a script.
//!
//! Each request touches only `memory.db` and the scripts folder. So it
//! answers at once, and the listing also answers beside the loop.

use anyhow::Result;

use super::Daemon;
use crate::protocol::{Request, Response};

/// Why scripts are off in this daemon: the build or the config lacks them.
fn off_reason() -> String {
    if crate::features::has("scripts") {
        String::from("scripts need a [scripts] section and a [sandbox] section")
    } else {
        crate::features::missing("scripts", "scripts")
    }
}

/// The answer to `request` when scripts are off: a listing is off, and an
/// action fails.
pub(super) fn off(request: &Request) -> Response {
    let message = off_reason();
    if matches!(request, Request::Scripts) {
        Response::Off { message }
    } else {
        Response::Error { message }
    }
}

/// The owner's listing, beside the loop, so a page never waits for a run.
#[cfg(feature = "scripts")]
pub(super) fn listing_of(scripts: Option<&crate::scripts::Scripts>) -> Result<Response> {
    scripts.map_or_else(
        || Ok(off(&Request::Scripts)),
        |scripts| Ok(scripts.listing()?),
    )
}

/// Handles one scripts request of the owner with `scripts`. Each change
/// answers with the listing after it.
#[cfg(feature = "scripts")]
fn answer(scripts: &crate::scripts::Scripts, request: Request) -> Result<Response> {
    let done = match request {
        Request::AcceptScript { id, digest } => scripts.accept(id, &digest).map(|_| ()),
        Request::DeclineScript { id } => scripts.decline(id),
        Request::RetireScript { name } => scripts.retire(&name),
        _ => Ok(()),
    };
    match done {
        Ok(()) => Ok(scripts.listing()?),
        Err(message) => Ok(Response::Error { message }),
    }
}

impl Daemon {
    /// Handles one scripts request of the owner.
    #[cfg(feature = "scripts")]
    pub(super) fn scripts_request(&self, request: Request) -> Result<Response> {
        match self.kit.scripts.as_deref() {
            Some(scripts) => answer(scripts, request),
            None => Ok(off(&request)),
        }
    }

    /// A build without scripts: the listing is off, and an action fails.
    #[cfg(not(feature = "scripts"))]
    #[expect(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        clippy::unnecessary_wraps,
        reason = "the scripts build reads the daemon, takes the request and can fail"
    )]
    pub(super) fn scripts_request(&self, request: Request) -> Result<Response> {
        Ok(off(&request))
    }

    /// Sets the scripts folder that a config with no `[scripts] dir` uses:
    /// `scripts/` next to the config file.
    #[cfg(feature = "scripts")]
    #[must_use]
    pub fn with_scripts_dir(mut self, dir: std::path::PathBuf) -> Self {
        self.kit.scripts_default = Some(dir);
        self.kit.set_scripts_dir();
        self.publish();
        self
    }
}
