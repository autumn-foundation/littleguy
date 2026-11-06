//! `lg serve`: the daemon that owns the database and the agent.
//!
//! The daemon is the single writer of one SQLite file. The runtime takes a
//! lock file next to the database, so a second daemon fails to open it. The
//! kernel drops the lock when the holder dies, so a crash strands nothing.
//!
//! One task owns the runtime. The accept loop hands each request to it over
//! a channel, so ops run one at a time, in arrival order. A tick drives the
//! assistant once a second, so a durable timer fires without a message.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use autumn_harvest_agent::{AgentHarness, AgentId, AgentModel, AgentOp, AgentProfile, sqlite};
use autumn_harvest_sqlite::SqliteRuntime;
use littleguy_core::{
    ASSISTANT_KIND, ChannelId, ChannelRule, Config, Inbound, MessageId, Route, Routing, SenderId,
    route,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot};

use crate::protocol::{MAX_REQUEST_BYTES, Request, Response};

/// The channel id of the local socket.
pub const CLI_CHANNEL: &str = "cli";

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
/// Only the local socket is a channel in milestone 0. The user who runs the
/// daemon is the one allowed sender. The socket offers no pairing.
///
/// # Errors
///
/// Returns an error when the owner name is not a valid agent name.
pub fn routing(config: &Config, owner_uid: u32) -> Result<Routing> {
    let rule = ChannelRule {
        enabled: true,
        pairing: false,
        allowed: std::iter::once(cli_sender(owner_uid)?).collect(),
    };
    let table = Routing::new(&config.owner)
        .with_context(|| format!("`{}` is not a valid agent name", config.owner))?;
    Ok(table.channel(ChannelId::new(CLI_CHANNEL)?, rule))
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

/// The daemon state: the runtime and the routing table.
pub struct Daemon {
    rt: SqliteRuntime,
    routing: Routing,
}

impl std::fmt::Debug for Daemon {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Daemon")
            .field("assistant", self.routing.assistant())
            .finish_non_exhaustive()
    }
}

impl Daemon {
    /// Opens the database at `db` and registers the assistant.
    ///
    /// # Errors
    ///
    /// Returns an error when another daemon holds the database, the database
    /// does not open, or the owner name is not a valid agent name.
    pub fn open(
        db: &Path,
        config: &Config,
        model: Arc<dyn AgentModel>,
        owner_uid: u32,
    ) -> Result<Self> {
        if let Some(parent) = db.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let mut rt =
            SqliteRuntime::open(db).with_context(|| format!("opening {}", db.display()))?;
        sqlite::register(&mut rt, Arc::new(harness(config, model)));
        Ok(Self {
            rt,
            routing: routing(config, owner_uid)?,
        })
    }

    /// The agent that the owner talks to.
    #[must_use]
    pub const fn assistant(&self) -> &AgentId {
        self.routing.assistant()
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
        let inbound = match inbound(uid, request) {
            Ok(inbound) => inbound,
            Err(error) => {
                return Response::Error {
                    message: format!("{error:#}"),
                };
            }
        };
        match self.handle(&inbound).await {
            Ok(response) => response,
            Err(error) => {
                tracing::error!(error = %format!("{error:#}"), "an op failed");
                Response::Error {
                    message: format!("{error:#}"),
                }
            }
        }
    }

    /// Routes one message, runs it, and returns the answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime cannot record, run or read the op.
    pub async fn handle(&mut self, inbound: &Inbound) -> Result<Response> {
        let id = match route(&self.routing, inbound) {
            Route::Agent(id) => id,
            Route::Pairing => return Ok(Response::Pairing),
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

/// Builds the inbound message of one socket request.
fn inbound(uid: u32, request: Request) -> Result<Inbound> {
    Ok(Inbound {
        channel: ChannelId::new(CLI_CHANNEL)?,
        sender: cli_sender(uid)?,
        message_id: MessageId::new(request.message_id).context("bad message id")?,
        text: request.text,
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

/// One request on its way to the runtime task.
struct Job {
    uid: u32,
    request: Request,
    reply: oneshot::Sender<Response>,
}

/// Runs the daemon until `stop` completes or the accept loop ends.
///
/// # Errors
///
/// Returns an error only when the loop cannot start.
pub async fn run(
    mut daemon: Daemon,
    listener: UnixListener,
    stop: impl Future<Output = ()>,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel::<Job>(64);
    let accept = tokio::spawn(accept_loop(listener, tx));
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    tokio::pin!(stop);
    tracing::info!(assistant = %daemon.assistant(), "littleguy is listening");
    loop {
        tokio::select! {
            job = rx.recv() => {
                let Some(job) = job else { break };
                let response = daemon.handle_request(job.uid, job.request).await;
                // A client that hung up gets no answer. The op still ran.
                let _ = job.reply.send(response);
            }
            _ = ticker.tick() => {
                if let Err(error) = daemon.tick().await {
                    tracing::warn!(error = %format!("{error:#}"), "a tick failed");
                }
            }
            () = &mut stop => break,
        }
    }
    accept.abort();
    Ok(())
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
                tx.send(Job {
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
        Request {
            message_id: String::from(id),
            text: String::from(text),
        }
    }

    fn open(db: &Path, model: Arc<OfflineModel>) -> Daemon {
        Daemon::open(db, &config(), model, UID).unwrap()
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
        let daemon = Daemon::open(&dir.path().join("lg.db"), &config(), model, uid).unwrap();
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
            tokio::join!(run(daemon, listener, stop), client);
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
        let second = Daemon::open(&db, &config(), Arc::new(OfflineModel::new()), UID);
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
}
