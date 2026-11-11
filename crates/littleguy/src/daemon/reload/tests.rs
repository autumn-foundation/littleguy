use std::sync::Mutex;

use autumn_harvest_agent::{
    AgentError, BoxFuture, ChatRequest, ChatResponse, ChatRole, ContentPart, StopReason, TokenUsage,
};

use super::*;
use crate::protocol::Request;

const UID: u32 = 1000;

/// A model that keeps the system text of each call.
#[derive(Debug, Default)]
struct Recorder {
    systems: Mutex<Vec<String>>,
}

impl Recorder {
    fn last_system(&self) -> String {
        self.systems
            .lock()
            .unwrap()
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

impl AgentModel for Recorder {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        let system: String = request
            .messages
            .iter()
            .filter(|message| message.role == ChatRole::System)
            .flat_map(|message| &message.content)
            .filter_map(|part| match part {
                ContentPart::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        self.systems.lock().unwrap().push(system);
        Box::pin(async {
            Ok(ChatResponse {
                content: vec![ContentPart::Text(String::from("ok"))],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(1, 1),
            })
        })
    }
}

fn chat(id: &str) -> Request {
    Request::Chat {
        message_id: String::from(id),
        text: String::from("hi"),
    }
}

fn write(file: &Path, text: &str) {
    std::fs::write(file, format!("owner = \"mark\"\n{text}")).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_reload_stores_the_system_prompt_and_names_the_restart_sections() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lg.toml");
    write(&file, "system = \"You are the old prompt.\"");
    let config = Config::load(&file).unwrap();
    let model = Arc::new(Recorder::default());
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::clone(&model) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap()
    .with_config_file(file.clone());
    daemon.handle_request(UID, chat("m1")).await;
    assert!(model.last_system().contains("the old prompt"));

    write(
        &file,
        "system = \"You are the new prompt.\"\n[ui]\nbind = \"127.0.0.1:9999\"",
    );
    let answer = daemon.handle_request(UID, Request::Reload).await;
    assert_eq!(
        answer,
        Response::Reloaded {
            applied: Vec::new(),
            restart: vec![String::from("ui")],
            profile: vec![String::from("system")],
        }
    );
    assert!(daemon.config().system.contains("the new prompt"));
    assert_eq!(daemon.config().ui, config.ui);
    // The running assistant keeps the profile of its run until
    // autumn-harvest reads it again (autumn-harvest #2658).
    daemon.handle_request(UID, chat("m2")).await;
    assert!(model.last_system().contains("the old prompt"));

    // The same file again changes nothing that runs.
    let again = daemon.handle_request(UID, Request::Reload).await;
    assert_eq!(
        again,
        Response::Reloaded {
            applied: Vec::new(),
            restart: vec![String::from("ui")],
            profile: Vec::new(),
        }
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_file_keeps_the_running_config() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lg.toml");
    write(&file, "system = \"You are the old prompt.\"");
    let config = Config::load(&file).unwrap();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(Recorder::default()) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap()
    .with_config_file(file.clone());
    write(&file, "window = 0");
    let answer = daemon.handle_request(UID, Request::Reload).await;
    assert!(
        matches!(answer, Response::Error { .. }),
        "a bad file applied"
    );
    assert_eq!(daemon.config(), &config);

    // A brief with a bad schedule does not build, so nothing changes.
    write(&file, "[brief]\nschedule = \"99 7 * * *\"");
    let answer = daemon.handle_request(UID, Request::Reload).await;
    assert!(
        matches!(answer, Response::Error { .. }),
        "a bad brief applied"
    );
    assert_eq!(daemon.config(), &config);

    // Only the owner may reload.
    let answer = daemon.handle_request(UID + 1, Request::Reload).await;
    assert!(
        matches!(answer, Response::Error { .. }),
        "a stranger reloaded"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_without_a_file_refuses_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let config = Config::for_owner(littleguy_core::OwnerName::new("mark").unwrap());
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(Recorder::default()) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap();
    assert!(daemon.config_file().is_none());
    let answer = daemon.handle_request(UID, Request::Reload).await;
    assert!(matches!(answer, Response::Error { .. }), "a reload ran");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_new_chat_sender_is_allowed_after_a_reload() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("lg.toml");
    write(&file, "");
    let config = Config::load(&file).unwrap();
    let mut daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::new(Recorder::default()) as Arc<dyn AgentModel>,
        UID,
        true,
    )
    .unwrap()
    .with_config_file(file.clone());
    let sender = SenderId::new("discord:123456789012345678").unwrap();
    let channel = littleguy_core::ChannelId::new("discord").unwrap();
    assert!(!daemon.routing.allowed(&channel).any(|s| s == &sender));
    write(&file, "[discord]\nallowed = [\"123456789012345678\"]");
    let answer = daemon.handle_request(UID, Request::Reload).await;
    assert!(matches!(answer, Response::Reloaded { .. }), "no reload");
    assert!(daemon.routing.allowed(&channel).any(|s| s == &sender));
    assert!(daemon.config_allowed.contains(&sender));
}

#[test]
fn the_answer_names_each_section() {
    let current = Config::parse("owner = \"mark\"").unwrap();
    let next = Config::parse("owner = \"mark\"\nwindow = 9\n[hooks]").unwrap();
    let answer = reloaded(&merge(&current, next));
    assert_eq!(
        answer,
        Response::Reloaded {
            applied: Vec::new(),
            restart: vec![String::from("hooks")],
            profile: vec![String::from("window")],
        }
    );
}

#[cfg(feature = "camel")]
mod camel {
    use littleguy_models::OfflineModel;
    use tokio::sync::oneshot;

    use super::*;
    use crate::daemon::{Links, bind, jobs, run};

    fn route(inbox: &Path) -> String {
        format!(
            "[[camel.route]]\nid = \"inbox\"\nfrom = \"file:{}?initialDelay=0&delay=100&delete=true\"\nprompt = \"A file arrived.\"",
            inbox.display()
        )
    }

    async fn until(what: impl Fn() -> bool) -> bool {
        for _ in 0..150 {
            if what() {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// The daemon starts with no route. A route added to the file starts
    /// camel through the watch, and a route taken out stops.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_route_added_to_the_file_runs_and_a_removed_one_stops() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("in");
        std::fs::create_dir_all(&inbox).unwrap();
        let file = dir.path().join("lg.toml");
        write(&file, "");
        let config = Config::load(&file).unwrap();
        let db = dir.path().join("lg.db");
        let model = Arc::new(OfflineModel::new());
        let daemon = Daemon::open(
            &db,
            &config,
            Arc::clone(&model) as Arc<dyn AgentModel>,
            UID,
            false,
        )
        .unwrap()
        .with_config_file(file.clone())
        .with_watch_every(Duration::from_millis(100));
        let listener = bind(&dir.path().join("lg.sock")).await.unwrap();
        let (handle, jobs) = jobs(UID);
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let served = tokio::spawn(run(daemon, listener, Links::default(), None, jobs, async {
            let _ = stop_rx.await;
        }));

        std::fs::write(inbox.join("one.txt"), "buy milk").unwrap();
        // The watch reads its first stamp once the loop runs.
        tokio::time::sleep(Duration::from_millis(300)).await;
        write(&file, &route(&inbox));
        assert!(
            until(|| model.calls() >= 1).await,
            "the new route never fed the assistant"
        );

        write(&file, "");
        let answer = handle.ask(Request::Reload).await.unwrap();
        assert!(matches!(answer, Response::Reloaded { .. }), "no reload");
        std::fs::write(inbox.join("two.txt"), "buy eggs").unwrap();
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        assert!(inbox.join("two.txt").exists(), "a removed route ran");
        assert_eq!(model.calls(), 1);

        // A route that does not build changes nothing.
        write(&file, "[[camel.route]]\nid = \"bad\"\nfrom = \"nope:x\"");
        let answer = handle.ask(Request::Reload).await.unwrap();
        assert!(
            matches!(answer, Response::Error { .. }),
            "a bad route applied"
        );

        stop_tx.send(()).unwrap();
        served.await.unwrap().unwrap();
    }
}
