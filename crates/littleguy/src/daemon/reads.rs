//! The reads that answer beside the loop of the daemon.
//!
//! The loop handles one job at a time, and a chat holds it until its run
//! ends. A page that only reads then waited for the whole run. Everything
//! that a read needs is durable, so it does not need the loop.
//!
//! So the daemon publishes a [`View`] after each job: shared handles on
//! the stores, the config, and the parts that live in memory, such as the
//! pairs. A [`Reads`] keeps the newest view. The web and the socket ask it
//! first, and a read never waits for a run. A write and a chat still go
//! through the loop.
//!
//! The state of the assistant comes from the runtime database. A reader
//! opens it read only, as the run log tools do, and takes the newest state
//! snapshot that the engine committed. While an op runs, that is the state
//! before the op, as the runtime query gives it.

use std::path::PathBuf;
use std::sync::{Arc, PoisonError, RwLock};

use anyhow::{Context, Result};
use autumn_harvest_agent::{AgentId, AgentState, WorkspaceMount};
use littleguy_core::Config;
use littleguy_stores::SqliteStores;
use littleguy_stores::runtime_state::{EntityState, entity_state};

use crate::protocol::{Request, Response, WorkspaceEntry};

/// Everything that a read needs, as the daemon had it after its last job.
#[derive(Clone)]
pub struct View {
    pub(super) owner_uid: u32,
    pub(super) config: Arc<Config>,
    pub(super) stores: Option<Arc<SqliteStores>>,
    pub(super) approvals: Option<Arc<SqliteStores>>,
    /// The runtime database, for the state of the assistant.
    pub(super) db: PathBuf,
    pub(super) assistant: AgentId,
    /// The pairs, from the routing in memory.
    pub(super) pairs: Response,
    #[cfg(feature = "refine")]
    pub(super) refine: Option<Arc<crate::refine::Refine>>,
    #[cfg(feature = "skills")]
    pub(super) skills_dir: Option<PathBuf>,
    #[cfg(feature = "oauth")]
    pub(super) connections: Option<crate::oauth::Lister>,
    #[cfg(feature = "patches")]
    pub(super) patches: Option<Arc<crate::patches::Patches>>,
}

impl std::fmt::Debug for View {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("View")
            .field("assistant", &self.assistant)
            .finish_non_exhaustive()
    }
}

/// The newest [`View`] of the daemon.
#[derive(Debug, Default)]
pub struct Reads {
    now: RwLock<Option<Arc<View>>>,
}

impl Reads {
    /// Keeps `view` as the newest.
    pub(super) fn publish(&self, view: View) {
        *self.now.write().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(view));
    }

    /// The answer to `request` from `uid`, when it is a read that needs no
    /// loop. `None` sends it through the loop.
    pub async fn answer(&self, uid: u32, request: &Request) -> Option<Response> {
        let view = self
            .now
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()?;
        // Only the owner reads. The loop refuses anyone else.
        if uid != view.owner_uid {
            return None;
        }
        let answer = view.answer(request).await?;
        Some(answer.unwrap_or_else(|error| {
            tracing::error!(error = %format!("{error:#}"), "a read failed");
            Response::Error {
                message: format!("{error:#}"),
            }
        }))
    }
}

impl View {
    /// The answer to `request`, when it is a read. `None` for any other
    /// request, and for a state that the snapshot cannot give.
    pub(super) async fn answer(&self, request: &Request) -> Option<Result<Response>> {
        Some(match request {
            Request::History => {
                let state = self.state()?;
                state.map(|state| Response::History {
                    turns: state.map_or_else(Vec::new, |state| super::turns(&state.transcript)),
                })
            }
            Request::Workspace => {
                let state = self.state()?;
                match state {
                    Ok(state) => self.workspace_files(state).await,
                    Err(error) => Err(error),
                }
            }
            Request::WorkspaceFile { path } => {
                let state = self.state()?;
                match state {
                    Ok(state) => self.workspace_file(state, path.clone()).await,
                    Err(error) => Err(error),
                }
            }
            #[cfg(feature = "history")]
            Request::Conversations => {
                super::history::listing(&self.config, &self.db, &self.assistant)
            }
            #[cfg(feature = "history")]
            Request::Conversation { id } => {
                super::history::conversation(&self.db, &self.assistant, *id)
            }
            #[cfg(not(feature = "history"))]
            Request::Conversations => Ok(Response::Off {
                message: crate::features::missing("past conversations", "history"),
            }),
            #[cfg(not(feature = "history"))]
            Request::Conversation { .. } => Ok(Response::Error {
                message: crate::features::missing("past conversations", "history"),
            }),
            Request::Approvals => super::gate::approval_list(self.approvals.as_deref()),
            Request::Notes => super::notes_listing(&self.config, self.stores.as_deref()),
            #[cfg(feature = "plans")]
            Request::Plans => super::plans::listing_of(&self.config, self.stores.as_deref()),
            #[cfg(feature = "skills")]
            Request::Skills => super::skills::listing_of(
                self.skills_dir.as_deref(),
                self.refine.is_some(),
                self.stores.as_deref(),
            ),
            #[cfg(feature = "refine")]
            Request::Overlays | Request::OverlayShow { .. } => {
                super::refine::read(self.refine.as_deref(), request.clone()).await
            }
            #[cfg(feature = "oauth")]
            Request::Connections => crate::oauth::listing(self.connections.as_ref()).await,
            #[cfg(not(feature = "plans"))]
            Request::Plans => Ok(Response::Off {
                message: crate::features::missing("goals and ideas", "plans"),
            }),
            #[cfg(not(feature = "skills"))]
            Request::Skills => Ok(Response::Off {
                message: crate::features::missing("skills", "skills"),
            }),
            #[cfg(not(feature = "refine"))]
            Request::Overlays => Ok(Response::Off {
                message: crate::features::missing("lg refine", "refine"),
            }),
            #[cfg(not(feature = "refine"))]
            Request::OverlayShow { .. } => Ok(Response::Error {
                message: crate::features::missing("lg refine", "refine"),
            }),
            #[cfg(not(feature = "oauth"))]
            Request::Connections => Ok(Response::Off {
                message: crate::features::missing("an OAuth connection", "oauth"),
            }),
            #[cfg(feature = "patches")]
            Request::Patches => super::patches::listing_of(self.patches.as_deref()),
            #[cfg(not(feature = "patches"))]
            Request::Patches => Ok(super::patches::off(request)),
            Request::Pairs => Ok(self.pairs.clone()),
            _ => return None,
        })
    }

    /// The committed state of the assistant, or `None` with no run. The
    /// outer `None` means that the snapshot cannot give it, so the loop
    /// reads it.
    fn state(&self) -> Option<Result<Option<AgentState>>> {
        match committed_state(&self.db, &self.assistant) {
            Ok(Committed::NoRun) => Some(Ok(None)),
            Ok(Committed::State(state)) => Some(Ok(Some(*state))),
            Ok(Committed::Unknown) => None,
            Err(error) => Some(Err(error)),
        }
    }

    /// The workspace of the assistant at `state`.
    async fn workspace(
        &self,
        state: Option<AgentState>,
    ) -> Result<autumn_harvest_agent::Workspace> {
        let store = crate::tools::workspace_store(crate::tools::workspace_for(&self.db));
        let mount = WorkspaceMount {
            scope: WorkspaceMount::agent_scope(self.assistant.as_str()),
            head: state.and_then(|state| state.workspace),
            generation: 0,
        };
        Ok(store.load(&mount).await?)
    }

    async fn workspace_files(&self, state: Option<AgentState>) -> Result<Response> {
        let files = self
            .workspace(state)
            .await?
            .files()
            .iter()
            .map(|(path, file)| WorkspaceEntry {
                path: path.clone(),
                bytes: u64::try_from(file.data.len()).unwrap_or(u64::MAX),
            })
            .collect();
        Ok(Response::Workspace { files })
    }

    async fn workspace_file(&self, state: Option<AgentState>, path: String) -> Result<Response> {
        let workspace = self.workspace(state).await?;
        let Some(file) = workspace.get(&path) else {
            return Ok(Response::Error {
                message: format!("the workspace has no file {path}"),
            });
        };
        Ok(Response::WorkspaceFile {
            bytes: u64::try_from(file.data.len()).unwrap_or(u64::MAX),
            text: super::shown_text(&file.data),
            path,
        })
    }
}

/// What the runtime database holds as the state of the assistant.
#[derive(Debug)]
pub(super) enum Committed {
    /// The assistant has no run.
    NoRun,
    /// The newest committed state.
    State(Box<AgentState>),
    /// The run has no snapshot that a reader can use, so the loop reads it.
    Unknown,
}

/// The newest committed state of agent `id` in the runtime database `db`.
/// The engine records a snapshot of the state at the start of each run
/// and after each op (`autumn_harvest::entity`), and the newest one is the
/// state that the runtime query gives.
///
/// # Errors
///
/// Returns an error when the file cannot be read or a snapshot does not
/// decode.
pub(super) fn committed_state(db: &std::path::Path, id: &AgentId) -> Result<Committed> {
    if !db.exists() {
        return Ok(Committed::NoRun);
    }
    let read = entity_state(
        db,
        autumn_harvest_agent::AGENT_WORKFLOW_NAME,
        id.as_str(),
        autumn_harvest::entity::ENTITY_STATE_SIDE_EFFECT,
    )?;
    let value = match read {
        EntityState::NoRun => return Ok(Committed::NoRun),
        EntityState::Unknown => return Ok(Committed::Unknown),
        EntityState::Snapshot(value) => value,
    };
    let Some(state) = value.get("state").filter(|state| state.is_object()) else {
        return Ok(Committed::Unknown);
    };
    let state = serde_json::from_value(state.clone()).context("decoding a state snapshot")?;
    Ok(Committed::State(Box::new(state)))
}
