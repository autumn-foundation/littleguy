//! The morning brief.
//!
//! The engine heartbeat runs on an interval only (gap 3 of the design). A
//! signal cannot carry a tick either: only the engine wake hook makes one.
//! So the daemon keeps the brief calendar and aims the engine timer at the
//! next slot of `brief.schedule`. It sends one `Control::SetHeartbeat` per
//! slot. Its interval is the time from now to the slot, so the engine books
//! the tick at that slot. The op key holds the slot, so a slot is armed
//! once, also across a restart.
//!
//! After the tick of a slot runs, the engine books the next tick one
//! interval later. That time is wrong, so the daemon arms the next slot.
//!
//! A tick is unattended and read-only, as every engine heartbeat is. It
//! reads the notes of the assistant. An answer of `HEARTBEAT_OK` is silent:
//! the engine keeps it out of the transcript and delivers nothing.
//!
//! Any other answer goes to [`BriefDelivery`] through the recorded
//! `agent_deliver` activity. The delivery sends it as a Discord DM to
//! `brief.to`, and keeps the report key in `memory.db`. The engine delivers
//! at least once, so the key check makes one DM per brief.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Result, bail};
use autumn_harvest::policy::Schedule;
use autumn_harvest::schedule_plan::next_fire_after;
use autumn_harvest_agent::agent::MIN_CUSTOM_HEARTBEAT_SECS;
use autumn_harvest_agent::{
    AgentError, AgentHeartbeat, AgentOp, BoxFuture, Control, Delivery, ErrorKind, HeartbeatSetting,
    Report, ReportSource,
};
use chrono::{DateTime, Utc};
use littleguy_channels::discord::DiscordRest;
use littleguy_core::BriefConfig;
use littleguy_stores::SqliteStores;

/// The prefix of the op key that arms one slot.
pub const KEY_PREFIX: &str = "brief:";

/// The time after a slot that the daemon waits for its tick. A tick that
/// has not run by then never comes, for example on a paused agent. The
/// daemon then arms the next slot.
pub const GRACE: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// The heartbeat that ticks `every` from now: read-only, with the brief
/// prompt.
#[must_use]
pub fn heartbeat(prompt: &str, every: Duration) -> AgentHeartbeat {
    AgentHeartbeat::every(every).prompt(prompt)
}

/// One arm of the engine timer: the op and its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arm {
    /// The op key. It names the slot.
    pub key: String,
    /// The `Control::SetHeartbeat` op.
    pub op: AgentOp,
}

/// The slot that the engine timer aims at, and the tick count at the arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Armed {
    slot: DateTime<Utc>,
    ticks: u64,
}

/// The brief calendar: the slot that the engine timer aims at.
#[derive(Debug, Clone)]
pub struct Calendar {
    schedule: Schedule,
    prompt: String,
    armed: Option<Armed>,
}

impl Calendar {
    /// The calendar of `config`. Nothing is armed yet.
    ///
    /// # Errors
    ///
    /// Returns an error when the schedule has no slot after `now`, for
    /// example a bad cron field or an unknown time zone.
    pub fn new(config: &BriefConfig, now: DateTime<Utc>) -> Result<Self> {
        let schedule = Schedule::CronInTimezone {
            expr: config.schedule.clone(),
            tz: config.timezone.clone(),
        };
        if next_fire_after(&schedule, now).is_none() {
            bail!(
                "brief.schedule `{}` in `{}` is not a schedule",
                config.schedule,
                config.timezone
            );
        }
        Ok(Self {
            schedule,
            prompt: config.prompt.clone(),
            armed: None,
        })
    }

    /// The slot that the engine timer aims at, once armed.
    #[cfg(test)]
    #[must_use]
    pub fn slot(&self) -> Option<DateTime<Utc>> {
        self.armed.map(|armed| armed.slot)
    }

    /// Whether the daemon must read the tick count before the next
    /// [`arm`](Self::arm): the armed slot has passed.
    #[must_use]
    pub fn waits_for_tick(&self, now: DateTime<Utc>) -> bool {
        self.armed.is_some_and(|armed| armed.slot <= now)
    }

    /// The arm of the next slot, when the engine timer needs one. `ticks`
    /// is the tick count of the assistant.
    ///
    /// - With nothing armed, it arms the first slot after `now`.
    /// - Before the armed slot, it arms nothing.
    /// - After the slot, it arms the next slot once the tick ran, or once
    ///   [`GRACE`] passed. So a slot that passes while the daemon is busy
    ///   still runs one brief, not one per missed slot.
    pub fn arm(&mut self, now: DateTime<Utc>, ticks: u64) -> Option<Arm> {
        if let Some(armed) = self.armed {
            let ran = ticks > armed.ticks;
            if now < armed.slot || (!ran && now < armed.slot + GRACE) {
                return None;
            }
        }
        let slot = next_fire_after(&self.schedule, now)?;
        self.armed = Some(Armed { slot, ticks });
        Some(Arm {
            key: format!("{KEY_PREFIX}{}", slot.to_rfc3339()),
            op: AgentOp::Control(Control::SetHeartbeat {
                setting: HeartbeatSetting::Custom {
                    heartbeat: heartbeat(&self.prompt, interval(now, slot)),
                },
            }),
        })
    }
}

/// The time from `now` to `slot`, in whole seconds, rounded up. The engine
/// refuses an interval under [`MIN_CUSTOM_HEARTBEAT_SECS`], so a slot that
/// close runs that much late.
fn interval(now: DateTime<Utc>, slot: DateTime<Utc>) -> Duration {
    let millis = u64::try_from((slot - now).num_milliseconds()).unwrap_or(0);
    Duration::from_secs(millis.div_ceil(1_000).max(MIN_CUSTOM_HEARTBEAT_SECS))
}

/// Sends each brief as a Discord DM to the owner, once.
///
/// It ignores every report that is not a heartbeat, because the channels
/// send those answers themselves.
pub struct BriefDelivery {
    stores: Arc<SqliteStores>,
    rest: Arc<OnceLock<DiscordRest>>,
    to: Option<String>,
}

impl std::fmt::Debug for BriefDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BriefDelivery")
            .field("discord", &self.rest.get().is_some())
            .finish_non_exhaustive()
    }
}

impl BriefDelivery {
    /// A delivery to `config.to`. The REST client arrives later in `rest`,
    /// when the Discord channel starts.
    #[must_use]
    pub fn new(
        config: &BriefConfig,
        stores: Arc<SqliteStores>,
        rest: Arc<OnceLock<DiscordRest>>,
    ) -> Self {
        let to = config
            .to
            .as_ref()
            .and_then(|sender| sender.as_str().strip_prefix("discord:").map(str::to_owned));
        Self { stores, rest, to }
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
        match (&self.to, self.rest.get()) {
            (Some(user), Some(rest)) => {
                let channel = rest
                    .open_dm(user)
                    .await
                    .map_err(|error| transport(&error))?;
                rest.send(&channel, &report.text)
                    .await
                    .map_err(|error| transport(&error))?;
            }
            (Some(_), None) => {
                tracing::warn!("the brief names a Discord user, but Discord is off");
            }
            (None, _) => {}
        }
        self.stores.mark_sent(&key)
    }
}

impl Delivery for BriefDelivery {
    fn deliver<'a>(&'a self, report: &'a Report) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            if report.source != ReportSource::Heartbeat {
                return Ok(());
            }
            self.send(report).await
        })
    }
}

/// A failed Discord call can pass, so the engine retries it.
fn transport(error: &littleguy_channels::discord::ChannelError) -> AgentError {
    AgentError::new(
        ErrorKind::Transport,
        format!("the brief did not reach Discord: {error}"),
    )
}

#[cfg(test)]
mod tests {
    use littleguy_core::config::{DEFAULT_BRIEF_PROMPT, DEFAULT_BRIEF_SCHEDULE};

    use super::*;

    fn config(schedule: &str, timezone: &str) -> BriefConfig {
        BriefConfig {
            schedule: String::from(schedule),
            timezone: String::from(timezone),
            prompt: String::from(DEFAULT_BRIEF_PROMPT),
            to: None,
        }
    }

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn every(arm: &Arm) -> u64 {
        match &arm.op {
            AgentOp::Control(Control::SetHeartbeat {
                setting: HeartbeatSetting::Custom { heartbeat },
            }) => heartbeat.every_secs,
            _ => 0,
        }
    }

    #[test]
    fn the_brief_skips_the_weekend() {
        // Friday 6 Nov 2026, after 07:30 in London (UTC in winter).
        let now = at("2026-11-06T08:00:00Z");
        let mut calendar =
            Calendar::new(&config(DEFAULT_BRIEF_SCHEDULE, "Europe/London"), now).unwrap();
        let arm = calendar.arm(now, 0).unwrap();
        assert_eq!(arm.key, "brief:2026-11-09T07:30:00+00:00");
        assert_eq!(calendar.slot(), Some(at("2026-11-09T07:30:00Z")));
        // From Friday 08:00 to Monday 07:30.
        assert_eq!(every(&arm), (2 * 24 + 23) * 3600 + 30 * 60);
        assert!(!calendar.waits_for_tick(at("2026-11-09T07:29:59Z")));
        assert_eq!(calendar.arm(at("2026-11-09T07:29:59Z"), 0), None);
    }

    #[test]
    fn the_next_slot_waits_for_the_tick() {
        let start = at("2026-11-01T09:00:00Z");
        let mut calendar = Calendar::new(&config("0 8 * * *", "UTC"), start).unwrap();
        assert!(calendar.arm(start, 3).is_some());
        let after = at("2026-11-02T08:00:01Z");
        assert!(calendar.waits_for_tick(after));
        // The tick has not run yet, so the engine timer stays.
        assert_eq!(calendar.arm(after, 3), None);
        let arm = calendar.arm(after, 4).unwrap();
        assert_eq!(arm.key, "brief:2026-11-03T08:00:00+00:00");
        assert_eq!(every(&arm), 24 * 3600 - 1);
    }

    #[test]
    fn a_tick_that_never_comes_arms_the_next_slot_after_the_grace() {
        let start = at("2026-11-01T09:00:00Z");
        let mut calendar = Calendar::new(&config("0 8 * * *", "UTC"), start).unwrap();
        assert!(calendar.arm(start, 0).is_some());
        assert_eq!(calendar.arm(at("2026-11-02T08:59:59Z"), 0), None);
        // Three slots pass with no tick. One arm aims at the next slot.
        let late = at("2026-11-05T09:00:00Z");
        let arm = calendar.arm(late, 0).unwrap();
        assert_eq!(arm.key, "brief:2026-11-06T08:00:00+00:00");
        assert_eq!(calendar.arm(late, 0), None);
    }

    #[test]
    fn a_close_slot_waits_the_engine_minimum() {
        let now = at("2026-11-02T07:59:30Z");
        let mut calendar = Calendar::new(&config("0 8 * * *", "UTC"), now).unwrap();
        let arm = calendar.arm(now, 0).unwrap();
        assert_eq!(every(&arm), MIN_CUSTOM_HEARTBEAT_SECS);
    }

    #[test]
    fn the_time_zone_moves_the_slot() {
        let now = at("2026-11-09T00:00:00Z");
        let mut calendar = Calendar::new(&config("30 7 * * *", "America/New_York"), now).unwrap();
        assert!(calendar.arm(now, 0).is_some());
        assert_eq!(calendar.slot(), Some(at("2026-11-09T12:30:00Z")));
    }

    #[test]
    fn a_bad_schedule_is_refused() {
        let now = at("2026-11-09T00:00:00Z");
        let error = Calendar::new(&config("30 7 * * *", "Mars/Olympus"), now).unwrap_err();
        assert!(error.to_string().contains("brief.schedule"));
        assert!(Calendar::new(&config("99 7 * * *", "UTC"), now).is_err());
    }

    #[test]
    fn the_brief_tick_is_read_only() {
        let beat = heartbeat(DEFAULT_BRIEF_PROMPT, Duration::from_secs(90));
        assert_eq!(beat.every_secs, 90);
        assert!(!beat.allow_actions && !beat.allow_memory_writes);
        assert_eq!(beat.prompt, DEFAULT_BRIEF_PROMPT);
    }
}
