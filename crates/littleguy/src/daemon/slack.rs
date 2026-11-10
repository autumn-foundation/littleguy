//! The Slack side of the daemon: the Socket Mode task, and the loop that
//! turns its events into jobs. Feature `slack`.

use std::sync::Arc;

use littleguy_channels::slack::{SlackEvent, SlackPress, run_socket};
use littleguy_channels::{ChannelError, Service, SlackApi};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Daemon, Job, Press, cursors, decide_press, deliver, gate};
use crate::chats::Chats;

/// Starts the socket of `api` and the loop that feeds `jobs`, in `tasks`.
pub(super) fn start(
    daemon: &Daemon,
    api: SlackApi,
    jobs: &mpsc::Sender<Job>,
    tasks: &mut Vec<JoinHandle<()>>,
) {
    daemon.chats.set_slack(api.clone());
    let (events_tx, events) = mpsc::unbounded_channel();
    tasks.push(tokio::spawn(run_socket(api.clone(), events_tx)));
    let chats = Arc::clone(&daemon.chats);
    tasks.push(tokio::spawn(slack_loop(api, chats, events, jobs.clone())));
}

/// Turns Socket Mode events into jobs, and sends each reply.
async fn slack_loop(
    api: SlackApi,
    chats: Arc<Chats>,
    mut events: mpsc::UnboundedReceiver<SlackEvent>,
    jobs: mpsc::Sender<Job>,
) {
    let service = Service::Slack;
    while let Some(event) = events.recv().await {
        match event {
            SlackEvent::Message(message) => deliver(&chats, &jobs, service, message).await,
            SlackEvent::Press(press) => answer_slack_press(&api, &jobs, press).await,
            SlackEvent::Ready => {
                let Some(cursors) = cursors(&jobs, service).await else {
                    return;
                };
                for cursor in cursors {
                    match api.messages_after(&cursor.channel_id, &cursor.after).await {
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

/// Hands one Slack button press to the daemon, and sends its answer. The
/// socket acknowledged the press already, so this answer has no deadline.
async fn answer_slack_press(api: &SlackApi, jobs: &mpsc::Sender<Job>, press: SlackPress) {
    let job = Press {
        service: Service::Slack,
        user_id: press.user_id.clone(),
        custom_id: press.custom_id.clone(),
        channel_id: press.channel_id.clone(),
    };
    let result: Result<(), ChannelError> = match decide_press(jobs, job).await {
        Some(gate::PressAnswer::Update(text)) => api.update_pressed(&press, &text).await,
        Some(gate::PressAnswer::Private(text)) => api.answer_privately(&press, &text).await,
        None => return,
    };
    if let Err(error) = result {
        tracing::warn!(%error, "the answer to a button press did not go out");
    }
}
