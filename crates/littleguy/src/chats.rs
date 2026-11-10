//! The outbound side of each chat service: one place that sends a reply, a
//! notice with buttons, or a brief, on Discord or on Slack.
//!
//! A client arrives when its channel starts, after the daemon opens. So
//! each client sits in a `OnceLock`, and a send to a service that has no
//! client yet fails with [`ChannelError::Off`].

use std::sync::OnceLock;

use littleguy_channels::{Button, ChannelError, DiscordRest, Service, SlackApi};
use littleguy_core::SenderId;

/// The clients of the chat services that are on.
#[derive(Debug, Default)]
pub struct Chats {
    discord: OnceLock<DiscordRest>,
    slack: OnceLock<SlackApi>,
}

impl Chats {
    /// Gives Discord its client. The first client stays, because a second
    /// one is the same bot.
    pub fn set_discord(&self, rest: DiscordRest) {
        let _ = self.discord.set(rest);
    }

    /// Gives Slack its client. The first client stays.
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
    pub async fn send_with_buttons(
        &self,
        service: Service,
        channel: &str,
        text: &str,
        buttons: &[Button],
    ) -> Result<(), ChannelError> {
        match service {
            Service::Discord => {
                let rest = self.discord.get().ok_or(ChannelError::Off(service))?;
                if buttons.is_empty() {
                    rest.send(channel, text).await
                } else {
                    rest.send_with_buttons(channel, text, buttons).await
                }
            }
            Service::Slack => {
                let api = self.slack.get().ok_or(ChannelError::Off(service))?;
                api.send_with_buttons(channel, text, buttons).await
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
        let channel = match service {
            Service::Discord => {
                let rest = self.discord.get().ok_or(ChannelError::Off(service))?;
                rest.open_dm(id).await?
            }
            Service::Slack => {
                let api = self.slack.get().ok_or(ChannelError::Off(service))?;
                api.open_dm(id).await?
            }
        };
        self.send_with_buttons(service, &channel, text, buttons)
            .await
    }
}

#[cfg(test)]
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
