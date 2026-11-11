//! The core of littleguy: ids, configuration and the routing kernel.
//!
//! The crate holds no I/O. The binary crate reads the config file and the
//! sockets, then asks [`route`] where each inbound message goes.

pub mod addr;
pub mod config;
pub mod ids;
pub mod pairing;
pub mod route;

pub use config::{
    ApprovalsConfig, BriefConfig, CamelRoute, ChatConfig, Config, ConfigError, DiscordConfig,
    FirecrackerConfig, FleetConfig, FlowConfig, FlowRecheck, GoogleConfig, HooksConfig,
    Integration, McpServer, McpTransport, MemoryConfig, RefineConfig, RefineDimension, Replica,
    S3Replica, SanMateoConfig, SanMateoError, SandboxBackend, SandboxConfig, SlackConfig,
    ToolsConfig, UiConfig, chat_user, discord_sender, slack_sender,
};
pub use ids::{ChannelId, IdError, MessageId, OwnerName, SenderId};
pub use pairing::{PairingCode, Pending, PendingPairings};
pub use route::{ChannelRule, Decision, Inbound, Route, Routing, decide, route};

/// The agent kind of the assistant that the owner talks to.
pub const ASSISTANT_KIND: &str = "assistant";
