//! The chat channels of littleguy: Discord and Slack.
//!
//! A channel turns the messages of one chat service into plain values, and
//! sends replies back. The daemon routes each message. A channel knows
//! nothing about agents.

pub mod chat;
pub mod discord;
pub mod slack;
#[cfg(feature = "testing")]
pub mod testing;

pub use chat::{Button, ButtonStyle, ChannelError, DmMessage, Service, split_message};
pub use discord::{ButtonPress, DiscordRest, GatewayEvent};
pub use slack::{SlackApi, SlackEvent, SlackPress};
