//! Patches end to end on the daemon (ADR 0042): the assistant proposes on
//! its own, the owner lists, opens and declines.

use littleguy_core::PatchesConfig;

use super::*;
use crate::patches::fake::{FakeGithub, REPO, with_main};

fn patches_config() -> Config {
    let mut config = config();
    config.tools = littleguy_core::ToolsConfig {
        web_fetch: false,
        web_search: false,
        files: false,
        runs: false,
        plans: false,
        ..littleguy_core::ToolsConfig::default()
    };
    config.patches = Some(PatchesConfig {
        integration: String::from("pr"),
        repo: String::from(REPO),
        base: String::from("main"),
    });
    config
}

fn open_with_github(dir: &Path, github: &FakeGithub) -> Daemon {
    let extras = Extras {
        api: Some((
            Arc::new(github.clone()) as Arc<dyn littleguy_tools::api::ProviderApi>,
            Vec::new(),
        )),
        ..Extras::default()
    };
    Daemon::open_with(
        &dir.join("lg.db"),
        &patches_config(),
        Arc::new(MemoryModel::default()),
        UID,
        false,
        extras,
    )
    .unwrap()
}

const PROPOSE: &str = r#"use propose_patch {"title":"Call a from b","body":"b needs a.","edits":[{"path":"src/lib.rs","old":"fn b() {}","new":"fn b() { a() }"}]}"#;

#[tokio::test(flavor = "multi_thread")]
async fn the_assistant_proposes_and_the_owner_opens_a_draft_pr() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = with_main();
    let mut daemon = open_with_github(dir.path(), &github);

    let proposed = daemon.handle_request(UID, request("p-1", PROPOSE)).await;
    assert!(
        reply_text(&proposed).contains(r#""status":"waits for the owner""#),
        "the patch waits for the owner"
    );
    assert!(github.repo().pulls.is_empty(), "nothing opens unasked");

    let Response::Patches { repo, patches } = daemon.handle_request(UID, Request::Patches).await
    else {
        panic!("not a listing");
    };
    assert_eq!(repo, REPO);
    let patch = patches[0].clone();
    assert_eq!(patch.status, "waiting");

    // Only the owner opens.
    let open = Request::OpenPatch {
        id: patch.id,
        digest: patch.digest.clone(),
    };
    assert!(matches!(
        daemon.handle_request(UID + 1, open.clone()).await,
        Response::Error { .. }
    ));
    let stale = Request::OpenPatch {
        id: patch.id,
        digest: String::from("stale"),
    };
    assert!(matches!(
        daemon.handle_request(UID, stale).await,
        Response::Error { message } if message.contains("changed since you read it")
    ));
    let Response::PatchOpened { pr, url, .. } = daemon.handle_request(UID, open).await else {
        panic!("the patch did not open");
    };
    assert_eq!(pr, 1);
    assert_eq!(url, "https://github.com/acme/lg/pull/1");
    assert!(github.repo().pulls[0].4, "a draft");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_declines_a_patch() {
    let dir = tempfile::tempdir().unwrap();
    let (github, _) = with_main();
    let mut daemon = open_with_github(dir.path(), &github);
    daemon.handle_request(UID, request("p-1", PROPOSE)).await;
    let decline = Request::DeclinePatch {
        id: 1,
        reason: Some(String::from("not now")),
    };
    let Response::Patches { patches, .. } = daemon.handle_request(UID, decline.clone()).await
    else {
        panic!("not a listing");
    };
    assert_eq!(patches[0].status, "declined");
    assert_eq!(patches[0].reason.as_deref(), Some("not now"));
    assert!(matches!(
        daemon.handle_request(UID, decline).await,
        Response::Error { .. }
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn patches_are_off_without_the_section() {
    let dir = tempfile::tempdir().unwrap();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config(),
        Arc::new(MemoryModel::default()),
        UID,
        false,
    )
    .unwrap();
    assert!(matches!(
        daemon.handle_request(UID, Request::Patches).await,
        Response::Off { message } if message.contains("[patches]")
    ));
    assert!(matches!(
        daemon
            .handle_request(
                UID,
                Request::DeclinePatch {
                    id: 1,
                    reason: None
                }
            )
            .await,
        Response::Error { .. }
    ));
}
