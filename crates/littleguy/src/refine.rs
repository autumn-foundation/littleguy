//! The refine loop of the assistant (M5.1, ADR 0013).
//!
//! A proposal is a profile overlay: a new system prompt, guardrails, tool
//! guidance or skills. The model of the assistant writes it from the tail
//! of the assistant transcript. The engine prompt of `ModelProposer` asks
//! for it, and the overlay store checks it against the ceilings of the
//! refine policy.
//!
//! A proposal never activates itself. It waits in the store until the
//! owner runs `lg refine activate`. The assistant reads the active overlay
//! at the start of each op, so the next message runs on it.
//!
//! The engine also ships the `agent_refine` workflow. It cannot read the
//! runs of a durable agent such as `assistant/<owner>`: its replay probe
//! reads `agent_loop` runs only. So this module asks the proposer once,
//! outside a workflow, and the owner is the selector.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use autumn_harvest_agent::{
    AgentHarness, AgentModel, ChatMessage, ChatRole, ContentPart, Dimension, Evidence,
    ModelProposer, OverlayProposer, OverlayStoreError, OverlayVersion, ProfileOverlay,
    ProfileOverlayStore, ProposeContext, ProposerReply, Provenance, Rollback, SqliteOverlayStore,
    StoredOverlay,
};
use littleguy_core::{ASSISTANT_KIND, RefineConfig, RefineDimension};

use crate::protocol::{ExpectedPointer, OverlayEntry};

/// The file of the overlay store, next to the database.
pub const FILE_NAME: &str = "overlays.db";

/// The operator id of a version that `lg refine propose` stores. The owner
/// still activates it, so a person decides each change.
pub const PROPOSER_ID: &str = "lg-refine";

/// The author of an overlay version that syncs the skills folder.
#[cfg(feature = "skills")]
pub const SKILLS_ID: &str = "lg-skills";

/// The attempts of one proposal. A reply that does not decode or does not
/// fit the ceilings gets its reason back on the next attempt.
pub const MAX_ATTEMPTS: u32 = 3;

/// The path of the overlay store for the database `db`.
#[must_use]
pub fn path_for(db: &Path) -> PathBuf {
    db.parent()
        .unwrap_or_else(|| Path::new("."))
        .join(FILE_NAME)
}

/// The outcome of one proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Proposal {
    /// The proposer wrote a new overlay. It waits as `version`.
    Stored {
        /// The stored version.
        version: OverlayVersion,
        /// The overlay.
        overlay: ProfileOverlay,
    },
    /// The proposer kept the active overlay. Nothing was stored.
    Kept,
}

/// The outcome of one activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Swap {
    /// The version is active now. `previous` was active before, if any. It
    /// equals the version when that version was already active.
    Moved {
        /// The version that was active before.
        previous: Option<u64>,
        /// The `seq` of this move, or the head as it stands when nothing
        /// moved. It is `None` when the daemon cannot tell that the head
        /// belongs to this move, when the read after the move fails, and
        /// before the first move.
        head: Option<u64>,
    },
    /// The pointer was not the expected one. Nothing changed.
    Refused {
        /// The active version after the refusal, if any.
        active: Option<u64>,
        /// The `seq` of the last move after the refusal, if any.
        head: Option<u64>,
    },
}

/// The outcome of one rollback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RolledBack {
    /// The version that was active.
    pub from: u64,
    /// The version that is active now.
    pub to: u64,
    /// The `seq` of the rollback, or `None` when the daemon cannot tell
    /// that the head belongs to it or cannot read it.
    pub head: Option<u64>,
}

/// The overlay store and the proposer of the assistant.
#[derive(Debug)]
pub struct Refine {
    store: Arc<SqliteOverlayStore>,
    settings: RefineConfig,
    proposer: ModelProposer,
    base: AgentHarness,
}

impl Refine {
    /// Opens the store at `path`. `base` is the harness of the assistant
    /// with no overlay store. It validates each overlay, and it gives the
    /// base profile.
    ///
    /// # Errors
    ///
    /// Returns an error when the store does not open.
    pub fn open(
        path: &Path,
        settings: &RefineConfig,
        base: &AgentHarness,
        model: Arc<dyn AgentModel>,
    ) -> Result<Self> {
        let store = SqliteOverlayStore::open(path, Arc::new(base.clone()))
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Self::with_store(store, settings, base, model))
    }

    /// [`Self::open`] over a store that is open. A test seam.
    #[must_use]
    pub fn with_store(
        store: SqliteOverlayStore,
        settings: &RefineConfig,
        base: &AgentHarness,
        model: Arc<dyn AgentModel>,
    ) -> Self {
        Self {
            store: Arc::new(store),
            settings: settings.clone(),
            proposer: ModelProposer::new(model),
            base: base.clone(),
        }
    }

    /// The store, for [`AgentHarness::overlay_store`].
    #[must_use]
    pub fn store(&self) -> Arc<dyn ProfileOverlayStore> {
        Arc::clone(&self.store) as Arc<dyn ProfileOverlayStore>
    }

    /// Asks the proposer for an overlay from `transcript`, and stores it.
    ///
    /// # Errors
    ///
    /// Returns an error when the model fails, when no attempt gives a valid
    /// overlay, or when the store fails.
    pub async fn propose(&self, transcript: &[ChatMessage]) -> Result<Proposal> {
        if transcript.is_empty() {
            bail!("the assistant has no transcript yet: talk to it first");
        }
        let profile = self
            .base
            .agent_profile(ASSISTANT_KIND)
            .context("the assistant has no profile")?;
        let policy = self.store.policy(ASSISTANT_KIND).await?.policy;
        let ceilings = policy.ceilings.resolve(&profile);
        let current = self
            .store
            .active(ASSISTANT_KIND)
            .await?
            .map(|stored| stored.overlay().clone())
            .unwrap_or_default();
        let evidence = evidence(transcript, self.settings.messages);
        let dimension = match self.settings.dimension {
            RefineDimension::Inv => Dimension::Inv,
            RefineDimension::Dom => Dimension::Dom,
        };
        let mut rejection: Option<String> = None;
        for attempt in 1..=MAX_ATTEMPTS {
            let reply = {
                let mut context =
                    ProposeContext::new(dimension, &evidence, &profile, &ceilings, &current);
                context.attempt = attempt;
                context.rejection = rejection.as_deref();
                self.proposer
                    .propose(&context)
                    .await
                    .map_err(|error| anyhow::anyhow!("the proposer failed: {error}"))?
            };
            let mut overlay = match decode(reply) {
                Ok(overlay) => overlay,
                Err(reason) => {
                    rejection = Some(reason);
                    continue;
                }
            };
            // The skills come from the skills folder only (ADR 0024). A
            // proposal keeps the active ones, whatever it says.
            overlay.skills.clone_from(&current.skills);
            if overlay == current {
                return Ok(Proposal::Kept);
            }
            let provenance = Provenance::operator(PROPOSER_ID);
            match self.store.put(ASSISTANT_KIND, &overlay, provenance).await {
                Ok(version) => return Ok(Proposal::Stored { version, overlay }),
                Err(OverlayStoreError::Overlay(error)) => rejection = Some(error.to_string()),
                Err(error) => return Err(error.into()),
            }
        }
        bail!(
            "the proposer gave no valid overlay in {MAX_ATTEMPTS} attempts. The last reason: {}",
            rejection.unwrap_or_default()
        )
    }

    /// The pointer, then every version, oldest first.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub async fn list(&self) -> Result<(ExpectedPointer, Vec<OverlayEntry>)> {
        let pointer = self.pointer().await?;
        let entries = self
            .store
            .list(ASSISTANT_KIND)
            .await?
            .iter()
            .map(|stored| entry(stored, pointer.active))
            .collect();
        Ok((pointer, entries))
    }

    /// The version `version` and its overlay as JSON, or `None`.
    ///
    /// # Errors
    ///
    /// Returns an error for version 0, or when the store fails.
    pub async fn show(&self, version: u64) -> Result<Option<(OverlayEntry, String)>> {
        let number = number(version)?;
        let Some(stored) = self.store.get(ASSISTANT_KIND, number).await? else {
            return Ok(None);
        };
        let active = self.active().await?;
        let json = serde_json::to_string_pretty(stored.overlay())?;
        Ok(Some((entry(&stored, active), json)))
    }

    /// Stores the overlay JSON `text` as a new version by `owner`. It does
    /// not activate it.
    ///
    /// # Errors
    ///
    /// Returns an error when the text is not an overlay, when the overlay
    /// does not fit the ceilings, or when the store fails.
    pub async fn put(&self, text: &str, owner: &str) -> Result<u64> {
        let overlay: ProfileOverlay =
            serde_json::from_str(text).context("the text is not overlay JSON")?;
        let version = self
            .store
            .put(ASSISTANT_KIND, &overlay, Provenance::operator(owner))
            .await?;
        Ok(version.get())
    }

    /// Makes `version` the active version, by `owner`.
    ///
    /// With `expected`, the pointer moves only when its active version and
    /// its last move are still the ones that the caller read. The store
    /// checks both in the swap transaction, so any move after the read
    /// refuses the activation, also a move away and back. With no
    /// `expected`, the active version just before the swap is the expected
    /// one.
    ///
    /// [`Swap::Moved`] names the head of this move, so a caller can tell a
    /// later move away and back from no move. `own_head` says why the
    /// head is left out when another move may come after this one.
    ///
    /// # Errors
    ///
    /// Returns an error for version 0, as the version or as the expected
    /// active version, for an unknown version, for one that no longer fits
    /// the ceilings, or when the store fails before the swap. A pointer
    /// that is not the expected one is [`Swap::Refused`], not an error. A
    /// failed read after the swap leaves the head out, since the move
    /// stands.
    pub async fn activate(
        &self,
        version: u64,
        expected: Option<ExpectedPointer>,
        owner: &str,
    ) -> Result<Swap> {
        let number = number(version)?;
        let actor = Provenance::operator(owner);
        let (before, moved) = if let Some(expected) = expected {
            let active = expected.active.map(self::number).transpose()?;
            let moved = self
                .store
                .activate_guarded(ASSISTANT_KIND, number, active, expected.head, actor)
                .await;
            (expected, moved)
        } else {
            // One read gives the version and the head together, so the
            // head of this move can be checked after the swap.
            let before = self.pointer().await?;
            let active = before.active.map(self::number).transpose()?;
            let moved = self
                .store
                .activate(ASSISTANT_KIND, number, active, actor)
                .await;
            (before, moved)
        };
        match moved {
            Ok(()) => Ok(Swap::Moved {
                previous: before.active,
                head: self.head_after(before, version).await,
            }),
            // The pointer is read again, whole: `actual` alone has no head.
            Err(OverlayStoreError::Conflict { .. }) => {
                let now = self.pointer().await?;
                Ok(Swap::Refused {
                    active: now.active,
                    head: now.head,
                })
            }
            Err(error) => Err(error.into()),
        }
    }

    /// Activates the version that was active before the current one, by
    /// `owner`.
    ///
    /// # Errors
    ///
    /// Returns an error when no earlier activation exists, or when the store
    /// fails before the swap. A failed read after the swap leaves the head
    /// out, since the rollback stands.
    pub async fn rollback(&self, owner: &str) -> Result<RolledBack> {
        let before = self.pointer().await?;
        let Rollback { from, to } = self
            .store
            .rollback(ASSISTANT_KIND, Provenance::operator(owner))
            .await?;
        let from = from.get();
        let to = to.get();
        // The rollback moves from the last move that the store reads. It is
        // the move of `before` unless another process moved in between, and
        // then `own_head` gives `None`.
        let before = ExpectedPointer {
            active: Some(from),
            head: before.head,
        };
        Ok(RolledBack {
            from,
            to,
            head: self.head_after(before, to).await,
        })
    }

    /// Makes `skills` the skills of the active overlay: it stores a new
    /// version with every other part kept, and activates it. Returns the
    /// new version, or `None` when the skills are already active.
    ///
    /// # Errors
    ///
    /// Returns an error when the engine refuses the overlay, when the
    /// pointer moved meanwhile, or when the store fails.
    #[cfg(feature = "skills")]
    pub async fn set_skills(
        &self,
        skills: std::collections::BTreeMap<String, autumn_harvest_agent::SkillText>,
    ) -> Result<Option<u64>> {
        let active = self.store.active(ASSISTANT_KIND).await?;
        let mut overlay = active
            .as_ref()
            .map(|stored| stored.overlay().clone())
            .unwrap_or_default();
        if overlay.skills == skills {
            return Ok(None);
        }
        overlay.skills = skills;
        let provenance = || Provenance::operator(SKILLS_ID);
        let version = self
            .store
            .put(ASSISTANT_KIND, &overlay, provenance())
            .await?;
        self.store
            .activate(
                ASSISTANT_KIND,
                version,
                active.map(|stored| stored.version()),
                provenance(),
            )
            .await?;
        Ok(Some(version.get()))
    }

    /// The active version and the `seq` of the last move, from one read.
    ///
    /// Each move logs a row in its swap transaction, so the last row names
    /// the active version and the head together. Two reads could pair an
    /// old version with a newer head. Before the first move, the log is
    /// empty, and the store names the active version.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub async fn pointer(&self) -> Result<ExpectedPointer> {
        Ok(match self.store.last_activation(ASSISTANT_KIND).await? {
            Some(last) => ExpectedPointer {
                active: Some(last.to().get()),
                head: Some(last.seq()),
            },
            None => ExpectedPointer {
                active: self.active().await?,
                head: None,
            },
        })
    }

    /// The head of a move to `version` from `before`, from a read right
    /// after the swap. `own_head` says when the head counts.
    ///
    /// The swap has committed by now. So a failed read never fails the
    /// call: the answer leaves the head out, as when the daemon cannot
    /// tell. An error would invite a retry, and a retry moves the pointer
    /// again or reads the own move as someone else's.
    async fn head_after(&self, before: ExpectedPointer, version: u64) -> Option<u64> {
        match self.pointer().await {
            Ok(now) => own_head(before, version, now),
            Err(error) => {
                tracing::warn!(
                    error = %format!("{error:#}"),
                    "the overlay moved, but reading its head failed"
                );
                None
            }
        }
    }

    /// The number of the active version, or `None`.
    async fn active(&self) -> Result<Option<u64>> {
        Ok(self
            .store
            .active(ASSISTANT_KIND)
            .await?
            .map(|stored| stored.version().get()))
    }
}

/// The head of a move to `version` from the pointer `before`, given the
/// pointer `now` that the daemon reads right after the move.
///
/// The engine returns no `seq` from a swap, so the daemon reads the pointer
/// again in the same job. The loop runs one job at a time, but another
/// process on the same store can move the pointer in between. So the head
/// counts only when `now` is exactly this move: the version is the one just
/// made active, and the head is one past the head of `before`, because the
/// store numbers the moves of a kind 1, 2, 3 and so on. A swap to the
/// active version logs no move, so then the head must not have changed.
/// Any other move after this one makes the head differ, and the answer
/// leaves the head out. It never names the head of a later move.
fn own_head(before: ExpectedPointer, version: u64, now: ExpectedPointer) -> Option<u64> {
    let expected = if before.active == Some(version) {
        before.head
    } else {
        before.head.map_or(Some(1), |head| head.checked_add(1))
    };
    if now.active == Some(version) && now.head == expected {
        now.head
    } else {
        None
    }
}

/// The overlay version `version`, or an error for 0.
fn number(version: u64) -> Result<OverlayVersion> {
    OverlayVersion::new(version).context("overlay versions start at 1")
}

/// The overlay in `reply`, or why it does not decode.
fn decode(reply: ProposerReply) -> Result<ProfileOverlay, String> {
    match reply {
        ProposerReply::Overlay(overlay) => Ok(overlay),
        ProposerReply::Json(text) => serde_json::from_str(&text)
            .map_err(|error| format!("the reply is not overlay JSON: {error}")),
    }
}

/// The list entry of `stored`.
fn entry(stored: &StoredOverlay, active: Option<u64>) -> OverlayEntry {
    let version = stored.version().get();
    OverlayEntry {
        version,
        author: stored.provenance().to_string(),
        created_at: crate::stamp::rfc3339(stored.created_at()),
        active: active == Some(version),
        parts: parts(stored.overlay()),
    }
}

/// The names of the parts that `overlay` sets, in key order.
#[must_use]
pub fn parts(overlay: &ProfileOverlay) -> Vec<String> {
    match serde_json::to_value(overlay) {
        Ok(serde_json::Value::Object(map)) => map.keys().cloned().collect(),
        _ => Vec::new(),
    }
}

/// The evidence text of the last `messages` messages of `transcript`.
///
/// Each message is one block, headed by its role, and each block is whole.
/// The owner bounds the evidence with `[refine] messages`.
#[must_use]
pub fn evidence(transcript: &[ChatMessage], messages: u32) -> Evidence {
    let keep = usize::try_from(messages).unwrap_or(usize::MAX);
    let start = transcript.len().saturating_sub(keep);
    let blocks: Vec<String> = transcript[start..].iter().map(block).collect();
    Evidence::text(blocks.join("\n\n"))
}

/// One message as an evidence block.
fn block(message: &ChatMessage) -> String {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    };
    let parts: Vec<String> = message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => text.clone(),
            ContentPart::ToolCall {
                name, arguments, ..
            } => format!("(calls {name}: {arguments})"),
            ContentPart::ToolResult { content, .. } => format!("(result: {content})"),
        })
        .collect();
    format!("[{role}]\n{}", parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use autumn_harvest_agent::{
        AgentError, AgentProfile, BoxFuture, ChatRequest, ChatResponse, StopReason, TokenUsage,
    };

    use super::*;

    /// A model that gives each scripted reply in turn, and records each
    /// request.
    #[derive(Debug, Default)]
    struct Script {
        replies: Mutex<Vec<String>>,
        seen: Mutex<Vec<ChatRequest>>,
    }

    impl Script {
        fn new(replies: &[&str]) -> Arc<Self> {
            Arc::new(Self {
                replies: Mutex::new(replies.iter().rev().map(|&r| r.to_owned()).collect()),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl AgentModel for Script {
        fn chat<'a>(
            &'a self,
            request: &'a ChatRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
            Box::pin(async move {
                self.seen.lock().unwrap().push(request.clone());
                let text = self.replies.lock().unwrap().pop().unwrap_or_default();
                Ok(ChatResponse {
                    content: vec![ContentPart::Text(text)],
                    stop_reason: StopReason::EndTurn,
                    usage: TokenUsage::new(1, 1),
                })
            })
        }
    }

    fn base(model: Arc<dyn AgentModel>) -> AgentHarness {
        AgentHarness::new(model).agent(AgentProfile::new(ASSISTANT_KIND).system("be brief"))
    }

    fn refine(replies: &[&str]) -> (Refine, Arc<Script>) {
        let script = Script::new(replies);
        let model: Arc<dyn AgentModel> = script.clone();
        let base = base(Arc::clone(&model));
        let store = SqliteOverlayStore::open_in_memory(Arc::new(base.clone())).unwrap();
        let refine = Refine::with_store(store, &RefineConfig::default(), &base, model);
        (refine, script)
    }

    fn talk() -> Vec<ChatMessage> {
        vec![
            ChatMessage::text(ChatRole::User, "what is on today"),
            ChatMessage::text(ChatRole::Assistant, "You have a long answer."),
        ]
    }

    const GUIDE: &str = r#"{"guardrails": "Answer in one sentence."}"#;

    #[tokio::test]
    async fn a_proposal_waits_until_the_owner_activates_it() {
        let (refine, script) = refine(&[GUIDE]);
        let proposal = refine.propose(&talk()).await.unwrap();
        let Proposal::Stored { version, overlay } = proposal else {
            panic!("expected a stored proposal, got {proposal:?}");
        };
        assert_eq!(version.get(), 1);
        assert_eq!(
            overlay.guardrails.as_deref(),
            Some("Answer in one sentence.")
        );
        let (pointer, entries) = refine.list().await.unwrap();
        let active = pointer.active;
        assert_eq!(active, None, "a proposal never activates itself");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].author, format!("operator:{PROPOSER_ID}"));
        assert_eq!(entries[0].parts, vec![String::from("guardrails")]);
        let request = &script.requests()[0];
        let sent = format!("{:?}", request.messages);
        assert!(sent.contains("You have a long answer."), "{sent}");

        assert_eq!(
            refine.activate(1, None, "mark").await.unwrap(),
            Swap::Moved {
                previous: None,
                head: Some(1),
            }
        );
        let (pointer, entries) = refine.list().await.unwrap();
        let active = pointer.active;
        assert_eq!(active, Some(1));
        assert!(entries[0].active);
    }

    #[cfg(feature = "skills")]
    #[tokio::test]
    async fn skills_sync_once_and_a_proposal_cannot_change_them() {
        use autumn_harvest_agent::SkillText;
        use std::collections::BTreeMap;
        // The proposer drops the skills and adds one of its own.
        let (refine, _) = refine(&[
            r#"{"guardrails": "Be short.", "skills": {"rogue": {"summary": "Mine.", "body": "x"}}}"#,
        ]);
        let skills = BTreeMap::from([(
            String::from("deploy"),
            SkillText::new("Ship the site.", "1. Build."),
        )]);
        assert_eq!(refine.set_skills(skills.clone()).await.unwrap(), Some(1));
        assert_eq!(
            refine.set_skills(skills.clone()).await.unwrap(),
            None,
            "no change"
        );
        let (pointer, entries) = refine.list().await.unwrap();
        let active = pointer.active;
        assert_eq!(active, Some(1), "a skills sync activates its version");
        assert_eq!(entries[0].author, format!("operator:{SKILLS_ID}"));

        let Proposal::Stored { overlay, .. } = refine.propose(&talk()).await.unwrap() else {
            panic!("expected a stored proposal");
        };
        assert_eq!(overlay.guardrails.as_deref(), Some("Be short."));
        assert_eq!(
            overlay.skills, skills,
            "the active skills stay, the rogue one goes"
        );
    }

    #[tokio::test]
    async fn a_bad_reply_gets_its_reason_on_the_next_attempt() {
        let (refine, script) = refine(&["not json", GUIDE]);
        let proposal = refine.propose(&talk()).await.unwrap();
        assert!(matches!(proposal, Proposal::Stored { .. }));
        let requests = script.requests();
        assert_eq!(requests.len(), 2);
        let retry = format!("{:?}", requests[1].messages);
        assert!(retry.contains("not overlay JSON"), "{retry}");
    }

    #[tokio::test]
    async fn an_overlay_above_the_ceilings_is_never_stored() {
        let high = r#"{"max_steps": 100000}"#;
        let (refine, script) = refine(&[high, high, high]);
        let error = refine.propose(&talk()).await.unwrap_err().to_string();
        assert!(error.contains("no valid overlay in 3 attempts"), "{error}");
        assert_eq!(script.requests().len(), 3);
        assert_eq!(refine.list().await.unwrap().1.len(), 0);
    }

    #[tokio::test]
    async fn keep_base_stores_nothing() {
        let (refine, _) = refine(&[autumn_harvest_agent::KEEP_BASE]);
        assert_eq!(refine.propose(&talk()).await.unwrap(), Proposal::Kept);
        assert_eq!(refine.list().await.unwrap().1.len(), 0);
    }

    #[tokio::test]
    async fn an_empty_transcript_asks_nothing() {
        let (refine, script) = refine(&[GUIDE]);
        let error = refine.propose(&[]).await.unwrap_err().to_string();
        assert!(error.contains("no transcript"), "{error}");
        assert_eq!(script.requests().len(), 0);
    }

    #[tokio::test]
    async fn the_owner_puts_shows_and_rolls_back_versions() {
        let (refine, _) = refine(&[]);
        assert_eq!(refine.put(GUIDE, "mark").await.unwrap(), 1);
        assert_eq!(refine.put("{}", "mark").await.unwrap(), 2);
        assert!(refine.put("{\"colour\": 1}", "mark").await.is_err());
        refine.activate(1, None, "mark").await.unwrap();
        refine.activate(2, None, "mark").await.unwrap();
        let back = refine.rollback("mark").await.unwrap();
        assert_eq!((back.from, back.to, back.head), (2, 1, Some(3)));
        assert_eq!(refine.pointer().await.unwrap().head, back.head);
        let (entry, json) = refine.show(1).await.unwrap().unwrap();
        assert!(entry.active);
        assert_eq!(entry.author, "operator:mark");
        assert!(json.contains("Answer in one sentence."), "{json}");
        assert!(refine.show(9).await.unwrap().is_none());
        assert!(refine.show(0).await.is_err());
        assert!(refine.activate(9, None, "mark").await.is_err());
    }

    /// Breaks each read of the last move after the next move. A trigger
    /// logs a row behind each move, and the actor kind of that row does
    /// not decode.
    fn break_reads_after_a_move(path: &Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER break_reads AFTER INSERT ON harvest_agent_overlay_activations
             WHEN NEW.actor_kind <> 'broken'
             BEGIN
                 INSERT INTO harvest_agent_overlay_activations
                     (kind, seq, from_version, to_version, actor_kind, actor, activated_at_ms)
                 VALUES (NEW.kind, NEW.seq + 1, NULL, NEW.to_version, 'broken', 'x', 0);
             END;",
        )
        .unwrap();
    }

    #[tokio::test]
    async fn a_move_stands_when_the_read_after_it_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overlays.db");
        let model: Arc<dyn AgentModel> = Script::new(&[]);
        let base = base(Arc::clone(&model));
        let refine = Refine::open(&path, &RefineConfig::default(), &base, model).unwrap();
        for _ in 0..3 {
            refine.put(GUIDE, "mark").await.unwrap();
        }
        refine.activate(1, None, "mark").await.unwrap();
        refine.activate(2, None, "mark").await.unwrap();
        break_reads_after_a_move(&path);

        // The rollback commits, so it answers with no head, not an error.
        let back = refine.rollback("mark").await.unwrap();
        assert_eq!((back.from, back.to, back.head), (2, 1, None));
        assert!(refine.pointer().await.is_err());
        // The guarded move commits too. The trigger row is the head now.
        let expected = ExpectedPointer {
            active: Some(1),
            head: Some(4),
        };
        assert_eq!(
            refine.activate(3, Some(expected), "mark").await.unwrap(),
            Swap::Moved {
                previous: Some(1),
                head: None,
            }
        );
        assert!(refine.show(3).await.unwrap().unwrap().0.active);
        // A read that fails before the move is an error, and nothing moves.
        assert!(refine.activate(2, None, "mark").await.is_err());
        assert!(refine.show(3).await.unwrap().unwrap().0.active);
    }

    #[test]
    fn a_head_counts_only_for_this_move() {
        let pointer = |active, head| ExpectedPointer { active, head };
        // A move from v1 at head 4 to v2 logs head 5.
        let before = pointer(Some(1), Some(4));
        assert_eq!(own_head(before, 2, pointer(Some(2), Some(5))), Some(5));
        // The first move logs head 1.
        let fresh = pointer(None, None);
        assert_eq!(own_head(fresh, 1, pointer(Some(1), Some(1))), Some(1));
        // The active version again logs nothing, so the head stays.
        let active = pointer(Some(2), Some(5));
        assert_eq!(own_head(active, 2, active), Some(5));
        assert_eq!(own_head(fresh, 2, pointer(Some(2), None)), None);
        // Another process moves after this move: away, or away and back.
        assert_eq!(own_head(before, 2, pointer(Some(3), Some(6))), None);
        assert_eq!(own_head(before, 2, pointer(Some(2), Some(7))), None);
        assert_eq!(own_head(active, 2, pointer(Some(2), Some(7))), None);
        // A full counter has no next head.
        let full = pointer(Some(1), Some(u64::MAX));
        assert_eq!(own_head(full, 2, pointer(Some(2), Some(u64::MAX))), None);
    }

    #[test]
    fn the_evidence_keeps_the_last_messages() {
        let transcript: Vec<ChatMessage> = (0..5)
            .map(|n| ChatMessage::text(ChatRole::User, format!("message {n}")))
            .collect();
        let text = evidence(&transcript, 2).render().into_owned();
        assert_eq!(text, "[user]\nmessage 3\n\n[user]\nmessage 4");
    }

    #[test]
    fn the_evidence_keeps_each_part_whole() {
        let long = "x".repeat(100_000);
        let call = ChatMessage {
            role: ChatRole::Assistant,
            content: vec![
                ContentPart::ToolCall {
                    id: String::from("c"),
                    name: String::from("web_fetch"),
                    arguments: serde_json::json!({ "url": long }),
                },
                ContentPart::ToolResult {
                    tool_call_id: String::from("c"),
                    content: long.clone(),
                },
            ],
        };
        let text = evidence(&[call], 10).render().into_owned();
        assert!(text.contains("(calls web_fetch: "));
        assert!(text.contains(&format!("(result: {long})")));

        let big = "é".repeat(100_000);
        let many = vec![
            ChatMessage::text(ChatRole::User, "old"),
            ChatMessage::text(ChatRole::User, big),
        ];
        let text = evidence(&many, 10).render().into_owned();
        assert!(text.starts_with("[user]\nold"));
    }

    #[test]
    fn the_store_sits_next_to_the_database() {
        assert_eq!(
            path_for(Path::new("/var/lg/lg.db")),
            PathBuf::from("/var/lg/overlays.db")
        );
        assert_eq!(path_for(Path::new("lg.db")), PathBuf::from("overlays.db"));
    }

    #[test]
    fn the_parts_name_each_set_key() {
        let overlay: ProfileOverlay =
            serde_json::from_str(r#"{"system": "s", "max_steps": 3}"#).unwrap();
        assert_eq!(parts(&overlay), vec!["max_steps", "system"]);
        assert_eq!(parts(&ProfileOverlay::default()).len(), 0);
    }
}
