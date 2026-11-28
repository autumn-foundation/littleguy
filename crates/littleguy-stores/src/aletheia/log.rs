//! The episode log, split into nodes (ADR 0047).
//!
//! The engine keeps an [`EpisodeLog`] as one value with private fields.
//! The store reads its serde form, and keeps each part apart:
//!
//! - the scalars `generation` and `dropped` on one `EpisodeLog` node;
//! - one `LogEntry` node for each episode in the log, which points at its
//!   `Episode` node;
//! - one `LogKey` node for each applied key;
//! - one `Derived` node for each derived fact hash.
//!
//! A change of the log only pops from the front and pushes to the back of
//! each list, and it flips the consolidated flag in place. [`align`] finds
//! how many items left the front. So a change writes only what changed,
//! and never the whole log again.

use serde_json::{Map, Value};

use autumn_harvest_agent::episode::{Episode, EpisodeLog};

/// One episode in the log, as its serde form gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// The episode, with its id.
    pub episode: Episode,
    /// Whether a pass turned it into facts.
    pub consolidated: bool,
}

/// The parts of one log.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Parts {
    /// The number of clears of the scope.
    pub generation: u64,
    /// The unconsolidated episodes that an eviction removed.
    pub dropped: u64,
    /// The episodes, oldest first.
    pub entries: Vec<Entry>,
    /// The fact hashes, oldest first.
    pub derived: Vec<String>,
    /// The applied keys, oldest first.
    pub keys: Vec<String>,
}

/// A part of the log that does not decode.
#[derive(Debug, thiserror::Error)]
#[error("the episode log does not decode: {0}")]
pub struct Undecodable(String);

impl Parts {
    /// The parts of `log`.
    ///
    /// # Errors
    ///
    /// Returns [`Undecodable`] when the serde form of the log is not the
    /// form that this module reads.
    pub fn of(log: &EpisodeLog) -> Result<Self, Undecodable> {
        let value = serde_json::to_value(log).map_err(|e| Undecodable(e.to_string()))?;
        let Value::Object(map) = value else {
            return Err(Undecodable("the log is not an object".to_owned()));
        };
        let number = |key: &str| map.get(key).and_then(Value::as_u64).unwrap_or(0);
        let strings = |key: &str| -> Vec<String> {
            map.get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default()
        };
        let entries = map
            .get("episodes")
            .and_then(Value::as_array)
            .map(|items| items.iter().map(entry).collect::<Result<Vec<_>, _>>())
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            generation: number("generation"),
            dropped: number("dropped"),
            entries,
            derived: strings("derived"),
            keys: strings("keys"),
        })
    }

    /// The log that these parts make.
    ///
    /// # Errors
    ///
    /// Returns [`Undecodable`] when the engine refuses the form.
    pub fn log(&self) -> Result<EpisodeLog, Undecodable> {
        let episodes = self
            .entries
            .iter()
            .map(|entry| {
                let mut object = match serde_json::to_value(&entry.episode) {
                    Ok(Value::Object(object)) => object,
                    Ok(_) => Map::new(),
                    Err(e) => return Err(Undecodable(e.to_string())),
                };
                object.insert("consolidated".to_owned(), Value::Bool(entry.consolidated));
                Ok(Value::Object(object))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let value = serde_json::json!({
            "generation": self.generation,
            "episodes": episodes,
            "dropped": self.dropped,
            "derived": self.derived,
            "keys": self.keys,
        });
        serde_json::from_value(value).map_err(|e| Undecodable(e.to_string()))
    }
}

/// One episode of the serde form of the log.
fn entry(value: &Value) -> Result<Entry, Undecodable> {
    let consolidated = value
        .get("consolidated")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut object = value.clone();
    if let Value::Object(map) = &mut object {
        map.remove("consolidated");
    }
    let episode: Episode =
        serde_json::from_value(object).map_err(|e| Undecodable(e.to_string()))?;
    Ok(Entry {
        episode,
        consolidated,
    })
}

/// How many items left the front of `old`, so that what is left of `old`
/// starts `new`.
///
/// The answer always exists: when nothing of `old` starts `new`, every
/// item left. The items of `new` after what is left of `old` are new. So
/// the store keeps `old[n..]`, adds `new[old.len() - n..]`, and ends with
/// `new` in every case.
#[must_use]
pub fn align<T, U>(old: &[T], new: &[U], same: impl Fn(&T, &U) -> bool) -> usize {
    (0..=old.len())
        .find(|&n| {
            let rest = &old[n..];
            rest.len() <= new.len() && rest.iter().zip(new).all(|(a, b)| same(a, b))
        })
        .unwrap_or(old.len())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use autumn_harvest_agent::AgentStop;
    use autumn_harvest_agent::episode::PassMark;
    use autumn_harvest_agent::memory::EditKey;
    use proptest::prelude::*;

    use super::*;

    fn episode(id: &str) -> Episode {
        Episode {
            id: id.to_owned(),
            text: format!("the answer of {id}"),
            stop: AgentStop::Completed,
            at_ms: 7,
            labels: BTreeSet::from(["untrusted".to_owned()]),
        }
    }

    #[test]
    fn the_parts_of_a_log_make_the_same_log() {
        let mut log = EpisodeLog::default();
        log.append(&EditKey::new("a1"), episode("e1"), 2);
        log.append(&EditKey::new("a2"), episode("e2"), 2);
        log.append(&EditKey::new("a3"), episode("e3"), 2);
        log.mark(&PassMark {
            key: EditKey::new("p1"),
            generation: 0,
            episodes: vec!["e2".to_owned()],
            dropped: 0,
            facts: vec!["Mark likes tea".to_owned()],
        });
        let parts = Parts::of(&log).unwrap();
        assert_eq!(parts.entries.len(), 2);
        assert_eq!(parts.dropped, 1);
        assert!(parts.entries[0].consolidated);
        assert_eq!(parts.keys, vec!["a1", "a2", "a3", "p1"]);
        assert_eq!(parts.derived.len(), 1);
        assert_eq!(parts.log().unwrap(), log);
    }

    #[test]
    fn a_cleared_log_keeps_its_generation() {
        let mut log = EpisodeLog::default();
        log.append(&EditKey::new("a1"), episode("e1"), 4);
        log.clear();
        let parts = Parts::of(&log).unwrap();
        assert_eq!(parts.generation, 1);
        assert_eq!(parts.entries, Vec::new());
        assert_eq!(parts.log().unwrap(), log);
    }

    #[test]
    fn align_counts_what_left_the_front() {
        let same = |a: &&str, b: &&str| a == b;
        assert_eq!(align(&["a", "b"], &["a", "b", "c"], same), 0);
        assert_eq!(align(&["a", "b"], &["b", "c"], same), 1);
        assert_eq!(align(&["a", "b"], &[], same), 2);
        assert_eq!(align::<&str, &str>(&[], &["a"], same), 0);
        assert_eq!(align(&["a", "b"], &["x"], same), 2);
    }

    fn apply(old: &[u8], new: &[u8]) -> Vec<u8> {
        let gone = align(old, new, |a, b| a == b);
        let mut kept: Vec<u8> = old[gone..].to_vec();
        kept.extend_from_slice(&new[old.len() - gone..]);
        kept
    }

    proptest! {
        /// Whatever the two lists, keeping the rest of the old one and
        /// adding the tail of the new one gives the new one.
        #[test]
        fn align_always_ends_with_the_new_list(
            old in prop::collection::vec(0_u8..4, 0..12),
            new in prop::collection::vec(0_u8..4, 0..12),
        ) {
            prop_assert_eq!(apply(&old, &new), new);
        }

        /// A pop from the front and a push to the back write only those.
        #[test]
        fn a_queue_step_moves_only_its_ends(
            old in prop::collection::vec(0_u8..200, 0..12),
            popped in 0_usize..4,
            pushed in prop::collection::vec(200_u8..=255, 0..4),
        ) {
            let popped = popped.min(old.len());
            let mut new = old[popped..].to_vec();
            new.extend_from_slice(&pushed);
            prop_assert_eq!(align(&old, &new, |a, b| a == b), popped);
        }
    }
}
