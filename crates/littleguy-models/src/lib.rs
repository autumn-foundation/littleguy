//! The models that littleguy can talk to.
//!
//! Each model implements [`AgentModel`](autumn_harvest_agent::AgentModel).
//!
//! - [`OfflineModel`] needs no key and no network, so the daemon and its
//!   tests run anywhere.
//! - [`ClaudeModel`] talks to Claude over the Messages API.

pub mod claude;
pub mod offline;
mod turn_store;

pub use claude::ClaudeModel;
pub use offline::OfflineModel;
