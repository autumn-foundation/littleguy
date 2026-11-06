//! A model that answers from the request alone.

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use autumn_harvest_agent::{
    AgentError, AgentModel, BoxFuture, ChatRequest, ChatResponse, ChatRole, ContentPart, ErrorKind,
    StopReason, TokenUsage,
};

/// The model id of the offline model. It is stable, so a response cache can
/// key on it.
pub const OFFLINE_MODEL_ID: &str = "littleguy-offline-1";

/// Echoes the last user message.
///
/// The model counts its calls. With a call log, it also appends one line per
/// call to that file, so a test in another process can count them across a
/// restart.
#[derive(Debug, Default)]
pub struct OfflineModel {
    calls: AtomicU64,
    call_log: Option<PathBuf>,
}

impl OfflineModel {
    /// A model with no call log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A model that appends one line per call to `path`.
    #[must_use]
    pub fn with_call_log(path: impl Into<PathBuf>) -> Self {
        Self {
            calls: AtomicU64::new(0),
            call_log: Some(path.into()),
        }
    }

    /// The calls in this process.
    #[must_use]
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::SeqCst)
    }

    /// The answer to `request`.
    #[must_use]
    pub fn answer(request: &ChatRequest) -> String {
        let last = request
            .messages
            .iter()
            .rev()
            .find(|message| message.role == ChatRole::User)
            .and_then(|message| {
                message.content.iter().find_map(|part| match part {
                    ContentPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
            })
            .unwrap_or_default();
        format!("littleguy (offline): you said '{last}'")
    }

    fn log_call(&self, answer: &str) -> Result<(), AgentError> {
        let Some(path) = &self.call_log else {
            return Ok(());
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|error| AgentError::new(ErrorKind::Config, error.to_string()))?;
        writeln!(file, "{answer}")
            .map_err(|error| AgentError::new(ErrorKind::Config, error.to_string()))
    }
}

impl AgentModel for OfflineModel {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let answer = Self::answer(request);
            self.log_call(&answer)?;
            Ok(ChatResponse {
                content: vec![ContentPart::Text(answer)],
                stop_reason: StopReason::EndTurn,
                usage: TokenUsage::new(1, 1),
            })
        })
    }

    fn model_id(&self) -> Option<&str> {
        Some(OFFLINE_MODEL_ID)
    }
}

#[cfg(test)]
mod tests {
    use autumn_harvest_agent::ChatMessage;

    use super::*;

    fn request(messages: Vec<ChatMessage>) -> ChatRequest {
        ChatRequest {
            messages,
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            draw: None,
        }
    }

    #[tokio::test]
    async fn it_echoes_the_last_user_message() {
        let model = OfflineModel::new();
        let reply = model
            .chat(&request(vec![
                ChatMessage::text(ChatRole::System, "be kind"),
                ChatMessage::text(ChatRole::User, "first"),
                ChatMessage::text(ChatRole::Assistant, "ok"),
                ChatMessage::text(ChatRole::User, "second"),
            ]))
            .await
            .unwrap();
        assert_eq!(
            reply.content,
            vec![ContentPart::Text(String::from(
                "littleguy (offline): you said 'second'"
            ))]
        );
        assert_eq!(reply.stop_reason, StopReason::EndTurn);
        assert_eq!(model.calls(), 1);
        assert_eq!(model.model_id(), Some(OFFLINE_MODEL_ID));
    }

    #[tokio::test]
    async fn a_request_with_no_user_text_gets_an_empty_echo() {
        let reply = OfflineModel::new()
            .chat(&request(Vec::new()))
            .await
            .unwrap();
        assert_eq!(
            reply.content,
            vec![ContentPart::Text(String::from(
                "littleguy (offline): you said ''"
            ))]
        );
    }

    #[tokio::test]
    async fn the_call_log_gets_one_line_per_call() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("calls.log");
        let model = OfflineModel::with_call_log(&path);
        let hi = request(vec![ChatMessage::text(ChatRole::User, "hi")]);
        model.chat(&hi).await.unwrap();
        model.chat(&hi).await.unwrap();
        let log = std::fs::read_to_string(&path).unwrap();
        assert_eq!(log.lines().count(), 2);
    }

    #[tokio::test]
    async fn an_unwritable_call_log_fails_the_call_as_config() {
        let dir = tempfile::tempdir().unwrap();
        let model = OfflineModel::with_call_log(dir.path());
        let hi = request(vec![ChatMessage::text(ChatRole::User, "hi")]);
        let error = model.chat(&hi).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Config);
    }
}
