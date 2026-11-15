//! The tools of goals and ideas (ADR 0020), wakes and skill drafts.
//!
//! The goal and idea tools come with the `plans` feature. A build without
//! it keeps the wake and skill tools, and `list_plans` lists only those.
//!
//! The assistant files ideas, researches them, proposes goals and works on
//! the goals that the owner accepted. It cannot accept, decline or drop: a
//! proposed goal does nothing until the owner accepts it on the pages, as a
//! refine overlay does nothing until the owner activates it.
//!
//! Every tool here is `ReadOnly` or `Internal`, so an unattended run, such as
//! a heartbeat tick, may call each one. The texts can hold what a research
//! read, so `list_plans` carries the `untrusted_input` label.
//!
//! `wake_me` books a wake: a look at something again at a later time
//! (ADR 0023). The daemon aims the engine timer at the next one.
//!
//! `propose_skill` drafts a skill: a `SKILL.md` that the owner accepts or
//! declines on the Skills tab (ADR 0024). A skill is standing instructions,
//! so no tool turns one on.

use std::sync::Arc;
#[cfg(feature = "followups")]
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};

use autumn_harvest_agent::Tool;
#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
use autumn_harvest_agent::flow::labels;
#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
use autumn_harvest_agent::{AgentError, ErrorKind, ToolContext};
#[cfg(any(
    feature = "plans",
    feature = "followups",
    feature = "skills",
    feature = "self-debug"
))]
use autumn_harvest_agent::{FnTool, ToolEffect};
#[cfg(any(feature = "plans", feature = "followups", feature = "self-debug"))]
use chrono::DateTime;
#[cfg(any(
    feature = "plans",
    feature = "followups",
    feature = "skills",
    feature = "self-debug"
))]
use chrono::Utc;
use littleguy_stores::SqliteStores;
#[cfg(feature = "plans")]
use littleguy_stores::plans::{Goal, Idea, Note};
#[cfg(any(
    feature = "plans",
    feature = "followups",
    feature = "skills",
    feature = "self-debug"
))]
use serde_json::{Value, json};

/// The key of one tool call. A retried call has the same key, so the store
/// adds its row once.
#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn call_key(ctx: &ToolContext) -> String {
    format!("{}:{}", ctx.run_id.as_str(), ctx.call_id)
}

#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

/// An epoch millisecond as RFC 3339, in UTC.
#[cfg(any(feature = "plans", feature = "followups", feature = "self-debug"))]
fn time(ms: Option<i64>) -> Value {
    ms.and_then(DateTime::<Utc>::from_timestamp_millis)
        .map_or(Value::Null, |at| json!(at.to_rfc3339()))
}

#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn refuse(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorKind::Tool, message)
}

#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn text<'a>(input: &'a Value, name: &str) -> Result<&'a str, AgentError> {
    input[name]
        .as_str()
        .ok_or_else(|| refuse(format!("`{name}` must be text")))
}

#[cfg(any(feature = "plans", feature = "followups"))]
fn id(input: &Value, name: &str) -> Result<i64, AgentError> {
    input[name]
        .as_i64()
        .ok_or_else(|| refuse(format!("`{name}` must be a whole number")))
}

#[cfg(feature = "plans")]
fn notes_json(notes: &[Note]) -> Vec<Value> {
    notes
        .iter()
        .map(|note| json!({"at": time(Some(note.at_ms)), "text": note.text}))
        .collect()
}

#[cfg(feature = "plans")]
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

#[cfg(feature = "plans")]
fn idea_json(idea: &Idea) -> Value {
    json!({
        "id": idea.id,
        "title": idea.title,
        "body": idea.body,
        "filed": time(Some(idea.created_ms)),
        "notes": notes_json(&idea.notes),
    })
}

/// What the wake tools share with the daemon.
#[cfg(feature = "followups")]
#[derive(Debug, Clone)]
pub struct WakeLinks {
    /// Set when a wake is booked or cancelled. The daemon then plans again.
    pub changed: Arc<AtomicBool>,
    /// When a person last wrote, in epoch milliseconds. A chain of wakes
    /// counts from there.
    pub last_person_ms: Arc<AtomicI64>,
}

#[cfg(feature = "followups")]
impl Default for WakeLinks {
    /// Changed at first, so the daemon reads the wakes of an earlier start.
    fn default() -> Self {
        Self {
            changed: Arc::new(AtomicBool::new(true)),
            last_person_ms: Arc::default(),
        }
    }
}

/// The longest wait that `wake_me` takes, in minutes.
#[cfg(feature = "followups")]
const MAX_WAKE_MINUTES: i64 = littleguy_stores::wakes::MAX_AHEAD_MS / 60_000;

#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn object(properties: &Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required})
}

/// What `list_plans` says it lists, from the parts of this build.
#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn list_plans_description() -> String {
    let mut what = Vec::new();
    if cfg!(feature = "plans") {
        what.extend(["goals", "ideas"]);
    }
    if cfg!(feature = "followups") {
        what.push("wakes");
    }
    if cfg!(feature = "skills") {
        what.push("skill drafts");
    }
    let last = what.pop().unwrap_or_default();
    let mut text = if what.is_empty() {
        format!("List your {last}.")
    } else {
        format!("List your {} and {last}.", what.join(", "))
    };
    if cfg!(feature = "plans") {
        text.push_str(
            " An active goal is yours to work on; a goal with `due` true needs a check now. \
             A proposed goal waits for the owner to accept it. An idea is something to plan \
             and research.",
        );
    }
    if cfg!(feature = "followups") {
        text.push_str(
            " A wake with `due` true is one you booked with wake_me, and its time has come.",
        );
    }
    text
}

/// The goal, idea, wake and skill tools over `stores`, as this build has
/// them.
#[must_use]
pub fn tools(
    stores: &Arc<SqliteStores>,
    #[cfg(feature = "followups")] wakes: &WakeLinks,
) -> Vec<Arc<dyn Tool>> {
    let tools = std::iter::empty::<Arc<dyn Tool>>();
    #[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
    let tools = tools.chain(std::iter::once(list_plans(stores)));
    // The goal and idea tools come right after `list_plans`.
    #[cfg(feature = "plans")]
    let tools = tools.chain(goal_tools(stores));
    #[cfg(feature = "self-debug")]
    let tools = tools.chain(std::iter::once(recent_wakes(stores)));
    #[cfg(feature = "skills")]
    let tools = tools.chain(std::iter::once(propose_skill(stores)));
    #[cfg(feature = "followups")]
    let tools = tools.chain(wake_tools(stores, wakes));
    // A build with none of the tool groups has no tools here.
    #[cfg(not(any(
        feature = "plans",
        feature = "followups",
        feature = "skills",
        feature = "self-debug"
    )))]
    let _ = stores;
    tools.collect()
}

/// `list_plans`: the goals, ideas, wakes and skill drafts of this build.
#[cfg(any(feature = "plans", feature = "followups", feature = "skills"))]
fn list_plans(stores: &Arc<SqliteStores>) -> Arc<dyn Tool> {
    let list = Arc::clone(stores);
    FnTool::new(
        "list_plans",
        list_plans_description(),
        json!({"type": "object", "properties": {}}),
        move |_input| {
            let stores = Arc::clone(&list);
            async move {
                #[cfg(any(feature = "plans", feature = "followups"))]
                let now = now_ms();
                let listing = json!({});
                #[cfg(feature = "skills")]
                let listing = {
                    let mut listing = listing;
                    listing["skill_drafts"] = stores
                        .skill_drafts()?
                        .iter()
                        .map(|draft| {
                            json!({
                                "id": draft.id,
                                "name": draft.name,
                                "description": draft.description,
                            })
                        })
                        .collect();
                    listing
                };
                #[cfg(feature = "followups")]
                let listing = {
                    let mut listing = listing;
                    listing["wakes"] = stores
                        .pending_wakes()?
                        .iter()
                        .map(|wake| {
                            json!({
                                "id": wake.id,
                                "at": time(Some(wake.at_ms)),
                                "due": wake.at_ms <= now,
                                "about": wake.about,
                            })
                        })
                        .collect();
                    listing
                };
                #[cfg(feature = "plans")]
                let listing = {
                    let mut listing = listing;
                    let plans = stores.plans()?;
                    listing["goals"] = plans
                        .goals
                        .iter()
                        .map(|goal| goal_json(goal, now))
                        .collect();
                    listing["ideas"] = plans.ideas.iter().map(idea_json).collect();
                    listing
                };
                Ok(listing)
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .label(labels::UNTRUSTED_INPUT)
    .shared()
}

/// `propose_skill`: a draft that the owner accepts or declines.
#[cfg(feature = "skills")]
fn propose_skill(stores: &Arc<SqliteStores>) -> Arc<dyn Tool> {
    let draft = Arc::clone(stores);
    FnTool::with_context(
        "propose_skill",
        "Draft a skill: know-how that you will load on demand later, written as the body \
         of a SKILL.md. Draft one when you find a way of doing something that you will \
         need again. It does nothing until the owner accepts it on the Skills tab; tell \
         them you drafted it. A draft with the name of an existing skill revises it.",
        object(
            &json!({
                "name": {"type": "string", "description": "lowercase-with-dashes, for example triage-inbox."},
                "description": {"type": "string", "description": "One line: when to load the skill."},
                "body": {"type": "string", "description": "The know-how, in markdown."},
            }),
            &["name", "description", "body"],
        ),
        move |input, ctx| {
            let stores = Arc::clone(&draft);
            async move {
                let id = stores.add_skill_draft(
                    &call_key(&ctx),
                    text(&input, "name")?,
                    text(&input, "description")?,
                    text(&input, "body")?,
                    now_ms(),
                )?;
                Ok(json!({"skill_draft": id, "status": "waits for the owner"}))
            }
        },
    )
    .effect(ToolEffect::Internal)
    .shared()
}

/// `recent_wakes`: the tick log, newest first.
#[cfg(feature = "self-debug")]
fn recent_wakes(stores: &Arc<SqliteStores>) -> Arc<dyn Tool> {
    let ticks = Arc::clone(stores);
    FnTool::new(
        "recent_wakes",
        "List the newest times that you woke on your own, newest first: heartbeats, briefs, \
         reflections and the wakes you booked. Each says when it was due, when it ended, \
         and how it went, for example skipped by the precheck with no model call. Your \
         run log cannot show these, because every op runs inside one long run.",
        json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer", "minimum": 1, "description": "The most rows. Default 20."},
            },
        }),
        move |input| {
            let stores = Arc::clone(&ticks);
            async move {
                let limit = input["limit"]
                    .as_u64()
                    .and_then(|limit| u32::try_from(limit).ok())
                    .unwrap_or(20)
                    .max(1);
                let rows = stores.recent_ticks(limit)?;
                Ok(json!({"ticks": rows.iter().map(|tick| json!({
                    "kind": tick.kind,
                    "due": time(Some(tick.due_ms)),
                    "ended": time(Some(tick.seen_ms)),
                    "outcome": tick.outcome,
                })).collect::<Vec<_>>()}))
            }
        },
    )
    .effect(ToolEffect::ReadOnly)
    .shared()
}

/// The tools that book and cancel a wake.
#[cfg(feature = "followups")]
fn wake_tools(stores: &Arc<SqliteStores>, wakes: &WakeLinks) -> Vec<Arc<dyn Tool>> {
    let book = (Arc::clone(stores), wakes.clone());
    let cancel = (Arc::clone(stores), wakes.clone());
    vec![
        FnTool::with_context(
            "wake_me",
            "Book a wake: you wake on your own later and look at something again, for example \
             a deploy, a reply or a page. Say what to look at. The wake shows in list_plans. \
             It runs read-only, as a heartbeat does.",
            object(
                &json!({
                    "in_minutes": {"type": "integer", "minimum": 1, "maximum": MAX_WAKE_MINUTES},
                    "about": {"type": "string", "description": "What to look at, and why."},
                }),
                &["in_minutes", "about"],
            ),
            move |input, ctx| {
                let (stores, links) = book.clone();
                async move {
                    let minutes = input["in_minutes"]
                        .as_i64()
                        .ok_or_else(|| refuse("`in_minutes` must be a whole number"))?;
                    let now = now_ms();
                    let at = now.saturating_add(minutes.saturating_mul(60_000));
                    let id = stores.add_wake(
                        &call_key(&ctx),
                        at,
                        text(&input, "about")?,
                        now,
                        links.last_person_ms.load(Ordering::SeqCst),
                    )?;
                    links.changed.store(true, Ordering::SeqCst);
                    Ok(json!({"wake": id, "at": time(Some(at))}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
        FnTool::new(
            "cancel_wake",
            "Cancel a wake that you booked and no longer need.",
            object(&json!({"wake": {"type": "integer"}}), &["wake"]),
            move |input| {
                let (stores, links) = cancel.clone();
                async move {
                    let wake = id(&input, "wake")?;
                    let cancelled = stores.cancel_wake(wake)?;
                    links.changed.store(true, Ordering::SeqCst);
                    Ok(json!({"wake": wake, "cancelled": cancelled}))
                }
            },
        )
        .effect(ToolEffect::Internal)
        .shared(),
    ]
}

/// The tools that file and research ideas, and propose and work on goals.
#[cfg(feature = "plans")]
#[expect(
    clippy::too_many_lines,
    reason = "four tools, each a schema and a short body, read best in one list"
)]
fn goal_tools(stores: &Arc<SqliteStores>) -> Vec<Arc<dyn Tool>> {
    let file = Arc::clone(stores);
    let research = Arc::clone(stores);
    let propose = Arc::clone(stores);
    let progress = Arc::clone(stores);
    vec![
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
        let tools = tools(
            &stores,
            #[cfg(feature = "followups")]
            &WakeLinks::default(),
        );
        let names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();
        let goals: &[&str] = if cfg!(feature = "plans") {
            &["add_idea", "research_idea", "propose_goal", "goal_progress"]
        } else {
            &[]
        };
        let skills: &[&str] = if cfg!(feature = "skills") {
            &["propose_skill"]
        } else {
            &[]
        };
        let wakes: &[&str] = if cfg!(feature = "followups") {
            &["wake_me", "cancel_wake"]
        } else {
            &[]
        };
        let lists = cfg!(any(
            feature = "plans",
            feature = "followups",
            feature = "skills"
        ));
        let want: Vec<&str> = lists
            .then_some("list_plans")
            .into_iter()
            .chain(goals.iter().copied())
            .chain(cfg!(feature = "self-debug").then_some("recent_wakes"))
            .chain(skills.iter().copied())
            .chain(wakes.iter().copied())
            .collect();
        assert_eq!(names, want);
        for tool in &tools {
            assert!(
                tool.effect() <= autumn_harvest_agent::ToolEffect::Internal,
                "{} must run on a read-only tick",
                tool.name()
            );
        }
        // `list_plans` shows text that a research read, so it is untrusted.
        for tool in tools.iter().filter(|tool| tool.name() == "list_plans") {
            assert!(
                tool.labels()
                    .iter()
                    .any(|label| label == autumn_harvest_agent::flow::labels::UNTRUSTED_INPUT)
            );
        }
    }
}
