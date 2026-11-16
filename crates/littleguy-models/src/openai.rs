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
//!
//! `chat_stream` sends the same request with `"stream": true` and reads the
//! Server-Sent Events. It passes each text delta to the caller. It joins
//! the chunks into the body that a plain request returns, and maps that
//! body with the same code.

use autumn_harvest_agent::{
    AgentError, AgentModel, BoxFuture, ChatRequest, ChatResponse, ChatRole, ContentPart, ErrorKind,
    StopReason, TokenUsage,
};
use serde_json::{Value, json};

use crate::http::{MAX_BODY_BYTES, SseFrames, client, error_kind, read_body};

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
    /// How hard a reasoning model thinks before it answers. `None` leaves
    /// the provider default.
    reasoning: Option<Reasoning>,
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
            reasoning: None,
            client: client(),
        }
    }

    /// Ask a reasoning model for `effort`. Hidden reasoning is output that
    /// the model writes before its answer, so less of it answers sooner.
    #[must_use]
    pub const fn reasoning(mut self, effort: Reasoning) -> Self {
        self.reasoning = Some(effort);
        self
    }

    /// The JSON body of `request` for this model.
    fn body(&self, request: &ChatRequest) -> Value {
        let mut body = request_body(request, &self.model, self.provider);
        with_reasoning(&mut body, self.provider, self.reasoning);
        body
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

impl OpenAiModel {
    /// Send `body` and return the response, or the API error.
    async fn send(&self, body: &Value) -> Result<reqwest::Response, AgentError> {
        let mut call = self.client.post(self.endpoint()).bearer_auth(&self.api_key);
        if self.provider == Provider::OpenRouter {
            call = call.header("X-Title", APP_TITLE);
        }
        let response =
            call.json(body).send().await.map_err(|e| {
                AgentError::new(ErrorKind::Transport, format!("request failed: {e}"))
            })?;
        let status = response.status().as_u16();
        if (200..300).contains(&status) {
            return Ok(response);
        }
        // The body names the error. It holds no secret: the key is a header.
        let text = read_body(response).await?;
        let detail: String = text.chars().take(500).collect();
        Err(AgentError::new(
            error_kind(status),
            format!("HTTP {status}: {detail}"),
        ))
    }
}

impl AgentModel for OpenAiModel {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            let body = self.body(request);
            let text = read_body(self.send(&body).await?).await?;
            let body: Value = serde_json::from_str(&text)
                .map_err(|e| AgentError::new(ErrorKind::Decode, format!("not JSON: {e}")))?;
            parse_response(&body)
        })
    }

    fn chat_stream<'a>(
        &'a self,
        request: &'a ChatRequest,
        on_delta: &'a (dyn Fn(&str) + Send + Sync),
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move {
            let body = streaming_body(self.body(request));
            let mut response = self.send(&body).await?;
            let mut stream = StreamAssembler::default();
            let mut frames = SseFrames::default();
            let mut read = 0_usize;
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| AgentError::new(ErrorKind::Transport, format!("stream lost: {e}")))?
            {
                read = read.saturating_add(chunk.len());
                if read > MAX_BODY_BYTES {
                    return Err(AgentError::new(
                        ErrorKind::Decode,
                        format!("the stream is over {MAX_BODY_BYTES} bytes"),
                    ));
                }
                for data in frames.push(&chunk) {
                    stream.event(&data, on_delta)?;
                }
            }
            for data in frames.end() {
                stream.event(&data, on_delta)?;
            }
            parse_response(&stream.finish()?)
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

/// How hard a reasoning model thinks before it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reasoning {
    /// No reasoning.
    None,
    /// The least reasoning that the model offers.
    Minimal,
    /// Little reasoning.
    Low,
    /// The middle.
    Medium,
    /// The most reasoning.
    High,
}

impl Reasoning {
    /// The name of the effort, as the APIs and the flag spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// Puts `effort` into `body` in the field of `provider`. OpenAI takes
/// `reasoning_effort`. OpenRouter takes a `reasoning` object, and turns
/// reasoning off with `enabled: false`. No effort sends nothing.
pub fn with_reasoning(body: &mut Value, provider: Provider, effort: Option<Reasoning>) {
    let Some(effort) = effort else {
        return;
    };
    match (provider, effort) {
        (Provider::OpenAi, effort) => body["reasoning_effort"] = json!(effort.as_str()),
        (Provider::OpenRouter, Reasoning::None) => body["reasoning"] = json!({"enabled": false}),
        (Provider::OpenRouter, effort) => {
            body["reasoning"] = json!({"effort": effort.as_str()});
        }
    }
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

/// The streamed form of `body`. The last chunk then carries the usage.
#[must_use]
pub fn streaming_body(mut body: Value) -> Value {
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage": true});
    body
}

/// One tool call of a stream, as its pieces arrive.
#[derive(Debug, Default)]
struct CallParts {
    id: String,
    name: String,
    arguments: String,
}

/// Builds the body of a streamed completion from its chunks.
///
/// The result has the shape of a plain response body, so
/// [`parse_response`] reads it. A chunk carries a piece of the text, of a
/// refusal or of a tool call. A tool call names its place with `index`,
/// and its arguments arrive in pieces.
#[derive(Debug, Default)]
pub struct StreamAssembler {
    /// A chunk with a choice came.
    seen: bool,
    /// `[DONE]` came.
    done: bool,
    text: String,
    refusal: String,
    calls: Vec<CallParts>,
    finish_reason: Option<String>,
    usage: Value,
}

impl StreamAssembler {
    /// Apply the data of one event. Each text piece goes to `on_delta`. An
    /// event with no text passes `""`, so the harness sends the text that
    /// waits.
    ///
    /// # Errors
    ///
    /// Data that is not JSON is a `Decode` error. An `error` object is the
    /// error of its code.
    pub fn event(&mut self, data: &str, on_delta: &dyn Fn(&str)) -> Result<(), AgentError> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let chunk: Value = serde_json::from_str(data).map_err(|e| {
            AgentError::new(
                ErrorKind::Decode,
                format!("a stream event is not JSON: {e}"),
            )
        })?;
        if chunk["error"].is_object() {
            return Err(body_error(&chunk["error"]));
        }
        if chunk["usage"].is_object() {
            self.usage = chunk["usage"].clone();
        }
        let mut text = "";
        if let Some(choice) = chunk["choices"].get(0) {
            self.seen = true;
            if let Some(reason) = choice["finish_reason"].as_str() {
                self.finish_reason = Some(reason.to_owned());
            }
            let delta = &choice["delta"];
            text = delta["content"].as_str().unwrap_or("");
            self.text.push_str(text);
            if let Some(refusal) = delta["refusal"].as_str() {
                self.refusal.push_str(refusal);
            }
            for call in delta["tool_calls"].as_array().into_iter().flatten() {
                self.call(call)?;
            }
        }
        on_delta(text);
        Ok(())
    }

    /// Add the piece of one tool call.
    fn call(&mut self, piece: &Value) -> Result<(), AgentError> {
        let index = piece["index"]
            .as_u64()
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| AgentError::new(ErrorKind::Decode, "a tool call has no index"))?;
        // A provider numbers the calls from 0, so a gap is a broken stream.
        if index > self.calls.len() {
            return Err(AgentError::new(
                ErrorKind::Decode,
                format!("tool call {index} came before call {}", self.calls.len()),
            ));
        }
        if index == self.calls.len() {
            self.calls.push(CallParts::default());
        }
        let parts = &mut self.calls[index];
        if let Some(id) = piece["id"].as_str() {
            id.clone_into(&mut parts.id);
        }
        let function = &piece["function"];
        if let Some(name) = function["name"].as_str() {
            parts.name.push_str(name);
        }
        if let Some(arguments) = function["arguments"].as_str() {
            parts.arguments.push_str(arguments);
        }
        Ok(())
    }

    /// The body of a plain response with the same message.
    ///
    /// # Errors
    ///
    /// A stream with no choice, or one that stops before its finish reason
    /// and before `[DONE]`, is a `Transport` error: the answer is not whole.
    pub fn finish(self) -> Result<Value, AgentError> {
        if !self.seen || (self.finish_reason.is_none() && !self.done) {
            return Err(AgentError::new(
                ErrorKind::Transport,
                "the stream ended before the answer did",
            ));
        }
        let calls: Vec<Value> = self
            .calls
            .into_iter()
            .map(|call| {
                json!({
                    "id": call.id,
                    "type": "function",
                    "function": {"name": call.name, "arguments": call.arguments},
                })
            })
            .collect();
        let mut message = json!({"role": "assistant", "content": self.text});
        if !self.refusal.is_empty() {
            message["refusal"] = json!(self.refusal);
        }
        if !calls.is_empty() {
            message["tool_calls"] = json!(calls);
        }
        Ok(json!({
            "choices": [{"message": message, "finish_reason": self.finish_reason}],
            "usage": self.usage,
        }))
    }
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
    fn the_reasoning_effort_goes_in_the_field_of_each_provider() {
        let openrouter = |effort| {
            let mut body = request_body(&request(), "x/y", Provider::OpenRouter);
            with_reasoning(&mut body, Provider::OpenRouter, effort);
            body
        };
        assert_eq!(
            openrouter(Some(Reasoning::Low))["reasoning"],
            json!({"effort": "low"})
        );
        assert_eq!(
            openrouter(Some(Reasoning::None))["reasoning"],
            json!({"enabled": false}),
            "OpenRouter turns reasoning off with enabled"
        );
        assert!(
            openrouter(None).get("reasoning").is_none(),
            "no setting sends nothing"
        );
        let mut openai = request_body(&request(), "gpt-x", Provider::OpenAi);
        with_reasoning(&mut openai, Provider::OpenAi, Some(Reasoning::Minimal));
        assert_eq!(openai["reasoning_effort"], "minimal");
        assert!(openai.get("reasoning").is_none());
        assert_eq!(
            [
                Reasoning::None,
                Reasoning::Minimal,
                Reasoning::Low,
                Reasoning::Medium,
                Reasoning::High
            ]
            .map(Reasoning::as_str),
            ["none", "minimal", "low", "medium", "high"]
        );
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

    /// Stream `chunks` as events in network chunks of `size` bytes, then
    /// `[DONE]`. Returns the body and each piece that reached `on_delta`.
    fn streamed_in(chunks: &[Value], size: usize) -> Result<(Value, Vec<String>), AgentError> {
        let mut wire = chunks.iter().fold(String::new(), |mut wire, chunk| {
            use std::fmt::Write as _;
            let _ = write!(wire, "data: {chunk}\n\n");
            wire
        });
        wire.push_str(": done soon\n\ndata: [DONE]\n\n");
        let mut frames = SseFrames::default();
        let mut stream = StreamAssembler::default();
        let seen = std::sync::Mutex::new(Vec::new());
        let on_delta = |piece: &str| seen.lock().unwrap().push(piece.to_owned());
        for chunk in wire.as_bytes().chunks(size) {
            for data in frames.push(chunk) {
                stream.event(&data, &on_delta)?;
            }
        }
        for data in frames.end() {
            stream.event(&data, &on_delta)?;
        }
        Ok((stream.finish()?, seen.into_inner().unwrap()))
    }

    fn delta(delta: &Value, finish: Option<&str>) -> Value {
        json!({"choices": [{"index": 0, "delta": delta, "finish_reason": finish}]})
    }

    #[test]
    fn a_stream_builds_the_body_of_a_plain_response() {
        let text = "h\u{e9}llo \u{1f642}";
        let chunks = [
            delta(&json!({"role": "assistant", "content": ""}), None),
            delta(&json!({"content": &text[..3]}), None),
            delta(&json!({"content": &text[3..]}), None),
            delta(&json!({"reasoning": "hm"}), None),
            delta(&json!({}), Some("stop")),
            json!({"choices": [], "usage": {"prompt_tokens": 9, "completion_tokens": 4}}),
        ];
        // One byte per chunk splits each character of more than one byte.
        let (body, pieces) = streamed_in(&chunks, 1).unwrap();
        assert_eq!(pieces.concat(), text);
        // The harness sends the text that waits at each empty piece.
        assert_eq!(pieces, ["", &text[..3], &text[3..], "", "", ""]);
        let response = parse_response(&body).unwrap();
        assert_eq!(response.content, vec![ContentPart::Text(text.into())]);
        assert_eq!(response.stop_reason, StopReason::EndTurn);
        assert_eq!(response.usage.input_tokens, 9);
        assert_eq!(response.usage.output_tokens, 4);
    }

    #[test]
    fn tool_calls_join_their_pieces_by_index() {
        let call = |index: u64, id: Option<&str>, name: Option<&str>, args: &str| {
            let mut piece = json!({"index": index, "function": {"arguments": args}});
            if let Some(id) = id {
                piece["id"] = json!(id);
                piece["type"] = json!("function");
            }
            if let Some(name) = name {
                piece["function"]["name"] = json!(name);
            }
            delta(&json!({"tool_calls": [piece]}), None)
        };
        let chunks = [
            delta(&json!({"content": "let me look"}), None),
            call(0, Some("c1"), Some("read_file"), ""),
            call(0, None, None, "{\"path\":"),
            call(1, Some("c2"), Some("web_search"), "{}"),
            call(0, None, None, "\"a.md\"}"),
            delta(&json!({}), Some("tool_calls")),
        ];
        let (body, _) = streamed_in(&chunks, 5).unwrap();
        let response = parse_response(&body).unwrap();
        assert_eq!(response.stop_reason, StopReason::ToolUse);
        assert_eq!(
            response.content,
            vec![
                ContentPart::Text("let me look".into()),
                ContentPart::ToolCall {
                    id: "c1".into(),
                    name: "read_file".into(),
                    arguments: json!({"path": "a.md"}),
                },
                ContentPart::ToolCall {
                    id: "c2".into(),
                    name: "web_search".into(),
                    arguments: json!({}),
                },
            ]
        );
        let gap = streamed_in(&[call(1, Some("c"), Some("x"), "{}")], 64).unwrap_err();
        assert_eq!(gap.kind(), ErrorKind::Decode);
    }

    #[test]
    fn a_stream_that_fails_breaks_off_or_refuses_is_an_error() {
        let upstream = json!({"error": {"code": 502, "message": "upstream down"}});
        let error =
            streamed_in(&[delta(&json!({"content": "a"}), None), upstream], 64).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Unavailable);
        assert!(error.message().contains("upstream down"));

        let mut cut = StreamAssembler::default();
        cut.event(&delta(&json!({"content": "a"}), None).to_string(), &|_| {})
            .unwrap();
        assert_eq!(cut.finish().unwrap_err().kind(), ErrorKind::Transport);
        assert_eq!(
            StreamAssembler::default().finish().unwrap_err().kind(),
            ErrorKind::Transport
        );

        let mut junk = StreamAssembler::default();
        assert_eq!(
            junk.event("{not json", &|_| {}).unwrap_err().kind(),
            ErrorKind::Decode
        );

        let refused = [delta(&json!({"refusal": "no"}), Some("stop"))];
        let (body, _) = streamed_in(&refused, 64).unwrap();
        assert_eq!(
            parse_response(&body).unwrap_err().kind(),
            ErrorKind::Provider
        );
    }

    #[test]
    fn a_streamed_request_asks_for_the_usage() {
        let body = streaming_body(request_body(&request(), "m", Provider::OpenRouter));
        assert_eq!(body["stream"], true);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }
}
