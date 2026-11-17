//! The council: model calls that decide the proposals of the assistant for
//! the owner (ADR 0027).
//!
//! When an op ends, the daemon marks each new proposal. A proposal from a
//! run that read outside text is tainted, and it waits for the owner. On
//! each tick, the council decides the oldest clean proposal: each member is
//! one model call with no tools, and more than half must accept. A goal
//! that it accepts becomes active, and a draft becomes a `SKILL.md` by
//! `council`. A decline closes the proposal. Each decision is a row in the
//! council log.

use anyhow::Result;
use autumn_harvest_agent::flow::labels;
use autumn_harvest_agent::{
    AgentModel, AgentState, ChatMessage, ChatRequest, ChatRole, ContentPart, DrawId,
};
use chrono::{DateTime, Utc};
use littleguy_core::{CouncilAccepts, CouncilConfig};
use littleguy_stores::SqliteStores;
use littleguy_stores::council::{Proposal, ProposalKind};
use serde_json::{Value, json};

use super::Daemon;

/// The author of a skill that the council accepted.
const COUNCIL_AUTHOR: &str = "council";

/// What each member reads first.
const COUNCIL_SYSTEM: &str = "You sit on a council that decides the proposals of an AI \
     agent while its owner is away. You read the charter of the agent and one proposal: a \
     goal to work on, or a skill, which is know-how that the agent loads when a task needs \
     it. Accept a proposal only when it fits the charter, is concrete, and asks for nothing \
     that the owner would not want. Decline one that repeats an open goal or a skill. Answer \
     with JSON only: {\"accept\": true or false, \"reason\": \"one short sentence\"}.";

/// One vote of one member.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Vote {
    accept: bool,
    reason: String,
}

/// The vote in the answer `text`: the first JSON object in it. An answer
/// that does not parse is a vote to decline.
fn vote_in(text: &str) -> Vote {
    let object = text
        .find('{')
        .zip(text.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<Value>(&text[start..=end]).ok());
    match object {
        Some(value) if value["accept"].is_boolean() => Vote {
            accept: value["accept"].as_bool().unwrap_or(false),
            reason: value["reason"]
                .as_str()
                .unwrap_or("no reason given")
                .trim()
                .to_owned(),
        },
        _ => Vote {
            accept: false,
            reason: String::from("the answer did not parse"),
        },
    }
}

/// Whether more than half of `votes` accept.
fn carried(votes: &[Vote]) -> bool {
    votes.iter().filter(|vote| vote.accept).count() * 2 > votes.len()
}

/// What a member reads about the agent and the proposal.
fn brief(charter: &str, open: &str, proposal: &Proposal) -> String {
    let what = match proposal.kind {
        ProposalKind::Goal => "a goal",
        ProposalKind::Skill => "a skill",
    };
    format!(
        "The charter of the agent:\n{charter}\n\nWhat it has now:\n{open}\n\nThe proposal, {what} \
         named {title}:\n{text}",
        title = proposal.title,
        text = proposal.text,
    )
}

/// The open goals and the skills of the agent, one per line.
fn open_work(stores: &SqliteStores, skills: &[(String, String)]) -> String {
    let goals = stores.plans().map(|plans| plans.goals).unwrap_or_default();
    let mut lines: Vec<String> = goals
        .iter()
        .filter(|goal| goal.status.as_str() == "active")
        .map(|goal| format!("- goal: {}", goal.title))
        .collect();
    lines.extend(
        skills
            .iter()
            .map(|(name, description)| format!("- skill {name}: {description}")),
    );
    if lines.is_empty() {
        String::from("nothing yet")
    } else {
        lines.join("\n")
    }
}

/// One member's vote on `brief`. Each member is its own draw, so a cache
/// never gives two members one answer.
async fn ask_member(model: &dyn AgentModel, brief: &str, member: u32) -> Vote {
    let request = ChatRequest {
        messages: vec![
            ChatMessage::text(ChatRole::System, COUNCIL_SYSTEM),
            ChatMessage::text(ChatRole::User, brief),
        ],
        tools: Vec::new(),
        // No output cap: a reasoning model thinks first, and a cap can end
        // the vote before its answer.
        max_tokens: None,
        temperature: None,
        draw: Some(DrawId::new(member)),
    };
    match model.chat(&request).await {
        Ok(response) => {
            let text: String = response
                .content
                .iter()
                .filter_map(|part| match part {
                    ContentPart::Text(text) => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            vote_in(&text)
        }
        Err(error) => Vote {
            accept: false,
            reason: format!("the model call failed: {error}"),
        },
    }
}

/// Whether the run of `state` read outside text. `tracked` says whether
/// the run records labels at all: without flow rules it records none, so
/// nothing says that it is clean.
fn tainted(state: Option<&AgentState>, tracked: bool) -> bool {
    !tracked || state.is_none_or(|state| state.flow_labels.contains(labels::UNTRUSTED_INPUT))
}

impl Daemon {
    /// The council of this daemon, when it has one.
    fn council(&self) -> Option<&CouncilConfig> {
        self.kit.config.council.as_ref()
    }

    /// Marks the proposals that the ops since `started` filed, from the
    /// taint of the run in `state`.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot write.
    pub(super) fn mark_proposals(
        &self,
        started: DateTime<Utc>,
        state: Option<&AgentState>,
    ) -> Result<()> {
        let (Some(_), Some(stores)) = (self.council(), self.kit.stores.as_ref()) else {
            return Ok(());
        };
        // With the Rule of Two off there are no flow rules, so no taint.
        let tracked = self.kit.config.flow.rule_of_two;
        let tainted = tainted(state, tracked);
        let marked = stores.mark_proposals(started.timestamp_millis(), tainted)?;
        if marked > 0 && !tracked {
            tracing::warn!(
                marked,
                "proposals wait for the owner: the council needs the Rule of Two to see taint"
            );
        } else if marked > 0 {
            tracing::info!(marked, tainted, "proposals marked for the council");
        }
        Ok(())
    }

    /// Decides the oldest clean proposal, when the council has one to
    /// decide. One decision per call keeps a tick short.
    ///
    /// # Errors
    ///
    /// Returns an error when the store or the skills folder cannot write.
    pub(super) async fn council_turn(&mut self) -> Result<()> {
        let Some(council) = self.council().cloned() else {
            return Ok(());
        };
        let Some(stores) = self.kit.stores.clone() else {
            return Ok(());
        };
        let dir = self.skills_dir();
        let goals = council.accept.contains(&CouncilAccepts::Goals);
        let skills = council.accept.contains(&CouncilAccepts::Skills) && dir.is_some();
        let Some(proposal) = stores.next_for_council(goals, skills)? else {
            return Ok(());
        };
        let loaded: Vec<(String, String)> = dir
            .as_deref()
            .and_then(|dir| crate::skills::load(dir).ok())
            .map(|shelf| {
                shelf
                    .skills
                    .into_iter()
                    .map(|skill| (skill.name, skill.description))
                    .collect()
            })
            .unwrap_or_default();
        let charter = council
            .charter
            .clone()
            .unwrap_or_else(|| self.kit.config.system.clone());
        let brief = brief(&charter, &open_work(&stores, &loaded), &proposal);
        let mut votes = Vec::new();
        for member in 0..council.members {
            votes.push(ask_member(self.kit.model.as_ref(), &brief, member).await);
        }
        let accepted = carried(&votes);
        let now = (self.clock)();
        let reasons: Vec<&str> = votes
            .iter()
            .filter(|vote| vote.accept == accepted)
            .map(|vote| vote.reason.as_str())
            .collect();
        match (proposal.kind, accepted) {
            (ProposalKind::Goal, true) => {
                stores.accept_goal(proposal.id, now.timestamp_millis())?;
                let note = format!("Accepted by the council: {}", reasons.join(" "));
                stores.goal_progress(
                    &format!("council:goal:{}", proposal.id),
                    proposal.id,
                    &note,
                    false,
                    now.timestamp_millis(),
                )?;
            }
            (ProposalKind::Goal, false) => {
                stores.decline_goal(proposal.id)?;
            }
            (ProposalKind::Skill, true) => {
                if let (Some(dir), Some(draft)) = (dir.as_deref(), stores.skill_draft(proposal.id)?)
                {
                    crate::skills::write(
                        dir,
                        &draft.name,
                        &draft.description,
                        &draft.body,
                        COUNCIL_AUTHOR,
                    )?;
                    stores.close_skill_draft(proposal.id, true)?;
                    self.sync_skills().await;
                }
            }
            (ProposalKind::Skill, false) => {
                stores.close_skill_draft(proposal.id, false)?;
            }
        }
        let record: Vec<Value> = votes
            .iter()
            .map(|vote| json!({"accept": vote.accept, "reason": vote.reason}))
            .collect();
        stores.record_council(
            proposal.kind,
            proposal.id,
            accepted,
            &Value::Array(record).to_string(),
            now.timestamp_millis(),
        )?;
        tracing::info!(
            kind = proposal.kind.as_str(),
            id = proposal.id,
            accepted,
            "the council decided"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_vote_reads_the_first_object_and_a_bad_answer_declines() {
        assert_eq!(
            vote_in("Sure. {\"accept\": true, \"reason\": \"Fits.\"} Done."),
            Vote {
                accept: true,
                reason: String::from("Fits.")
            }
        );
        assert!(!vote_in("yes").accept);
        assert!(
            !vote_in("{\"accept\": \"yes\"}").accept,
            "a string is not a vote"
        );
        let yes = || Vote {
            accept: true,
            reason: String::new(),
        };
        let no = || Vote {
            accept: false,
            reason: String::new(),
        };
        assert!(carried(&[yes(), yes(), no()]));
        assert!(!carried(&[yes(), no()]), "a tie is not more than half");
        assert!(carried(&[yes()]));
    }
}
