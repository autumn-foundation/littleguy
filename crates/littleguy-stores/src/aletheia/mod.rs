//! The memory blocks and the episode log in AletheiaDB (ADR 0047).
//!
//! [`AletheiaStores`] implements the engine's [`MemoryStore`] and
//! [`EpisodeStore`] on a directory of AletheiaDB, through
//! `autumn-plugin-aletheiadb`. The rules stay in the engine, as in the
//! SQLite store: `memory::apply_to` for a memory edit and [`EpisodeLog`]
//! for the log. Each change is one write transaction.
//!
//! The store also keeps what the prompt cannot hold. Each version of each
//! memory entry is a `Fact` node, and an entry that goes away has its valid
//! time ended. Each episode is an `Episode` node that stays when the log
//! evicts it. Both carry an embedding, so [`AletheiaStores::recall`] finds
//! them by meaning, now or as of a past time.
//!
//! One lock runs the writes one at a time. A write transaction of
//! AletheiaDB does not refuse two writers that each create a key that the
//! other one looks for. The embedding calls run before each write, while
//! the lock waits.

mod graph;
mod log;
mod recall;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use autumn_harvest_agent::episode::{
    Appended, Cleared, Episode, EpisodeLog, EpisodeStore, PassMark,
};
use autumn_harvest_agent::memory::{EditKey, MemoryBlock, MemoryOp, MemoryScope, MemoryStore};
use autumn_harvest_agent::model::BoxFuture;
use autumn_harvest_agent::{AgentError, ErrorKind};
use autumn_plugin_aletheiadb::{
    AletheiaDb, AletheiaDbConfig, AletheiaDbError, AletheiaTime, IN_MEMORY, NodeView, VectorMetric,
    WriteTx,
};
use littleguy_core::embed::Embedder;
use serde_json::Value;

use self::graph::{
    CLEAR, DERIVED, EDIT_KEY, EMBEDDING, EPISODE, FACT, LOG, LOG_ENTRY, LOG_KEY, META, Reads,
    SCOPE, find_all, find_one, flag, int, join, patch, props, retract_all, text, uint, vector,
};
use self::log::{Entry, Parts, align};
pub use self::recall::{Recall, RecallKind, Recalled};

/// The most edit keys that one memory scope keeps, as in the SQLite store.
pub const MAX_EDIT_KEYS: u64 = 4_096;

/// The name of the directory beside the database.
pub const DIRECTORY: &str = "memory.aletheia";

/// How long one call to the database may take. An import of a large
/// `memory.db` writes in one transaction.
const CALL_TIMEOUT_MS: u64 = 120_000;

/// A failure to open the store.
#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    /// Another process holds the directory.
    #[error("another process holds {0}: stop the other daemon first")]
    Locked(PathBuf),
    /// The lock file does not open.
    #[error("the lock file of the memory directory does not open: {0}")]
    Io(#[from] std::io::Error),
    /// The database does not open.
    #[error("the memory directory does not open: {0}")]
    Db(#[from] AletheiaDbError),
    /// The directory holds vectors of other dimensions.
    #[error(
        "memory.embeddings.dimensions is {configured}, but the memory directory holds vectors \
         of {stored}: set it back to {stored}, or start a new directory"
    )]
    Dimensions {
        /// The dimensions in the directory.
        stored: u32,
        /// The dimensions of the embedder.
        configured: u32,
    },
    /// A store call failed while the store opened.
    #[error("{0}")]
    Agent(#[from] AgentError),
}

/// The memory and the episode log of littleguy in AletheiaDB.
pub struct AletheiaStores {
    db: AletheiaDb,
    embedder: Arc<dyn Embedder>,
    writer: tokio::sync::Mutex<()>,
    template: Mutex<Vec<MemoryBlock>>,
    /// The lock file of the directory. It is held while the store is open.
    _lock: Option<std::fs::File>,
}

impl std::fmt::Debug for AletheiaStores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AletheiaStores")
            .field("db", &self.db)
            .field("embedder", &self.embedder.name())
            .finish_non_exhaustive()
    }
}

/// The directory of the store beside the database `db`.
#[must_use]
pub fn path_for(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(DIRECTORY)
}

/// A store error as the engine sees it. A database failure can pass, so a
/// retry may help.
fn unavailable(error: &AletheiaDbError) -> AgentError {
    AgentError::new(
        ErrorKind::Unavailable,
        format!("the memory store failed: {error}"),
    )
}

/// A stored value that does not decode stays broken.
fn undecodable(what: impl std::fmt::Display) -> AgentError {
    AgentError::new(
        ErrorKind::Decode,
        format!("the memory store holds a value that does not decode: {what}"),
    )
}

/// A failure inside one transaction: a database error, or a value that
/// does not decode. The plugin carries the second one as invalid input.
fn decode_in_tx(what: impl std::fmt::Display) -> AletheiaDbError {
    AletheiaDbError::InvalidInput(format!("undecodable: {what}"))
}

/// The error that a transaction failure gives the engine.
fn agent(error: &AletheiaDbError) -> AgentError {
    let message = error.to_string();
    if let Some(rest) = message.split("undecodable: ").nth(1) {
        return undecodable(rest);
    }
    unavailable(error)
}

/// The time now.
fn now() -> Result<AletheiaTime, AletheiaDbError> {
    AletheiaTime::now()
}

/// The time now, in epoch milliseconds.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

impl AletheiaStores {
    /// Opens the store in the directory `dir`, and creates it when it is
    /// missing. The file `<dir>.lock` keeps a second process out.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError`] when another process holds the directory, the
    /// database does not open, or it holds vectors of other dimensions.
    pub async fn open(
        dir: impl AsRef<Path>,
        embedder: Arc<dyn Embedder>,
    ) -> Result<Self, OpenError> {
        let dir = dir.as_ref();
        if let Some(parent) = dir.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        let lock_path = dir.with_extension("lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)?;
        rustix::fs::flock(&lock, rustix::fs::FlockOperation::NonBlockingLockExclusive)
            .map_err(|_| OpenError::Locked(dir.to_path_buf()))?;
        Self::start(dir.to_string_lossy().into_owned(), embedder, Some(lock)).await
    }

    /// Opens a store in process memory, for tests.
    ///
    /// # Errors
    ///
    /// Returns [`OpenError`] when the database does not start.
    pub async fn open_in_memory(embedder: Arc<dyn Embedder>) -> Result<Self, OpenError> {
        Self::start(IN_MEMORY.to_owned(), embedder, None).await
    }

    async fn start(
        path: String,
        embedder: Arc<dyn Embedder>,
        lock: Option<std::fs::File>,
    ) -> Result<Self, OpenError> {
        let mut config = AletheiaDbConfig::default();
        config.path = path;
        config.vector_property = Some(EMBEDDING.to_owned());
        config.vector_dimensions = Some(embedder.dimensions());
        config.vector_metric = VectorMetric::Cosine;
        config.timeout_ms = CALL_TIMEOUT_MS;
        let db = AletheiaDb::open(config).await?;
        let store = Self {
            db,
            embedder,
            writer: tokio::sync::Mutex::new(()),
            template: Mutex::new(MemoryBlock::defaults()),
            _lock: lock,
        };
        store.check_meta().await?;
        Ok(store)
    }

    /// Reads the embedder that the directory records. A new directory
    /// records this one. Other dimensions fail the open. Another model of
    /// the same dimensions embeds every node again.
    async fn check_meta(&self) -> Result<(), OpenError> {
        let meta = self.db.read(|tx| find_one(tx, META, "kind", META)).await?;
        let name = self.embedder.name().to_owned();
        let dims = self.embedder.dimensions();
        match meta {
            Some(meta) => {
                let stored = u32::try_from(int(&meta, "dimensions")).unwrap_or(0);
                if stored != dims {
                    return Err(OpenError::Dimensions {
                        stored,
                        configured: dims,
                    });
                }
                if text(&meta, "embedder").as_deref() != Some(name.as_str()) {
                    self.db.rebuild_vector_index().await?;
                    self.embed_nodes(true).await?;
                    let id = meta.id;
                    self.db
                        .write(move |tx| {
                            tx.update_node(id, &patch(vec![("embedder", Value::from(name))]))
                        })
                        .await?;
                }
            }
            None => {
                self.db
                    .write(move |tx| {
                        tx.create_node(
                            META,
                            &props(
                                META,
                                "",
                                vec![
                                    ("embedder", Value::from(name)),
                                    ("dimensions", Value::from(dims)),
                                    ("imported", Value::Bool(false)),
                                ],
                            ),
                        )
                        .map(drop)
                    })
                    .await?;
            }
        }
        // A rebuild keeps the index that the directory holds, and fills a
        // new one from the stored nodes.
        self.db.rebuild_vector_index().await?;
        Ok(())
    }

    /// The blocks of a new scope.
    fn template(&self) -> Vec<MemoryBlock> {
        self.template
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Sets the size of each block that `limits` names, as
    /// `SqliteStores::set_limits` does: for a new scope, and for every
    /// stored scope. A stored block never shrinks below what its entries
    /// use.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write or a scope does not
    /// decode.
    pub async fn set_limits(&self, limits: &[(&str, usize)]) -> Result<(), AgentError> {
        let limits: Vec<(String, usize)> = limits
            .iter()
            .map(|(label, size)| ((*label).to_owned(), *size))
            .collect();
        let resize = move |blocks: &mut Vec<MemoryBlock>, keep_used: bool| {
            for block in blocks {
                if let Some((_, limit)) = limits.iter().find(|(label, _)| *label == block.label) {
                    block.limit_chars = if keep_used {
                        (*limit).max(block.used_chars())
                    } else {
                        *limit
                    };
                }
            }
        };
        resize(
            &mut self.template.lock().unwrap_or_else(PoisonError::into_inner),
            false,
        );
        let _writer = self.writer.lock().await;
        self.db
            .write(move |tx| {
                let at = now()?;
                for node in find_all(tx, SCOPE, "kind", SCOPE)? {
                    let Some(stored) = text(&node, "blocks") else {
                        continue;
                    };
                    let mut blocks: Vec<MemoryBlock> =
                        serde_json::from_str(&stored).map_err(decode_in_tx)?;
                    let before = blocks.clone();
                    resize(&mut blocks, true);
                    if blocks == before {
                        continue;
                    }
                    let encoded = serde_json::to_string(&blocks).map_err(decode_in_tx)?;
                    let scope = text(&node, "scope").unwrap_or_default();
                    put_version(
                        tx,
                        SCOPE,
                        &scope,
                        Some(&node),
                        vec![("blocks", Value::from(encoded))],
                        at,
                    )?;
                }
                Ok(())
            })
            .await
            .map_err(|e| agent(&e))
    }

    /// [`set_limits`](Self::set_limits) from code that cannot wait: a new
    /// scope takes the sizes now, and a task resizes the stored scopes. A
    /// call outside a Tokio runtime resizes only the new scopes, and logs
    /// it.
    pub fn set_limits_soon(self: &Arc<Self>, limits: &[(&str, usize)]) {
        let owned: Vec<(String, usize)> = limits
            .iter()
            .map(|(label, size)| ((*label).to_owned(), *size))
            .collect();
        {
            let mut template = self.template.lock().unwrap_or_else(PoisonError::into_inner);
            for block in template.iter_mut() {
                if let Some((_, limit)) = owned.iter().find(|(label, _)| *label == block.label) {
                    block.limit_chars = *limit;
                }
            }
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!("no runtime: the stored memory blocks keep their sizes until a reload");
            return;
        };
        let store = Arc::clone(self);
        runtime.spawn(async move {
            let limits: Vec<(&str, usize)> = owned.iter().map(|(l, n)| (l.as_str(), *n)).collect();
            if let Err(error) = store.set_limits(&limits).await {
                tracing::warn!(%error, "the stored memory blocks did not resize");
            }
        });
    }

    /// Embeds `texts`. A failure embeds nothing: the node then waits for
    /// the next index pass, and the edit still lands.
    async fn embed_quietly(&self, texts: &[String]) -> Vec<Option<Vec<f32>>> {
        if texts.is_empty() {
            return Vec::new();
        }
        let dims = self.embedder.dimensions() as usize;
        match self.embedder.embed(texts).await {
            Ok(vectors) if vectors.len() == texts.len() => vectors
                .into_iter()
                .map(|v| (v.len() == dims).then_some(v))
                .collect(),
            Ok(_) => {
                tracing::warn!("the embedder answered another number of vectors");
                vec![None; texts.len()]
            }
            Err(error) => {
                tracing::warn!(%error, "the embedder failed; the index pass embeds later");
                vec![None; texts.len()]
            }
        }
    }

    /// Embeds each fact and episode that has no embedding, or every one
    /// with `all`. Returns how many it embedded.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read or write, or the
    /// embedder fails.
    pub async fn embed_nodes(&self, all: bool) -> Result<usize, AgentError> {
        let _writer = self.writer.lock().await;
        let mut done = 0;
        for label in [FACT, EPISODE] {
            let waiting: Vec<(u64, String)> = self
                .db
                .read(move |tx| {
                    Ok(find_all(tx, label, "kind", label)?
                        .into_iter()
                        .filter(|node| all || !node.properties.contains_key(EMBEDDING))
                        .filter_map(|node| text(&node, "text").map(|t| (node.id, t)))
                        .collect())
                })
                .await
                .map_err(|e| agent(&e))?;
            for batch in waiting.chunks(32) {
                let texts: Vec<String> = batch.iter().map(|(_, t)| t.clone()).collect();
                let vectors = self.embedder.embed(&texts).await?;
                let dims = self.embedder.dimensions() as usize;
                let pairs: Vec<(u64, Vec<f32>)> = batch
                    .iter()
                    .zip(vectors)
                    .filter(|(_, v)| v.len() == dims)
                    .map(|((id, _), v)| (*id, v))
                    .collect();
                done += pairs.len();
                self.db
                    .write(move |tx| {
                        for (id, v) in &pairs {
                            tx.update_node(*id, &patch(vec![(EMBEDDING, vector(v))]))?;
                        }
                        Ok(())
                    })
                    .await
                    .map_err(|e| agent(&e))?;
            }
        }
        Ok(done)
    }

    /// Applies one memory edit once for its key.
    async fn apply_now(
        &self,
        scope: &MemoryScope,
        op: &MemoryOp,
        key: &EditKey,
        labels: &BTreeSet<String>,
    ) -> Result<MemoryBlock, AgentError> {
        let _writer = self.writer.lock().await;
        let scope = scope.as_str().to_owned();
        let key_id = join(&[&scope, key.as_str()]);
        let (stored, before) = {
            let scope = scope.clone();
            let key_id = key_id.clone();
            self.db
                .read(move |tx| {
                    let stored = find_one(tx, EDIT_KEY, "id", &key_id)?.map(|n| text(&n, "error"));
                    Ok((stored, stored_blocks(tx, &scope)?))
                })
                .await
                .map_err(|e| agent(&e))?
        };
        let before = match before {
            Some(text) => serde_json::from_str(&text).map_err(undecodable)?,
            None => self.template(),
        };
        if let Some(stored) = stored {
            // An edit fails only by the rules of `apply_op`, and each such
            // error is a `Tool` error.
            return stored.map_or_else(
                || current(&before, op),
                |message| Err(AgentError::new(ErrorKind::Tool, message)),
            );
        }
        let mut after = before.clone();
        let result = autumn_harvest_agent::memory::apply_to(&mut after, op, labels);
        let changes = if result.is_ok() {
            changes(&before, &after)
        } else {
            Changes::default()
        };
        let texts: Vec<String> = changes.added.iter().map(|e| e.text.clone()).collect();
        let vectors = self.embed_quietly(&texts).await;
        let error = result.as_ref().err().map(|e| e.message().to_owned());
        let blocks = if result.is_ok() {
            Some(serde_json::to_string(&after).map_err(undecodable)?)
        } else {
            None
        };
        let key = key.as_str().to_owned();
        let replace = matches!(op, MemoryOp::Replace { .. });
        self.db
            .write(move |tx| {
                let at = now()?;
                let node = find_one(tx, SCOPE, "scope", &scope)?;
                let seq = node.as_ref().map_or(0, |node| uint(node, "next_key"));
                tx.create_node(
                    EDIT_KEY,
                    &props(
                        EDIT_KEY,
                        &scope,
                        vec![
                            ("id", Value::from(key_id)),
                            ("key", Value::from(key)),
                            ("error", error.map_or(Value::Null, Value::from)),
                            ("seq", Value::from(seq)),
                            ("slot", Value::from(join(&[&scope, &seq.to_string()]))),
                        ],
                    ),
                )?;
                if let Some(old) = seq.checked_sub(MAX_EDIT_KEYS) {
                    let slot = join(&[&scope, &old.to_string()]);
                    if let Some(old) = find_one(tx, EDIT_KEY, "slot", &slot)? {
                        tx.retract_node(old.id, at)?;
                    }
                }
                let mut update = vec![("next_key", Value::from(seq + 1))];
                if let Some(blocks) = blocks {
                    update.push(("blocks", Value::from(blocks)));
                }
                put_version(tx, SCOPE, &scope, node.as_ref(), update, at)?;
                write_facts(tx, &scope, &changes, &vectors, replace)
            })
            .await
            .map_err(|e| agent(&e))?;
        result
    }

    /// Runs `change` on the log of `scope` and writes what changed, in one
    /// transaction. Each new episode is embedded first.
    async fn change_log<T: Send + 'static>(
        &self,
        scope: &MemoryScope,
        change: impl FnOnce(&mut EpisodeLog) -> T,
    ) -> Result<T, AgentError> {
        let _writer = self.writer.lock().await;
        let scope = scope.as_str().to_owned();
        let old = self.read_log(&scope).await?;
        let mut log = old.parts.log().map_err(undecodable)?;
        let value = change(&mut log);
        let new = Parts::of(&log).map_err(undecodable)?;
        self.write_log(scope, old, new).await?;
        Ok(value)
    }

    /// The stored parts of the log of `scope`, with their node ids.
    async fn read_log(&self, scope: &str) -> Result<StoredLog, AgentError> {
        let scope = scope.to_owned();
        self.db
            .read(move |tx| read_log(tx, &scope))
            .await
            .map_err(|e| agent(&e))
    }

    /// Writes the change from `old` to `new`.
    async fn write_log(&self, scope: String, old: StoredLog, new: Parts) -> Result<(), AgentError> {
        let gone = align(&old.entries, &new.entries, |a, b| {
            a.episode_id == b.episode.id
        });
        let kept = old.entries.len() - gone;
        let cleared = new.generation != old.parts.generation;
        let added: Vec<Entry> = new.entries[kept..].to_vec();
        let texts: Vec<String> = added.iter().map(|e| e.episode.text.clone()).collect();
        let vectors = self.embed_quietly(&texts).await;
        self.db
            .write(move |tx| write_log(tx, &scope, &old, &new, &vectors, cleared))
            .await
            .map_err(|e| agent(&e))
    }
}

/// The blocks that `scope` stores, as JSON, or `None` for a new scope.
fn stored_blocks(tx: &impl Reads, scope: &str) -> Result<Option<String>, AletheiaDbError> {
    Ok(find_one(tx, SCOPE, "scope", scope)?.and_then(|node| text(&node, "blocks")))
}

/// Writes a new version of a node that changes often: the fields of `old`
/// with `changes` on top, in a new node. The old node ends at `at`.
///
/// AletheiaDB keeps at most 1 000 versions of one node. A scope node or a
/// log node changes at each edit, so an update in place would fill that.
/// A new node per version keeps each node at two versions.
fn put_version(
    tx: &mut WriteTx<'_>,
    label: &str,
    scope: &str,
    old: Option<&NodeView>,
    changes: Vec<(&str, Value)>,
    at: AletheiaTime,
) -> Result<u64, AletheiaDbError> {
    let mut fields = old.map(|node| node.properties.clone()).unwrap_or_default();
    for (key, value) in changes {
        fields.insert(key.to_owned(), value);
    }
    fields.insert("kind".to_owned(), Value::from(label));
    fields.insert("scope".to_owned(), Value::from(scope));
    if let Some(old) = old {
        tx.retract_node(old.id, at)?;
    }
    tx.create_node(label, &fields)
}

/// The block that `op` names, unchanged.
fn current(blocks: &[MemoryBlock], op: &MemoryOp) -> Result<MemoryBlock, AgentError> {
    blocks
        .iter()
        .find(|block| block.label == op.block())
        .cloned()
        .ok_or_else(|| {
            AgentError::new(ErrorKind::Tool, format!("no memory block {:?}", op.block()))
        })
}

/// One memory entry, with its block and its flow labels.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FactEntry {
    block: String,
    text: String,
    labels: Vec<String>,
}

/// The entries that an edit removed and added.
#[derive(Debug, Clone, Default)]
struct Changes {
    removed: Vec<FactEntry>,
    added: Vec<FactEntry>,
}

/// Every entry of `blocks`.
fn entries(blocks: &[MemoryBlock]) -> Vec<FactEntry> {
    blocks
        .iter()
        .flat_map(|block| {
            block.entries.iter().map(|text| FactEntry {
                block: block.label.clone(),
                text: text.clone(),
                labels: block
                    .labels
                    .get(text)
                    .map(|set| set.iter().cloned().collect())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// The entries in `before` and not in `after`, and the other way round.
/// Each is a multiset difference, so a repeated entry counts each time.
fn changes(before: &[MemoryBlock], after: &[MemoryBlock]) -> Changes {
    let mut removed = entries(before);
    let mut added = Vec::new();
    for entry in entries(after) {
        if let Some(at) = removed.iter().position(|old| *old == entry) {
            removed.remove(at);
        } else {
            added.push(entry);
        }
    }
    Changes { removed, added }
}

/// Ends the fact of each removed entry and creates one for each added
/// entry. A replace links its new fact to the old one.
///
/// An ended fact keeps its node, with the time it ended in `ended_ms`. So
/// the vector index keeps it, and a recall as of a past time finds it. Its
/// `live` key goes, so no later edit finds it as an entry of the block.
fn write_facts(
    tx: &mut WriteTx<'_>,
    scope: &str,
    changes: &Changes,
    vectors: &[Option<Vec<f32>>],
    replace: bool,
) -> Result<(), AletheiaDbError> {
    let mut gone = Vec::new();
    let ended_ms = now_ms();
    for entry in &changes.removed {
        let id = join(&[scope, &entry.block, &entry.text]);
        if let Some(node) = find_one(tx, FACT, "live", &id)? {
            tx.update_node(
                node.id,
                &patch(vec![
                    ("live", Value::Null),
                    ("ended_ms", Value::from(ended_ms)),
                ]),
            )?;
            gone.push(node.id);
        }
    }
    let replaces = (replace && gone.len() == 1 && changes.added.len() == 1)
        .then(|| gone[0])
        .map_or(Value::Null, Value::from);
    let added_ms = now_ms();
    for (n, entry) in changes.added.iter().enumerate() {
        let mut more = vec![
            ("block", Value::from(entry.block.clone())),
            ("text", Value::from(entry.text.clone())),
            (
                "labels",
                Value::from(serde_json::to_string(&entry.labels).map_err(decode_in_tx)?),
            ),
            (
                "live",
                Value::from(join(&[scope, &entry.block, &entry.text])),
            ),
            ("added_ms", Value::from(added_ms)),
            ("ended_ms", Value::Null),
            ("replaces", replaces.clone()),
        ];
        if let Some(Some(v)) = vectors.get(n) {
            more.push((EMBEDDING, vector(v)));
        }
        tx.create_node(FACT, &props(FACT, scope, more))?;
    }
    Ok(())
}

/// One stored place in the log.
#[derive(Debug, Clone)]
struct StoredEntry {
    node: u64,
    episode_id: String,
    consolidated: bool,
}

/// One stored string of the log: a key or a hash.
#[derive(Debug, Clone)]
struct StoredItem {
    node: u64,
    value: String,
}

/// The stored log of one scope.
#[derive(Debug, Clone, Default)]
struct StoredLog {
    log_node: Option<u64>,
    next: u64,
    parts: Parts,
    entries: Vec<StoredEntry>,
    keys: Vec<StoredItem>,
    derived: Vec<StoredItem>,
}

/// The nodes of `label` in `scope`, oldest place first.
fn by_seq(tx: &impl Reads, label: &str, scope: &str) -> Result<Vec<NodeView>, AletheiaDbError> {
    let mut nodes = find_all(tx, label, "scope", scope)?;
    nodes.sort_by_key(|node| int(node, "seq"));
    Ok(nodes)
}

/// Reads the stored log of `scope`.
fn read_log(tx: &impl Reads, scope: &str) -> Result<StoredLog, AletheiaDbError> {
    let mut stored = StoredLog::default();
    if let Some(node) = find_one(tx, LOG, "scope", scope)? {
        stored.log_node = Some(node.id);
        stored.next = uint(&node, "next");
        stored.parts.generation = uint(&node, "generation");
        stored.parts.dropped = uint(&node, "dropped");
    }
    for node in by_seq(tx, LOG_ENTRY, scope)? {
        let episode_node = tx.node(uint(&node, "episode"))?;
        let body = text(&episode_node, "body").unwrap_or_default();
        let episode: Episode = serde_json::from_str(&body).map_err(decode_in_tx)?;
        let consolidated = flag(&node, "consolidated");
        stored.entries.push(StoredEntry {
            node: node.id,
            episode_id: episode.id.clone(),
            consolidated,
        });
        stored.parts.entries.push(Entry {
            episode,
            consolidated,
        });
    }
    for (label, property, items, values) in [
        (LOG_KEY, "key", &mut stored.keys, &mut stored.parts.keys),
        (
            DERIVED,
            "hash",
            &mut stored.derived,
            &mut stored.parts.derived,
        ),
    ] {
        for node in by_seq(tx, label, scope)? {
            let value = text(&node, property).unwrap_or_default();
            values.push(value.clone());
            items.push(StoredItem {
                node: node.id,
                value,
            });
        }
    }
    Ok(stored)
}

/// Writes the change from `old` to `new` in one transaction.
fn write_log(
    tx: &mut WriteTx<'_>,
    scope: &str,
    old: &StoredLog,
    new: &Parts,
    vectors: &[Option<Vec<f32>>],
    cleared: bool,
) -> Result<(), AletheiaDbError> {
    let at = now()?;
    let mut next = old.next;
    if cleared {
        // A clear forgets the episodes too, not only their places.
        retract_all(tx, &find_all(tx, EPISODE, "scope", scope)?, at)?;
    }
    // The places in the log.
    let gone = align(&old.entries, &new.entries, |a, b| {
        a.episode_id == b.episode.id
    });
    for entry in &old.entries[..gone] {
        tx.retract_node(entry.node, at)?;
    }
    let kept = old.entries.len() - gone;
    for (stored, entry) in old.entries[gone..].iter().zip(&new.entries) {
        if stored.consolidated != entry.consolidated {
            tx.update_node(
                stored.node,
                &patch(vec![("consolidated", Value::Bool(entry.consolidated))]),
            )?;
        }
    }
    for (n, entry) in new.entries[kept..].iter().enumerate() {
        let episode = episode_node(tx, scope, &entry.episode, vectors.get(n))?;
        tx.create_node(
            LOG_ENTRY,
            &props(
                LOG_ENTRY,
                scope,
                vec![
                    ("episode_id", Value::from(entry.episode.id.clone())),
                    ("episode", Value::from(episode)),
                    ("consolidated", Value::Bool(entry.consolidated)),
                    ("seq", Value::from(next)),
                ],
            ),
        )?;
        next += 1;
    }
    // The keys and the hashes.
    for (label, property, stored, values) in [
        (LOG_KEY, "key", &old.keys, &new.keys),
        (DERIVED, "hash", &old.derived, &new.derived),
    ] {
        let gone = align(stored, values, |a, b| a.value == *b);
        for item in &stored[..gone] {
            tx.retract_node(item.node, at)?;
        }
        for value in &values[stored.len() - gone..] {
            tx.create_node(
                label,
                &props(
                    label,
                    scope,
                    vec![
                        (property, Value::from(value.clone())),
                        ("seq", Value::from(next)),
                    ],
                ),
            )?;
            next += 1;
        }
    }
    // The scalars.
    let scalars = vec![
        ("generation", Value::from(new.generation)),
        ("dropped", Value::from(new.dropped)),
        ("next", Value::from(next)),
    ];
    let node = old.log_node.map(|id| tx.get_node(id)).transpose()?;
    put_version(tx, LOG, scope, node.as_ref(), scalars, at)?;
    Ok(())
}

/// The node of `episode` in `scope`: the one that exists, or a new one.
fn episode_node(
    tx: &mut WriteTx<'_>,
    scope: &str,
    episode: &Episode,
    embedding: Option<&Option<Vec<f32>>>,
) -> Result<u64, AletheiaDbError> {
    let id = join(&[scope, &episode.id]);
    if let Some(node) = find_one(tx, EPISODE, "eid", &id)? {
        return Ok(node.id);
    }
    let body = serde_json::to_string(episode).map_err(decode_in_tx)?;
    let mut more = vec![
        ("eid", Value::from(id)),
        ("episode_id", Value::from(episode.id.clone())),
        ("text", Value::from(episode.text.clone())),
        ("at_ms", Value::from(episode.at_ms)),
        ("body", Value::from(body)),
    ];
    if let Some(Some(v)) = embedding {
        more.push((EMBEDDING, vector(v)));
    }
    tx.create_node(EPISODE, &props(EPISODE, scope, more))
}

impl MemoryStore for AletheiaStores {
    fn load<'a>(
        &'a self,
        scope: &'a MemoryScope,
    ) -> BoxFuture<'a, Result<Vec<MemoryBlock>, AgentError>> {
        Box::pin(async move {
            let scope = scope.as_str().to_owned();
            let stored = self
                .db
                .read(move |tx| stored_blocks(tx, &scope))
                .await
                .map_err(|e| agent(&e))?;
            stored.map_or_else(
                || Ok(self.template()),
                |text| serde_json::from_str(&text).map_err(undecodable),
            )
        })
    }

    fn apply<'a>(
        &'a self,
        scope: &'a MemoryScope,
        op: MemoryOp,
        key: &'a EditKey,
    ) -> BoxFuture<'a, Result<MemoryBlock, AgentError>> {
        Box::pin(async move { self.apply_now(scope, &op, key, &BTreeSet::new()).await })
    }

    fn apply_labelled<'a>(
        &'a self,
        scope: &'a MemoryScope,
        op: MemoryOp,
        key: &'a EditKey,
        labels: &'a BTreeSet<String>,
    ) -> BoxFuture<'a, Result<MemoryBlock, AgentError>> {
        Box::pin(async move { self.apply_now(scope, &op, key, labels).await })
    }

    fn clear<'a>(&'a self, scope: &'a MemoryScope) -> BoxFuture<'a, Result<(), AgentError>> {
        Box::pin(async move {
            let _writer = self.writer.lock().await;
            let scope = scope.as_str().to_owned();
            self.db
                .write(move |tx| {
                    let at = now()?;
                    for label in [FACT, EDIT_KEY, SCOPE] {
                        retract_all(tx, &find_all(tx, label, "scope", &scope)?, at)?;
                    }
                    Ok(())
                })
                .await
                .map_err(|e| agent(&e))
        })
    }
}

impl EpisodeStore for AletheiaStores {
    fn load<'a>(&'a self, scope: &'a MemoryScope) -> BoxFuture<'a, Result<EpisodeLog, AgentError>> {
        Box::pin(async move {
            let stored = self.read_log(scope.as_str()).await?;
            stored.parts.log().map_err(undecodable)
        })
    }

    fn append<'a>(
        &'a self,
        scope: &'a MemoryScope,
        key: &'a EditKey,
        episode: Episode,
        max_episodes: u32,
    ) -> BoxFuture<'a, Result<Appended, AgentError>> {
        Box::pin(async move {
            self.change_log(scope, |log| log.append(key, episode, max_episodes))
                .await
        })
    }

    fn mark<'a>(
        &'a self,
        scope: &'a MemoryScope,
        mark: PassMark,
    ) -> BoxFuture<'a, Result<bool, AgentError>> {
        Box::pin(async move { self.change_log(scope, |log| log.mark(&mark)).await })
    }

    fn clear<'a>(
        &'a self,
        scope: &'a MemoryScope,
        key: &'a EditKey,
    ) -> BoxFuture<'a, Result<Cleared, AgentError>> {
        Box::pin(async move {
            let _writer = self.writer.lock().await;
            let name = scope.as_str().to_owned();
            let last = {
                let name = name.clone();
                self.db
                    .read(move |tx| find_one(tx, CLEAR, "scope", &name))
                    .await
                    .map_err(|e| agent(&e))?
            };
            if let Some(last) = &last
                && text(last, "key").as_deref() == Some(key.as_str())
            {
                let cleared = text(last, "cleared").unwrap_or_default();
                return serde_json::from_str(&cleared).map_err(undecodable);
            }
            let old = self.read_log(&name).await?;
            let mut log = old.parts.log().map_err(undecodable)?;
            let cleared = log.clear();
            let new = Parts::of(&log).map_err(undecodable)?;
            let encoded = serde_json::to_string(&cleared).map_err(undecodable)?;
            let key = key.as_str().to_owned();
            let last = last.map(|node| node.id);
            self.db
                .write(move |tx| {
                    write_log(tx, &name, &old, &new, &[], true)?;
                    let fields = vec![("key", Value::from(key)), ("cleared", Value::from(encoded))];
                    let at = now()?;
                    let node = last.map(|id| tx.get_node(id)).transpose()?;
                    put_version(tx, CLEAR, &name, node.as_ref(), fields, at).map(drop)
                })
                .await
                .map_err(|e| agent(&e))?;
            Ok(cleared)
        })
    }
}

/// The memory rows of a `memory.db`, for the move into a new directory.
#[derive(Debug, Clone, Default)]
pub struct MemoryRows {
    /// Each scope and its blocks.
    pub scopes: Vec<(String, Vec<MemoryBlock>)>,
    /// Each edit key, oldest first: the scope, the key and the error.
    pub edits: Vec<(String, String, Option<String>)>,
    /// Each scope and its episode log.
    pub logs: Vec<(String, EpisodeLog)>,
    /// Each scope, the key of its last clear and what it cleared.
    pub clears: Vec<(String, String, Cleared)>,
}

impl MemoryRows {
    /// Whether no row is there.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.scopes.is_empty()
            && self.edits.is_empty()
            && self.logs.is_empty()
            && self.clears.is_empty()
    }
}

impl AletheiaStores {
    /// Whether the directory took the rows of a `memory.db` already, or
    /// holds a memory of its own.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read.
    pub async fn imported(&self) -> Result<bool, AgentError> {
        self.db
            .read(|tx| {
                let meta = find_one(tx, META, "kind", META)?;
                let done = meta.is_some_and(|meta| flag(&meta, "imported"));
                let scopes = !tx.find(SCOPE, "kind", &Value::from(SCOPE))?.is_empty();
                let logs = !tx.find(LOG, "kind", &Value::from(LOG))?.is_empty();
                Ok(done || scopes || logs)
            })
            .await
            .map_err(|e| agent(&e))
    }

    /// Moves `rows` into the store once. Returns `false` and writes
    /// nothing when [`imported`](Self::imported) holds. The facts and the
    /// episodes wait for the index pass for their embeddings.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot read or write.
    pub async fn import(&self, rows: MemoryRows) -> Result<bool, AgentError> {
        if self.imported().await? {
            return Ok(false);
        }
        let writer = self.writer.lock().await;
        let mut logs = Vec::new();
        for (scope, log) in &rows.logs {
            logs.push((scope.clone(), Parts::of(log).map_err(undecodable)?));
        }
        let mut clears = Vec::new();
        for (scope, key, cleared) in &rows.clears {
            let encoded = serde_json::to_string(cleared).map_err(undecodable)?;
            clears.push((scope.clone(), key.clone(), encoded));
        }
        let mut scopes = Vec::new();
        for (scope, blocks) in &rows.scopes {
            let encoded = serde_json::to_string(blocks).map_err(undecodable)?;
            scopes.push((scope.clone(), encoded, blocks.clone()));
        }
        let edits = rows.edits;
        self.db
            .write(move |tx| {
                let at = now()?;
                let mut next: std::collections::BTreeMap<String, u64> =
                    std::collections::BTreeMap::new();
                for (scope, key, error) in &edits {
                    let seq = next.entry(scope.clone()).or_insert(0);
                    tx.create_node(
                        EDIT_KEY,
                        &props(
                            EDIT_KEY,
                            scope,
                            vec![
                                ("id", Value::from(join(&[scope, key]))),
                                ("key", Value::from(key.clone())),
                                ("error", error.clone().map_or(Value::Null, Value::from)),
                                ("seq", Value::from(*seq)),
                                ("slot", Value::from(join(&[scope, &seq.to_string()]))),
                            ],
                        ),
                    )?;
                    *seq += 1;
                }
                let mut stored: std::collections::BTreeMap<String, Value> = next
                    .keys()
                    .map(|scope| (scope.clone(), Value::Null))
                    .collect();
                for (scope, encoded, blocks) in &scopes {
                    stored.insert(scope.clone(), Value::from(encoded.clone()));
                    let added = Changes {
                        removed: Vec::new(),
                        added: entries(blocks),
                    };
                    write_facts(tx, scope, &added, &[], false)?;
                }
                for (scope, blocks) in stored {
                    let seq = next.get(&scope).copied().unwrap_or(0);
                    put_version(
                        tx,
                        SCOPE,
                        &scope,
                        None,
                        vec![("blocks", blocks), ("next_key", Value::from(seq))],
                        at,
                    )?;
                }
                for (scope, parts) in &logs {
                    write_log(tx, scope, &StoredLog::default(), parts, &[], false)?;
                }
                for (scope, key, encoded) in &clears {
                    tx.create_node(
                        CLEAR,
                        &props(
                            CLEAR,
                            scope,
                            vec![
                                ("key", Value::from(key.clone())),
                                ("cleared", Value::from(encoded.clone())),
                            ],
                        ),
                    )?;
                }
                if let Some(meta) = find_one(tx, META, "kind", META)? {
                    tx.update_node(meta.id, &patch(vec![("imported", Value::Bool(true))]))?;
                }
                Ok(())
            })
            .await
            .map_err(|e| agent(&e))?;
        drop(writer);
        self.embed_nodes(false).await?;
        Ok(true)
    }

    /// Records that the directory needs no move, for a start that finds no
    /// `memory.db`.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub async fn mark_imported(&self) -> Result<(), AgentError> {
        let _writer = self.writer.lock().await;
        self.db
            .write(|tx| {
                if let Some(meta) = find_one(tx, META, "kind", META)? {
                    tx.update_node(meta.id, &patch(vec![("imported", Value::Bool(true))]))?;
                }
                Ok(())
            })
            .await
            .map_err(|e| agent(&e))
    }
}

#[cfg(test)]
mod tests;
