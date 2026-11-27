//! An embedder over the OpenAI `/v1/embeddings` API (ADR 0047).
//!
//! OpenAI, OpenRouter, Ollama, llama.cpp and vLLM all speak it. The client
//! posts the texts in one request and reads one vector for each, in the
//! order of their `index`. A vector of another length fails the call, so a
//! wrong `dimensions` in the config shows at once.
//!
//! The request does not send `dimensions`. Only some models take it, and
//! others refuse a request that has it.

use std::time::Duration;

use autumn_harvest_agent::model::BoxFuture;
use autumn_harvest_agent::{AgentError, ErrorKind};
use littleguy_core::embed::Embedder;
use serde_json::{Value, json};

use crate::http;

/// How long one embeddings request may take. An embedding is quick, and an
/// edit waits for it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);

/// An embedder that calls an OpenAI-compatible endpoint.
pub struct OpenAiEmbedder {
    client: reqwest::Client,
    endpoint: String,
    model: String,
    dimensions: u32,
    key: Option<String>,
}

impl std::fmt::Debug for OpenAiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiEmbedder")
            .field("endpoint", &self.endpoint)
            .field("model", &self.model)
            .field("dimensions", &self.dimensions)
            .field("key", &self.key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl OpenAiEmbedder {
    /// An embedder of `model` at the base URL `url`, for example
    /// `https://api.openai.com/v1`. `key`, when set, goes as a bearer
    /// token.
    #[must_use]
    pub fn new(
        url: impl Into<String>,
        model: impl Into<String>,
        dimensions: u32,
        key: Option<String>,
    ) -> Self {
        let url = url.into();
        Self {
            client: http::client(),
            endpoint: format!("{}/embeddings", url.trim_end_matches('/')),
            model: model.into(),
            dimensions,
            key: key.filter(|key| !key.trim().is_empty()),
        }
    }

    async fn call(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AgentError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let mut request = self
            .client
            .post(&self.endpoint)
            .timeout(REQUEST_TIMEOUT)
            .json(&json!({ "model": self.model, "input": texts }));
        if let Some(key) = &self.key {
            request = request.bearer_auth(key);
        }
        let response = request.send().await.map_err(|e| {
            AgentError::new(
                ErrorKind::Transport,
                format!("the embeddings request failed: {e}"),
            )
        })?;
        let status = response.status().as_u16();
        let body = http::read_body(response).await?;
        if !(200..300).contains(&status) {
            return Err(AgentError::new(
                http::error_kind(status),
                format!("the embeddings endpoint answered {status}"),
            ));
        }
        let value: Value = serde_json::from_str(&body).map_err(|e| {
            AgentError::new(
                ErrorKind::Decode,
                format!("the embeddings answer is not JSON: {e}"),
            )
        })?;
        vectors(&value, texts.len(), self.dimensions as usize)
    }
}

/// The vectors of an embeddings answer, in the order of their `index`.
///
/// # Errors
///
/// Returns a `Decode` error when the answer has another number of vectors,
/// a vector of another length, or no `data` list.
pub fn vectors(
    value: &Value,
    count: usize,
    dimensions: usize,
) -> Result<Vec<Vec<f32>>, AgentError> {
    let decode = |why: String| AgentError::new(ErrorKind::Decode, why);
    let data = value
        .get("data")
        .and_then(Value::as_array)
        .ok_or_else(|| decode("the embeddings answer has no data list".to_owned()))?;
    if data.len() != count {
        return Err(decode(format!(
            "the embeddings answer has {} vectors for {count} texts",
            data.len()
        )));
    }
    let mut slots: Vec<Option<Vec<f32>>> = vec![None; count];
    for (n, item) in data.iter().enumerate() {
        let index = item
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .unwrap_or(n);
        let numbers = item
            .get("embedding")
            .and_then(Value::as_array)
            .ok_or_else(|| decode("an embeddings item has no embedding".to_owned()))?;
        #[expect(
            clippy::cast_possible_truncation,
            reason = "an embedding is f32; JSON carries it as f64"
        )]
        let vector: Vec<f32> = numbers
            .iter()
            .filter_map(Value::as_f64)
            .map(|x| x as f32)
            .collect();
        if vector.len() != dimensions || numbers.len() != dimensions {
            return Err(decode(format!(
                "the model answered a vector of {} dimensions, and memory.embeddings.dimensions \
                 is {dimensions}",
                numbers.len()
            )));
        }
        let slot = slots
            .get_mut(index)
            .ok_or_else(|| decode(format!("an embeddings item has the index {index}")))?;
        *slot = Some(vector);
    }
    slots
        .into_iter()
        .map(|slot| slot.ok_or_else(|| decode("two embeddings items share an index".to_owned())))
        .collect()
}

impl Embedder for OpenAiEmbedder {
    fn name(&self) -> &str {
        &self.model
    }

    fn dimensions(&self) -> u32 {
        self.dimensions
    }

    fn embed<'a>(
        &'a self,
        texts: &'a [String],
    ) -> BoxFuture<'a, Result<Vec<Vec<f32>>, AgentError>> {
        Box::pin(self.call(texts))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vectors_follow_their_index() {
        let answer = json!({ "data": [
            { "index": 1, "embedding": [0.0, 1.0] },
            { "index": 0, "embedding": [1.0, 0.0] },
        ]});
        let got = vectors(&answer, 2, 2).unwrap_or_default();
        assert_eq!(got, vec![vec![1.0, 0.0], vec![0.0, 1.0]]);
    }

    #[test]
    fn a_vector_of_another_length_names_the_config_key() {
        let answer = json!({ "data": [{ "index": 0, "embedding": [1.0, 0.0, 0.0] }] });
        let error = vectors(&answer, 1, 2).err().map(|e| e.message().to_owned());
        assert!(error.is_some_and(|m| m.contains("memory.embeddings.dimensions")));
    }

    #[test]
    fn a_bad_answer_is_a_decode_error() {
        for (answer, count) in [
            (json!({}), 1),
            (json!({ "data": [] }), 1),
            (json!({ "data": [{ "index": 0 }] }), 1),
            (json!({ "data": [{ "index": 5, "embedding": [1.0] }] }), 1),
            (
                json!({ "data": [
                    { "index": 0, "embedding": [1.0] },
                    { "index": 0, "embedding": [1.0] },
                ]}),
                2,
            ),
            (json!({ "data": [{ "index": 0, "embedding": ["x"] }] }), 1),
        ] {
            let error = vectors(&answer, count, 1).err();
            assert_eq!(error.map(|e| e.kind()), Some(ErrorKind::Decode), "{answer}");
        }
    }

    #[test]
    fn the_debug_output_hides_the_key() {
        let embedder = OpenAiEmbedder::new("https://e.com/v1/", "m", 8, Some("sk-secret".into()));
        let text = format!("{embedder:?}");
        assert!(!text.contains("sk-secret"));
        assert!(text.contains("https://e.com/v1/embeddings"));
        assert_eq!(embedder.name(), "m");
        assert_eq!(embedder.dimensions(), 8);
    }
}
