//! A model over the OpenAI Chat Completions API, for OpenAI and OpenRouter.
//!
//! OpenRouter speaks the same wire as OpenAI, so one adapter serves both.
//! [`Provider`] picks the base URL, the key variable and the cap field.
//! [`OpenAiModel::base_url`] points it at any other server that speaks the
//! API, such as a local one. The mapping functions are pure, so the tests
//! need no network and no key.
//!
//! The harness's `ChatMessage` maps to this API with no loss. A tool call
//! carries its arguments as a JSON string here, so the adapter encodes them
//! on the way out and parses them on the way back. Arguments that are not
//! JSON fail as a `Decode` error, so the harness never runs a tool on them.

use autumn_harvest_agent::{
    AgentError, AgentModel, BoxFuture, ChatRequest, ChatResponse, ChatRole, ContentPart, ErrorKind,
    StopReason, TokenUsage,
};
use serde_json::{Value, json};

use crate::http::{client, error_kind, read_body};

/// The base URL of the OpenAI API.
pub const OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// The base URL of the OpenRouter API.
pub const OPENROUTER_BASE_URL: &str = "https://openrouter.ai/api/v1";
/// The name that OpenRouter shows for this app.
pub const APP_TITLE: &str = "littleguy";

/// Who serves the API. Each provider has its own defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    /// OpenAI, or a server that copies its API.
    OpenAi,
    /// OpenRouter, which routes to many vendors.
    OpenRouter,
}

impl Provider {
    /// The default base URL.
    #[must_use]
    pub const fn base_url(self) -> &'static str {
        match self {
            Self::OpenAi => OPENAI_BASE_URL,
            Self::OpenRouter => OPENROUTER_BASE_URL,
        }
    }

    /// The environment variable that holds the key.
    #[must_use]
    pub const fn key_var(self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_API_KEY",
            Self::OpenRouter => "OPENROUTER_API_KEY",
        }
    }

    /// The name of the output cap. OpenAI reasoning models refuse the old
    /// `max_tokens`. OpenRouter documents `max_tokens` for every vendor.
    #[must_use]
    pub const fn cap_field(self) -> &'static str {
        match self {
            Self::OpenAi => "max_completion_tokens",
            Self::OpenRouter => "max_tokens",
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "openai",
            Self::OpenRouter => "openrouter",
        }
    }
}

/// A model over the Chat Completions API.
#[derive(Clone)]
pub struct OpenAiModel {
    provider: Provider,
    api_key: String,
    base_url: String,
    model: String,
    /// The provider and the model, so a response cache keeps them apart.
    id: String,
    client: reqwest::Client,
}

/// The key stays out of every log line.
impl std::fmt::Debug for OpenAiModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiModel")
            .field("provider", &self.provider)
            .field("api_key", &"<redacted>")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl OpenAiModel {
    /// A model `model` of `provider`, with `api_key`.
    #[must_use]
    pub fn new(provider: Provider, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let model = model.into();
        Self {
            provider,
            api_key: api_key.into(),
            base_url: provider.base_url().into(),
            id: format!("{}:{model}", provider.label()),
            model,
            client: client(),
        }
    }

    /// Send each request under `base_url` in place of the provider default.
    #[must_use]
    pub fn base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// The URL of the completions endpoint.
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }
}

impl AgentModel for OpenAiModel {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            let body = request_body(request, &self.model, self.provider);
            let mut call = self.client.post(self.endpoint()).bearer_auth(&self.api_key);
            if self.provider == Provider::OpenRouter {
                call = call.header("X-Title", APP_TITLE);
            }
            let response = call.json(&body).send().await.map_err(|e| {
                AgentError::new(ErrorKind::Transport, format!("request failed: {e}"))
            })?;
            let status = response.status().as_u16();
            let text = read_body(response).await?;
            if !(200..300).contains(&status) {
                // The body names the error. It holds no secret: the key is a header.
                let detail: String = text.chars().take(500).collect();
                return Err(AgentError::new(
                    error_kind(status),
                    format!("HTTP {status}: {detail}"),
                ));
            }
            let body: Value = serde_json::from_str(&text)
                .map_err(|e| AgentError::new(ErrorKind::Decode, format!("not JSON: {e}")))?;
            parse_response(&body)
        })
    }

    fn model_id(&self) -> Option<&str> {
        Some(&self.id)
    }
}

/// The text parts of `parts`, joined.
fn text_of(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The messages of `request` as the API takes them.
///
/// One harness message can become many here. Each tool result is its own
/// `tool` message, and the API wants them in the order of the calls.
#[must_use]
pub fn wire_messages(request: &ChatRequest) -> Vec<Value> {
    let mut out = Vec::new();
    for message in &request.messages {
        match message.role {
            ChatRole::System => {
                out.push(json!({"role": "system", "content": text_of(&message.content)}));
            }
            ChatRole::User => {
                out.push(json!({"role": "user", "content": text_of(&message.content)}));
            }
            ChatRole::Assistant => out.push(assistant_message(&message.content)),
            ChatRole::Tool => {
                for part in &message.content {
                    if let ContentPart::ToolResult {
                        tool_call_id,
                        content,
                    } = part
                    {
                        out.push(json!({
                            "role": "tool",
                            "tool_call_id": tool_call_id,
                            "content": content,
                        }));
                    }
                }
            }
        }
    }
    out
}

/// One assistant message. A turn with only tool calls has `null` content.
fn assistant_message(parts: &[ContentPart]) -> Value {
    let text = text_of(parts);
    let calls: Vec<Value> = parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::ToolCall {
                id,
                name,
                arguments,
            } => Some(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": arguments.to_string()},
            })),
            _ => None,
        })
        .collect();
    let mut message = json!({"role": "assistant", "content": text});
    if !calls.is_empty() {
        if text.is_empty() {
            message["content"] = Value::Null;
        }
        message["tool_calls"] = json!(calls);
    }
    message
}

/// The JSON body of one request.
#[must_use]
pub fn request_body(request: &ChatRequest, model: &str, provider: Provider) -> Value {
    let mut body = json!({
        "model": model,
        "messages": wire_messages(request),
    });
    if let Some(cap) = request.max_tokens {
        body[provider.cap_field()] = json!(cap);
    }
    if let Some(temperature) = request.temperature {
        body["temperature"] = json!(temperature);
    }
    if !request.tools.is_empty() {
        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    },
                })
            })
            .collect();
        body["tools"] = json!(tools);
    }
    body
}

/// The error of an `error` object in a body. OpenRouter can send one with
/// status 200, when the upstream vendor fails mid-request.
fn body_error(error: &Value) -> AgentError {
    let code = error["code"]
        .as_u64()
        .and_then(|code| u16::try_from(code).ok());
    let kind = code.map_or(ErrorKind::Provider, error_kind);
    let detail: String = error["message"]
        .as_str()
        .unwrap_or("no message")
        .chars()
        .take(500)
        .collect();
    AgentError::new(kind, format!("provider error: {detail}"))
}

/// Map one response body to a [`ChatResponse`].
///
/// # Errors
///
/// - An `error` object is the error of its code.
/// - A refusal or a `content_filter` stop is a `Provider` error, so the run
///   fails and says why.
/// - A body with no choice, or tool arguments that are not JSON, is a
///   `Decode` error.
pub fn parse_response(body: &Value) -> Result<ChatResponse, AgentError> {
    if body["error"].is_object() {
        return Err(body_error(&body["error"]));
    }
    let choice = &body["choices"][0];
    let message = &choice["message"];
    if !message.is_object() {
        return Err(AgentError::new(
            ErrorKind::Decode,
            "the response has no message",
        ));
    }
    if let Some(refusal) = message["refusal"].as_str() {
        let detail: String = refusal.chars().take(500).collect();
        return Err(AgentError::new(
            ErrorKind::Provider,
            format!("the model declined the request: {detail}"),
        ));
    }
    let stop_reason = match choice["finish_reason"].as_str() {
        Some("stop") => StopReason::EndTurn,
        Some("tool_calls" | "function_call") => StopReason::ToolUse,
        Some("length") => StopReason::MaxTokens,
        Some("content_filter") => {
            return Err(AgentError::new(
                ErrorKind::Provider,
                "the provider filtered the answer",
            ));
        }
        _ => StopReason::Unknown,
    };
    let mut content = Vec::new();
    if let Some(text) = message["content"].as_str()
        && !text.is_empty()
    {
        content.push(ContentPart::Text(text.to_owned()));
    }
    for call in message["tool_calls"].as_array().into_iter().flatten() {
        content.push(tool_call(call)?);
    }
    let count = |value: &Value| u32::try_from(value.as_u64().unwrap_or(0)).unwrap_or(u32::MAX);
    let usage = &body["usage"];
    // The prompt count holds the cached tokens already.
    let mut tokens = TokenUsage::new(
        count(&usage["prompt_tokens"]),
        count(&usage["completion_tokens"]),
    );
    tokens.cache_read_tokens = count(&usage["prompt_tokens_details"]["cached_tokens"]);
    Ok(ChatResponse {
        content,
        stop_reason,
        usage: tokens,
    })
}

/// One tool call of a response.
fn tool_call(call: &Value) -> Result<ContentPart, AgentError> {
    let missing =
        |what: &str| AgentError::new(ErrorKind::Decode, format!("a tool call has no {what}"));
    let id = call["id"].as_str().ok_or_else(|| missing("id"))?;
    let name = call["function"]["name"]
        .as_str()
        .ok_or_else(|| missing("name"))?;
    let raw = call["function"]["arguments"].as_str().unwrap_or("");
    let arguments = if raw.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(raw).map_err(|e| {
            AgentError::new(
                ErrorKind::Decode,
                format!("the arguments of {name} are not JSON: {e}"),
            )
        })?
    };
    Ok(ContentPart::ToolCall {
        id: id.to_owned(),
        name: name.to_owned(),
        arguments,
    })
}

#[cfg(test)]
mod tests {
    use autumn_harvest_agent::{ChatMessage, ToolDefinition};

    use super::*;

    fn request() -> ChatRequest {
        ChatRequest {
            messages: vec![
                ChatMessage::text(ChatRole::System, "You are littleguy."),
                ChatMessage::text(ChatRole::User, "Read the file."),
                ChatMessage {
                    role: ChatRole::Assistant,
                    content: vec![
                        ContentPart::ToolCall {
                            id: "call_1".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "a.txt"}),
                        },
                        ContentPart::ToolCall {
                            id: "call_2".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "b.txt"}),
                        },
                    ],
                },
                ChatMessage {
                    role: ChatRole::Tool,
                    content: vec![
                        ContentPart::ToolResult {
                            tool_call_id: "call_1".into(),
                            content: "A".into(),
                        },
                        ContentPart::ToolResult {
                            tool_call_id: "call_2".into(),
                            content: "B".into(),
                        },
                    ],
                },
            ],
            tools: vec![ToolDefinition {
                name: "read_file".into(),
                description: "Read a file.".into(),
                input_schema: json!({"type": "object"}),
            }],
            max_tokens: Some(512),
            temperature: Some(0.5),
            draw: None,
        }
    }

    #[test]
    fn the_request_maps_roles_calls_results_and_tools() {
        let body = request_body(&request(), "gpt-x", Provider::OpenAi);
        assert_eq!(body["model"], "gpt-x");
        assert_eq!(body["max_completion_tokens"], 512);
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["temperature"], 0.5);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5, "each tool result is its own message");
        assert_eq!(messages[0]["role"], "system");
        assert_eq!(messages[0]["content"], "You are littleguy.");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[2]["role"], "assistant");
        assert!(messages[2]["content"].is_null());
        let call = &messages[2]["tool_calls"][1];
        assert_eq!(call["id"], "call_2");
        assert_eq!(call["type"], "function");
        assert_eq!(call["function"]["arguments"], "{\"path\":\"b.txt\"}");
        assert_eq!(messages[3]["role"], "tool");
        assert_eq!(messages[3]["tool_call_id"], "call_1");
        assert_eq!(messages[4]["content"], "B");
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["parameters"]["type"], "object");
    }

    #[test]
    fn openrouter_takes_max_tokens_and_a_bare_request_sends_no_settings() {
        let body = request_body(&request(), "x/y", Provider::OpenRouter);
        assert_eq!(body["max_tokens"], 512);
        assert!(body.get("max_completion_tokens").is_none());
        let bare = ChatRequest {
            messages: vec![ChatMessage::text(ChatRole::User, "hi")],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            draw: None,
        };
        let body = request_body(&bare, "x/y", Provider::OpenRouter);
        assert!(body.get("max_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn an_assistant_text_with_a_call_keeps_both() {
        let message = assistant_message(&[
            ContentPart::Text("Looking.".into()),
            ContentPart::ToolCall {
                id: "c".into(),
                name: "t".into(),
                arguments: json!({}),
            },
        ]);
        assert_eq!(message["content"], "Looking.");
        assert_eq!(message["tool_calls"][0]["id"], "c");
        let plain = assistant_message(&[ContentPart::Text("Hi.".into())]);
        assert_eq!(plain["content"], "Hi.");
        assert!(plain.get("tool_calls").is_none());
    }

    #[test]
    fn a_tool_call_response_maps_to_calls_and_usage() {
        let body = json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_9",
                        "type": "function",
                        "function": {"name": "read_file", "arguments": "{\"path\":\"a\"}"},
                    }, {
                        "id": "call_10",
                        "type": "function",
                        "function": {"name": "now", "arguments": ""},
                    }],
                },
            }],
            "usage": {"prompt_tokens": 30, "completion_tokens": 7, "prompt_tokens_details": {"cached_tokens": 20}},
        });
        let response = parse_response(&body).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.content,
            vec![
                ContentPart::ToolCall {
                    id: "call_9".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a"}),
                },
                ContentPart::ToolCall {
                    id: "call_10".into(),
                    name: "now".into(),
                    arguments: json!({}),
                },
            ]
        );
        assert_eq!(response.usage.input_tokens, 30);
        assert_eq!(response.usage.output_tokens, 7);
        assert_eq!(response.usage.cache_read_tokens, 20);
    }

    #[test]
    fn finish_reasons_map() {
        let with = |reason: &str| json!({"choices": [{"finish_reason": reason, "message": {"content": "x"}}]});
        assert_eq!(
            parse_response(&with("stop")).unwrap().stop_reason,
            StopReason::EndTurn
        );
        assert_eq!(
            parse_response(&with("length")).unwrap().stop_reason,
            StopReason::MaxTokens
        );
        assert_eq!(
            parse_response(&with("odd")).unwrap().stop_reason,
            StopReason::Unknown
        );
        let filtered = parse_response(&with("content_filter")).unwrap_err();
        assert_eq!(filtered.kind(), ErrorKind::Provider);
    }

    #[test]
    fn a_refusal_an_error_body_and_bad_arguments_are_errors() {
        let refused = json!({"choices": [{"finish_reason": "stop", "message": {"content": null, "refusal": "no"}}]});
        assert_eq!(
            parse_response(&refused).unwrap_err().kind(),
            ErrorKind::Provider
        );
        let upstream = json!({"error": {"code": 502, "message": "upstream down"}});
        let error = parse_response(&upstream).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert!(error.message().contains("upstream down"));
        let odd = json!({"error": {"code": "bad", "message": "x"}});
        assert_eq!(
            parse_response(&odd).unwrap_err().kind(),
            ErrorKind::Provider
        );
        let bad = json!({"choices": [{"finish_reason": "tool_calls", "message": {"tool_calls": [
            {"id": "c", "function": {"name": "t", "arguments": "{not json"}}
        ]}}]});
        assert_eq!(parse_response(&bad).unwrap_err().kind(), ErrorKind::Decode);
        let nameless =
            json!({"choices": [{"message": {"tool_calls": [{"id": "c", "function": {}}]}}]});
        assert_eq!(
            parse_response(&nameless).unwrap_err().kind(),
            ErrorKind::Decode
        );
        let idless =
            json!({"choices": [{"message": {"tool_calls": [{"function": {"name": "t"}}]}}]});
        assert_eq!(
            parse_response(&idless).unwrap_err().kind(),
            ErrorKind::Decode
        );
        assert_eq!(
            parse_response(&json!({"choices": []})).unwrap_err().kind(),
            ErrorKind::Decode
        );
    }

    #[test]
    fn the_provider_sets_the_defaults_and_the_id() {
        let model = OpenAiModel::new(Provider::OpenRouter, "k", "anthropic/claude-opus-5.5");
        assert_eq!(
            model.endpoint(),
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(
            model.model_id(),
            Some("openrouter:anthropic/claude-opus-5.5")
        );
        let local = OpenAiModel::new(Provider::OpenAi, "k", "m").base_url("http://h:1/v1/");
        assert_eq!(local.endpoint(), "http://h:1/v1/chat/completions");
        assert_eq!(Provider::OpenAi.key_var(), "OPENAI_API_KEY");
        assert_eq!(Provider::OpenRouter.key_var(), "OPENROUTER_API_KEY");
    }

    #[test]
    fn the_key_stays_out_of_debug_output() {
        let model = OpenAiModel::new(Provider::OpenAi, "sk-secret", "m");
        assert!(!format!("{model:?}").contains("sk-secret"));
    }
}
