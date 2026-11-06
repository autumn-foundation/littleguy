//! The models that littleguy can talk to.
//!
//! Each model implements [`AgentModel`](autumn_harvest_agent::AgentModel).
//!
//! - [`OfflineModel`] needs no key and no network, so the daemon and its
//!   tests run anywhere.
//! - [`ClaudeModel`] talks to Claude over the Messages API.
//! - [`OpenAiModel`] talks to OpenAI, OpenRouter, or any server that speaks
//!   the Chat Completions API.

pub mod claude;
mod http;
pub mod offline;
pub mod openai;
mod turn_store;

pub use claude::ClaudeModel;
pub use offline::OfflineModel;
pub use openai::{OpenAiModel, Provider};
