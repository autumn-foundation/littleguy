//! The chat channels of littleguy: Discord and Slack.
//!
//! A channel turns the messages of one chat service into plain values, and
//! sends replies back. The daemon routes each message. A channel knows
//! nothing about agents.
//!
//! Feature `discord` builds the Discord channel and feature `slack` builds
//! the Slack channel. Both are on by default.

pub mod chat;
#[cfg(feature = "discord")]
pub mod discord;
#[cfg(feature = "slack")]
pub mod slack;
#[cfg(feature = "testing")]
pub mod testing;

pub use chat::{Button, ButtonStyle, ChannelError, DmMessage, Service, split_message};
#[cfg(feature = "discord")]
pub use discord::{ButtonPress, DiscordRest, GatewayEvent};
#[cfg(feature = "slack")]
pub use slack::{SlackApi, SlackEvent, SlackPress};
