//! Saved scripts end to end on the daemon (ADR 0045): the assistant
//! proposes, the owner accepts, and a heartbeat tick runs the script.

use autumn_harvest::sandbox::SandboxBackend;
use littleguy_core::{SandboxApprove, SandboxBackend as BackendKind, SandboxConfig, ScriptsConfig};

use super::*;
use crate::scripts::testing::EchoBackend;

const PROPOSE: &str = r#"use propose_script {"name": "walk-repo", "runtime": "sh", "description": "Print each argument.", "code": "printf '%s\n' \"$@\"\n"}"#;

fn scripts_config(dir: &Path) -> Config {
    let mut config = config();
    config.tools.files = false;
    let sandbox = SandboxConfig {
        backend: BackendKind::Process,
        network: false,
        approve: SandboxApprove::Always,
    };
    config.scripts = Some(ScriptsConfig {
        dir: Some(dir.join("scripts")),
        network: false,
    });
    config.sandbox = Some(sandbox);
    config
}

fn open_scripted(
    dir: &Path,
    config: &Config,
    backend: &Arc<EchoBackend>,
    model: Arc<dyn AgentModel>,
) -> Daemon {
    let sandbox = config.sandbox.as_ref().unwrap();
    let runtime = crate::sandbox::runtime_on(
        Arc::clone(backend) as Arc<dyn SandboxBackend>,
        &config.scripts.as_ref().unwrap().sandbox(sandbox),
    )
    .unwrap();
    let extras = Extras {
        scripts: Some(runtime),
        ..Extras::default()
    };
    Daemon::open_with(&dir.join("lg.db"), config, model, UID, false, extras).unwrap()
}

/// Proposes the script as the owner's assistant, and accepts it.
async fn accept(daemon: &mut Daemon) -> String {
    daemon.handle_request(UID, request("p-1", PROPOSE)).await;
    let Response::Scripts { drafts, .. } = daemon.handle_request(UID, Request::Scripts).await
    else {
        panic!("not a listing");
    };
    assert_eq!(drafts.len(), 1, "the draft waits");
    let draft = drafts[0].clone();
    let accept = Request::AcceptScript {
        id: draft.id,
        digest: draft.digest.clone(),
    };
    assert!(
        matches!(
            daemon.handle_request(UID + 1, accept.clone()).await,
            Response::Error { .. }
        ),
        "only the owner accepts"
    );
    let Response::Scripts {
        scripts, drafts, ..
    } = daemon.handle_request(UID, accept).await
    else {
        panic!("not accepted");
    };
    assert_eq!(drafts, []);
    assert_eq!(scripts[0].name, "walk-repo");
    assert!(!scripts[0].drifted);
    draft.name
}

#[tokio::test(flavor = "multi_thread")]
async fn the_owner_accepts_and_the_assistant_runs_a_script() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(EchoBackend::default());
    let config = scripts_config(dir.path());
    let mut daemon = open_scripted(
        dir.path(),
        &config,
        &backend,
        Arc::new(MemoryModel::default()),
    );
    // Nothing runs before the owner accepts.
    daemon
        .handle_request(
            UID,
            request("r-0", r#"use run_script {"name": "walk-repo"}"#),
        )
        .await;
    assert_eq!(backend.runs(), 0);
    accept(&mut daemon).await;
    daemon
        .handle_request(
            UID,
            request(
                "r-1",
                r#"use run_script {"name": "walk-repo", "args": ["a; rm -rf /"]}"#,
            ),
        )
        .await;
    assert_eq!(backend.runs(), 1);
    let Response::Scripts { runs, .. } = daemon.handle_request(UID, Request::Scripts).await else {
        panic!("not a listing");
    };
    assert_eq!(runs[0].args, ["a; rm -rf /"]);
    assert_eq!(runs[0].outcome, "exited 0");
    // The owner retires it, and it no longer runs.
    let retire = Request::RetireScript {
        name: String::from("walk-repo"),
    };
    let Response::Scripts { scripts, .. } = daemon.handle_request(UID, retire).await else {
        panic!("not retired");
    };
    assert_eq!(scripts, []);
    daemon
        .handle_request(
            UID,
            request("r-2", r#"use run_script {"name": "walk-repo"}"#),
        )
        .await;
    assert_eq!(backend.runs(), 1);
}

#[cfg(feature = "heartbeat")]
#[tokio::test(flavor = "multi_thread")]
async fn a_heartbeat_tick_runs_a_blessed_script() {
    use std::sync::atomic::Ordering;
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(EchoBackend::default());
    let model = Arc::new(MemoryModel::default());
    let clock = HandClock::at("2026-11-23T12:00:00Z");
    let hand = clock.clone();
    let mut config = scripts_config(dir.path());
    config.heartbeat = heartbeat_config().heartbeat;
    if let Some(beat) = config.heartbeat.as_mut() {
        beat.prompt = String::from(
            r#"use run_script {"name": "walk-repo", "args": ["owner/repo"], "stdin": "from a tick"}"#,
        );
    }
    let mut daemon = open_scripted(
        dir.path(),
        &config,
        &backend,
        Arc::clone(&model) as Arc<dyn AgentModel>,
    )
    .with_clock(move || hand.now());
    daemon.tick().await.unwrap();
    accept(&mut daemon).await;
    daemon
        .handle_request(
            UID,
            request(
                "p-2",
                r#"use add_idea {"title": "Docs", "body": "Links rot."}"#,
            ),
        )
        .await;
    let before = model.calls.load(Ordering::SeqCst);
    clock.set("2026-11-23T13:00:01Z");
    tick_until(&mut daemon, || model.calls.load(Ordering::SeqCst) > before).await;
    // The tick is read-only, and run_script still ran: the one exception
    // to ADR 0021, and the run log shows it.
    assert_eq!(backend.runs(), 1, "the tick ran the script");
    let plan = backend.jobs.lock().unwrap()[0].plan.clone();
    assert_eq!(&plan.argv[2..], ["owner/repo"]);
    assert_eq!(plan.stdin, b"from a tick");
    let Response::Scripts { runs, .. } = daemon.handle_request(UID, Request::Scripts).await else {
        panic!("not a listing");
    };
    assert_eq!(runs[0].args, ["owner/repo"]);
    assert_eq!(runs[0].stdin.as_deref(), Some("from a tick"));
}

#[tokio::test(flavor = "multi_thread")]
async fn scripts_are_off_without_the_section() {
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
        daemon.handle_request(UID, Request::Scripts).await,
        Response::Off { message } if message.contains("[scripts]")
    ));
    assert!(matches!(
        daemon
            .handle_request(UID, Request::DeclineScript { id: 1 })
            .await,
        Response::Error { .. }
    ));
    // The listing beside the loop says the same.
    let view = daemon.view();
    assert!(matches!(
        view.answer(&Request::Scripts).await,
        Some(Ok(Response::Off { .. }))
    ));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stale_digest_or_a_declined_draft_accepts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let backend = Arc::new(EchoBackend::default());
    let config = scripts_config(dir.path());
    let mut daemon = open_scripted(
        dir.path(),
        &config,
        &backend,
        Arc::new(MemoryModel::default()),
    );
    daemon.handle_request(UID, request("p-1", PROPOSE)).await;
    let stale = Request::AcceptScript {
        id: 1,
        digest: String::from("stale"),
    };
    assert!(matches!(
        daemon.handle_request(UID, stale).await,
        Response::Error { message } if message.contains("changed since you read it")
    ));
    let Response::Scripts { drafts, .. } = daemon
        .handle_request(UID, Request::DeclineScript { id: 1 })
        .await
    else {
        panic!("not declined");
    };
    assert_eq!(drafts, []);
    assert!(!dir.path().join("scripts").join("walk-repo.sh").exists());
}
