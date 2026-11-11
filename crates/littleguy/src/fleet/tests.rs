use autumn_harvest_agent::memory::{EditKey, MemoryOp, MemoryScope, MemoryStore};
use littleguy_core::S3Replica;
use littleguy_stores::SqliteStores;
use serde_json::json;

use super::*;

fn owner() -> OwnerName {
    OwnerName::new(String::from("mark")).unwrap()
}

fn config(volume: &Path, replica: &Path, restore_missing: bool) -> FleetConfig {
    FleetConfig {
        root: Some(volume.join("fleet")),
        restore_missing,
        rpo_secs: 2,
        replica: Replica::Path(replica.to_path_buf()),
    }
}

fn scope() -> MemoryScope {
    MemoryScope::new("agent:assistant/mark")
}

async fn entries(stores: &SqliteStores) -> Vec<String> {
    let blocks = MemoryStore::load(stores, &scope()).await.unwrap();
    blocks
        .into_iter()
        .find(|block| block.label == "memory")
        .unwrap()
        .entries
}

/// Writes one row of each store in the tenant database.
async fn remember(tenant: &Tenant) {
    let stores = SqliteStores::open_shared(tenant.path()).unwrap();
    let add = MemoryOp::Add {
        block: String::from("memory"),
        text: String::from("Mark drinks tea"),
    };
    stores
        .apply(&scope(), add, &EditKey::new("k1"))
        .await
        .unwrap();
    stores.mark_sent("brief:1").unwrap();
    stores
        .record_approval("c", "run_code", &json!({"a": 1}), "ask")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_memory_comes_back_from_the_replica_on_a_new_volume() {
    let dir = tempfile::tempdir().unwrap();
    let replica = dir.path().join("replica");
    let first = dir.path().join("volume-a");
    let tenant = Tenant::open(
        &config(&first, &replica, true),
        &first.join("lg.db"),
        &owner(),
    )
    .await
    .unwrap();
    assert!(tenant.path().starts_with(first.join("fleet")));
    remember(&tenant).await;
    tenant.close().await;

    // The volume is lost. A new one has no fleet at all.
    let second = dir.path().join("volume-b");
    let tenant = Tenant::open(
        &config(&second, &replica, true),
        &second.join("lg.db"),
        &owner(),
    )
    .await
    .unwrap();
    assert!(tenant.path().starts_with(second.join("fleet")));
    let stores = SqliteStores::open_shared(tenant.path()).unwrap();
    assert_eq!(
        entries(&stores).await,
        vec![String::from("Mark drinks tea")]
    );
    assert!(stores.was_sent("brief:1").unwrap());
    assert_eq!(stores.approval(1).unwrap().unwrap().tool, "run_code");
    drop(stores);
    tenant.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reopen_on_the_same_volume_keeps_the_rows() {
    let dir = tempfile::tempdir().unwrap();
    let replica = dir.path().join("replica");
    let volume = dir.path().join("volume");
    let fleet = config(&volume, &replica, false);
    // The default root is `fleet` beside the database.
    let fleet = FleetConfig {
        root: None,
        ..fleet
    };
    let db = volume.join("lg.db");
    let tenant = Tenant::open(&fleet, &db, &owner()).await.unwrap();
    assert_eq!(tenant.path().parent().map(Path::exists), Some(true));
    assert!(tenant.path().starts_with(volume.join(FLEET_DIR)));
    remember(&tenant).await;
    tenant.close().await;

    let tenant = Tenant::open(&fleet, &db, &owner()).await.unwrap();
    let stores = SqliteStores::open_shared(tenant.path()).unwrap();
    assert!(stores.was_sent("brief:1").unwrap());
    drop(stores);
    tenant.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn without_restore_a_new_volume_starts_empty() {
    let dir = tempfile::tempdir().unwrap();
    let replica = dir.path().join("replica");
    let first = dir.path().join("volume-a");
    let tenant = Tenant::open(
        &config(&first, &replica, false),
        &first.join("lg.db"),
        &owner(),
    )
    .await
    .unwrap();
    remember(&tenant).await;
    tenant.close().await;

    let second = dir.path().join("volume-b");
    let tenant = Tenant::open(
        &config(&second, &replica, false),
        &second.join("lg.db"),
        &owner(),
    )
    .await
    .unwrap();
    let stores = SqliteStores::open_shared(tenant.path()).unwrap();
    assert!(!stores.was_sent("brief:1").unwrap());
    drop(stores);
    tenant.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_replica_without_its_keys_fails_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let fleet = FleetConfig {
        root: Some(dir.path().join("fleet")),
        restore_missing: false,
        rpo_secs: 30,
        replica: Replica::S3(S3Replica {
            bucket: String::from("lg"),
            region: Some(String::from("auto")),
            endpoint: Some(String::from("https://replica.invalid")),
            access_key_id_env: String::from("LG_TEST_UNSET_REPLICA_KEY_ID"),
            secret_access_key_env: String::from("LG_TEST_UNSET_REPLICA_SECRET"),
            force_path_style: true,
        }),
    };
    let error = Tenant::open(&fleet, &dir.path().join("lg.db"), &owner())
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("LG_TEST_UNSET_REPLICA_KEY_ID"),
        "the error names no variable"
    );
}

#[test]
fn the_replica_settings_follow_the_config() {
    let dir = Path::new("/data");
    let path = replication_config(&config(dir, Path::new("/replica"), false));
    assert!(path.enabled);
    assert_eq!(path.rpo_secs, 2);
    assert_eq!(path.path.as_deref(), Some("/replica"));
    assert!(path.s3.is_none());
    assert_eq!(path.prefix.as_deref(), Some(REPLICA_PREFIX));
    assert!(is_tenant_name("mark"));
    assert!(is_tenant_name("0-a_b"));
    for bad in [
        "",
        "Mark",
        "-mark",
        "_mark",
        "ma.rk",
        "ma/rk",
        &"a".repeat(129),
    ] {
        assert!(!is_tenant_name(bad), "{bad} passed");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_owner_that_is_no_tenant_name_fails_the_open() {
    let dir = tempfile::tempdir().unwrap();
    let fleet = config(dir.path(), &dir.path().join("replica"), false);
    let owner = OwnerName::new(String::from("Mark")).unwrap();
    let error = Tenant::open(&fleet, &dir.path().join("lg.db"), &owner)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("tenant name"),
        "a plain error"
    );
}
