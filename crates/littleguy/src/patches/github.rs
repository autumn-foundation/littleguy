//! The GitHub calls of a patch, through the San Mateo proxy (ADR 0042).
//!
//! San Mateo holds the token of the patch connection, and adds it to each
//! call. This client sends only the calls that a patch needs, on one
//! repository. [`allowed`] lists them, and [`Github::send`] refuses any
//! other call. So no path of this code can merge a PR, mark one ready,
//! comment or touch another repository.
//!
//! It uses the REST API of git data, so the daemon needs no git binary and
//! no checkout.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::sync::Arc;

use littleguy_core::config::is_name;
use littleguy_tools::api::{ApiAnswer, ApiCall, ProviderApi};
use serde_json::{Value, json};

use super::edit::{BaseFile, Change};

/// A failed GitHub call, as text for the model or the owner.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct GithubError(pub String);

fn fail(message: impl Into<String>) -> GithubError {
    GithubError(message.into())
}

/// Whether `sha` is a full commit id: 40 lower-case hex digits.
#[must_use]
pub fn is_sha(sha: &str) -> bool {
    sha.len() == 40
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether `method` on `path` is a call that a patch needs, on `repo`.
///
/// The calls: read a file, a commit, a tree, a branch or a PR; write a
/// blob, a tree, a commit and a branch; move a branch; find and open a PR.
#[must_use]
pub fn allowed(repo: &str, method: &str, path: &str) -> bool {
    let Some(rest) = path.strip_prefix(&format!("/repos/{repo}/")) else {
        return false;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match (method, parts.as_slice()) {
        ("GET", ["contents", file @ ..]) => !file.is_empty(),
        ("GET", ["git", "commits" | "trees", sha]) => is_sha(sha),
        ("GET" | "PATCH", ["git", "ref" | "refs", "heads", branch @ ..]) => {
            // `ref` reads one branch, and `refs` moves one.
            (method == "GET") == (parts[1] == "ref")
                && !branch.is_empty()
                && branch.iter().all(|part| is_name(part))
        }
        ("GET" | "POST", ["pulls"]) | ("POST", ["git", "blobs" | "trees" | "commits" | "refs"]) => {
            true
        }
        ("GET", ["pulls", number]) => number.parse::<u64>().is_ok(),
        _ => false,
    }
}

/// Percent-encodes each segment of a repository path, keeping `/`.
fn encode_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~/".contains(&byte) {
            out.push(char::from(byte));
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// One pull request, as the daemon reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullState {
    /// Its number.
    pub number: i64,
    /// Whether it is still open.
    pub open: bool,
    /// Whether it merged.
    pub merged: bool,
}

/// The GitHub calls of patches on one repository.
#[derive(Clone)]
pub struct Github {
    api: Arc<dyn ProviderApi>,
    integration: String,
    repo: String,
}

impl std::fmt::Debug for Github {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Github")
            .field("integration", &self.integration)
            .field("repo", &self.repo)
            .finish_non_exhaustive()
    }
}

/// The JSON of `answer`, or the error that its status says.
fn json_of(answer: &ApiAnswer, what: &str) -> Result<Value, GithubError> {
    if !(200..300).contains(&answer.status) {
        let message = serde_json::from_slice::<Value>(&answer.body)
            .ok()
            .and_then(|doc| doc["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                String::from_utf8_lossy(&answer.body)
                    .chars()
                    .take(300)
                    .collect()
            });
        return Err(fail(format!(
            "GitHub refused {what} with status {}: {message}",
            answer.status
        )));
    }
    serde_json::from_slice(&answer.body).map_err(|e| {
        fail(format!(
            "GitHub answered {what} with JSON that does not read: {e}"
        ))
    })
}

/// The text field `name` of `doc`.
fn field(doc: &Value, pointer: &str, what: &str) -> Result<String, GithubError> {
    doc.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| fail(format!("GitHub answered {what} without `{pointer}`")))
}

impl Github {
    /// The client of `repo` on the San Mateo integration `integration`.
    #[must_use]
    pub fn new(api: Arc<dyn ProviderApi>, integration: &str, repo: &str) -> Self {
        Self {
            api,
            integration: integration.to_owned(),
            repo: repo.to_owned(),
        }
    }

    /// The repository, `owner/name`.
    #[must_use]
    pub fn repo(&self) -> &str {
        &self.repo
    }

    /// Sends one call, when [`allowed`] lists it.
    async fn send(
        &self,
        method: &str,
        path: String,
        params: Vec<(String, String)>,
        accept: &str,
        body: Option<&Value>,
    ) -> Result<ApiAnswer, GithubError> {
        if !allowed(&self.repo, method, &path) {
            return Err(fail(format!(
                "the patch client does not send {method} {path}"
            )));
        }
        let mut headers = vec![(String::from("accept"), accept.to_owned())];
        let body = match body {
            Some(body) => {
                headers.push((
                    String::from("content-type"),
                    String::from("application/json"),
                ));
                serde_json::to_vec(body).map_err(|e| fail(e.to_string()))?
            }
            None => Vec::new(),
        };
        self.api
            .send(ApiCall {
                integration: self.integration.clone(),
                method: method.to_owned(),
                path,
                params,
                headers,
                body,
            })
            .await
            .map_err(|e| fail(e.to_string()))
    }

    async fn get(
        &self,
        path: String,
        params: Vec<(String, String)>,
        what: &str,
    ) -> Result<Value, GithubError> {
        let answer = self
            .send("GET", path, params, "application/vnd.github+json", None)
            .await?;
        json_of(&answer, what)
    }

    async fn post(&self, path: String, body: &Value, what: &str) -> Result<Value, GithubError> {
        let answer = self
            .send(
                "POST",
                path,
                Vec::new(),
                "application/vnd.github+json",
                Some(body),
            )
            .await?;
        json_of(&answer, what)
    }

    fn at(&self, rest: &str) -> String {
        format!("/repos/{}/{rest}", self.repo)
    }

    /// The head commit of `branch`, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn branch_head(&self, branch: &str) -> Result<Option<String>, GithubError> {
        let answer = self
            .send(
                "GET",
                self.at(&format!("git/ref/heads/{branch}")),
                Vec::new(),
                "application/vnd.github+json",
                None,
            )
            .await?;
        if answer.status == 404 {
            return Ok(None);
        }
        let doc = json_of(&answer, "the branch read")?;
        field(&doc, "/object/sha", "the branch read").map(Some)
    }

    /// The text of each file of `paths` at `commit`. A file that does not
    /// exist is `None`. A file that is not UTF-8 text is `Some(Err(()))`.
    ///
    /// # Errors
    ///
    /// Returns an error when a call fails.
    pub async fn files_at(
        &self,
        commit: &str,
        paths: impl IntoIterator<Item = &str>,
    ) -> Result<BTreeMap<String, BaseFile>, GithubError> {
        let mut files = BTreeMap::new();
        for path in paths {
            if files.contains_key(path) {
                continue;
            }
            let answer = self
                .send(
                    "GET",
                    self.at(&format!("contents/{}", encode_path(path))),
                    vec![(String::from("ref"), commit.to_owned())],
                    "application/vnd.github.raw+json",
                    None,
                )
                .await?;
            let file = match answer.status {
                404 => None,
                200..=299 => Some(String::from_utf8(answer.body).map_err(|_| ())),
                _ => {
                    json_of(&answer, &format!("the read of `{path}`"))?;
                    None
                }
            };
            files.insert(path.to_owned(), file);
        }
        Ok(files)
    }

    /// The tree of the commit `commit`.
    async fn tree_of(&self, commit: &str) -> Result<String, GithubError> {
        let doc = self
            .get(
                self.at(&format!("git/commits/{commit}")),
                Vec::new(),
                "the commit read",
            )
            .await?;
        field(&doc, "/tree/sha", "the commit read")
    }

    /// The mode of each blob of the tree `tree`, by path. A tree too large
    /// for one answer gives the modes that came back.
    async fn modes(&self, tree: &str) -> Result<BTreeMap<String, String>, GithubError> {
        let doc = self
            .get(
                self.at(&format!("git/trees/{tree}")),
                vec![(String::from("recursive"), String::from("1"))],
                "the tree read",
            )
            .await?;
        Ok(doc["tree"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                Some((
                    entry["path"].as_str()?.to_owned(),
                    entry["mode"].as_str()?.to_owned(),
                ))
            })
            .collect())
    }

    /// Makes one commit with `changes` on top of `parent`, and returns its
    /// id. A changed file keeps its mode. A new file is `100644`.
    ///
    /// # Errors
    ///
    /// Returns an error when a call fails.
    pub async fn commit(
        &self,
        parent: &str,
        changes: &[Change],
        message: &str,
    ) -> Result<String, GithubError> {
        let base_tree = self.tree_of(parent).await?;
        let modes = self.modes(&base_tree).await?;
        let mut entries = Vec::with_capacity(changes.len());
        for change in changes {
            let mode = modes
                .get(&change.path)
                .cloned()
                .unwrap_or_else(|| String::from("100644"));
            let sha = match &change.after {
                Some(text) => {
                    let blob = self
                        .post(
                            self.at("git/blobs"),
                            &json!({"content": text, "encoding": "utf-8"}),
                            "the blob write",
                        )
                        .await?;
                    Value::String(field(&blob, "/sha", "the blob write")?)
                }
                None => Value::Null,
            };
            entries.push(json!({"path": change.path, "mode": mode, "type": "blob", "sha": sha}));
        }
        let tree = self
            .post(
                self.at("git/trees"),
                &json!({"base_tree": base_tree, "tree": entries}),
                "the tree write",
            )
            .await?;
        let tree = field(&tree, "/sha", "the tree write")?;
        let commit = self
            .post(
                self.at("git/commits"),
                &json!({"message": message, "tree": tree, "parents": [parent]}),
                "the commit write",
            )
            .await?;
        field(&commit, "/sha", "the commit write")
    }

    /// Creates `branch` at `commit`.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn create_branch(&self, branch: &str, commit: &str) -> Result<(), GithubError> {
        self.post(
            self.at("git/refs"),
            &json!({"ref": format!("refs/heads/{branch}"), "sha": commit}),
            "the branch write",
        )
        .await
        .map(drop)
    }

    /// Moves `branch` forward to `commit`. GitHub refuses a move that is
    /// not a fast forward.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn move_branch(&self, branch: &str, commit: &str) -> Result<(), GithubError> {
        let answer = self
            .send(
                "PATCH",
                self.at(&format!("git/refs/heads/{branch}")),
                Vec::new(),
                "application/vnd.github+json",
                Some(&json!({"sha": commit, "force": false})),
            )
            .await?;
        json_of(&answer, "the branch move").map(drop)
    }

    /// The PR whose head is `branch` of this repository, in any state.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn pull_of(&self, branch: &str) -> Result<Option<i64>, GithubError> {
        let owner = self.repo.split('/').next().unwrap_or_default();
        let doc = self
            .get(
                self.at("pulls"),
                vec![
                    (String::from("head"), format!("{owner}:{branch}")),
                    (String::from("state"), String::from("all")),
                ],
                "the PR search",
            )
            .await?;
        Ok(doc
            .as_array()
            .and_then(|pulls| pulls.first())
            .and_then(|pull| pull["number"].as_i64()))
    }

    /// Opens a draft PR from `branch` into `base`. Returns its number.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn open_draft(
        &self,
        branch: &str,
        base: &str,
        title: &str,
        body: &str,
    ) -> Result<i64, GithubError> {
        let doc = self
            .post(
                self.at("pulls"),
                &json!({"title": title, "body": body, "head": branch, "base": base, "draft": true}),
                "the PR open",
            )
            .await?;
        doc["number"]
            .as_i64()
            .ok_or_else(|| fail("GitHub opened the PR without a number"))
    }

    /// The state of the PR `number`.
    ///
    /// # Errors
    ///
    /// Returns an error when the call fails.
    pub async fn pull(&self, number: i64) -> Result<PullState, GithubError> {
        let doc = self
            .get(
                self.at(&format!("pulls/{number}")),
                Vec::new(),
                "the PR read",
            )
            .await?;
        Ok(PullState {
            number,
            open: doc["state"].as_str() == Some("open"),
            merged: doc["merged"].as_bool().unwrap_or(false),
        })
    }

    /// The URL of the PR `number`.
    #[must_use]
    pub fn pull_url(&self, number: i64) -> String {
        format!("https://github.com/{}/pull/{number}", self.repo)
    }
}

#[cfg(test)]
mod tests {
    use littleguy_core::config::is_repo;

    use super::*;

    const REPO: &str = "autumn-foundation/littleguy";
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn only_the_calls_of_a_patch_are_allowed() {
        let ok = [
            ("GET", "contents/crates/a.rs".to_owned()),
            ("GET", format!("git/commits/{SHA}")),
            ("GET", format!("git/trees/{SHA}")),
            ("GET", "git/ref/heads/littleguy/patch-3-fix".to_owned()),
            ("PATCH", "git/refs/heads/littleguy/patch-3-fix".to_owned()),
            ("POST", "git/blobs".to_owned()),
            ("POST", "git/trees".to_owned()),
            ("POST", "git/commits".to_owned()),
            ("POST", "git/refs".to_owned()),
            ("GET", "pulls".to_owned()),
            ("POST", "pulls".to_owned()),
            ("GET", "pulls/12".to_owned()),
        ];
        for (method, rest) in ok {
            assert!(
                allowed(REPO, method, &format!("/repos/{REPO}/{rest}")),
                "{method} {rest}"
            );
        }
        let refused = [
            // No merge, no ready, no comment, no review.
            ("PUT", format!("/repos/{REPO}/pulls/12/merge")),
            ("PATCH", format!("/repos/{REPO}/pulls/12")),
            ("POST", format!("/repos/{REPO}/issues/12/comments")),
            ("POST", format!("/repos/{REPO}/pulls/12/reviews")),
            // No delete and no force on a branch through another verb.
            ("DELETE", format!("/repos/{REPO}/git/refs/heads/main")),
            ("GET", format!("/repos/{REPO}/git/refs/heads/x")),
            ("PATCH", format!("/repos/{REPO}/git/ref/heads/x")),
            ("PATCH", format!("/repos/{REPO}/git/refs/heads/../tags/v1")),
            // No other repository, and no sha that is not one.
            (
                "GET",
                String::from("/repos/autumn-foundation/autumn-harvest/pulls"),
            ),
            ("GET", format!("/repos/{REPO}/git/commits/main")),
            ("GET", format!("/repos/{REPO}/pulls/x")),
            ("GET", String::from("/user")),
            ("PUT", format!("/repos/{REPO}/contents/a.rs")),
        ];
        for (method, path) in refused {
            assert!(!allowed(REPO, method, &path), "{method} {path}");
        }
    }

    #[test]
    fn shas_are_checked() {
        assert!(is_repo(REPO));
        assert!(is_sha(SHA));
        assert!(!is_sha("main"));
        assert!(!is_sha(&SHA.to_uppercase()));
        assert!(!is_sha(&SHA[1..]));
    }

    #[test]
    fn a_path_is_encoded_by_segment() {
        assert_eq!(encode_path("docs/a b#c.md"), "docs/a%20b%23c.md");
        assert_eq!(encode_path("src/lib.rs"), "src/lib.rs");
    }
}
