//! Recall: find facts and episodes by meaning, now or as of a time.
//!
//! [`AletheiaStores::recall`] embeds the query and asks the vector index
//! for the closest nodes. The index holds every fact and episode that has
//! an embedding, the ended facts too. The search asks for more hits than
//! it needs, then keeps those of the right scope, kind and time. When the
//! filter leaves too few, it asks again for twice as many.
//!
//! A time filter reads the times that each node keeps: `added_ms` and
//! `ended_ms` of a fact, `at_ms` of an episode. The as-of search of
//! AletheiaDB needs a temporal vector index. That index takes snapshots,
//! and a rebuild at a reopen skips it, so the store does not use it.

use autumn_harvest_agent::memory::MemoryScope;
use autumn_harvest_agent::{AgentError, ErrorKind};
use autumn_plugin_aletheiadb::{NodeView, SemanticQuery};
use serde_json::Value;

use super::graph::{EPISODE, FACT, int, text};
use super::{AletheiaStores, agent};

/// The most hits that one recall returns.
pub const MAX_RECALL: usize = 50;

/// What a recall looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallKind {
    /// Memory entries, the ones that the blocks hold now and the ones
    /// that an edit replaced or removed.
    Fact,
    /// Episodes: what the assistant answered at the end of an op.
    Episode,
}

impl RecallKind {
    /// The node label of the kind.
    const fn label(self) -> &'static str {
        match self {
            Self::Fact => FACT,
            Self::Episode => EPISODE,
        }
    }

    /// The kind of a node label.
    fn of(label: &str) -> Option<Self> {
        match label {
            FACT => Some(Self::Fact),
            EPISODE => Some(Self::Episode),
            _ => None,
        }
    }
}

/// One recall query.
#[derive(Debug, Clone)]
pub struct Recall {
    /// The memory scope of the facts, `agent:<id>` in the engine.
    pub facts: MemoryScope,
    /// The scope of the episode log, `agent:<id>:episodes` in the engine.
    pub episodes: MemoryScope,
    /// The words to search by.
    pub query: String,
    /// The most hits, from 1 to [`MAX_RECALL`].
    pub limit: usize,
    /// The kinds to return. Empty means both.
    pub kinds: Vec<RecallKind>,
    /// Search the memory as it was at this time, in epoch milliseconds.
    pub as_of_ms: Option<i64>,
}

/// One hit of a recall.
#[derive(Debug, Clone, PartialEq)]
pub struct Recalled {
    /// What the hit is.
    pub kind: RecallKind,
    /// The text of the entry or the episode.
    pub text: String,
    /// The block of a fact.
    pub block: Option<String>,
    /// The similarity, higher is closer.
    pub score: f32,
    /// When the entry was added, or when the op of the episode ended, in
    /// epoch milliseconds.
    pub at_ms: i64,
    /// Whether the hit is still in the memory now. A fact that an edit
    /// replaced or removed, or an episode that a clear removed, is not.
    pub current: bool,
}

impl AletheiaStores {
    /// The facts and episodes of the scopes of `recall` closest to
    /// `recall.query`.
    ///
    /// # Errors
    ///
    /// Returns an error when the query is empty, the embedder fails, or
    /// the store cannot read.
    pub async fn recall(&self, recall: &Recall) -> Result<Vec<Recalled>, AgentError> {
        let query = recall.query.trim();
        if query.is_empty() {
            return Err(AgentError::new(
                ErrorKind::Tool,
                "recall needs a query: the words to search by",
            ));
        }
        let limit = recall.limit.clamp(1, MAX_RECALL);
        let kinds = if recall.kinds.is_empty() {
            vec![RecallKind::Fact, RecallKind::Episode]
        } else {
            recall.kinds.clone()
        };
        let vectors = self.embedder.embed(&[query.to_owned()]).await?;
        let Some(embedding) = vectors.into_iter().next() else {
            return Err(AgentError::new(
                ErrorKind::Unavailable,
                "the embedder answered no vector",
            ));
        };
        let label = match kinds.as_slice() {
            [kind] => Some(kind.label()),
            _ => None,
        };
        // Other scopes, kinds and times share the index, so ask for more.
        let mut want = limit * 4 + 16;
        loop {
            let mut search = SemanticQuery::new(embedding.clone()).with_k(want);
            if let Some(label) = label {
                search = search.with_label(label);
            }
            let hits = self
                .db
                .semantic_search(&search)
                .await
                .map_err(|e| agent(&e))?;
            let exhausted = hits.len() < want;
            let found: Vec<Recalled> = hits
                .into_iter()
                .filter_map(|hit| keep(&hit.node, hit.score, recall, &kinds))
                .take(limit)
                .collect();
            if found.len() == limit || exhausted || want >= MAX_SEARCH {
                return Ok(found);
            }
            want = (want * 2).min(MAX_SEARCH);
        }
    }
}

/// The most nodes that one search asks the index for.
const MAX_SEARCH: usize = 4_096;

/// The hit that `node` gives `recall`, or `None` when its scope, its kind
/// or its time does not fit.
fn keep(node: &NodeView, score: f32, recall: &Recall, kinds: &[RecallKind]) -> Option<Recalled> {
    let kind = RecallKind::of(&node.label)?;
    let wanted = match kind {
        RecallKind::Fact => &recall.facts,
        RecallKind::Episode => &recall.episodes,
    };
    if !kinds.contains(&kind) || text(node, "scope").as_deref() != Some(wanted.as_str()) {
        return None;
    }
    let ended = node.properties.get("ended_ms").and_then(Value::as_i64);
    let at_ms = match kind {
        RecallKind::Fact => int(node, "added_ms"),
        RecallKind::Episode => int(node, "at_ms"),
    };
    let fits = recall.as_of_ms.map_or_else(
        || ended.is_none(),
        |then| at_ms <= then && ended.is_none_or(|end| end > then),
    );
    fits.then(|| Recalled {
        kind,
        text: text(node, "text").unwrap_or_default(),
        block: text(node, "block"),
        score,
        at_ms,
        current: ended.is_none(),
    })
}
