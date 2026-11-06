//! The chat channels of littleguy.
//!
//! A channel turns the messages of one chat service into plain values, and
//! sends replies back. The daemon routes each message. A channel knows
//! nothing about agents.

pub mod discord;
#[cfg(feature = "testing")]
pub mod testing;

pub use discord::{DiscordRest, DmMessage, GatewayEvent};
