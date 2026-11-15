//! The owner's side of goals and ideas (ADR 0020): the listing, and each
//! decision that the assistant cannot make itself.

use anyhow::Result;
use chrono::Utc;
use littleguy_stores::plans::{Goal, Idea, Note, Plans};

use super::Daemon;
use crate::protocol::{GoalEntry, IdeaEntry, PlanDecision, PlanNote, Response};

fn notes(notes: &[Note]) -> Vec<PlanNote> {
    notes
        .iter()
        .map(|note| PlanNote {
            at_ms: note.at_ms,
            text: note.text.clone(),
        })
        .collect()
}

fn goal(goal: &Goal) -> GoalEntry {
    GoalEntry {
        id: goal.id,
        status: goal.status.as_str().to_owned(),
        title: goal.title.clone(),
        why: goal.why.clone(),
        check_every_minutes: goal.check_every_secs.map(|secs| secs / 60),
        next_check_ms: goal.next_check_ms,
        idea: goal.idea,
        notes: notes(&goal.notes),
    }
}

fn idea(idea: &Idea) -> IdeaEntry {
    IdeaEntry {
        id: idea.id,
        title: idea.title.clone(),
        body: idea.body.clone(),
        created_ms: idea.created_ms,
        notes: notes(&idea.notes),
    }
}

/// The listing of `plans`.
fn listing(plans: &Plans) -> Response {
    Response::Plans {
        goals: plans.goals.iter().map(goal).collect(),
        ideas: plans.ideas.iter().map(idea).collect(),
    }
}

/// The answer when the store of goals and ideas is closed.
fn off() -> Response {
    Response::Error {
        message: String::from("goals and ideas are off: set `plans = true` under [tools]"),
    }
}

impl Daemon {
    /// The goals and the ideas.
    pub(super) fn plans(&self) -> Result<Response> {
        let Some(stores) = self
            .kit
            .stores
            .as_ref()
            .filter(|_| self.kit.config.tools.plans)
        else {
            return Ok(off());
        };
        Ok(listing(&stores.plans()?))
    }

    /// Applies the owner's `decision` on goal or idea `id`, and returns the
    /// listing after it.
    pub(super) fn decide_plan(&self, decision: PlanDecision, id: i64) -> Result<Response> {
        let Some(stores) = self
            .kit
            .stores
            .as_ref()
            .filter(|_| self.kit.config.tools.plans)
        else {
            return Ok(off());
        };
        let (changed, what) = match decision {
            PlanDecision::Accept => (
                stores.accept_goal(id, Utc::now().timestamp_millis())?,
                "a proposed goal",
            ),
            PlanDecision::Decline => (stores.decline_goal(id)?, "a proposed goal"),
            PlanDecision::DropGoal => (stores.drop_goal(id)?, "an active goal"),
            PlanDecision::DropIdea => (stores.drop_idea(id)?, "an open idea"),
        };
        if !changed {
            return Ok(Response::Error {
                message: format!("{id} is not {what}"),
            });
        }
        tracing::info!(?decision, id, "the owner decided on a plan");
        Ok(listing(&stores.plans()?))
    }
}
