//! `lg serve`: the daemon that owns the database and the agent.
//!
//! The daemon is the single writer of one SQLite file. The runtime takes a
//! lock file next to the database, so a second daemon fails to open it. The
//! kernel drops the lock when the holder dies, so a crash strands nothing.
//!
//! One task owns the runtime. The socket and the Discord channel hand each
//! message to it over a channel, so ops run one at a time, in arrival order.
//! A tick drives the assistant once a second, so a durable timer fires
//! without a message.
//!
//! A Discord reply goes out once. The message id is the idempotency key of
//! its op, so a message that arrives twice runs once. The pairing store
//! keeps the last message whose reply went out. After a reconnect, the
//! daemon reads the history after that cursor, so a message sent while it
//! was down still gets its answer.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use autumn_harvest_agent::{AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile, sqlite};
use autumn_harvest_sqlite::SqliteRuntime;
use littleguy_channels::discord::{DiscordRest, DmMessage, GatewayEvent, run_gateway};
use littleguy_core::{
    ASSISTANT_KIND, ChannelId, ChannelRule, Config, Inbound, MessageId, PairingCode, Pending,
    PendingPairings, Route, Routing, SenderId, discord_sender, pairing::PAIRING_TTL_SECS, route,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::camel::Camel;
use crate::oauth::Connections;
use crate::pairs::{Entry, PairStore};
use crate::protocol::{MAX_REQUEST_BYTES, PairEntry, Request, Response};

/// The channel id of the local socket.
pub const CLI_CHANNEL: &str = "cli";

/// The channel id of Discord.
pub const DISCORD_CHANNEL: &str = "discord";

/// The prefix of the message id of a Discord message.
const DISCORD_MESSAGE_PREFIX: &str = "discord:";

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
/// socket, which offers no pairing. With `discord`, the Discord channel
/// allows the senders of the config file and of the pairing store. Each
/// camel route of the config is an allowed sender of the camel channel.
///
/// # Errors
///
/// Returns an error when the owner name is not a valid agent name.
pub fn routing(
    config: &Config,
    owner_uid: u32,
    store: &PairStore,
    discord: bool,
) -> Result<Routing> {
    let cli = ChannelRule {
        enabled: true,
        pairing: false,
        allowed: std::iter::once(cli_sender(owner_uid)?).collect(),
    };
    let mut table = Routing::new(&config.owner)
        .with_context(|| format!("`{}` is not a valid agent name", config.owner))?
        .channel(ChannelId::new(CLI_CHANNEL)?, cli);
    if discord {
        let channel = ChannelId::new(DISCORD_CHANNEL)?;
        let paired = store
            .entries()
            .filter(|e| e.paired && e.channel == channel)
            .map(|e| e.sender.clone());
        let rule = ChannelRule {
            enabled: true,
            pairing: config.discord.pairing,
            allowed: config
                .discord
                .allowed
                .iter()
                .cloned()
                .chain(paired)
                .collect(),
        };
        table = table.channel(channel, rule);
    }
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

/// The harness of the assistant.
#[must_use]
pub fn harness(config: &Config, model: Arc<dyn AgentModel>) -> AgentHarness {
    AgentHarness::new(model).agent(
        AgentProfile::new(ASSISTANT_KIND)
            .system(config.system.as_str())
            .window(config.window),
    )
}

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

/// One reply for the Discord channel to send.
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

/// The daemon state: the runtime, the routing table and the pairings.
pub struct Daemon {
    rt: SqliteRuntime,
    routing: Routing,
    owner_uid: u32,
    config_allowed: BTreeSet<SenderId>,
    pending: PendingPairings,
    store: PairStore,
    connections: Option<Connections>,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("assistant", self.routing.assistant())
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Opens the database at `db` and registers the assistant. With
    /// `discord`, the Discord channel takes messages.
    ///
    /// # Errors
    ///
    /// Returns an error when another daemon holds the database, the database
    /// or the pairing store does not open, or the owner name is not a valid
    /// agent name.
    pub fn open(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        owner_uid: u32,
        discord: bool,
    ) -> Result<Self> {
        if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut rt =
            SqliteRuntime::open(db).with_context(|| format!("opening {}", db.display()))?;
        sqlite::register(&mut rt, Arc::new(harness(config, model)));
        // The runtime lock is held now, so this daemon is the store's one writer.
        let store = PairStore::open(crate::pairs::path_for(db))?;
        Ok(Self {
            routing: routing(config, owner_uid, &store, discord)?,
            rt,
            owner_uid,
            config_allowed: config.discord.allowed.iter().cloned().collect(),
            pending: PendingPairings::default(),
            store,
            connections: None,
        })
    }

    /// Keeps OAuth connections with `connections`.
    #[must_use]
    pub fn with_connections(mut self, connections: Connections) -> Self {
        self.connections = Some(connections);
        self
    }

    /// The agent that the owner talks to.
    #[must_use]
    pub const fn assistant(&self) -> &AgentId {
        self.routing.assistant()
    }

    /// The mapper of the camel routes of `config`. It checks each message
    /// against the routing table of this daemon.
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
    #[cfg(test)]
    pub fn state(
        &self,
    ) -> impl Future<Output = Result<Option<autumn_harvest_agent::AgentState>>> + Send + 'static
    {
        // The query starts now, so the future holds no borrow of the runtime.
        let query = sqlite::state(&self.rt, self.routing.assistant());
        async move { Ok(query.await?) }
    }

    /// Handles one request from the local socket, sent by `uid`.
    pub async fn handle_request(&mut self, uid: u32, request: Request) -> Response {
        let result = match request {
            Request::Chat { message_id, text } => match cli_inbound(uid, message_id, text) {
                Ok(inbound) => self.handle(&inbound, "").await,
                Err(error) => Err(error),
            },
            owner_only if uid != self.owner_uid => {
                tracing::warn!(uid, request = ?owner_only, "refused an owner request");
                Ok(Response::Error {
                    message: String::from("only the owner of the daemon may do that"),
                })
            }
            Request::Pair { code } => self.pair(&code),
            Request::Pairs => Ok(self.pairs()),
            Request::Unpair { sender } => self.unpair(&sender),
            request @ (Request::Connect { .. }
            | Request::Connections
            | Request::Disconnect { .. }) => {
                oauth_request(self.connections.as_ref(), request).await
            }
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
        let before = sqlite::state(&self.rt, &id).await?;
        let failures_before = before.as_ref().map_or(0, |state| state.failures);
        let key = inbound.message_id.as_str();
        let op = AgentOp::message_with_id(inbound.text.as_str(), key);
        let outcome = sqlite::send(&mut self.rt, &id, &op, Some(key))?;
        if !outcome.signal_delivered {
            return Ok(Response::Duplicate {
                last_answer: before.and_then(|state| state.last_answer),
            });
        }
        sqlite::drive(&mut self.rt, &id).await?;
        let Some(state) = sqlite::state(&self.rt, &id).await? else {
            return Ok(Response::NoAnswer {
                reason: String::from("the assistant has no running run"),
            });
        };
        if state.failures > failures_before {
            return Ok(Response::NoAnswer {
                reason: state
                    .last_error
                    .unwrap_or_else(|| String::from("the op failed")),
            });
        }
        Ok(state.last_answer.map_or_else(
            || Response::NoAnswer {
                reason: String::from("the op ended without an answer"),
            },
            |text| Response::Reply { text },
        ))
    }

    /// Handles one Discord direct message. Returns the reply to send, if
    /// any. A message whose reply went out already gets none.
    ///
    /// # Errors
    ///
    /// Returns an error when the author id is not a snowflake, or the
    /// runtime fails.
    pub async fn discord(&mut self, message: &DmMessage) -> Result<Option<Outgoing>> {
        let channel = ChannelId::new(DISCORD_CHANNEL)?;
        let sender = discord_sender(&message.author_id)?;
        if self.store.is_delivered(&channel, &sender, &message.id) {
            return Ok(None);
        }
        let inbound = Inbound {
            channel,
            sender: sender.clone(),
            message_id: MessageId::new(format!("{DISCORD_MESSAGE_PREFIX}{}", message.id))?,
            text: message.content.clone(),
        };
        let response = self.handle(&inbound, &message.channel_id).await?;
        let (text, track) = match response {
            Response::Reply { text } => (text, true),
            // The op ran before, but its reply did not go out.
            Response::Duplicate { last_answer } => (
                last_answer.unwrap_or_else(|| String::from("(no answer yet)")),
                true,
            ),
            Response::NoAnswer { reason } => (format!("Sorry, I could not answer: {reason}"), true),
            Response::Pairing { code } => (pairing_text(code.as_deref()), false),
            _ => return Ok(None),
        };
        Ok(Some(Outgoing {
            channel_id: message.channel_id.clone(),
            text,
            track: track.then_some(sender),
            message_id: message.id.clone(),
        }))
    }

    /// Records that the reply to `message_id` went out.
    ///
    /// # Errors
    ///
    /// Returns an error when the pairing store does not save.
    pub fn delivered(&mut self, sender: &SenderId, reply_to: &str, message_id: &str) -> Result<()> {
        let channel = ChannelId::new(DISCORD_CHANNEL)?;
        self.store.delivered(&channel, sender, reply_to, message_id)
    }

    /// The cursor of each allowed Discord sender with a known DM channel.
    #[must_use]
    pub fn cursors(&self) -> Vec<Cursor> {
        let Ok(channel) = ChannelId::new(DISCORD_CHANNEL) else {
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
        let delivered = pending
            .message_id
            .as_str()
            .strip_prefix(DISCORD_MESSAGE_PREFIX)
            .unwrap_or_default()
            .to_owned();
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

    /// The allowed senders of every channel.
    fn pairs(&self) -> Response {
        let mut entries = Vec::new();
        for channel in [CLI_CHANNEL, DISCORD_CHANNEL] {
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

    /// Takes a paired Discord sender off the channel. A sender that the
    /// config file allows stays allowed.
    fn unpair(&mut self, sender: &str) -> Result<Response> {
        let Ok(sender) = SenderId::new(sender) else {
            return Ok(Response::Unpaired { removed: false });
        };
        let channel = ChannelId::new(DISCORD_CHANNEL)?;
        let removed = self.store.unpair(&channel, &sender)?;
        if removed && !self.config_allowed.contains(&sender) {
            self.routing.revoke(&channel, &sender);
        }
        Ok(Response::Unpaired { removed })
    }

    /// Drives the assistant until it blocks, so a due timer fires.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot drive the run.
    pub async fn tick(&mut self) -> Result<()> {
        sqlite::drive(&mut self.rt, self.routing.assistant()).await?;
        Ok(())
    }
}

/// Handles one OAuth request of the owner.
async fn oauth_request(connections: Option<&Connections>, request: Request) -> Result<Response> {
    let Some(connections) = connections else {
        return Ok(Response::Error {
            message: String::from(
                "this daemon keeps no OAuth connections. Add a [san_mateo] section",
            ),
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
enum Job {
    /// A request from the local socket.
    Socket {
        uid: u32,
        request: Request,
        reply: oneshot::Sender<Response>,
    },
    /// A Discord direct message.
    Discord {
        message: DmMessage,
        reply: oneshot::Sender<Option<Outgoing>>,
    },
    /// A Discord reply went out.
    Delivered {
        sender: SenderId,
        reply_to: String,
        message_id: String,
    },
    /// The cursors to read the missed history from.
    Cursors { reply: oneshot::Sender<Vec<Cursor>> },
}

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
        }
        Job::Discord { message, reply } => {
            let outgoing = daemon.discord(&message).await.unwrap_or_else(|error| {
                tracing::error!(error = %format!("{error:#}"), "a discord message failed");
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
        Job::Cursors { reply } => {
            let _ = reply.send(daemon.cursors());
        }
    }
}

/// Runs the daemon until `stop` completes or the accept loop ends.
///
/// # Errors
///
/// Returns an error only when the loop cannot start.
pub async fn run(
    mut daemon: Daemon,
    listener: UnixListener,
    discord: Option<DiscordLink>,
    camel: Option<Camel>,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<Job>(64);
    // Without camel routes, the sender drops at once and the arm sleeps.
    let (camel_tx, mut camel_rx) = mpsc::channel::<crate::camel::Apply>(4);
    let mut tasks = vec![tokio::spawn(accept_loop(listener, tx.clone()))];
    if let Some(link) = discord {
        let (events_tx, events) = mpsc::unbounded_channel();
        tasks.push(tokio::spawn(run_gateway(
            link.token.clone(),
            link.gateway_url.clone(),
            events_tx,
        )));
        tasks.push(tokio::spawn(discord_loop(link.rest, events, tx.clone())));
    }
    if let Some(camel) = &camel {
        tasks.push(tokio::spawn(crate::camel::pump(camel.inbound(), camel_tx)));
    } else {
        drop(camel_tx);
    }
    drop(tx);
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(stop);
    tracing::info!(assistant = %daemon.assistant(), "littleguy is listening");
    loop {
        tokio::select! {
            job = rx.recv() => {
                let Some(job) = job else { break };
                run_job(&mut daemon, job).await;
            }
            Some(apply) = camel_rx.recv() => daemon.camel(apply).await,
            _ = ticker.tick() => {
                if let Err(error) = daemon.tick().await {
                    tracing::warn!(error = %format!("{error:#}"), "a tick failed");
                }
            }
            () = &mut stop => break,
        }
    }
    for task in tasks {
        task.abort();
    }
    if let Some(camel) = camel {
        camel.stop().await;
    }
    if let Some(connections) = daemon.connections.take() {
        connections.stop().await;
    }
    Ok(())
}

/// Turns gateway events into jobs, and sends each reply.
async fn discord_loop(
    rest: DiscordRest,
    mut events: mpsc::UnboundedReceiver<GatewayEvent>,
    jobs: mpsc::Sender<Job>,
) {
    while let Some(event) = events.recv().await {
        match event {
            GatewayEvent::Message(message) => deliver(&rest, &jobs, message).await,
            GatewayEvent::Ready => {
                let (reply, cursors) = oneshot::channel();
                if jobs.send(Job::Cursors { reply }).await.is_err() {
                    return;
                }
                for cursor in cursors.await.unwrap_or_default() {
                    match rest.messages_after(&cursor.channel_id, &cursor.after).await {
                        Ok(missed) => {
                            for message in missed {
                                deliver(&rest, &jobs, message).await;
                            }
                        }
                        Err(error) => tracing::warn!(%error, "reading missed messages failed"),
                    }
                }
            }
        }
    }
}

/// Runs one message and sends its reply. A failed send moves no cursor, so
/// the history read after the next reconnect tries it again.
async fn deliver(rest: &DiscordRest, jobs: &mpsc::Sender<Job>, message: DmMessage) {
    let (reply, answer) = oneshot::channel();
    if jobs.send(Job::Discord { message, reply }).await.is_err() {
        return;
    }
    let Ok(Some(outgoing)) = answer.await else {
        return;
    };
    if let Err(error) = rest.send(&outgoing.channel_id, &outgoing.text).await {
        tracing::warn!(%error, "a discord reply did not go out");
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

async fn accept_loop(listener: UnixListener, tx: mpsc::Sender<Job>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let tx = tx.clone();
                tokio::spawn(async move {
                    if let Err(error) = serve_connection(stream, tx).await {
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

async fn serve_connection(stream: UnixStream, tx: mpsc::Sender<Job>) -> Result<()> {
    let uid = stream
        .peer_cred()
        .context("reading the peer credentials")?
        .uid();
    let (read, mut write) = stream.into_split();
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
            Ok(request) => {
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
    let mut out = serde_json::to_vec(&response)?;
    out.push(b'\n');
    write
        .write_all(&out)
        .await
        .context("writing the response")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

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
            response,
            Response::Reply {
                text: String::from("littleguy (offline): you said 'hello'")
            }
        );
        assert_eq!(model.calls(), 1);
        assert_eq!(daemon.assistant().as_str(), "assistant/mark");
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
        let (served, (reply, bad_json, too_long)) =
            tokio::join!(run(daemon, listener, None, None, stop), client);
        served.unwrap();
        assert_eq!(
            reply,
            Response::Reply {
                text: String::from("littleguy (offline): you said 'hi'")
            }
        );
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
            daemon.cursors(),
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
        assert!(daemon.cursors().is_empty(), "no DM channel is known yet");

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
            daemon.cursors(),
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
        let (served, ()) = tokio::join!(run(daemon, listener, Some(link), None, stop), script);
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
        let (served, held) = tokio::join!(run(daemon, listener, None, Some(camel), stop), watch);
        served.unwrap();
        held
    }

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

    #[tokio::test(flavor = "multi_thread")]
    async fn oauth_requests_are_for_the_owner_and_need_the_section() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("lg.db");
        let mut daemon = open(&db, Arc::new(OfflineModel::new()));
        let refused = daemon.handle_request(UID + 1, Request::Connections).await;
        assert!(matches!(refused, Response::Error { ref message } if message.contains("owner")));
        let none = daemon.handle_request(UID, Request::Connections).await;
        assert!(matches!(none, Response::Error { ref message } if message.contains("[san_mateo]")));

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
}
