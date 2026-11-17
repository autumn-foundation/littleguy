//! The core of littleguy: ids, configuration and the routing kernel.
//!
//! The crate holds no I/O. The binary crate reads the config file and the
//! sockets, then asks [`route`] where each inbound message goes.

pub mod addr;
pub mod config;
pub mod house_keys;
pub mod ids;
pub mod pairing;
pub mod reload;
pub mod route;

pub use config::{
    ApiConfig, ApprovalsConfig, BriefConfig, CamelRoute, ChatConfig, Config, ConfigError,
    CouncilAccepts, CouncilConfig, DiscordConfig, FirecrackerConfig, FleetConfig, FlowConfig,
    FlowRecheck, GoogleConfig, HeartbeatConfig, HeartbeatDelivery, HooksConfig, Integration,
    McpServer, McpTransport, MemoryConfig, QuietHours, RefineConfig, RefineDimension, Replica,
    RunsConfig, S3Replica, SanMateoConfig, SanMateoError, SandboxApprove, SandboxBackend,
    SandboxConfig, SkillsConfig, SlackConfig, ToolsConfig, UiConfig, chat_user, discord_sender,
    slack_sender,
};
pub use ids::{ChannelId, IdError, MessageId, OwnerName, SenderId};
pub use pairing::{PairingCode, Pending, PendingPairings};
pub use route::{ChannelRule, Decision, Inbound, Route, Routing, decide, route};

/// The agent kind of the assistant that the owner talks to.
pub const ASSISTANT_KIND: &str = "assistant";
