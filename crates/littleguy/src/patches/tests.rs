//! The patch tools and the open, on a fake GitHub that keeps commits,
//! trees, branches and PRs in memory.

use std::sync::Arc;

use autumn_harvest_agent::{AgentError, RunId, Tool, ToolContext, ToolEffect};
use littleguy_stores::SqliteStores;
use littleguy_stores::patches::PatchStatus;
use serde_json::{Value, json};

use super::edit::Edit;
use super::fake::{FakeGithub, REPO};
use super::github::Github;
use super::{Patches, branch_for, scannable, tools, warnings};

struct Rig {
    github: FakeGithub,
    patches: Arc<Patches>,
    tools: Vec<Arc<dyn Tool>>,
    main: String,
}

/// A repository with `src/lib.rs` on `main`, and the patch tools on it.
fn rig() -> Rig {
    let (github, main) = super::fake::with_main();
    let patches = Arc::new(Patches {
        github: Github::new(Arc::new(github.clone()), "pr", REPO),
        base_branch: String::from("main"),
        stores: Arc::new(SqliteStores::open_in_memory().unwrap()),
        secrets: Arc::new(scannable([String::from("sk-live-0123456789")])),
    });
    let tools = tools(&patches);
    Rig {
        github,
        patches,
        tools,
        main,
    }
}

fn ctx(call: &str) -> ToolContext {
    ToolContext {
        run_id: RunId::new("r"),
        call_id: call.to_owned(),
        session_id: None,
        step: 0,
        workspace: None,
    }
}

impl Rig {
    async fn call(&self, name: &str, input: Value, call: &str) -> Result<Value, AgentError> {
        let tool = self.tools.iter().find(|tool| tool.name() == name).unwrap();
        tool.execute(input, &ctx(call)).await
    }

    async fn propose(&self, input: Value, call: &str) -> i64 {
        let answer = self.call("propose_patch", input, call).await.unwrap();
        answer["patch"].as_i64().unwrap()
    }

    fn digest(&self, id: i64) -> String {
        self.patches.stores.patch(id).unwrap().unwrap().new.digest
    }

    fn file_on(&self, branch: &str, path: &str) -> Option<String> {
        let repo = self.github.repo();
        let head = repo.branches.get(branch)?;
        repo.tree_of(head)?.get(path).map(|(text, _)| text.clone())
    }
}

fn fix() -> Value {
    json!({
        "title": "Call a from b",
        "body": "b needs a.",
        "edits": [{"path": "src/lib.rs", "old": "fn b() {}", "new": "fn b() { a() }"}],
        "idea": 24,
    })
}

#[tokio::test]
async fn each_tool_runs_on_a_tick() {
    let rig = rig();
    let names: Vec<&str> = rig.tools.iter().map(|tool| tool.name()).collect();
    assert_eq!(
        names,
        [
            "propose_patch",
            "read_patch",
            "list_patches",
            "withdraw_patch"
        ]
    );
    for tool in &rig.tools {
        assert!(
            tool.effect() <= ToolEffect::Internal,
            "{} runs on a tick",
            tool.name()
        );
    }
}

#[tokio::test]
async fn a_patch_waits_then_the_owner_opens_a_draft_pr() {
    let rig = rig();
    let answer = rig.call("propose_patch", fix(), "c1").await.unwrap();
    let id = answer["patch"].as_i64().unwrap();
    assert_eq!(
        answer["base"],
        rig.main.as_str(),
        "no base means the head of main"
    );
    assert!(
        answer["diff"]
            .as_str()
            .unwrap()
            .contains("-fn b() {}\n+fn b() { a() }\n")
    );
    // Nothing is pushed while the patch waits.
    assert!(
        rig.github
            .repo()
            .calls
            .iter()
            .all(|(method, _)| method == "GET")
    );
    let read = rig
        .call("read_patch", json!({"id": id}), "c2")
        .await
        .unwrap();
    assert_eq!(read["status"], "waiting");
    assert_eq!(read["idea"], 24);
    assert_eq!(read["edits"][0]["path"], "src/lib.rs");

    let pr = rig.patches.open(id, &rig.digest(id)).await.unwrap();
    assert_eq!(pr, 1);
    let branch = branch_for(id, "Call a from b");
    assert_eq!(branch, "littleguy/patch-1-call-a-from-b");
    assert_eq!(
        rig.file_on(&branch, "src/lib.rs").as_deref(),
        Some("fn a() {}\nfn b() { a() }\n")
    );
    {
        let repo = rig.github.repo();
        let pull = &repo.pulls[0];
        assert_eq!(
            (pull.0.as_str(), pull.1.as_str()),
            (branch.as_str(), "main")
        );
        assert!(pull.4, "the PR is a draft");
        assert!(
            pull.3.contains("Proposed by littleguy as patch #1"),
            "{}",
            pull.3
        );
        // main did not move.
        assert_eq!(repo.branches["main"], rig.main);
    }
    let opened = rig.patches.stores.patch(id).unwrap().unwrap();
    assert_eq!(opened.status, PatchStatus::Opened);
    assert_eq!(opened.pr, Some(1));
    // A second press finds nothing that waits.
    assert!(rig.patches.open(id, &rig.digest(id)).await.is_err());
}

#[tokio::test]
async fn the_owner_opens_only_the_diff_that_the_owner_read() {
    let rig = rig();
    let id = rig.propose(fix(), "c1").await;
    let refused = rig.patches.open(id, "0000").await.unwrap_err();
    assert!(refused.contains("changed since you read it"), "{refused}");
    assert_eq!(rig.github.repo().pulls.len(), 0);
    assert_eq!(
        rig.patches.stores.patch(id).unwrap().unwrap().status,
        PatchStatus::Waiting
    );
}

#[tokio::test]
async fn an_open_that_stopped_resumes_and_opens_one_pr() {
    let rig = rig();
    let id = rig.propose(fix(), "c1").await;
    let digest = rig.digest(id);
    rig.github.repo().fail_once = Some((String::from("POST"), format!("/repos/{REPO}/pulls")));
    assert!(rig.patches.open(id, &digest).await.is_err());
    let stopped = rig.patches.stores.patch(id).unwrap().unwrap();
    assert_eq!(stopped.status, PatchStatus::Waiting);
    assert!(stopped.commit.is_some(), "the commit is recorded");
    let commits = rig.github.repo().commits.len();
    assert_eq!(rig.patches.open(id, &digest).await.unwrap(), 1);
    assert_eq!(rig.github.repo().commits.len(), commits, "no second commit");
    assert_eq!(rig.github.repo().pulls.len(), 1, "one PR");
}

#[tokio::test]
async fn a_revision_moves_the_branch_of_its_pr_forward() {
    let rig = rig();
    let first = rig.propose(fix(), "c1").await;
    rig.patches.open(first, &rig.digest(first)).await.unwrap();
    let branch = branch_for(first, "Call a from b");
    let head = rig.github.repo().branches[&branch].clone();
    // A revision against main, not the branch head, is refused.
    let wrong = rig
        .call(
            "propose_patch",
            json!({
                "title": "Call a twice", "body": "more", "revises": first, "base": rig.main,
                "edits": [{"path": "src/lib.rs", "old": "a() }", "new": "a(); a() }"}],
            }),
            "c2",
        )
        .await
        .unwrap_err();
    assert!(wrong.to_string().contains(&head), "{wrong}");
    let revision = rig
        .propose(
            json!({
                "title": "Call a twice", "body": "more", "revises": first,
                "edits": [{"path": "src/lib.rs", "old": "a() }", "new": "a(); a() }"}],
            }),
            "c3",
        )
        .await;
    assert_eq!(
        rig.patches
            .open(revision, &rig.digest(revision))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        rig.file_on(&branch, "src/lib.rs").as_deref(),
        Some("fn a() {}\nfn b() { a(); a() }\n")
    );
    assert_eq!(rig.github.repo().pulls.len(), 1, "the same PR");
}

#[tokio::test]
async fn a_revision_waits_when_its_branch_moved() {
    let rig = rig();
    let first = rig.propose(fix(), "c1").await;
    rig.patches.open(first, &rig.digest(first)).await.unwrap();
    let revision = rig
        .propose(
            json!({
                "title": "Again", "body": "x", "revises": first,
                "edits": [{"path": "src/lib.rs", "old": "fn a() {}", "new": "fn a() { }"}],
            }),
            "c2",
        )
        .await;
    // The owner pushes to the branch by hand.
    let branch = branch_for(first, "Call a from b");
    {
        let mut repo = rig.github.repo();
        let head = repo.branches[&branch].clone();
        let moved = repo.commit(
            &[("src/lib.rs", "fn a() {}\nfn b() { a() }\n")],
            Some(&head),
        );
        repo.branches.insert(branch.clone(), moved);
    }
    let refused = rig
        .patches
        .open(revision, &rig.digest(revision))
        .await
        .unwrap_err();
    assert!(refused.contains("moved"), "{refused}");
}

#[tokio::test]
async fn a_secret_or_a_ci_path_never_becomes_a_patch() {
    let rig = rig();
    let secret = rig
        .call(
            "propose_patch",
            json!({
                "title": "Leak", "body": "key sk-live-0123456789",
                "edits": [{"path": "src/lib.rs", "old": "fn a() {}", "new": "fn a() { }"}],
            }),
            "c1",
        )
        .await
        .unwrap_err();
    assert!(secret.to_string().contains("house key"), "{secret}");
    let ci = rig
        .call(
            "propose_patch",
            json!({
                "title": "CI", "body": "x",
                "edits": [{"path": ".github/workflows/ci.yml", "create": "on: push"}],
            }),
            "c2",
        )
        .await
        .unwrap_err();
    assert!(ci.to_string().contains(".github/"), "{ci}");
    let bad_base = rig
        .call(
            "propose_patch",
            json!({"title": "x", "body": "x", "base": "main", "edits": [{"path": "a", "create": "b"}]}),
            "c3",
        )
        .await
        .unwrap_err();
    assert!(bad_base.to_string().contains("40"), "{bad_base}");
    assert_eq!(rig.patches.stores.patches().unwrap().len(), 0);
}

#[tokio::test]
async fn a_merge_ends_the_patch_and_notes_its_idea() {
    let rig = rig();
    #[cfg(feature = "plans")]
    let idea = rig
        .patches
        .stores
        .add_idea("i", "Patches never leave", "idea 24", 0)
        .unwrap();
    #[cfg(not(feature = "plans"))]
    let idea = 24;
    let mut input = fix();
    input["idea"] = json!(idea);
    let id = rig.propose(input, "c1").await;
    rig.patches.open(id, &rig.digest(id)).await.unwrap();
    let listed = rig.call("list_patches", json!({}), "c2").await.unwrap();
    assert_eq!(listed["patches"][0]["status"], "opened");
    assert_eq!(
        listed["patches"][0]["pr_url"],
        "https://github.com/acme/lg/pull/1"
    );
    {
        let mut repo = rig.github.repo();
        repo.pulls[0].5 = false;
        repo.pulls[0].6 = true;
    }
    let listed = rig.call("list_patches", json!({}), "c3").await.unwrap();
    assert_eq!(listed["patches"][0]["status"], "merged");
    assert!(listed.get("pr_states").is_none());
    #[cfg(feature = "plans")]
    {
        let plans = rig.patches.stores.plans().unwrap();
        let notes = &plans.ideas[0].notes;
        assert!(
            notes[0]
                .text
                .contains("is merged: https://github.com/acme/lg/pull/1"),
            "{notes:?}"
        );
    }
}

#[tokio::test]
async fn the_assistant_withdraws_a_waiting_patch_only() {
    let rig = rig();
    let id = rig.propose(fix(), "c1").await;
    let withdrawn = rig
        .call("withdraw_patch", json!({"id": id}), "c2")
        .await
        .unwrap();
    assert_eq!(withdrawn["status"], "withdrawn");
    assert!(
        rig.call("withdraw_patch", json!({"id": id}), "c3")
            .await
            .is_err()
    );
    assert!(rig.patches.open(id, &rig.digest(id)).await.is_err());
}

#[test]
fn a_branch_name_comes_from_the_id_and_the_title() {
    assert_eq!(
        branch_for(3, "Fix: the  notes!"),
        "littleguy/patch-3-fix-the-notes"
    );
    assert_eq!(branch_for(4, "!!!"), "littleguy/patch-4");
    let long = branch_for(5, &"word ".repeat(30));
    assert!(long.len() <= "littleguy/patch-5-".len() + 41, "{long}");
    assert!(!long.ends_with('-'));
}

#[test]
fn a_short_secret_is_not_scanned() {
    assert_eq!(
        scannable([String::from("short"), String::from(" long-enough \n")]),
        ["long-enough"]
    );
}

#[test]
fn a_build_file_is_a_warning_for_the_owner() {
    let edit = |path: &str| Edit::Create {
        path: path.to_owned(),
        create: String::new(),
    };
    assert_eq!(
        warnings(&[
            edit("crates/a/Cargo.toml"),
            edit("src/lib.rs"),
            edit("xtask/src/main.rs")
        ]),
        [
            "crates/a/Cargo.toml changes how the code builds",
            "xtask/src/main.rs changes how the code builds",
        ]
    );
    assert_eq!(warnings(&[edit("docs/a.md")]), [] as [String; 0]);
}

#[tokio::test]
async fn the_owner_lists_and_declines_a_patch() {
    let rig = rig();
    let id = rig.propose(fix(), "c1").await;
    let crate::protocol::Response::Patches { repo, patches } = rig.patches.listing().unwrap()
    else {
        panic!("not a listing");
    };
    assert_eq!(repo, REPO);
    assert_eq!(patches[0].id, id);
    assert_eq!(patches[0].digest, rig.digest(id));
    assert!(patches[0].diff.contains("+fn b() { a() }"));
    rig.patches.decline(id, Some("not yet")).unwrap();
    assert!(rig.patches.decline(id, None).is_err());
    let read = rig
        .call("read_patch", json!({"id": id}), "c2")
        .await
        .unwrap();
    assert_eq!(read["status"], "declined");
    assert_eq!(read["reason"], "not yet");
}
