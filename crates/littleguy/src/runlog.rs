//! The tools that let the assistant debug its own runs.
//!
//! - `recent_runs`: the newest runs, and how each one ended.
//! - `failed_attempts`: the newest model or tool attempts that failed, and
//!   why.
//! - `model_usage`: the model calls of a window: tokens, cost and latency.
//!
//! Each tool runs one fixed, read-only query through
//! [`littleguy_stores::RunLog`]. None of them reads a message. An error text
//! can quote what failed, for example a page that a fetch read. So the two
//! tools that return error text carry the `untrusted_input` label, as the
//! web tools do. `[tools] runs` turns the three tools on or off.

use std::path::Path;
use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolEffect};
use chrono::{DateTime, Utc};
use littleguy_stores::RunLog;
use littleguy_stores::runlog::{Failure, MAX_ROWS, Run, Usage};
use serde_json::{Value, json};

/// The rows of a call that names no limit.
const DEFAULT_ROWS: u32 = 10;

/// The window of `model_usage` that names no hours: one day.
const DEFAULT_HOURS: u32 = 24;

/// The longest window of `model_usage`: 30 days.
const MAX_HOURS: u32 = 30 * 24;

/// An epoch millisecond as RFC 3339, in UTC.
fn time(ms: Option<i64>) -> Value {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
        .map_or(Value::Null, |at| json!(at.to_rfc3339()))
}

/// The whole number `name` of `input`, between 1 and `max`, or `default`.
fn bounded(input: &Value, name: &str, default: u32, max: u32) -> u32 {
    input[name]
        .as_u64()
        .and_then(|value| u32::try_from(value).ok())
        .map_or(default, |value| value.clamp(1, max))
}

fn run_json(run: &Run) -> Value {
    json!({
        "id": run.id,
        "workflow": run.workflow,
        "state": run.state,
        "started": time(run.started_ms),
        "closed": time(run.closed_ms),
        "attempt": run.attempt,
        "schedule": run.schedule,
        "error": run.error,
    })
}

fn failure_json(failure: &Failure) -> Value {
    json!({
        "run": failure.run,
        "workflow": failure.workflow,
        "activity": failure.activity,
        "attempt": failure.attempt,
        "error": failure.error,
    })
}

/// A cost in millionths of a dollar, as dollars.
#[expect(
    clippy::cast_precision_loss,
    reason = "a cost below 2^53 micro dollars is exact, and the ledger caps it far lower"
)]
fn dollars(micros: i64) -> f64 {
    micros as f64 / 1_000_000.0
}

fn usage_json(usage: &Usage) -> Value {
    json!({
        "model": usage.model,
        "calls": usage.calls,
        "input_tokens": usage.input_tokens,
        "output_tokens": usage.output_tokens,
        "cost_usd": dollars(usage.cost_usd_micros),
        "unpriced_calls": usage.unpriced,
        "mean_latency_ms": usage.mean_latency_ms,
        "max_latency_ms": usage.max_latency_ms,
    })
}

/// Runs `read` on a thread that may block, because a query is blocking I/O.
async fn blocking<T: Send + 'static>(
    read: impl FnOnce() -> Result<T, AgentError> + Send + 'static,
) -> Result<T, AgentError> {
    tokio::task::spawn_blocking(read)
        .await
        .map_err(|error| AgentError::new(ErrorKind::Tool, format!("the read stopped: {error}")))?
}

fn limit_schema(what: &str) -> Value {
    json!({
        "type": "object",
        "properties": {
            "limit": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_ROWS,
                "description": format!("The most {what} to return. Default {DEFAULT_ROWS}."),
            },
        },
    })
}

/// The three tools over the runtime database `db`.
#[must_use]
pub fn tools(db: &Path) -> Vec<Arc<dyn Tool>> {
    let log = RunLog::new(db);
    let runs = log.clone();
    let failures = log.clone();
    let mut runs_schema = limit_schema("runs");
    runs_schema["properties"]["failed_only"] = json!({
        "type": "boolean",
        "description": "Only the runs that failed, timed out, or were cancelled or terminated.",
    });
    vec![
        FnTool::new(
            "recent_runs",
            "List your own newest runs, newest first: the workflow, how each one ended \
             (COMPLETED, FAILED, RUNNING and so on), when, and the error of a failed run. \
             Use it to debug yourself. It shows no messages.",
            runs_schema,
            move |input| {
                let log = runs.clone();
                async move {
                    let limit = bounded(&input, "limit", DEFAULT_ROWS, MAX_ROWS);
                    let failed_only = input["failed_only"].as_bool().unwrap_or(false);
                    let rows = blocking(move || log.runs(limit, failed_only)).await?;
                    Ok(json!({"runs": rows.iter().map(run_json).collect::<Vec<_>>()}))
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .label(labels::UNTRUSTED_INPUT)
        .shared(),
        FnTool::new(
            "failed_attempts",
            "List your own newest model calls and tool calls that failed, newest first, \
             with the error of each. A failed attempt can retry and then succeed. Use it \
             to find out why something went wrong.",
            limit_schema("attempts"),
            move |input| {
                let log = failures.clone();
                async move {
                    let limit = bounded(&input, "limit", DEFAULT_ROWS, MAX_ROWS);
                    let rows = blocking(move || log.failures(limit)).await?;
                    Ok(json!({"failures": rows.iter().map(failure_json).collect::<Vec<_>>()}))
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .label(labels::UNTRUSTED_INPUT)
        .shared(),
        FnTool::new(
            "model_usage",
            "Sum your own model calls of the last hours, per model: calls, tokens, cost in \
             US dollars and latency. A call with no price counts as unpriced.",
            json!({
                "type": "object",
                "properties": {
                    "hours": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": MAX_HOURS,
                        "description": format!("The window, in hours. Default {DEFAULT_HOURS}."),
                    },
                },
            }),
            move |input| {
                let log = log.clone();
                async move {
                    let hours = bounded(&input, "hours", DEFAULT_HOURS, MAX_HOURS);
                    let since = Utc::now() - chrono::Duration::hours(i64::from(hours));
                    let since_ms = since.timestamp_millis();
                    let rows = blocking(move || log.usage(since_ms)).await?;
                    Ok(json!({
                        "since": time(Some(since_ms)),
                        "models": rows.iter().map(usage_json).collect::<Vec<_>>(),
                    }))
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .shared(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_limit_stays_in_its_bounds() {
        assert_eq!(bounded(&json!({}), "limit", 10, 50), 10);
        assert_eq!(bounded(&json!({"limit": 0}), "limit", 10, 50), 1);
        assert_eq!(bounded(&json!({"limit": 500}), "limit", 10, 50), 50);
        assert_eq!(bounded(&json!({"limit": -3}), "limit", 10, 50), 10);
        assert_eq!(bounded(&json!({"limit": "7"}), "limit", 10, 50), 10);
    }

    #[test]
    fn times_and_costs_read_well() {
        assert_eq!(time(Some(0)), json!("1970-01-01T00:00:00+00:00"));
        assert_eq!(time(None), Value::Null);
        assert!((dollars(1_500_000) - 1.5).abs() < f64::EPSILON);
    }

    #[test]
    fn the_tools_read_only_and_label_their_error_text() {
        let tools = tools(Path::new("lg.db"));
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(names, ["recent_runs", "failed_attempts", "model_usage"]);
        for tool in &tools {
            assert_eq!(tool.effect(), ToolEffect::ReadOnly, "{}", tool.name());
        }
        let untrusted = |tool: &Arc<dyn Tool>| {
            tool.labels()
                .iter()
                .any(|label| label == labels::UNTRUSTED_INPUT)
        };
        assert!(untrusted(&tools[0]) && untrusted(&tools[1]));
        assert!(!untrusted(&tools[2]), "usage holds no text");
    }
}
