//! The delivery of the morning brief.
//!
//! The wake plan ([`crate::wake`]) aims the engine timer at each brief
//! slot. A tick is unattended and read-only, as every engine heartbeat is.
//! It reads the notes of the assistant. An answer of `HEARTBEAT_OK` is
//! silent: the engine keeps it out of the transcript and delivers nothing.
//!
//! Any other answer goes to [`BriefDelivery`] through the recorded
//! `agent_deliver` activity. The delivery sends it as a direct message to
//! `brief.to`, on Discord or Slack, and keeps the report key in
//! `memory.db`. The engine delivers at least once, so the key check makes
//! one DM per brief.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use autumn_harvest_agent::{AgentError, BoxFuture, Delivery, ErrorKind, Report, ReportSource};
use littleguy_channels::ChannelError;
use littleguy_core::{BriefConfig, SenderId};
use littleguy_stores::SqliteStores;

use crate::chats::Chats;

/// Sends each brief as a direct message to the owner, once.
///
/// It ignores every report that is not a heartbeat, because the channels
/// send those answers themselves. It also ignores a heartbeat tick that
/// was not a brief: the wake plan says which one it armed, and a heartbeat
/// speaks in the web chat.
pub struct BriefDelivery {
    stores: Arc<SqliteStores>,
    chats: Arc<Chats>,
    to: Option<SenderId>,
    brief_armed: Arc<AtomicBool>,
}

impl std::fmt::Debug for BriefDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BriefDelivery")
            .field("to", &self.to)
            .finish_non_exhaustive()
    }
}

impl BriefDelivery {
    /// A delivery to `config.to`. The chat client arrives later in
    /// `chats`, when its channel starts. `brief_armed` is the flag of the
    /// wake plan.
    #[must_use]
    pub fn new(
        config: &BriefConfig,
        stores: Arc<SqliteStores>,
        chats: Arc<Chats>,
        brief_armed: Arc<AtomicBool>,
    ) -> Self {
        Self {
            stores,
            chats,
            to: config.to.clone(),
            brief_armed,
        }
    }

    async fn send(&self, report: &Report) -> Result<(), AgentError> {
        let key = report.key();
        if self.stores.was_sent(&key)? {
            return Ok(());
        }
        tracing::info!(
            chars = report.text.chars().count(),
            "a morning brief is ready"
        );
        tracing::debug!(text = %report.text, "the morning brief");
        if let Some(user) = &self.to {
            match self.chats.dm(user, &report.text, &[]).await {
                Ok(()) => {}
                Err(ChannelError::Off(service)) => {
                    tracing::warn!(%service, "the brief names a user of a service that is off");
                }
                Err(error) => return Err(transport(&error)),
            }
        }
        self.stores.mark_sent(&key)
    }
}

impl Delivery for BriefDelivery {
    fn deliver<'a>(&'a self, report: &'a Report) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            if report.source != ReportSource::Heartbeat || !self.brief_armed.load(Ordering::SeqCst)
            {
                return Ok(());
            }
            self.send(report).await
        })
    }
}

/// A failed chat call can pass, so the engine retries it.
fn transport(error: &ChannelError) -> AgentError {
    AgentError::new(
        ErrorKind::Transport,
        format!("the brief did not reach its chat: {error}"),
    )
}
