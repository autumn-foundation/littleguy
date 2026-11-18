//! Patches that the assistant proposes, and the owner opens as draft PRs
//! (ADR 0042). Feature `patches`.
//!
//! The assistant writes a fix of its own source as a patch: exact edits
//! against a commit. `propose_patch` is `Internal`, so a tick may call it.
//! The patch waits for the owner. The owner reads the diff and opens it.
//! Then the daemon, not a model, makes the commit, the branch and a draft
//! PR through the San Mateo proxy ([`github`]).
//!
//! Every patch waits for the owner. The assistant reads its own source
//! with tools that bring outside text, and the repository can be public.
//! So the owner's read is the gate against a patch that a page steered,
//! and against private data in a public PR. No tool opens, pushes,
//! comments, marks ready or merges.

pub mod edit;
#[cfg(test)]
pub mod fake;
pub mod github;

use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolContext, ToolEffect};
use chrono::{DateTime, Utc};
use littleguy_stores::SqliteStores;
use littleguy_stores::patches::{NewPatch, Patch, PatchFor, PatchStatus};
use serde_json::{Value, json};

use self::edit::{Change, Edit};
use self::github::{Github, is_sha};

/// The prefix of each branch that a patch pushes.
pub const BRANCH_PREFIX: &str = "littleguy/";

/// The shortest secret that the scan looks for. A shorter value would
/// match ordinary text.
const MIN_SECRET_CHARS: usize = 8;

/// What the patch tools and the open share.
#[derive(Debug, Clone)]
pub struct Patches {
    /// The GitHub calls of the repository.
    pub github: Github,
    /// The branch that each PR targets.
    pub base_branch: String,
    /// The store of the patches.
    pub stores: Arc<SqliteStores>,
    /// The values of the house keys. A patch that holds one is refused.
    pub secrets: Arc<Vec<String>>,
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn time(ms: i64) -> Value {
    DateTime::<Utc>::from_timestamp_millis(ms).map_or(Value::Null, |at| json!(at.to_rfc3339()))
}

/// The values of `secrets` that are long enough to scan for.
#[must_use]
pub fn scannable(secrets: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut values: Vec<String> = secrets
        .into_iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| value.chars().count() >= MIN_SECRET_CHARS)
        .collect();
    values.sort();
    values.dedup();
    values
}

/// Whether any of `texts` holds a value of `secrets`.
fn holds_secret(secrets: &[String], texts: &[&str]) -> bool {
    secrets
        .iter()
        .any(|secret| texts.iter().any(|text| text.contains(secret.as_str())))
}

/// The branch of the patch `id` with the title `title`: the prefix, the
/// id, and up to 40 characters of the title in lower case with dashes.
#[must_use]
pub fn branch_for(id: i64, title: &str) -> String {
    let mut slug = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_end_matches('-');
    if slug.is_empty() {
        format!("{BRANCH_PREFIX}patch-{id}")
    } else {
        format!("{BRANCH_PREFIX}patch-{id}-{slug}")
    }
}

/// A patch ready to store: its edits applied at its base.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    /// The row to store.
    pub patch: NewPatch,
    /// The changed files.
    pub changes: Vec<Change>,
}

/// What the assistant asks for in one patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    /// The PR title.
    pub title: String,
    /// The PR body.
    pub body: String,
    /// The commit that the edits are written against, if named.
    pub base: Option<String>,
    /// The edits.
    pub edits: Vec<Edit>,
    /// The open patch whose PR this one revises.
    pub revises: Option<i64>,
    /// The goal or idea that it serves.
    pub serves: Option<PatchFor>,
}

impl Patches {
    /// The commit that a proposal is written against. A new patch takes
    /// the named commit, or the head of the base branch. A revision takes
    /// the head of the branch of its PR, and a named commit must be that
    /// head.
    async fn base_of(&self, proposal: &Proposal) -> Result<String, String> {
        let head = match proposal.revises {
            Some(revised) => {
                let found = self
                    .stores
                    .patch(revised)
                    .map_err(|e| e.to_string())?
                    .filter(|patch| patch.status == PatchStatus::Opened)
                    .ok_or_else(|| format!("patch {revised} is not an open PR"))?;
                let branch = found
                    .branch
                    .ok_or_else(|| format!("patch {revised} has no branch"))?;
                self.github
                    .branch_head(&branch)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("the branch {branch} of patch {revised} is gone"))?
            }
            None => match &proposal.base {
                Some(base) => return Ok(base.clone()),
                None => self
                    .github
                    .branch_head(&self.base_branch)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("the branch {} does not exist", self.base_branch))?,
            },
        };
        match &proposal.base {
            Some(base) if *base != head => Err(format!(
                "a revision is written against the head of its branch, {head}, not {base}. \
                 Read the files at {head} and write the edits against it"
            )),
            _ => Ok(head),
        }
    }

    /// Applies `edits` at `base`, and returns the changes.
    async fn changes_at(&self, base: &str, edits: &[Edit]) -> Result<Vec<Change>, String> {
        edit::check(edits).map_err(|e| e.to_string())?;
        let files = self
            .github
            .files_at(base, edits.iter().map(Edit::path))
            .await
            .map_err(|e| e.to_string())?;
        edit::apply(&files, edits).map_err(|e| e.to_string())
    }

    /// Checks `proposal`, applies it at its base, and scans it for a
    /// secret.
    ///
    /// # Errors
    ///
    /// Returns why the proposal cannot be stored, for the model.
    pub async fn prepare(&self, proposal: Proposal) -> Result<Prepared, String> {
        let title = proposal.title.trim().to_owned();
        if title.is_empty() || title.contains('\n') {
            return Err(String::from("`title` is one line of text"));
        }
        if let Some(base) = &proposal.base
            && !is_sha(base)
        {
            return Err(format!(
                "`base` is a full commit id of 40 lower-case hex digits, not `{base}`. Leave it \
                 out to write against the head of {}",
                self.base_branch
            ));
        }
        let base = self.base_of(&proposal).await?;
        let changes = self.changes_at(&base, &proposal.edits).await?;
        let diff = edit::diff(&changes);
        if holds_secret(&self.secrets, &[&title, &proposal.body, &diff]) {
            return Err(String::from(
                "the patch holds the value of a house key. A secret never goes in a patch",
            ));
        }
        let edits = serde_json::to_string(&proposal.edits).map_err(|e| e.to_string())?;
        let digest = edit::digest(&title, &proposal.body, &base, &diff);
        Ok(Prepared {
            patch: NewPatch {
                title,
                body: proposal.body,
                base,
                edits,
                diff,
                digest,
                revises: proposal.revises,
                serves: proposal.serves,
            },
            changes,
        })
    }

    /// Opens the waiting patch `id` as a draft PR, when `digest` is the
    /// digest that the owner read. Returns the PR number.
    ///
    /// Each step records its result, so a second call after a failure or a
    /// restart resumes, and never opens a second PR.
    ///
    /// # Errors
    ///
    /// Returns why the patch did not open, for the owner.
    pub async fn open(&self, id: i64, digest: &str) -> Result<i64, String> {
        let patch = self
            .stores
            .patch(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("there is no patch {id}"))?;
        if patch.status != PatchStatus::Waiting {
            return Err(format!(
                "patch {id} is {}, not waiting",
                patch.status.as_str()
            ));
        }
        if patch.new.digest != digest {
            return Err(format!(
                "patch {id} changed since you read it. Read it again before you open it"
            ));
        }
        let edits: Vec<Edit> =
            serde_json::from_str(&patch.new.edits).map_err(|e| format!("patch {id}: {e}"))?;
        let changes = self.changes_at(&patch.new.base, &edits).await?;
        let diff = edit::diff(&changes);
        let again = edit::digest(&patch.new.title, &patch.new.body, &patch.new.base, &diff);
        if again != digest {
            return Err(format!(
                "patch {id} no longer gives the diff that you read at its base"
            ));
        }
        if holds_secret(&self.secrets, &[&patch.new.title, &patch.new.body, &diff]) {
            return Err(format!("patch {id} holds the value of a house key"));
        }
        let revised = match patch.new.revises {
            Some(revised) => Some(
                self.stores
                    .patch(revised)
                    .map_err(|e| e.to_string())?
                    .filter(|found| found.status == PatchStatus::Opened)
                    .ok_or_else(|| format!("patch {revised} is no longer an open PR"))?,
            ),
            None => None,
        };
        let branch = match &revised {
            Some(found) => found
                .branch
                .clone()
                .ok_or_else(|| format!("patch {} has no branch", found.id))?,
            None => branch_for(id, &patch.new.title),
        };
        let commit = self
            .commit_of(&patch, &branch, &changes, revised.is_some())
            .await?;
        self.point(&branch, &commit, &patch.new.base, revised.is_some())
            .await?;
        let pr = match &revised {
            Some(found) => found
                .pr
                .ok_or_else(|| format!("patch {} has no PR", found.id))?,
            None => match self
                .github
                .pull_of(&branch)
                .await
                .map_err(|e| e.to_string())?
            {
                Some(pr) => pr,
                None => self
                    .github
                    .open_draft(
                        &branch,
                        &self.base_branch,
                        &patch.new.title,
                        &pr_body(&patch),
                    )
                    .await
                    .map_err(|e| e.to_string())?,
            },
        };
        self.stores
            .record_patch_opened(id, pr, now_ms())
            .map_err(|e| e.to_string())?;
        tracing::info!(patch = id, pr, "the owner opened a patch");
        Ok(pr)
    }

    /// The commit of `patch`: the recorded one, or a new one on its base.
    async fn commit_of(
        &self,
        patch: &Patch,
        branch: &str,
        changes: &[Change],
        revision: bool,
    ) -> Result<String, String> {
        if let Some(commit) = &patch.commit {
            return Ok(commit.clone());
        }
        if revision {
            let head = self
                .github
                .branch_head(branch)
                .await
                .map_err(|e| e.to_string())?;
            if head.as_deref() != Some(patch.new.base.as_str()) {
                return Err(format!(
                    "the branch {branch} moved since patch {} was written. littleguy can \
                     propose it again against the new head",
                    patch.id
                ));
            }
        }
        let message = format!(
            "{}\n\n{}\n\nProposed by littleguy as patch #{}.",
            patch.new.title,
            patch.new.body.trim(),
            patch.id
        );
        let commit = self
            .github
            .commit(&patch.new.base, changes, &message)
            .await
            .map_err(|e| e.to_string())?;
        self.stores
            .record_patch_commit(patch.id, branch, &commit, now_ms())
            .map_err(|e| e.to_string())?;
        Ok(commit)
    }

    /// Points `branch` at `commit`: creates it for a new patch, or moves it
    /// forward from `base` for a revision. A branch that is there already
    /// is left alone.
    async fn point(
        &self,
        branch: &str,
        commit: &str,
        base: &str,
        revision: bool,
    ) -> Result<(), String> {
        let head = self
            .github
            .branch_head(branch)
            .await
            .map_err(|e| e.to_string())?;
        let result = match head {
            Some(head) if head == commit => Ok(()),
            None if !revision => self.github.create_branch(branch, commit).await,
            Some(head) if revision && head == base => self.github.move_branch(branch, commit).await,
            Some(head) => {
                return Err(format!(
                    "the branch {branch} holds {head}, which is not this patch. Nothing was pushed"
                ));
            }
            None => return Err(format!("the branch {branch} is gone")),
        };
        result.map_err(|e| e.to_string())
    }

    /// Reads the state of each open PR, and ends each patch whose PR merged
    /// or closed. A patch that served a goal or an idea leaves a note on
    /// it. Returns the patches that ended.
    ///
    /// # Errors
    ///
    /// Returns why a read failed.
    pub async fn refresh(&self) -> Result<Vec<Patch>, String> {
        let mut ended = Vec::new();
        for pr in self.stores.open_prs().map_err(|e| e.to_string())? {
            let state = self.github.pull(pr).await.map_err(|e| e.to_string())?;
            if state.open {
                continue;
            }
            let now = now_ms();
            let closed = self
                .stores
                .end_pr(pr, state.merged, now)
                .map_err(|e| e.to_string())?;
            for patch in &closed {
                self.note_on_plan(patch, pr, state.merged, now);
            }
            ended.extend(closed);
        }
        Ok(ended)
    }

    /// Notes the end of `patch` on the goal or idea that it served. The
    /// note is a help, so a goal that is no longer active gets none.
    #[cfg(feature = "plans")]
    fn note_on_plan(&self, patch: &Patch, pr: i64, merged: bool, now: i64) {
        let how = if merged {
            "merged"
        } else {
            "closed without a merge"
        };
        let note = format!(
            "Patch {} ({}) is {how}: {}",
            patch.id,
            patch.new.title,
            self.github.pull_url(pr)
        );
        let key = format!("patch:{}:{how}", patch.id);
        let result = match patch.new.serves {
            Some(PatchFor::Goal(goal)) => self.stores.goal_progress(&key, goal, &note, false, now),
            Some(PatchFor::Idea(idea)) => self.stores.research_idea(&key, idea, &note, now),
            None => Ok(()),
        };
        if let Err(error) = result {
            tracing::debug!(%error, patch = patch.id, "no note on the plan of a patch");
        }
    }

    /// A build without plans has no goal or idea to note.
    #[cfg(not(feature = "plans"))]
    #[expect(clippy::unused_self, reason = "the plans build reads the stores")]
    const fn note_on_plan(&self, _patch: &Patch, _pr: i64, _merged: bool, _now: i64) {}

    /// The JSON of `patch` for the model and the pages.
    fn entry(&self, patch: &Patch, whole: bool) -> Value {
        let mut entry = json!({
            "id": patch.id,
            "title": patch.new.title,
            "status": patch.status.as_str(),
            "base": patch.new.base,
            "proposed": time(patch.created_ms),
            "revises": patch.new.revises,
            "pr": patch.pr,
            "pr_url": patch.pr.map(|pr| self.github.pull_url(pr)),
        });
        match patch.new.serves {
            Some(PatchFor::Goal(goal)) => entry["goal"] = json!(goal),
            Some(PatchFor::Idea(idea)) => entry["idea"] = json!(idea),
            None => {}
        }
        if let Some(reason) = &patch.reason {
            entry["reason"] = json!(reason);
        }
        if whole {
            entry["body"] = json!(patch.new.body);
            entry["edits"] = serde_json::from_str(&patch.new.edits).unwrap_or(Value::Null);
            entry["diff"] = json!(patch.new.diff);
        }
        entry
    }
}

/// The paths of a patch that the owner reads with care. An edit of a
/// manifest, a lock file, a build script, the automation or the image
/// changes what CI and every build run. A patch may need one, so it is a
/// warning, not a refusal.
#[must_use]
pub fn warnings(edits: &[Edit]) -> Vec<String> {
    let mut found: Vec<String> = edits
        .iter()
        .map(Edit::path)
        .filter(|path| {
            let name = path.rsplit('/').next().unwrap_or(path);
            [
                "Cargo.toml",
                "Cargo.lock",
                "build.rs",
                "rust-toolchain.toml",
                "Dockerfile",
            ]
            .contains(&name)
                || path.starts_with("xtask/")
                || *path == "compose.yaml"
        })
        .map(|path| format!("{path} changes how the code builds"))
        .collect();
    found.sort();
    found.dedup();
    found
}

impl Patches {
    /// The entry of `patch` on the owner's pages.
    #[must_use]
    pub fn owner_entry(&self, patch: &Patch) -> crate::protocol::PatchEntry {
        let edits: Vec<Edit> = serde_json::from_str(&patch.new.edits).unwrap_or_default();
        crate::protocol::PatchEntry {
            id: patch.id,
            title: patch.new.title.clone(),
            body: patch.new.body.clone(),
            status: patch.status.as_str().to_owned(),
            base: patch.new.base.clone(),
            diff: patch.new.diff.clone(),
            digest: patch.new.digest.clone(),
            revises: patch.new.revises,
            pr: patch.pr,
            pr_url: patch.pr.map(|pr| self.github.pull_url(pr)),
            reason: patch.reason.clone(),
            warnings: warnings(&edits),
            created_ms: patch.created_ms,
        }
    }

    /// The owner's listing of the patches. It reads the store only.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub fn listing(&self) -> Result<crate::protocol::Response, AgentError> {
        Ok(crate::protocol::Response::Patches {
            repo: self.github.repo().to_owned(),
            patches: self
                .stores
                .patches()?
                .iter()
                .map(|patch| self.owner_entry(patch))
                .collect(),
        })
    }

    /// The owner declines the waiting patch `id`.
    ///
    /// # Errors
    ///
    /// Returns why it did not decline.
    pub fn decline(&self, id: i64, reason: Option<&str>) -> Result<(), String> {
        if self
            .stores
            .decline_patch(id, reason, now_ms())
            .map_err(|e| e.to_string())?
        {
            Ok(())
        } else {
            Err(format!("patch {id} does not wait for you"))
        }
    }
}

/// The body of the PR of `patch`: the assistant's text, and a line that
/// says where it came from.
fn pr_body(patch: &Patch) -> String {
    format!(
        "{}\n\n---\nProposed by littleguy as patch #{} against `{}`, and opened by the owner.",
        patch.new.body.trim(),
        patch.id,
        patch.new.base
    )
}

/// The proposal in the input of `propose_patch`.
fn proposal_of(input: &Value) -> Result<Proposal, AgentError> {
    let text = |name: &str| {
        input[name]
            .as_str()
            .map(str::to_owned)
            .ok_or_else(|| refuse(format!("`{name}` must be text")))
    };
    let edits: Vec<Edit> = serde_json::from_value(input["edits"].clone()).map_err(|e| {
        refuse(format!(
            "`edits` is a list of {{path, old, new}}, {{path, create}} or {{path, delete: true}}: {e}"
        ))
    })?;
    let serves = match (input["goal"].as_i64(), input["idea"].as_i64()) {
        (Some(_), Some(_)) => return Err(refuse("name a goal or an idea, not both")),
        (Some(goal), None) => Some(PatchFor::Goal(goal)),
        (None, Some(idea)) => Some(PatchFor::Idea(idea)),
        (None, None) => None,
    };
    Ok(Proposal {
        title: text("title")?,
        body: text("body")?,
        base: input["base"].as_str().map(str::to_owned),
        edits,
        revises: input["revises"].as_i64(),
        serves,
    })
}

/// The patch tools on `patches`.
#[must_use]
pub fn tools(patches: &Arc<Patches>) -> Vec<Arc<dyn Tool>> {
    vec![
        propose_patch(patches),
        read_patch(patches),
        list_patches(patches),
        withdraw_patch(patches),
    ]
}

/// The key of one tool call. A retried call has the same key.
fn call_key(ctx: &ToolContext) -> String {
    format!("{}:{}:{}", ctx.run_id.as_str(), ctx.step, ctx.call_id)
}

/// `propose_patch`: a patch that waits for the owner.
fn propose_patch(patches: &Arc<Patches>) -> Arc<dyn Tool> {
    let shared = Arc::clone(patches);
    let repo = patches.github.repo().to_owned();
    let base = patches.base_branch.clone();
    FnTool::with_context(
        "propose_patch",
        format!(
            "Propose a change to {repo} as a patch. The owner reads its diff and opens it as a \
             draft PR against {base}; nothing is pushed until then, and you cannot merge. It \
             works on a tick. Each edit is an exact replacement {{path, old, new}}, where `old` \
             occurs exactly once in the file, or a new file {{path, create}}, or {{path, delete: \
             true}}. The edits apply in order. They are written against `base`, a full commit \
             id; leave it out to use the head of {base}. A patch never writes under .github/. \
             To change an open PR, set `revises` to its patch id: its edits are written against \
             the head of that PR's branch. CI on the PR is the compile check. Tell the owner \
             that a patch waits."
        ),
        json!({
            "type": "object",
            "properties": {
                "title": {"type": "string", "description": "The PR title, one line."},
                "body": {"type": "string", "description": "The PR body: why, what changes, and how it was tested."},
                "base": {"type": "string", "description": "The full commit id that the edits are written against."},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "old": {"type": "string"},
                            "new": {"type": "string"},
                            "create": {"type": "string"},
                            "delete": {"type": "boolean"},
                        },
                        "required": ["path"],
                    },
                },
                "revises": {"type": "integer", "description": "The id of an opened patch whose PR this one changes."},
                "goal": {"type": "integer", "description": "The goal that it serves."},
                "idea": {"type": "integer", "description": "The idea that it serves."},
            },
            "required": ["title", "body", "edits"],
        }),
        move |input, ctx| {
            let patches = Arc::clone(&shared);
            async move {
                let proposal = proposal_of(&input)?;
                let prepared = patches.prepare(proposal).await.map_err(refuse)?;
                let id = patches
                    .stores
                    .add_patch(&call_key(&ctx), &prepared.patch, now_ms())?;
                Ok(json!({
                    "patch": id,
                    "status": "waits for the owner",
                    "base": prepared.patch.base,
                    "files": prepared.changes.iter().map(|c| c.path.as_str()).collect::<Vec<_>>(),
                    "diff": prepared.patch.diff,
                }))
            }
        },
    )
    .effect(ToolEffect::Internal)
    // The diff quotes the files of the repository.
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

/// `read_patch`: one patch whole.
fn read_patch(patches: &Arc<Patches>) -> Arc<dyn Tool> {
    let shared = Arc::clone(patches);
    FnTool::new(
        "read_patch",
        "Read one patch whole: its body, edits, diff, status and PR. Read a patch before you \
         revise it.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "The patch id, from list_patches."}},
            "required": ["id"],
        }),
        move |input| {
            let patches = Arc::clone(&shared);
            async move {
                let id = input["id"]
                    .as_i64()
                    .ok_or_else(|| refuse("`id` must be a whole number"))?;
                let patch = patches
                    .stores
                    .patch(id)?
                    .ok_or_else(|| refuse(format!("there is no patch {id}")))?;
                Ok(patches.entry(&patch, true))
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

/// `list_patches`: the patches that wait, the open PRs and the newest
/// that ended. It reads the state of each open PR first.
fn list_patches(patches: &Arc<Patches>) -> Arc<dyn Tool> {
    let shared = Arc::clone(patches);
    FnTool::new(
        "list_patches",
        "List your patches: the ones that wait for the owner, the open draft PRs, and the \
         newest that ended (merged, closed, declined or withdrawn). It reads the state of each \
         open PR on GitHub first, so a merge shows here.",
        json!({"type": "object", "properties": {}}),
        move |_input| {
            let patches = Arc::clone(&shared);
            async move {
                let refreshed = patches.refresh().await;
                let listed: Vec<Value> = patches
                    .stores
                    .patches()?
                    .iter()
                    .map(|patch| patches.entry(patch, false))
                    .collect();
                let mut answer = json!({"patches": listed});
                if let Err(error) = refreshed {
                    answer["pr_states"] = json!(format!("not read: {error}"));
                }
                Ok(answer)
            }
        },
    )
    // It ends the patches whose PR ended, in the assistant's own store.
    .effect(ToolEffect::Internal)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

/// `withdraw_patch`: the assistant takes back a waiting patch.
fn withdraw_patch(patches: &Arc<Patches>) -> Arc<dyn Tool> {
    let shared = Arc::clone(patches);
    FnTool::new(
        "withdraw_patch",
        "Withdraw a patch that waits for the owner, when it is wrong or a newer one replaces \
         it. An open PR cannot be withdrawn: the owner closes it.",
        json!({
            "type": "object",
            "properties": {"id": {"type": "integer", "description": "The patch id."}},
            "required": ["id"],
        }),
        move |input| {
            let patches = Arc::clone(&shared);
            async move {
                let id = input["id"]
                    .as_i64()
                    .ok_or_else(|| refuse("`id` must be a whole number"))?;
                if patches.stores.withdraw_patch(id, now_ms())? {
                    Ok(json!({"patch": id, "status": "withdrawn"}))
                } else {
                    Err(refuse(format!("patch {id} does not wait for the owner")))
                }
            }
        },
    )
    .effect(ToolEffect::Internal)
    .shared()
}

#[cfg(test)]
mod tests;
