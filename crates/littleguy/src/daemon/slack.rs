//! The Slack side of the daemon. Feature `slack`.
//!
//! Over Socket Mode, a task holds the socket open and a loop turns its
//! events into jobs. Over the Events API, Slack calls the hooks, and a
//! [`SlackInbox`] turns each verified request into a job.

use std::sync::Arc;

use littleguy_channels::slack::{SlackEvent, SlackPress, run_socket};
use littleguy_channels::{ChannelError, DmMessage, Service, SlackApi};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::{Daemon, Handle, Job, Press, SlackLink, cursors, decide_press, deliver, gate};
use crate::chats::Chats;

/// Starts `link` in `tasks`. Socket Mode starts the socket and the loop
/// that feeds `jobs`. The Events API reads the missed messages once, since
/// the hooks bring the rest.
pub(super) fn start(
    daemon: &Daemon,
    link: SlackLink,
    jobs: &mpsc::Sender<Job>,
    tasks: &mut Vec<JoinHandle<()>>,
) {
    let chats = Arc::clone(&daemon.chats);
    match link {
        SlackLink::Socket(api) => {
            chats.set_slack(api.clone());
            let (events_tx, events) = mpsc::unbounded_channel();
            tasks.push(tokio::spawn(run_socket(api.clone(), events_tx)));
            tasks.push(tokio::spawn(slack_loop(api, chats, events, jobs.clone())));
        }
        SlackLink::Events { api, .. } => {
            chats.set_slack(api.clone());
            tasks.push(tokio::spawn(catch_up(api, chats, jobs.clone())));
        }
    }
}

/// Turns Socket Mode events into jobs, and sends each reply.
async fn slack_loop(
    api: SlackApi,
    chats: Arc<Chats>,
    mut events: mpsc::UnboundedReceiver<SlackEvent>,
    jobs: mpsc::Sender<Job>,
) {
    while let Some(event) = events.recv().await {
        match event {
            SlackEvent::Message(message) => deliver(&chats, &jobs, Service::Slack, message).await,
            SlackEvent::Press(press) => answer_slack_press(&api, &jobs, press).await,
            SlackEvent::Ready => {
                if !read_missed(&api, &chats, &jobs).await {
                    return;
                }
            }
        }
    }
}

/// Reads the messages that came while the daemon was away, once.
async fn catch_up(api: SlackApi, chats: Arc<Chats>, jobs: mpsc::Sender<Job>) {
    read_missed(&api, &chats, &jobs).await;
}

/// Runs each message after the cursors of Slack. Returns false when the
/// daemon stops.
async fn read_missed(api: &SlackApi, chats: &Chats, jobs: &mpsc::Sender<Job>) -> bool {
    let service = Service::Slack;
    let Some(cursors) = cursors(jobs, service).await else {
        return false;
    };
    for cursor in cursors {
        match api.messages_after(&cursor.channel_id, &cursor.after).await {
            Ok(missed) => {
                for message in missed {
                    deliver(chats, jobs, service, message).await;
                }
            }
            Err(error) => tracing::warn!(%error, "reading missed messages failed"),
        }
    }
    true
}

/// Hands one Slack button press to the daemon, and sends its answer. Slack
/// has the acknowledgement already, so this answer has no deadline.
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

/// The way in for the Slack hooks of the Events API.
///
/// Each method runs one item to its end: the reply or the answer goes out
/// before it returns. A hook acknowledges Slack first and spawns the call,
/// because Slack waits three seconds at most.
#[derive(Debug, Clone)]
pub struct SlackInbox {
    api: SlackApi,
    chats: Arc<Chats>,
    jobs: mpsc::Sender<Job>,
}

impl SlackInbox {
    /// Runs one direct message, and sends its reply. Its message id is the
    /// key of its op, so a retry from Slack runs nothing new.
    pub async fn message(&self, message: DmMessage) {
        deliver(&self.chats, &self.jobs, Service::Slack, message).await;
    }

    /// Answers one button press.
    pub async fn press(&self, press: SlackPress) {
        answer_slack_press(&self.api, &self.jobs, press).await;
    }
}

impl Daemon {
    /// The inbox of the Slack hooks, on `api`, through the job loop of
    /// `handle`.
    #[must_use]
    pub fn slack_inbox(&self, api: SlackApi, handle: &Handle) -> SlackInbox {
        SlackInbox {
            api,
            chats: Arc::clone(&self.chats),
            jobs: handle.tx.clone(),
        }
    }
}
