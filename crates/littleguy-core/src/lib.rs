//! The core of littleguy: ids, configuration and the routing kernel.
//!
//! The crate holds no I/O. The binary crate reads the config file and the
//! sockets, then asks [`route`] where each inbound message goes.

pub mod config;
pub mod ids;
pub mod route;

pub use config::{Config, ConfigError};
pub use ids::{ChannelId, IdError, MessageId, OwnerName, SenderId};
pub use route::{ChannelRule, Decision, Inbound, Route, Routing, decide, route};

/// The agent kind of the assistant that the owner talks to.
pub const ASSISTANT_KIND: &str = "assistant";
