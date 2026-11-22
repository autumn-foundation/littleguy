//! What a consolidation pass did, for the reflection after it (ADR 0030).
//!
//! The reflection used to say "new facts are in your notes" after every
//! pass. A pass whose model reply did not parse writes no fact, and still
//! ends as completed, so the prompt told a false premise. Now the daemon
//! reads the report of each pass and the facts that it wrote, and the
//! prompt lists them. A pass that wrote no fact gets no reflection.
//!
//! The prompt also names each pass, the episodes that it read, and whether
//! it marked them (idea #25). A pass that ends without a mark leaves its
//! episodes pending, so the next pass reads them again. With the ids in the
//! prompt, the assistant can tell a repeat of one batch from a backlog that
//! drains.
//!
//! The prompt counts what the passes sent, not what changed (goal #11). The
//! memory store skips a fact that its block holds already, and the engine
//! still counts it as written. So no count here can say which facts are new.

use std::fmt::Write as _;
use std::str::FromStr;

use autumn_harvest::{ExecutionId, WorkflowEvent};
use autumn_harvest_agent::{ConsolidationReport, ConsolidationStop};
use autumn_harvest_sqlite::{ExecutionOutcome, SqliteRuntime};
use chrono::{DateTime, Utc};

/// The activity of a pass that reads its episodes. Its output holds each
/// episode with its id.
const READ_ACTIVITY: &str = "agent_consolidate_read";

/// The activity of a pass that writes its facts. Its input holds each
/// entry as the memory block gets it.
const WRITE_ACTIVITY: &str = "agent_consolidate_write";

/// The note on the memory block. The engine reads the block once per
/// segment, and each op is a new segment. The reflection is an op that
/// starts after the pass, so its block already holds the new facts. An
/// assistant that does not know this reads them as old facts that the pass
/// derived again (idea #25).
///
/// The store skips a fact that the block held before, and the engine
/// counts it as written all the same. So the note never says that every
/// fact is new (goal #11).
pub const MEMORY_NOTE: &str = "The Memory section of your instructions was read when \
     this reflection started, after the passes. So it already holds each fact above that \
     the store kept. The count is what the passes sent, not what changed: the store skips \
     a fact that the block held before, and refuses one that does not fit. So a fact \
     above is new from these passes unless the block held it before.";

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// The execution id of the pass. It names this pass instance.
    pub exec: String,
    /// When the pass started, or `None` when its history has no start.
    pub started: Option<DateTime<Utc>>,
    /// How many episodes it read.
    pub episodes: u64,
    /// The ids of the episodes that it read.
    pub episode_ids: Vec<String>,
    /// The pass marked its episodes as consolidated. An unmarked episode
    /// stays pending, so the next pass reads it again.
    pub marked: bool,
    /// The facts that the memory store refused. Their source episodes stay
    /// pending.
    pub refused: u64,
    /// The reason of each refused fact, such as a full block.
    pub refusals: Vec<String>,
    /// The facts that it wrote, as the memory block holds them.
    pub facts: Vec<String>,
    /// Why it stopped, as the engine names it, such as `invalid_reply`.
    pub stop: String,
}

/// What the pass `exec` did. A pass that did not complete wrote nothing.
///
/// # Errors
///
/// Returns an error when the runtime cannot read the pass.
pub fn read_pass(rt: &SqliteRuntime, exec: &str) -> anyhow::Result<Pass> {
    let id = ExecutionId::from_str(exec)
        .map_err(|_| anyhow::anyhow!("`{exec}` is not an execution id"))?;
    let ExecutionOutcome::Completed(output) = rt.outcome(id)? else {
        return Ok(Pass {
            exec: exec.to_owned(),
            stop: String::from("not completed"),
            ..Pass::default()
        });
    };
    let report: ConsolidationReport = serde_json::from_value(output.clone()).unwrap_or_default();
    let mut pass = Pass {
        exec: exec.to_owned(),
        episodes: report.episodes,
        // Only a completed pass applies its mark. Every other stop leaves
        // the episodes pending.
        marked: report.stop == ConsolidationStop::Completed,
        refused: report.facts_refused,
        refusals: report.refusals,
        stop: output["stop"].as_str().unwrap_or("unknown").to_owned(),
        ..Pass::default()
    };
    let mut read = None;
    for event in rt.load_history(id)? {
        match event {
            WorkflowEvent::WorkflowStarted { timestamp, .. } => {
                pass.started
                    .get_or_insert_with(|| crate::instant::from_engine(timestamp));
            }
            WorkflowEvent::ActivityScheduled {
                activity_id, name, ..
            } if name == READ_ACTIVITY => read = Some(activity_id),
            WorkflowEvent::ActivityCompleted {
                activity_id,
                output,
            } if read == Some(activity_id) => {
                pass.episode_ids = strings(&output["episodes"], "id");
            }
            WorkflowEvent::ActivityScheduled { name, input, .. }
                if name == WRITE_ACTIVITY && report.facts_written > 0 =>
            {
                pass.facts.extend(strings(&input["facts"], "entry"));
            }
            _ => {}
        }
    }
    Ok(pass)
}

/// The string field `key` of each object in the array `list`.
fn strings(list: &serde_json::Value, key: &str) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| item[key].as_str())
        .map(str::to_owned)
        .collect()
}

/// `text` on one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `n` of `noun`, with a plural `s` when `n` is not 1.
fn count(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

/// One line on what `pass` read and whether it marked it.
fn pass_line(pass: &Pass) -> String {
    let mut line = format!("- pass {}", pass.exec);
    if let Some(started) = pass.started {
        let _ = write!(line, ", started {}", started.format("%Y-%m-%dT%H:%M:%SZ"));
    }
    let _ = write!(
        line,
        ", ended {}: it read {}",
        pass.stop,
        count(pass.episodes, "episode")
    );
    if !pass.episode_ids.is_empty() {
        let _ = write!(line, " ({})", pass.episode_ids.join(", "));
    }
    if pass.episodes == 0 {
        line.push('.');
    } else if pass.marked {
        line.push_str(" and marked them, so no later pass reads them again.");
    } else {
        line.push_str(" and marked none, so the next pass reads them again.");
    }
    if pass.refused > 0 {
        let _ = write!(line, " The store refused {}", count(pass.refused, "fact"));
        if !pass.refusals.is_empty() {
            let reasons: Vec<String> = pass.refusals.iter().map(|why| one_line(why)).collect();
            let _ = write!(line, " ({})", reasons.join("; "));
        }
        line.push_str(", so their source episodes stay pending.");
    }
    line
}

/// The prompt of the reflection on `passes`, or `None` when they wrote no
/// fact, so there is nothing to reflect on.
#[must_use]
pub fn prompt(passes: &[Pass]) -> Option<String> {
    let facts: Vec<&String> = passes.iter().flat_map(|pass| &pass.facts).collect();
    if facts.is_empty() {
        return None;
    }
    let episodes: u64 = passes.iter().map(|pass| pass.episodes).sum();
    // The count is what the passes sent. The store skips a fact that the
    // block holds already, so the prompt never says the block changed.
    let mut text = format!(
        "Your memory was just consolidated. The passes read {} and sent {} to your \
         memory store:\n",
        count(episodes, "episode"),
        count(facts.len() as u64, "fact")
    );
    // Every fact, whole: the reflection judges what the passes sent.
    for fact in &facts {
        text.push_str("- ");
        text.push_str(&one_line(fact));
        text.push('\n');
    }
    // Each pass by id, so a repeat of one batch shows as a repeat.
    text.push_str("The passes:\n");
    for pass in passes {
        text.push_str(&pass_line(pass));
        text.push('\n');
    }
    text.push_str(MEMORY_NOTE);
    text.push('\n');
    text.push_str(crate::wake::REFLECT_PROMPT);
    Some(text)
}

/// The tick log row of `passes` when they sent no fact, so no reflection
/// runs. A pass that ended without a mark says so, because its episodes
/// wait for the next pass, which can fail the same way. Only the tick log
/// of `self-debug` reads it.
#[cfg(feature = "self-debug")]
#[must_use]
pub fn skipped(passes: &[Pass]) -> String {
    let Some(first) = passes.first() else {
        return String::from("skipped: no consolidation pass; no model call");
    };
    if first.marked || first.episodes == 0 {
        return format!(
            "skipped: the consolidation pass sent no facts ({}); no model call",
            first.stop
        );
    }
    let waiting = match first.episodes {
        1 => String::from("1 episode waits"),
        n => format!("{n} episodes wait"),
    };
    format!(
        "skipped: the consolidation pass sent no facts ({}) and ended without a mark, so \
         {waiting} for the next pass; no model call",
        first.stop,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pass(exec: &str, ids: &[&str], facts: &[&str], stop: &str) -> Pass {
        Pass {
            exec: exec.to_owned(),
            started: None,
            episodes: ids.len() as u64,
            episode_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            marked: stop == "completed",
            refused: 0,
            refusals: Vec::new(),
            facts: facts.iter().map(|fact| (*fact).to_owned()).collect(),
            stop: stop.to_owned(),
        }
    }

    #[test]
    fn a_pass_with_no_fact_gets_no_reflection() {
        let empty = Pass {
            episodes: 20,
            stop: String::from("invalid_reply"),
            ..Pass::default()
        };
        assert_eq!(prompt(&[empty]), None);
    }

    #[test]
    fn the_prompt_lists_the_facts_of_every_pass_and_then_what_to_do() {
        let one = Pass {
            episodes: 8,
            ..pass(
                "p1",
                &[],
                &["mark likes green tea [episodes: e1]"],
                "completed",
            )
        };
        let two = Pass {
            episodes: 4,
            ..pass(
                "p2",
                &[],
                &["mark works on\nlittleguy [episodes: e9]"],
                "completed",
            )
        };
        let text = prompt(&[one, two]).unwrap();
        assert!(
            text.contains("The passes read 12 episodes and sent 2 facts to your memory store:\n")
        );
        assert!(text.contains("- mark likes green tea [episodes: e1]\n"));
        assert!(
            text.contains("- mark works on littleguy [episodes: e9]\n"),
            "one line"
        );
        assert!(text.ends_with(crate::wake::REFLECT_PROMPT));
        assert!(!text.contains("new facts are in your notes"));
    }

    #[test]
    fn a_long_list_shows_every_fact_whole() {
        let long = "y".repeat(1_000);
        let mut facts: Vec<String> = (0..100).map(|n| format!("fact {n}")).collect();
        facts.push(long.clone());
        let many = Pass {
            episodes: 50,
            facts,
            stop: String::from("completed"),
            ..Pass::default()
        };
        let text = prompt(&[many]).unwrap();
        assert!(text.contains("- fact 99\n"));
        assert!(text.contains(&format!("- {long}\n")));
        assert!(!text.contains("more, in your memory block"));
    }

    #[test]
    fn each_pass_shows_its_id_its_start_and_the_episodes_that_it_marked() {
        let started = DateTime::parse_from_rfc3339("2026-11-17T09:00:03.250Z")
            .unwrap()
            .with_timezone(&Utc);
        let done = Pass {
            started: Some(started),
            ..pass(
                "pass-a",
                &["r:s0", "r:s1"],
                &["mark likes tea"],
                "completed",
            )
        };
        let text = prompt(&[done]).unwrap();
        assert!(
            text.contains(
                "- pass pass-a, started 2026-11-17T09:00:03Z, ended completed: it read \
                 2 episodes (r:s0, r:s1) and marked them, so no later pass reads them again.\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_pass_that_ended_without_a_mark_says_its_episodes_come_back() {
        let done = pass("pass-a", &["r:s0"], &["mark likes tea"], "completed");
        let stuck = pass("pass-b", &["r:s1", "r:s2"], &[], "invalid_reply");
        let text = prompt(&[done, stuck]).unwrap();
        assert!(
            text.contains(
                "- pass pass-b, ended invalid_reply: it read 2 episodes (r:s1, r:s2) and \
                 marked none, so the next pass reads them again.\n"
            ),
            "{text}"
        );
        assert!(text.contains("read 1 episode (r:s0) and marked them"));
    }

    #[test]
    fn a_refused_fact_keeps_its_sources_pending() {
        let refused = Pass {
            refused: 2,
            ..pass("pass-a", &["r:s0"], &["mark likes tea"], "completed")
        };
        let text = prompt(&[refused]).unwrap();
        assert!(
            text.contains(" The store refused 2 facts, so their source episodes stay pending."),
            "{text}"
        );
    }

    #[test]
    fn the_prompt_says_the_memory_block_already_holds_the_new_facts() {
        let done = pass("pass-a", &["r:s0"], &["mark likes tea"], "completed");
        let text = prompt(&[done]).unwrap();
        let note = text.find(MEMORY_NOTE).expect("the memory note");
        assert!(
            note > text.find("The passes:\n").unwrap(),
            "after the passes"
        );
        assert!(note < text.find(crate::wake::REFLECT_PROMPT).unwrap());
    }

    #[test]
    fn a_pass_with_no_episode_says_only_what_it_read() {
        let idle = pass("pass-c", &[], &[], "nothing_to_do");
        assert_eq!(
            pass_line(&idle),
            "- pass pass-c, ended nothing_to_do: it read 0 episodes."
        );
    }

    #[test]
    fn the_prompt_counts_what_was_sent_and_never_claims_the_block_changed() {
        // A drain of facts that the block holds already: the store keeps
        // none of them, so the prompt must not say it added them.
        let drain = pass(
            "pass-a",
            &["r:s0"],
            &["a [episodes: r:s0]"; 12],
            "completed",
        );
        let text = prompt(&[drain]).unwrap();
        assert!(text.contains("sent 12 facts to your memory store:"));
        assert!(!text.contains("added"), "no claim of a change: {text}");
        assert!(!text.contains("not facts that you held before"));
        assert!(text.contains("unless the block held it before"));
    }

    #[test]
    fn a_refused_fact_is_named_with_its_reason() {
        let full = Pass {
            refused: 1,
            refusals: vec![String::from("the block \"memory\" is\nfull")],
            ..pass("pass-a", &["r:s0"], &["f1", "f2"], "completed")
        };
        let text = prompt(&[full]).unwrap();
        assert!(
            text.contains(
                " The store refused 1 fact (the block \"memory\" is full), so their source \
                 episodes stay pending."
            ),
            "{text}"
        );
    }

    #[cfg(feature = "self-debug")]
    #[test]
    fn a_skipped_reflection_says_when_the_episodes_wait() {
        let stuck = Pass {
            episodes: 20,
            ..pass("p1", &[], &[], "invalid_reply")
        };
        assert_eq!(
            skipped(&[stuck]),
            "skipped: the consolidation pass sent no facts (invalid_reply) and ended without \
             a mark, so 20 episodes wait for the next pass; no model call"
        );
        let idle = pass("p2", &[], &[], "nothing_to_do");
        assert_eq!(
            skipped(&[idle]),
            "skipped: the consolidation pass sent no facts (nothing_to_do); no model call"
        );
        assert_eq!(
            skipped(&[]),
            "skipped: no consolidation pass; no model call"
        );
    }
}
