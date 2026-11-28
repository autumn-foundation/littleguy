//! The node layout of the AletheiaDB store (ADR 0047), and small helpers
//! that read and write it.
//!
//! Every node has two plain properties: `kind`, which equals its label,
//! and `scope`. So a lookup by `kind` lists every node of a label, and a
//! lookup by `scope` lists the nodes of one scope.
//!
//! A lookup key joins its parts with [`SEP`], a unit separator. No scope,
//! block or key that the engine makes holds that character.

use autumn_plugin_aletheiadb::{AletheiaDbError, NodeView, ReadTx, WriteTx};
use serde_json::{Map, Value};

/// The label of the node that holds the blocks of one scope.
pub const SCOPE: &str = "MemoryScope";
/// The label of one version of one memory entry.
pub const FACT: &str = "Fact";
/// The label of one memory edit key.
pub const EDIT_KEY: &str = "EditKey";
/// The label of the node that holds the scalars of one episode log.
pub const LOG: &str = "EpisodeLog";
/// The label of one episode. It stays when the log evicts its episode.
pub const EPISODE: &str = "Episode";
/// The label of one place in an episode log.
pub const LOG_ENTRY: &str = "LogEntry";
/// The label of one applied key of an episode log.
pub const LOG_KEY: &str = "LogKey";
/// The label of one derived fact hash of an episode log.
pub const DERIVED: &str = "Derived";
/// The label of the last episode clear of one scope.
pub const CLEAR: &str = "EpisodeClear";
/// The label of the one node that describes the directory.
pub const META: &str = "Meta";

/// The property that carries the embedding of a fact or an episode.
pub const EMBEDDING: &str = "embedding";

/// The separator of the parts of a lookup key.
pub const SEP: char = '\u{1f}';

/// Joins the parts of a lookup key.
#[must_use]
pub fn join(parts: &[&str]) -> String {
    let mut key = String::new();
    for (n, part) in parts.iter().enumerate() {
        if n > 0 {
            key.push(SEP);
        }
        key.push_str(part);
    }
    key
}

/// The reads that a write transaction and a read transaction share.
pub trait Reads {
    /// The node `id` as the transaction sees it.
    fn node(&self, id: u64) -> Result<NodeView, AletheiaDbError>;
    /// The ids of the nodes with `label` whose `property` equals `value`,
    /// in id order.
    fn find(&self, label: &str, property: &str, value: &Value)
    -> Result<Vec<u64>, AletheiaDbError>;
}

impl Reads for WriteTx<'_> {
    fn node(&self, id: u64) -> Result<NodeView, AletheiaDbError> {
        self.get_node(id)
    }

    fn find(
        &self,
        label: &str,
        property: &str,
        value: &Value,
    ) -> Result<Vec<u64>, AletheiaDbError> {
        self.find_nodes(label, property, value)
    }
}

impl Reads for ReadTx<'_> {
    fn node(&self, id: u64) -> Result<NodeView, AletheiaDbError> {
        self.get_node(id)
    }

    fn find(
        &self,
        label: &str,
        property: &str,
        value: &Value,
    ) -> Result<Vec<u64>, AletheiaDbError> {
        self.find_nodes(label, property, value)
    }
}

/// The first node with `label` whose `property` equals `value`.
pub fn find_one(
    tx: &impl Reads,
    label: &str,
    property: &str,
    value: &str,
) -> Result<Option<NodeView>, AletheiaDbError> {
    let ids = tx.find(label, property, &Value::from(value))?;
    ids.first().map(|id| tx.node(*id)).transpose()
}

/// Every node with `label` whose `property` equals `value`, in id order.
pub fn find_all(
    tx: &impl Reads,
    label: &str,
    property: &str,
    value: &str,
) -> Result<Vec<NodeView>, AletheiaDbError> {
    tx.find(label, property, &Value::from(value))?
        .into_iter()
        .map(|id| tx.node(id))
        .collect()
}

/// The properties of a new node: `kind`, `scope`, then `more`.
#[must_use]
pub fn props(label: &str, scope: &str, more: Vec<(&str, Value)>) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert("kind".to_owned(), Value::from(label));
    map.insert("scope".to_owned(), Value::from(scope));
    for (key, value) in more {
        map.insert(key.to_owned(), value);
    }
    map
}

/// The properties of an update.
#[must_use]
pub fn patch(more: Vec<(&str, Value)>) -> Map<String, Value> {
    more.into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// A string property, or `None`.
#[must_use]
pub fn text(node: &NodeView, property: &str) -> Option<String> {
    node.properties
        .get(property)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// An integer property, or 0.
#[must_use]
pub fn int(node: &NodeView, property: &str) -> i64 {
    node.properties
        .get(property)
        .and_then(Value::as_i64)
        .unwrap_or(0)
}

/// An unsigned integer property, or 0.
#[must_use]
pub fn uint(node: &NodeView, property: &str) -> u64 {
    u64::try_from(int(node, property)).unwrap_or(0)
}

/// A boolean property, or `false`.
#[must_use]
pub fn flag(node: &NodeView, property: &str) -> bool {
    node.properties
        .get(property)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// A vector as a JSON array, the form the plugin stores as a vector.
#[must_use]
pub fn vector(values: &[f32]) -> Value {
    Value::Array(
        values
            .iter()
            .map(|x| {
                serde_json::Number::from_f64(f64::from(*x))
                    .map_or_else(|| Value::from(0), Value::Number)
            })
            .collect(),
    )
}

/// Ends the valid time of each node in `nodes` at `at`.
pub fn retract_all(
    tx: &mut WriteTx<'_>,
    nodes: &[NodeView],
    at: autumn_plugin_aletheiadb::AletheiaTime,
) -> Result<(), AletheiaDbError> {
    for node in nodes {
        tx.retract_node(node.id, at)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_joins_its_parts_with_the_separator() {
        assert_eq!(join(&["a", "b", "c"]), "a\u{1f}b\u{1f}c");
        assert_eq!(join(&["only"]), "only");
        assert_eq!(join(&[]), "");
    }

    #[test]
    fn new_properties_carry_the_kind_and_the_scope() {
        let map = props(FACT, "agent", vec![("text", Value::from("tea"))]);
        assert_eq!(map["kind"], Value::from(FACT));
        assert_eq!(map["scope"], Value::from("agent"));
        assert_eq!(map["text"], Value::from("tea"));
    }

    #[test]
    fn a_missing_property_reads_as_its_default() {
        let node = NodeView {
            id: 1,
            label: FACT.to_owned(),
            properties: Map::new(),
        };
        assert_eq!(text(&node, "text"), None);
        assert_eq!(int(&node, "seq"), 0);
        assert_eq!(uint(&node, "seq"), 0);
        assert!(!flag(&node, "consolidated"));
    }

    #[test]
    fn a_vector_becomes_a_number_array() {
        assert_eq!(vector(&[0.5, -1.0]), serde_json::json!([0.5, -1.0]));
        assert_eq!(vector(&[f32::NAN]), serde_json::json!([0]));
    }
}
