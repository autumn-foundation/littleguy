//! The `recall` tool: the assistant finds what it knew by meaning (ADR
//! 0047).
//!
//! The memory blocks go into every prompt, but they have a size limit. So
//! an entry that an edit replaced or removed, and an episode that the log
//! evicted, are gone from the prompt. The AletheiaDB store keeps both.
//! `recall` searches them by meaning, now or as of a past time.
//!
//! A fact or an episode can hold what a page said, so the tool carries the
//! `untrusted_input` label, as `read_note` does.

use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::memory::MemoryScope;
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolEffect};
use littleguy_core::ASSISTANT_KIND;
use littleguy_stores::aletheia::{AletheiaStores, Recall, RecallKind, Recalled};
use serde_json::{Value, json};

/// The hits of one recall when the call names no limit.
pub const DEFAULT_LIMIT: usize = 8;

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

/// The memory scope of the assistant of `owner`, as the engine names it:
/// `agent:<kind>/<owner>`. The episodes live in `<scope>:episodes`.
#[must_use]
pub fn scope_of(owner: &str) -> MemoryScope {
    MemoryScope::new(format!("agent:{ASSISTANT_KIND}/{owner}"))
}

/// The recall that `input` asks for.
///
/// # Errors
///
/// Returns a `Tool` error for an empty query, an unknown kind, or an
/// `as_of` that is not a time.
pub fn parse(input: &Value, scope: &MemoryScope) -> Result<Recall, AgentError> {
    let query = input["query"]
        .as_str()
        .filter(|query| !query.trim().is_empty())
        .ok_or_else(|| refuse("`query` must be the words to search by"))?;
    let limit = match &input["limit"] {
        Value::Null => DEFAULT_LIMIT,
        value => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .filter(|n| *n > 0)
            .ok_or_else(|| refuse("`limit` must be a whole number from 1"))?,
    };
    let kinds = match input["kind"].as_str() {
        None | Some("all") => Vec::new(),
        Some("facts") => vec![RecallKind::Fact],
        Some("episodes") => vec![RecallKind::Episode],
        Some(other) => {
            return Err(refuse(format!(
                "`kind` is `facts`, `episodes` or `all`, not `{other}`"
            )));
        }
    };
    let as_of_ms = match input["as_of"].as_str() {
        None => None,
        Some(text) => Some(as_of(text)?),
    };
    Ok(Recall {
        facts: scope.clone(),
        episodes: MemoryScope::new(format!("{}:episodes", scope.as_str())),
        query: query.to_owned(),
        limit,
        kinds,
        as_of_ms,
    })
}

/// The epoch milliseconds of an RFC 3339 time, or of the end of a date.
fn as_of(text: &str) -> Result<i64, AgentError> {
    let text = text.trim();
    if let Ok(at) = text.parse::<jiff::Timestamp>() {
        return Ok(at.as_millisecond());
    }
    if let Ok(date) = text.parse::<jiff::civil::Date>() {
        // A date means the whole day, so search as of its last moment.
        let end = jiff::tz::TimeZone::UTC
            .to_timestamp(date.to_datetime(jiff::civil::Time::MAX))
            .map_err(|e| refuse(format!("`as_of`: {e}")))?;
        return Ok(end.as_millisecond());
    }
    Err(refuse(
        "`as_of` must be a time such as 2026-03-01T09:00:00Z, or a date such as 2026-03-01",
    ))
}

/// The reply for `hits`.
#[must_use]
pub fn reply(hits: &[Recalled]) -> Value {
    let rows: Vec<Value> = hits
        .iter()
        .map(|hit| {
            let mut row = json!({
                "kind": match hit.kind {
                    RecallKind::Fact => "fact",
                    RecallKind::Episode => "episode",
                },
                "text": hit.text,
                "score": (f64::from(hit.score) * 1000.0).round() / 1000.0,
                "current": hit.current,
            });
            if let Some(block) = &hit.block {
                row["block"] = json!(block);
            }
            row["at"] = crate::stamp::from_millis(hit.at_ms).map_or(Value::Null, |at| {
                json!(at.strftime("%Y-%m-%dT%H:%M:%SZ").to_string())
            });
            row
        })
        .collect();
    json!({ "hits": rows })
}

/// The `recall` tool over `stores`, for the assistant of `owner`.
#[must_use]
pub fn tool(stores: &Arc<AletheiaStores>, owner: &str) -> Arc<dyn Tool> {
    let stores = Arc::clone(stores);
    let scope = scope_of(owner);
    FnTool::new(
        "recall",
        "Search your memory by meaning: the facts you keep, the ones you replaced or removed, \
         and what you answered in past ops, also the ones that your notes no longer hold. Use \
         it when you half remember something, or to see what you knew at a past time with \
         as_of. Each hit says whether it is still current.",
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "What to look for, in your own words."},
                "kind": {"type": "string", "enum": ["all", "facts", "episodes"], "description": "What to search. Default all."},
                "limit": {"type": "integer", "minimum": 1, "maximum": 50, "description": "The most hits. Default 8."},
                "as_of": {"type": "string", "description": "Search the memory as it was then: an RFC 3339 time or a date."},
            },
            "required": ["query"],
        }),
        move |input| {
            let stores = Arc::clone(&stores);
            let scope = scope.clone();
            async move {
                let recall = parse(&input, &scope)?;
                let hits = stores.recall(&recall).await?;
                Ok(reply(&hits))
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

#[cfg(test)]
mod tests {
    use autumn_harvest_agent::memory::{EditKey, MemoryOp, MemoryStore};
    use littleguy_core::embed::HashingEmbedder;

    use super::*;

    fn scope() -> MemoryScope {
        scope_of("mark")
    }

    #[test]
    fn the_scope_is_the_one_the_engine_names() {
        assert_eq!(scope().as_str(), "agent:assistant/mark");
    }

    #[test]
    fn a_call_parses_with_its_defaults() {
        let recall = parse(&json!({"query": "tea"}), &scope()).unwrap();
        assert_eq!(recall.limit, DEFAULT_LIMIT);
        assert!(recall.kinds.is_empty());
        assert_eq!(recall.as_of_ms, None);
        assert_eq!(recall.episodes.as_str(), "agent:assistant/mark:episodes");
        let facts = parse(
            &json!({"query": "tea", "kind": "facts", "limit": 3, "as_of": "2026-03-01T00:00:00Z"}),
            &scope(),
        )
        .unwrap();
        assert_eq!(facts.kinds, vec![RecallKind::Fact]);
        assert_eq!(facts.limit, 3);
        assert_eq!(facts.as_of_ms, Some(1_772_323_200_000));
        let day = parse(&json!({"query": "tea", "as_of": "2026-03-01"}), &scope()).unwrap();
        assert_eq!(day.as_of_ms, Some(1_772_409_599_999));
    }

    #[test]
    fn a_bad_call_is_refused() {
        for input in [
            json!({}),
            json!({"query": "  "}),
            json!({"query": "tea", "kind": "notes"}),
            json!({"query": "tea", "limit": 0}),
            json!({"query": "tea", "limit": "five"}),
            json!({"query": "tea", "as_of": "last spring"}),
        ] {
            let error = parse(&input, &scope()).unwrap_err();
            assert_eq!(error.kind(), ErrorKind::Tool, "{input}");
        }
    }

    #[tokio::test]
    async fn the_tool_finds_a_fact_that_an_edit_replaced() {
        let stores = Arc::new(
            AletheiaStores::open_in_memory(Arc::new(HashingEmbedder::default()))
                .await
                .unwrap(),
        );
        stores
            .apply(
                &scope(),
                MemoryOp::Add {
                    block: "memory".into(),
                    text: "Mark's dentist is Dr Silva".into(),
                },
                &EditKey::new("k1"),
            )
            .await
            .unwrap();
        stores
            .apply(
                &scope(),
                MemoryOp::Replace {
                    block: "memory".into(),
                    old: "Silva".into(),
                    text: "Mark's dentist is Dr Costa".into(),
                },
                &EditKey::new("k2"),
            )
            .await
            .unwrap();
        let tool = tool(&stores, "mark");
        assert_eq!(tool.name(), "recall");
        let recall = parse(
            &json!({"query": "dentist Silva", "kind": "facts"}),
            &scope(),
        )
        .unwrap();
        let hits = stores.recall(&recall).await.unwrap();
        let answer = reply(&hits);
        let rows = answer["hits"].as_array().unwrap();
        assert_eq!(rows[0]["text"], json!("Mark's dentist is Dr Costa"));
        assert_eq!(rows[0]["current"], json!(true));
        assert_eq!(rows[0]["block"], json!("memory"));
        assert!(rows[0]["at"].is_string());
    }
}
