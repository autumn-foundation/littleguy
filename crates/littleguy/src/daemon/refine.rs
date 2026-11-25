//! The refine requests of the daemon (ADR 0013).
//!
//! With a `[refine]` section, the assistant harness gets an overlay store.
//! The assistant then reads the active overlay at the start of each op.
//! Every request here is the owner's. A proposal never activates itself.

use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use anyhow::Result;
use autumn_harvest_agent::{AgentHarness, AgentModel, AgentState, sqlite};
use autumn_harvest_sqlite::SqliteResult;
use littleguy_core::Config;

use super::Daemon;
use crate::client::show_version;
use crate::protocol::{Request, Response};
use crate::refine::{Proposal, Refine, Swap, path_for};

/// The text for a refine request when the config has no `[refine]`.
const OFF: &str = "the refine loop is off: add a [refine] section to the config";

/// `assistant` with the overlay store of `config`, and the refine state.
/// With no `[refine]` section, both come back unchanged.
///
/// # Errors
///
/// Returns an error when the overlay store does not open.
pub(super) fn attach(
    assistant: AgentHarness,
    config: &Config,
    db: &Path,
    model: Arc<dyn AgentModel>,
) -> Result<(AgentHarness, Option<Arc<Refine>>)> {
    let Some(settings) = &config.refine else {
        return Ok((assistant, None));
    };
    let refine = Refine::open(&path_for(db), settings, &assistant, model)?;
    Ok((
        assistant.overlay_store(refine.store()),
        Some(Arc::new(refine)),
    ))
}

impl Daemon {
    /// Runs one refine request of the owner.
    ///
    /// The state query starts here, so the future holds no borrow of the
    /// runtime. Only a proposal reads its answer.
    pub(super) fn refine_request(
        &self,
        request: Request,
    ) -> impl Future<Output = Result<Response>> + Send + '_ {
        let query = sqlite::state(&self.rt, self.routing.assistant());
        let owner = self.routing.assistant().name().to_owned();
        answer(self.refine.as_deref(), owner, query, request)
    }
}

/// The answer to a read of the overlays: the listing or one version. It
/// needs no state of the assistant.
///
/// # Errors
///
/// Returns an error when the store fails.
pub(super) async fn read(refine: Option<&Refine>, request: Request) -> Result<Response> {
    let no_state = std::future::ready(Ok(None));
    answer(refine, String::new(), no_state, request).await
}

/// The answer to `request`, over the refine state `refine`.
///
/// # Errors
///
/// Returns an error when the runtime, the model or the store fails.
async fn answer(
    refine: Option<&Refine>,
    owner: String,
    query: impl Future<Output = SqliteResult<Option<AgentState>>> + Send,
    request: Request,
) -> Result<Response> {
    let Some(refine) = refine else {
        let message = String::from(OFF);
        // The listing is off. An action on an overlay fails.
        return Ok(if matches!(request, Request::Overlays) {
            Response::Off { message }
        } else {
            Response::Error { message }
        });
    };
    let transcript = if matches!(request, Request::RefinePropose) {
        query
            .await?
            .map(|state| state.transcript)
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    Ok(match request {
        Request::RefinePropose => match refine.propose(&transcript).await? {
            Proposal::Stored { version, overlay } => Response::Proposed {
                version: version.get(),
                overlay: serde_json::to_string_pretty(&overlay)?,
            },
            Proposal::Kept => Response::Kept,
        },
        Request::Overlays => {
            let (pointer, entries) = refine.list().await?;
            Response::Overlays {
                active: pointer.active,
                head: pointer.head,
                entries,
            }
        }
        Request::OverlayShow { version } => match refine.show(version).await? {
            Some((entry, overlay)) => Response::Overlay { entry, overlay },
            None => Response::Error {
                message: format!("no overlay version {version}"),
            },
        },
        Request::OverlayPut { overlay } => Response::OverlayStored {
            version: refine.put(&overlay, &owner).await?,
        },
        Request::OverlayActivate { version, expected } => {
            let swap = refine.activate(version, expected, &owner).await?;
            activated(version, expected.is_some(), swap)
        }
        Request::OverlayRollback => {
            let back = refine.rollback(&owner).await?;
            Response::OverlayActive {
                version: back.to.get(),
                previous: Some(back.from.get()),
                guarded: false,
            }
        }
        other => Response::Error {
            message: format!("not a refine request: {other:?}"),
        },
    })
}

/// The answer to an activation of `version` that ended in `swap`.
/// `guarded` says whether the request sent `expected`.
fn activated(version: u64, guarded: bool, swap: Swap) -> Response {
    match swap {
        Swap::Moved { previous } => Response::OverlayActive {
            version,
            previous,
            guarded,
        },
        Swap::Refused { active, head } if guarded => Response::OverlayMoved {
            version,
            active,
            head,
        },
        // An unguarded request reads the pointer and swaps in one job, and
        // the loop runs one job at a time. Only a store that another
        // process opened can move the pointer in between. An older caller
        // does not know `overlay_moved`, so it gets an error.
        Swap::Refused { active, .. } => Response::Error {
            message: format!(
                "the active overlay moved to {} meanwhile, so version {version} is not active",
                show_version(active)
            ),
        },
    }
}

#[cfg(test)]
mod tests {
    use autumn_harvest_agent::{
        AgentError, BoxFuture, ChatRequest, ChatResponse, ChatRole, ContentPart, KEEP_BASE,
        StopReason, TokenUsage,
    };
    use littleguy_core::{OwnerName, RefineConfig};

    use super::*;
    use crate::protocol::ExpectedPointer;

    const UID: u32 = 1000;

    const PIRATE: &str = "Talk like a pirate.";

    /// A model that writes a pirate overlay when the proposer asks, and
    /// otherwise answers with the system prompt that it got.
    #[derive(Debug)]
    struct Mirror;

    impl AgentModel for Mirror {
        fn chat<'a>(
            &'a self,
            request: &'a ChatRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
            Box::pin(async move {
                let system: String = request
                    .messages
                    .iter()
                    .filter(|message| message.role == ChatRole::System)
                    .flat_map(|message| &message.content)
                    .filter_map(|part| match part {
                        ContentPart::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                let text = if system.contains(KEEP_BASE) {
                    format!("{{\"system\": \"{PIRATE}\"}}")
                } else {
                    format!("system: {system}")
                };
                Ok(ChatResponse {
                    content: vec![ContentPart::Text(text)],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::new(1, 1),
                })
            })
        }
    }

    fn open(dir: &Path, refine: bool) -> Daemon {
        let mut config = Config::for_owner(OwnerName::new("mark").unwrap());
        config.refine = refine.then(RefineConfig::default);
        Daemon::open(&dir.join("lg.db"), &config, Arc::new(Mirror), UID, false).unwrap()
    }

    async fn say(daemon: &mut Daemon, id: &str) -> String {
        let request = Request::Chat {
            message_id: String::from(id),
            text: String::from("hello"),
        };
        match daemon.handle_request(UID, request).await {
            Response::Reply { text, .. } => text,
            _ => panic!("expected a reply"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_activated_proposal_changes_the_next_answer() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        let before = say(&mut daemon, "m-1").await;
        assert!(!before.contains(PIRATE), "the config prompt runs first");

        let proposed = daemon.handle_request(UID, Request::RefinePropose).await;
        let Response::Proposed { version, overlay } = proposed else {
            panic!("expected a proposal");
        };
        assert_eq!(version, 1);
        assert!(overlay.contains(PIRATE), "the proposal holds the overlay");
        let waiting = say(&mut daemon, "m-2").await;
        assert!(!waiting.contains(PIRATE), "a proposal waits");

        let active = daemon
            .handle_request(
                UID,
                Request::OverlayActivate {
                    version: 1,
                    expected: None,
                },
            )
            .await;
        assert_eq!(
            active,
            Response::OverlayActive {
                version: 1,
                previous: None,
                guarded: false,
            }
        );
        let after = say(&mut daemon, "m-3").await;
        assert!(after.contains(PIRATE), "the active overlay runs");
        assert!(dir.path().join(crate::refine::FILE_NAME).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_owner_rolls_back_to_the_last_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        let pirate = format!("{{\"system\": \"{PIRATE}\"}}");
        for overlay in [pirate, String::from("{}")] {
            let stored = daemon
                .handle_request(UID, Request::OverlayPut { overlay })
                .await;
            assert!(
                matches!(stored, Response::OverlayStored { .. }),
                "the put stores a version"
            );
        }
        for version in [1, 2] {
            daemon
                .handle_request(
                    UID,
                    Request::OverlayActivate {
                        version,
                        expected: None,
                    },
                )
                .await;
        }
        let plain = say(&mut daemon, "m-1").await;
        assert!(!plain.contains(PIRATE), "version 2 is the config prompt");
        let back = daemon.handle_request(UID, Request::OverlayRollback).await;
        assert_eq!(
            back,
            Response::OverlayActive {
                version: 1,
                previous: Some(2),
                guarded: false,
            }
        );
        let pirate = say(&mut daemon, "m-2").await;
        assert!(pirate.contains(PIRATE), "the rollback restores version 1");
        let Response::Overlays {
            active, entries, ..
        } = daemon.handle_request(UID, Request::Overlays).await
        else {
            panic!("expected the overlay list");
        };
        assert_eq!(active, Some(1));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].author, "operator:mark");
        let shown = daemon
            .handle_request(UID, Request::OverlayShow { version: 1 })
            .await;
        assert!(matches!(shown, Response::Overlay { .. }), "version 1 shows");
        let missing = daemon
            .handle_request(UID, Request::OverlayShow { version: 7 })
            .await;
        assert!(
            matches!(missing, Response::Error { .. }),
            "version 7 is missing"
        );
    }

    /// Stores `count` overlays as versions 1 to `count`.
    async fn put_versions(daemon: &mut Daemon, count: u64) {
        for _ in 0..count {
            let stored = daemon
                .handle_request(
                    UID,
                    Request::OverlayPut {
                        overlay: String::from("{}"),
                    },
                )
                .await;
            assert!(
                matches!(stored, Response::OverlayStored { .. }),
                "{stored:?}"
            );
        }
    }

    async fn activate(
        daemon: &mut Daemon,
        version: u64,
        expected: Option<ExpectedPointer>,
    ) -> Response {
        daemon
            .handle_request(UID, Request::OverlayActivate { version, expected })
            .await
    }

    /// The pointer as `overlays` names it, ready to pass back.
    async fn pointer(daemon: &mut Daemon) -> ExpectedPointer {
        match daemon.handle_request(UID, Request::Overlays).await {
            Response::Overlays { active, head, .. } => ExpectedPointer { active, head },
            other => panic!("expected the overlay list, got {other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_activation_moves_from_the_pointer_it_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        put_versions(&mut daemon, 3).await;

        // Before any move, the pointer has no version and no head.
        let fresh = pointer(&mut daemon).await;
        assert_eq!(
            fresh,
            ExpectedPointer {
                active: None,
                head: None
            }
        );
        assert_eq!(
            activate(&mut daemon, 1, Some(fresh)).await,
            Response::OverlayActive {
                version: 1,
                previous: None,
                guarded: true,
            }
        );
        let read = pointer(&mut daemon).await;
        assert_eq!(read.active, Some(1));
        assert!(read.head.is_some(), "the move set the head");
        assert_eq!(
            activate(&mut daemon, 2, Some(read)).await,
            Response::OverlayActive {
                version: 2,
                previous: Some(1),
                guarded: true,
            }
        );
        // The active version again, from a fresh read, is no move.
        let read = pointer(&mut daemon).await;
        assert_eq!(
            activate(&mut daemon, 2, Some(read)).await,
            Response::OverlayActive {
                version: 2,
                previous: Some(2),
                guarded: true,
            }
        );
        assert_eq!(pointer(&mut daemon).await, read, "nothing moved");
        // An unguarded request says so.
        assert_eq!(
            activate(&mut daemon, 3, None).await,
            Response::OverlayActive {
                version: 3,
                previous: Some(2),
                guarded: false,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_after_the_read_refuses_the_activation() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        put_versions(&mut daemon, 3).await;
        activate(&mut daemon, 1, None).await;
        let read = pointer(&mut daemon).await;

        // A person activates v2 on the web pages after the caller read v1.
        activate(&mut daemon, 2, None).await;
        let now = pointer(&mut daemon).await;
        assert_eq!(
            activate(&mut daemon, 3, Some(read)).await,
            Response::OverlayMoved {
                version: 3,
                active: Some(2),
                head: now.head,
            }
        );
        assert_eq!(pointer(&mut daemon).await, now, "the person's move holds");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_move_away_and_back_refuses_the_activation() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        put_versions(&mut daemon, 3).await;
        activate(&mut daemon, 1, None).await;
        let read = pointer(&mut daemon).await;

        // A person tries v2, then rolls back to v1: the version the caller
        // read is active again, but the person chose it after the read.
        activate(&mut daemon, 2, None).await;
        daemon.handle_request(UID, Request::OverlayRollback).await;
        let now = pointer(&mut daemon).await;
        assert_eq!(now.active, read.active, "the same version is active");
        assert_ne!(now.head, read.head, "the head moved twice");
        assert_eq!(
            activate(&mut daemon, 3, Some(read)).await,
            Response::OverlayMoved {
                version: 3,
                active: Some(1),
                head: now.head,
            }
        );
        assert_eq!(pointer(&mut daemon).await, now, "the rollback holds");
        // A caller that reads again may go ahead.
        assert_eq!(
            activate(&mut daemon, 3, Some(now)).await,
            Response::OverlayActive {
                version: 3,
                previous: Some(1),
                guarded: true,
            }
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_guarded_activation_still_checks_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        put_versions(&mut daemon, 1).await;
        let fresh = pointer(&mut daemon).await;
        // An unknown version is an error, whatever the caller read.
        let unknown = activate(&mut daemon, 9, Some(fresh)).await;
        assert!(matches!(unknown, Response::Error { .. }), "{unknown:?}");
        // Version 0 never exists, so no caller read it as active.
        let zero = ExpectedPointer {
            active: Some(0),
            head: None,
        };
        let zero = activate(&mut daemon, 1, Some(zero)).await;
        assert!(matches!(zero, Response::Error { .. }), "{zero:?}");
        assert_eq!(pointer(&mut daemon).await, fresh, "nothing moved");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn with_refine_off_a_guarded_activation_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), false);
        let fresh = ExpectedPointer {
            active: None,
            head: None,
        };
        let off = activate(&mut daemon, 1, Some(fresh)).await;
        assert!(matches!(off, Response::Error { .. }), "{off:?}");
    }

    #[test]
    fn a_refusal_is_overlay_moved_only_for_a_guarded_request() {
        let refused = Swap::Refused {
            active: Some(2),
            head: Some(5),
        };
        assert_eq!(
            activated(3, true, refused),
            Response::OverlayMoved {
                version: 3,
                active: Some(2),
                head: Some(5),
            }
        );
        let Response::Error { message } = activated(3, false, refused) else {
            panic!("an unguarded refusal is an error");
        };
        assert_eq!(
            message,
            "the active overlay moved to v2 meanwhile, so version 3 is not active"
        );
        let Response::Error { message } = activated(
            3,
            false,
            Swap::Refused {
                active: None,
                head: None,
            },
        ) else {
            panic!("an unguarded refusal is an error");
        };
        assert!(message.contains("moved to none"), "{message}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_proposal_with_no_transcript_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(dir.path(), true);
        let response = daemon.handle_request(UID, Request::RefinePropose).await;
        let Response::Error { message } = response else {
            panic!("expected an error");
        };
        assert!(
            message.contains("no transcript"),
            "the error names the cause"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refine_is_off_without_its_section_and_only_for_the_owner() {
        let dir = tempfile::tempdir().unwrap();
        let mut off = open(dir.path(), false);
        // The listing is off, not broken. An action fails.
        assert_eq!(
            off.handle_request(UID, Request::Overlays).await,
            Response::Off {
                message: String::from(OFF)
            }
        );
        assert_eq!(
            off.handle_request(UID, Request::OverlayRollback).await,
            Response::Error {
                message: String::from(OFF)
            }
        );
        assert!(!dir.path().join(crate::refine::FILE_NAME).exists());
        let other = tempfile::tempdir().unwrap();
        let mut on = open(other.path(), true);
        let refused = on.handle_request(UID + 1, Request::RefinePropose).await;
        assert_eq!(
            refused,
            Response::Error {
                message: String::from("only the owner of the daemon may do that")
            }
        );
    }
}
