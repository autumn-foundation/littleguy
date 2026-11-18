//! The owner's side of patches (ADR 0042): list them, open one as a draft
//! PR, or decline one.
//!
//! An open sends GitHub calls through the San Mateo proxy, so it waits in
//! the daemon loop like any other owner request. Each step records its
//! result, so a second press after a failure resumes.

use anyhow::Result;

use super::Daemon;
use crate::protocol::{Request, Response};

/// Why patches are off in this daemon: the build or the config lacks them.
fn off_reason() -> String {
    if crate::features::has("patches") {
        String::from(
            "patches need a [patches] section with a San Mateo integration that holds a GitHub \
             token",
        )
    } else {
        crate::features::missing("patches", "patches")
    }
}

/// The answer to `request` when patches are off: a listing is off, and an
/// action fails.
pub(super) fn off(request: &Request) -> Response {
    let message = off_reason();
    if matches!(request, Request::Patches) {
        Response::Off { message }
    } else {
        Response::Error { message }
    }
}

/// The owner's listing, beside the loop, so a page never waits for a run.
#[cfg(feature = "patches")]
pub(super) fn listing_of(patches: Option<&crate::patches::Patches>) -> Result<Response> {
    patches.map_or_else(
        || Ok(off(&Request::Patches)),
        |patches| Ok(patches.listing()?),
    )
}

impl Daemon {
    /// Handles one patches request of the owner.
    #[cfg(feature = "patches")]
    ///
    /// The future owns its patches handle, so it holds no borrow of the
    /// daemon across an await.
    pub(super) fn patches_request(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response>> + use<> {
        let patches = self.kit.patches.clone();
        async move {
            let Some(patches) = patches else {
                return Ok(off(&request));
            };
            let refused = |message: String| Ok(Response::Error { message });
            match request {
                Request::OpenPatch { id, digest } => match patches.open(id, &digest).await {
                    Ok(pr) => Ok(Response::PatchOpened {
                        id,
                        pr,
                        url: patches.github.pull_url(pr),
                    }),
                    Err(message) => refused(message),
                },
                Request::DeclinePatch { id, reason } => {
                    match patches.decline(id, reason.as_deref()) {
                        Ok(()) => Ok(patches.listing()?),
                        Err(message) => refused(message),
                    }
                }
                _ => Ok(patches.listing()?),
            }
        }
    }

    /// A build without patches: the listing is off, and an action fails.
    #[cfg(not(feature = "patches"))]
    #[expect(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        reason = "the patches build reads the daemon and takes the request"
    )]
    pub(super) fn patches_request(&self, request: Request) -> std::future::Ready<Result<Response>> {
        std::future::ready(Ok(off(&request)))
    }
}
