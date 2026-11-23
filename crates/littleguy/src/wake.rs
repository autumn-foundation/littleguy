//! When the assistant wakes on its own: the morning brief and the heartbeat
//! (ADR 0021).
//!
//! The engine keeps one heartbeat timer per agent, and it runs on an
//! interval only. Both the brief and the heartbeat need it. So the daemon
//! plans every wake here and aims the one timer at the earliest: it sends
//! one `Control::SetHeartbeat` whose interval is the time from now to that
//! wake. After the tick runs, the engine books the next tick one interval
//! later. That time is wrong, so the daemon arms the next wake.
//!
//! The op key names what an arm sets: the kind, the time of the wake and a
//! digest of the setting less its interval (ADR 0045). A retry of one arm
//! keeps its key, also across a restart, so the engine applies it once. The
//! interval follows from the time of the wake, so the key leaves it out.
//! Two arms that set different things at one time get two keys, so neither
//! is dropped. The engine can still drop an arm whose key went out before a
//! later arm replaced it. The daemon then reads the setting that the engine
//! holds. When it differs, the daemon sends the arm again under a key that
//! names that setting ([`Arm::over`]).
//!
//! - **The brief** wakes at each slot of `brief.schedule`.
//! - **The heartbeat** wakes `every_minutes` after the last arm, outside
//!   its quiet hours. Its ticks ask the precheck ([`Due`]) first, which
//!   skips a tick with no model call when no goal is due and no idea waits.
//! - **A reflection** wakes once, soon after a memory consolidation pass,
//!   so the assistant files ideas from what the pass learned (ADR 0022).
//!   Its tick asks the precheck too. It runs only while its pass is
//!   pending ([`PendingPass`]), so one pass costs one model call at most.
//! - **A follow-up** wakes at the next wake that the assistant booked with
//!   `wake_me` (ADR 0023). Its prompt is fixed: the text of the wake stays
//!   in the store, where `list_plans` shows it.
//!
//! When a one-shot wake ran and nothing else is planned, the plan turns the
//! engine timer off. Otherwise the engine would book the same tick again.
//!
//! Each tick prompt starts with a mark, so the history shows its answer as
//! the assistant speaking on its own ([`is_tick`]). A tick is unattended. Its
//! tools may read, and they may write the assistant's own bookkeeping
//! (`Internal`). The workspace is read-only on a tick: the engine keeps no
//! workspace change of a read-only run, so the file tools that change it
//! are `Write`, which the engine refuses there.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use autumn_harvest::policy::Schedule;
use autumn_harvest_agent::agent::MIN_CUSTOM_HEARTBEAT_SECS;
use autumn_harvest_agent::{AgentHeartbeat, AgentOp, Control, HeartbeatSetting};
#[cfg(any(feature = "heartbeat", feature = "reflection"))]
use autumn_harvest_agent::{BoxFuture, HeartbeatTask, Precheck};
use jiff::Timestamp;

use autumn_harvest::schedule_plan::next_fire_after;
#[cfg(feature = "heartbeat")]
use littleguy_core::QuietHours;
use littleguy_core::{BriefConfig, HeartbeatConfig};
#[cfg(feature = "heartbeat")]
use littleguy_stores::SqliteStores;

/// The first line of the prompt of a brief tick.
pub const BRIEF_MARK: &str = "[littleguy brief]";

/// The first line of the prompt of a heartbeat tick.
pub const HEARTBEAT_MARK: &str = "[littleguy heartbeat]";

/// The first line of the prompt of a reflection tick.
pub const REFLECT_MARK: &str = "[littleguy reflection]";

/// The prefix of the op key that arms one reflection. The id of the pass
/// follows.
#[cfg(feature = "reflection")]
pub const REFLECT_KEY: &str = "reflect:";

/// The first line of the prompt of a follow-up tick.
pub const FOLLOWUP_MARK: &str = "[littleguy follow-up]";

/// The prefix of the op key that arms one follow-up.
#[cfg(feature = "followups")]
pub const FOLLOWUP_KEY: &str = "followup:";

/// The instruction of a follow-up tick. It is fixed: the text that the
/// model wrote stays in the store, so it never becomes a user message.
#[cfg(feature = "followups")]
pub const FOLLOWUP_PROMPT: &str = "A wake that you booked with wake_me is due. Call \
     list_plans: each wake with `due` true says what to look at. Look, and note what you \
     found on the goal or idea it concerns. Book another wake only if you must look again. \
     Reply with exactly HEARTBEAT_OK unless the owner should know something now.";

/// What a reflection tick does with the facts that its prompt lists. The
/// prompt says what the pass did first ([`crate::reflection`]).
#[cfg(all(feature = "reflection", feature = "skills"))]
pub const REFLECT_PROMPT: &str = "Call list_plans. For anything worth planning or \
     researching that is not an idea yet, file one with add_idea. Add what you now know \
     to a related open idea with research_idea. Do not file an idea that exists. Propose \
     a goal only when an idea is clearly worth doing. When what you now know answers an \
     open idea that needs no goal, conclude it with conclude_idea. If you found a way of \
     doing something that you will need again, draft a skill with propose_skill. Reply \
     with exactly HEARTBEAT_OK unless the owner should know something now.";

/// What a reflection tick does, in a build without skills.
#[cfg(all(feature = "reflection", not(feature = "skills")))]
pub const REFLECT_PROMPT: &str = "Call list_plans. For anything worth planning or \
     researching that is not an idea yet, file one with add_idea. Add what you now know \
     to a related open idea with research_idea. Do not file an idea that exists. Propose \
     a goal only when an idea is clearly worth doing. When what you now know answers an \
     open idea that needs no goal, conclude it with conclude_idea. Reply with exactly \
     HEARTBEAT_OK unless the owner should know something now.";

/// The prefix of the op key that arms one brief slot.
pub const BRIEF_KEY: &str = "brief:";

/// The prefix of the op key that arms one heartbeat.
#[cfg(feature = "heartbeat")]
pub const HEARTBEAT_KEY: &str = "heartbeat:";

/// The prefix of the op key that turns the engine timer off.
pub const OFF_KEY: &str = "wake-off:";

/// The time after a wake that the daemon waits for its tick. A tick that
/// has not run by then never comes, for example on a paused agent. The
/// daemon then arms the next wake.
pub const GRACE: jiff::SignedDuration = jiff::SignedDuration::from_hours(1);

/// Whether `text` is the prompt of a tick, not a message of a person.
#[must_use]
pub fn is_tick(text: &str) -> bool {
    [BRIEF_MARK, HEARTBEAT_MARK, REFLECT_MARK, FOLLOWUP_MARK]
        .iter()
        .any(|mark| text.starts_with(mark))
}

/// The prompt of a tick: the mark, then the instruction.
fn marked(mark: &str, prompt: &str) -> String {
    format!("{mark}\n{prompt}")
}

/// The read-only heartbeat that ticks `every` from now with `prompt`.
#[must_use]
pub fn heartbeat(prompt: &str, every: Duration) -> AgentHeartbeat {
    AgentHeartbeat::every(every).prompt(prompt)
}

/// One arm of the engine timer: the op and its key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Arm {
    /// The op key. It names the wake and what the arm sets.
    pub key: String,
    /// The `Control::SetHeartbeat` op.
    pub op: AgentOp,
}

impl Arm {
    /// The arm that sets `setting`, keyed by `prefix`, the time of the
    /// wake, and a digest of the setting.
    fn keyed(prefix: &str, at: Timestamp, setting: HeartbeatSetting) -> Self {
        Self {
            key: format!(
                "{prefix}{}#{:016x}",
                crate::stamp::rfc3339(at),
                digest(&setting)
            ),
            op: AgentOp::Control(Control::SetHeartbeat { setting }),
        }
    }

    /// The setting that this arm sends.
    const fn setting(&self) -> Option<&HeartbeatSetting> {
        match &self.op {
            AgentOp::Control(Control::SetHeartbeat { setting }) => Some(setting),
            _ => None,
        }
    }

    /// Whether the engine holds what this arm sets, when it holds `held`.
    /// The engine narrows a custom heartbeat under the profile, so only the
    /// prompt and the precheck count. Each kind of wake has its own prompt.
    #[must_use]
    pub fn holds(&self, held: &HeartbeatSetting) -> bool {
        match (self.setting(), held) {
            (Some(HeartbeatSetting::Off), HeartbeatSetting::Off)
            | (Some(HeartbeatSetting::Profile), HeartbeatSetting::Profile) => true,
            (
                Some(HeartbeatSetting::Custom { heartbeat: ours }),
                HeartbeatSetting::Custom { heartbeat: theirs },
            ) => ours.prompt == theirs.prompt && ours.precheck == theirs.precheck,
            _ => false,
        }
    }

    /// This arm again, for an engine that dropped its key and holds
    /// `held`. The new key adds a digest of `held`. So the arm goes out
    /// once over each setting that replaced it, and a retry keeps the key.
    #[must_use]
    pub fn over(&self, held: &HeartbeatSetting) -> Self {
        Self {
            key: format!("{}~{:016x}", self.key, digest(held)),
            op: self.op.clone(),
        }
    }
}

/// A digest of what `setting` sets, less its interval. The interval
/// follows from the time of the wake, which the key holds already.
fn digest(setting: &HeartbeatSetting) -> u64 {
    let text = match setting {
        HeartbeatSetting::Custom { heartbeat } => {
            let mut what = heartbeat.clone();
            what.every_secs = 0;
            serde_json::to_string(&HeartbeatSetting::Custom { heartbeat: what })
        }
        other => serde_json::to_string(other),
    };
    // A setting always serializes. An empty text still leaves the kind and
    // the time in the key.
    crate::fnv::fnv1a(&text.unwrap_or_default())
}

/// The op that turns the engine timer off, keyed by `now`.
#[must_use]
pub fn off(now: Timestamp) -> Arm {
    Arm {
        key: format!("{OFF_KEY}{}", crate::stamp::rfc3339(now)),
        op: AgentOp::Control(Control::SetHeartbeat {
            setting: HeartbeatSetting::Off,
        }),
    }
}

/// The consolidation pass whose reflection may still run. The plan sets it
/// with [`Wake::reflect`]. The reflection precheck takes it, so a pass wakes
/// one reflection tick at most, whatever the engine timer holds. The daemon
/// clears it when it sees the reflection tick ran.
#[cfg(feature = "reflection")]
#[derive(Debug, Default)]
pub struct PendingPass(std::sync::Mutex<Option<String>>);

#[cfg(feature = "reflection")]
impl PendingPass {
    fn slot(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        // The slot holds one id. A panic while it is held leaves a whole
        // value, so a poisoned lock is safe to use.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn set(&self, pass: &str) {
        *self.slot() = Some(pass.to_owned());
    }

    /// The pending pass, which is no longer pending after this call.
    pub fn take(&self) -> Option<String> {
        self.slot().take()
    }

    /// The pending pass, which stays pending.
    #[cfg(test)]
    #[must_use]
    pub fn get(&self) -> Option<String> {
        self.slot().clone()
    }
}

/// What a wake is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A slot of the morning brief.
    Brief,
    /// A heartbeat.
    #[cfg(feature = "heartbeat")]
    Heartbeat,
    /// A reflection after a consolidation pass.
    #[cfg(feature = "reflection")]
    Reflection,
    /// A wake that the assistant booked.
    #[cfg(feature = "followups")]
    Followup,
}

impl Kind {
    /// The name in the tick log.
    #[cfg(feature = "self-debug")]
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Brief => "brief",
            #[cfg(feature = "heartbeat")]
            Self::Heartbeat => "heartbeat",
            #[cfg(feature = "reflection")]
            Self::Reflection => "reflection",
            #[cfg(feature = "followups")]
            Self::Followup => "follow-up",
        }
    }
}

/// What the precheck decided for the last heartbeat tick, for the tick log.
#[cfg(feature = "heartbeat")]
#[derive(Debug, Default)]
pub struct Verdict(std::sync::atomic::AtomicU8);

#[cfg(feature = "heartbeat")]
impl Verdict {
    /// No verdict since the last read. Only the tick log reads one.
    #[cfg(any(feature = "self-debug", test))]
    const UNKNOWN: u8 = 0;
    const RAN: u8 = 1;
    const SKIPPED: u8 = 2;

    fn set(&self, ran: bool) {
        let value = if ran { Self::RAN } else { Self::SKIPPED };
        self.0.store(value, Ordering::SeqCst);
    }

    /// Whether the precheck skipped the last heartbeat tick, and forgets it.
    /// `None` when no precheck ran since.
    #[cfg(any(feature = "self-debug", test))]
    #[must_use]
    pub fn take_skipped(&self) -> Option<bool> {
        match self.0.swap(Self::UNKNOWN, Ordering::SeqCst) {
            Self::RAN => Some(false),
            Self::SKIPPED => Some(true),
            _ => None,
        }
    }
}

/// The wake that the engine timer aims at, and the tick count at the arm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Armed {
    at: Timestamp,
    kind: Kind,
    ticks: u64,
}

/// The daily window with no heartbeat, as two cron times.
#[cfg(feature = "heartbeat")]
#[derive(Debug, Clone)]
struct Quiet {
    start: Schedule,
    end: Schedule,
}

#[cfg(feature = "heartbeat")]
impl Quiet {
    fn new(hours: QuietHours, timezone: &str) -> Self {
        let at = |minutes: u16| Schedule::CronInTimezone {
            expr: format!("{} {} * * *", minutes % 60, minutes / 60),
            tz: timezone.to_owned(),
        };
        Self {
            start: at(hours.start),
            end: at(hours.end),
        }
    }

    /// `at`, or the end of the quiet window that holds it. A time is quiet
    /// when the window ends before it starts again.
    fn after(&self, at: Timestamp) -> Option<Timestamp> {
        let end = next_fire_after(&self.end, at)?;
        let start = next_fire_after(&self.start, at)?;
        Some(if end < start { end } else { at })
    }
}

/// The heartbeat part of the plan.
#[cfg(feature = "heartbeat")]
#[derive(Debug, Clone)]
struct Beat {
    every: jiff::SignedDuration,
    quiet: Option<Quiet>,
    prompt: String,
}

#[cfg(feature = "heartbeat")]
impl Beat {
    fn next(&self, now: Timestamp) -> Option<Timestamp> {
        let at = now + self.every;
        self.quiet
            .as_ref()
            .map_or(Some(at), |quiet| quiet.after(at))
    }
}

/// The brief part of the plan.
#[derive(Debug, Clone)]
struct Brief {
    schedule: Schedule,
    prompt: String,
}

/// The plan of every wake.
#[derive(Debug, Clone)]
pub struct Wake {
    brief: Option<Brief>,
    #[cfg(feature = "heartbeat")]
    beat: Option<Beat>,
    /// The pass that waits for its reflection, and the prompt that says
    /// what it did.
    #[cfg(feature = "reflection")]
    reflect: Option<(String, String)>,
    /// The next wake that the assistant booked.
    #[cfg(feature = "followups")]
    followup: Option<Timestamp>,
    armed: Option<Armed>,
    /// Whether a brief is armed now. The brief delivery reads it, so a
    /// heartbeat answer never goes out as a brief.
    brief_armed: Arc<AtomicBool>,
}

impl Wake {
    /// The plan of `brief` and `beat`, or `None` with neither. Nothing is
    /// armed yet. `brief_armed` is shared with the brief delivery.
    ///
    /// # Errors
    ///
    /// Returns an error when a schedule has no time after `now`, for
    /// example a bad cron field or an unknown time zone.
    pub fn new(
        brief: Option<&BriefConfig>,
        beat: Option<&HeartbeatConfig>,
        brief_armed: Arc<AtomicBool>,
        now: Timestamp,
    ) -> Result<Option<Self>> {
        let brief = brief
            .map(|config| {
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
                Ok(Brief {
                    schedule,
                    prompt: marked(BRIEF_MARK, &config.prompt),
                })
            })
            .transpose()?;
        #[cfg(not(feature = "heartbeat"))]
        if beat.is_some() {
            bail!(crate::features::missing("[heartbeat]", "heartbeat"));
        }
        #[cfg(feature = "heartbeat")]
        let beat = beat
            .map(|config| {
                let quiet = config
                    .quiet
                    .map(|hours| Quiet::new(hours, &config.timezone));
                if quiet
                    .as_ref()
                    .is_some_and(|quiet| quiet.after(now).is_none())
                {
                    bail!(
                        "heartbeat.timezone `{}` is not a time zone",
                        config.timezone
                    );
                }
                Ok(Beat {
                    every: jiff::SignedDuration::from_mins(i64::from(config.every_minutes)),
                    quiet,
                    prompt: marked(HEARTBEAT_MARK, &config.prompt),
                })
            })
            .transpose()?;
        #[cfg(not(feature = "heartbeat"))]
        let no_beat = true;
        #[cfg(feature = "heartbeat")]
        let no_beat = beat.is_none();
        if brief.is_none() && no_beat {
            return Ok(None);
        }
        Ok(Some(Self {
            brief,
            #[cfg(feature = "heartbeat")]
            beat,
            #[cfg(feature = "reflection")]
            reflect: None,
            #[cfg(feature = "followups")]
            followup: None,
            armed: None,
            brief_armed,
        }))
    }

    /// A plan with no brief and no heartbeat, for a reflection or a booked
    /// wake alone.
    #[cfg(any(feature = "reflection", feature = "followups"))]
    #[must_use]
    pub const fn idle(brief_armed: Arc<AtomicBool>) -> Self {
        Self {
            brief: None,
            #[cfg(feature = "heartbeat")]
            beat: None,
            #[cfg(feature = "reflection")]
            reflect: None,
            #[cfg(feature = "followups")]
            followup: None,
            armed: None,
            brief_armed,
        }
    }

    /// Sets the next wake that the assistant booked, at `now`. A sooner
    /// wake replaces the arm that waits, and so does a change to the
    /// follow-up that is armed.
    #[cfg(feature = "followups")]
    pub fn set_followup(&mut self, next: Option<Timestamp>, now: Timestamp) {
        if next == self.followup {
            return;
        }
        self.followup = next;
        let replace = self.armed.is_some_and(|armed| {
            armed.at > now && (armed.kind == Kind::Followup || next.is_some_and(|at| at < armed.at))
        });
        if replace {
            self.armed = None;
        }
    }

    /// The time of the armed wake, when its tick ran: it passed, and the
    /// tick count moved since the arm.
    #[cfg(any(feature = "followups", feature = "self-debug"))]
    #[must_use]
    pub fn ran(&self, now: Timestamp, ticks: u64) -> Option<(Timestamp, Kind)> {
        self.armed
            .filter(|armed| armed.at <= now && ticks > armed.ticks)
            .map(|armed| (armed.at, armed.kind))
    }

    /// The pass whose reflection is planned but not armed yet, and its
    /// prompt.
    #[cfg(feature = "reflection")]
    #[must_use]
    pub fn pending_reflection(&self) -> Option<(String, String)> {
        self.reflect.clone()
    }

    /// Plans one reflection on the consolidation pass `pass` with `prompt`,
    /// as the next wake. It replaces the arm that waits, which the next arm
    /// books again. `pending` is what the reflection precheck reads.
    #[cfg(feature = "reflection")]
    pub fn reflect(&mut self, pass: &str, prompt: &str, pending: &PendingPass) {
        self.reflect = Some((pass.to_owned(), prompt.to_owned()));
        pending.set(pass);
        self.armed = None;
    }

    /// Forgets the arm that waits, so the next [`arm`](Self::arm) arms
    /// again. The daemon calls it when the engine dropped the arm.
    pub const fn forget_arm(&mut self) {
        self.armed = None;
    }

    /// The wake that the engine timer aims at, once armed.
    #[cfg(test)]
    #[must_use]
    pub fn armed_at(&self) -> Option<(Timestamp, Kind)> {
        self.armed.map(|armed| (armed.at, armed.kind))
    }

    /// Whether the daemon must read the tick count before the next
    /// [`arm`](Self::arm): the armed wake has passed.
    #[must_use]
    pub fn waits_for_tick(&self, now: Timestamp) -> bool {
        self.armed.is_some_and(|armed| armed.at <= now)
    }

    /// The arm of the next wake, when the engine timer needs one. `ticks`
    /// is the tick count of the assistant.
    ///
    /// - With nothing armed, it arms the first wake after `now`.
    /// - Before the armed wake, it arms nothing.
    /// - After the wake, it arms the next one once the tick ran, or once
    ///   [`GRACE`] passed. So a wake that passes while the daemon is busy
    ///   still runs one tick, not one per missed wake.
    ///
    /// A brief slot wins a tie with a heartbeat.
    pub fn arm(&mut self, now: Timestamp, ticks: u64) -> Option<Arm> {
        if let Some(armed) = self.armed {
            let ran = ticks > armed.ticks;
            if now < armed.at || (!ran && now < armed.at + GRACE) {
                return None;
            }
        }
        // A reflection comes as soon as the engine allows. Its arm names that
        // time, so the daemon reads no state before the tick can run.
        #[cfg(any(feature = "reflection", feature = "followups"))]
        let soonest =
            now + jiff::SignedDuration::from_secs(MIN_CUSTOM_HEARTBEAT_SECS.cast_signed());
        #[cfg(feature = "reflection")]
        let reflect = self.reflect.as_ref().map(|_| (soonest, Kind::Reflection));
        #[cfg(not(feature = "reflection"))]
        let reflect = None;
        #[cfg(feature = "followups")]
        let followup = self.followup.map(|at| (at.max(soonest), Kind::Followup));
        #[cfg(not(feature = "followups"))]
        let followup = None;
        let brief = self
            .brief
            .as_ref()
            .and_then(|brief| Some((next_fire_after(&brief.schedule, now)?, Kind::Brief)));
        #[cfg(feature = "heartbeat")]
        let beat = self
            .beat
            .as_ref()
            .and_then(|beat| Some((beat.next(now)?, Kind::Heartbeat)));
        #[cfg(not(feature = "heartbeat"))]
        let beat = None;
        // The earliest wins, and a brief wins a tie with a heartbeat.
        let next = [reflect, followup, brief, beat]
            .into_iter()
            .flatten()
            .reduce(|first, other| if other.0 < first.0 { other } else { first });
        let Some((at, kind)) = next else {
            // A wake ran, and nothing else is planned: stop the timer once.
            return self.armed.take().map(|_| off(now));
        };
        self.armed = Some(Armed { at, kind, ticks });
        self.brief_armed
            .store(kind == Kind::Brief, Ordering::SeqCst);
        let every = interval(now, at);
        let (prefix, heartbeat) = match kind {
            Kind::Brief => {
                let prompt = self
                    .brief
                    .as_ref()
                    .map_or("", |brief| brief.prompt.as_str());
                (String::from(BRIEF_KEY), heartbeat(prompt, every))
            }
            #[cfg(feature = "heartbeat")]
            Kind::Heartbeat => {
                let prompt = self.beat.as_ref().map_or("", |beat| beat.prompt.as_str());
                (
                    String::from(HEARTBEAT_KEY),
                    heartbeat(prompt, every).precheck(),
                )
            }
            #[cfg(feature = "followups")]
            Kind::Followup => (
                String::from(FOLLOWUP_KEY),
                heartbeat(&marked(FOLLOWUP_MARK, FOLLOWUP_PROMPT), every),
            ),
            #[cfg(feature = "reflection")]
            Kind::Reflection => {
                let (pass, prompt) = self.reflect.take().unwrap_or_default();
                // The precheck runs it only while its pass is pending.
                (
                    format!("{REFLECT_KEY}{pass}@"),
                    heartbeat(&marked(REFLECT_MARK, &prompt), every).precheck(),
                )
            }
        };
        Some(Arm::keyed(
            &prefix,
            at,
            HeartbeatSetting::Custom { heartbeat },
        ))
    }
}

/// The time from `now` to `at`, in whole seconds, rounded up. The engine
/// refuses an interval under [`MIN_CUSTOM_HEARTBEAT_SECS`], so a wake that
/// close runs that much late.
fn interval(now: Timestamp, at: Timestamp) -> Duration {
    let millis = u64::try_from(at.duration_since(now).as_millis()).unwrap_or(0);
    Duration::from_secs(millis.div_ceil(1_000).max(MIN_CUSTOM_HEARTBEAT_SECS))
}

/// The precheck of a tick.
///
/// - A heartbeat runs only when a goal is due, or, with `research`, when an
///   idea is open. With no store of goals and ideas, every heartbeat runs.
/// - A reflection runs only while its pass is pending, and it takes the
///   pass. So an engine timer that keeps the reflection setting by mistake
///   costs no model call after the first tick.
/// - Any other tick always runs.
#[cfg(any(feature = "heartbeat", feature = "reflection"))]
#[derive(Debug)]
pub struct Due {
    /// The goals and ideas, when they are on.
    #[cfg(feature = "heartbeat")]
    pub plans: Option<Arc<SqliteStores>>,
    /// Research open ideas when no goal is due.
    #[cfg(feature = "heartbeat")]
    pub research: bool,
    /// Where the decision goes, for the tick log.
    #[cfg(feature = "heartbeat")]
    pub verdict: Arc<Verdict>,
    /// The pass that waits for its reflection.
    #[cfg(feature = "reflection")]
    pub pass: Arc<PendingPass>,
}

#[cfg(feature = "heartbeat")]
impl Due {
    fn worth_it(&self, now_ms: i64) -> bool {
        let Some(plans) = &self.plans else {
            return true;
        };
        let due = plans.due_goals(now_ms).map(|due| !due.is_empty());
        let ideas = || plans.plans().is_ok_and(|plans| !plans.ideas.is_empty());
        // A store that fails runs the tick, which then says why.
        due.unwrap_or(true) || (self.research && ideas())
    }
}

#[cfg(any(feature = "heartbeat", feature = "reflection"))]
impl Precheck for Due {
    fn should_run<'a>(&'a self, task: &'a HeartbeatTask) -> BoxFuture<'a, bool> {
        Box::pin(async move {
            #[cfg(feature = "reflection")]
            if task.prompt.starts_with(REFLECT_MARK) {
                let pass = self.pass.take();
                if pass.is_none() {
                    tracing::warn!(
                        "a reflection tick came with no pass pending: the engine timer \
                         holds a reflection setting that it should not, so the tick is skipped"
                    );
                }
                return pass.is_some();
            }
            #[cfg(feature = "heartbeat")]
            if task.prompt.starts_with(HEARTBEAT_MARK) {
                let run = self.worth_it(Timestamp::now().as_millisecond());
                self.verdict.set(run);
                return run;
            }
            true
        })
    }
}

#[cfg(test)]
mod tests {
    use littleguy_core::QuietHours;
    use littleguy_core::config::{
        DEFAULT_BRIEF_PROMPT, DEFAULT_BRIEF_SCHEDULE, DEFAULT_HEARTBEAT_PROMPT, HeartbeatDelivery,
    };

    use super::*;

    fn brief(schedule: &str, timezone: &str) -> BriefConfig {
        BriefConfig {
            schedule: String::from(schedule),
            timezone: String::from(timezone),
            prompt: String::from(DEFAULT_BRIEF_PROMPT),
            to: None,
        }
    }

    fn beat(every_minutes: u32, quiet: Option<(u16, u16)>) -> HeartbeatConfig {
        HeartbeatConfig {
            every_minutes,
            quiet: quiet.map(|(start, end)| QuietHours { start, end }),
            timezone: String::from("UTC"),
            research: true,
            deliver: HeartbeatDelivery::Web,
            prompt: String::from(DEFAULT_HEARTBEAT_PROMPT),
        }
    }

    fn at(text: &str) -> Timestamp {
        text.parse::<Timestamp>().unwrap()
    }

    fn plan(brief: Option<&BriefConfig>, beat: Option<&HeartbeatConfig>, now: Timestamp) -> Wake {
        Wake::new(brief, beat, Arc::new(AtomicBool::new(false)), now)
            .unwrap()
            .unwrap()
    }

    fn set(arm: &Arm) -> &AgentHeartbeat {
        match &arm.op {
            AgentOp::Control(Control::SetHeartbeat {
                setting: HeartbeatSetting::Custom { heartbeat },
            }) => heartbeat,
            other => panic!("not a heartbeat: {other:?}"),
        }
    }

    /// The part of a key that names the wake, less the digest of what the
    /// arm sets.
    fn base(key: &str) -> &str {
        key.split_once('#').map_or(key, |(wake, _)| wake)
    }

    #[test]
    fn nothing_to_plan_is_no_plan() {
        let flag = Arc::new(AtomicBool::new(false));
        assert!(
            Wake::new(None, None, flag, Timestamp::now())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn the_brief_skips_the_weekend() {
        // Friday 6 Nov 2026, after 07:30 in London (UTC in winter).
        let now = at("2026-11-06T08:00:00Z");
        let mut wake = plan(
            Some(&brief(DEFAULT_BRIEF_SCHEDULE, "Europe/London")),
            None,
            now,
        );
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(base(&arm.key), "brief:2026-11-09T07:30:00+00:00");
        assert_eq!(
            wake.armed_at(),
            Some((at("2026-11-09T07:30:00Z"), Kind::Brief))
        );
        // From Friday 08:00 to Monday 07:30.
        assert_eq!(set(&arm).every_secs, (2 * 24 + 23) * 3600 + 30 * 60);
        assert!(!set(&arm).precheck, "a brief always runs");
        assert!(set(&arm).prompt.starts_with(BRIEF_MARK));
        assert!(!wake.waits_for_tick(at("2026-11-09T07:29:59Z")));
        assert_eq!(wake.arm(at("2026-11-09T07:29:59Z"), 0), None);
    }

    #[test]
    fn the_next_wake_waits_for_the_tick_or_the_grace() {
        let start = at("2026-11-01T09:00:00Z");
        let mut wake = plan(Some(&brief("0 8 * * *", "UTC")), None, start);
        assert!(wake.arm(start, 3).is_some());
        let after = at("2026-11-02T08:00:01Z");
        assert!(wake.waits_for_tick(after));
        assert_eq!(wake.arm(after, 3), None, "the tick has not run yet");
        let arm = wake.arm(after, 4).unwrap();
        assert_eq!(base(&arm.key), "brief:2026-11-03T08:00:00+00:00");
        assert_eq!(set(&arm).every_secs, 24 * 3600 - 1);
        // Three slots pass with no tick. One arm aims at the next slot.
        let late = at("2026-11-06T09:00:00Z");
        let arm = wake.arm(late, 4).unwrap();
        assert_eq!(base(&arm.key), "brief:2026-11-07T08:00:00+00:00");
        assert_eq!(wake.arm(late, 4), None);
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn a_heartbeat_asks_the_precheck_and_beats_from_its_last_arm() {
        let now = at("2026-11-14T12:00:00Z");
        let flag = Arc::new(AtomicBool::new(true));
        let mut wake = Wake::new(None, Some(&beat(30, None)), Arc::clone(&flag), now)
            .unwrap()
            .unwrap();
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-14T12:30:00+00:00");
        assert_eq!(set(&arm).every_secs, 30 * 60);
        assert!(set(&arm).precheck);
        assert!(!set(&arm).allow_actions && !set(&arm).allow_memory_writes);
        assert!(set(&arm).prompt.starts_with(HEARTBEAT_MARK));
        assert!(!flag.load(Ordering::SeqCst), "a heartbeat is not a brief");
        // The tick ran a little late. The next beat counts from the arm.
        let arm = wake.arm(at("2026-11-14T12:31:00Z"), 1).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-14T13:01:00+00:00");
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn the_brief_wins_when_it_comes_first_and_sets_the_flag() {
        let now = at("2026-11-02T07:00:00Z");
        let flag = Arc::new(AtomicBool::new(false));
        let mut wake = Wake::new(
            Some(&brief("30 7 * * *", "UTC")),
            Some(&beat(60, None)),
            Arc::clone(&flag),
            now,
        )
        .unwrap()
        .unwrap();
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(base(&arm.key), "brief:2026-11-02T07:30:00+00:00");
        assert!(flag.load(Ordering::SeqCst));
        // After the brief, the heartbeat is next.
        let arm = wake.arm(at("2026-11-02T07:30:05Z"), 1).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-02T08:30:05+00:00");
        assert!(!flag.load(Ordering::SeqCst));
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn quiet_hours_push_a_beat_to_their_end() {
        let quiet = beat(60, Some((22 * 60, 8 * 60)));
        let mut wake = plan(None, Some(&quiet), at("2026-11-14T21:30:00Z"));
        // 22:30 is quiet, so the beat waits until 08:00.
        let arm = wake.arm(at("2026-11-14T21:30:00Z"), 0).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-15T08:00:00+00:00");
        // 20:00 to 21:00 is outside the window.
        let mut wake = plan(None, Some(&quiet), at("2026-11-14T20:00:00Z"));
        let arm = wake.arm(at("2026-11-14T20:00:00Z"), 0).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-14T21:00:00+00:00");
        // A window inside one day: 12:00 to 13:00.
        let lunch = beat(60, Some((12 * 60, 13 * 60)));
        let mut wake = plan(None, Some(&lunch), at("2026-11-14T11:30:00Z"));
        let arm = wake.arm(at("2026-11-14T11:30:00Z"), 0).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-14T13:00:00+00:00");
    }

    #[test]
    fn a_close_wake_waits_the_engine_minimum() {
        let now = at("2026-11-02T07:59:30Z");
        let mut wake = plan(Some(&brief("0 8 * * *", "UTC")), None, now);
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(set(&arm).every_secs, MIN_CUSTOM_HEARTBEAT_SECS);
    }

    #[test]
    fn a_bad_schedule_or_zone_is_refused() {
        let now = at("2026-11-09T00:00:00Z");
        let flag = || Arc::new(AtomicBool::new(false));
        let error = Wake::new(
            Some(&brief("30 7 * * *", "Mars/Olympus")),
            None,
            flag(),
            now,
        )
        .unwrap_err();
        assert!(error.to_string().contains("brief.schedule"));
        assert!(Wake::new(Some(&brief("99 7 * * *", "UTC")), None, flag(), now).is_err());
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn a_bad_heartbeat_zone_is_refused() {
        let now = at("2026-11-09T00:00:00Z");
        let flag = || Arc::new(AtomicBool::new(false));
        let mut martian = beat(60, Some((60, 120)));
        martian.timezone = String::from("Mars/Olympus");
        let error = Wake::new(None, Some(&martian), flag(), now).unwrap_err();
        assert!(error.to_string().contains("heartbeat.timezone"));
    }

    #[cfg(feature = "reflection")]
    #[test]
    fn a_reflection_comes_first_runs_once_and_the_timer_stops_after_it() {
        let now = at("2026-11-14T03:05:00Z");
        let flag = Arc::new(AtomicBool::new(true));
        let mut wake = Wake::idle(Arc::clone(&flag));
        let pending = PendingPass::default();
        assert_eq!(wake.arm(now, 0), None, "nothing planned, nothing armed");
        wake.reflect("pass-7", "Two facts.", &pending);
        assert_eq!(
            wake.pending_reflection(),
            Some((String::from("pass-7"), String::from("Two facts.")))
        );
        assert_eq!(pending.get().as_deref(), Some("pass-7"));
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(wake.pending_reflection(), None);
        assert_eq!(
            pending.get().as_deref(),
            Some("pass-7"),
            "the pass waits for its tick, not for its arm"
        );
        assert_eq!(base(&arm.key), "reflect:pass-7@2026-11-14T03:06:00+00:00");
        assert_eq!(set(&arm).every_secs, MIN_CUSTOM_HEARTBEAT_SECS);
        assert!(
            set(&arm).precheck,
            "a reflection asks whether its pass waits"
        );
        assert!(set(&arm).prompt.starts_with(REFLECT_MARK));
        assert!(!flag.load(Ordering::SeqCst), "a reflection is not a brief");
        assert!(!wake.waits_for_tick(at("2026-11-14T03:05:59Z")));
        // The tick ran. Nothing else is planned, so the timer stops, once.
        let later = at("2026-11-14T03:06:01Z");
        let stop = wake.arm(later, 1).unwrap();
        assert!(stop.key.starts_with(OFF_KEY));
        assert_eq!(wake.arm(later, 1), None);
    }

    #[cfg(all(feature = "heartbeat", feature = "reflection"))]
    #[test]
    fn a_reflection_goes_before_an_armed_heartbeat_which_comes_back() {
        let now = at("2026-11-14T03:00:00Z");
        let mut wake = plan(None, Some(&beat(60, None)), now);
        assert!(wake.arm(now, 0).unwrap().key.starts_with(HEARTBEAT_KEY));
        wake.reflect("p", "A fact.", &PendingPass::default());
        let soon = at("2026-11-14T03:05:00Z");
        assert!(wake.arm(soon, 0).unwrap().key.starts_with("reflect:p@"));
        let arm = wake.arm(at("2026-11-14T03:06:01Z"), 1).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-14T04:06:01+00:00");
    }

    /// The nightly pass ends in quiet hours, on top of the heartbeat that
    /// waits for the end of the window. The arm after the reflection aims
    /// at the same heartbeat, with the same key as its first arm. The
    /// daemon sees the engine drop it while the reflection setting holds,
    /// and the arm goes out again under a key that names that setting.
    #[cfg(all(feature = "heartbeat", feature = "reflection"))]
    #[test]
    fn the_heartbeat_after_a_reflection_in_quiet_hours_goes_out_again() {
        let quiet = beat(60, Some((22 * 60, 8 * 60)));
        let night = at("2026-11-21T02:30:00Z");
        let mut wake = plan(None, Some(&quiet), night);
        let first = wake.arm(night, 0).unwrap();
        assert_eq!(base(&first.key), "heartbeat:2026-11-21T08:00:00+00:00");

        let pending = PendingPass::default();
        wake.reflect("pass-1", "A fact.", &pending);
        let pass_end = at("2026-11-21T03:00:00Z");
        let reflection = wake.arm(pass_end, 0).unwrap();
        assert!(set(&reflection).prompt.starts_with(REFLECT_MARK));
        // The reflection tick ran, so the next arm comes.
        let next = wake.arm(at("2026-11-21T03:01:30Z"), 1).unwrap();
        assert!(set(&next).prompt.starts_with(HEARTBEAT_MARK));
        assert_eq!(next.key, first.key, "the same wake and the same setting");

        // The engine holds the reflection, not what the arm sets.
        let held = reflection.setting().unwrap();
        assert!(!next.holds(held));
        let again = next.over(held);
        assert_ne!(again.key, first.key);
        assert!(again.key.starts_with(&first.key));
        assert_eq!(again.op, next.op);
        assert_eq!(again, next.over(held), "a retry keeps its key");
        // Once applied, the engine holds what the arm sets.
        assert!(again.holds(next.setting().unwrap()));
    }

    #[cfg(feature = "heartbeat")]
    #[test]
    fn two_arms_at_one_wake_that_set_different_prompts_get_two_keys() {
        let now = at("2026-11-14T12:00:00Z");
        let mut first = beat(60, None);
        first.prompt = String::from("check in");
        let mut second = first.clone();
        second.prompt = String::from("look around");
        let a = plan(None, Some(&first), now).arm(now, 0).unwrap();
        let b = plan(None, Some(&second), now).arm(now, 0).unwrap();
        assert_eq!(base(&a.key), base(&b.key), "one wake");
        assert_ne!(a.key, b.key, "two settings");
        assert!(!a.holds(b.setting().unwrap()));
        // The same plan arms the same key, also in a new process.
        let again = plan(None, Some(&first), now).arm(now, 0).unwrap();
        assert_eq!(a.key, again.key);
        assert_eq!(
            a.key.len() - base(&a.key).len(),
            17,
            "a hash sign and 16 hex digits"
        );
    }

    #[test]
    fn the_key_leaves_the_interval_out_and_holds_ignores_narrowing() {
        let one = heartbeat("[littleguy brief]\nhi", Duration::from_secs(60));
        let mut other = one.clone();
        other.every_secs = 3600;
        let custom = |heartbeat: AgentHeartbeat| HeartbeatSetting::Custom { heartbeat };
        assert_eq!(digest(&custom(one.clone())), digest(&custom(other.clone())));
        // The engine narrows a custom heartbeat. The arm still holds.
        let arm = Arm::keyed(BRIEF_KEY, at("2026-11-14T00:00:00Z"), custom(one.clone()));
        other.max_steps = Some(8);
        assert!(arm.holds(&custom(other)));
        assert!(!arm.holds(&HeartbeatSetting::Off));
        assert!(!arm.holds(&HeartbeatSetting::Profile));
        let mut prechecked = one;
        prechecked.precheck = true;
        assert!(!arm.holds(&custom(prechecked)));
        let stop = off(at("2026-11-14T00:00:00Z"));
        assert!(stop.holds(&HeartbeatSetting::Off));
        assert_ne!(
            digest(&HeartbeatSetting::Off),
            digest(&HeartbeatSetting::Profile)
        );
    }

    #[test]
    fn a_forgotten_arm_arms_again() {
        let now = at("2026-11-02T07:00:00Z");
        let mut wake = plan(Some(&brief("0 8 * * *", "UTC")), None, now);
        let first = wake.arm(now, 0).unwrap();
        assert_eq!(wake.arm(now, 0), None);
        wake.forget_arm();
        assert_eq!(wake.arm(now, 0), Some(first));
    }

    #[cfg(feature = "reflection")]
    #[tokio::test]
    async fn a_reflection_runs_only_while_its_pass_is_pending() {
        let pending = Arc::new(PendingPass::default());
        let due = Due {
            #[cfg(feature = "heartbeat")]
            plans: None,
            #[cfg(feature = "heartbeat")]
            research: false,
            #[cfg(feature = "heartbeat")]
            verdict: Arc::default(),
            pass: Arc::clone(&pending),
        };
        let reflection = HeartbeatTask {
            prompt: marked(REFLECT_MARK, "Two facts."),
            ..HeartbeatTask::default()
        };
        assert!(
            !due.should_run(&reflection).await,
            "no pass waits, so no model call"
        );
        let mut wake = Wake::idle(Arc::default());
        wake.reflect("pass-7", "Two facts.", &pending);
        assert!(due.should_run(&reflection).await, "the pass waits");
        assert_eq!(pending.get(), None, "the tick took the pass");
        // An engine timer that keeps the reflection setting ticks again.
        assert!(
            !due.should_run(&reflection).await,
            "the same pass runs once"
        );
        // A brief or a follow-up never asks for a pass.
        let brief = HeartbeatTask {
            prompt: marked(BRIEF_MARK, "go"),
            ..HeartbeatTask::default()
        };
        assert!(due.should_run(&brief).await);
        pending.set("pass-8");
        assert_eq!(pending.take().as_deref(), Some("pass-8"));
        assert!(!due.should_run(&reflection).await, "a cleared pass");
    }

    #[cfg(feature = "heartbeat")]
    #[cfg(feature = "followups")]
    #[test]
    fn a_booked_wake_comes_before_the_heartbeat_and_reports_its_run() {
        let now = at("2026-11-14T12:00:00Z");
        let mut wake = plan(None, Some(&beat(60, None)), now);
        assert!(wake.arm(now, 0).unwrap().key.starts_with(HEARTBEAT_KEY));
        // A wake at 12:20 is sooner than the 13:00 beat, so it takes the arm.
        wake.set_followup(Some(at("2026-11-14T12:20:00Z")), now);
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(base(&arm.key), "followup:2026-11-14T12:20:00+00:00");
        assert!(set(&arm).prompt.starts_with(FOLLOWUP_MARK));
        assert!(
            set(&arm).prompt.ends_with(FOLLOWUP_PROMPT),
            "a fixed prompt"
        );
        assert!(!set(&arm).precheck);
        // The same wake again changes nothing.
        wake.set_followup(Some(at("2026-11-14T12:20:00Z")), now);
        assert_eq!(wake.arm(now, 0), None);

        let after = at("2026-11-14T12:20:30Z");
        assert_eq!(wake.ran(after, 0), None, "the tick has not run");
        assert_eq!(
            wake.ran(after, 1),
            Some((at("2026-11-14T12:20:00Z"), Kind::Followup))
        );
        // The daemon marks it done; no wake is left, so the beat is next.
        wake.set_followup(None, after);
        let arm = wake.arm(after, 1).unwrap();
        assert_eq!(base(&arm.key), "heartbeat:2026-11-14T13:20:30+00:00");
    }

    #[cfg(feature = "followups")]
    #[test]
    fn a_wake_due_now_waits_the_engine_minimum() {
        let now = at("2026-11-14T12:00:00Z");
        let mut wake = Wake::idle(Arc::new(AtomicBool::new(false)));
        wake.set_followup(Some(now), now);
        let arm = wake.arm(now, 0).unwrap();
        assert_eq!(base(&arm.key), "followup:2026-11-14T12:01:00+00:00");
    }

    #[test]
    fn ticks_are_known_by_their_mark() {
        assert!(is_tick(&marked(FOLLOWUP_MARK, "x")));
        assert!(is_tick(&marked(REFLECT_MARK, "x")));
        assert!(is_tick(&marked(HEARTBEAT_MARK, "x")));
        assert!(is_tick(&marked(BRIEF_MARK, "x")));
        assert!(!is_tick("hello [littleguy heartbeat]"));
        assert_eq!(
            off(at("2026-11-14T00:00:00Z")).key,
            "wake-off:2026-11-14T00:00:00+00:00"
        );
    }

    #[cfg(not(feature = "heartbeat"))]
    #[test]
    fn a_build_without_the_heartbeat_refuses_one() {
        let error = Wake::new(
            None,
            Some(&beat(60, None)),
            Arc::default(),
            at("2026-11-14T00:00:00Z"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("`heartbeat` feature"));
    }

    #[cfg(feature = "heartbeat")]
    #[tokio::test]
    async fn the_precheck_runs_a_heartbeat_only_with_work_to_do() {
        let stores = Arc::new(SqliteStores::open_in_memory().unwrap());
        let task = |prompt: &str| HeartbeatTask {
            prompt: prompt.to_owned(),
            ..HeartbeatTask::default()
        };
        let heartbeat = task(&marked(HEARTBEAT_MARK, "go"));
        let brief = task(&marked(BRIEF_MARK, "go"));
        let verdict = Arc::new(Verdict::default());
        let due = Due {
            plans: Some(Arc::clone(&stores)),
            research: true,
            verdict: Arc::clone(&verdict),
            #[cfg(feature = "reflection")]
            pass: Arc::default(),
        };
        assert!(!due.should_run(&heartbeat).await, "nothing to do");
        assert_eq!(verdict.take_skipped(), Some(true));
        assert_eq!(verdict.take_skipped(), None, "taken once");
        assert!(due.should_run(&brief).await, "a brief always runs");
        assert_eq!(
            verdict.take_skipped(),
            None,
            "a brief asks no precheck verdict"
        );
        assert!(
            Due {
                plans: None,
                research: true,
                verdict: Arc::default(),
                #[cfg(feature = "reflection")]
                pass: Arc::default(),
            }
            .should_run(&heartbeat)
            .await,
            "with plans off, the heartbeat runs"
        );

        stores.add_idea("i", "An idea", "b", 0).unwrap();
        assert!(due.should_run(&heartbeat).await, "an idea waits");
        assert_eq!(verdict.take_skipped(), Some(false));
        // An answered idea waits for nothing, so it wakes no tick.
        stores.conclude_idea("c", 1, "Answered.", 1).unwrap();
        assert!(!due.should_run(&heartbeat).await, "an answered idea");
        assert_eq!(verdict.take_skipped(), Some(true));
        assert!(stores.reopen_idea(1).unwrap());
        assert!(due.should_run(&heartbeat).await, "a reopened idea waits");
        assert_eq!(verdict.take_skipped(), Some(false));
        let quiet_research = Due {
            plans: Some(Arc::clone(&stores)),
            research: false,
            verdict: Arc::default(),
            #[cfg(feature = "reflection")]
            pass: Arc::default(),
        };
        assert!(!quiet_research.should_run(&heartbeat).await);
        let goal = stores
            .propose_goal("g", "G", "w", Some(60), None, 0)
            .unwrap();
        stores.accept_goal(goal, 0).unwrap();
        assert!(quiet_research.should_run(&heartbeat).await, "a goal is due");
    }
}
