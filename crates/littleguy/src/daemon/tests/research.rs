//! The research tool end to end on the daemon (ADR 0044).
//!
//! A scripted model plays both sides. The assistant calls `sweep` over two
//! sources once, then answers. Each source run answers with the source it
//! read. The reply comes from the rows, and the usage of the op counts the
//! source runs too.

use std::sync::Mutex;

use autumn_harvest_agent::{
    AgentError, BoxFuture, ChatMessage, ChatRequest, ChatResponse, ContentPart, SWEEP_TOOL,
    StopReason, TokenUsage,
};

use super::*;

/// The tokens of one source run: 10 in and 5 out.
const ITEM_IN: u32 = 10;
const ITEM_OUT: u32 = 5;

/// A model for the assistant and for each source run.
#[derive(Debug, Default)]
struct Researcher {
    /// Whether each assistant request offered `sweep`.
    offered: Mutex<Vec<bool>>,
    /// The result of each `sweep` call, as the assistant saw it.
    results: Mutex<Vec<serde_json::Value>>,
}

fn text_of(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The text of the last tool result in `request`, if any.
fn tool_result(request: &ChatRequest) -> Option<String> {
    request
        .messages
        .iter()
        .rev()
        .flat_map(|message| message.content.iter().rev())
        .find_map(|part| match part {
            ContentPart::ToolResult { content, .. } => Some(content.clone()),
            _ => None,
        })
}

fn is_item(request: &ChatRequest) -> bool {
    request
        .messages
        .first()
        .is_some_and(|first| text_of(first) == crate::research::RESEARCH_SYSTEM)
}

fn respond(content: Vec<ContentPart>, usage: TokenUsage) -> ChatResponse {
    let stop_reason = if content
        .iter()
        .any(|part| matches!(part, ContentPart::ToolCall { .. }))
    {
        StopReason::ToolUse
    } else {
        StopReason::EndTurn
    };
    ChatResponse {
        content,
        stop_reason,
        usage,
    }
}

impl Researcher {
    fn reply(&self, request: &ChatRequest) -> ChatResponse {
        if is_item(request) {
            let text: String = request.messages.iter().map(text_of).collect();
            let source = text.rsplit("Source: ").next().unwrap_or_default().trim();
            return respond(
                vec![ContentPart::Text(format!("read {source}"))],
                TokenUsage::new(ITEM_IN, ITEM_OUT),
            );
        }
        let offered = request.tools.iter().any(|tool| tool.name == SWEEP_TOOL);
        self.offered.lock().unwrap().push(offered);
        if let Some(result) = tool_result(request) {
            let value: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
            self.results.lock().unwrap().push(value);
            return respond(
                vec![ContentPart::Text(String::from("Both sources read."))],
                TokenUsage::new(1, 1),
            );
        }
        if !offered {
            return respond(
                vec![ContentPart::Text(String::from("No sweep here."))],
                TokenUsage::new(1, 1),
            );
        }
        respond(
            vec![ContentPart::ToolCall {
                id: String::from("s1"),
                name: String::from(SWEEP_TOOL),
                arguments: serde_json::json!({
                    "instruction": "Summarize the source in one line.",
                    "items": [
                        {"label": "alpha", "source": "Source: https://a.example"},
                        {"label": "beta", "source": "Source: https://b.example"}
                    ]
                }),
            }],
            TokenUsage::new(1, 1),
        )
    }
}

impl AgentModel for Researcher {
    fn chat<'a>(
        &'a self,
        request: &'a ChatRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, AgentError>> {
        Box::pin(async move { Ok(self.reply(request)) })
    }
}

fn open(section: &str) -> (tempfile::TempDir, Arc<Researcher>, Daemon) {
    let dir = tempfile::tempdir().unwrap();
    let model = Arc::new(Researcher::default());
    let config = Config::parse(&format!("owner = \"mark\"\n{section}")).unwrap();
    let daemon = Daemon::open(
        &dir.path().join("lg.db"),
        &config,
        Arc::clone(&model) as Arc<dyn AgentModel>,
        UID,
        false,
    )
    .unwrap();
    (dir, model, daemon)
}

#[tokio::test(flavor = "multi_thread")]
async fn one_sweep_call_reads_each_source_and_names_its_spend() {
    let (_dir, model, mut daemon) =
        open("[research]\nmax_tokens_per_call = 100000\nconcurrency = 2");
    let response = daemon
        .handle_request(UID, request("m-1", "read both"))
        .await;
    assert_eq!(reply_text(&response), "Both sources read.");

    let results = model.results.lock().unwrap().clone();
    assert_eq!(results.len(), 1, "one sweep call: {results:?}");
    let result = &results[0];
    let rows = result["items"].as_array().unwrap();
    let labels: Vec<&str> = rows
        .iter()
        .map(|row| row["label"].as_str().unwrap())
        .collect();
    assert_eq!(labels, ["alpha", "beta"]);
    assert!(rows.iter().all(|row| row["ok"] == true), "{result}");
    let text = result["text"].as_str().unwrap();
    assert!(text.contains("read https://a.example"), "{text}");
    assert!(text.contains("read https://b.example"), "{text}");
    assert_eq!(result["spent"]["ceiling"], 100_000);
    assert_eq!(result["spent"]["total_tokens"], 2 * (ITEM_IN + ITEM_OUT));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_usage_of_an_op_counts_its_source_runs() {
    let (_dir, _model, mut daemon) = open("[research]");
    let response = daemon
        .handle_request(UID, request("m-1", "read both"))
        .await;
    let Response::Reply {
        usage: Some(usage), ..
    } = response
    else {
        panic!("no usage: {response:?}");
    };
    // Two assistant turns and one turn in each of the two source runs.
    assert_eq!(usage.calls, 4, "{usage:?}");
    assert_eq!(usage.input_tokens, 2 + 2 * u64::from(ITEM_IN));
    assert_eq!(usage.output_tokens, 2 + 2 * u64::from(ITEM_OUT));

    // A later op counts only its own calls. The model sees the earlier
    // result in its window, so it answers with one call and no sweep.
    let response = daemon.handle_request(UID, request("m-2", "again")).await;
    let Response::Reply {
        usage: Some(usage), ..
    } = response
    else {
        panic!("no usage: {response:?}");
    };
    assert_eq!(usage.calls, 1, "{usage:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn without_the_section_the_assistant_has_no_sweep_tool() {
    let (_dir, model, mut daemon) = open("");
    let response = daemon
        .handle_request(UID, request("m-1", "read both"))
        .await;
    assert_eq!(reply_text(&response), "No sweep here.");
    assert_eq!(*model.offered.lock().unwrap(), [false]);
}
