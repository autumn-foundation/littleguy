//! The tools of goals and ideas (ADR 0020).
//!
//! The assistant files ideas, researches them, proposes goals and works on
//! the goals that the owner accepted. It cannot accept, decline or drop: a
//! proposed goal does nothing until the owner accepts it on the pages, as a
//! refine overlay does nothing until the owner activates it.
//!
//! Every tool here is `ReadOnly` or `Internal`, so an unattended run, such as
//! a heartbeat tick, may call each one. The texts can hold what a research
//! read, so `list_plans` carries the `untrusted_input` label.

use std::sync::Arc;

use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{AgentError, ErrorKind, FnTool, Tool, ToolContext, ToolEffect};
use chrono::{DateTime, Utc};
use littleguy_stores::SqliteStores;
use littleguy_stores::plans::{Goal, Idea, Note};
use serde_json::{Value, json};

/// The key of one tool call. A retried call has the same key, so the store
/// adds its row once.
fn call_key(ctx: &ToolContext) -> String {
    format!("{}:{}", ctx.run_id.as_str(), ctx.call_id)
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// An epoch millisecond as RFC 3339, in UTC.
fn time(ms: Option<i64>) -> Value {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
        .map_or(Value::Null, |at| json!(at.to_rfc3339()))
}

fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

fn text<'a>(input: &'a Value, name: &str) -> Result<&'a str, AgentError> {
    input[name]
        .as_str()
        .ok_or_else(|| refuse(format!("`{name}` must be text")))
}

fn id(input: &Value, name: &str) -> Result<i64, AgentError> {
    input[name]
        .as_i64()
        .ok_or_else(|| refuse(format!("`{name}` must be a whole number")))
}

fn notes_json(notes: &[Note]) -> Vec<Value> {
    notes
        .iter()
        .map(|note| json!({"at": time(Some(note.at_ms)), "text": note.text}))
        .collect()
}

fn goal_json(goal: &Goal, now: i64) -> Value {
    json!({
        "id": goal.id,
        "status": goal.status.as_str(),
        "title": goal.title,
        "why": goal.why,
        "check_every_minutes": goal.check_every_secs.map(|secs| secs / 60),
        "next_check": time(goal.next_check_ms),
        "due": goal.next_check_ms.is_some_and(|at| at <= now),
        "from_idea": goal.idea,
        "notes": notes_json(&goal.notes),
    })
}

fn idea_json(idea: &Idea) -> Value {
    json!({
        "id": idea.id,
        "title": idea.title,
        "body": idea.body,
        "filed": time(Some(idea.created_ms)),
        "notes": notes_json(&idea.notes),
    })
}

fn object(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// The goal and idea tools over `stores`.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "five tools, each a schema and a short body, read best in one list"
)]
pub fn tools(stores: &Arc<SqliteStores>) -> Vec<Arc<dyn Tool>> {
    let list = Arc::clone(stores);
    let file = Arc::clone(stores);
    let research = Arc::clone(stores);
    let propose = Arc::clone(stores);
    let progress = Arc::clone(stores);
    vec![
        FnTool::new(
            "list_plans",
            "List your goals and ideas. An active goal is yours to work on; a goal with \
             `due` true needs a check now. A proposed goal waits for the owner to accept it. \
             An idea is something to plan and research.",
            json!({"type": "object", "properties": {}}),
            move |_input| {
                let stores = Arc::clone(&list);
                async move {
                    let plans = stores.plans()?;
                    let now = now_ms();
                    Ok(json!({
                        "goals": plans.goals.iter().map(|goal| goal_json(goal, now)).collect::<Vec<_>>(),
                        "ideas": plans.ideas.iter().map(idea_json).collect::<Vec<_>>(),
                    }))
                }
            },
        )
        .effect(ToolEffect::ReadOnly)
        .label(labels::UNTRUSTED_INPUT)
        .shared(),
        FnTool::with_context(
            "add_idea",
            "File an idea: something worth planning or researching that might become a goal. \
             File one when the owner mentions something worth exploring, or when you notice \
             one yourself.",
            object(
                &json!({
                    "title": {"type": "string", "description": "A short name."},
                    "body": {"type": "string", "description": "What the idea is, and why it might matter."},
                }),
                &["title", "body"],
            ),
            move |input, ctx| {
                let stores = Arc::clone(&file);
                async move {
                    let id = stores.add_idea(
                        &call_key(&ctx),
                        text(&input, "title")?,
                        text(&input, "body")?,
                        now_ms(),
                    )?;
                    Ok(json!({"idea": id}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
        FnTool::with_context(
            "research_idea",
            "Add a research note to an open idea: what you found, what is still unknown, and \
             what it would take.",
            object(
                &json!({
                    "idea": {"type": "integer"},
                    "note": {"type": "string"},
                }),
                &["idea", "note"],
            ),
            move |input, ctx| {
                let stores = Arc::clone(&research);
                async move {
                    let idea = id(&input, "idea")?;
                    stores.research_idea(&call_key(&ctx), idea, text(&input, "note")?, now_ms())?;
                    Ok(json!({"idea": idea, "noted": true}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
        FnTool::with_context(
            "propose_goal",
            "Propose a goal: a thing to do. It does nothing until the owner accepts it on the \
             Workspace page; tell them you proposed it. Name the idea it comes from, if any. \
             Ask for checks when the goal means watching something over time.",
            object(
                &json!({
                    "title": {"type": "string", "description": "A short name."},
                    "why": {"type": "string", "description": "Why it matters, and what done looks like."},
                    "check_every_minutes": {"type": "integer", "minimum": 1, "description": "How often to check on it, if at all."},
                    "from_idea": {"type": "integer"},
                }),
                &["title", "why"],
            ),
            move |input, ctx| {
                let stores = Arc::clone(&propose);
                async move {
                    let every = input["check_every_minutes"]
                        .as_i64()
                        .map(|minutes| minutes.saturating_mul(60));
                    let goal = stores.propose_goal(
                        &call_key(&ctx),
                        text(&input, "title")?,
                        text(&input, "why")?,
                        every,
                        input["from_idea"].as_i64(),
                        now_ms(),
                    )?;
                    Ok(json!({"goal": goal, "status": "proposed"}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
        FnTool::with_context(
            "goal_progress",
            "Note progress on an active goal, or a check that found nothing new. A goal with \
             checks is due again one interval later. Set `finished` when the goal is done.",
            object(
                &json!({
                    "goal": {"type": "integer"},
                    "note": {"type": "string"},
                    "finished": {"type": "boolean"},
                }),
                &["goal", "note"],
            ),
            move |input, ctx| {
                let stores = Arc::clone(&progress);
                async move {
                    let goal = id(&input, "goal")?;
                    let finished = input["finished"].as_bool().unwrap_or(false);
                    stores.goal_progress(
                        &call_key(&ctx),
                        goal,
                        text(&input, "note")?,
                        finished,
                        now_ms(),
                    )?;
                    Ok(json!({"goal": goal, "finished": finished}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_tool_is_safe_unattended_and_none_accepts() {
        let stores = Arc::new(SqliteStores::open_in_memory().unwrap());
        let tools = tools(&stores);
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        assert_eq!(
            names,
            [
                "list_plans",
                "add_idea",
                "research_idea",
                "propose_goal",
                "goal_progress"
            ]
        );
        for tool in &tools {
            assert!(
                tool.effect() <= ToolEffect::Internal,
                "{} must run on a read-only tick",
                tool.name()
            );
        }
        assert!(
            tools[0]
                .labels()
                .iter()
                .any(|label| label == labels::UNTRUSTED_INPUT)
        );
    }
}
