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

use crate::protocol::OverlayEntry;

/// The file of the overlay store, next to the database.
pub const FILE_NAME: &str = "overlays.db";

/// The operator id of a version that `lg refine propose` stores. The owner
/// still activates it, so a person decides each change.
pub const PROPOSER_ID: &str = "lg-refine";

/// The attempts of one proposal. A reply that does not decode or does not
/// fit the ceilings gets its reason back on the next attempt.
pub const MAX_ATTEMPTS: u32 = 3;

/// The largest evidence text, in bytes. The oldest messages go first.
pub const MAX_EVIDENCE_BYTES: usize = 64 * 1024;

/// The largest text of one tool call or tool result in the evidence, in
/// characters.
pub const MAX_PART_CHARS: usize = 500;

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
            let overlay = match decode(reply) {
                Ok(overlay) => overlay,
                Err(reason) => {
                    rejection = Some(reason);
                    continue;
                }
            };
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

    /// Every version, oldest first, and the active one.
    ///
    /// # Errors
    ///
    /// Returns an error when the store fails.
    pub async fn list(&self) -> Result<(Option<u64>, Vec<OverlayEntry>)> {
        let active = self.active().await?;
        let entries = self
            .store
            .list(ASSISTANT_KIND)
            .await?
            .iter()
            .map(|stored| entry(stored, active))
            .collect();
        Ok((active, entries))
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
    /// # Errors
    ///
    /// Returns an error for an unknown version, for one that no longer fits
    /// the ceilings, when the pointer moved meanwhile, or when the store
    /// fails.
    pub async fn activate(&self, version: u64, owner: &str) -> Result<()> {
        let number = number(version)?;
        let expected = self
            .store
            .active(ASSISTANT_KIND)
            .await?
            .map(|stored| stored.version());
        self.store
            .activate(
                ASSISTANT_KIND,
                number,
                expected,
                Provenance::operator(owner),
            )
            .await?;
        Ok(())
    }

    /// Activates the version that was active before the current one, by
    /// `owner`.
    ///
    /// # Errors
    ///
    /// Returns an error when no earlier activation exists, or when the store
    /// fails.
    pub async fn rollback(&self, owner: &str) -> Result<Rollback> {
        Ok(self
            .store
            .rollback(ASSISTANT_KIND, Provenance::operator(owner))
            .await?)
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
        created_at: stored.created_at().to_rfc3339(),
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
/// Each message is one block, headed by its role. A tool call or a tool
/// result is cut to [`MAX_PART_CHARS`]. The oldest blocks go first when the
/// text passes [`MAX_EVIDENCE_BYTES`].
#[must_use]
pub fn evidence(transcript: &[ChatMessage], messages: u32) -> Evidence {
    let keep = usize::try_from(messages).unwrap_or(usize::MAX);
    let start = transcript.len().saturating_sub(keep);
    let mut blocks: Vec<String> = transcript[start..].iter().map(block).collect();
    let mut total: usize = blocks.iter().map(|block| block.len() + 2).sum();
    let mut first = 0;
    while total > MAX_EVIDENCE_BYTES && first + 1 < blocks.len() {
        total -= blocks[first].len() + 2;
        first += 1;
    }
    let mut text = blocks.split_off(first).join("\n\n");
    if text.len() > MAX_EVIDENCE_BYTES {
        let cut = text.len() - MAX_EVIDENCE_BYTES;
        let cut = (cut..=text.len())
            .find(|&index| text.is_char_boundary(index))
            .unwrap_or(text.len());
        text.drain(..cut);
    }
    Evidence::text(text)
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
            } => format!("(calls {name}: {})", cut(&arguments.to_string())),
            ContentPart::ToolResult { content, .. } => format!("(result: {})", cut(content)),
        })
        .collect();
    format!("[{role}]\n{}", parts.join("\n"))
}

/// `text` cut to [`MAX_PART_CHARS`] characters.
fn cut(text: &str) -> String {
    match text.char_indices().nth(MAX_PART_CHARS) {
        Some((index, _)) => format!("{}...", &text[..index]),
        None => text.to_owned(),
    }
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
        let (active, entries) = refine.list().await.unwrap();
        assert_eq!(active, None, "a proposal never activates itself");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].author, format!("operator:{PROPOSER_ID}"));
        assert_eq!(entries[0].parts, vec![String::from("guardrails")]);
        let request = &script.requests()[0];
        let sent = format!("{:?}", request.messages);
        assert!(sent.contains("You have a long answer."), "{sent}");

        refine.activate(1, "mark").await.unwrap();
        let (active, entries) = refine.list().await.unwrap();
        assert_eq!(active, Some(1));
        assert!(entries[0].active);
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
        refine.activate(1, "mark").await.unwrap();
        refine.activate(2, "mark").await.unwrap();
        let back = refine.rollback("mark").await.unwrap();
        assert_eq!((back.from.get(), back.to.get()), (2, 1));
        let (entry, json) = refine.show(1).await.unwrap().unwrap();
        assert!(entry.active);
        assert_eq!(entry.author, "operator:mark");
        assert!(json.contains("Answer in one sentence."), "{json}");
        assert!(refine.show(9).await.unwrap().is_none());
        assert!(refine.show(0).await.is_err());
        assert!(refine.activate(9, "mark").await.is_err());
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
    fn the_evidence_cuts_tool_parts_and_stays_under_its_bound() {
        let long = "x".repeat(MAX_PART_CHARS * 2);
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
                    content: long,
                },
            ],
        };
        let text = evidence(&[call], 10).render().into_owned();
        assert!(text.contains("(calls web_fetch: "), "{text}");
        assert!(text.len() < MAX_PART_CHARS * 3, "{}", text.len());

        let big = "é".repeat(MAX_EVIDENCE_BYTES);
        let many = vec![
            ChatMessage::text(ChatRole::User, "old"),
            ChatMessage::text(ChatRole::User, big),
        ];
        let text = evidence(&many, 10).render().into_owned();
        assert!(text.len() <= MAX_EVIDENCE_BYTES);
        assert!(!text.contains("old"));
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
