//! The routing kernel: where one inbound message goes.
//!
//! [`decide`] is the whole decision. It takes three facts and returns one
//! [`Decision`]. `formal/verus/src/route.rs` proves the same table for every
//! input. [`route`] reads the three facts from the [`Routing`] and the
//! message, then names the agent.
//!
//! The rule that matters: a sender that the owner has not allowed never
//! reaches an agent. Such a sender gets a pairing prompt on a channel that
//! offers pairing, and nothing on any other channel.

use std::collections::{BTreeMap, BTreeSet};

use autumn_harvest_agent::AgentId;

use crate::ASSISTANT_KIND;
use crate::ids::{ChannelId, MessageId, OwnerName, SenderId};

/// One message from a channel, before routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inbound {
    /// The channel that received it.
    pub channel: ChannelId,
    /// The sender as the channel names it.
    pub sender: SenderId,
    /// The id of the message. It is the idempotency key of its op.
    pub message_id: MessageId,
    /// The text.
    pub text: String,
}

/// The decision of the kernel, before the agent is named.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Deliver the message to the owner's assistant.
    Deliver,
    /// Answer with a pairing prompt. Deliver nothing.
    Pairing,
    /// Drop the message. Answer nothing.
    Drop,
}

/// Where one message goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
    /// Deliver the message to this agent.
    Agent(AgentId),
    /// Answer with a pairing prompt.
    Pairing,
    /// Drop the message.
    Drop,
}

/// The rule of one channel.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ChannelRule {
    /// The channel takes messages at all.
    pub enabled: bool,
    /// The channel can show a pairing prompt to a sender it does not know.
    pub pairing: bool,
    /// The senders that the owner allowed.
    pub allowed: BTreeSet<SenderId>,
}

/// The routing table of one daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Routing {
    assistant: AgentId,
    channels: BTreeMap<ChannelId, ChannelRule>,
}

impl Routing {
    /// A table for `owner` with no channel.
    ///
    /// The owner name is a valid agent name, because [`OwnerName`] and
    /// [`AgentId`] accept the same printable ASCII. A name that the agent id
    /// still refuses yields `None`.
    #[must_use]
    pub fn new(owner: &OwnerName) -> Option<Self> {
        let assistant = AgentId::new(ASSISTANT_KIND, owner.as_str()).ok()?;
        Some(Self {
            assistant,
            channels: BTreeMap::new(),
        })
    }

    /// Adds or replaces the rule of `channel`.
    #[must_use]
    pub fn channel(mut self, channel: ChannelId, rule: ChannelRule) -> Self {
        self.channels.insert(channel, rule);
        self
    }

    /// The agent that the owner talks to.
    #[must_use]
    pub const fn assistant(&self) -> &AgentId {
        &self.assistant
    }

    /// Allows `sender` on `channel`. A channel with no rule takes no sender,
    /// so the call returns `false` and changes nothing.
    pub fn allow(&mut self, channel: &ChannelId, sender: SenderId) -> bool {
        self.channels.get_mut(channel).is_some_and(|rule| {
            rule.allowed.insert(sender);
            true
        })
    }

    /// Takes `sender` off `channel`. Returns whether the sender was allowed.
    pub fn revoke(&mut self, channel: &ChannelId, sender: &SenderId) -> bool {
        self.channels
            .get_mut(channel)
            .is_some_and(|rule| rule.allowed.remove(sender))
    }

    /// The allowed senders of `channel`, in order.
    pub fn allowed(&self, channel: &ChannelId) -> impl Iterator<Item = &SenderId> {
        self.channels
            .get(channel)
            .into_iter()
            .flat_map(|rule| &rule.allowed)
    }
}

/// The decision table. `formal/verus/src/route.rs` proves it.
///
/// - A disabled channel drops every message.
/// - An allowed sender on an enabled channel reaches the assistant.
/// - Any other sender gets a pairing prompt where the channel offers one,
///   and is dropped elsewhere.
#[must_use]
pub const fn decide(enabled: bool, allowed: bool, pairing: bool) -> Decision {
    // mirror-begin
    if !enabled {
        return Decision::Drop;
    }
    if allowed {
        return Decision::Deliver;
    }
    if pairing {
        Decision::Pairing
    } else {
        Decision::Drop
    }
    // mirror-end
}

/// Routes one message. A channel with no rule is a disabled channel.
#[must_use]
pub fn route(routing: &Routing, message: &Inbound) -> Route {
    let rule = routing.channels.get(&message.channel);
    let enabled = rule.is_some_and(|rule| rule.enabled);
    let allowed = rule.is_some_and(|rule| rule.allowed.contains(&message.sender));
    let pairing = rule.is_some_and(|rule| rule.pairing);
    match decide(enabled, allowed, pairing) {
        Decision::Deliver => Route::Agent(routing.assistant.clone()),
        Decision::Pairing => Route::Pairing,
        Decision::Drop => Route::Drop,
    }
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    fn owner() -> OwnerName {
        OwnerName::new("mark").unwrap()
    }

    fn cli() -> ChannelId {
        ChannelId::new("cli").unwrap()
    }

    fn message(channel: &str, sender: &str) -> Inbound {
        Inbound {
            channel: ChannelId::new(channel).unwrap(),
            sender: SenderId::new(sender).unwrap(),
            message_id: MessageId::new("m-1").unwrap(),
            text: String::from("hello"),
        }
    }

    fn table(pairing: bool) -> Routing {
        let rule = ChannelRule {
            enabled: true,
            pairing,
            allowed: BTreeSet::from([SenderId::new("uid:1000").unwrap()]),
        };
        Routing::new(&owner()).unwrap().channel(cli(), rule)
    }

    #[test]
    fn the_owner_reaches_the_assistant() {
        let routing = table(false);
        let route = route(&routing, &message("cli", "uid:1000"));
        assert_eq!(
            route,
            Route::Agent(AgentId::new("assistant", "mark").unwrap())
        );
        assert_eq!(routing.assistant().as_str(), "assistant/mark");
    }

    #[test]
    fn a_stranger_is_dropped_without_pairing() {
        assert_eq!(route(&table(false), &message("cli", "uid:0")), Route::Drop);
    }

    #[test]
    fn a_stranger_gets_a_pairing_prompt_where_the_channel_offers_one() {
        assert_eq!(
            route(&table(true), &message("cli", "uid:0")),
            Route::Pairing
        );
    }

    #[test]
    fn a_channel_with_no_rule_drops_even_an_allowed_sender() {
        assert_eq!(
            route(&table(true), &message("discord", "uid:1000")),
            Route::Drop
        );
    }

    #[test]
    fn a_disabled_channel_drops_the_owner() {
        let rule = ChannelRule {
            enabled: false,
            pairing: true,
            allowed: BTreeSet::from([SenderId::new("uid:1000").unwrap()]),
        };
        let routing = Routing::new(&owner()).unwrap().channel(cli(), rule);
        assert_eq!(route(&routing, &message("cli", "uid:1000")), Route::Drop);
    }

    #[test]
    fn an_allowed_sender_reaches_the_assistant_until_it_is_revoked() {
        let mut routing = table(true);
        let stranger = SenderId::new("uid:7").unwrap();
        assert_eq!(route(&routing, &message("cli", "uid:7")), Route::Pairing);
        assert!(routing.allow(&cli(), stranger.clone()));
        assert!(matches!(
            route(&routing, &message("cli", "uid:7")),
            Route::Agent(_)
        ));
        let listed: Vec<&str> = routing.allowed(&cli()).map(SenderId::as_str).collect();
        assert_eq!(listed, ["uid:1000", "uid:7"]);
        assert!(routing.revoke(&cli(), &stranger));
        assert!(!routing.revoke(&cli(), &stranger));
        assert_eq!(route(&routing, &message("cli", "uid:7")), Route::Pairing);
    }

    #[test]
    fn a_channel_with_no_rule_takes_no_sender() {
        let mut routing = table(true);
        let telegram = ChannelId::new("discord").unwrap();
        assert!(!routing.allow(&telegram, SenderId::new("discord:1").unwrap()));
        assert_eq!(routing.allowed(&telegram).count(), 0);
        assert_eq!(
            route(&routing, &message("discord", "discord:1")),
            Route::Drop
        );
    }

    /// The Verus spec of `formal/verus/src/route.rs`, restated in Rust.
    fn spec(enabled: bool, allowed: bool, pairing: bool) -> Decision {
        match (enabled, allowed, pairing) {
            (true, true, _) => Decision::Deliver,
            (true, false, true) => Decision::Pairing,
            _ => Decision::Drop,
        }
    }

    proptest! {
        #[test]
        fn decide_matches_the_spec(enabled: bool, allowed: bool, pairing: bool) {
            prop_assert_eq!(decide(enabled, allowed, pairing), spec(enabled, allowed, pairing));
        }

        #[test]
        fn only_an_allowed_sender_on_an_enabled_channel_reaches_an_agent(
            enabled: bool,
            pairing: bool,
            sender in "uid:[0-9]{1,5}",
        ) {
            let rule = ChannelRule {
                enabled,
                pairing,
                allowed: BTreeSet::from([SenderId::new("uid:1000").unwrap()]),
            };
            let routing = Routing::new(&owner()).unwrap().channel(cli(), rule);
            let reached = matches!(route(&routing, &message("cli", &sender)), Route::Agent(_));
            prop_assert_eq!(reached, enabled && sender == "uid:1000");
        }
    }
}
