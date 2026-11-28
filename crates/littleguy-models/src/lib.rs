//! The models that littleguy can talk to.
//!
//! Each model implements [`AgentModel`](autumn_harvest_agent::AgentModel).
//!
//! - [`OfflineModel`] needs no key and no network, so the daemon and its
//!   tests run anywhere.
//! - `ClaudeModel` talks to Claude over the Messages API. Feature
//!   `claude`.
//! - `OpenAiModel` talks to OpenAI, OpenRouter, or any server that speaks
//!   the Chat Completions API. Feature `openai`.
//! - `OpenAiEmbedder` embeds texts over the embeddings API of the same
//!   servers, for the memory search (ADR 0047). Feature `openai`.

#[cfg(feature = "claude")]
pub mod claude;
#[cfg(feature = "openai")]
pub mod embeddings;
#[cfg(any(feature = "claude", feature = "openai"))]
mod http;
pub mod offline;
#[cfg(feature = "openai")]
pub mod openai;
#[cfg(feature = "claude")]
mod turn_store;

#[cfg(feature = "claude")]
pub use claude::ClaudeModel;
#[cfg(feature = "openai")]
pub use embeddings::OpenAiEmbedder;
pub use offline::OfflineModel;
#[cfg(feature = "openai")]
pub use openai::{OpenAiModel, Provider, Reasoning};
