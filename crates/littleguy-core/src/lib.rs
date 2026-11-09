//! The core of littleguy: ids, configuration and the routing kernel.
//!
//! The crate holds no I/O. The binary crate reads the config file and the
//! sockets, then asks [`route`] where each inbound message goes.

pub mod config;
pub mod ids;
pub mod pairing;
pub mod route;

pub use config::{CamelRoute, Config, ConfigError, DiscordConfig, discord_sender};
pub use ids::{ChannelId, IdError, MessageId, OwnerName, SenderId};
pub use pairing::{PairingCode, Pending, PendingPairings};
pub use route::{ChannelRule, Decision, Inbound, Route, Routing, decide, route};

/// The agent kind of the assistant that the owner talks to.
pub const ASSISTANT_KIND: &str = "assistant";
