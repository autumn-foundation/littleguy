use std::collections::BTreeSet;
use std::sync::Arc;

use autumn_harvest_agent::AgentStop;
use autumn_harvest_agent::episode::{Episode, EpisodeStore, InMemoryEpisodeStore, PassMark};
use autumn_harvest_agent::memory::{
    EditKey, InMemoryMemoryStore, MemoryOp, MemoryScope, MemoryStore,
};
use littleguy_core::embed::{Embedder, HashingEmbedder};
use proptest::prelude::*;

use super::*;

fn embedder() -> Arc<dyn Embedder> {
    Arc::new(HashingEmbedder::new(64))
}

async fn store() -> AletheiaStores {
    AletheiaStores::open_in_memory(embedder()).await.unwrap()
}

fn scope() -> MemoryScope {
    MemoryScope::new("agent:assistant/mark")
}

/// The scope of the episode log, as the engine names it.
fn log_scope() -> MemoryScope {
    MemoryScope::new("agent:assistant/mark:episodes")
}

fn add(text: &str) -> MemoryOp {
    MemoryOp::Add {
        block: String::from("memory"),
        text: String::from(text),
    }
}

fn replace(old: &str, text: &str) -> MemoryOp {
    MemoryOp::Replace {
        block: String::from("memory"),
        old: String::from(old),
        text: String::from(text),
    }
}

fn remove(old: &str) -> MemoryOp {
    MemoryOp::Remove {
        block: String::from("memory"),
        old: String::from(old),
    }
}

async fn entries(store: &AletheiaStores) -> Vec<String> {
    let blocks = MemoryStore::load(store, &scope()).await.unwrap();
    blocks
        .into_iter()
        .find(|block| block.label == "memory")
        .unwrap()
        .entries
}

fn episode(id: &str, text: &str) -> Episode {
    Episode {
        id: String::from(id),
        text: String::from(text),
        stop: AgentStop::Completed,
        at_ms: 1_700_000_000_000,
        labels: BTreeSet::new(),
    }
}

fn recall(query: &str, kinds: Vec<RecallKind>) -> Recall {
    Recall {
        facts: scope(),
        episodes: log_scope(),
        query: query.to_owned(),
        limit: 5,
        kinds,
        as_of_ms: None,
    }
}

#[tokio::test]
async fn a_memory_edit_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DIRECTORY);
    {
        let store = AletheiaStores::open(&path, embedder()).await.unwrap();
        store
            .apply(&scope(), add("Mark likes tea"), &EditKey::new("k1"))
            .await
            .unwrap();
    }
    let store = AletheiaStores::open(&path, embedder()).await.unwrap();
    assert_eq!(entries(&store).await, vec![String::from("Mark likes tea")]);
    // The key survives too.
    store
        .apply(&scope(), remove("tea"), &EditKey::new("k2"))
        .await
        .unwrap();
    let retry = store
        .apply(&scope(), add("Mark likes tea"), &EditKey::new("k1"))
        .await
        .unwrap();
    assert_eq!(retry.entries, Vec::<String>::new());
}

#[tokio::test]
async fn a_second_process_cannot_open_the_directory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DIRECTORY);
    let first = AletheiaStores::open(&path, embedder()).await.unwrap();
    let second = AletheiaStores::open(&path, embedder()).await.unwrap_err();
    assert!(matches!(second, OpenError::Locked(_)), "{second}");
    drop(first);
    AletheiaStores::open(&path, embedder()).await.unwrap();
}

#[tokio::test]
async fn other_dimensions_fail_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DIRECTORY);
    drop(AletheiaStores::open(&path, embedder()).await.unwrap());
    let error = AletheiaStores::open(&path, Arc::new(HashingEmbedder::new(32)))
        .await
        .unwrap_err();
    assert!(
        matches!(
            error,
            OpenError::Dimensions {
                stored: 64,
                configured: 32
            }
        ),
        "{error}"
    );
    assert!(error.to_string().contains("memory.embeddings.dimensions"));
}

#[tokio::test]
async fn an_edit_key_applies_once() {
    let store = store().await;
    store
        .apply(&scope(), add("a"), &EditKey::new("k1"))
        .await
        .unwrap();
    store
        .apply(&scope(), remove("a"), &EditKey::new("k2"))
        .await
        .unwrap();
    let retry = store
        .apply(&scope(), add("a"), &EditKey::new("k1"))
        .await
        .unwrap();
    assert_eq!(retry.entries, Vec::<String>::new());
    assert_eq!(entries(&store).await, Vec::<String>::new());
}

#[tokio::test]
async fn a_failed_edit_keeps_its_failure() {
    let store = store().await;
    let first = store
        .apply(&scope(), remove("tea"), &EditKey::new("k1"))
        .await
        .unwrap_err();
    store
        .apply(&scope(), add("tea"), &EditKey::new("k2"))
        .await
        .unwrap();
    let retry = store
        .apply(&scope(), remove("tea"), &EditKey::new("k1"))
        .await
        .unwrap_err();
    assert_eq!(retry, first);
    assert_eq!(entries(&store).await, vec![String::from("tea")]);
}

#[tokio::test]
async fn an_entry_keeps_its_flow_labels() {
    let store = store().await;
    let labels = BTreeSet::from([String::from("untrusted")]);
    store
        .apply_labelled(&scope(), add("from the web"), &EditKey::new("k1"), &labels)
        .await
        .unwrap();
    let blocks = MemoryStore::load(&store, &scope()).await.unwrap();
    assert_eq!(
        autumn_harvest_agent::memory::snapshot_labels(&blocks),
        labels
    );
}

#[tokio::test]
async fn a_memory_clear_removes_the_entries_and_the_keys() {
    let store = store().await;
    store
        .apply(&scope(), add("a"), &EditKey::new("k1"))
        .await
        .unwrap();
    MemoryStore::clear(&store, &scope()).await.unwrap();
    assert_eq!(entries(&store).await, Vec::<String>::new());
    store
        .apply(&scope(), add("a"), &EditKey::new("k1"))
        .await
        .unwrap();
    assert_eq!(entries(&store).await, vec![String::from("a")]);
}

#[tokio::test]
async fn set_limits_sizes_new_and_stored_blocks_and_never_drops_an_entry() {
    let store = store().await;
    store
        .set_limits(&[("memory", 16_000), ("user", 4_000)])
        .await
        .unwrap();
    let limit = |blocks: &[MemoryBlock], label: &str| {
        blocks
            .iter()
            .find(|block| block.label == label)
            .unwrap()
            .limit_chars
    };
    let fresh = MemoryStore::load(&store, &scope()).await.unwrap();
    assert_eq!(limit(&fresh, "memory"), 16_000);
    assert_eq!(limit(&fresh, "user"), 4_000);
    store
        .apply(&scope(), add(&"x".repeat(300)), &EditKey::new("k1"))
        .await
        .unwrap();
    store.set_limits(&[("memory", 100)]).await.unwrap();
    let stored = MemoryStore::load(&store, &scope()).await.unwrap();
    assert_eq!(limit(&stored, "memory"), 300, "a block keeps what it uses");
    let other = MemoryStore::load(&store, &MemoryScope::new("other"))
        .await
        .unwrap();
    assert_eq!(limit(&other, "memory"), 100);
}

#[tokio::test]
async fn the_oldest_edit_keys_are_forgotten() {
    let store = store().await;
    let total = MAX_EDIT_KEYS + 2;
    for n in 0..total {
        store
            .apply(&scope(), add("same"), &EditKey::new(format!("k{n}")))
            .await
            .unwrap();
    }
    let kept = store
        .db
        .read(|tx| tx.find_nodes(EDIT_KEY, "kind", &Value::from(EDIT_KEY)))
        .await
        .unwrap();
    assert_eq!(kept.len() as u64, MAX_EDIT_KEYS);
    // The oldest key applies again: it is forgotten.
    let again = store
        .apply(&scope(), add("again"), &EditKey::new("k0"))
        .await
        .unwrap();
    assert!(again.entries.contains(&String::from("again")));
}

#[tokio::test]
async fn a_replaced_entry_stays_as_a_fact_that_recall_finds() {
    let store = store().await;
    store
        .apply(
            &scope(),
            add("Mark drinks green tea every morning"),
            &EditKey::new("k1"),
        )
        .await
        .unwrap();
    store
        .apply(
            &scope(),
            replace("green tea", "Mark drinks black coffee every morning"),
            &EditKey::new("k2"),
        )
        .await
        .unwrap();
    let hits = store
        .recall(&recall("green tea morning", vec![RecallKind::Fact]))
        .await
        .unwrap();
    assert_ne!(hits, Vec::new());
    // The search runs on now, so the old fact is gone and the new one
    // answers.
    assert!(hits.iter().all(|hit| hit.current));
    assert!(hits[0].text.contains("coffee"));
    assert_eq!(hits[0].block.as_deref(), Some("memory"));

    // The old fact keeps its versions, linked from the new one.
    let (old, new) = store
        .db
        .read(|tx| {
            let mut all = Vec::new();
            for id in tx.find_nodes(FACT, "kind", &Value::from(FACT))? {
                all.push(tx.get_node(id)?);
            }
            Ok(all)
        })
        .await
        .map(|nodes| {
            let new = nodes
                .into_iter()
                .find(|n| text(n, "text").is_some_and(|t| t.contains("coffee")))
                .unwrap();
            (uint(&new, "replaces"), new)
        })
        .unwrap();
    assert!(old > 0);
    assert!(new.properties.contains_key(EMBEDDING));
    let ended = store.db.get_node(old).await.unwrap();
    assert!(int(&ended, "ended_ms") > 0, "the old fact ended");
    assert_eq!(ended.properties["live"], Value::Null);
    let history = store.db.node_history(old).await.unwrap();
    assert!(history.len() >= 2, "the old fact keeps its versions");
}

#[tokio::test]
async fn recall_as_of_a_past_time_finds_the_old_fact() {
    let store = store().await;
    store
        .apply(
            &scope(),
            add("The owner lives in Lisbon"),
            &EditKey::new("k1"),
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    let then = now_ms();
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    store
        .apply(&scope(), remove("Lisbon"), &EditKey::new("k2"))
        .await
        .unwrap();
    let now_hits = store
        .recall(&recall(
            "where does the owner live Lisbon",
            vec![RecallKind::Fact],
        ))
        .await
        .unwrap();
    assert!(now_hits.iter().all(|hit| !hit.text.contains("Lisbon")));
    let mut past = recall("where does the owner live Lisbon", vec![RecallKind::Fact]);
    past.as_of_ms = Some(then);
    let past_hits = store.recall(&past).await.unwrap();
    let lisbon = past_hits
        .iter()
        .find(|hit| hit.text.contains("Lisbon"))
        .expect("the fact as it was then");
    assert!(!lisbon.current, "the fact is gone now");
}

#[tokio::test]
async fn an_evicted_episode_stays_for_recall() {
    let store = store().await;
    store
        .append(
            &log_scope(),
            &EditKey::new("a1"),
            episode("e1", "booked the dentist for Tuesday"),
            1,
        )
        .await
        .unwrap();
    store
        .append(
            &log_scope(),
            &EditKey::new("a2"),
            episode("e2", "the weather in Porto is mild"),
            1,
        )
        .await
        .unwrap();
    let log = EpisodeStore::load(&store, &log_scope()).await.unwrap();
    let ids: Vec<&str> = log.episodes().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["e2"], "the log evicted e1");
    let hits = store
        .recall(&recall("dentist Tuesday", vec![RecallKind::Episode]))
        .await
        .unwrap();
    assert_eq!(hits[0].text, "booked the dentist for Tuesday");
    assert_eq!(hits[0].kind, RecallKind::Episode);
    assert_eq!(hits[0].at_ms, 1_700_000_000_000);
}

#[tokio::test]
async fn recall_keeps_to_its_scope_and_kinds() {
    let store = store().await;
    let other = MemoryScope::new("other");
    store
        .apply(&other, add("secret tea"), &EditKey::new("k1"))
        .await
        .unwrap();
    store
        .apply(&scope(), add("plain tea"), &EditKey::new("k1"))
        .await
        .unwrap();
    store
        .append(
            &log_scope(),
            &EditKey::new("a1"),
            episode("e1", "tea talk"),
            10,
        )
        .await
        .unwrap();
    let all = store.recall(&recall("tea", Vec::new())).await.unwrap();
    assert!(all.iter().all(|hit| hit.text != "secret tea"));
    assert!(all.iter().any(|hit| hit.kind == RecallKind::Fact));
    assert!(all.iter().any(|hit| hit.kind == RecallKind::Episode));
    let facts = store
        .recall(&recall("tea", vec![RecallKind::Fact]))
        .await
        .unwrap();
    assert!(facts.iter().all(|hit| hit.kind == RecallKind::Fact));
    let empty = store.recall(&recall("  ", Vec::new())).await.unwrap_err();
    assert_eq!(empty.kind(), ErrorKind::Tool);
}

#[tokio::test]
async fn an_episode_clear_forgets_the_episodes() {
    let store = store().await;
    store
        .append(
            &log_scope(),
            &EditKey::new("a1"),
            episode("e1", "tea talk"),
            10,
        )
        .await
        .unwrap();
    let first = EpisodeStore::clear(&store, &log_scope(), &EditKey::new("c1"))
        .await
        .unwrap();
    let retry = EpisodeStore::clear(&store, &log_scope(), &EditKey::new("c1"))
        .await
        .unwrap();
    assert_eq!(first.episodes, 1);
    assert_eq!(retry, first);
    let log = EpisodeStore::load(&store, &log_scope()).await.unwrap();
    assert!(log.is_empty());
    assert_eq!(log.generation(), 1);
    let hits = store
        .recall(&recall("tea talk", vec![RecallKind::Episode]))
        .await
        .unwrap();
    assert_eq!(hits, Vec::new());
    let stale = PassMark {
        key: EditKey::new("p1"),
        generation: 0,
        episodes: vec![String::from("e1")],
        dropped: 0,
        facts: Vec::new(),
    };
    assert!(!store.mark(&log_scope(), stale).await.unwrap());
}

/// An embedder that always fails.
#[derive(Debug)]
struct Broken;

impl Embedder for Broken {
    fn name(&self) -> &'static str {
        "hashing"
    }

    fn dimensions(&self) -> u32 {
        64
    }

    fn embed<'a>(
        &'a self,
        _texts: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>, AgentError>> {
        Box::pin(std::future::ready(Err(AgentError::new(
            ErrorKind::Unavailable,
            "down",
        ))))
    }
}

#[tokio::test]
async fn an_edit_lands_when_the_embedder_fails_and_the_index_pass_embeds_it_later() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DIRECTORY);
    {
        let store = AletheiaStores::open(&path, Arc::new(Broken)).await.unwrap();
        store
            .apply(&scope(), add("tea"), &EditKey::new("k1"))
            .await
            .unwrap();
        store
            .append(
                &log_scope(),
                &EditKey::new("a1"),
                episode("e1", "tea talk"),
                10,
            )
            .await
            .unwrap();
        assert_eq!(entries(&store).await, vec![String::from("tea")]);
        assert!(store.embed_nodes(false).await.is_err());
    }
    let store = AletheiaStores::open(&path, embedder()).await.unwrap();
    assert_eq!(store.embed_nodes(false).await.unwrap(), 2);
    assert_eq!(store.embed_nodes(false).await.unwrap(), 0);
    let hits = store.recall(&recall("tea", Vec::new())).await.unwrap();
    assert_eq!(hits.len(), 2);
}

#[tokio::test]
async fn another_model_of_the_same_dimensions_embeds_every_node_again() {
    #[derive(Debug)]
    struct Renamed(HashingEmbedder);
    impl Embedder for Renamed {
        fn name(&self) -> &'static str {
            "renamed"
        }
        fn dimensions(&self) -> u32 {
            self.0.dimensions()
        }
        fn embed<'a>(
            &'a self,
            texts: &'a [String],
        ) -> BoxFuture<'a, Result<Vec<Vec<f32>>, AgentError>> {
            self.0.embed(texts)
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(DIRECTORY);
    {
        let store = AletheiaStores::open(&path, embedder()).await.unwrap();
        store
            .apply(&scope(), add("tea"), &EditKey::new("k1"))
            .await
            .unwrap();
    }
    let store = AletheiaStores::open(&path, Arc::new(Renamed(HashingEmbedder::new(64))))
        .await
        .unwrap();
    let meta = store
        .db
        .read(|tx| find_one(tx, META, "kind", META))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(text(&meta, "embedder").as_deref(), Some("renamed"));
    assert_eq!(
        store
            .recall(&recall("tea", Vec::new()))
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn the_rows_of_memory_db_move_in_once() {
    use crate::tests::now as ready;
    let sqlite = crate::SqliteStores::open_in_memory().unwrap();
    ready(sqlite.apply(&scope(), add("tea"), &EditKey::new("k1"))).unwrap();
    ready(sqlite.apply(&scope(), remove("cake"), &EditKey::new("k2"))).unwrap_err();
    ready(sqlite.append(
        &log_scope(),
        &EditKey::new("a1"),
        episode("e1", "tea talk"),
        10,
    ))
    .unwrap();
    ready(EpisodeStore::clear(
        &sqlite,
        &MemoryScope::new("old"),
        &EditKey::new("c1"),
    ))
    .unwrap();
    let rows = sqlite.memory_rows().unwrap();
    assert!(!rows.is_empty());

    let store = store().await;
    assert!(!store.imported().await.unwrap());
    assert!(store.import(rows.clone()).await.unwrap());
    assert!(
        !store.import(rows).await.unwrap(),
        "a second move writes nothing"
    );

    assert_eq!(entries(&store).await, vec![String::from("tea")]);
    let retry = store
        .apply(&scope(), remove("cake"), &EditKey::new("k2"))
        .await
        .unwrap_err();
    assert!(
        retry.message().contains("cake"),
        "the failure moved with its key"
    );
    let log = EpisodeStore::load(&store, &log_scope()).await.unwrap();
    assert_eq!(
        log,
        ready(EpisodeStore::load(&sqlite, &log_scope())).unwrap()
    );
    let cleared = EpisodeStore::clear(&store, &MemoryScope::new("old"), &EditKey::new("c1"))
        .await
        .unwrap();
    assert_eq!(cleared.episodes, 0);
    // The imported nodes get their embeddings.
    let hits = store.recall(&recall("tea", Vec::new())).await.unwrap();
    assert_eq!(hits.len(), 2);
}

#[tokio::test]
async fn a_start_with_no_memory_db_records_the_move() {
    let store = store().await;
    store.mark_imported().await.unwrap();
    assert!(store.imported().await.unwrap());
}

#[tokio::test]
async fn the_debug_output_shows_no_entry() {
    let store = store().await;
    store
        .apply(&scope(), add("private thing"), &EditKey::new("k1"))
        .await
        .unwrap();
    let text = format!("{store:?}");
    assert!(!text.contains("private"));
    assert!(text.contains("hashing"));
}

#[test]
fn changes_are_a_multiset_difference() {
    let mut before = MemoryBlock::defaults();
    before[0].entries = vec!["a".into(), "a".into(), "b".into()];
    let mut after = before.clone();
    after[0].entries = vec!["a".into(), "c".into()];
    let diff = changes(&before, &after);
    let texts = |list: &[FactEntry]| list.iter().map(|e| e.text.clone()).collect::<Vec<_>>();
    assert_eq!(texts(&diff.removed), vec!["a", "b"]);
    assert_eq!(texts(&diff.added), vec!["c"]);
}

#[test]
fn an_undecodable_error_keeps_its_kind_through_a_transaction() {
    let error = agent(&decode_in_tx("bad json"));
    assert_eq!(error.kind(), ErrorKind::Decode);
    let other = agent(&AletheiaDbError::InvalidInput("other".to_owned()));
    assert_eq!(other.kind(), ErrorKind::Unavailable);
}

#[test]
fn the_directory_sits_beside_the_database() {
    assert_eq!(
        path_for(Path::new("/data/lg.db")),
        Path::new("/data/memory.aletheia")
    );
    assert_eq!(path_for(Path::new("lg.db")), Path::new("memory.aletheia"));
}

/// One step of the memory property.
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

/// One step of the episode property.
#[derive(Debug, Clone)]
enum LogStep {
    Append {
        key: u8,
        id: u8,
        max: u32,
    },
    Mark {
        key: u8,
        generation: u64,
        ids: Vec<u8>,
        dropped: u64,
        fact: u8,
    },
    Clear {
        key: u8,
    },
}

fn log_step() -> impl Strategy<Value = LogStep> {
    prop_oneof![
        6 => (0..12_u8, 0..8_u8, 1..5_u32).prop_map(|(key, id, max)| LogStep::Append { key, id, max }),
        3 => (0..12_u8, 0..2_u64, prop::collection::vec(0..8_u8, 0..4), 0..2_u64, 0..3_u8)
            .prop_map(|(key, generation, ids, dropped, fact)| LogStep::Mark {
                key,
                generation,
                ids,
                dropped,
                fact,
            }),
        1 => (0..12_u8).prop_map(|key| LogStep::Clear { key }),
    ]
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap()
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    /// The AletheiaDB store answers every edit as the engine's own store
    /// does.
    #[test]
    fn the_aletheia_store_matches_the_engine_memory_store(
        steps in prop::collection::vec(step(), 1..30),
    ) {
        runtime().block_on(async {
            let ours = store().await;
            let engine = InMemoryMemoryStore::new();
            for step in steps {
                match step {
                    Step::Edit { op, key } => {
                        let key = EditKey::new(format!("k{key}"));
                        let a = ours.apply(&scope(), op.clone(), &key).await;
                        let b = engine.apply(&scope(), op, &key).await;
                        prop_assert_eq!(a.is_ok(), b.is_ok());
                        if let (Ok(a), Ok(b)) = (a, b) {
                            prop_assert_eq!(a, b);
                        }
                    }
                    Step::Clear => {
                        MemoryStore::clear(&ours, &scope()).await.unwrap();
                        MemoryStore::clear(&engine, &scope()).await.unwrap();
                    }
                }
                prop_assert_eq!(
                    MemoryStore::load(&ours, &scope()).await.unwrap(),
                    MemoryStore::load(&engine, &scope()).await.unwrap()
                );
            }
            Ok(())
        })?;
    }

    /// The split episode log answers every change as the engine's own
    /// store does.
    #[test]
    fn the_aletheia_store_matches_the_engine_episode_store(
        steps in prop::collection::vec(log_step(), 1..30),
    ) {
        runtime().block_on(async {
            let ours = store().await;
            let engine = InMemoryEpisodeStore::default();
            for step in steps {
                match step {
                    LogStep::Append { key, id, max } => {
                        let key = EditKey::new(format!("a{key}"));
                        let item = episode(&format!("e{id}"), &format!("answer {id}"));
                        let a = ours.append(&scope(), &key, item.clone(), max).await.unwrap();
                        let b = engine.append(&scope(), &key, item, max).await.unwrap();
                        prop_assert_eq!(a, b);
                    }
                    LogStep::Mark { key, generation, ids, dropped, fact } => {
                        let mark = PassMark {
                            key: EditKey::new(format!("p{key}")),
                            generation,
                            episodes: ids.iter().map(|id| format!("e{id}")).collect(),
                            dropped,
                            facts: vec![format!("fact {fact}")],
                        };
                        let a = ours.mark(&scope(), mark.clone()).await.unwrap();
                        let b = engine.mark(&scope(), mark).await.unwrap();
                        prop_assert_eq!(a, b);
                    }
                    LogStep::Clear { key } => {
                        let key = EditKey::new(format!("c{key}"));
                        let a = EpisodeStore::clear(&ours, &scope(), &key).await.unwrap();
                        let b = EpisodeStore::clear(&engine, &scope(), &key).await.unwrap();
                        prop_assert_eq!(a, b);
                    }
                }
                prop_assert_eq!(
                    EpisodeStore::load(&ours, &scope()).await.unwrap(),
                    EpisodeStore::load(&engine, &scope()).await.unwrap()
                );
            }
            Ok(())
        })?;
    }
}

#[tokio::test]
async fn a_long_log_never_fills_the_versions_of_a_node() {
    // AletheiaDB keeps at most 1 000 versions of one node, so the log node
    // must not gain a version for each op.
    let store = store().await;
    for n in 0..1_100 {
        store
            .append(
                &scope(),
                &EditKey::new(format!("a{n}")),
                episode(&format!("e{n}"), "op"),
                50,
            )
            .await
            .unwrap();
    }
    let log = EpisodeStore::load(&store, &scope()).await.unwrap();
    assert_eq!(log.len(), 50);
}
