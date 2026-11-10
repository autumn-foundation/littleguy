//! The Discord side of the daemon: the gateway task, and the loop that
//! turns its events into jobs. Feature `discord`.

use std::sync::Arc;

use littleguy_channels::Service;
use littleguy_channels::discord::{ButtonPress, DiscordRest, GatewayEvent, run_gateway};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Daemon, Job, Press, cursors, decide_press, deliver, gate};
use crate::chats::Chats;

/// What the daemon needs to run the Discord channel. Its `Debug` form
/// leaves the token out.
#[derive(Clone)]
pub struct DiscordLink {
    /// The REST client.
    pub rest: DiscordRest,
    /// The bot token, for the gateway.
    pub token: String,
    /// The gateway URL.
    pub gateway_url: String,
}

impl std::fmt::Debug for DiscordLink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DiscordLink")
            .field("rest", &self.rest)
            .field("gateway_url", &self.gateway_url)
            .finish_non_exhaustive()
    }
}

/// Starts the gateway of `link` and the loop that feeds `jobs`, in `tasks`.
pub(super) fn start(
    daemon: &Daemon,
    link: DiscordLink,
    jobs: &mpsc::Sender<Job>,
    tasks: &mut Vec<JoinHandle<()>>,
) {
    daemon.chats.set_discord(link.rest.clone());
    let (events_tx, events) = mpsc::unbounded_channel();
    tasks.push(tokio::spawn(run_gateway(
        link.token.clone(),
        link.gateway_url.clone(),
        events_tx,
    )));
    let chats = Arc::clone(&daemon.chats);
    tasks.push(tokio::spawn(discord_loop(
        link.rest,
        chats,
        events,
        jobs.clone(),
    )));
}

/// Turns gateway events into jobs, and sends each reply.
async fn discord_loop(
    rest: DiscordRest,
    chats: Arc<Chats>,
    mut events: mpsc::UnboundedReceiver<GatewayEvent>,
    jobs: mpsc::Sender<Job>,
) {
    let service = Service::Discord;
    while let Some(event) = events.recv().await {
        match event {
            GatewayEvent::Message(message) => deliver(&chats, &jobs, service, message).await,
            GatewayEvent::Press(press) => answer_discord_press(&rest, &jobs, press).await,
            GatewayEvent::Ready => {
                let Some(cursors) = cursors(&jobs, service).await else {
                    return;
                };
                for cursor in cursors {
                    match rest.messages_after(&cursor.channel_id, &cursor.after).await {
                        Ok(missed) => {
                            for message in missed {
                                deliver(&chats, &jobs, service, message).await;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "reading missed messages failed"),
                    }
                }
            }
        }
    }
}

/// Hands one Discord button press to the daemon, and sends its answer.
async fn answer_discord_press(rest: &DiscordRest, jobs: &mpsc::Sender<Job>, press: ButtonPress) {
    let job = Press {
        service: Service::Discord,
        user_id: press.user_id.clone(),
        custom_id: press.custom_id.clone(),
        channel_id: press.channel_id.clone(),
    };
    let result = match decide_press(jobs, job).await {
        Some(gate::PressAnswer::Update(text)) => rest.update_pressed(&press, &text).await,
        Some(gate::PressAnswer::Private(text)) => rest.answer_privately(&press, &text).await,
        None => return,
    };
    if let Err(error) = result {
        tracing::warn!(%error, "the answer to a button press did not go out");
    }
}
