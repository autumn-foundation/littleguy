//! Camel routes that feed the assistant.
//!
//! Each `[[camel.route]]` of the config is a camel route, such as a watched
//! directory or a timer. Every route ends in one `harvest:` stream. The
//! bridge of harvest-camel takes each message off that stream in three
//! phases, so the runtime stays in the one task that owns it:
//!
//! 1. `receive` waits for messages and maps each one, in the camel task.
//! 2. `apply` writes them into the runtime, in the runtime task.
//! 3. `settle` acks, abandons or dead-letters each one, in the camel task.
//!
//! A message is acked only after its write is durable, so a crash before
//! that point leaves it for the next poll. The idempotency key of the
//! signal comes from a stable message id, so a redelivery runs once.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use autumn_harvest::entity::ENTITY_OP_SIGNAL;
use autumn_harvest_agent::{AGENT_WORKFLOW_NAME, Agent, AgentId, AgentOp};
use autumn_harvest_plugin::connector::{
    ConnectorError, EventSource, MappedMessage, MappingError, MessageCoordinates, MessageCtx,
    SourceBinding,
};
use autumn_harvest_sqlite::SqliteAuthStore;
use camel_api::{Exchange, Value};
use camel_builder::{RouteBuilder, StepAccumulator};
use camel_component_direct::DirectComponent;
use camel_component_file::FileComponent;
use camel_component_timer::TimerComponent;
use camel_core::CamelContext;
use harvest_camel::sqlite::{AppliedBatch, InboundBatch, SqliteDeadLetters, SqliteInbound};
use harvest_camel::{CamelSource, HarvestBridge, check_route};
use littleguy_core::{
    CamelRoute, ChannelId, Inbound, MessageId, Route, Routing, SenderId, ids::MAX_ID_BYTES, route,
};
use tokio::sync::{mpsc, oneshot};

/// The channel id of camel routes.
pub const CAMEL_CHANNEL: &str = "camel";

/// The one harvest stream that every route ends in.
pub const STREAM: &str = "littleguy";

/// The name of the connector binding. It is the dedupe namespace, so it
/// never changes.
const BINDING: &str = "littleguy-camel";

/// The header that holds the redelivery-stable id of a message.
pub const ID_HEADER: &str = "LittleguyMessageId";

/// The header that names the route of a message.
pub const ROUTE_HEADER: &str = "LittleguyRoute";

/// How long one receive waits for a message.
const RECEIVE_WAIT: Duration = Duration::from_millis(500);

/// The id of the camel route of the config route `id`.
#[must_use]
pub fn camel_route_id(id: &str) -> String {
    format!("lg-{id}")
}

/// A stable id for one exchange, from the headers that the component sets.
///
/// A file keeps its path and modification time across polls, so a file
/// that is read again keeps its id. A timer fire has its name and time.
/// Any other exchange gets a fresh id, so it is never deduped.
#[must_use]
pub fn message_id_of(exchange: &Exchange) -> Value {
    let header = |name: &str| exchange.input.header(name).map(plain);
    if let Some(id) = header(ID_HEADER) {
        return Value::String(id);
    }
    let id = match (
        header("CamelFileAbsolutePath"),
        header("CamelFileLastModified"),
        header("CamelTimerName"),
        header("CamelTimerFiredTime"),
    ) {
        (Some(path), Some(modified), _, _) => format!("file:{path}@{modified}"),
        (_, _, Some(name), Some(fired)) => format!("timer:{name}@{fired}"),
        _ => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos());
            format!("exchange:{nanos}")
        }
    };
    Value::String(id)
}

/// The text of a header value: a string as it is, anything else as JSON.
fn plain(value: &Value) -> String {
    value
        .as_str()
        .map_or_else(|| value.to_string(), ToOwned::to_owned)
}

/// The FNV-1a hash of `text`. It is stable across builds and platforms.
fn fnv1a(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The agent message id of a camel message. A long id keeps its start and
/// ends in a hash of the whole, so it stays unique and fits the bound.
fn agent_message_id(route_id: &str, message_id: &str) -> Result<MessageId, MappingError> {
    let full = format!("camel:{route_id}:{message_id}");
    let text = if full.len() <= MAX_ID_BYTES {
        full
    } else {
        let hash = format!(":{:016x}", fnv1a(&full));
        let mut end = MAX_ID_BYTES - hash.len();
        while !full.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}{hash}", &full[..end])
    };
    MessageId::new(text).map_err(|error| MappingError::Deserialize(error.to_string()))
}

/// What the mapper needs: the assistant, the routing table and the routes.
#[derive(Debug, Clone)]
pub struct Mapper {
    /// The assistant that each message goes to.
    pub assistant: AgentId,
    /// The routing table. It allows each route of the config.
    pub routing: Routing,
    /// The routes of the config.
    pub routes: Vec<CamelRoute>,
}

impl Mapper {
    /// Maps one camel message to an op for the assistant.
    ///
    /// # Errors
    ///
    /// Returns `Deserialize` for a body that is not UTF-8 or a message with
    /// no stable id, and `Rejected` for a route that the routing table
    /// drops. Each one is poison, so it goes to the dead letters.
    pub fn map(&self, ctx: &MessageCtx) -> Result<MappedMessage, MappingError> {
        let route_id = ctx
            .header(ROUTE_HEADER)
            .ok_or_else(|| MappingError::Rejected(format!("the message has no {ROUTE_HEADER}")))?;
        let config = self
            .routes
            .iter()
            .find(|r| r.id == route_id)
            .ok_or_else(|| MappingError::Rejected(format!("no camel route is `{route_id}`")))?;
        let MessageCoordinates::Opaque { id, .. } = &ctx.coordinates else {
            return Err(MappingError::Deserialize(String::from(
                "the message has no stable id",
            )));
        };
        let body = std::str::from_utf8(&ctx.raw_body)
            .map_err(|_| MappingError::Deserialize(String::from("the body is not UTF-8 text")))?;
        let text = config
            .prompt
            .as_deref()
            .map_or_else(|| body.to_owned(), |prompt| format!("{prompt}\n\n{body}"));
        let message_id = agent_message_id(route_id, id)?;
        let inbound = Inbound {
            channel: ChannelId::new(CAMEL_CHANNEL)
                .map_err(|error| MappingError::Rejected(error.to_string()))?,
            sender: config
                .sender()
                .map_err(|error| MappingError::Rejected(error.to_string()))?,
            message_id: message_id.clone(),
            text,
        };
        match route(&self.routing, &inbound) {
            Route::Agent(agent) if agent == self.assistant => {}
            _ => {
                return Err(MappingError::Rejected(format!(
                    "the routing table drops {}",
                    inbound.sender
                )));
            }
        }
        let op = AgentOp::message_with_id(inbound.text.as_str(), message_id.as_str());
        Ok(MappedMessage::new(
            self.assistant.as_str(),
            Agent::payload(&op),
        ))
    }
}

/// The connector binding of every route.
#[must_use]
pub fn binding(mapper: Mapper) -> SourceBinding {
    SourceBinding::signals_with_start(BINDING, STREAM, AGENT_WORKFLOW_NAME, ENTITY_OP_SIGNAL)
        .map_raw(move |ctx| mapper.map(ctx))
}

/// The senders of the camel channel.
///
/// # Errors
///
/// Never for a checked config.
pub fn senders(routes: &[CamelRoute]) -> Result<Vec<SenderId>> {
    routes
        .iter()
        .map(|r| r.sender().context("a camel route id"))
        .collect()
}

/// One batch for the runtime task to apply.
#[derive(Debug)]
pub struct Apply {
    /// The mapped messages.
    pub batch: InboundBatch,
    /// Where the applied batch goes back.
    pub reply: oneshot::Sender<AppliedBatch>,
}

/// The running camel context and its bridge.
pub struct Camel {
    context: CamelContext,
    source: Arc<CamelSource>,
    inbound: Arc<SqliteInbound>,
}

impl std::fmt::Debug for Camel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Camel")
            .field("inbound", &self.inbound)
            .finish_non_exhaustive()
    }
}

impl Camel {
    /// Starts every route of `mapper`. Dead letters go to the database at
    /// `db`, in `harvest_connector_dead_letters`.
    ///
    /// # Errors
    ///
    /// Returns an error when a route does not build or start, or the dead
    /// letter store does not open.
    pub async fn start(mapper: Mapper, db: &Path) -> Result<Self> {
        let routes = mapper.routes.clone();
        let (source, component) = HarvestBridge::new(STREAM);
        let store = SqliteAuthStore::open(db)
            .with_context(|| format!("opening the dead letters in {}", db.display()))?;
        let inbound =
            SqliteInbound::new(binding(mapper), Arc::clone(&source) as Arc<dyn EventSource>)
                .context("the camel binding")?
                .with_dead_letter_sink(Arc::new(SqliteDeadLetters::new(Arc::new(store))));
        let mut context = CamelContext::builder()
            .build()
            .await
            .context("building the camel context")?;
        context.register_component(FileComponent::new());
        context.register_component(TimerComponent::new());
        context.register_component(DirectComponent::new());
        context.register_component(component);
        for config in &routes {
            let definition = RouteBuilder::from(&config.from)
                .route_id(camel_route_id(&config.id))
                .set_header(ROUTE_HEADER, Value::String(config.id.clone()))
                .set_header_fn(ID_HEADER, message_id_of)
                .to(format!("harvest:{STREAM}?idHeader={ID_HEADER}"))
                .build()
                .with_context(|| format!("building camel route `{}`", config.id))?;
            check_route(&definition).with_context(|| format!("camel route `{}`", config.id))?;
            context
                .add_route_definition(definition)
                .await
                .with_context(|| format!("adding camel route `{}`", config.id))?;
        }
        context.start().await.context("starting the camel routes")?;
        tracing::info!(routes = routes.len(), "camel routes are running");
        Ok(Self {
            context,
            source,
            inbound: Arc::new(inbound),
        })
    }

    /// The bridge, for the loop that receives and settles.
    #[must_use]
    pub fn inbound(&self) -> Arc<SqliteInbound> {
        Arc::clone(&self.inbound)
    }

    /// Stops the routes and closes the bridge. A message in flight is not
    /// acked, so the next start reads it again.
    pub async fn stop(mut self) {
        if let Err(error) = self.context.stop().await {
            tracing::warn!(%error, "the camel routes did not stop cleanly");
        }
        self.source.close().await;
    }
}

/// Receives batches, has the runtime task apply them, and settles them.
/// `wrap` turns each batch into a job of `jobs`. It ends when the bridge
/// closes or the runtime task stops.
pub async fn pump<T>(inbound: Arc<SqliteInbound>, jobs: mpsc::Sender<T>, wrap: fn(Apply) -> T) {
    loop {
        let batch = match inbound.receive(RECEIVE_WAIT).await {
            Ok(batch) => batch,
            Err(ConnectorError::Closed) => return,
            Err(error) => {
                tracing::warn!(%error, "a camel receive failed");
                tokio::time::sleep(RECEIVE_WAIT).await;
                continue;
            }
        };
        if batch.is_empty() {
            continue;
        }
        let (reply, applied) = oneshot::channel();
        if jobs.send(wrap(Apply { batch, reply })).await.is_err() {
            return;
        }
        let Ok(applied) = applied.await else {
            return;
        };
        let summary = inbound.settle(applied).await;
        tracing::info!(
            received = summary.received,
            acked = summary.acked,
            retried = summary.retried,
            dead_lettered = summary.dead_lettered,
            "settled camel messages"
        );
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use autumn_harvest_plugin::connector::{InboundMessage, MessageHandle};
    use camel_api::Message;
    use littleguy_core::{ChannelRule, OwnerName};

    use super::*;

    fn route(id: &str, prompt: Option<&str>) -> CamelRoute {
        CamelRoute {
            id: id.to_owned(),
            from: String::from("file:/in"),
            prompt: prompt.map(ToOwned::to_owned),
        }
    }

    fn table(routes: Vec<CamelRoute>, allowed: &[&str]) -> Mapper {
        let owner = OwnerName::new("mark").unwrap();
        let rule = ChannelRule {
            enabled: true,
            pairing: false,
            allowed: allowed.iter().map(|s| SenderId::new(*s).unwrap()).collect(),
        };
        let routing = Routing::new(&owner)
            .unwrap()
            .channel(ChannelId::new(CAMEL_CHANNEL).unwrap(), rule);
        Mapper {
            assistant: routing.assistant().clone(),
            routing,
            routes,
        }
    }

    fn ctx(headers: &[(&str, &str)], id: Option<&str>, body: &[u8]) -> MessageCtx {
        let coordinates = id.map_or_else(
            || MessageCoordinates::KafkaOffset {
                topic: String::from(STREAM),
                partition: 0,
                offset: 1,
            },
            |id| MessageCoordinates::Opaque {
                stream: String::from(STREAM),
                id: id.to_owned(),
            },
        );
        let message = InboundMessage {
            coordinates,
            payload: body.to_vec(),
            key: None,
            headers: headers
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect::<BTreeMap<_, _>>(),
            handle: MessageHandle::opaque(String::from("h")),
        };
        MessageCtx::new(BINDING, &message)
    }

    #[test]
    fn a_message_becomes_an_op_for_the_assistant() {
        let one = table(vec![route("inbox", Some("Read it."))], &["camel:inbox"]);
        let mapped = one
            .map(&ctx(
                &[(ROUTE_HEADER, "inbox")],
                Some("file:/in/a@1"),
                b"hello",
            ))
            .unwrap();
        assert_eq!(mapped.workflow_id.as_str(), "assistant/mark");
        let op = AgentOp::message_with_id("Read it.\n\nhello", "camel:inbox:file:/in/a@1");
        assert_eq!(mapped.payload, Agent::payload(&op));
        let bare = table(vec![route("inbox", None)], &["camel:inbox"]);
        let mapped = bare
            .map(&ctx(&[(ROUTE_HEADER, "inbox")], Some("x"), b"hello"))
            .unwrap();
        assert_eq!(
            mapped.payload,
            Agent::payload(&AgentOp::message_with_id("hello", "camel:inbox:x"))
        );
    }

    #[test]
    fn bad_messages_are_poison() {
        let one = table(vec![route("inbox", None)], &["camel:inbox"]);
        let header = [(ROUTE_HEADER, "inbox")];
        assert!(matches!(
            one.map(&ctx(&[], Some("x"), b"hi")),
            Err(MappingError::Rejected(_))
        ));
        assert!(matches!(
            one.map(&ctx(&[(ROUTE_HEADER, "other")], Some("x"), b"hi")),
            Err(MappingError::Rejected(_))
        ));
        assert!(matches!(
            one.map(&ctx(&header, None, b"hi")),
            Err(MappingError::Deserialize(_))
        ));
        assert!(matches!(
            one.map(&ctx(&header, Some("x"), &[0xff, 0xfe])),
            Err(MappingError::Deserialize(_))
        ));
        let closed = table(vec![route("inbox", None)], &[]);
        assert!(matches!(
            closed.map(&ctx(&header, Some("x"), b"hi")),
            Err(MappingError::Rejected(m)) if m.contains("camel:inbox")
        ));
    }

    #[test]
    fn a_long_id_is_hashed_to_fit() {
        let long = "p/".repeat(200);
        let a = agent_message_id("inbox", &long).unwrap();
        let b = agent_message_id("inbox", &format!("{long}x")).unwrap();
        assert!(a.as_str().len() <= MAX_ID_BYTES);
        assert_ne!(a, b);
        assert_eq!(a, agent_message_id("inbox", &long).unwrap(), "stable");
        assert_eq!(fnv1a(""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a("a"), 0xaf63_dc4c_8601_ec8c);
    }

    #[test]
    fn the_message_id_comes_from_stable_headers() {
        let with = |headers: &[(&str, Value)]| {
            let mut message = Message::new("x");
            for (name, value) in headers {
                message.set_header(*name, value.clone());
            }
            message_id_of(&Exchange::new(message))
        };
        assert_eq!(
            with(&[(ID_HEADER, Value::from("given"))]),
            Value::from("given")
        );
        assert_eq!(
            with(&[
                ("CamelFileAbsolutePath", Value::from("/in/a.txt")),
                ("CamelFileLastModified", Value::from(17)),
            ]),
            Value::from("file:/in/a.txt@17")
        );
        assert_eq!(
            with(&[
                ("CamelTimerName", Value::from("t")),
                ("CamelTimerFiredTime", Value::from("2026")),
            ]),
            Value::from("timer:t@2026")
        );
        let fresh = with(&[]);
        assert!(fresh.as_str().is_some_and(|id| id.starts_with("exchange:")));
    }
}
