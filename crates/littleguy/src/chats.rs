//! The outbound side of each chat service: one place that sends a reply, a
//! notice with buttons, or a brief, on Discord or on Slack.
//!
//! A client arrives when its channel starts, after the daemon opens. So
//! each client sits in a `OnceLock`, and a send to a service that has no
//! client yet fails with [`ChannelError::Off`]. A build without the
//! feature of a service has no client for it, so each send to it fails
//! the same way.

#[cfg(any(feature = "discord", feature = "slack"))]
use std::sync::OnceLock;

#[cfg(feature = "discord")]
use littleguy_channels::DiscordRest;
#[cfg(feature = "slack")]
use littleguy_channels::SlackApi;
use littleguy_channels::{Button, ChannelError, Service};
use littleguy_core::SenderId;

/// The clients of the chat services that are on.
#[derive(Debug, Default)]
pub struct Chats {
    #[cfg(feature = "discord")]
    discord: OnceLock<DiscordRest>,
    #[cfg(feature = "slack")]
    slack: OnceLock<SlackApi>,
}

impl Chats {
    /// Gives Discord its client. The first client stays, because a second
    /// one is the same bot.
    #[cfg(feature = "discord")]
    pub fn set_discord(&self, rest: DiscordRest) {
        let _ = self.discord.set(rest);
    }

    /// Gives Slack its client. The first client stays.
    #[cfg(feature = "slack")]
    pub fn set_slack(&self, api: SlackApi) {
        let _ = self.slack.set(api);
    }

    /// Sends `text` to `channel` on `service`.
    ///
    /// # Errors
    ///
    /// Returns an error when the service is off or the send fails.
    pub async fn send(
        &self,
        service: Service,
        channel: &str,
        text: &str,
    ) -> Result<(), ChannelError> {
        self.send_with_buttons(service, channel, text, &[]).await
    }

    /// Sends `text` to `channel` on `service`, with `buttons` under it.
    ///
    /// # Errors
    ///
    /// Returns an error when the service is off or the send fails.
    #[cfg_attr(
        not(any(feature = "discord", feature = "slack")),
        expect(
            clippy::unused_async,
            clippy::unused_async_trait_impl,
            reason = "a build without a chat service has no call to wait on"
        )
    )]
    pub async fn send_with_buttons(
        &self,
        service: Service,
        channel: &str,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), ChannelError> {
        match service {
            #[cfg(feature = "discord")]
            Service::Discord => {
                let rest = self.discord.get().ok_or(ChannelError::Off(service))?;
                if buttons.is_empty() {
                    rest.send(channel, text).await
                } else {
                    rest.send_with_buttons(channel, text, buttons).await
                }
            }
            #[cfg(feature = "slack")]
            Service::Slack => {
                let api = self.slack.get().ok_or(ChannelError::Off(service))?;
                api.send_with_buttons(channel, text, buttons).await
            }
            #[cfg(not(all(feature = "discord", feature = "slack")))]
            _ => {
                let _ = (channel, text, buttons);
                Err(ChannelError::Off(service))
            }
        }
    }

    /// Sends `text` with `buttons` as a direct message to the chat user
    /// `user`, such as `slack:U1`.
    ///
    /// # Errors
    ///
    /// Returns an error when `user` names no chat service, the service is
    /// off, or a call fails.
    pub async fn dm(
        &self,
        user: &SenderId,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), ChannelError> {
        let (service, id) = Service::split_sender(user.as_str())
            .ok_or_else(|| ChannelError::Decode(format!("`{user}` is not a chat user")))?;
        let channel = self.open_dm(service, id).await?;
        self.send_with_buttons(service, &channel, text, buttons)
            .await
    }

    /// Opens the direct message channel of the user `id` on `service`.
    #[cfg_attr(
        not(any(feature = "discord", feature = "slack")),
        expect(
            clippy::unused_async,
            clippy::unused_async_trait_impl,
            reason = "a build without a chat service has no call to wait on"
        )
    )]
    async fn open_dm(&self, service: Service, id: &str) -> Result<String, ChannelError> {
        match service {
            #[cfg(feature = "discord")]
            Service::Discord => {
                let rest = self.discord.get().ok_or(ChannelError::Off(service))?;
                rest.open_dm(id).await
            }
            #[cfg(feature = "slack")]
            Service::Slack => {
                let api = self.slack.get().ok_or(ChannelError::Off(service))?;
                api.open_dm(id).await
            }
            #[cfg(not(all(feature = "discord", feature = "slack")))]
            _ => {
                let _ = id;
                Err(ChannelError::Off(service))
            }
        }
    }
}

#[cfg(test)]
mod off_tests {
    use super::*;

    #[tokio::test]
    async fn a_service_without_a_client_is_off() {
        let chats = Chats::default();
        for service in Service::ALL {
            let error = chats.send(service, "1", "hi").await.unwrap_err();
            assert!(matches!(error, ChannelError::Off(off) if off == service));
        }
        let user = SenderId::new("slack:U1").unwrap();
        let error = chats.dm(&user, "hi", &[]).await.unwrap_err();
        assert!(matches!(error, ChannelError::Off(Service::Slack)));
    }
}

#[cfg(all(test, feature = "discord", feature = "slack"))]
mod tests {
    use littleguy_channels::testing::FakeDiscord;
    use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack};

    use super::*;

    #[tokio::test]
    async fn each_service_sends_on_its_own_client() {
        let chats = Chats::default();
        let slack_user = SenderId::new("slack:U1").unwrap();
        let error = chats.dm(&slack_user, "hi", &[]).await.unwrap_err();
        assert!(
            matches!(error, ChannelError::Off(Service::Slack)),
            "{error}"
        );
        let error = chats.send(Service::Discord, "1", "hi").await.unwrap_err();
        assert!(matches!(error, ChannelError::Off(Service::Discord)));
        let cli = SenderId::new("uid:1000").unwrap();
        assert!(chats.dm(&cli, "hi", &[]).await.is_err());

        let discord = FakeDiscord::start().await.unwrap();
        let slack = FakeSlack::start().await.unwrap();
        chats.set_discord(DiscordRest::new(discord.api_base.clone(), "token"));
        chats.set_slack(SlackApi::new(slack.api_base.clone(), BOT_TOKEN, APP_TOKEN));

        chats.dm(&slack_user, "to slack", &[]).await.unwrap();
        let discord_user = SenderId::new("discord:7").unwrap();
        chats.dm(&discord_user, "to discord", &[]).await.unwrap();
        assert_eq!(slack.posted()[0].channel, "DU1");
        assert_eq!(slack.posted()[0].text, "to slack");
        assert_eq!(discord.sent()[0].channel_id, "dm7");
        assert_eq!(discord.sent()[0].content, "to discord");
    }
}
