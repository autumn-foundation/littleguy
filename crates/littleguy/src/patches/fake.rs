//! A fake GitHub for the patch tests: commits, trees, branches and PRs in
//! memory, behind the provider API of the San Mateo proxy.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use autumn_harvest_agent::{AgentError, BoxFuture};
use littleguy_tools::api::{ApiAnswer, ApiCall, ProviderApi};
use serde_json::{Value, json};

/// The repository of the fake.
pub const REPO: &str = "acme/lg";

/// One file of a tree: its text and its mode.
type Tree = BTreeMap<String, (String, String)>;

/// The state of the fake repository.
#[derive(Debug, Default)]
pub struct Repo {
    next: u64,
    /// Each commit: its tree and its parent.
    pub commits: BTreeMap<String, (String, Option<String>)>,
    /// Each tree by id.
    pub trees: BTreeMap<String, Tree>,
    /// Each blob by id.
    pub blobs: BTreeMap<String, String>,
    /// Each branch and its head.
    pub branches: BTreeMap<String, String>,
    /// Each PR: head, base, title, body, draft, open, merged.
    pub pulls: Vec<(String, String, String, String, bool, bool, bool)>,
    /// Each call, as its method and path.
    pub calls: Vec<(String, String)>,
    /// The method and path of a call that fails once, as if the daemon
    /// stopped there.
    pub fail_once: Option<(String, String)>,
}

impl Repo {
    fn sha(&mut self) -> String {
        self.next += 1;
        format!("{:040x}", self.next)
    }

    /// Adds a commit with `files` on `parent`, and returns its id.
    pub fn commit(&mut self, files: &[(&str, &str)], parent: Option<&str>) -> String {
        let tree_sha = self.sha();
        let tree = files
            .iter()
            .map(|(path, text)| {
                (
                    (*path).to_owned(),
                    ((*text).to_owned(), String::from("100644")),
                )
            })
            .collect();
        self.trees.insert(tree_sha.clone(), tree);
        let sha = self.sha();
        self.commits
            .insert(sha.clone(), (tree_sha, parent.map(str::to_owned)));
        sha
    }

    pub fn tree_of(&self, commit: &str) -> Option<&Tree> {
        self.commits
            .get(commit)
            .and_then(|(tree, _)| self.trees.get(tree))
    }
}

/// A GitHub that answers the calls of a patch from memory.
#[derive(Debug, Clone, Default)]
pub struct FakeGithub(Arc<Mutex<Repo>>);

fn answer(status: u16, body: &Value) -> ApiAnswer {
    ApiAnswer {
        status,
        headers: Vec::new(),
        body: serde_json::to_vec(body).unwrap(),
    }
}

impl FakeGithub {
    pub fn repo(&self) -> std::sync::MutexGuard<'_, Repo> {
        self.0.lock().unwrap()
    }

    #[expect(clippy::too_many_lines, reason = "one arm per GitHub call")]
    #[expect(
        clippy::significant_drop_tightening,
        reason = "the fake answers each call under one lock"
    )]
    fn handle(&self, call: &ApiCall) -> ApiAnswer {
        let mut repo = self.repo();
        repo.calls.push((call.method.clone(), call.path.clone()));
        if repo.fail_once.as_ref() == Some(&(call.method.clone(), call.path.clone())) {
            repo.fail_once = None;
            return answer(502, &json!({"message": "bad gateway"}));
        }
        let rest = call
            .path
            .strip_prefix(&format!("/repos/{REPO}/"))
            .unwrap_or_default()
            .to_owned();
        let body: Value = serde_json::from_slice(&call.body).unwrap_or(Value::Null);
        let param = |name: &str| {
            call.params
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.clone())
                .unwrap_or_default()
        };
        let parts: Vec<&str> = rest.split('/').collect();
        match (call.method.as_str(), parts.as_slice()) {
            ("GET", ["contents", file @ ..]) => {
                let path = file.join("/");
                match repo.tree_of(&param("ref")).and_then(|tree| tree.get(&path)) {
                    Some((text, _)) => ApiAnswer {
                        status: 200,
                        headers: Vec::new(),
                        body: text.clone().into_bytes(),
                    },
                    None => answer(404, &json!({"message": "Not Found"})),
                }
            }
            ("GET", ["git", "commits", sha]) => match repo.commits.get(*sha) {
                Some((tree, _)) => answer(200, &json!({"tree": {"sha": tree}})),
                None => answer(404, &json!({"message": "Not Found"})),
            },
            ("GET", ["git", "trees", sha]) => {
                let entries: Vec<Value> = repo
                    .trees
                    .get(*sha)
                    .into_iter()
                    .flatten()
                    .map(|(path, (_, mode))| json!({"path": path, "mode": mode, "type": "blob"}))
                    .collect();
                answer(200, &json!({"tree": entries}))
            }
            ("POST", ["git", "blobs"]) => {
                let sha = repo.sha();
                let text = body["content"].as_str().unwrap().to_owned();
                repo.blobs.insert(sha.clone(), text);
                answer(201, &json!({"sha": sha}))
            }
            ("POST", ["git", "trees"]) => {
                let mut tree = repo.trees[body["base_tree"].as_str().unwrap()].clone();
                for entry in body["tree"].as_array().unwrap() {
                    let path = entry["path"].as_str().unwrap().to_owned();
                    match entry["sha"].as_str() {
                        Some(blob) => {
                            let text = repo.blobs[blob].clone();
                            let mode = entry["mode"].as_str().unwrap().to_owned();
                            tree.insert(path, (text, mode));
                        }
                        None => {
                            tree.remove(&path);
                        }
                    }
                }
                let sha = repo.sha();
                repo.trees.insert(sha.clone(), tree);
                answer(201, &json!({"sha": sha}))
            }
            ("POST", ["git", "commits"]) => {
                let sha = repo.sha();
                let tree = body["tree"].as_str().unwrap().to_owned();
                let parent = body["parents"][0].as_str().map(str::to_owned);
                repo.commits.insert(sha.clone(), (tree, parent));
                answer(201, &json!({"sha": sha}))
            }
            ("GET", ["git", "ref", "heads", branch @ ..]) => {
                repo.branches.get(&branch.join("/")).map_or_else(
                    || answer(404, &json!({"message": "Not Found"})),
                    |sha| answer(200, &json!({"object": {"sha": sha}})),
                )
            }
            ("POST", ["git", "refs"]) => {
                let name = body["ref"]
                    .as_str()
                    .unwrap()
                    .strip_prefix("refs/heads/")
                    .unwrap()
                    .to_owned();
                if repo.branches.contains_key(&name) {
                    return answer(422, &json!({"message": "Reference already exists"}));
                }
                repo.branches
                    .insert(name, body["sha"].as_str().unwrap().to_owned());
                answer(201, &json!({}))
            }
            ("PATCH", ["git", "refs", "heads", branch @ ..]) => {
                let name = branch.join("/");
                let new = body["sha"].as_str().unwrap().to_owned();
                let parent = repo.commits.get(&new).and_then(|(_, p)| p.clone());
                if body["force"] != json!(false) || repo.branches.get(&name) != parent.as_ref() {
                    return answer(422, &json!({"message": "Update is not a fast forward"}));
                }
                repo.branches.insert(name, new);
                answer(200, &json!({}))
            }
            ("GET", ["pulls"]) => {
                let head = param("head");
                let found: Vec<Value> = repo
                    .pulls
                    .iter()
                    .enumerate()
                    .filter(|(_, pull)| format!("acme:{}", pull.0) == head)
                    .map(|(n, _)| json!({"number": n + 1}))
                    .collect();
                answer(200, &json!(found))
            }
            ("POST", ["pulls"]) => {
                repo.pulls.push((
                    body["head"].as_str().unwrap().to_owned(),
                    body["base"].as_str().unwrap().to_owned(),
                    body["title"].as_str().unwrap().to_owned(),
                    body["body"].as_str().unwrap().to_owned(),
                    body["draft"].as_bool().unwrap(),
                    true,
                    false,
                ));
                answer(201, &json!({"number": repo.pulls.len()}))
            }
            ("GET", ["pulls", number]) => {
                let pull = &repo.pulls[number.parse::<usize>().unwrap() - 1];
                answer(
                    200,
                    &json!({"state": if pull.5 { "open" } else { "closed" }, "merged": pull.6}),
                )
            }
            _ => answer(404, &json!({"message": "Not Found"})),
        }
    }
}

impl ProviderApi for FakeGithub {
    fn send(&self, call: ApiCall) -> BoxFuture<'_, Result<ApiAnswer, AgentError>> {
        let answer = self.handle(&call);
        Box::pin(async move { Ok(answer) })
    }
}

/// A fake with `src/lib.rs` on `main`. Returns it and the head of main.
#[must_use]
pub fn with_main() -> (FakeGithub, String) {
    let github = FakeGithub::default();
    let main = github
        .repo()
        .commit(&[("src/lib.rs", "fn a() {}\nfn b() {}\n")], None);
    github
        .repo()
        .branches
        .insert(String::from("main"), main.clone());
    (github, main)
}
