//! The models that littleguy can talk to.
//!
//! Each model implements [`AgentModel`](autumn_harvest_agent::AgentModel).
//! Milestone 0 ships the [`OfflineModel`] only. It needs no key and no
//! network, so the daemon and its tests run anywhere.

pub mod offline;

pub use offline::OfflineModel;
