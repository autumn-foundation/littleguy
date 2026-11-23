//! `lg serve`: the daemon that owns the database and the agent.
//!
//! The daemon is the single writer of one SQLite file. The runtime takes a
//! lock file next to the database, so a second daemon fails to open it. The
//! kernel drops the lock when the holder dies, so a crash strands nothing.
//!
//! One task owns the runtime. The socket and the chat channels, Discord and
//! Slack, hand each message to it over a channel, so ops run one at a time,
//! in arrival order.
//! A tick drives the assistant once a second, so a durable timer fires
//! without a message.
//!
//! A chat reply goes out once. The message id is the idempotency key of
//! its op, so a message that arrives twice runs once. The pairing store
//! keeps the last message whose reply went out. After a reconnect, the
//! daemon reads the history after that cursor, so a message sent while it
//! was down still gets its answer.
//!
//! A `run_code` call waits for a person ([`crate::approvals`]). While it
//! waits, the daemon takes no new op. It holds each new chat message and
//! runs it after the decision. `lg chat` gets a busy answer instead.

use std::collections::{BTreeSet, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use autumn_harvest_agent::{
    AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile, Control, sqlite,
};
use autumn_harvest_sqlite::SqliteRuntime;
use jiff::Timestamp;
use littleguy_channels::{DmMessage, Service};
use littleguy_core::{
    ASSISTANT_KIND, ChannelId, ChannelRule, Config, Inbound, MessageId, PairingCode, Pending,
    PendingPairings, Route, Routing, SenderId, discord_sender, pairing::PAIRING_TTL_SECS, route,
    slack_sender,
};
use littleguy_stores::SqliteStores;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::approvals::Requester;
use crate::brief::BriefDelivery;
use crate::camel::Camel;
use crate::chats::Chats;
#[cfg(feature = "oauth")]
use crate::oauth::Connections;
use crate::pairs::{Entry, PairStore};
use crate::protocol::{
    MAX_REQUEST_BYTES, PairEntry, Request, Response, Speaker, Turn, WorkspaceEntry,
};
use crate::wake::Wake;

/// The channel id of the local socket.
pub const CLI_CHANNEL: &str = "cli";

/// The sender id of `user` on `service`.
///
/// # Errors
///
/// Returns an error when `user` is not a user id of the service.
pub fn chat_sender(service: Service, user: &str) -> Result<SenderId> {
    Ok(match service {
        Service::Discord => discord_sender(user)?,
        Service::Slack => slack_sender(user)?,
    })
}

/// The message id of one chat message, the idempotency key of its op. A
/// Discord snowflake is unique on its own. A Slack `ts` is unique in its
/// channel only, so the channel goes in the key.
///
/// # Errors
///
/// Returns an error when the key is not a valid message id.
pub fn chat_message_id(service: Service, message: &DmMessage) -> Result<MessageId> {
    let key = match service {
        Service::Discord => format!("{service}:{}", message.id),
        Service::Slack => format!("{service}:{}:{}", message.channel_id, message.id),
    };
    Ok(MessageId::new(key)?)
}

/// The delivery cursor in a chat message id: the part after its last `:`.
fn cursor_of(message_id: &MessageId) -> String {
    message_id
        .as_str()
        .rsplit_once(':')
        .map(|(_, cursor)| cursor.to_owned())
        .unwrap_or_default()
}

/// How often the daemon drives the assistant without a message.
pub const TICK: Duration = Duration::from_secs(1);

/// The sender id of a local user.
///
/// # Errors
///
/// Never in practice: the text is printable ASCII and short.
pub fn cli_sender(uid: u32) -> Result<SenderId> {
    Ok(SenderId::new(format!("uid:{uid}"))?)
}

/// The routing table of a daemon run by `owner_uid`.
///
/// The user who runs the daemon is the one allowed sender on the local
/// socket, which offers no pairing. Each chat service in `chats` allows the
/// senders of the config file and of the pairing store. Each camel route of
/// the config is an allowed sender of the camel channel.
///
/// # Errors
///
/// Returns an error when the owner name is not a valid agent name.
pub fn routing(
    config: &Config,
    owner_uid: u32,
    store: &PairStore,
    chats: &[Service],
) -> Result<Routing> {
    let cli = ChannelRule {
        enabled: true,
        pairing: false,
        allowed: std::iter::once(cli_sender(owner_uid)?).collect(),
    };
    let mut table = Routing::new(&config.owner)
        .with_context(|| format!("`{}` is not a valid agent name", config.owner))?
        .channel(ChannelId::new(CLI_CHANNEL)?, cli);
    for service in chats {
        let channel = ChannelId::new(service.name())?;
        let settings = chat_config(config, *service);
        let paired = store
            .entries()
            .filter(|e| e.paired && e.channel == channel)
            .map(|e| e.sender.clone());
        let rule = ChannelRule {
            enabled: true,
            pairing: settings.pairing,
            allowed: settings.allowed.iter().cloned().chain(paired).collect(),
        };
        table = table.channel(channel, rule);
    }
    #[cfg(feature = "camel")]
    if !config.camel.is_empty() {
        let rule = ChannelRule {
            enabled: true,
            pairing: false,
            allowed: crate::camel::senders(&config.camel)?.into_iter().collect(),
        };
        table = table.channel(ChannelId::new(crate::camel::CAMEL_CHANNEL)?, rule);
    }
    Ok(table)
}

/// Logs when `approver` is a user of a chat that is not in `services`.
fn warn_unreachable(approver: Option<&SenderId>, services: &[Service]) {
    let on = approver
        .and_then(|sender| Service::split_sender(sender.as_str()))
        .is_some_and(|(service, _)| services.contains(&service));
    if approver.is_some() && !on {
        tracing::warn!("approvals.approver is set, but its chat is off: decide with `lg approve`");
    }
}

/// The config section of `service`.
const fn chat_config(config: &Config, service: Service) -> &littleguy_core::ChatConfig {
    match service {
        Service::Discord => &config.discord,
        Service::Slack => &config.slack,
    }
}

/// The harness of the assistant. With `memory`, the assistant keeps notes
/// and an episode log in that store. `db` is the database that the
/// consolidation dispatcher reads. With `brief`, the delivery sends the
/// answer of each heartbeat tick to the owner. The daemon arms each tick.
/// `tools` gives the assistant its tools, and a workspace when the file
/// tools are on. `guard` adds the `run_code` tool, the flow rules and the
/// approval gate. With `[compaction]`, the assistant saves progress notes
/// near the request cap, and the engine resets its window (ADR 0043).
#[must_use]
pub fn harness(
    config: &Config,
    model: Arc<dyn AgentModel>,
    memory: Option<(Arc<SqliteStores>, &Path)>,
    brief: Option<Arc<BriefDelivery>>,
    tools: crate::tools::Tools,
    guard: Option<crate::guard::Guard>,
) -> AgentHarness {
    let mut profile = AgentProfile::new(ASSISTANT_KIND)
        .system(config.system.as_str())
        .window(config.window)
        // A tick with no cap of its own takes this one too. With no bound
        // in the config, an op runs until the model answers (ADR 0034).
        .max_steps(config.max_steps.unwrap_or(u32::MAX))
        // Each op ends its run, so the next op reads the profile again
        // (ADR 0035).
        .max_ops_per_run(runs::OPS_PER_RUN);
    // With no cap in the config, the engine cap applies.
    profile.max_request_bytes = config.max_request_bytes;
    // Near the request cap, the assistant saves its progress notes, and the
    // engine resets the window to the task and the notes (ADR 0043).
    #[cfg(feature = "compaction")]
    if let Some(section) = &config.compaction {
        profile = profile.context_reset(section.context_reset());
    }
    // Each tool cuts its own result, by a setting of the owner such as
    // `fetch_max_chars`. The engine cuts no result again (ADR 0034).
    let harness = AgentHarness::new(model).tool_output_limit(usize::MAX);
    let (mut harness, mut profile) = tools.apply(harness, profile);
    // The `sweep` tool reads many sources in read-only runs of their own
    // (ADR 0044).
    #[cfg(feature = "research")]
    if let Some(section) = &config.research {
        (harness, profile) = crate::research::apply(harness, profile, section);
    }
    if let Some(guard) = guard {
        (harness, profile) = guard.apply(harness, profile);
    } else {
        #[cfg(feature = "council")]
        if config.council.is_some() {
            // The council reads the taint of a run, and only flow rules
            // record it. No tool here can complete a rule that asks, or the
            // guard would be on, so the rules only keep the labels (ADR
            // 0027).
            let flow = config.flow.rules();
            if !flow.is_empty() {
                harness = harness.flow_rules(flow);
            }
        }
    }
    if let Some((stores, db)) = memory {
        harness = crate::memory::harness(harness, stores, db);
        profile = crate::memory::profile(profile, &config.memory);
    }
    if let Some(delivery) = brief {
        harness = harness.delivery(delivery);
        profile = profile.deliver();
    }
    harness.agent(profile)
}

/// The most bytes of one workspace file that a request shows.
pub const MAX_SHOWN_BYTES: usize = 256 * 1024;

/// The notes of the assistant in `stores`, for the owner (ADR 0029).
///
/// # Errors
///
/// Returns an error when the store fails.
#[cfg(feature = "notes")]
fn notes_listing(config: &Config, stores: Option<&SqliteStores>) -> Result<Response> {
    let Some(stores) = stores.filter(|_| config.tools.files && config.memory.enabled) else {
        return Ok(Response::Off {
            message: String::from(
                "notes are off: they need `files = true` under [tools] and memory on",
            ),
        });
    };
    let notes = stores
        .notes()?
        .into_iter()
        .map(|note| crate::protocol::NoteEntry {
            name: note.name,
            text: note.text,
            updated_ms: note.updated_ms,
        })
        .collect();
    Ok(Response::Notes { notes })
}

/// A build without notes: the listing is off.
#[cfg(not(feature = "notes"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the notes build reads the store, and the callers handle both the same way"
)]
fn notes_listing(_config: &Config, _stores: Option<&SqliteStores>) -> Result<Response> {
    Ok(Response::Off {
        message: crate::features::missing("notes", "notes"),
    })
}

/// The text of a workspace file, when it is UTF-8 and at most
/// [`MAX_SHOWN_BYTES`].
fn shown_text(data: &[u8]) -> Option<String> {
    if data.len() > MAX_SHOWN_BYTES {
        return None;
    }
    std::str::from_utf8(data).ok().map(str::to_owned)
}

/// Why an op stopped before its answer, in words for the owner.
fn stopped_early(stop: autumn_harvest_agent::AgentStop) -> String {
    use autumn_harvest_agent::AgentStop;
    match stop {
        AgentStop::StepsExhausted => {
            String::from("I used every tool round that one message gets. Ask me to go on")
        }
        AgentStop::TokensExhausted | AgentStop::BudgetExceeded => {
            String::from("I ran out of my token budget")
        }
        AgentStop::TranscriptFull => String::from("the conversation got too long"),
        AgentStop::LoopDetected => String::from("I kept making the same tool calls"),
        AgentStop::OutputCapped => String::from("my answer hit the output limit"),
        AgentStop::ReplyTooLarge => String::from("my answer was too large to send"),
        AgentStop::Interrupted => String::from("the op was interrupted"),
        other => format!("the op stopped early ({other:?})"),
    }
}

/// The answer to a person, from how the op stopped and its last text. An
/// op that stopped early says why, also when it wrote some text first. An
/// op with no text at all is no answer, never an empty reply.
fn answer_of(stop: Option<autumn_harvest_agent::AgentStop>, text: &str) -> Response {
    use autumn_harvest_agent::AgentStop;
    let text = text.trim();
    match stop {
        None | Some(AgentStop::Completed) if !text.is_empty() => Response::Reply {
            text: text.to_owned(),
            usage: None,
        },
        None | Some(AgentStop::Completed) => Response::NoAnswer {
            reason: String::from("the op ended without an answer"),
        },
        Some(stop) if text.is_empty() => Response::NoAnswer {
            reason: stopped_early(stop),
        },
        Some(stop) => Response::Reply {
            text: format!("{text}\n\n(I stopped early: {}.)", stopped_early(stop)),
            usage: None,
        },
    }
}

/// `response` with `usage`, when it is a reply.
fn with_usage(response: Response, usage: Option<crate::protocol::Usage>) -> Response {
    match response {
        Response::Reply { text, .. } => Response::Reply { text, usage },
        other => other,
    }
}

/// The run id of segment `segment` of the assistant, as the engine names
/// it.
fn run_id(state: &autumn_harvest_agent::AgentState, segment: u64) -> String {
    format!("{}:s{segment}", state.incarnation)
}

/// The answer to the person who asked `asked`, from the transcript: the
/// last text of the assistant after the newest message with that text, and
/// before the next message of anyone. A tick prompt is such a message, so a
/// tick answer never counts.
fn answer_in(transcript: &[autumn_harvest_agent::ChatMessage], asked: &str) -> Response {
    use autumn_harvest_agent::ChatRole;
    let asked = asked.trim();
    let answer = transcript
        .iter()
        .rposition(|message| message.role == ChatRole::User && message_text(message) == asked)
        .and_then(|start| {
            transcript[start + 1..]
                .iter()
                .take_while(|message| message.role != ChatRole::User)
                .filter(|message| message.role == ChatRole::Assistant)
                .map(message_text)
                .filter(|text| !text.is_empty())
                .last()
        });
    answer.map_or_else(
        || Response::NoAnswer {
            reason: String::from("the op ended without an answer"),
        },
        |text| Response::Reply { text, usage: None },
    )
}

/// The text parts of `message`, trimmed, with a blank line between them.
fn message_text(message: &autumn_harvest_agent::ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            autumn_harvest_agent::ContentPart::Text(text) => Some(text.trim()),
            _ => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// What a tick that ran did: its tool calls and its answer.
#[cfg(feature = "self-debug")]
fn ran_with(trace: &crate::trace::TickTrace) -> String {
    let tools = if trace.tools.is_empty() {
        String::from("no tool calls")
    } else {
        format!("called {}", trace.tools.join(", "))
    };
    let answer = if trace.answer.trim() == "HEARTBEAT_OK" {
        String::from("nothing to tell (HEARTBEAT_OK)")
    } else if trace.answer.is_empty() {
        String::from("no answer")
    } else {
        format!("answered: {}", trace.answer)
    };
    format!("ran; {tools}; {answer}")
}

/// How a tick went, in a few words. `skipped` is the verdict of the
/// heartbeat precheck, `finished` counts the booked wakes it ended, and
/// `trace` says what it did: its tool calls and its answer (ADR 0028).
#[cfg(feature = "self-debug")]
fn tick_outcome(
    skipped: Option<bool>,
    finished: usize,
    trace: Option<&crate::trace::TickTrace>,
) -> String {
    let ran = if skipped == Some(true) {
        String::from("skipped by the precheck: no goal due and no idea open; no model call")
    } else {
        trace.map_or_else(|| String::from("ran"), ran_with)
    };
    match finished {
        0 => ran,
        1 => format!("{ran}; ended 1 booked wake"),
        n => format!("{ran}; ended {n} booked wakes"),
    }
}

/// The time of the first pending wake in `stores`.
#[cfg(feature = "followups")]
fn next_wake(stores: &SqliteStores) -> Result<Option<Timestamp>> {
    Ok(stores
        .pending_wakes()?
        .first()
        .and_then(|wake| crate::stamp::from_millis(wake.at_ms)))
}

/// How often the daemon looks for consolidation passes to reflect on.
#[cfg(feature = "reflection")]
const PASS_LOOK_EVERY: jiff::SignedDuration = jiff::SignedDuration::from_mins(1);

/// How far back the daemon looks for a pass. A pass from before stays
/// without a reflection.
#[cfg(feature = "reflection")]
const PASS_LOOK_BACK: jiff::SignedDuration = jiff::SignedDuration::from_hours(24 * 2);

/// The seconds since the Unix epoch.
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

/// A fresh pairing code from the operating system's randomness.
fn fresh_code() -> PairingCode {
    let mut bytes = [0_u8; 5];
    if getrandom::fill(&mut bytes).is_err() {
        // No randomness: mix the clock in, so codes still differ. The owner
        // still approves each one, so a weak code admits no one alone.
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes()[..5]);
    }
    PairingCode::from_random(bytes)
}

/// The text that a stranger gets for `code`.
#[must_use]
pub fn pairing_text(code: Option<&str>) -> String {
    let minutes = PAIRING_TTL_SECS / 60;
    code.map_or_else(
        || String::from("littleguy cannot pair anyone right now. Try again in a few minutes."),
        |code| {
            format!(
                "Hi! littleguy does not know you yet. Ask its owner to run `lg pair {code}` \
                 in the next {minutes} minutes."
            )
        },
    )
}

/// One reply for a chat channel to send.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outgoing {
    /// The DM channel.
    pub channel_id: String,
    /// The text.
    pub text: String,
    /// The sender, when the reply moves its delivery cursor.
    pub track: Option<SenderId>,
    /// The message that this reply answers.
    pub message_id: String,
}

/// One cursor to read the missed history from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cursor {
    /// The DM channel.
    pub channel_id: String,
    /// The last message whose reply went out.
    pub after: String,
}

/// What [`Daemon::open_with`] adds to a config: the parts that need async
/// work or a test seam.
#[derive(Debug, Default)]
pub struct Extras {
    /// The runtime of the `run_code` tool.
    pub sandbox: Option<crate::sandbox::Runtime>,
    /// The tools of the MCP servers.
    pub mcp: Vec<Arc<dyn autumn_harvest_agent::Tool>>,
    /// The Google API of the owner, for the `[google]` tools.
    #[cfg(feature = "google")]
    pub google: Option<Arc<dyn littleguy_tools::google::GoogleApi>>,
    /// The API of the providers, for the `[api]` tools.
    #[cfg(feature = "api")]
    pub api: Option<crate::tools::ApiLink>,
    /// The Slack channel takes messages.
    pub slack: bool,
    /// The tenant database of a fleet, which holds the stores instead of
    /// `memory.db` (ADR 0017).
    pub tenant: Option<std::path::PathBuf>,
    /// The model of the reviewer of `approve = "auto"`. `None` takes the
    /// model of the assistant (ADR 0037).
    #[cfg(feature = "auto-approve")]
    pub reviewer: Option<Arc<dyn AgentModel>>,
    /// The secrets by variable name: the sealed house keys, then the
    /// environment (ADR 0040).
    pub keys: Arc<crate::house_keys::HouseKeys>,
}

/// The daemon state: the runtime, the routing table and the pairings.
pub struct Daemon {
    rt: SqliteRuntime,
    routing: Routing,
    owner_uid: u32,
    config_allowed: BTreeSet<SenderId>,
    pending: PendingPairings,
    store: PairStore,
    #[cfg(feature = "oauth")]
    connections: Option<Connections>,
    /// The plan of the brief and the heartbeat.
    wake: Option<Wake>,
    /// Whether the engine timer may still tick for a plan that went away.
    /// The next arm checks once.
    disarm: bool,
    /// When the daemon last looked for consolidation passes to reflect on.
    #[cfg(feature = "reflection")]
    passes_seen: Option<Timestamp>,
    /// The skills folder of a config with no `[skills] dir`.
    #[cfg(feature = "skills")]
    skills_default: Option<std::path::PathBuf>,
    /// The skills folder at the last sync. `None` syncs at the next tick.
    #[cfg(feature = "skills")]
    skills_stamp: Option<Vec<(std::path::PathBuf, std::time::SystemTime, u64)>>,
    chats: Arc<Chats>,
    clock: Clock,
    /// The store of the calls that wait, when a call can wait.
    approvals: Option<Arc<SqliteStores>>,
    /// The chat user who decides, such as `slack:U1`.
    approver: Option<SenderId>,
    /// The request that the assistant waits on.
    waiting: Option<i64>,
    /// The words of the message whose op is in flight, for the reviewer of
    /// `approve = "auto"`. The state holds an op only once it ends.
    op_words: Option<String>,
    /// The chat messages that arrived while a request waited.
    held: VecDeque<(Service, DmMessage)>,
    /// The sign-in links of the web pages, once they serve.
    logins: Option<crate::web::Logins>,
    /// The overlay store and the proposer, with a `[refine]` section.
    #[cfg(feature = "refine")]
    refine: Option<Arc<crate::refine::Refine>>,
    /// What a reload of the config needs, and the config that runs.
    kit: Box<reload::Kit>,
    /// The readers of the answers that the model writes now.
    live: Arc<live::Live>,
    /// Whether the run of the assistant must end before the next op, so
    /// that op reads a new profile (ADR 0035).
    roll_due: bool,
    /// The ends that this daemon asked for, for their idempotency keys.
    rolls: u64,
    /// When the last purge of the ended runs finished.
    purged_at: Option<Timestamp>,
    /// The reads that answer beside the loop.
    reads: Arc<Reads>,
}

/// The wall clock of the daemon. Tests set one clock for the daemon and its
/// runtime.
type Clock = Arc<dyn Fn() -> Timestamp + Send + Sync>;

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("assistant", self.routing.assistant())
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Opens the database at `db` and registers the assistant. With
    /// `discord`, the Discord channel takes messages. Slack stays off. It starts no MCP
    /// server. A test seam: `lg serve` calls [`Self::open_with`].
    ///
    /// # Errors
    ///
    /// Returns an error when another daemon holds the database, the database
    /// or the pairing store does not open, or the owner name is not a valid
    /// agent name.
    #[cfg(test)]
    pub fn open(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        owner_uid: u32,
        discord: bool,
    ) -> Result<Self> {
        let sandbox = config
            .sandbox
            .as_ref()
            .map(crate::sandbox::runtime)
            .transpose()?;
        let extras = Extras {
            sandbox,
            ..Extras::default()
        };
        Self::open_with(db, config, model, owner_uid, discord, extras)
    }

    /// [`Self::open`] with `extras`: the sandbox runtime, the MCP tools, the
    /// Google API and Slack. The sandbox runtime counts only with a
    /// `[sandbox]` section.
    ///
    /// # Errors
    ///
    /// As [`Self::open`].
    pub fn open_with(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        owner_uid: u32,
        discord: bool,
        extras: Extras,
    ) -> Result<Self> {
        if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut rt =
            SqliteRuntime::open(db).with_context(|| format!("opening {}", db.display()))?;
        let services: Vec<Service> = [(Service::Discord, discord), (Service::Slack, extras.slack)]
            .into_iter()
            .filter_map(|(service, on)| on.then_some(service))
            .collect();
        let chats = Arc::new(Chats::default());
        let live = Arc::new(live::Live::default());
        #[cfg(feature = "auto-approve")]
        let reviewer = extras.reviewer.unwrap_or_else(|| Arc::clone(&model));
        let mut kit = reload::Kit {
            db: db.to_path_buf(),
            model,
            #[cfg(feature = "auto-approve")]
            reviewer,
            sandbox: extras.sandbox,
            mcp: extras.mcp,
            #[cfg(feature = "google")]
            google: extras.google,
            #[cfg(feature = "api")]
            api: extras.api,
            #[cfg(feature = "patches")]
            patches: None,
            tenant: extras.tenant,
            keys: extras.keys,
            services,
            stores: None,
            config: config.clone(),
            file: None,
            watch_every: reload::WATCH_EVERY,
            live: Arc::clone(&live),
            brief_armed: Arc::default(),
            #[cfg(feature = "followups")]
            wake_links: crate::plans::WakeLinks::default(),
            #[cfg(feature = "heartbeat")]
            verdict: Arc::default(),
            #[cfg(feature = "skills")]
            skills_dir: crate::plans::SkillsDir::default(),
            untrusted: Arc::default(),
            #[cfg(feature = "self-debug")]
            boot: crate::config_view::Boot::this_process().clone(),
        };
        let built = kit.assemble(config, &chats)?;
        sqlite::register(&mut rt, Arc::new(built.harness));
        crate::memory::schedule(&mut rt, &config.memory)?;
        // The runtime lock is held now, so this daemon is the store's one writer.
        let store = PairStore::open(crate::pairs::path_for(db))?;
        // A request that waited before a restart still waits.
        let waiting = match &built.approvals {
            Some(stores) => stores.open_approvals()?.first().map(|record| record.id),
            None => None,
        };
        Ok(Self {
            routing: routing(config, owner_uid, &store, &kit.services)?,
            rt,
            owner_uid,
            config_allowed: reload::config_allowed(config),
            pending: PendingPairings::default(),
            store,
            #[cfg(feature = "oauth")]
            connections: None,
            wake: built.wake,
            disarm: true,
            #[cfg(feature = "reflection")]
            passes_seen: None,
            #[cfg(feature = "skills")]
            skills_default: None,
            #[cfg(feature = "skills")]
            skills_stamp: None,
            chats,
            clock: Arc::new(Timestamp::now),
            approvals: built.approvals,
            approver: built.approver,
            waiting,
            op_words: None,
            held: VecDeque::new(),
            logins: None,
            #[cfg(feature = "refine")]
            refine: built.refine,
            kit: Box::new(kit),
            live,
            // A run from before this start holds an old profile.
            roll_due: true,
            rolls: 0,
            purged_at: None,
            reads: Arc::default(),
        })
    }

    /// Sets the clock of the daemon and of its runtime. A test seam.
    #[cfg(all(test, any(feature = "discord", feature = "sandbox")))]
    fn with_clock(mut self, clock: impl Fn() -> Timestamp + Send + Sync + 'static) -> Self {
        let clock: Clock = Arc::new(clock);
        let runtime = Arc::clone(&clock);
        self.rt.set_clock(move || runtime());
        self.clock = clock;
        self
    }

    /// Keeps OAuth connections with `connections`.
    #[cfg(feature = "oauth")]
    #[must_use]
    pub fn with_connections(mut self, connections: Connections) -> Self {
        self.connections = Some(connections);
        self
    }

    /// Hands out sign-in links of the web pages with `logins`.
    #[must_use]
    pub fn with_logins(mut self, logins: crate::web::Logins) -> Self {
        self.logins = Some(logins);
        self
    }

    /// The reads that answer beside the loop. A [`Handle`] with them
    /// answers a read while a run holds the loop.
    #[must_use]
    pub fn reads(&self) -> Arc<Reads> {
        Arc::clone(&self.reads)
    }

    /// What a read needs, as the daemon has it now.
    fn view(&self) -> reads::View {
        reads::View {
            owner_uid: self.owner_uid,
            config: Arc::new(self.kit.config.clone()),
            stores: self.kit.stores.clone(),
            approvals: self.approvals.clone(),
            db: self.kit.db.clone(),
            assistant: self.assistant().clone(),
            pairs: self.pairs(),
            #[cfg(feature = "refine")]
            refine: self.refine.clone(),
            #[cfg(feature = "skills")]
            skills_dir: self.skills_dir(),
            #[cfg(feature = "oauth")]
            connections: self.connections.as_ref().map(Connections::lister),
            #[cfg(feature = "patches")]
            patches: self.kit.patches.clone(),
        }
    }

    /// Publishes what a read needs now, for the reads beside the loop.
    fn publish(&self) {
        self.reads.publish(self.view());
    }

    /// The readers of live answers. A [`Handle`] with them streams a chat.
    #[must_use]
    pub fn live(&self) -> Arc<live::Live> {
        Arc::clone(&self.live)
    }

    /// The agent that the owner talks to.
    #[must_use]
    pub const fn assistant(&self) -> &AgentId {
        self.routing.assistant()
    }

    /// The mapper of the camel routes of `config`. It checks each message
    /// against the routing table of this daemon.
    #[cfg(feature = "camel")]
    #[must_use]
    pub fn camel_mapper(&self, config: &Config) -> crate::camel::Mapper {
        crate::camel::Mapper {
            assistant: self.routing.assistant().clone(),
            routing: self.routing.clone(),
            routes: config.camel.clone(),
        }
    }

    /// Writes one camel batch into the runtime, then drives the assistant,
    /// so it answers at once.
    #[cfg(feature = "camel")]
    async fn camel(&mut self, apply: crate::camel::Apply) {
        let applied = apply.batch.apply(&mut self.rt);
        // The writes are durable now, so the camel task may ack them.
        let _ = apply.reply.send(applied);
        if let Err(error) = self.tick().await {
            tracing::warn!(error = %format!("{error:#}"), "driving after a camel batch failed");
            return;
        }
        match sqlite::state(&self.rt, self.routing.assistant()).await {
            Ok(Some(state)) => tracing::debug!(
                answer = state.last_answer.as_deref().unwrap_or_default(),
                "the assistant answered a camel message"
            ),
            Ok(None) => {}
            Err(error) => tracing::warn!(%error, "reading the assistant failed"),
        }
    }

    /// The committed state of the assistant, if it runs.
    ///
    /// # Errors
    ///
    /// Returns an error when the query fails.
    pub fn state(
        &self,
    ) -> impl Future<Output = Result<Option<autumn_harvest_agent::AgentState>>> + Send + 'static
    {
        // The query starts now, so the future holds no borrow of the runtime.
        // The query future is large, so it lives on the heap. Each caller
        // then awaits a small future.
        let query = Box::pin(sqlite::state(&self.rt, self.routing.assistant()));
        async move { Ok(query.await?) }
    }

    /// Handles a chat from the local socket. The pieces of its op go to the
    /// reader of its message id.
    async fn chat_request(
        &mut self,
        uid: u32,
        message_id: String,
        text: String,
    ) -> Result<Response> {
        let inbound = cli_inbound(uid, message_id, text)?;
        self.live.begin(inbound.message_id.as_str());
        let answer = self.handle(&inbound, "").await;
        self.live.end();
        answer
    }

    /// Handles one request from the local socket, sent by `uid`.
    #[expect(
        clippy::too_many_lines,
        reason = "one arm per request kind, and each arm calls one handler"
    )]
    pub async fn handle_request(&mut self, uid: u32, request: Request) -> Response {
        let result = match request {
            // The socket server streams `chat_stream`. Here it is a chat.
            Request::Chat { message_id, text } | Request::ChatStream { message_id, text } => {
                self.chat_request(uid, message_id, text).await
            }
            owner_only if uid != self.owner_uid => {
                tracing::warn!(uid, request = ?owner_only, "refused an owner request");
                Ok(Response::Error {
                    message: String::from("only the owner of the daemon may do that"),
                })
            }
            Request::Approve { id } => {
                self.decide_and_resume(id, crate::approvals::Choice::Approve)
                    .await
            }
            Request::Deny { id, reason } => {
                let reason =
                    reason.unwrap_or_else(|| String::from(crate::approvals::DEFAULT_DENY_REASON));
                self.decide_and_resume(id, crate::approvals::Choice::Deny(reason))
                    .await
            }
            Request::Reload => self.reload_file().map(|merge| reload::reloaded(&merge)),
            Request::UiLink => Ok(self.logins.as_ref().map_or_else(
                || Response::Error {
                    message: String::from("the web pages do not serve in this daemon"),
                },
                |logins| Response::ConnectLink {
                    link: logins.link(),
                },
            )),
            Request::History => self.history().await,
            // The same reads as beside the loop (ADR 0038).
            request @ (Request::Approvals
            | Request::Notes
            | Request::Plans
            | Request::Conversations
            | Request::Conversation { .. }) => {
                self.view().answer(&request).await.unwrap_or_else(|| {
                    Ok(Response::Error {
                        message: String::from("this read cannot be answered"),
                    })
                })
            }
            Request::NewConversation => self.new_conversation().await,
            Request::Workspace => self.workspace_files().await,
            Request::WorkspaceFile { path } => self.workspace_file(path).await,
            request @ (Request::Skills
            | Request::AcceptSkill { .. }
            | Request::DeclineSkill { .. }
            | Request::RetireSkill { .. }) => Box::pin(self.skills_request(request)).await,
            request @ (Request::Patches
            | Request::OpenPatch { .. }
            | Request::DeclinePatch { .. }) => self.patches_request(request).await,
            #[cfg(feature = "plans")]
            Request::Decide { decision, id } => self.decide_plan(decision, id),
            #[cfg(not(feature = "plans"))]
            Request::Decide { .. } => Ok(Response::Error {
                message: crate::features::missing("goals and ideas", "plans"),
            }),
            Request::Pair { code } => self.pair(&code),
            Request::Pairs => Ok(self.pairs()),
            Request::Unpair { sender } => self.unpair(&sender),
            #[cfg(feature = "oauth")]
            request @ (Request::Connect { .. }
            | Request::Connections
            | Request::Disconnect { .. }) => {
                oauth_request(self.connections.as_ref(), request).await
            }
            #[cfg(not(feature = "oauth"))]
            Request::Connections => Ok(Response::Off {
                message: crate::features::missing("an OAuth connection", "oauth"),
            }),
            #[cfg(not(feature = "oauth"))]
            Request::Connect { .. } | Request::Disconnect { .. } => Ok(Response::Error {
                message: crate::features::missing("an OAuth connection", "oauth"),
            }),
            #[cfg(feature = "refine")]
            request @ (Request::RefinePropose
            | Request::Overlays
            | Request::OverlayShow { .. }
            | Request::OverlayPut { .. }
            | Request::OverlayActivate { .. }
            | Request::OverlayRollback) => Box::pin(self.refine_request(request)).await,
            #[cfg(not(feature = "refine"))]
            Request::Overlays => Ok(Response::Off {
                message: crate::features::missing("lg refine", "refine"),
            }),
            #[cfg(not(feature = "refine"))]
            Request::RefinePropose
            | Request::OverlayShow { .. }
            | Request::OverlayPut { .. }
            | Request::OverlayActivate { .. }
            | Request::OverlayRollback => Ok(Response::Error {
                message: crate::features::missing("lg refine", "refine"),
            }),
        };
        result.unwrap_or_else(|error| {
            tracing::error!(error = %format!("{error:#}"), "a request failed");
            Response::Error {
                message: format!("{error:#}"),
            }
        })
    }

    /// Routes one message, runs it, and returns the answer. `reply_to` is
    /// where a pairing prompt would go.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot record, run or read the op.
    pub async fn handle(&mut self, inbound: &Inbound, reply_to: &str) -> Result<Response> {
        let id = match route(&self.routing, inbound) {
            Route::Agent(id) => id,
            Route::Pairing => {
                let pending = Pending {
                    channel: inbound.channel.clone(),
                    sender: inbound.sender.clone(),
                    reply_to: reply_to.to_owned(),
                    message_id: inbound.message_id.clone(),
                    issued_at: now_secs(),
                };
                let code = self.pending.issue(pending, fresh_code);
                tracing::info!(sender = %inbound.sender, issued = code.is_some(), "a stranger asked to pair");
                return Ok(Response::Pairing {
                    code: code.map(|code| code.to_string()),
                });
            }
            Route::Drop => {
                tracing::info!(channel = %inbound.channel, sender = %inbound.sender, "dropped a message");
                return Ok(Response::Dropped);
            }
        };
        if let Some(approval) = self.waiting {
            return Ok(Response::Busy { approval });
        }
        // A person wrote, so a new chain of wakes may start.
        #[cfg(feature = "followups")]
        self.kit.wake_links.last_person_ms.store(
            (self.clock)().as_millisecond(),
            std::sync::atomic::Ordering::SeqCst,
        );
        #[cfg(feature = "council")]
        let started = (self.clock)();
        // A new profile reaches the run before this message.
        self.roll().await?;
        // A tick that is due already runs first, so the op of this message
        // is the next segment, and its run id is known before it starts.
        sqlite::drive(&mut self.rt, &id).await?;
        let before = sqlite::state(&self.rt, &id).await?;
        let failures_before = before.as_ref().map_or(0, |state| state.failures);
        // The transcript says at the start of each op whether the
        // conversation holds untrusted input.
        self.kit
            .untrusted
            .reset(before.as_ref().map_or(&[], |state| &state.transcript));
        // With no run yet, nothing can run before this op: the first run
        // that writes is its run.
        match &before {
            Some(state) => self.live.expect(&run_id(state, state.segments)),
            None => self.live.expect_first(),
        }
        let key = inbound.message_id.as_str();
        let mark = usage::mark(&self.rt, &id)?;
        let op = AgentOp::message_with_id(inbound.text.as_str(), key);
        let outcome = sqlite::send(&mut self.rt, &id, &op, Some(key))?;
        self.op_words = Some(inbound.text.clone());
        if !outcome.signal_delivered {
            return Ok(Response::Duplicate {
                last_answer: before.and_then(|state| state.last_answer),
            });
        }
        let run = sqlite::drive(&mut self.rt, &id).await?;
        let requester = Service::from_name(inbound.channel.as_str())
            .map_or(Requester::Cli, |service| {
                Requester::Chat(service, reply_to.to_owned())
            });
        if let Some(approval) = self.settle(run.as_ref(), Some(&requester)).await? {
            return Ok(Response::Waiting { approval });
        }
        let state = sqlite::state(&self.rt, &id).await?;
        // The proposals of this op get their marks before any answer.
        #[cfg(feature = "council")]
        self.mark_proposals(started, state.as_ref())?;
        let Some(state) = state else {
            return Ok(Response::NoAnswer {
                reason: String::from("the assistant has no running run"),
            });
        };
        // The state names the last op of the drive. When a tick ran after
        // this message, its answer, stop and failures are the tick's. Then
        // the answer of this message is in the transcript.
        let alone = before.as_ref().map_or(state.segments == 1, |before| {
            before.incarnation == state.incarnation && state.segments == before.segments + 1
        });
        // The answer stands even when the ledger cannot be read. A tick
        // that ran after the op in the same drive counts in it too.
        let usage = usage::since(&self.rt, mark)
            .inspect_err(|error| tracing::warn!(%error, "the usage of an op is not known"))
            .ok();
        if !alone {
            return Ok(with_usage(
                answer_in(&state.transcript, &inbound.text),
                usage,
            ));
        }
        if state.failures > failures_before {
            return Ok(Response::NoAnswer {
                reason: state
                    .last_error
                    .unwrap_or_else(|| String::from("the op failed")),
            });
        }
        Ok(with_usage(
            answer_of(
                state.last_stop,
                state.last_answer.as_deref().unwrap_or_default(),
            ),
            usage,
        ))
    }

    /// Handles one Discord direct message, as [`Self::chat`] does.
    ///
    /// # Errors
    ///
    /// As [`Self::chat`].
    #[cfg(test)]
    pub async fn discord(&mut self, message: &DmMessage) -> Result<Option<Outgoing>> {
        self.chat(Service::Discord, message).await
    }

    /// Handles one direct message on `service`. Returns the reply to send,
    /// if any. A message whose reply went out already gets none.
    ///
    /// # Errors
    ///
    /// Returns an error when the author id is not a user id of the service,
    /// or the runtime fails.
    pub async fn chat(
        &mut self,
        service: Service,
        message: &DmMessage,
    ) -> Result<Option<Outgoing>> {
        let channel = ChannelId::new(service.name())?;
        let sender = chat_sender(service, &message.author_id)?;
        if self.store.is_delivered(&channel, &sender, &message.id) {
            return Ok(None);
        }
        // A request waits, so the message runs after the decision.
        if self.waiting.is_some() {
            let known = self.held.iter().any(|(held, m)| {
                *held == service && m.id == message.id && m.channel_id == message.channel_id
            });
            if !known {
                self.held.push_back((service, message.clone()));
            }
            return Ok(None);
        }
        let inbound = Inbound {
            channel,
            sender: sender.clone(),
            message_id: chat_message_id(service, message)?,
            text: message.content.clone(),
        };
        let response = self.handle(&inbound, &message.channel_id).await?;
        let (text, track) = match response {
            Response::Reply { text, .. } => (text, true),
            // The op ran before, but its reply did not go out.
            Response::Duplicate { last_answer } => (
                last_answer.unwrap_or_else(|| String::from("(no answer yet)")),
                true,
            ),
            Response::NoAnswer { reason } => (format!("Sorry, I could not answer: {reason}"), true),
            Response::Pairing { code } => (pairing_text(code.as_deref()), false),
            Response::Waiting { approval } => (crate::approvals::waiting_text(approval), true),
            _ => return Ok(None),
        };
        Ok(Some(Outgoing {
            channel_id: message.channel_id.clone(),
            text,
            track: track.then_some(sender),
            message_id: message.id.clone(),
        }))
    }

    /// Records that the reply to `message_id` went out. The service is the
    /// one that `sender` names.
    ///
    /// # Errors
    ///
    /// Returns an error when `sender` names no chat service, or the pairing
    /// store does not save.
    pub fn delivered(&mut self, sender: &SenderId, reply_to: &str, message_id: &str) -> Result<()> {
        let (service, _) = Service::split_sender(sender.as_str())
            .with_context(|| format!("`{sender}` is not a chat user"))?;
        let channel = ChannelId::new(service.name())?;
        self.store.delivered(&channel, sender, reply_to, message_id)
    }

    /// The cursor of each allowed sender of `service` with a known DM
    /// channel.
    #[must_use]
    pub fn cursors(&self, service: Service) -> Vec<Cursor> {
        let Ok(channel) = ChannelId::new(service.name()) else {
            return Vec::new();
        };
        let allowed: BTreeSet<&SenderId> = self.routing.allowed(&channel).collect();
        self.store
            .entries()
            .filter(|e| e.channel == channel && allowed.contains(&e.sender))
            .filter(|e| !e.reply_to.is_empty() && !e.delivered.is_empty())
            .map(|e| Cursor {
                channel_id: e.reply_to.clone(),
                after: e.delivered.clone(),
            })
            .collect()
    }

    /// Redeems a pairing code.
    fn pair(&mut self, code: &str) -> Result<Response> {
        let Some(code) = PairingCode::parse(code) else {
            return Ok(Response::Error {
                message: String::from("that is not a pairing code"),
            });
        };
        let Some(pending) = self.pending.redeem(&code, now_secs()) else {
            return Ok(Response::Error {
                message: String::from("no live pairing has that code. Codes last 10 minutes"),
            });
        };
        if !self.routing.allow(&pending.channel, pending.sender.clone()) {
            bail!("the channel {} takes no senders", pending.channel);
        }
        let delivered = cursor_of(&pending.message_id);
        self.store.put(Entry {
            channel: pending.channel,
            sender: pending.sender.clone(),
            reply_to: pending.reply_to,
            delivered,
            paired: true,
        })?;
        tracing::info!(sender = %pending.sender, "paired a sender");
        Ok(Response::Paired {
            sender: pending.sender.to_string(),
        })
    }

    /// The text turns of the assistant conversation, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot read the state.
    fn history(&self) -> impl Future<Output = Result<Response>> + Send + 'static {
        // The query starts now, so the future holds no borrow of the daemon.
        let query = self.state();
        async move {
            let state = query.await?;
            let turns = state.map_or_else(Vec::new, |state| turns(&state.transcript));
            Ok(Response::History { turns })
        }
    }

    /// Starts a new conversation: the assistant forgets its transcript. The
    /// engine clears the flow labels of the transcript too. Memory, notes
    /// and the workspace stay.
    ///
    /// A call that waits for a decision holds the op of the run, so the
    /// owner decides it first. The key names the segment that the clear
    /// follows, so a retry of one clear clears once.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot read the state, send the op
    /// or drive the run.
    async fn new_conversation(&mut self) -> Result<Response> {
        if let Some(approval) = self.waiting {
            return Ok(Response::Busy { approval });
        }
        let id = self.assistant().clone();
        // With no run, there is no transcript to clear.
        if let Some(state) = sqlite::state(&self.rt, &id).await? {
            let key = format!("new-conversation:{}", state.segments);
            let op = AgentOp::Control(Control::ClearTranscript);
            sqlite::send(&mut self.rt, &id, &op, Some(&key))?;
            sqlite::drive(&mut self.rt, &id).await?;
        }
        self.kit.untrusted.reset(&[]);
        tracing::info!("the owner started a new conversation");
        Ok(Response::Cleared)
    }

    /// The workspace of the assistant at its head: the snapshot that the
    /// last finished op recorded. A tool call in flight is not in it.
    fn workspace(
        &self,
    ) -> impl Future<Output = Result<autumn_harvest_agent::Workspace>> + Send + 'static {
        // The query starts now, so the future holds no borrow of the daemon.
        let query = self.state();
        let store = crate::tools::workspace_store(crate::tools::workspace_for(&self.kit.db));
        let scope = autumn_harvest_agent::WorkspaceMount::agent_scope(self.assistant().as_str());
        async move {
            let head = query.await?.and_then(|state| state.workspace);
            let mount = autumn_harvest_agent::WorkspaceMount {
                scope,
                head,
                generation: 0,
            };
            Ok(store.load(&mount).await?)
        }
    }

    /// The files of the assistant workspace, by path.
    fn workspace_files(&self) -> impl Future<Output = Result<Response>> + Send + 'static {
        let workspace = self.workspace();
        async move {
            let files = workspace
                .await?
                .files()
                .iter()
                .map(|(path, file)| WorkspaceEntry {
                    path: path.clone(),
                    bytes: u64::try_from(file.data.len()).unwrap_or(u64::MAX),
                })
                .collect();
            Ok(Response::Workspace { files })
        }
    }

    /// One file of the assistant workspace.
    fn workspace_file(
        &self,
        path: String,
    ) -> impl Future<Output = Result<Response>> + Send + 'static {
        let workspace = self.workspace();
        async move {
            let workspace = workspace.await?;
            let Some(file) = workspace.get(&path) else {
                return Ok(Response::Error {
                    message: format!("the workspace has no file {path}"),
                });
            };
            Ok(Response::WorkspaceFile {
                bytes: u64::try_from(file.data.len()).unwrap_or(u64::MAX),
                text: shown_text(&file.data),
                path,
            })
        }
    }

    fn pairs(&self) -> Response {
        let mut entries = Vec::new();
        let channels = std::iter::once(CLI_CHANNEL).chain(Service::ALL.map(Service::name));
        for channel in channels {
            let Ok(id) = ChannelId::new(channel) else {
                continue;
            };
            for sender in self.routing.allowed(&id) {
                let source = if channel == CLI_CHANNEL {
                    "owner"
                } else if self.config_allowed.contains(sender) {
                    "config"
                } else {
                    "paired"
                };
                entries.push(PairEntry {
                    channel: channel.to_owned(),
                    sender: sender.to_string(),
                    source: source.to_owned(),
                });
            }
        }
        Response::Pairs { entries }
    }

    /// Takes a paired chat sender off its channel. A sender that the config
    /// file allows stays allowed.
    fn unpair(&mut self, sender: &str) -> Result<Response> {
        let Ok(sender) = SenderId::new(sender) else {
            return Ok(Response::Unpaired { removed: false });
        };
        let Some((service, _)) = Service::split_sender(sender.as_str()) else {
            return Ok(Response::Unpaired { removed: false });
        };
        let channel = ChannelId::new(service.name())?;
        let removed = self.store.unpair(&channel, &sender)?;
        if removed && !self.config_allowed.contains(&sender) {
            self.routing.revoke(&channel, &sender);
        }
        Ok(Response::Unpaired { removed })
    }

    /// Aims the engine timer at the next wake of the brief or the
    /// heartbeat, when it needs an arm. It returns the op key of the arm
    /// that it sent. The key names the wake, so a wake is armed once.
    ///
    /// With no plan, it checks once whether an earlier plan left the timer
    /// booked, and turns it off.
    ///
    /// # Errors
    ///
    /// Returns an error when the state does not read or the runtime does
    /// not take the op.
    pub async fn arm_wake(&mut self) -> Result<Option<String>> {
        let now = (self.clock)();
        let id = self.routing.assistant().clone();
        #[cfg(feature = "followups")]
        self.read_wakes(now)?;
        let Some(wake) = self.wake.as_mut() else {
            if !std::mem::take(&mut self.disarm) {
                return Ok(None);
            }
            // The state query replays the run, so it runs once per plan.
            let booked = sqlite::state(&self.rt, &id)
                .await?
                .and_then(|state| state.next_heartbeat_ms);
            if booked.is_none() {
                return Ok(None);
            }
            let arm = crate::wake::off(now);
            tracing::info!("no brief and no heartbeat: the engine timer stops");
            sqlite::send(&mut self.rt, &id, &arm.op, Some(&arm.key))?;
            return Ok(Some(arm.key));
        };
        // The state query replays the run, so it runs only after a wake.
        let state = if wake.waits_for_tick(now) {
            sqlite::state(&self.rt, &id).await?
        } else {
            None
        };
        let ticks = state.as_ref().map_or(0, |state| state.ticks);
        // A tick ran: the booked wakes up to its time are done, and the
        // tick log gets a row.
        #[cfg(any(feature = "followups", feature = "self-debug"))]
        if let Some((due, kind)) = wake.ran(now, ticks)
            && let Some(stores) = self.kit.stores.as_ref()
        {
            #[cfg(feature = "followups")]
            let finished = {
                let finished = stores.finish_wakes(due.as_millisecond())?;
                wake.set_followup(next_wake(stores)?, now);
                finished
            };
            // With no booked wakes, a tick ends none.
            #[cfg(not(feature = "followups"))]
            let finished = 0;
            #[cfg(feature = "self-debug")]
            {
                #[cfg(feature = "heartbeat")]
                let skipped = (kind == crate::wake::Kind::Heartbeat)
                    .then(|| self.kit.verdict.take_skipped())
                    .flatten();
                // With no heartbeat there is no precheck to skip a tick.
                #[cfg(not(feature = "heartbeat"))]
                let skipped = None;
                // What the tick did: the newest run that no person started.
                // When it is the last segment, the state holds its answer,
                // also from a model that does not stream.
                let trace = self.live.traces().take_unprompted().map(|mut trace| {
                    let last = state.as_ref().filter(|state| {
                        state.segments > 0 && run_id(state, state.segments - 1) == trace.run_id
                    });
                    if let Some(answer) = last.and_then(|state| state.last_answer.as_deref()) {
                        trace.answer = crate::trace::one_line(answer);
                    }
                    trace
                });
                stores.record_tick(&littleguy_stores::tick_log::Tick {
                    kind: kind.label().to_owned(),
                    due_ms: due.as_millisecond(),
                    seen_ms: now.as_millisecond(),
                    outcome: tick_outcome(skipped, finished, trace.as_ref()),
                })?;
            }
            // Only the tick log reads the kind and the count.
            #[cfg(not(feature = "self-debug"))]
            let _ = (kind, finished);
        }
        let Some(arm) = wake.arm(now, ticks) else {
            return Ok(None);
        };
        tracing::info!(wake = %arm.key, "the next wake");
        sqlite::send(&mut self.rt, &id, &arm.op, Some(&arm.key))?;
        Ok(Some(arm.key))
    }

    /// Reads the next booked wake into the plan, when a wake tool changed
    /// the wakes, a reload made a new plan, or the daemon just started.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read.
    #[cfg(feature = "followups")]
    fn read_wakes(&mut self, now: Timestamp) -> Result<()> {
        let links = &self.kit.wake_links;
        if !links
            .changed
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(());
        }
        let Some(stores) = self
            .kit
            .stores
            .as_ref()
            .filter(|_| self.kit.config.tools.plans)
        else {
            return Ok(());
        };
        let next = next_wake(stores)?;
        if next.is_none() && self.wake.is_none() {
            return Ok(());
        }
        let brief_armed = Arc::clone(&self.kit.brief_armed);
        self.wake
            .get_or_insert_with(|| Wake::idle(brief_armed))
            .set_followup(next, now);
        Ok(())
    }

    /// A build without skills: the listing is off, and an action fails.
    #[cfg(not(feature = "skills"))]
    #[expect(
        clippy::unused_self,
        clippy::needless_pass_by_value,
        reason = "the skills build reads the daemon and takes the request, and the router calls both the same way"
    )]
    fn skills_request(&self, request: Request) -> std::future::Ready<Result<Response>> {
        let message = crate::features::missing("skills", "skills");
        std::future::ready(Ok(if matches!(request, Request::Skills) {
            Response::Off { message }
        } else {
            Response::Error { message }
        }))
    }

    /// Plans one reflection when a consolidation pass completed that no
    /// reflection saw yet (ADR 0022). It looks at most once a minute. Each
    /// pass counts once, also across a restart: `memory.db` keeps its key.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime or the store cannot read.
    #[cfg(feature = "reflection")]
    fn notice_passes(&mut self) -> Result<()> {
        let config = &self.kit.config;
        if !(config.memory.enabled && config.memory.reflect && config.tools.plans) {
            return Ok(());
        }
        let now = (self.clock)();
        if self
            .passes_seen
            .is_some_and(|seen| now - seen < PASS_LOOK_EVERY)
        {
            return Ok(());
        }
        self.passes_seen = Some(now);
        let Some(stores) = self.kit.stores.clone() else {
            return Ok(());
        };
        let filter = autumn_harvest_sqlite::ExecutionFilter {
            workflow_name: Some(String::from(
                autumn_harvest_agent::consolidate::CONSOLIDATE_WORKFLOW_NAME,
            )),
            states: vec![String::from("COMPLETED")],
            started_after: Some(now - PASS_LOOK_BACK),
            limit: Some(10),
            ..autumn_harvest_sqlite::ExecutionFilter::default()
        };
        let mut newest = None;
        let mut passes = Vec::new();
        for pass in self.rt.list_executions(&filter)?.executions {
            let autumn_harvest_sqlite::Stored::Value(id) = pass.exec_id else {
                continue;
            };
            let key = format!("{}{id}", crate::wake::REFLECT_KEY);
            if stores.was_sent(&key)? {
                continue;
            }
            stores.mark_sent(&key)?;
            // A pass that cannot be read counts as one that wrote nothing.
            let done = crate::reflection::read_pass(&self.rt, &id).unwrap_or_default();
            if !done.marked && done.episodes > 0 {
                tracing::warn!(pass = %id, stop = %done.stop, episodes = done.episodes,
                    "a consolidation pass ended without a mark, so its episodes wait for the next pass");
            } else if done.facts.is_empty() {
                tracing::warn!(pass = %id, stop = %done.stop, episodes = done.episodes,
                    "a consolidation pass sent no facts, so no reflection follows");
            }
            passes.push(done);
            // The listing is newest first. One reflection covers them all.
            newest.get_or_insert(id);
        }
        let Some(pass) = newest else {
            return Ok(());
        };
        let Some(prompt) = crate::reflection::prompt(&passes) else {
            // Nothing was learned. The tick log says so, with no model call.
            #[cfg(feature = "self-debug")]
            stores.record_tick(&littleguy_stores::tick_log::Tick {
                kind: String::from("reflection"),
                due_ms: now.as_millisecond(),
                seen_ms: now.as_millisecond(),
                outcome: crate::reflection::skipped(&passes),
            })?;
            return Ok(());
        };
        tracing::info!(%pass, "a consolidation pass ended: the assistant reflects");
        let brief_armed = Arc::clone(&self.kit.brief_armed);
        self.wake
            .get_or_insert_with(|| Wake::idle(brief_armed))
            .reflect(&pass, &prompt);
        Ok(())
    }

    /// Drives the assistant until it blocks, so a due timer fires. Then it
    /// arms the next brief slot.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot drive the run.
    pub async fn tick(&mut self) -> Result<()> {
        #[cfg(feature = "council")]
        let started = (self.clock)();
        // A new profile reaches the run before a tick.
        self.roll().await?;
        // A poll fires each due schedule, such as the nightly consolidation,
        // and drives every run once.
        self.rt.poll_once().await?;
        let first = sqlite::drive(&mut self.rt, self.routing.assistant()).await?;
        // A consolidation pass that just ended plans a reflection.
        #[cfg(feature = "reflection")]
        self.notice_passes()?;
        // A change to the skills folder reaches the overlay.
        #[cfg(feature = "skills")]
        self.sync_skills().await;
        // The arm comes after the drive, so a due brief tick runs first. A
        // second drive books the armed tick at once.
        let run = if self.arm_wake().await?.is_some() {
            sqlite::drive(&mut self.rt, self.routing.assistant()).await?
        } else {
            first
        };
        // A wait can start or end without a message: a brief tick asks, or
        // a deadline passes.
        self.settle(run.as_ref(), None).await?;
        self.release_held().await;
        // The proposals of the ops of this tick get their marks, and then
        // the council decides one that is clean.
        #[cfg(feature = "council")]
        {
            let state = sqlite::state(&self.rt, self.routing.assistant()).await?;
            self.mark_proposals(started, state.as_ref())?;
            self.council_turn().await?;
        }
        // A failed purge leaves the runs for the next tick. The tick goes on.
        if let Err(error) = self.purge((self.clock)()) {
            tracing::warn!(error = %format!("{error:#}"), "the purge of the ended runs failed");
        }
        Ok(())
    }
}

/// Handles one OAuth request of the owner.
#[cfg(feature = "oauth")]
async fn oauth_request(connections: Option<&Connections>, request: Request) -> Result<Response> {
    let Some(connections) = connections else {
        let message = String::from(crate::oauth::NO_CONNECTIONS);
        // A listing is off. An action on a connection fails.
        return Ok(if matches!(request, Request::Connections) {
            Response::Off { message }
        } else {
            Response::Error { message }
        });
    };
    Ok(match request {
        Request::Connect { integration } => Response::ConnectLink {
            link: connections.connect(&integration).await?,
        },
        Request::Disconnect {
            integration,
            connection,
        } => Response::Disconnected {
            removed: connections.disconnect(&integration, &connection).await?,
        },
        _ => Response::Connections {
            entries: connections.list().await?,
        },
    })
}

/// Builds the inbound message of one socket chat request.
fn cli_inbound(uid: u32, message_id: String, text: String) -> Result<Inbound> {
    Ok(Inbound {
        channel: ChannelId::new(CLI_CHANNEL)?,
        sender: cli_sender(uid)?,
        message_id: MessageId::new(message_id).context("bad message id")?,
        text,
    })
}

/// Binds the private socket at `path`.
///
/// A socket file that no daemon answers on is stale, and is removed. A live
/// one is an error. The socket gets mode 0600, so only its owner connects.
///
/// # Errors
///
/// Returns an error when another daemon answers on `path`, or the bind fails.
pub async fn bind(path: &Path) -> Result<UnixListener> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if path.exists() {
        if UnixStream::connect(path).await.is_ok() {
            bail!("a daemon already answers on {}", path.display());
        }
        std::fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    set_private(path)?;
    Ok(listener)
}

fn set_private(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting the mode of {}", path.display()))
}

/// One piece of work for the runtime task.
#[cfg_attr(
    not(any(feature = "discord", feature = "slack")),
    expect(
        dead_code,
        reason = "a build without a chat service has no chat to feed it"
    )
)]
enum Job {
    /// A request from the local socket.
    Socket {
        uid: u32,
        request: Request,
        reply: oneshot::Sender<Response>,
    },
    /// A direct message on a chat service.
    Chat {
        service: Service,
        message: DmMessage,
        reply: oneshot::Sender<Option<Outgoing>>,
    },
    /// A chat reply went out.
    Delivered {
        sender: SenderId,
        reply_to: String,
        message_id: String,
    },
    /// The cursors of `service` to read the missed history from.
    Cursors {
        service: Service,
        reply: oneshot::Sender<Vec<Cursor>>,
    },
    /// A batch of the camel routes.
    #[cfg(feature = "camel")]
    Camel(crate::camel::Apply),
    /// A press of an approval button.
    Press {
        press: Press,
        reply: oneshot::Sender<gate::PressAnswer>,
    },
    /// A tick from outside, such as a hook that wakes a sleeping host. The
    /// answer comes once the runs block, so the caller holds the host
    /// awake while they work.
    Tick { reply: oneshot::Sender<bool> },
}

/// One button press, as each chat service gives it.
#[derive(Debug, Clone)]
struct Press {
    service: Service,
    user_id: String,
    custom_id: String,
    channel_id: String,
}

/// The chat channels that [`run`] starts.
#[derive(Debug, Clone, Default)]
pub struct Links {
    /// Discord, when it is on.
    #[cfg(feature = "discord")]
    pub discord: Option<DiscordLink>,
    /// Slack, when it is on.
    #[cfg(feature = "slack")]
    pub slack: Option<SlackLink>,
}

/// How Slack reaches the daemon.
#[cfg(feature = "slack")]
#[derive(Debug, Clone)]
pub enum SlackLink {
    /// Socket Mode: the daemon holds a socket open. The client holds both
    /// tokens.
    Socket(littleguy_channels::SlackApi),
    /// The Events API: Slack calls the hooks, so a sleeping host wakes. The
    /// client holds the bot token, and the hooks check each request with
    /// the signing secret.
    Events {
        /// The Web API client, for the replies.
        api: littleguy_channels::SlackApi,
        /// The secret that signs each request from Slack.
        secret: littleguy_channels::SigningSecret,
    },
}

#[cfg(test)]
impl Links {
    /// Discord alone, on `link`.
    #[cfg(feature = "discord")]
    fn with_discord(link: DiscordLink) -> Self {
        Self {
            discord: Some(link),
            #[cfg(feature = "slack")]
            slack: None,
        }
    }

    /// Slack alone, on `link`.
    #[cfg(feature = "slack")]
    fn with_slack(link: SlackLink) -> Self {
        Self {
            #[cfg(feature = "discord")]
            discord: None,
            slack: Some(link),
        }
    }
}

/// The queue of the job loop, before [`run`] takes it.
#[derive(Debug)]
pub struct Jobs {
    tx: mpsc::Sender<Job>,
    rx: mpsc::Receiver<Job>,
}

/// A client of the job loop inside this process, such as the web pages.
///
/// Each request goes through [`Daemon::handle_request`] as the owner, so the
/// pages can do what `lg` can do, and no more.
#[derive(Debug, Clone)]
pub struct Handle {
    tx: mpsc::Sender<Job>,
    uid: u32,
    /// The readers of live answers, when the daemon streams them.
    live: Option<Arc<live::Live>>,
    /// The reads that answer beside the loop, when the daemon has them.
    reads: Option<Arc<Reads>>,
}

impl Handle {
    /// Sends `request` as the owner and waits for the answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon stops before it answers.
    pub async fn ask(&self, request: Request) -> Result<Response> {
        // A read answers at once, also while a run holds the loop.
        if let Some(reads) = &self.reads
            && let Some(answer) = reads.answer(self.uid, &request).await
        {
            return Ok(answer);
        }
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Job::Socket {
                uid: self.uid,
                request,
                reply,
            })
            .await
            .context("the daemon is stopping")?;
        answer.await.context("the daemon dropped the request")
    }

    /// Answers each read from `reads`, beside the loop.
    #[must_use]
    pub fn with_reads(mut self, reads: Arc<Reads>) -> Self {
        self.reads = Some(reads);
        self
    }

    /// Streams chats with the readers of `live`.
    #[must_use]
    pub fn with_live(mut self, live: Arc<live::Live>) -> Self {
        self.live = Some(live);
        self
    }

    /// Sends `text` to the assistant as the owner, and returns its pieces:
    /// the text while the model writes it, then the answer. Without live
    /// readers, the answer is the one piece. The chat runs to its end even
    /// when the receiver goes.
    #[must_use]
    pub fn chat_live(&self, text: String) -> mpsc::Receiver<live::Piece> {
        let message_id = crate::client::fresh_message_id();
        let (pieces, receiver) = mpsc::channel(live::LIVE_PIECES);
        if let Some(live) = &self.live {
            live.watch(&message_id, pieces.clone());
        }
        let handle = self.clone();
        tokio::spawn(async move {
            let request = Request::Chat {
                message_id: message_id.clone(),
                text,
            };
            let answer = handle
                .ask(request)
                .await
                .unwrap_or_else(|error| Response::Error {
                    message: format!("{error:#}"),
                });
            if let Some(live) = &handle.live {
                live.forget(&message_id);
            }
            let _ = pieces.send(live::Piece::Answer(answer)).await;
        });
        receiver
    }

    /// Ticks the daemon now, as its own timer does, and waits until the
    /// runs block. Returns whether the tick worked.
    ///
    /// # Errors
    ///
    /// Returns an error when the daemon stops before it answers.
    pub async fn tick(&self) -> Result<bool> {
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Job::Tick { reply })
            .await
            .context("the daemon is stopping")?;
        answer.await.context("the daemon dropped the tick")
    }
}

/// A job queue, and a handle on it that speaks as `owner_uid`.
#[must_use]
pub fn jobs(owner_uid: u32) -> (Handle, Jobs) {
    let (tx, rx) = mpsc::channel::<Job>(64);
    (
        Handle {
            tx: tx.clone(),
            uid: owner_uid,
            live: None,
            reads: None,
        },
        Jobs { tx, rx },
    )
}

/// Runs one job on the runtime.
async fn run_job(daemon: &mut Daemon, job: Job) {
    match job {
        Job::Socket {
            uid,
            request,
            reply,
        } => {
            let response = daemon.handle_request(uid, request).await;
            // A client that hung up gets no answer. The op still ran.
            let _ = reply.send(response);
            daemon.release_held().await;
        }
        Job::Chat {
            service,
            message,
            reply,
        } => {
            let outgoing = daemon.chat(service, &message).await.unwrap_or_else(|error| {
                tracing::error!(error = %format!("{error:#}"), %service, "a chat message failed");
                None
            });
            let _ = reply.send(outgoing);
        }
        Job::Delivered {
            sender,
            reply_to,
            message_id,
        } => {
            if let Err(error) = daemon.delivered(&sender, &reply_to, &message_id) {
                tracing::error!(error = %format!("{error:#}"), "the delivery cursor did not save");
            }
        }
        #[cfg(feature = "camel")]
        Job::Camel(apply) => daemon.camel(apply).await,
        Job::Cursors { service, reply } => {
            let _ = reply.send(daemon.cursors(service));
        }
        Job::Press { press, reply } => {
            // The answer goes first: the chat waits three seconds for it.
            let (answer, decided) = match chat_sender(press.service, &press.user_id) {
                Ok(user) => daemon.press(&user, &press.custom_id),
                Err(_) => (
                    gate::PressAnswer::Private(String::from(
                        "Only the approver can decide this request.",
                    )),
                    None,
                ),
            };
            let _ = reply.send(answer);
            if let Some(record) = decided {
                daemon
                    .resume_after_press(&record, press.service, &press.channel_id)
                    .await;
            }
            daemon.release_held().await;
        }
        Job::Tick { reply } => {
            let worked = match daemon.tick().await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), "a hook tick failed");
                    false
                }
            };
            let _ = reply.send(worked);
        }
    }
}

/// Runs the daemon until `stop` completes or the accept loop ends.
///
/// The loop holds the daemon and the camel context, so its future lives on
/// the heap.
///
/// # Errors
///
/// Returns an error only when the loop cannot start.
pub fn run(
    daemon: Daemon,
    listener: UnixListener,
    links: Links,
    camel: Option<Camel>,
    jobs: Jobs,
    stop: impl Future<Output = ()>,
) -> Pin<Box<impl Future<Output = Result<()>>>> {
    Box::pin(serve(daemon, listener, links, camel, jobs, stop))
}

/// The loop of [`run`].
async fn serve(
    mut daemon: Daemon,
    listener: UnixListener,
    links: Links,
    mut camel: Option<Camel>,
    jobs: Jobs,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let Jobs { tx, mut rx } = jobs;
    daemon.publish();
    let mut tasks = vec![tokio::spawn(accept_loop(
        listener,
        tx.clone(),
        daemon.live(),
        daemon.reads(),
    ))];
    #[cfg(feature = "discord")]
    if let Some(link) = links.discord {
        discord::start(&daemon, link, &tx, &mut tasks);
    }
    #[cfg(feature = "slack")]
    if let Some(link) = links.slack {
        slack::start(&daemon, link, &tx, &mut tasks);
    }
    // A build with no chat service and no camel starts no task here.
    #[cfg(not(any(feature = "discord", feature = "slack", feature = "camel")))]
    let _ = (&links, &mut tasks);
    #[cfg(all(feature = "camel", not(any(feature = "discord", feature = "slack"))))]
    let _ = &links;
    #[cfg(feature = "camel")]
    if let Some(camel) = &camel {
        let pump = crate::camel::pump(camel.inbound(), tx.clone(), Job::Camel);
        tasks.push(tokio::spawn(pump));
    }
    // A reload can start camel later. A weak sender lets the loop end when
    // every other sender is gone.
    let weak = tx.downgrade();
    drop(tx);
    if let Some(file) = daemon.config_file() {
        let watch = reload::watch(
            file.to_path_buf(),
            daemon.kit.watch_every,
            weak.clone(),
            daemon.owner_uid,
        );
        tasks.push(tokio::spawn(watch));
    }
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(stop);
    tracing::info!(assistant = %daemon.assistant(), "littleguy is listening");
    loop {
        tokio::select! {
            job = rx.recv() => {
                let Some(job) = job else { break };
                match job {
                    Job::Socket { uid, request: Request::Reload, reply } => {
                        // Boxed, so the loop future stays small.
                        let answer = Box::pin(reload::reload_job(
                            &mut daemon,
                            &mut camel,
                            &mut tasks,
                            &weak,
                            uid,
                        ))
                        .await;
                        let _ = reply.send(answer);
                    }
                    job => run_job(&mut daemon, job).await,
                }
            }
            _ = ticker.tick() => {
                if let Err(error) = daemon.tick().await {
                    tracing::warn!(error = %format!("{error:#}"), "a tick failed");
                }
            }
            () = &mut stop => break,
        }
        // Each job may change what a read sees: a pair, a reload, a skill.
        daemon.publish();
    }
    for task in tasks {
        task.abort();
    }
    #[cfg(feature = "camel")]
    if let Some(camel) = camel {
        camel.stop().await;
    }
    #[cfg(not(feature = "camel"))]
    let _ = camel;
    #[cfg(feature = "oauth")]
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
    Ok(())
}

/// Asks the daemon for the cursors of `service`.
#[cfg_attr(
    not(any(feature = "discord", feature = "slack")),
    expect(
        dead_code,
        reason = "a build without a chat service has no chat to feed it"
    )
)]
async fn cursors(jobs: &mpsc::Sender<Job>, service: Service) -> Option<Vec<Cursor>> {
    let (reply, cursors) = oneshot::channel();
    jobs.send(Job::Cursors { service, reply }).await.ok()?;
    Some(cursors.await.unwrap_or_default())
}

/// Runs one message and sends its reply. A failed send moves no cursor, so
/// the history read after the next reconnect tries it again.
#[cfg_attr(
    not(any(feature = "discord", feature = "slack")),
    expect(
        dead_code,
        reason = "a build without a chat service has no chat to feed it"
    )
)]
async fn deliver(chats: &Chats, jobs: &mpsc::Sender<Job>, service: Service, message: DmMessage) {
    let (reply, answer) = oneshot::channel();
    let job = Job::Chat {
        service,
        message,
        reply,
    };
    if jobs.send(job).await.is_err() {
        return;
    }
    let Ok(Some(outgoing)) = answer.await else {
        return;
    };
    if let Err(error) = chats
        .send(service, &outgoing.channel_id, &outgoing.text)
        .await
    {
        tracing::warn!(%error, %service, "a chat reply did not go out");
        return;
    }
    if let Some(sender) = outgoing.track {
        let _ = jobs
            .send(Job::Delivered {
                sender,
                reply_to: outgoing.channel_id,
                message_id: outgoing.message_id,
            })
            .await;
    }
}

/// Hands one press to the daemon, and returns its answer.
#[cfg_attr(
    not(any(feature = "discord", feature = "slack")),
    expect(
        dead_code,
        reason = "a build without a chat service has no chat to feed it"
    )
)]
async fn decide_press(jobs: &mpsc::Sender<Job>, press: Press) -> Option<gate::PressAnswer> {
    let (reply, answer) = oneshot::channel();
    jobs.send(Job::Press { press, reply }).await.ok()?;
    answer.await.ok()
}

async fn accept_loop(
    listener: UnixListener,
    tx: mpsc::Sender<Job>,
    live: Arc<live::Live>,
    reads: Arc<Reads>,
) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tx = tx.clone();
                let live = Arc::clone(&live);
                let reads = Arc::clone(&reads);
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, tx, &live, &reads).await {
                        tracing::debug!(error = %format!("{error:#}"), "a connection failed");
                    }
                });
            }
            Err(error) => {
                tracing::warn!(%error, "accept failed");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Writes one JSON line.
async fn write_line(
    write: &mut tokio::net::unix::OwnedWriteHalf,
    line: &(impl serde::Serialize + Sync),
) -> Result<()> {
    let mut out = serde_json::to_vec(line)?;
    out.push(b'\n');
    write.write_all(&out).await.context("writing a line")
}

/// Hands a `chat_stream` to the owner as a chat, and writes each piece of
/// its answer as a line while the model writes it. Returns the answer.
///
/// The reader of the message id goes in before the op reaches the owner,
/// so no piece of the op comes too early.
async fn stream_chat(
    uid: u32,
    message_id: String,
    text: String,
    tx: &mpsc::Sender<Job>,
    taps: &live::Live,
    write: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<Response> {
    let (pieces, taken) = mpsc::channel(live::LIVE_PIECES);
    taps.watch(&message_id, pieces);
    let (reply, answer) = oneshot::channel();
    let request = Request::Chat {
        message_id: message_id.clone(),
        text,
    };
    let job = Job::Socket {
        uid,
        request,
        reply,
    };
    let result = if tx.send(job).await.is_ok() {
        relay(answer, taken, write).await
    } else {
        Err(anyhow::anyhow!("the daemon is stopping"))
    };
    taps.forget(&message_id);
    result
}

/// Writes each piece as it comes, until the answer comes. The sink sends
/// each piece during the drive, before the owner answers, so the pieces
/// that still wait then are written first. A reader that goes keeps the op
/// running to its end.
async fn relay(
    answer: oneshot::Receiver<Response>,
    mut taken: mpsc::Receiver<live::Piece>,
    write: &mut tokio::net::unix::OwnedWriteHalf,
) -> Result<Response> {
    let mut answer = std::pin::pin!(answer);
    let mut written = Ok(());
    let response = loop {
        tokio::select! {
            biased;
            Some(piece) = taken.recv() => {
                if let (Ok(()), Some(line)) = (&written, wire_piece(piece)) {
                    written = write_line(write, &line).await;
                }
            }
            response = &mut answer => break response,
        }
    };
    while let Ok(piece) = taken.try_recv() {
        if let (Ok(()), Some(line)) = (&written, wire_piece(piece)) {
            written = write_line(write, &line).await;
        }
    }
    written.and_then(|()| response.context("the daemon dropped the request"))
}

/// A live piece as a line of the wire. The answer is the response line,
/// not a piece, and the sink never sends it.
fn wire_piece(piece: live::Piece) -> Option<crate::protocol::Piece> {
    match piece {
        live::Piece::Restart => Some(crate::protocol::Piece::Restart),
        live::Piece::Text(text) => Some(crate::protocol::Piece::Delta { text }),
        live::Piece::Answer(_) => None,
    }
}

/// Why the socket refuses a caller with the uid of the daemon.
pub const SANDBOXED_PEER: &str = "this socket takes no request from code in a sandbox";

/// Whether the process `pid` is in the user namespace of this daemon.
///
/// The process sandbox runs each call in a new user namespace with the uid
/// of the daemon. So a peer with that uid in another namespace is code
/// that the assistant runs, and it must not act as the owner (ADR 0032).
/// A peer that cannot be checked counts as one in another namespace.
#[cfg(target_os = "linux")]
#[must_use]
pub fn in_our_user_namespace(pid: Option<i32>) -> bool {
    let Some(pid) = pid else {
        return false;
    };
    let ours = std::fs::read_link("/proc/self/ns/user");
    let theirs = std::fs::read_link(format!("/proc/{pid}/ns/user"));
    matches!((ours, theirs), (Ok(ours), Ok(theirs)) if ours == theirs)
}

/// Whether the process `pid` is in the user namespace of this daemon. This
/// system has no user namespaces, and no process sandbox, so it is.
#[cfg(not(target_os = "linux"))]
#[must_use]
pub const fn in_our_user_namespace(_pid: Option<i32>) -> bool {
    true
}

async fn serve_connection(
    stream: UnixStream,
    tx: mpsc::Sender<Job>,
    taps: &live::Live,
    reads: &Reads,
) -> Result<()> {
    let peer = stream.peer_cred().context("reading the peer credentials")?;
    let uid = peer.uid();
    let (read, mut write) = stream.into_split();
    // A caller with another uid is no owner, and the uid rules hold for it.
    // A caller with our uid must also be in our user namespace.
    if uid == rustix::process::getuid().as_raw() && !in_our_user_namespace(peer.pid()) {
        tracing::warn!(pid = ?peer.pid(), "refused a socket caller from a sandbox");
        let refused = Response::Error {
            message: String::from(SANDBOXED_PEER),
        };
        return write_line(&mut write, &refused).await;
    }
    let limit = u64::try_from(MAX_REQUEST_BYTES).unwrap_or(u64::MAX);
    let mut reader = BufReader::new(read.take(limit + 1));
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .await
        .context("reading the request")?;
    let response = if line.len() > MAX_REQUEST_BYTES {
        Response::Error {
            message: format!("the request is longer than {MAX_REQUEST_BYTES} bytes"),
        }
    } else {
        match serde_json::from_str::<Request>(&line) {
            Ok(Request::ChatStream { message_id, text }) => {
                stream_chat(uid, message_id, text, &tx, taps, &mut write).await?
            }
            Ok(request) => {
                // A read answers at once, also while a run holds the loop.
                if let Some(answer) = reads.answer(uid, &request).await {
                    return write_line(&mut write, &answer).await;
                }
                let (reply, answer) = oneshot::channel();
                tx.send(Job::Socket {
                    uid,
                    request,
                    reply,
                })
                .await
                .context("the daemon is stopping")?;
                answer.await.context("the daemon dropped the request")?
            }
            Err(error) => Response::Error {
                message: format!("the request is not valid: {error}"),
            },
        }
    };
    write_line(&mut write, &response).await
}

#[cfg(all(test, target_os = "linux"))]
mod namespace_tests {
    #[test]
    fn this_process_is_in_its_own_namespace_and_no_pid_is_not() {
        assert!(super::in_our_user_namespace(Some(
            std::process::id().cast_signed()
        )));
        assert!(!super::in_our_user_namespace(None));
        assert!(
            !super::in_our_user_namespace(Some(i32::MAX)),
            "a process that cannot be read counts as a stranger"
        );
    }
}

#[cfg(feature = "council")]
mod council;
#[cfg(feature = "discord")]
mod discord;
mod gate;
#[cfg(feature = "history")]
mod history;
pub mod live;
pub use reads::Reads;
mod patches;
#[cfg(feature = "plans")]
mod plans;
mod reads;
#[cfg(feature = "refine")]
mod refine;
mod reload;
#[cfg(feature = "auto-approve")]
mod review;
mod runs;
#[cfg(feature = "skills")]
mod skills;
mod usage;
#[cfg(feature = "discord")]
pub use discord::DiscordLink;
#[cfg(feature = "slack")]
mod slack;
#[cfg(feature = "slack")]
pub use slack::SlackInbox;

/// The text turns of `transcript`, oldest first. A turn joins the text
/// parts of one message. Tool calls, tool results and system text are not
/// turns, and a message with no text is left out.
fn turns(transcript: &[autumn_harvest_agent::ChatMessage]) -> Vec<Turn> {
    use autumn_harvest_agent::ChatRole;
    let mut turns = Vec::new();
    // A tick prompt is not a person. The next answer is the assistant on
    // its own.
    let mut after_tick = false;
    for message in transcript {
        let text = message_text(message);
        if text.is_empty() {
            continue;
        }
        let from = match message.role {
            ChatRole::User if crate::wake::is_tick(&text) => {
                after_tick = true;
                continue;
            }
            ChatRole::User => {
                after_tick = false;
                Speaker::Person
            }
            ChatRole::Assistant if std::mem::take(&mut after_tick) => Speaker::Unprompted,
            ChatRole::Assistant => Speaker::Assistant,
            ChatRole::System | ChatRole::Tool => continue,
        };
        turns.push(Turn { from, text });
    }
    turns
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    #[cfg(feature = "discord")]
    use littleguy_channels::DiscordRest;
    #[cfg(feature = "slack")]
    use littleguy_channels::SlackApi;
    use littleguy_core::OwnerName;
    use littleguy_models::OfflineModel;

    use super::*;

    const UID: u32 = 1000;

    fn config() -> Config {
        Config::for_owner(OwnerName::new("mark").unwrap())
    }

    fn request(id: &str, text: &str) -> Request {
        Request::Chat {
            message_id: String::from(id),
            text: String::from(text),
        }
    }

    fn open(db: &Path, model: Arc<OfflineModel>) -> Daemon {
        Daemon::open(db, &config(), model, UID, false).unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_owner_gets_an_answer() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = open(&dir.path().join("lg.db"), Arc::clone(&model));
        let response = daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(
            reply_text(&response),
            "littleguy (offline): you said 'hello'"
        );
        assert_eq!(model.calls(), 1);
        assert_eq!(daemon.assistant().as_str(), "assistant/mark");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_state_on_file_is_the_state_that_the_runtime_gives() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon = open(&db, Arc::new(OfflineModel::new()));
        let before = reads::committed_state(&db, daemon.assistant()).unwrap();
        assert!(
            matches!(before, reads::Committed::NoRun),
            "no run, no state"
        );
        for n in 0..3 {
            daemon
                .handle_request(UID, request(&format!("m-{n}"), &format!("hello {n}")))
                .await;
        }
        let engine = daemon.state().await.unwrap().unwrap();
        let reads::Committed::State(file) =
            reads::committed_state(&db, daemon.assistant()).unwrap()
        else {
            panic!("the run has a snapshot");
        };
        assert_eq!(file.transcript, engine.transcript);
        assert_eq!(file.workspace, engine.workspace);
        assert_eq!(file.segments, engine.segments);
        assert_eq!(file.last_answer, engine.last_answer);
        // The reads answer the history as the loop does.
        daemon.publish();
        let beside = daemon.reads().answer(UID, &Request::History).await;
        let looped = daemon.handle_request(UID, Request::History).await;
        assert_eq!(beside, Some(looped));
        // A stranger gets nothing beside the loop. The loop refuses it.
        assert_eq!(
            daemon.reads().answer(UID + 1, &Request::History).await,
            None
        );
        // A write is never a read.
        assert_eq!(
            daemon.reads().answer(UID, &request("m-9", "hi")).await,
            None
        );
    }

    #[cfg(feature = "history")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_conversation_leaves_the_old_one_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()));
        let empty = daemon.handle_request(UID, Request::Conversations).await;
        assert!(
            matches!(&empty, Response::Conversations { entries, keep_days: 30, partial: false } if entries.is_empty()),
            "an empty history lists no conversation"
        );
        daemon
            .handle_request(UID, request("cli-1-1", "hello"))
            .await;
        daemon.handle_request(UID, request("cli-2-1", "tea")).await;
        daemon.handle_request(UID, Request::NewConversation).await;
        daemon
            .handle_request(UID, request("cli-3-1", "again"))
            .await;

        let Response::Conversations { entries, .. } =
            daemon.handle_request(UID, Request::Conversations).await
        else {
            panic!("a listing");
        };
        let shape: Vec<(i64, &str, bool)> = entries
            .iter()
            .map(|entry| (entry.turns, entry.first_text.as_str(), entry.live))
            .collect();
        assert_eq!(shape, [(1, "again", true), (2, "hello", false)]);

        let old = entries[1].id;
        let Response::Conversation { turns, .. } = daemon
            .handle_request(UID, Request::Conversation { id: old })
            .await
        else {
            panic!("a conversation");
        };
        let said: Vec<(Speaker, &str)> = turns
            .iter()
            .map(|turn| (turn.from, turn.text.as_str()))
            .collect();
        assert_eq!(
            said,
            [
                (Speaker::Person, "hello"),
                (Speaker::Assistant, "littleguy (offline): you said 'hello'"),
                (Speaker::Person, "tea"),
                (Speaker::Assistant, "littleguy (offline): you said 'tea'"),
            ]
        );
        // The reads answer the same beside the loop.
        daemon.publish();
        let beside = daemon
            .reads()
            .answer(UID, &Request::Conversation { id: old })
            .await;
        assert_eq!(beside, Some(Response::Conversation { id: old, turns }));
        // Only the owner reads it.
        let stranger = daemon.handle_request(UID + 1, Request::Conversations).await;
        assert!(
            matches!(stranger, Response::Error { .. }),
            "a stranger cannot read the history"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_same_message_id_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = open(&dir.path().join("lg.db"), Arc::clone(&model));
        daemon.handle_request(UID, request("m-1", "hello")).await;
        let again = daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(
            again,
            Response::Duplicate {
                last_answer: Some(String::from("littleguy (offline): you said 'hello'"))
            }
        );
        assert_eq!(model.calls(), 1);
        assert_eq!(daemon.state().await.unwrap().unwrap().segments, 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn another_user_is_dropped_and_runs_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = open(&dir.path().join("lg.db"), Arc::clone(&model));
        let response = daemon.handle_request(UID + 1, request("m-1", "hi")).await;
        assert_eq!(response, Response::Dropped);
        assert_eq!(model.calls(), 0);
        assert!(daemon.state().await.unwrap().is_none());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_history_shows_the_text_turns_to_the_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()));
        assert_eq!(
            daemon.handle_request(UID, Request::History).await,
            Response::History { turns: Vec::new() }
        );
        daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(
            daemon.handle_request(UID, Request::History).await,
            Response::History {
                turns: vec![
                    Turn {
                        from: Speaker::Person,
                        text: String::from("hello"),
                    },
                    Turn {
                        from: Speaker::Assistant,
                        text: String::from("littleguy (offline): you said 'hello'"),
                    },
                ]
            }
        );
        let stranger = daemon.handle_request(UID + 1, Request::History).await;
        // The assertion prints no response, so a failure logs no turn.
        assert!(matches!(
            stranger,
            Response::Error { message } if message == "only the owner of the daemon may do that"
        ));
    }

    #[test]
    fn a_turn_keeps_text_and_leaves_out_tools() {
        use autumn_harvest_agent::{ChatMessage, ChatRole, ContentPart};
        let call = ContentPart::ToolCall {
            id: String::from("c1"),
            name: String::from("list_files"),
            arguments: serde_json::json!({}),
        };
        let transcript = vec![
            ChatMessage::text(ChatRole::User, "what is there?"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![call],
            },
            ChatMessage::text(ChatRole::Tool, "notes/a.md"),
            ChatMessage {
                role: ChatRole::Assistant,
                content: vec![
                    ContentPart::Text(String::from("One file.")),
                    ContentPart::Text(String::from(" ")),
                ],
            },
        ];
        assert_eq!(
            turns(&transcript),
            vec![
                Turn {
                    from: Speaker::Person,
                    text: String::from("what is there?"),
                },
                Turn {
                    from: Speaker::Assistant,
                    text: String::from("One file."),
                },
            ]
        );
    }

    #[test]
    fn a_tick_answer_speaks_on_its_own_and_its_prompt_is_hidden() {
        use autumn_harvest_agent::{ChatMessage, ChatRole, ContentPart};
        let tick = format!("{}\nCheck your goals.", crate::wake::HEARTBEAT_MARK);
        let call = ChatMessage {
            role: ChatRole::Assistant,
            content: vec![ContentPart::ToolCall {
                id: String::from("c1"),
                name: String::from("list_plans"),
                arguments: serde_json::json!({}),
            }],
        };
        let transcript = vec![
            ChatMessage::text(ChatRole::User, "hi"),
            ChatMessage::text(ChatRole::Assistant, "hello"),
            ChatMessage::text(ChatRole::User, tick),
            call,
            ChatMessage::text(ChatRole::Tool, "{}"),
            ChatMessage::text(ChatRole::Assistant, "Goal 1 shipped."),
            ChatMessage::text(ChatRole::User, "nice"),
            ChatMessage::text(ChatRole::Assistant, "thanks"),
        ];
        let from: Vec<Speaker> = turns(&transcript).iter().map(|turn| turn.from).collect();
        assert_eq!(
            from,
            [
                Speaker::Person,
                Speaker::Assistant,
                Speaker::Unprompted,
                Speaker::Person,
                Speaker::Assistant
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_message_id_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()));
        let response = daemon.handle_request(UID, request("", "hi")).await;
        assert!(matches!(response, Response::Error { .. }));
    }

    /// The message is acknowledged, then the process dies before it runs.
    /// A new daemon runs it once, and a finished turn is never paid again.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_runs_an_acknowledged_message_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let id = AgentId::new("assistant", "mark").unwrap();
        {
            let first = Arc::new(OfflineModel::new());
            let mut daemon = open(&db, Arc::clone(&first));
            let op = AgentOp::message_with_id("hello", "m-1");
            sqlite::send(&mut daemon.rt, &id, &op, Some("m-1")).unwrap();
            assert_eq!(first.calls(), 0);
        }
        let second = Arc::new(OfflineModel::new());
        let mut daemon = open(&db, Arc::clone(&second));
        daemon.tick().await.unwrap();
        assert_eq!(second.calls(), 1);
        let state = daemon.state().await.unwrap().unwrap();
        assert_eq!(
            state.last_answer.as_deref(),
            Some("littleguy (offline): you said 'hello'")
        );
        drop(daemon);

        let third = Arc::new(OfflineModel::new());
        let mut daemon = open(&db, Arc::clone(&third));
        daemon.tick().await.unwrap();
        assert_eq!(
            third.calls(),
            0,
            "a recorded turn is replayed, not paid again"
        );
    }

    async fn raw(socket: &Path, line: &[u8]) -> Response {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write.write_all(line).await.unwrap();
        write.shutdown().await.unwrap();
        let mut answer = String::new();
        BufReader::new(read).read_line(&mut answer).await.unwrap();
        serde_json::from_str(&answer).unwrap()
    }

    /// Sends `line`, and reads every line of the answer.
    async fn lines(socket: &Path, line: &serde_json::Value) -> Vec<serde_json::Value> {
        let stream = UnixStream::connect(socket).await.unwrap();
        let (read, mut write) = stream.into_split();
        write
            .write_all(format!("{line}\n").as_bytes())
            .await
            .unwrap();
        let mut lines = Vec::new();
        let mut reader = BufReader::new(read);
        let mut text = String::new();
        while reader.read_line(&mut text).await.unwrap() > 0 {
            lines.push(serde_json::from_str(&text).unwrap());
            text.clear();
        }
        lines
    }

    /// A `chat_stream` gets the pieces of its answer, then the answer. The
    /// pieces after the last restart make the text of the answer.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_chat_stream_writes_its_pieces_then_the_answer() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let daemon = Daemon::open(&dir.path().join("lg.db"), &config(), model, uid, false).unwrap();
        let listener = bind(&socket).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let streamed = serde_json::json!({
            "kind": "chat_stream", "message_id": "s-1", "text": "a long way down",
        });
        let client = async {
            let first = lines(&socket, &streamed).await;
            let again = lines(&socket, &streamed).await;
            let plain = lines(
                &socket,
                &serde_json::json!({"kind": "chat", "message_id": "s-2", "text": "hi"}),
            )
            .await;
            stop_tx.send(()).unwrap();
            (first, again, plain)
        };
        let (served, (first, again, plain)) = tokio::join!(
            run(daemon, listener, Links::default(), None, jobs(UID).1, stop),
            client
        );
        served.unwrap();
        let (last, pieces) = first.split_last().unwrap();
        assert_eq!(last["kind"], "reply");
        let answer = last["text"].as_str().unwrap();
        assert_eq!(answer, "littleguy (offline): you said 'a long way down'");
        let start = pieces
            .iter()
            .rposition(|piece| piece["kind"] == "restart")
            .map_or(0, |at| at + 1);
        let text: String = pieces[start..]
            .iter()
            .map(|piece| {
                assert_eq!(piece["kind"], "delta", "{piece}");
                piece["text"].as_str().unwrap()
            })
            .collect();
        assert_eq!(text, answer, "the pieces make the answer: {pieces:?}");
        assert!(pieces.iter().any(|piece| piece["kind"] == "delta"));
        // A seen id ran nothing, so it has no pieces.
        assert_eq!(again.len(), 1);
        assert_eq!(again[0]["kind"], "duplicate");
        // A plain chat still gets one line.
        assert_eq!(plain.len(), 1);
        assert_eq!(plain[0]["kind"], "reply");
    }

    /// The whole socket path: accept, route, run, answer, stop.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_loop_answers_over_the_socket_and_stops() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let daemon = Daemon::open(&dir.path().join("lg.db"), &config(), model, uid, false).unwrap();
        let listener = bind(&socket).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let client = async {
            let reply = crate::client::send(&socket, &request("m-1", "hi"))
                .await
                .unwrap();
            let bad_json = raw(&socket, b"not json\n").await;
            let too_long = raw(&socket, &vec![b'x'; MAX_REQUEST_BYTES + 2]).await;
            stop_tx.send(()).unwrap();
            (reply, bad_json, too_long)
        };
        let (served, (reply, bad_json, too_long)) = tokio::join!(
            run(daemon, listener, Links::default(), None, jobs(UID).1, stop),
            client
        );
        served.unwrap();
        assert_eq!(reply_text(&reply), "littleguy (offline): you said 'hi'");
        assert!(
            matches!(&bad_json, Response::Error { message } if message.contains("not valid")),
            "{bad_json:?}"
        );
        assert!(
            matches!(&too_long, Response::Error { message } if message.contains("longer than")),
            "{too_long:?}"
        );
    }

    #[test]
    fn a_second_daemon_on_one_database_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let _first = open(&db, Arc::new(OfflineModel::new()));
        let second = Daemon::open(&db, &config(), Arc::new(OfflineModel::new()), UID, false);
        let error = format!("{:#}", second.unwrap_err());
        assert!(error.contains("lock"), "{error}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_live_socket_is_refused_and_a_stale_one_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("run").join("lg.sock");
        let listener = bind(&path).await.unwrap();
        assert!(bind(&path).await.is_err());
        drop(listener);
        assert!(path.exists());
        let again = bind(&path).await;
        assert!(again.is_ok());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    fn discord_open(db: &Path, config: &Config, model: Arc<OfflineModel>) -> Daemon {
        Daemon::open(db, config, model, UID, true).unwrap()
    }

    fn dm(id: &str, author: &str, text: &str) -> DmMessage {
        DmMessage {
            id: id.to_owned(),
            channel_id: format!("dm{author}"),
            author_id: author.to_owned(),
            content: text.to_owned(),
        }
    }

    /// The code inside a pairing text.
    fn code_in(text: &str) -> String {
        let start = text.find("lg pair ").unwrap() + "lg pair ".len();
        text[start..].split('`').next().unwrap().to_owned()
    }

    fn echo(text: &str) -> String {
        format!("littleguy (offline): you said '{text}'")
    }

    #[test]
    fn the_pairing_text_names_the_code_or_says_try_later() {
        let text = pairing_text(Some("ABCD-1234"));
        assert!(text.contains("`lg pair ABCD-1234`"), "{text}");
        assert!(text.contains("10 minutes"), "{text}");
        assert!(pairing_text(None).contains("Try again"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bad_pair_requests_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = discord_open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(OfflineModel::new()),
        );
        let prompt = daemon
            .discord(&dm("1001", "111", "hello"))
            .await
            .unwrap()
            .unwrap();
        let code = code_in(&prompt.text);
        let refused = daemon
            .handle_request(UID + 1, Request::Pair { code: code.clone() })
            .await;
        assert!(matches!(refused, Response::Error { ref message } if message.contains("owner")));
        let typo = daemon
            .handle_request(
                UID,
                Request::Pair {
                    code: String::from("nope"),
                },
            )
            .await;
        assert!(
            matches!(typo, Response::Error { ref message } if message.contains("not a pairing code"))
        );
        let unknown = daemon
            .handle_request(
                UID,
                Request::Pair {
                    code: String::from("ZZZZ-ZZZZ"),
                },
            )
            .await;
        assert!(
            matches!(unknown, Response::Error { ref message } if message.contains("no live pairing"))
        );

        for request in [
            Request::Pairs,
            Request::Unpair {
                sender: String::from("discord:111"),
            },
        ] {
            let refused = daemon.handle_request(UID + 1, request).await;
            assert!(matches!(refused, Response::Error { .. }));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stranger_pairs_through_the_owner_and_unpairs() {
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let mut daemon = discord_open(&dir.path().join("lg.db"), &config(), Arc::clone(&model));

        let first = daemon
            .discord(&dm("1001", "111", "hello"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.channel_id, "dm111");
        assert_eq!(first.track, None, "a pairing prompt moves no cursor");
        let code = code_in(&first.text);
        assert_eq!(model.calls(), 0, "a stranger runs nothing");
        // The stranger asks again and gets the same code.
        let again = daemon
            .discord(&dm("1002", "111", "hello?"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(code_in(&again.text), code);

        let paired = daemon
            .handle_request(
                UID,
                Request::Pair {
                    code: code.to_lowercase(),
                },
            )
            .await;
        assert_eq!(
            paired,
            Response::Paired {
                sender: String::from("discord:111")
            }
        );
        let used = daemon.handle_request(UID, Request::Pair { code }).await;
        assert!(matches!(used, Response::Error { .. }), "a code works once");

        let reply = daemon
            .discord(&dm("1003", "111", "hi"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.text, echo("hi"));
        assert_eq!(reply.track, Some(discord_sender("111").unwrap()));
        assert_eq!(model.calls(), 1);

        let pairs = daemon.handle_request(UID, Request::Pairs).await;
        // An answer can hold a connect link, so the failure does not print it.
        let Response::Pairs { entries } = pairs else {
            panic!("`lg pairs` gave another answer");
        };
        let rows: Vec<(&str, &str, &str)> = entries
            .iter()
            .map(|e| (e.channel.as_str(), e.sender.as_str(), e.source.as_str()))
            .collect();
        assert_eq!(
            rows,
            vec![
                ("cli", "uid:1000", "owner"),
                ("discord", "discord:111", "paired")
            ]
        );

        let removed = daemon
            .handle_request(
                UID,
                Request::Unpair {
                    sender: String::from("discord:111"),
                },
            )
            .await;
        assert_eq!(removed, Response::Unpaired { removed: true });
        let gone = daemon
            .discord(&dm("1004", "111", "still there?"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            gone.track, None,
            "an unpaired sender gets a pairing prompt again"
        );
        let bad = daemon
            .handle_request(
                UID,
                Request::Unpair {
                    sender: String::new(),
                },
            )
            .await;
        assert_eq!(bad, Response::Unpaired { removed: false });
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pairing_survives_a_restart_and_gives_a_cursor() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        {
            let mut daemon = discord_open(&db, &config(), Arc::new(OfflineModel::new()));
            let prompt = daemon
                .discord(&dm("2001", "222", "hi"))
                .await
                .unwrap()
                .unwrap();
            let code = code_in(&prompt.text);
            daemon.handle_request(UID, Request::Pair { code }).await;
        }
        let model = Arc::new(OfflineModel::new());
        let mut daemon = discord_open(&db, &config(), Arc::clone(&model));
        assert_eq!(
            daemon.cursors(Service::Discord),
            vec![Cursor {
                channel_id: String::from("dm222"),
                after: String::from("2001")
            }],
            "the prompted message counts as answered"
        );
        assert_eq!(
            daemon.discord(&dm("2001", "222", "hi")).await.unwrap(),
            None
        );
        let reply = daemon
            .discord(&dm("2002", "222", "back"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(reply.text, echo("back"));
        assert_eq!(model.calls(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reply_goes_out_once_and_a_failed_one_is_sent_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        config.discord.allowed.push(discord_sender("333").unwrap());
        let model = Arc::new(OfflineModel::new());
        let mut daemon = discord_open(&dir.path().join("lg.db"), &config, Arc::clone(&model));
        assert!(
            daemon.cursors(Service::Discord).is_empty(),
            "no DM channel is known yet"
        );

        let first = daemon
            .discord(&dm("3001", "333", "one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.text, echo("one"));
        // The send failed, so no cursor moved. The same message comes back.
        let resent = daemon
            .discord(&dm("3001", "333", "one"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            resent.text,
            echo("one"),
            "the stored answer goes out, not a new run"
        );
        assert_eq!(model.calls(), 1);

        let sender = discord_sender("333").unwrap();
        daemon.delivered(&sender, "dm333", "3001").unwrap();
        assert_eq!(
            daemon.discord(&dm("3001", "333", "one")).await.unwrap(),
            None
        );
        assert_eq!(
            daemon.cursors(Service::Discord),
            vec![Cursor {
                channel_id: String::from("dm333"),
                after: String::from("3001")
            }]
        );

        // A sender from the config file stays allowed after an unpair.
        let removed = daemon
            .handle_request(
                UID,
                Request::Unpair {
                    sender: sender.to_string(),
                },
            )
            .await;
        assert_eq!(removed, Response::Unpaired { removed: false });
        let Response::Pairs { entries } = daemon.handle_request(UID, Request::Pairs).await else {
            panic!("not pairs");
        };
        assert!(
            entries
                .iter()
                .any(|e| e.sender == "discord:333" && e.source == "config")
        );
        let next = daemon
            .discord(&dm("3002", "333", "two"))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.text, echo("two"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bad_author_id_is_an_error_and_the_cli_takes_no_discord() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = discord_open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(OfflineModel::new()),
        );
        assert!(daemon.discord(&dm("1", "not-a-number", "x")).await.is_err());
        let mut plain = open(
            &dir.path().join("other").join("lg.db"),
            Arc::new(OfflineModel::new()),
        );
        assert_eq!(
            plain.discord(&dm("1", "444", "x")).await.unwrap(),
            None,
            "no discord rule drops it"
        );
    }

    #[cfg(feature = "discord")]
    /// The whole Discord path: a stranger pairs over the socket, then a
    /// reconnect reads the missed history and answers each message once.
    #[tokio::test(flavor = "multi_thread")]
    async fn discord_pairs_answers_and_backfills_once() {
        use littleguy_channels::testing::{FakeDiscord, dm_event};

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            uid,
            true,
        )
        .unwrap();
        let listener = bind(&socket).await.unwrap();
        let link = DiscordLink {
            rest: DiscordRest::new(fake.api_base.clone(), "token"),
            token: String::from("token"),
            gateway_url: fake.gateway_url.clone(),
        };
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let script = async {
            assert!(fake.wait_for(10, |f| !f.identifies().is_empty()).await);
            fake.push_dm("5001", "dm555", "555", "hello");
            assert!(fake.wait_for(10, |f| f.sent().len() == 1).await);
            let code = code_in(&fake.sent()[0].content);
            let paired = crate::client::send(&socket, &Request::Pair { code })
                .await
                .unwrap();
            assert_eq!(
                paired,
                Response::Paired {
                    sender: String::from("discord:555")
                }
            );

            fake.push_dm("5002", "dm555", "555", "hi");
            assert!(fake.wait_for(10, |f| f.sent().len() == 2).await);

            // The daemon misses two messages while it is away.
            for (id, text) in [
                ("5001", "hello"),
                ("5002", "hi"),
                ("5003", "missed"),
                ("5004", "also"),
            ] {
                fake.add_history(dm_event(id, "dm555", "555", text));
            }
            fake.drop_connection();
            assert!(fake.wait_for(10, |f| f.sent().len() == 4).await);
            // A live copy of a backfilled message runs nothing new.
            fake.push_dm("5004", "dm555", "555", "also");
            fake.push_dm("5005", "dm555", "555", "last");
            assert!(fake.wait_for(10, |f| f.sent().len() == 5).await);
            stop_tx.send(()).unwrap();
        };
        let (served, ()) = tokio::join!(
            run(
                daemon,
                listener,
                Links::with_discord(link),
                None,
                jobs(UID).1,
                stop
            ),
            script
        );
        served.unwrap();
        let sent: Vec<String> = fake.sent().into_iter().map(|s| s.content).collect();
        assert_eq!(
            sent[1..],
            [echo("hi"), echo("missed"), echo("also"), echo("last")]
        );
        assert!(fake.sent().iter().all(|s| s.channel_id == "dm555"));
        assert_eq!(model.calls(), 4);
        assert!(fake.connects() >= 2);
    }

    #[cfg(feature = "slack")]
    /// The whole Slack path: a stranger pairs over the socket, then a
    /// message gets one answer, and a missed message is read back once.
    #[tokio::test(flavor = "multi_thread")]
    async fn slack_pairs_answers_and_backfills_once() {
        use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack, dm_event};

        let fake = FakeSlack::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("lg.sock");
        let uid = rustix::process::getuid().as_raw();
        let model = Arc::new(OfflineModel::new());
        let extras = Extras {
            slack: true,
            ..Extras::default()
        };
        let daemon = Daemon::open_with(
            &dir.path().join("lg.db"),
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            uid,
            false,
            extras,
        )
        .unwrap();
        let listener = bind(&socket).await.unwrap();
        let links = Links::with_slack(SlackLink::Socket(SlackApi::new(
            fake.api_base.clone(),
            BOT_TOKEN,
            APP_TOKEN,
        )));
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let script =
            async {
                assert!(fake.wait_for(10, |f| f.connects() == 1).await);
                fake.push_dm("5001.000001", "D555", "U555", "hello");
                assert!(fake.wait_for(10, |f| f.posted().len() == 1).await);
                let code = code_in(&fake.posted()[0].text);
                let paired = crate::client::send(&socket, &Request::Pair { code })
                    .await
                    .unwrap();
                assert_eq!(
                    paired,
                    Response::Paired {
                        sender: String::from("slack:U555")
                    }
                );
                let pairs = crate::client::send(&socket, &Request::Pairs).await.unwrap();
                let Response::Pairs { entries } = pairs else {
                    panic!("not a list of pairs");
                };
                assert!(entries.iter().any(|e| e.channel == "slack"
                    && e.sender == "slack:U555"
                    && e.source == "paired"));

                fake.push_dm("5002.000001", "D555", "U555", "hi");
                assert!(fake.wait_for(10, |f| f.posted().len() == 2).await);

                // The daemon misses two messages while it is away.
                for (ts, text) in [
                    ("5001.000001", "hello"),
                    ("5002.000001", "hi"),
                    ("5002.000002", "missed"),
                    ("5003.000000", "also"),
                ] {
                    fake.add_history(dm_event(ts, "D555", "U555", text));
                }
                fake.drop_connection();
                assert!(fake.wait_for(10, |f| f.posted().len() == 4).await);
                // A live copy of a backfilled message runs nothing new.
                fake.push_dm("5003.000000", "D555", "U555", "also");
                fake.push_dm("5004.000000", "D555", "U555", "last");
                assert!(fake.wait_for(10, |f| f.posted().len() == 5).await);
                let unpaired = crate::client::send(
                    &socket,
                    &Request::Unpair {
                        sender: String::from("slack:U555"),
                    },
                )
                .await
                .unwrap();
                assert_eq!(unpaired, Response::Unpaired { removed: true });
                stop_tx.send(()).unwrap();
            };
        let (served, ()) = tokio::join!(
            run(daemon, listener, links, None, jobs(UID).1, stop),
            script
        );
        served.unwrap();
        let posted: Vec<String> = fake.posted().into_iter().map(|p| p.text).collect();
        assert_eq!(
            posted[1..],
            [echo("hi"), echo("missed"), echo("also"), echo("last")]
                .map(|text| littleguy_channels::slack::escape(&text))
        );
        assert!(fake.posted().iter().all(|p| p.channel == "D555"));
        assert_eq!(model.calls(), 4);
        assert!(fake.connects() >= 2);
    }

    /// Opens the daemon at `dir` with Slack on.
    #[cfg(feature = "slack")]
    fn slack_daemon(dir: &Path, model: &Arc<OfflineModel>) -> Daemon {
        let extras = Extras {
            slack: true,
            ..Extras::default()
        };
        let model = Arc::clone(model) as Arc<dyn AgentModel>;
        let uid = rustix::process::getuid().as_raw();
        Daemon::open_with(&dir.join("lg.db"), &config(), model, uid, false, extras).unwrap()
    }

    #[cfg(feature = "slack")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_events_api_reads_the_missed_messages_at_start() {
        use littleguy_channels::SigningSecret;
        use littleguy_channels::testing::slack::{BOT_TOKEN, FakeSlack, dm_event};

        let fake = FakeSlack::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(OfflineModel::new());
        let link = || SlackLink::Events {
            api: SlackApi::web_only(fake.api_base.clone(), BOT_TOKEN),
            secret: SigningSecret::new("s").unwrap(),
        };
        let uid = rustix::process::getuid().as_raw();
        let message = |ts: &str, text: &str| DmMessage {
            id: ts.to_owned(),
            channel_id: String::from("D555"),
            author_id: String::from("U555"),
            content: text.to_owned(),
        };

        // The first run takes two messages over the hooks.
        let daemon = slack_daemon(dir.path(), &model);
        let listener = bind(&dir.path().join("lg.sock")).await.unwrap();
        let (handle, queue) = jobs(uid);
        let inbox = daemon.slack_inbox(
            SlackApi::web_only(fake.api_base.clone(), BOT_TOKEN),
            &handle,
        );
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let script = async {
            inbox.message(message("5001.000001", "hello")).await;
            assert_eq!(fake.posted().len(), 1);
            let code = code_in(&fake.posted()[0].text);
            let paired = handle.ask(Request::Pair { code }).await.unwrap();
            assert!(matches!(paired, Response::Paired { .. }));
            inbox.message(message("5002.000001", "hi")).await;
            assert_eq!(fake.posted().len(), 2);
            // The queue runs in order, so this answer comes after the
            // cursor moves.
            handle.ask(Request::Pairs).await.unwrap();
            stop_tx.send(()).unwrap();
        };
        let (served, ()) = tokio::join!(
            run(
                daemon,
                listener,
                Links::with_slack(link()),
                None,
                queue,
                stop
            ),
            script
        );
        served.unwrap();
        drop((handle, inbox));

        // A message comes while the host sleeps, and its event is lost.
        for (ts, text) in [("5002.000001", "hi"), ("5003.000000", "missed")] {
            fake.add_history(dm_event(ts, "D555", "U555", text));
        }
        let daemon = slack_daemon(dir.path(), &model);
        let listener = bind(&dir.path().join("lg.sock")).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let script = async {
            assert!(fake.wait_for(10, |f| f.posted().len() == 3).await);
            stop_tx.send(()).unwrap();
        };
        let (served, ()) = tokio::join!(
            run(
                daemon,
                listener,
                Links::with_slack(link()),
                None,
                jobs(uid).1,
                stop
            ),
            script
        );
        served.unwrap();
        let posted = fake.posted();
        assert_eq!(
            posted[2].text,
            littleguy_channels::slack::escape(&echo("missed"))
        );
        assert_eq!(fake.connects(), 0, "the Events API opens no socket");
    }

    #[test]
    fn a_chat_message_has_a_key_and_a_cursor() {
        let message = DmMessage {
            id: String::from("5.000001"),
            channel_id: String::from("D1"),
            author_id: String::from("U1"),
            content: String::from("x"),
        };
        let slack = chat_message_id(Service::Slack, &message).unwrap();
        assert_eq!(slack.as_str(), "slack:D1:5.000001");
        assert_eq!(cursor_of(&slack), "5.000001");
        let discord = chat_message_id(Service::Discord, &message).unwrap();
        assert_eq!(cursor_of(&discord), "5.000001");
        assert_eq!(cursor_of(&MessageId::new("plain").unwrap()), "");
        assert_eq!(
            chat_sender(Service::Slack, "U1").unwrap().as_str(),
            "slack:U1"
        );
        assert!(chat_sender(Service::Slack, "42").is_err());
        assert!(chat_sender(Service::Discord, "U1").is_err());
    }

    #[cfg(feature = "camel")]
    fn camel_config(dir: &Path, noop: bool) -> Config {
        let mut config = config();
        let from = format!(
            "file:{}?initialDelay=0&delay=100&{}",
            dir.display(),
            if noop { "noop=true" } else { "delete=true" }
        );
        config.camel.push(littleguy_core::CamelRoute {
            id: String::from("inbox"),
            from,
            prompt: Some(String::from("A file arrived.")),
        });
        config
    }

    #[cfg(feature = "camel")]
    /// Runs the daemon with the camel routes of `config` until `done`
    /// holds or 15 seconds pass. Returns whether `done` held.
    async fn run_camel(
        db: &Path,
        config: &Config,
        model: Arc<OfflineModel>,
        done: impl Fn(&OfflineModel) -> bool + Sync,
    ) -> bool {
        let dir = db.parent().unwrap();
        let daemon = Daemon::open(
            db,
            config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap();
        let camel = Camel::start(daemon.camel_mapper(config), db).await.unwrap();
        let listener = bind(&dir.join("lg.sock")).await.unwrap();
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let stop = async {
            let _ = stop_rx.await;
        };
        let watch = async {
            let mut held = false;
            for _ in 0..150 {
                if done(&model) {
                    held = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            stop_tx.send(()).unwrap();
            held
        };
        let (served, held) = tokio::join!(
            run(
                daemon,
                listener,
                Links::default(),
                Some(camel),
                jobs(UID).1,
                stop
            ),
            watch
        );
        served.unwrap();
        held
    }

    #[cfg(feature = "camel")]
    /// A file in the watched directory becomes one message for the
    /// assistant, and the route deletes it once the write is durable.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_route_feeds_the_assistant() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("in");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("note.txt"), "buy milk").unwrap();
        // A body that is not UTF-8 is poison: a dead letter, not a turn.
        std::fs::write(inbox.join("bad.bin"), [0xff_u8, 0xfe]).unwrap();
        let db = dir.path().join("lg.db");
        let config = camel_config(&inbox, false);
        let model = Arc::new(OfflineModel::new());
        let held = run_camel(&db, &config, Arc::clone(&model), |m| m.calls() >= 1).await;
        assert!(held, "the file never reached the assistant");
        assert_eq!(model.calls(), 1);
        let daemon = open(&db, Arc::new(OfflineModel::new()));
        let state = daemon.state().await.unwrap().unwrap();
        assert_eq!(
            state.last_answer.as_deref(),
            Some("littleguy (offline): you said 'A file arrived.\n\nbuy milk'")
        );
        drop(daemon);
        for _ in 0..50 {
            if std::fs::read_dir(&inbox).unwrap().next().is_none() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let left: Vec<_> = std::fs::read_dir(&inbox).unwrap().collect();
        assert!(
            left.is_empty(),
            "both files are settled and deleted: {left:?}"
        );
    }

    #[cfg(feature = "camel")]
    /// A route with `noop=true` reads every file again after a restart. The
    /// signal key of each file is the same, so a file runs once.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_read_again_after_a_restart_runs_once() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("in");
        std::fs::create_dir_all(&inbox).unwrap();
        std::fs::write(inbox.join("a.txt"), "first").unwrap();
        let db = dir.path().join("lg.db");
        let config = camel_config(&inbox, true);
        let first = Arc::new(OfflineModel::new());
        assert!(run_camel(&db, &config, Arc::clone(&first), |m| m.calls() >= 1).await);
        assert_eq!(first.calls(), 1);

        std::fs::write(inbox.join("b.txt"), "second").unwrap();
        let second = Arc::new(OfflineModel::new());
        assert!(run_camel(&db, &config, Arc::clone(&second), |m| m.calls() >= 1).await);
        // The poll that read b.txt read a.txt too. Give it time to misbehave.
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(second.calls(), 1, "a.txt ran once, before the restart");
        let daemon = open(&db, Arc::new(OfflineModel::new()));
        let state = daemon.state().await.unwrap().unwrap();
        assert!(
            state
                .last_answer
                .as_deref()
                .is_some_and(|a| a.contains("second")),
            "{state:?}"
        );
    }

    #[cfg(feature = "camel")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_camel_channel_allows_only_the_config_routes() {
        let dir = tempfile::tempdir().unwrap();
        let config = camel_config(dir.path(), true);
        let daemon = open_with(&dir.path().join("lg.db"), &config);
        let channel = ChannelId::new(crate::camel::CAMEL_CHANNEL).unwrap();
        let allowed: Vec<&str> = daemon
            .routing
            .allowed(&channel)
            .map(SenderId::as_str)
            .collect();
        assert_eq!(allowed, vec!["camel:inbox"]);
        let plain = open(
            &dir.path().join("other").join("lg.db"),
            Arc::new(OfflineModel::new()),
        );
        assert_eq!(plain.routing.allowed(&channel).count(), 0);
    }

    fn open_with(db: &Path, config: &Config) -> Daemon {
        Daemon::open(db, config, Arc::new(OfflineModel::new()), UID, false).unwrap()
    }

    #[cfg(not(feature = "oauth"))]
    #[tokio::test]
    async fn oauth_requests_name_the_missing_feature() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()));
        let requests = [
            Request::Connections,
            Request::Connect {
                integration: String::from("google"),
            },
            Request::Disconnect {
                integration: String::from("google"),
                connection: String::from("c-1"),
            },
        ];
        for request in requests {
            // The listing is off, not broken. An action fails.
            let listing = matches!(request, Request::Connections);
            let message = match daemon.handle_request(UID, request).await {
                Response::Off { message } if listing => message,
                Response::Error { message } if !listing => message,
                _ => panic!("the wrong kind of answer"),
            };
            assert!(message.contains("`oauth` feature"));
        }
    }

    #[cfg(feature = "oauth")]
    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_requests_are_for_the_owner_and_need_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon = open(&db, Arc::new(OfflineModel::new()));
        let refused = daemon.handle_request(UID + 1, Request::Connections).await;
        assert!(matches!(refused, Response::Error { ref message } if message.contains("owner")));
        // With no [san_mateo], the listing is off and a connect fails.
        let none = daemon.handle_request(UID, Request::Connections).await;
        assert!(matches!(none, Response::Off { ref message } if message.contains("[san_mateo]")));
        let connect = Request::Connect {
            integration: String::from("acme"),
        };
        let failed = daemon.handle_request(UID, connect).await;
        assert!(
            matches!(failed, Response::Error { ref message } if message.contains("[san_mateo]"))
        );

        let fake = san_mateo::MemoryTransport::new();
        let config = crate::oauth::tests::section("acme", None);
        let connections = crate::oauth::tests::start(&db, &config, &fake)
            .await
            .unwrap();
        let mut daemon = daemon.with_connections(connections);
        let link = daemon
            .handle_request(
                UID,
                Request::Connect {
                    integration: String::from("acme"),
                },
            )
            .await;
        // The link holds a session token, so the assertion does not print it.
        assert!(
            matches!(&link, Response::ConnectLink { link } if link.contains("connect_session_token="))
        );
        let unknown = daemon
            .handle_request(
                UID,
                Request::Connect {
                    integration: String::from("x"),
                },
            )
            .await;
        assert!(matches!(unknown, Response::Error { .. }));
        // These answers come from San Mateo. The checks do not print them.
        let list = daemon.handle_request(UID, Request::Connections).await;
        assert!(matches!(list, Response::Connections { ref entries } if entries.is_empty()));
        let gone = daemon
            .handle_request(
                UID,
                Request::Disconnect {
                    integration: String::from("acme"),
                    connection: String::from("c-1"),
                },
            )
            .await;
        assert!(matches!(gone, Response::Disconnected { removed: false }));
        if let Some(connections) = daemon.connections.take() {
            connections.stop().await;
        }
    }

    /// A model for the memory tests. `remember <note>` calls the memory
    /// tool. `save <path> <text>` and `show <path>` call the file tools.
    /// `run <code>` calls `run_code`, and answers `ran: <stdout>`.
    /// `use <tool> <json>` calls any tool, and answers `got: <text>` for a
    /// text result.
    /// `recall` answers with the notes in its prompt, `quiet` gets
    /// `HEARTBEAT_OK`, and any other text gets `ok: <text>`. A
    /// consolidation pass gets one fact per episode that says `tea`.
    #[derive(Debug, Default)]
    struct MemoryModel {
        calls: std::sync::atomic::AtomicU64,
        passes: std::sync::atomic::AtomicU64,
        /// The system prompt of the last call that offered tools: the
        /// memory snapshot that the assistant saw.
        seen: std::sync::Mutex<String>,
    }

    impl MemoryModel {
        fn text(message: &autumn_harvest_agent::ChatMessage) -> String {
            message
                .content
                .iter()
                .filter_map(|part| match part {
                    autumn_harvest_agent::ContentPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        }

        /// The answer to a tool result: the text of a read, the output of
        /// a run, a plain text result, or the whole result of a run-log
        /// tool.
        fn echo(result: &serde_json::Value) -> Option<String> {
            let ran = result["stdout"]
                .as_str()
                .map(|out| format!("ran: {}", out.trim()));
            let said = result.as_str().map(|text| format!("got: {text}"));
            let logged = [
                "runs",
                "failures",
                "models",
                "idea",
                "goal",
                "wake",
                "skill_draft",
                "skills",
                "ticks",
                "build",
                "patch",
                "patches",
            ]
            .iter()
            .any(|key| result.get(key).is_some())
            .then(|| format!("log: {result}"));
            let failed = result["error"]
                .as_str()
                .map(|error| format!("error: {error}"));
            result["text"]
                .as_str()
                .map(str::to_owned)
                .or(ran)
                .or(said)
                .or(failed)
                .or(logged)
        }

        fn reply(request: &autumn_harvest_agent::ChatRequest) -> autumn_harvest_agent::ContentPart {
            use autumn_harvest_agent::{ChatRole, ContentPart};
            let system = request
                .messages
                .first()
                .filter(|message| message.role == ChatRole::System)
                .map(Self::text)
                .unwrap_or_default();
            let last = request.messages.last();
            let user = last.map(Self::text).unwrap_or_default();
            // A tick prompt starts with its mark line. The model reads past it.
            let user = if crate::wake::is_tick(&user) {
                user.split_once('\n')
                    .map_or_else(String::new, |(_, rest)| rest.to_owned())
            } else {
                user
            };
            if system == autumn_harvest_agent::consolidate::PASS_SYSTEM {
                let facts: Vec<serde_json::Value> = user
                    .lines()
                    .filter(|line| line.contains("tea"))
                    .filter_map(|line| line.split("id=\"").nth(1)?.split('"').next())
                    .map(|id| {
                        serde_json::json!({
                            "subject": "mark", "relation": "likes", "object": "tea",
                            "episodes": [id],
                        })
                    })
                    .collect();
                return ContentPart::Text(serde_json::json!({ "facts": facts }).to_string());
            }
            let answered = last.is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|part| matches!(part, ContentPart::ToolResult { .. }))
            });
            if answered {
                // A read answers with the text of the file.
                let read = last
                    .into_iter()
                    .flat_map(|message| &message.content)
                    .find_map(|part| match part {
                        ContentPart::ToolResult { content, .. } => {
                            serde_json::from_str::<serde_json::Value>(content).ok()
                        }
                        _ => None,
                    })
                    .and_then(|result| Self::echo(&result));
                return ContentPart::Text(read.unwrap_or_else(|| String::from("noted")));
            }
            let file_call = |name: &str, arguments: serde_json::Value| ContentPart::ToolCall {
                id: String::from("call-1"),
                name: String::from(name),
                arguments,
            };
            if let Some(rest) = user.strip_prefix("save ") {
                let (path, text) = rest.split_once(' ').unwrap_or((rest, ""));
                return file_call(
                    "write_file",
                    serde_json::json!({"path": path, "text": text}),
                );
            }
            if let Some(code) = user.strip_prefix("run ") {
                return file_call(
                    crate::sandbox::TOOL,
                    serde_json::json!({"runtime": "sh", "code": code}),
                );
            }
            if let Some(rest) = user.strip_prefix("use ") {
                let (tool, input) = rest.split_once(' ').unwrap_or((rest, "{}"));
                let input = serde_json::from_str(input).unwrap_or_default();
                return file_call(tool, input);
            }
            if let Some(path) = user.strip_prefix("show ") {
                return file_call("read_file", serde_json::json!({"path": path}));
            }
            if let Some(note) = user.strip_prefix("remember ") {
                return ContentPart::ToolCall {
                    id: String::from("call-1"),
                    name: String::from(autumn_harvest_agent::memory::MEMORY_TOOL),
                    arguments: serde_json::json!({
                        "action": "add", "block": "memory", "text": note,
                    }),
                };
            }
            if user == "quiet" {
                return ContentPart::Text(String::from(
                    autumn_harvest_agent::delivery::HEARTBEAT_OK,
                ));
            }
            if user != "recall" {
                return ContentPart::Text(format!("ok: {user}"));
            }
            let notes: Vec<&str> = system
                .lines()
                .filter_map(|line| line.strip_prefix("- "))
                .collect();
            ContentPart::Text(format!("I know: {}", notes.join("; ")))
        }
    }

    impl AgentModel for MemoryModel {
        fn chat<'a>(
            &'a self,
            request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            use std::sync::atomic::Ordering;
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                let content = Self::reply(request);
                if request.tools.is_empty() {
                    self.passes.fetch_add(1, Ordering::SeqCst);
                } else if let Some(system) = request.messages.first() {
                    *self
                        .seen
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner) = Self::text(system);
                }
                let stop_reason =
                    if matches!(content, autumn_harvest_agent::ContentPart::ToolCall { .. }) {
                        autumn_harvest_agent::StopReason::ToolUse
                    } else {
                        autumn_harvest_agent::StopReason::EndTurn
                    };
                Ok(autumn_harvest_agent::ChatResponse {
                    content: vec![content],
                    stop_reason,
                    usage: autumn_harvest_agent::TokenUsage::new(1, 1),
                })
            })
        }
    }

    #[test]
    fn an_early_stop_is_never_an_empty_reply() {
        use autumn_harvest_agent::AgentStop;
        let Response::Reply { text, .. } = answer_of(Some(AgentStop::Completed), " Hi. ") else {
            panic!("a completed op with text is a reply");
        };
        assert_eq!(text, "Hi.");
        assert!(matches!(
            answer_of(Some(AgentStop::Completed), ""),
            Response::NoAnswer { .. }
        ));
        let Response::NoAnswer { reason } = answer_of(Some(AgentStop::StepsExhausted), "") else {
            panic!("an early stop with no text is no answer");
        };
        assert!(reason.contains("tool round"));
        let Response::Reply { text, .. } = answer_of(Some(AgentStop::LoopDetected), "Half done.")
        else {
            panic!("an early stop keeps its text");
        };
        assert!(text.starts_with("Half done."));
        assert!(text.contains("I stopped early"));
    }

    /// A model that asks for another tool call each time, so an op runs out
    /// of tool rounds.
    #[derive(Debug, Default)]
    struct BusyModel {
        calls: std::sync::atomic::AtomicU64,
    }

    impl AgentModel for BusyModel {
        fn chat<'a>(
            &'a self,
            _request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            use std::sync::atomic::Ordering;
            Box::pin(async move {
                // Each call reads another path, so no two calls repeat.
                let n = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(autumn_harvest_agent::ChatResponse {
                    content: vec![autumn_harvest_agent::ContentPart::ToolCall {
                        id: format!("c{n}"),
                        name: String::from("read_file"),
                        arguments: serde_json::json!({"path": format!("notes/{n}.md")}),
                    }],
                    stop_reason: autumn_harvest_agent::StopReason::ToolUse,
                    usage: autumn_harvest_agent::TokenUsage::new(1, 1),
                })
            })
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_that_runs_out_of_tool_rounds_says_so() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let busy = Arc::new(BusyModel::default());
        let mut config = config();
        config.max_steps = Some(3);
        let model: Arc<dyn AgentModel> = Arc::clone(&busy) as Arc<dyn AgentModel>;
        let mut daemon =
            Daemon::open(&dir.path().join("lg.db"), &config, model, UID, false).unwrap();
        let Response::NoAnswer { reason } = daemon.handle_request(UID, request("b-1", "dig")).await
        else {
            panic!("an op that ran out of rounds answered");
        };
        assert!(reason.contains("tool round"));
        // The config caps the rounds: three ran, and the fourth call found
        // the cap.
        assert_eq!(busy.calls.load(Ordering::SeqCst), 4);
    }

    /// Opens a daemon on `db` with `max_steps` and a [`BusyModel`], sends
    /// one message, and returns the model calls of that message.
    async fn busy_calls(daemon: &mut Daemon, busy: &BusyModel, key: &str) -> u64 {
        use std::sync::atomic::Ordering;
        let before = busy.calls.load(Ordering::SeqCst);
        let response = daemon.handle_request(UID, request(key, "dig")).await;
        assert!(matches!(response, Response::NoAnswer { .. }));
        busy.calls.load(Ordering::SeqCst) - before
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reload_reaches_the_next_message() {
        let dir = tempfile::tempdir().unwrap();
        let busy = Arc::new(BusyModel::default());
        let mut config = config();
        config.max_steps = Some(3);
        let model: Arc<dyn AgentModel> = Arc::clone(&busy) as Arc<dyn AgentModel>;
        let mut daemon =
            Daemon::open(&dir.path().join("lg.db"), &config, model, UID, false).unwrap();
        assert_eq!(busy_calls(&mut daemon, &busy, "r-1").await, 4);
        config.max_steps = Some(5);
        daemon.reload(config).unwrap();
        // The run that read three rounds ended, so this message has five.
        assert_eq!(busy_calls(&mut daemon, &busy, "r-2").await, 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_reaches_the_next_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let busy = Arc::new(BusyModel::default());
        let model: Arc<dyn AgentModel> = Arc::clone(&busy) as Arc<dyn AgentModel>;
        let mut config = config();
        config.max_steps = Some(3);
        let mut daemon = Daemon::open(&db, &config, Arc::clone(&model), UID, false).unwrap();
        assert_eq!(busy_calls(&mut daemon, &busy, "s-1").await, 4);
        drop(daemon);
        config.max_steps = Some(5);
        let mut daemon = Daemon::open(&db, &config, model, UID, false).unwrap();
        assert_eq!(busy_calls(&mut daemon, &busy, "s-2").await, 6);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_ended_run_goes_after_keep_days_and_the_running_one_stays() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(OfflineModel::new()),
            UID,
            false,
        )
        .unwrap();
        for key in ["k-1", "k-2"] {
            let response = daemon.handle_request(UID, request(key, "hello")).await;
            assert!(matches!(response, Response::Reply { .. }));
        }
        let now = Timestamp::now();
        // Each message ended its run. Within thirty days, every run stays.
        let early = daemon
            .purge(now + jiff::SignedDuration::from_hours(24 * 29))
            .unwrap();
        assert_eq!(early.map(|report| report.removed), Some(0));
        // The purge waits a day before it looks again.
        let soon = daemon.purge(
            now + jiff::SignedDuration::from_hours(24 * 29) + jiff::SignedDuration::from_hours(1),
        );
        assert!(soon.unwrap().is_none());
        let late = daemon
            .purge(now + jiff::SignedDuration::from_hours(24 * 31))
            .unwrap()
            .unwrap();
        assert!(late.removed >= 2, "the ended runs of both messages go");
        assert!(late.complete);
        // The run that waits for the next message stays, and it answers.
        assert!(
            sqlite::current_run(&daemon.rt, daemon.assistant())
                .unwrap()
                .is_some()
        );
        let response = daemon.handle_request(UID, request("k-3", "hello")).await;
        assert!(matches!(response, Response::Reply { .. }));
    }

    fn reply_text(response: &Response) -> &str {
        match response {
            Response::Reply { text, .. } => text,
            _ => "",
        }
    }

    fn notes(db: &Path) -> Vec<String> {
        use autumn_harvest_agent::memory::{MemoryScope, MemoryStore};
        let stores = littleguy_stores::SqliteStores::open(crate::memory::path_for(db)).unwrap();
        let scope = MemoryScope::new("agent:assistant/mark");
        let blocks = futures_ready(stores.load(&scope)).unwrap();
        blocks
            .into_iter()
            .find(|block| block.label == "memory")
            .map(|block| block.entries)
            .unwrap_or_default()
    }

    /// Runs a store future, which is ready at once.
    fn futures_ready<T>(future: impl std::future::Future<Output = T>) -> T {
        use std::task::{Context, Poll, Waker};
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("a store call waited"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_note_reaches_the_next_run_and_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        {
            let mut daemon =
                Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
            let noted = daemon
                .handle_request(UID, request("m-1", "remember Mark drinks tea"))
                .await;
            assert_eq!(reply_text(&noted), "noted");
        }
        assert_eq!(notes(&db), vec![String::from("Mark drinks tea")]);

        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        let recall = daemon.handle_request(UID, request("m-2", "recall")).await;
        assert_eq!(reply_text(&recall), "I know: Mark drinks tea");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_nightly_pass_turns_episodes_into_facts_once() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let model = Arc::new(MemoryModel::default());
        let mut daemon = Daemon::open(
            &db,
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap();
        daemon
            .handle_request(UID, request("m-1", "I like tea"))
            .await;
        daemon
            .handle_request(UID, request("m-2", "the weather is bad"))
            .await;

        // Fire the nightly schedule now, as 03:00 would.
        let schedules = daemon.rt.list_schedules().unwrap();
        assert_eq!(schedules.len(), 1);
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        let passes = model.passes.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(passes, 1);
        let facts = notes(&db);
        assert_eq!(facts.len(), 1, "{facts:?}");
        assert!(
            facts[0].starts_with("mark likes tea [episodes: "),
            "{facts:?}"
        );

        // No new episode waits, so the next firing calls no model.
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        assert_eq!(model.passes.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn memory_off_keeps_no_notes_and_no_schedule() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut on = config();
        drop(open_with(&db, &on));
        on.memory.enabled = false;
        // The goals and ideas live in the memory file too.
        on.tools.plans = false;
        let mut daemon = open_with(&db, &on);
        assert!(daemon.rt.list_schedules().unwrap().is_empty());
        let reply = daemon.handle_request(UID, request("m-1", "hi")).await;
        assert!(matches!(reply, Response::Reply { .. }));
        // A daemon that never kept memory opens no memory file.
        let fresh = dir.path().join("fresh");
        std::fs::create_dir(&fresh).unwrap();
        drop(open_with(&fresh.join("lg.db"), &on));
        assert!(!crate::memory::path_for(&fresh.join("lg.db")).exists());
    }

    #[test]
    fn a_bad_consolidation_schedule_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = config();
        bad.memory.timezone = String::from("Mars/Olympus");
        let error = Daemon::open(
            &dir.path().join("lg.db"),
            &bad,
            Arc::new(OfflineModel::new()),
            UID,
            false,
        )
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("memory.consolidate"),
            "{error:#}"
        );
    }

    fn brief_config(prompt: &str, to: Option<&str>) -> Config {
        let mut config = config();
        config.brief = Some(littleguy_core::BriefConfig {
            schedule: String::from("0 8 * * *"),
            timezone: String::from("UTC"),
            prompt: String::from(prompt),
            to: to.map(|sender| SenderId::new(sender).unwrap()),
        });
        config
    }

    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    fn heartbeat_config() -> Config {
        let mut config = config();
        config.heartbeat = Some(littleguy_core::HeartbeatConfig {
            every_minutes: 60,
            quiet: None,
            timezone: String::from("UTC"),
            research: true,
            deliver: littleguy_core::HeartbeatDelivery::Web,
            prompt: String::from("check in"),
        });
        config
    }

    #[cfg(all(
        any(feature = "heartbeat", feature = "reflection"),
        any(feature = "discord", feature = "sandbox")
    ))]
    fn armed(daemon: &Daemon) -> Option<Timestamp> {
        daemon
            .wake
            .as_ref()
            .and_then(Wake::armed_at)
            .map(|(at, _)| at)
    }

    #[cfg(all(
        any(feature = "heartbeat", feature = "reflection"),
        any(feature = "discord", feature = "sandbox")
    ))]
    fn utc(text: &str) -> Timestamp {
        text.parse::<Timestamp>().unwrap()
    }

    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_heartbeat_wakes_the_model_only_with_work_and_speaks_in_the_chat() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-14T12:00:00Z");
        let hand = clock.clone();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &heartbeat_config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.tick().await.unwrap();
        assert_eq!(armed(&daemon), Some(utc("2026-11-14T13:00:00Z")));

        // No goal is due and no idea waits: the tick costs no model call.
        clock.set("2026-11-14T13:00:01Z");
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);
        assert_eq!(armed(&daemon), Some(utc("2026-11-14T14:00:01Z")));
        // The tick log says the precheck skipped it.
        #[cfg(feature = "self-debug")]
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        #[cfg(feature = "self-debug")]
        {
            let skipped = stores.recent_ticks(1).unwrap();
            assert_eq!(skipped[0].kind, "heartbeat");
            assert!(skipped[0].outcome.starts_with("skipped by the precheck"));
        }

        // An idea waits, so the next heartbeat runs, and it speaks.
        daemon
            .handle_request(
                UID,
                request(
                    "h-1",
                    r#"use add_idea {"title": "Docs", "body": "Links rot."}"#,
                ),
            )
            .await;
        let before = model.calls.load(Ordering::SeqCst);
        clock.set("2026-11-14T14:00:02Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        let Response::History { turns } = daemon.handle_request(UID, Request::History).await else {
            panic!("no history");
        };
        let last = turns.last().unwrap();
        assert_eq!(last.from, Speaker::Unprompted);
        assert_eq!(last.text, "ok: check in");
        // This time the heartbeat ran. The log row comes at the next arm.
        clock.set("2026-11-14T14:00:30Z");
        daemon.tick().await.unwrap();
        #[cfg(feature = "self-debug")]
        // It says what the tick did, and the person's add_idea is not part
        // of it.
        assert_eq!(
            stores.recent_ticks(1).unwrap()[0].outcome,
            "ran; no tool calls; answered: ok: check in"
        );
        // The message prints no turn, so a failure logs no conversation.
        assert!(
            !turns.iter().any(|turn| turn.text.contains("[littleguy")),
            "a tick prompt shows in the history"
        );
    }

    /// A model whose consolidation replies do not parse, and that answers
    /// the assistant as [`MemoryModel`] does.
    #[derive(Debug, Default)]
    struct BadPassModel {
        inner: MemoryModel,
    }

    impl AgentModel for BadPassModel {
        fn chat<'a>(
            &'a self,
            request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            let pass = request.messages.first().is_some_and(|first| {
                MemoryModel::text(first) == autumn_harvest_agent::consolidate::PASS_SYSTEM
            });
            if !pass {
                return self.inner.chat(request);
            }
            Box::pin(async move {
                // Cut off, as at an output cap.
                Ok(autumn_harvest_agent::ChatResponse {
                    content: vec![autumn_harvest_agent::ContentPart::Text(String::from(
                        r#"{"facts": [{"subject": "mark", "relation": "lik"#,
                    ))],
                    stop_reason: autumn_harvest_agent::StopReason::MaxTokens,
                    usage: autumn_harvest_agent::TokenUsage::new(1, 1),
                })
            })
        }
    }

    #[cfg(all(feature = "reflection", feature = "self-debug"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pass_that_wrote_no_fact_wakes_no_reflection_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(BadPassModel::default()),
            UID,
            false,
        )
        .unwrap();
        daemon
            .handle_request(UID, request("r-1", "I like tea"))
            .await;
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        daemon.tick().await.unwrap();
        assert!(
            daemon
                .wake
                .as_ref()
                .is_none_or(|wake| wake.pending_reflection().is_none()),
            "no reflection on a pass that learned nothing"
        );
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let row = &stores.recent_ticks(1).unwrap()[0];
        assert_eq!(row.kind, "reflection");
        assert_eq!(
            row.outcome,
            "skipped: the consolidation pass sent no facts (output_capped) and ended without \
             a mark, so 1 episode waits for the next pass; no model call"
        );
    }

    /// The fixed pass cap of autumn-harvest-agent 0.10.
    const OLD_PASS_CAP: u32 = 2048;

    /// A model that reasons: a pass reply under a cap of
    /// [`OLD_PASS_CAP`] is cut off, as the reasoning fills it. With more room it answers as
    /// [`MemoryModel`] does.
    #[derive(Debug, Default)]
    struct ReasoningModel {
        inner: MemoryModel,
        cut: BadPassModel,
    }

    impl AgentModel for ReasoningModel {
        fn chat<'a>(
            &'a self,
            request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            let cramped = request.max_tokens.is_some_and(|cap| cap <= OLD_PASS_CAP);
            if cramped {
                self.cut.chat(request)
            } else {
                self.inner.chat(request)
            }
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pass_gets_room_for_a_model_that_reasons() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon = Daemon::open(
            &db,
            &config(),
            Arc::new(ReasoningModel::default()),
            UID,
            false,
        )
        .unwrap();
        daemon
            .handle_request(UID, request("r-1", "I like tea"))
            .await;
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        let facts = notes(&db);
        assert_eq!(facts.len(), 1, "the pass wrote its fact: {facts:?}");
    }

    /// Fire the nightly schedule now, as 03:00 would, and read the pass
    /// that it ran.
    #[cfg(feature = "reflection")]
    async fn run_one_pass(daemon: &mut Daemon) -> crate::reflection::Pass {
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        let filter = autumn_harvest_sqlite::ExecutionFilter {
            workflow_name: Some(String::from(
                autumn_harvest_agent::consolidate::CONSOLIDATE_WORKFLOW_NAME,
            )),
            ..autumn_harvest_sqlite::ExecutionFilter::default()
        };
        let listed = daemon.rt.list_executions(&filter).unwrap().executions;
        let autumn_harvest_sqlite::Stored::Value(id) = &listed[0].exec_id else {
            panic!("no pass id");
        };
        crate::reflection::read_pass(&daemon.rt, id).unwrap()
    }

    #[cfg(feature = "reflection")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_read_pass_names_its_instance_and_the_episodes_that_it_marked() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap();
        daemon
            .handle_request(UID, request("r-1", "I like tea"))
            .await;
        let before = Timestamp::now();
        let pass = run_one_pass(&mut daemon).await;
        assert_eq!(pass.stop, "completed");
        assert!(pass.marked);
        assert_eq!(pass.episodes, 1);
        assert_eq!(pass.episode_ids.len(), 1, "{pass:?}");
        // The fact cites the episode that the pass read.
        assert!(pass.facts[0].contains(&pass.episode_ids[0]), "{pass:?}");
        assert_ne!(pass.exec, "");
        let started = pass.started.expect("a start time");
        assert!(started >= before - jiff::SignedDuration::from_secs(1));
        let text = crate::reflection::prompt(std::slice::from_ref(&pass)).unwrap();
        assert!(text.contains(&format!("- pass {}, started ", pass.exec)));
        assert!(text.contains(&format!("({}) and marked them", pass.episode_ids[0])));
        // The next op reads the block again, so it already holds the fact.
        let recall = daemon.handle_request(UID, request("r-2", "recall")).await;
        // The assertion prints no reply, so a failure logs no conversation.
        assert!(reply_text(&recall).contains(&pass.facts[0]));
    }

    #[cfg(feature = "reflection")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_pass_that_ended_without_a_mark_reads_the_same_episodes_again() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(BadPassModel::default()),
            UID,
            false,
        )
        .unwrap();
        daemon
            .handle_request(UID, request("r-1", "I like tea"))
            .await;
        let first = run_one_pass(&mut daemon).await;
        assert_eq!(first.stop, "output_capped");
        assert!(!first.marked);
        assert_eq!(first.facts, Vec::<String>::new());
        assert_eq!(first.episode_ids.len(), 1, "{first:?}");
        // The episode stays pending, so a second pass reads the same id.
        let second = run_one_pass(&mut daemon).await;
        assert_ne!(second.exec, first.exec, "another pass instance");
        assert_eq!(second.episode_ids, first.episode_ids);
    }

    #[cfg(all(feature = "reflection", any(feature = "discord", feature = "sandbox")))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_consolidation_pass_wakes_one_reflection_then_the_timer_stops() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-14T03:00:00Z");
        let hand = clock.clone();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon
            .handle_request(UID, request("r-1", "I like tea"))
            .await;
        daemon
            .rt
            .trigger_schedule(
                autumn_harvest_agent::consolidate_all::CONSOLIDATE_ALL_WORKFLOW_NAME,
                false,
                None,
            )
            .unwrap();
        daemon.rt.run_until_idle().await.unwrap();
        assert_eq!(model.passes.load(Ordering::SeqCst), 1);

        // The tick sees the pass and plans the reflection at the engine minimum.
        daemon.tick().await.unwrap();
        assert_eq!(armed(&daemon), Some(utc("2026-11-14T03:01:00Z")));
        let before = model.calls.load(Ordering::SeqCst);
        clock.set("2026-11-14T03:01:01Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        let Response::History { turns } = daemon.handle_request(UID, Request::History).await else {
            panic!("no history");
        };
        let last = turns.last().unwrap();
        assert_eq!(last.from, Speaker::Unprompted);
        // The assertion prints no turn, so a failure logs no conversation.
        assert!(
            last.text
                .starts_with("ok: Your memory was just consolidated")
        );
        // It says what the pass sent, not only that it ran, and never
        // that the block changed.
        assert!(last.text.contains("read 1 episode ("));
        assert!(last.text.contains("and sent 1 fact to your memory store:"));
        assert!(!last.text.contains("added"));
        assert!(last.text.contains("mark likes tea"));
        // The memory block of the reflection op holds the fact already: the
        // op reads its snapshot when it starts, after the pass ended.
        let seen = model.seen.lock().unwrap().clone();
        assert!(
            seen.contains("mark likes tea"),
            "the snapshot holds the fact"
        );

        // The pass counts once, and nothing else is planned: the timer stops.
        let after = model.calls.load(Ordering::SeqCst);
        for minute in 3..6 {
            clock.set(&format!("2026-11-14T03:0{minute}:30Z"));
            daemon.tick().await.unwrap();
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), after);
        let booked = sqlite::state(&daemon.rt, daemon.assistant())
            .await
            .unwrap()
            .and_then(|state| state.next_heartbeat_ms);
        assert_eq!(booked, None);
    }

    #[cfg(any(feature = "discord", feature = "sandbox"))]
    #[cfg(feature = "followups")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_booked_wake_runs_once_at_its_time_then_the_timer_stops() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        // The tool reads the wall clock, so the daemon clock starts there.
        let start = Timestamp::now();
        let clock = HandClock::at(&crate::stamp::rfc3339(start));
        let hand = clock.clone();
        // No nightly pass: on the wall clock, one can fall in this test.
        let mut quiet = config();
        quiet.memory.enabled = false;
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &quiet,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        let booked = daemon
            .handle_request(
                UID,
                request(
                    "w-1",
                    r#"use wake_me {"in_minutes": 10, "about": "check the deploy"}"#,
                ),
            )
            .await;
        assert!(reply_text(&booked).starts_with(r#"log: {"at":"#));
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let pending = stores.pending_wakes().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].about, "check the deploy");

        // The next tick aims the timer at the wake.
        daemon.tick().await.unwrap();
        let (at, kind) = daemon.wake.as_ref().and_then(Wake::armed_at).unwrap();
        assert_eq!(kind, crate::wake::Kind::Followup);
        assert_eq!(at.as_millisecond(), pending[0].at_ms);

        let before = model.calls.load(Ordering::SeqCst);
        clock.set(&crate::stamp::rfc3339(
            at + jiff::SignedDuration::from_secs(1),
        ));
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        let Response::History { turns } = daemon.handle_request(UID, Request::History).await else {
            panic!("no history");
        };
        let last = turns.last().unwrap();
        assert_eq!(last.from, Speaker::Unprompted);
        // The prompt is fixed. The text of the wake is not in it.
        assert!(last.text.starts_with("ok: A wake that you booked"));
        assert!(!last.text.contains("check the deploy"));

        // The wake is done, it runs once, and the timer stops.
        let after = model.calls.load(Ordering::SeqCst);
        for seconds in [30, 90, 150] {
            clock.set(&crate::stamp::rfc3339(
                at + jiff::SignedDuration::from_secs(seconds),
            ));
            daemon.tick().await.unwrap();
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), after);
        assert_eq!(stores.pending_wakes().unwrap(), []);
        let booked = sqlite::state(&daemon.rt, daemon.assistant())
            .await
            .unwrap()
            .and_then(|state| state.next_heartbeat_ms);
        assert_eq!(booked, None);
    }

    #[cfg(any(feature = "discord", feature = "sandbox"))]
    #[cfg(feature = "followups")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_booked_wake_survives_a_restart_and_fires_on_time() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let model = Arc::new(MemoryModel::default());
        // The tool reads the wall clock, so the daemon clock starts there.
        let clock = HandClock::at(&crate::stamp::rfc3339(Timestamp::now()));
        // No nightly pass: on the wall clock, one can fall in this test.
        let mut quiet = config();
        quiet.memory.enabled = false;
        let open = |clock: &HandClock| {
            let hand = clock.clone();
            Daemon::open(
                &db,
                &quiet,
                Arc::clone(&model) as Arc<dyn AgentModel>,
                UID,
                false,
            )
            .unwrap()
            .with_clock(move || hand.now())
        };

        // Book a wake, then stop the daemon before it is due.
        {
            let mut daemon = open(&clock);
            daemon
                .handle_request(
                    UID,
                    request(
                        "r-1",
                        r#"use wake_me {"in_minutes": 30, "about": "check CI"}"#,
                    ),
                )
                .await;
            daemon.tick().await.unwrap();
        }

        // The next start arms the wake at the time it was booked for.
        let mut daemon = open(&clock);
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let booked = stores.pending_wakes().unwrap();
        assert_eq!(
            booked.len(),
            1,
            "the wake is still pending after the restart"
        );
        daemon.tick().await.unwrap();
        let (at, kind) = daemon.wake.as_ref().and_then(Wake::armed_at).unwrap();
        assert_eq!(kind, crate::wake::Kind::Followup);
        assert_eq!(at.as_millisecond(), booked[0].at_ms);

        // At its time, it fires once.
        let before = model.calls.load(Ordering::SeqCst);
        clock.set(&crate::stamp::rfc3339(
            at + jiff::SignedDuration::from_secs(1),
        ));
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        for seconds in [30, 90] {
            clock.set(&crate::stamp::rfc3339(
                at + jiff::SignedDuration::from_secs(seconds),
            ));
            daemon.tick().await.unwrap();
        }
        assert_eq!(stores.pending_wakes().unwrap(), []);

        // The tick log has it, and so does recent_wakes.
        #[cfg(feature = "self-debug")]
        {
            let ticks = stores.recent_ticks(5).unwrap();
            assert_eq!(ticks[0].kind, "follow-up");
            assert_eq!(ticks[0].due_ms, booked[0].at_ms);
            let outcome = &ticks[0].outcome;
            assert!(outcome.starts_with("ran; "), "the tick ran");
            assert!(
                outcome.ends_with("; ended 1 booked wake"),
                "it ended the booked wake"
            );
            let seen = daemon
                .handle_request(UID, request("r-2", "use recent_wakes {}"))
                .await;
            assert!(reply_text(&seen).contains(r#""kind":"follow-up""#));
        }
    }

    /// A model that moves the hand clock once, during the next call it
    /// gets, so a timer comes due in the middle of an op.
    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    #[derive(Debug)]
    struct LateClockModel {
        inner: MemoryModel,
        clock: HandClock,
        jump: std::sync::Mutex<Option<&'static str>>,
    }

    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    impl std::fmt::Debug for HandClock {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("HandClock")
        }
    }

    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    impl AgentModel for LateClockModel {
        fn chat<'a>(
            &'a self,
            request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            let jump = self.jump.lock().unwrap().take();
            if let Some(at) = jump {
                self.clock.set(at);
            }
            self.inner.chat(request)
        }
    }

    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_in_the_same_drive_never_answers_for_a_person() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let clock = HandClock::at("2026-11-14T12:00:00Z");
        let model = Arc::new(LateClockModel {
            inner: MemoryModel::default(),
            clock: clock.clone(),
            jump: std::sync::Mutex::new(None),
        });
        let hand = clock.clone();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &heartbeat_config(),
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.tick().await.unwrap();
        assert_eq!(armed(&daemon), Some(utc("2026-11-14T13:00:00Z")));
        // An idea waits, so the heartbeat runs a model call.
        daemon
            .handle_request(
                UID,
                request(
                    "t-1",
                    r#"use add_idea {"title": "Docs", "body": "Links rot."}"#,
                ),
            )
            .await;

        // The heartbeat comes due while the model answers the person.
        clock.set("2026-11-14T12:59:50Z");
        *model.jump.lock().unwrap() = Some("2026-11-14T13:00:05Z");
        let before = model.inner.calls.load(Ordering::SeqCst);
        let answer = daemon.handle_request(UID, request("t-2", "hi")).await;
        assert_eq!(
            reply_text(&answer),
            "ok: hi",
            "the person gets their own answer"
        );

        // The tick ran, in that drive or the next, and it speaks on its own.
        tick_until(&mut daemon, || {
            model.inner.calls.load(Ordering::SeqCst) >= before + 2
        })
        .await;
        let Response::History { turns } = daemon.handle_request(UID, Request::History).await else {
            panic!("no history");
        };
        let last = turns.last().unwrap();
        assert_eq!(last.from, Speaker::Unprompted);
        assert_eq!(last.text, "ok: check in");
        let hi = turns.iter().position(|turn| turn.text == "ok: hi").unwrap();
        assert_eq!(turns[hi].from, Speaker::Assistant);
    }

    #[cfg(all(
        feature = "heartbeat",
        feature = "notes",
        any(feature = "discord", feature = "sandbox")
    ))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_note_written_on_a_tick_is_kept() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-14T12:00:00Z");
        let hand = clock.clone();
        let mut config = heartbeat_config();
        if let Some(beat) = config.heartbeat.as_mut() {
            beat.prompt = String::from(
                r#"use write_note {"name": "heartbeat-design", "text": "Fixed on a tick."}"#,
            );
        }
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.tick().await.unwrap();
        daemon
            .handle_request(
                UID,
                request(
                    "p-1",
                    r#"use add_idea {"title": "Docs", "body": "Links rot."}"#,
                ),
            )
            .await;
        let before = model.calls.load(Ordering::SeqCst);
        clock.set("2026-11-14T13:00:01Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let note = stores.note("heartbeat-design").unwrap().unwrap();
        assert_eq!(note.text, "Fixed on a tick.");
    }

    #[cfg(all(
        feature = "heartbeat",
        feature = "self-debug",
        any(feature = "discord", feature = "sandbox")
    ))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tick_that_writes_a_file_is_refused_and_does_not_hear_success() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-14T12:00:00Z");
        let hand = clock.clone();
        let mut config = heartbeat_config();
        if let Some(beat) = config.heartbeat.as_mut() {
            beat.prompt = String::from("save notes/probe.md written on a tick");
        }
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.tick().await.unwrap();
        daemon
            .handle_request(
                UID,
                request(
                    "p-1",
                    r#"use add_idea {"title": "Docs", "body": "Links rot."}"#,
                ),
            )
            .await;
        let before = model.calls.load(Ordering::SeqCst);
        clock.set("2026-11-14T13:00:01Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        clock.set("2026-11-14T13:00:30Z");
        daemon.tick().await.unwrap();
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let outcome = stores.recent_ticks(1).unwrap()[0].outcome.clone();
        // The model hears that the write did not happen, not a byte count.
        // The policy refuses before the tool runs, so no call is traced.
        assert!(outcome.contains("may only read data"), "{outcome}");
        assert!(!outcome.contains("noted"), "{outcome}");
        let Response::Workspace { files } = daemon.handle_request(UID, Request::Workspace).await
        else {
            panic!("no workspace");
        };
        assert!(files.iter().all(|file| file.path != "notes/probe.md"));
    }

    #[cfg(all(
        feature = "heartbeat",
        feature = "self-debug",
        any(feature = "discord", feature = "sandbox")
    ))]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_tick_log_names_the_tools_of_the_tick_and_not_of_the_person() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-14T12:00:00Z");
        let hand = clock.clone();
        let mut config = heartbeat_config();
        if let Some(beat) = config.heartbeat.as_mut() {
            beat.prompt = String::from("use list_plans {}");
        }
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.tick().await.unwrap();
        // The person files an idea, so the heartbeat has work.
        daemon
            .handle_request(
                UID,
                request(
                    "p-1",
                    r#"use add_idea {"title": "Docs", "body": "Links rot."}"#,
                ),
            )
            .await;
        let before = model.calls.load(Ordering::SeqCst);
        clock.set("2026-11-14T13:00:01Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
        clock.set("2026-11-14T13:00:30Z");
        daemon.tick().await.unwrap();
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let outcome = stores.recent_ticks(1).unwrap()[0].outcome.clone();
        assert!(
            outcome.starts_with("ran; called list_plans; answered: log: "),
            "{outcome}"
        );
        assert!(
            !outcome.contains("add_idea"),
            "the person's call is not the tick's"
        );
    }

    #[cfg(all(feature = "heartbeat", any(feature = "discord", feature = "sandbox")))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_heartbeat_that_goes_away_stops_the_engine_timer() {
        let dir = tempfile::tempdir().unwrap();
        let clock = HandClock::at("2026-11-14T12:00:00Z");
        let hand = clock.clone();
        let config = heartbeat_config();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config,
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon.tick().await.unwrap();
        let booked = || async {
            sqlite::state(&daemon.rt, daemon.assistant())
                .await
                .unwrap()
                .and_then(|state| state.next_heartbeat_ms)
        };
        assert!(booked().await.is_some());

        let mut without = config;
        without.heartbeat = None;
        let merge = daemon.reload(without).unwrap();
        assert_eq!(merge.applied, ["heartbeat"]);
        daemon.tick().await.unwrap();
        let booked = sqlite::state(&daemon.rt, daemon.assistant())
            .await
            .unwrap()
            .and_then(|state| state.next_heartbeat_ms);
        assert_eq!(booked, None, "no plan, no tick");
    }

    #[cfg(any(feature = "discord", feature = "sandbox"))]
    /// Ticks until `done` holds, at most twenty times.
    async fn tick_until(daemon: &mut Daemon, done: impl Fn() -> bool) {
        for _ in 0..20 {
            daemon.tick().await.unwrap();
            if done() {
                return;
            }
        }
    }

    /// A clock that a test moves by hand.
    #[cfg(any(feature = "discord", feature = "sandbox"))]
    #[derive(Clone)]
    struct HandClock(Arc<std::sync::atomic::AtomicI64>);

    #[cfg(any(feature = "discord", feature = "sandbox"))]
    impl HandClock {
        fn at(text: &str) -> Self {
            let clock = Self(Arc::new(std::sync::atomic::AtomicI64::new(0)));
            clock.set(text);
            clock
        }

        fn set(&self, text: &str) {
            let millis = text.parse::<Timestamp>().unwrap().as_millisecond();
            self.0.store(millis, std::sync::atomic::Ordering::SeqCst);
        }

        fn now(&self) -> Timestamp {
            let millis = self.0.load(std::sync::atomic::Ordering::SeqCst);
            crate::stamp::from_millis(millis).unwrap()
        }
    }

    #[cfg(feature = "discord")]
    /// A daemon with a brief, a hand clock and a fake Discord.
    fn brief_daemon(
        db: &Path,
        config: &Config,
        model: &Arc<MemoryModel>,
        clock: &HandClock,
        fake: &littleguy_channels::testing::FakeDiscord,
    ) -> Daemon {
        let hand = clock.clone();
        let daemon = Daemon::open(
            db,
            config,
            Arc::clone(model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_clock(move || hand.now());
        daemon
            .chats
            .set_discord(DiscordRest::new(fake.api_base.clone(), "token"));
        daemon
    }

    #[cfg(feature = "discord")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_brief_reaches_the_owner_once_a_day() {
        use littleguy_channels::testing::FakeDiscord;
        use std::sync::atomic::Ordering;

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let config = brief_config("Morning.", Some("discord:555"));
        let mut daemon = brief_daemon(&dir.path().join("lg.db"), &config, &model, &clock, &fake);

        let armed = daemon.arm_wake().await.unwrap();
        assert_eq!(armed.as_deref(), Some("brief:2026-11-09T08:00:00+00:00"));
        // Before the slot, ticks send nothing and arm nothing new.
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(daemon.arm_wake().await.unwrap(), None);
        assert_eq!(fake.sent().len(), 0);
        assert_eq!(model.calls.load(Ordering::SeqCst), 0);

        clock.set("2026-11-09T08:00:01Z");
        tick_until(&mut daemon, || fake.sent().len() == 1).await;
        let sent = fake.sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].channel_id, "dm555");
        assert_eq!(sent[0].content, "ok: Morning.");
        let next = daemon
            .wake
            .as_ref()
            .and_then(Wake::armed_at)
            .map(|(at, _)| at);
        assert_eq!(
            next.map(crate::stamp::rfc3339).as_deref(),
            Some("2026-11-10T08:00:00+00:00")
        );

        // More ticks in the same slot run nothing new.
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(fake.sent().len(), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);

        // A chat answer goes to its channel, not to the brief.
        let answer = daemon.handle_request(UID, request("m-1", "hello")).await;
        assert_eq!(reply_text(&answer), "ok: hello");
        assert_eq!(fake.sent().len(), 1);

        clock.set("2026-11-10T08:00:01Z");
        tick_until(&mut daemon, || fake.sent().len() == 2).await;
        assert_eq!(fake.sent().len(), 2);
        assert_eq!(model.calls.load(Ordering::SeqCst), 3);
    }

    #[cfg(all(feature = "discord", feature = "slack"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_brief_to_a_slack_user_goes_to_slack() {
        use littleguy_channels::testing::FakeDiscord;
        use littleguy_channels::testing::slack::{APP_TOKEN, BOT_TOKEN, FakeSlack};

        let discord = FakeDiscord::start().await.unwrap();
        let slack = FakeSlack::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let config = brief_config("Morning <team>.", Some("slack:U555"));
        let mut daemon = brief_daemon(&dir.path().join("lg.db"), &config, &model, &clock, &discord);
        daemon.tick().await.unwrap();
        clock.set("2026-11-09T08:00:01Z");
        // Slack is off at first, so the brief logs and stays unsent.
        tick_until(&mut daemon, || {
            model.calls.load(std::sync::atomic::Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(
            slack.posted(),
            [] as [littleguy_channels::testing::slack::Posted; 0]
        );
        daemon
            .chats
            .set_slack(SlackApi::new(slack.api_base.clone(), BOT_TOKEN, APP_TOKEN));
        clock.set("2026-11-10T08:00:01Z");
        tick_until(&mut daemon, || slack.posted().len() == 1).await;
        let posted = slack.posted();
        assert_eq!(posted[0].channel, "DU555");
        assert_eq!(posted[0].text, "ok: Morning &lt;team&gt;.");
        assert_eq!(discord.sent(), [] as [littleguy_channels::testing::Sent; 0]);
    }

    #[cfg(feature = "discord")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_quiet_brief_sends_nothing() {
        use littleguy_channels::testing::FakeDiscord;
        use std::sync::atomic::Ordering;

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let mut config = brief_config("quiet", Some("discord:555"));
        // The brief opens its store even with memory off.
        config.memory.enabled = false;
        let mut daemon = brief_daemon(&dir.path().join("lg.db"), &config, &model, &clock, &fake);
        daemon.tick().await.unwrap();
        clock.set("2026-11-09T08:00:01Z");
        tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) == 1).await;
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
        assert_eq!(fake.sent().len(), 0);
    }

    #[cfg(feature = "discord")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_restart_arms_the_same_slot_once() {
        use littleguy_channels::testing::FakeDiscord;
        use std::sync::atomic::Ordering;

        let fake = FakeDiscord::start().await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let model = Arc::new(MemoryModel::default());
        let clock = HandClock::at("2026-11-09T07:00:00Z");
        let config = brief_config("Morning.", Some("discord:555"));
        {
            let mut daemon = brief_daemon(&db, &config, &model, &clock, &fake);
            daemon.tick().await.unwrap();
        }
        clock.set("2026-11-09T07:30:00Z");
        let mut daemon = brief_daemon(&db, &config, &model, &clock, &fake);
        // The new daemon arms the same slot. The engine takes it once.
        assert!(daemon.arm_wake().await.unwrap().is_some());
        clock.set("2026-11-09T08:00:01Z");
        tick_until(&mut daemon, || fake.sent().len() == 1).await;
        for _ in 0..3 {
            daemon.tick().await.unwrap();
        }
        assert_eq!(fake.sent().len(), 1);
        assert_eq!(model.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_bad_brief_schedule_is_refused_at_open() {
        let dir = tempfile::tempdir().unwrap();
        let mut bad = brief_config("Morning.", None);
        if let Some(brief) = bad.brief.as_mut() {
            brief.timezone = String::from("Mars/Olympus");
        }
        let Err(error) = Daemon::open(
            &dir.path().join("lg.db"),
            &bad,
            Arc::new(OfflineModel::new()),
            UID,
            false,
        ) else {
            panic!("a bad brief schedule opened");
        };
        assert!(format!("{error:#}").contains("brief.schedule"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_workspace_file_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let open =
            || Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        {
            let mut daemon = open();
            let saved = daemon
                .handle_request(UID, request("w-1", "save notes/tea.md green tea"))
                .await;
            assert_eq!(reply_text(&saved), "noted");
        }
        assert!(crate::tools::workspace_for(&db).is_dir());
        let mut daemon = open();
        let shown = daemon
            .handle_request(UID, request("w-2", "show notes/tea.md"))
            .await;
        assert_eq!(reply_text(&shown), "green tea");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_long_tool_result_reaches_the_model_whole() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        // Longer than the engine's own cut of a tool result.
        let long = "tea.".repeat(autumn_harvest_agent::harness::DEFAULT_TOOL_OUTPUT_LIMIT);
        daemon
            .handle_request(UID, request("l-1", &format!("save notes/long.md {long}")))
            .await;
        let shown = daemon
            .handle_request(UID, request("l-2", "show notes/long.md"))
            .await;
        assert_eq!(reply_text(&shown).len(), long.len(), "the read is whole");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_owner_reads_the_workspace_and_no_one_else() {
        use crate::protocol::WorkspaceEntry;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        assert_eq!(
            daemon.handle_request(UID, Request::Workspace).await,
            Response::Workspace { files: Vec::new() }
        );
        daemon
            .handle_request(UID, request("w-1", "save notes/tea.md green tea"))
            .await;
        assert_eq!(
            daemon.handle_request(UID, Request::Workspace).await,
            Response::Workspace {
                files: vec![WorkspaceEntry {
                    path: String::from("notes/tea.md"),
                    bytes: 9,
                }]
            }
        );
        let file = |path: &str| Request::WorkspaceFile {
            path: path.to_owned(),
        };
        assert_eq!(
            daemon.handle_request(UID, file("notes/tea.md")).await,
            Response::WorkspaceFile {
                path: String::from("notes/tea.md"),
                bytes: 9,
                text: Some(String::from("green tea")),
            }
        );
        let missing = daemon.handle_request(UID, file("notes/none.md")).await;
        assert!(matches!(
            missing,
            Response::Error { message } if message == "the workspace has no file notes/none.md"
        ));
        for request in [Request::Workspace, file("notes/tea.md")] {
            assert!(matches!(
                daemon.handle_request(UID + 1, request).await,
                Response::Error { message } if message == "only the owner of the daemon may do that"
            ));
        }
    }

    /// The usage of a reply. The failure does not print the response: some
    /// responses carry a secret, such as the token of a link.
    fn usage_of(response: &Response) -> crate::protocol::Usage {
        match response {
            Response::Reply {
                usage: Some(usage), ..
            } => *usage,
            _ => panic!("the response is not a reply with usage"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reply_carries_the_usage_of_its_own_op() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        let first = daemon.handle_request(UID, request("u-1", "hello")).await;
        assert_eq!(
            usage_of(&first),
            crate::protocol::Usage {
                calls: 1,
                input_tokens: 1,
                output_tokens: 1,
                cost_usd_micros: 0,
                unpriced_calls: 1,
            }
        );
        // A tool call takes two model calls. The usage counts this op only,
        // not the op before it.
        let second = daemon
            .handle_request(UID, request("u-2", "remember tea"))
            .await;
        assert_eq!(usage_of(&second).calls, 2);
        assert_eq!(usage_of(&second).input_tokens, 2);
        // A duplicate ran nothing, so it carries no usage.
        let again = daemon.handle_request(UID, request("u-1", "hello")).await;
        assert!(
            matches!(again, Response::Duplicate { .. }),
            "a repeated id gets a duplicate"
        );
    }

    /// A model with a price: 3 micro dollars per input token and 15 per
    /// output token. Each call reads 100 tokens and writes 20.
    #[derive(Debug)]
    struct PricedModel;

    impl AgentModel for PricedModel {
        fn chat<'a>(
            &'a self,
            _request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            Box::pin(async move {
                Ok(autumn_harvest_agent::ChatResponse {
                    content: vec![autumn_harvest_agent::ContentPart::Text(String::from("ok"))],
                    stop_reason: autumn_harvest_agent::StopReason::EndTurn,
                    usage: autumn_harvest_agent::TokenUsage::new(100, 20),
                })
            })
        }

        fn model_id(&self) -> Option<&str> {
            Some("priced-test")
        }

        fn cost_usd_micros(&self, usage: &autumn_harvest_agent::TokenUsage) -> Option<u64> {
            Some(u64::from(usage.input_tokens) * 3 + u64::from(usage.output_tokens) * 15)
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reply_carries_the_cost_of_a_priced_model() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon = Daemon::open(&db, &config(), Arc::new(PricedModel), UID, false).unwrap();
        let reply = daemon.handle_request(UID, request("p-1", "hello")).await;
        assert_eq!(
            usage_of(&reply),
            crate::protocol::Usage {
                calls: 1,
                input_tokens: 100,
                output_tokens: 20,
                cost_usd_micros: 100 * 3 + 20 * 15,
                unpriced_calls: 0,
            }
        );
    }

    #[test]
    fn a_page_shows_only_text_that_is_not_too_large() {
        assert_eq!(shown_text(b"tea").as_deref(), Some("tea"));
        assert_eq!(shown_text(&[0xff, 0xfe]), None);
        assert_eq!(shown_text(&vec![b'a'; MAX_SHOWN_BYTES + 1]), None);
        assert!(shown_text(&vec![b'a'; MAX_SHOWN_BYTES]).is_some());
    }

    #[cfg(feature = "self-debug")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_assistant_reads_the_config_it_runs_with() {
        let dir = tempfile::tempdir().unwrap();
        let mut beating = config();
        beating.heartbeat = Some(littleguy_core::HeartbeatConfig {
            every_minutes: 45,
            quiet: None,
            timezone: String::from("UTC"),
            research: true,
            deliver: littleguy_core::HeartbeatDelivery::Web,
            prompt: String::from("check in"),
        });
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &beating,
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap();
        let seen = daemon
            .handle_request(UID, request("k-1", "use config_summary {}"))
            .await;
        let text = reply_text(&seen);
        assert!(
            text.contains(r#""every_minutes":45"#),
            "the heartbeat is on"
        );
        assert!(text.contains(r#""refine":null"#), "refine is absent");
    }

    /// The `config_summary` value that the assistant reads now.
    #[cfg(feature = "self-debug")]
    async fn summary_seen(daemon: &mut Daemon, key: &str) -> serde_json::Value {
        let seen = daemon
            .handle_request(UID, request(key, "use config_summary {}"))
            .await;
        let text = reply_text(&seen);
        let json = text.strip_prefix("log: ").unwrap_or(text);
        serde_json::from_str(json).unwrap()
    }

    #[cfg(feature = "self-debug")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_reload_keeps_the_boot_and_the_summary_names_the_build() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = config();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config,
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap();
        let before = summary_seen(&mut daemon, "b-1").await;
        let boot = crate::config_view::Boot::this_process();
        assert_eq!(before["boot"]["id"], boot.id.as_str());
        assert_eq!(before["build"]["version"], crate::config_view::VERSION);
        // A live change rebuilds the tool. It must not stamp a new boot.
        std::thread::sleep(Duration::from_millis(5));
        config.max_steps = Some(7);
        let merge = daemon.reload(config).unwrap();
        assert!(!merge.is_empty(), "the reload rebuilt the tools");
        let after = summary_seen(&mut daemon, "b-2").await;
        assert_eq!(after["agent"]["max_steps"], 7, "the reload applied");
        assert_eq!(after["boot"], before["boot"], "a reload is not a restart");
    }

    #[cfg(feature = "self-debug")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_assistant_reads_its_own_runs_and_usage_but_no_message() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        daemon
            .handle_request(UID, request("r-1", "my secret is plum"))
            .await;
        let runs = daemon
            .handle_request(UID, request("r-2", "use recent_runs {\"limit\": 5}"))
            .await;
        let runs: serde_json::Value =
            serde_json::from_str(reply_text(&runs).strip_prefix("log: ").unwrap()).unwrap();
        let first = &runs["runs"][0];
        assert!(first["workflow"].is_string(), "{runs}");
        assert!(first["state"].is_string(), "{runs}");
        assert!(first["started"].is_string(), "{runs}");
        assert!(
            !runs.to_string().contains("plum"),
            "a run shows no message: {runs}"
        );

        let usage = daemon
            .handle_request(UID, request("r-3", "use model_usage {\"hours\": 1}"))
            .await;
        let usage: serde_json::Value =
            serde_json::from_str(reply_text(&usage).strip_prefix("log: ").unwrap()).unwrap();
        let calls: i64 = usage["models"]
            .as_array()
            .unwrap()
            .iter()
            .map(|model| model["calls"].as_i64().unwrap())
            .sum();
        assert!(
            calls >= 4,
            "each model turn so far is in the ledger: {usage}"
        );

        let failed = daemon
            .handle_request(UID, request("r-4", "use failed_attempts {}"))
            .await;
        assert_eq!(reply_text(&failed), "log: {\"failures\":[]}");
    }

    #[cfg(not(feature = "plans"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_build_without_plans_says_so_and_has_no_goal_tools() {
        use crate::protocol::PlanDecision;
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &config(),
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap();
        let Response::Off { message } = daemon.handle_request(UID, Request::Plans).await else {
            panic!("the listing is not off");
        };
        assert!(message.contains("`plans` feature"));
        let decide = Request::Decide {
            decision: PlanDecision::Accept,
            id: 1,
        };
        assert!(matches!(
            daemon.handle_request(UID, decide).await,
            Response::Error { .. }
        ));
        let tried = daemon
            .handle_request(
                UID,
                request("p-1", r#"use add_idea {"title": "T", "body": "B"}"#),
            )
            .await;
        assert!(!reply_text(&tried).starts_with("ok"));
    }

    #[cfg(feature = "plans")]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_idea_becomes_a_goal_only_when_the_owner_accepts() {
        use crate::protocol::PlanDecision;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        let said = |response: &Response| reply_text(response).to_owned();
        let filed = daemon
            .handle_request(
                UID,
                request(
                    "p-1",
                    r#"use add_idea {"title": "Docs links", "body": "They rot."}"#,
                ),
            )
            .await;
        assert_eq!(said(&filed), r#"log: {"idea":1}"#);
        daemon
            .handle_request(
                UID,
                request(
                    "p-2",
                    r#"use research_idea {"idea": 1, "note": "3 are broken."}"#,
                ),
            )
            .await;
        let proposed = daemon
            .handle_request(
                UID,
                request(
                    "p-3",
                    r#"use propose_goal {"title": "Fix links", "why": "Dead ends.", "check_every_minutes": 60, "from_idea": 1}"#,
                ),
            )
            .await;
        assert_eq!(said(&proposed), r#"log: {"goal":1,"status":"proposed"}"#);

        let Response::Plans { goals, ideas, .. } = daemon.handle_request(UID, Request::Plans).await
        else {
            panic!("no plans");
        };
        assert_eq!(
            (goals[0].status.as_str(), goals[0].idea),
            ("proposed", Some(1))
        );
        assert_eq!(ideas[0].notes[0].text, "3 are broken.");

        let accept = Request::Decide {
            decision: PlanDecision::Accept,
            id: 1,
        };
        for request in [Request::Plans, accept.clone()] {
            assert!(matches!(
                daemon.handle_request(UID + 1, request).await,
                Response::Error { message } if message == "only the owner of the daemon may do that"
            ));
        }
        let Response::Plans { goals, ideas, .. } = daemon.handle_request(UID, accept.clone()).await
        else {
            panic!("the accept failed");
        };
        assert_eq!(goals[0].status, "active");
        assert_eq!(goals[0].check_every_minutes, Some(60));
        assert_eq!(ideas, [], "the idea is promoted");
        assert!(matches!(
            daemon.handle_request(UID, accept).await,
            Response::Error { message } if message == "1 is not a proposed goal"
        ));

        daemon
            .handle_request(
                UID,
                request(
                    "p-4",
                    r#"use goal_progress {"goal": 1, "note": "Fixed.", "finished": true}"#,
                ),
            )
            .await;
        assert_eq!(
            daemon.handle_request(UID, Request::Plans).await,
            Response::Plans {
                goals: Vec::new(),
                ideas: Vec::new(),
                answered: Vec::new(),
            }
        );
    }

    #[cfg(feature = "plans")]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_answered_idea_leaves_the_open_list_until_the_owner_reopens_it() {
        use crate::protocol::PlanDecision;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon =
            Daemon::open(&db, &config(), Arc::new(MemoryModel::default()), UID, false).unwrap();
        for (id, text) in [
            (
                "p-1",
                r#"use add_idea {"title": "Docs links", "body": "Do they rot?"}"#,
            ),
            (
                "p-2",
                r#"use add_idea {"title": "Other", "body": "Still open."}"#,
            ),
        ] {
            daemon.handle_request(UID, request(id, text)).await;
        }
        let concluded = daemon
            .handle_request(
                UID,
                request(
                    "p-3",
                    r#"use conclude_idea {"idea": 1, "note": "No: all links resolve."}"#,
                ),
            )
            .await;
        assert_eq!(
            reply_text(&concluded),
            r#"log: {"idea":1,"status":"answered"}"#
        );

        // The assistant's listing names it, and carries no body or notes.
        let listed = daemon
            .handle_request(UID, request("p-4", "use list_plans {}"))
            .await;
        let listing: serde_json::Value =
            serde_json::from_str(reply_text(&listed).trim_start_matches("log: ")).unwrap();
        assert_eq!(listing["ideas"].as_array().unwrap().len(), 1);
        assert_eq!(listing["ideas"][0]["id"], 2);
        assert_eq!(
            listing["answered_ideas"],
            serde_json::json!([{"id": 1, "title": "Docs links"}])
        );

        // The owner sees it, with its closing note.
        let Response::Plans {
            ideas, answered, ..
        } = daemon.handle_request(UID, Request::Plans).await
        else {
            panic!("no plans");
        };
        assert_eq!(ideas.iter().map(|idea| idea.id).collect::<Vec<_>>(), [2]);
        assert_eq!(answered[0].id, 1);
        assert_eq!(answered[0].notes[0].text, "No: all links resolve.");

        let reopen = |id| Request::Decide {
            decision: PlanDecision::ReopenIdea,
            id,
        };
        assert!(matches!(
            daemon.handle_request(UID + 1, reopen(1)).await,
            Response::Error { message } if message == "only the owner of the daemon may do that"
        ));
        assert!(matches!(
            daemon.handle_request(UID, reopen(2)).await,
            Response::Error { message } if message == "2 is not an answered idea"
        ));
        let Response::Plans {
            ideas, answered, ..
        } = daemon.handle_request(UID, reopen(1)).await
        else {
            panic!("the reopen failed");
        };
        assert_eq!(ideas.iter().map(|idea| idea.id).collect::<Vec<_>>(), [1, 2]);
        assert_eq!(answered, []);
    }

    /// A model whose council members vote `accept`, and that answers the
    /// assistant as [`MemoryModel`] does.
    #[cfg(feature = "council")]
    #[derive(Debug, Default)]
    struct CouncilModel {
        inner: MemoryModel,
        accept: std::sync::atomic::AtomicBool,
        votes: std::sync::atomic::AtomicU64,
    }

    #[cfg(feature = "council")]
    impl AgentModel for CouncilModel {
        fn chat<'a>(
            &'a self,
            request: &'a autumn_harvest_agent::ChatRequest,
        ) -> autumn_harvest_agent::BoxFuture<
            'a,
            Result<autumn_harvest_agent::ChatResponse, autumn_harvest_agent::AgentError>,
        > {
            use std::sync::atomic::Ordering;
            let council = request
                .messages
                .first()
                .is_some_and(|first| MemoryModel::text(first).starts_with("You sit on a council"));
            if !council {
                return self.inner.chat(request);
            }
            assert!(request.tools.is_empty(), "a member has no tools");
            self.votes.fetch_add(1, Ordering::SeqCst);
            let accept = self.accept.load(Ordering::SeqCst);
            Box::pin(async move {
                Ok(autumn_harvest_agent::ChatResponse {
                    content: vec![autumn_harvest_agent::ContentPart::Text(format!(
                        r#"{{"accept": {accept}, "reason": "It fits the charter."}}"#
                    ))],
                    stop_reason: autumn_harvest_agent::StopReason::EndTurn,
                    usage: autumn_harvest_agent::TokenUsage::new(1, 1),
                })
            })
        }
    }

    /// A daemon with `council` in `config`, on a model whose members
    /// accept, with its skills folder in `dir`.
    #[cfg(feature = "council")]
    fn council_daemon(
        dir: &Path,
        mut config: Config,
        council: littleguy_core::CouncilConfig,
    ) -> (Daemon, Arc<CouncilModel>) {
        config.council = Some(council);
        let model = Arc::new(CouncilModel::default());
        model
            .accept
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let daemon = Daemon::open(
            &dir.join("lg.db"),
            &config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_skills_dir(dir.join("skills"));
        (daemon, model)
    }

    #[cfg(feature = "council")]
    #[tokio::test(flavor = "multi_thread")]
    async fn the_council_decides_clean_proposals_and_leaves_tainted_ones() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let shelf = dir.path().join("skills");
        let mut with_refine = config();
        with_refine.refine = Some(littleguy_core::RefineConfig::default());
        let council = littleguy_core::CouncilConfig {
            accept: vec![
                littleguy_core::CouncilAccepts::Goals,
                littleguy_core::CouncilAccepts::Skills,
            ],
            members: 3,
            charter: Some(String::from("Zara, a rogue who wants the crown.")),
        };
        let (mut daemon, model) = council_daemon(dir.path(), with_refine, council);
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());

        // A clean proposal: no outside text in the run.
        daemon
            .handle_request(
                UID,
                request(
                    "c-1",
                    r#"use propose_goal {"title": "Steal the seal", "why": "The crown needs it."}"#,
                ),
            )
            .await;
        daemon.tick().await.unwrap();
        assert_eq!(model.votes.load(Ordering::SeqCst), 3, "three members voted");
        let plans = stores.plans().unwrap();
        assert_eq!(plans.goals[0].status.as_str(), "active");
        assert!(
            plans.goals[0].notes[0]
                .text
                .starts_with("Accepted by the council")
        );

        // A clean skill draft is accepted, written by the council.
        daemon
            .handle_request(
                UID,
                request(
                    "c-2",
                    r#"use propose_skill {"name": "pick-a-lock", "description": "Open a lock.", "body": "1. Listen."}"#,
                ),
            )
            .await;
        daemon.tick().await.unwrap();
        let written = std::fs::read_to_string(shelf.join("pick-a-lock/SKILL.md")).unwrap();
        assert!(written.contains("author: council"));

        // A declined proposal closes.
        model.accept.store(false, Ordering::SeqCst);
        daemon
            .handle_request(
                UID,
                request(
                    "c-3",
                    r#"use propose_goal {"title": "Burn the inn", "why": "Fun."}"#,
                ),
            )
            .await;
        daemon.tick().await.unwrap();
        let declined = stores.plans().unwrap();
        assert!(
            declined
                .goals
                .iter()
                .all(|goal| goal.title != "Burn the inn")
        );

        // list_plans carries untrusted input, so the run is tainted now, and
        // its next proposal waits for the owner.
        model.accept.store(true, Ordering::SeqCst);
        daemon
            .handle_request(UID, request("c-4", "use list_plans {}"))
            .await;
        daemon
            .handle_request(
                UID,
                request(
                    "c-5",
                    r#"use propose_goal {"title": "Read the letter", "why": "A page said so."}"#,
                ),
            )
            .await;
        let before = model.votes.load(Ordering::SeqCst);
        daemon.tick().await.unwrap();
        assert_eq!(
            model.votes.load(Ordering::SeqCst),
            before,
            "no vote on a tainted one"
        );
        let waiting = stores.plans().unwrap();
        let letter = waiting
            .goals
            .iter()
            .find(|goal| goal.title == "Read the letter")
            .unwrap();
        assert_eq!(letter.status.as_str(), "proposed");
        assert_eq!(stores.council_log(10).unwrap().len(), 3);
    }

    #[cfg(feature = "council")]
    #[tokio::test(flavor = "multi_thread")]
    async fn without_the_rule_of_two_the_council_decides_nothing() {
        use std::sync::atomic::Ordering;
        let dir = tempfile::tempdir().unwrap();
        let mut blind = config();
        blind.flow.rule_of_two = false;
        let council = littleguy_core::CouncilConfig {
            accept: vec![littleguy_core::CouncilAccepts::Goals],
            members: 1,
            charter: None,
        };
        let (mut daemon, model) = council_daemon(dir.path(), blind, council);
        daemon
            .handle_request(
                UID,
                request("b-1", r#"use propose_goal {"title": "T", "why": "W"}"#),
            )
            .await;
        daemon.tick().await.unwrap();
        assert_eq!(
            model.votes.load(Ordering::SeqCst),
            0,
            "no taint is seen, so none is trusted"
        );
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        assert_eq!(stores.plans().unwrap().goals[0].status.as_str(), "proposed");
    }

    #[cfg(feature = "skills")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_draft_close_to_a_skill_is_refused_unless_littleguy_says_it_differs() {
        let dir = tempfile::tempdir().unwrap();
        let shelf = dir.path().join("skills");
        let mut with_refine = config();
        with_refine.refine = Some(littleguy_core::RefineConfig::default());
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &with_refine,
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap()
        .with_skills_dir(shelf.clone());
        crate::skills::write(
            &shelf,
            "read-a-page",
            "Read one web page.",
            "1. Fetch.",
            "mark",
        )
        .unwrap();
        daemon.tick().await.unwrap();

        // list_plans shows the skill, so a draft need not repeat it.
        let listed = daemon
            .handle_request(UID, request("d-1", "use list_plans {}"))
            .await;
        assert!(reply_text(&listed).contains(r#""name":"read-a-page""#));

        let draft = |id: &'static str, extra: &str| {
            request(
                id,
                &format!(
                    r#"use propose_skill {{"name": "read-a-web-page", "description": "Read a page.", "body": "1. Fetch."{extra}}}"#
                ),
            )
        };
        let refused = daemon.handle_request(UID, draft("d-2", "")).await;
        assert!(reply_text(&refused).contains("close to `read-a-page`"));
        let drafts = |response: Response| match response {
            Response::Skills { drafts, .. } => drafts.len(),
            _ => usize::MAX,
        };
        assert_eq!(
            drafts(daemon.handle_request(UID, Request::Skills).await),
            0,
            "a refused draft is not stored"
        );

        // littleguy checked, and its skill is a different one.
        let kept = daemon
            .handle_request(
                UID,
                draft("d-3", r#", "not_a_duplicate_of": ["read-a-page"]"#),
            )
            .await;
        assert!(reply_text(&kept).starts_with("log: "));
        assert_eq!(drafts(daemon.handle_request(UID, Request::Skills).await), 1);
    }

    #[cfg(feature = "skills")]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_waiting_draft_reads_in_full_by_id_and_lists_without_its_body() {
        let dir = tempfile::tempdir().unwrap();
        let mut with_refine = config();
        with_refine.refine = Some(littleguy_core::RefineConfig::default());
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &with_refine,
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap()
        .with_skills_dir(dir.path().join("skills"));
        let drafted = daemon
            .handle_request(
                UID,
                request(
                    "r-1",
                    r#"use propose_skill {"name": "weekly-digest", "description": "Write the Friday digest.", "body": "1. Read the week.\n2. Sum it up."}"#,
                ),
            )
            .await;
        let drafted: serde_json::Value =
            serde_json::from_str(reply_text(&drafted).strip_prefix("log: ").unwrap()).unwrap();
        let id = drafted["skill_draft"].as_i64().unwrap();

        // The listing renders, holds the draft, and leaves its body out.
        let listed = daemon
            .handle_request(UID, request("r-2", "use list_plans {}"))
            .await;
        let listed: serde_json::Value =
            serde_json::from_str(reply_text(&listed).strip_prefix("log: ").unwrap()).unwrap();
        let drafts = listed["skill_drafts"].as_array().unwrap();
        assert_eq!(drafts.len(), 1, "the seeded draft is listed");
        assert_eq!(drafts[0]["id"], id);
        assert!(drafts[0].get("body").is_none(), "the listing has no body");

        // read_skill_draft returns the whole draft.
        let read = daemon
            .handle_request(
                UID,
                request("r-3", &format!(r#"use read_skill_draft {{"id": {id}}}"#)),
            )
            .await;
        let read: serde_json::Value =
            serde_json::from_str(reply_text(&read).strip_prefix("log: ").unwrap()).unwrap();
        assert_eq!(read["skill_draft"], id);
        assert_eq!(read["name"], "weekly-digest");
        assert_eq!(read["description"], "Write the Friday digest.");
        assert_eq!(read["body"], "1. Read the week.\n2. Sum it up.");

        // An id that does not wait is refused, and the reply says where to look.
        let missing = daemon
            .handle_request(
                UID,
                request(
                    "r-4",
                    &format!(r#"use read_skill_draft {{"id": {}}}"#, id + 1),
                ),
            )
            .await;
        assert!(reply_text(&missing).contains("list_plans lists the waiting drafts"));

        // A declined draft no longer reads.
        assert!(matches!(
            daemon
                .handle_request(UID, Request::DeclineSkill { draft: id })
                .await,
            Response::Skills { .. }
        ));
        let declined = daemon
            .handle_request(
                UID,
                request("r-5", &format!(r#"use read_skill_draft {{"id": {id}}}"#)),
            )
            .await;
        assert!(reply_text(&declined).contains(&format!("no skill draft {id} waits")));
    }

    #[cfg(feature = "skills")]
    #[tokio::test(flavor = "multi_thread")]
    async fn skills_from_the_folder_and_from_littleguy_reach_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let shelf = dir.path().join("skills");
        let mut with_refine = config();
        with_refine.refine = Some(littleguy_core::RefineConfig::default());
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &with_refine,
            Arc::new(MemoryModel::default()),
            UID,
            false,
        )
        .unwrap()
        .with_skills_dir(shelf.clone());

        // The owner writes a SKILL.md by hand. The next tick puts it in the
        // prompt.
        crate::skills::write(&shelf, "deploy", "Ship the site.", "1. Build.", "mark").unwrap();
        daemon.tick().await.unwrap();
        let recall = daemon.handle_request(UID, request("s-1", "recall")).await;
        assert!(reply_text(&recall).contains("`deploy`: Ship the site."));

        // littleguy drafts a skill. It waits; it is not in the prompt.
        let drafted = daemon
            .handle_request(
                UID,
                request(
                    "s-2",
                    r#"use propose_skill {"name": "weekly-digest", "description": "Write the Friday digest.", "body": "1. Read the week."}"#,
                ),
            )
            .await;
        assert!(reply_text(&drafted).starts_with("log: "));
        let Response::Skills { drafts, .. } = daemon.handle_request(UID, Request::Skills).await
        else {
            panic!("no skills");
        };
        assert_eq!(drafts.len(), 1);
        assert!(!drafts[0].revises);
        // Only the owner decides.
        assert!(matches!(
            daemon
                .handle_request(UID + 1, Request::AcceptSkill { draft: drafts[0].id })
                .await,
            Response::Error { message } if message == "only the owner of the daemon may do that"
        ));

        // The owner accepts: a SKILL.md by littleguy, live at once.
        let Response::Skills { skills, drafts, .. } = daemon
            .handle_request(
                UID,
                Request::AcceptSkill {
                    draft: drafts[0].id,
                },
            )
            .await
        else {
            panic!("the accept failed");
        };
        assert_eq!(drafts, []);
        let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
        assert_eq!(names, ["deploy", "weekly-digest"]);
        let file = std::fs::read_to_string(shelf.join("weekly-digest").join("SKILL.md")).unwrap();
        assert!(file.contains("author: littleguy"));
        let recall = daemon.handle_request(UID, request("s-3", "recall")).await;
        assert!(reply_text(&recall).contains("`weekly-digest`: Write the Friday digest."));

        // Retiring keeps the file and takes the skill out of the prompt.
        daemon
            .handle_request(
                UID,
                Request::RetireSkill {
                    name: String::from("deploy"),
                },
            )
            .await;
        assert!(shelf.join(crate::skills::RETIRED).is_dir());
        let recall = daemon.handle_request(UID, request("s-4", "recall")).await;
        assert!(!reply_text(&recall).contains("`deploy`"));
        assert!(matches!(
            daemon
                .handle_request(UID, Request::DeclineSkill { draft: 1 })
                .await,
            Response::Error { .. }
        ));
    }

    #[cfg(not(feature = "skills"))]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_build_without_skills_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()));
        assert!(matches!(
            daemon.handle_request(UID, Request::Skills).await,
            Response::Off { message } if message.contains("`skills` feature")
        ));
        assert!(matches!(
            daemon
                .handle_request(UID, Request::RetireSkill { name: String::from("x") })
                .await,
            Response::Error { message } if message.contains("`skills` feature")
        ));
    }

    #[cfg(feature = "skills")]
    #[tokio::test(flavor = "multi_thread")]
    async fn skills_are_off_without_the_refine_loop() {
        let dir = tempfile::tempdir().unwrap();
        let mut daemon = open(&dir.path().join("lg.db"), Arc::new(OfflineModel::new()))
            .with_skills_dir(dir.path().join("skills"));
        assert!(matches!(
            daemon.handle_request(UID, Request::Skills).await,
            Response::Off { message } if message.contains("refine")
        ));
        assert!(matches!(
            daemon
                .handle_request(UID, Request::AcceptSkill { draft: 1 })
                .await,
            Response::Error { message } if message.contains("refine")
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_memory_blocks_take_their_size_from_the_config_and_a_reload() {
        use autumn_harvest_agent::memory::{MemoryScope, MemoryStore};
        /// The size of each block of `scope`, in order.
        async fn sizes(stores: &SqliteStores, scope: &MemoryScope) -> Vec<usize> {
            MemoryStore::load(stores, scope)
                .await
                .unwrap()
                .into_iter()
                .map(|block| block.limit_chars)
                .collect()
        }
        let dir = tempfile::tempdir().unwrap();
        let mut sized = config();
        sized.memory.memory_chars = 9_000;
        sized.memory.user_chars = 3_000;
        let mut daemon = Daemon::open(
            &dir.path().join("lg.db"),
            &sized,
            Arc::new(OfflineModel::new()),
            UID,
            false,
        )
        .unwrap();
        let stores = Arc::clone(daemon.kit.stores.as_ref().unwrap());
        let scope = MemoryScope::new("agent:test");
        assert_eq!(sizes(&stores, &scope).await, [9_000, 3_000]);
        let mut bigger = sized;
        bigger.memory.memory_chars = 30_000;
        daemon.reload(bigger).unwrap();
        assert_eq!(sizes(&stores, &scope).await, [30_000, 3_000]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn files_off_gives_no_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut config = config();
        config.tools.files = false;
        let mut daemon =
            Daemon::open(&db, &config, Arc::new(MemoryModel::default()), UID, false).unwrap();
        // The model calls tools that the assistant does not have, so
        // nothing is kept.
        daemon
            .handle_request(UID, request("w-1", "save notes/tea.md green"))
            .await;
        let shown = daemon
            .handle_request(UID, request("w-2", "show notes/tea.md"))
            .await;
        assert_ne!(reply_text(&shown), "green");
        assert!(!crate::tools::workspace_for(&db).exists());
    }

    #[cfg(feature = "api")]
    mod api;
    #[cfg(feature = "compaction")]
    mod compaction;
    #[cfg(feature = "mcp")]
    mod flow;
    #[cfg(feature = "google")]
    mod google;
    #[cfg(feature = "patches")]
    mod patches;
    #[cfg(feature = "research")]
    mod research;
    #[cfg(feature = "sandbox")]
    mod sandboxed;
}
