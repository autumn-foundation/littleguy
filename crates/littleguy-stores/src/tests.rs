use std::collections::BTreeSet;
use std::future::Future;
use std::task::{Context, Poll, Waker};

use autumn_harvest_agent::AgentStop;
use autumn_harvest_agent::episode::{Episode, EpisodeStore, PassMark};
use autumn_harvest_agent::memory::{
    EditKey, InMemoryMemoryStore, MemoryOp, MemoryScope, MemoryStore,
};
use proptest::prelude::*;

use super::{SCHEMA_VERSION, SqliteStores, StoreError};

/// Runs a store future. Every store call is ready at once.
fn now<T>(future: impl Future<Output = T>) -> T {
    let mut future = std::pin::pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("a store call waited"),
    }
}

fn scope() -> MemoryScope {
    MemoryScope::new("agent:assistant/mark")
}

fn add(text: &str) -> MemoryOp {
    MemoryOp::Add {
        block: String::from("memory"),
        text: String::from(text),
    }
}

fn remove(old: &str) -> MemoryOp {
    MemoryOp::Remove {
        block: String::from("memory"),
        old: String::from(old),
    }
}

fn entries(store: &SqliteStores) -> Vec<String> {
    let blocks = now(MemoryStore::load(store, &scope())).unwrap();
    blocks
        .into_iter()
        .find(|block| block.label == "memory")
        .unwrap()
        .entries
}

fn episode(id: &str) -> Episode {
    Episode {
        id: String::from(id),
        text: format!("the answer of {id}"),
        stop: AgentStop::Completed,
        at_ms: 1,
        labels: BTreeSet::new(),
    }
}

#[test]
fn a_memory_edit_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    now(store.apply(&scope(), add("Mark drinks tea"), &EditKey::new("k1"))).unwrap();
    drop(store);

    let store = SqliteStores::open(&path).unwrap();
    assert_eq!(entries(&store), vec![String::from("Mark drinks tea")]);
    // A new scope starts with the default blocks.
    let other = now(MemoryStore::load(&store, &MemoryScope::new("other"))).unwrap();
    assert_eq!(other.len(), 2);
    assert!(other.iter().all(|block| block.entries.is_empty()));
}

#[test]
fn an_edit_key_applies_once() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    now(store.apply(&scope(), remove("a"), &EditKey::new("k2"))).unwrap();
    // The retry of k1 changes nothing and returns the block as it is.
    let retry = now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    assert_eq!(retry.entries, Vec::<String>::new());
    assert_eq!(entries(&store), Vec::<String>::new());
}

#[test]
fn a_failed_edit_keeps_its_failure() {
    let store = SqliteStores::open_in_memory().unwrap();
    let first = now(store.apply(&scope(), remove("tea"), &EditKey::new("k1"))).unwrap_err();
    now(store.apply(&scope(), add("tea"), &EditKey::new("k2"))).unwrap();
    // The entry exists now, but the retry of k1 gets its first answer.
    let retry = now(store.apply(&scope(), remove("tea"), &EditKey::new("k1"))).unwrap_err();
    assert_eq!(retry, first);
    assert_eq!(entries(&store), vec![String::from("tea")]);
}

#[test]
fn an_edit_to_an_unknown_block_is_refused() {
    let store = SqliteStores::open_in_memory().unwrap();
    let op = MemoryOp::Add {
        block: String::from("nope"),
        text: String::from("x"),
    };
    let error = now(store.apply(&scope(), op.clone(), &EditKey::new("k1"))).unwrap_err();
    assert!(error.message().contains("nope"));
    assert_eq!(
        now(store.apply(&scope(), op, &EditKey::new("k1"))).unwrap_err(),
        error
    );
}

#[test]
fn an_entry_keeps_its_flow_labels() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    let labels = BTreeSet::from([String::from("untrusted")]);
    now(store.apply_labelled(&scope(), add("from the web"), &EditKey::new("k1"), &labels)).unwrap();
    drop(store);

    let store = SqliteStores::open(&path).unwrap();
    let blocks = now(MemoryStore::load(&store, &scope())).unwrap();
    assert_eq!(
        autumn_harvest_agent::memory::snapshot_labels(&blocks),
        labels
    );
}

#[test]
fn a_memory_clear_removes_the_entries_and_the_keys() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    now(MemoryStore::clear(&store, &scope())).unwrap();
    assert_eq!(entries(&store), Vec::<String>::new());
    // The key is gone too, so the same edit applies again.
    now(store.apply(&scope(), add("a"), &EditKey::new("k1"))).unwrap();
    assert_eq!(entries(&store), vec![String::from("a")]);
}

#[test]
fn the_oldest_edit_keys_are_forgotten() {
    let store = SqliteStores::open_in_memory().unwrap();
    let total = super::MAX_EDIT_KEYS + 2;
    for n in 0..total {
        let key = EditKey::new(format!("k{n}"));
        now(store.apply(&scope(), add("same"), &key)).unwrap();
    }
    let kept: i64 = store
        .read(|conn| Ok(conn.query_row("SELECT COUNT(*) FROM memory_edits", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(kept, super::MAX_EDIT_KEYS);
}

#[test]
fn the_episode_log_survives_a_reopen_and_appends_once() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let store = SqliteStores::open(&path).unwrap();
    let first = now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    let retry = now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    assert_eq!(first.pending, 1);
    assert_eq!(retry.pending, 1);
    drop(store);

    let store = SqliteStores::open(&path).unwrap();
    let log = now(EpisodeStore::load(&store, &scope())).unwrap();
    let ids: Vec<&str> = log.episodes().map(|episode| episode.id.as_str()).collect();
    assert_eq!(ids, vec!["e1"]);
}

#[test]
fn a_pass_marks_its_episodes_once() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    now(store.append(&scope(), &EditKey::new("a2"), episode("e2"), 10)).unwrap();
    let mark = PassMark {
        key: EditKey::new("p1"),
        generation: 0,
        episodes: vec![String::from("e1")],
        dropped: 0,
        facts: vec![String::from("Mark likes tea")],
    };
    assert!(now(store.mark(&scope(), mark.clone())).unwrap());
    assert!(now(store.mark(&scope(), mark)).unwrap());
    let log = now(EpisodeStore::load(&store, &scope())).unwrap();
    assert_eq!(log.pending_len(), 1);
    assert_eq!(log.derived().count(), 1);
}

#[test]
fn an_episode_clear_answers_a_retry_with_its_first_counts() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.append(&scope(), &EditKey::new("a1"), episode("e1"), 10)).unwrap();
    let first = now(EpisodeStore::clear(&store, &scope(), &EditKey::new("c1"))).unwrap();
    let retry = now(EpisodeStore::clear(&store, &scope(), &EditKey::new("c1"))).unwrap();
    assert_eq!(first.episodes, 1);
    assert_eq!(retry, first);
    let log = now(EpisodeStore::load(&store, &scope())).unwrap();
    assert!(log.is_empty());
    assert_eq!(log.generation(), 1);
    // A pass that read the old generation marks nothing.
    let stale = PassMark {
        key: EditKey::new("p1"),
        generation: 0,
        episodes: vec![String::from("e1")],
        dropped: 0,
        facts: Vec::new(),
    };
    assert!(!now(store.mark(&scope(), stale)).unwrap());
}

#[test]
fn a_newer_schema_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.db");
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.pragma_update(None, "user_version", SCHEMA_VERSION + 1)
        .unwrap();
    drop(conn);
    let error = SqliteStores::open(&path).unwrap_err();
    assert!(matches!(error, StoreError::Schema { found } if found == SCHEMA_VERSION + 1));
}

#[test]
fn a_value_that_does_not_decode_is_a_decode_error() {
    let store = SqliteStores::open_in_memory().unwrap();
    store
        .write(|tx| {
            tx.execute(
                "INSERT INTO memory_scopes (scope, blocks) VALUES (?1, 'not json')",
                [scope().as_str()],
            )?;
            Ok(())
        })
        .unwrap();
    let error = now(MemoryStore::load(&store, &scope())).unwrap_err();
    assert!(error.message().contains("does not decode"));
}

#[test]
fn the_debug_output_shows_no_entry() {
    let store = SqliteStores::open_in_memory().unwrap();
    now(store.apply(&scope(), add("a secret"), &EditKey::new("k1"))).unwrap();
    assert!(!format!("{store:?}").contains("secret"));
}

/// One step of the conformance property.
#[derive(Debug, Clone)]
enum Step {
    Edit { op: MemoryOp, key: u8 },
    Clear,
}

fn step() -> impl Strategy<Value = Step> {
    let text = prop::sample::select(vec!["tea", "coffee", "tea and cake", " ", "x"]);
    let block = prop::sample::select(vec!["memory", "user", "nope"]);
    let op = (0..3_u8, block, text.clone(), text).prop_map(|(kind, block, old, text)| {
        let block = String::from(block);
        match kind {
            0 => MemoryOp::Add {
                block,
                text: String::from(text),
            },
            1 => MemoryOp::Replace {
                block,
                old: String::from(old),
                text: String::from(text),
            },
            _ => MemoryOp::Remove {
                block,
                old: String::from(old),
            },
        }
    });
    prop_oneof![
        9 => (op, 0..6_u8).prop_map(|(op, key)| Step::Edit { op, key }),
        1 => Just(Step::Clear),
    ]
}

proptest! {
    /// The SQLite store answers every edit as the engine's own store does.
    #[test]
    fn the_sqlite_store_matches_the_engine_store(steps in prop::collection::vec(step(), 1..40)) {
        let sqlite = SqliteStores::open_in_memory().unwrap();
        let engine = InMemoryMemoryStore::new();
        for step in steps {
            match step {
                Step::Edit { op, key } => {
                    let key = EditKey::new(format!("k{key}"));
                    let ours = now(sqlite.apply(&scope(), op.clone(), &key));
                    let theirs = now(engine.apply(&scope(), op, &key));
                    // An unknown block names the known blocks only in the
                    // engine message, so compare the outcome, then the block.
                    prop_assert_eq!(ours.is_ok(), theirs.is_ok());
                    if let (Ok(ours), Ok(theirs)) = (ours, theirs) {
                        prop_assert_eq!(ours, theirs);
                    }
                }
                Step::Clear => {
                    now(MemoryStore::clear(&sqlite, &scope())).unwrap();
                    now(MemoryStore::clear(&engine, &scope())).unwrap();
                }
            }
            prop_assert_eq!(
                now(MemoryStore::load(&sqlite, &scope())).unwrap(),
                now(MemoryStore::load(&engine, &scope())).unwrap()
            );
        }
    }
}
