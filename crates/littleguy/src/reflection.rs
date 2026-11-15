//! What a consolidation pass did, for the reflection after it (ADR 0030).
//!
//! The reflection used to say "new facts are in your notes" after every
//! pass. A pass whose model reply did not parse writes no fact, and still
//! ends as completed, so the prompt told a false premise. Now the daemon
//! reads the report of each pass and the facts that it wrote, and the
//! prompt lists them. A pass that wrote no fact gets no reflection.

use std::fmt::Write as _;
use std::str::FromStr;

use autumn_harvest::{ExecutionId, WorkflowEvent};
use autumn_harvest_agent::ConsolidationReport;
use autumn_harvest_sqlite::{ExecutionOutcome, SqliteRuntime};

/// The activity of a pass that writes its facts. Its input holds each
/// entry as the memory block gets it.
const WRITE_ACTIVITY: &str = "agent_consolidate_write";

/// The most facts that a reflection prompt lists.
pub const MAX_FACTS_SHOWN: usize = 40;

/// The most characters of one fact in the prompt.
const MAX_FACT_CHARS: usize = 240;

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// How many episodes it read.
    pub episodes: u64,
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
            stop: String::from("not completed"),
            ..Pass::default()
        });
    };
    let report: ConsolidationReport = serde_json::from_value(output.clone()).unwrap_or_default();
    let stop = output["stop"].as_str().unwrap_or("unknown").to_owned();
    let mut facts = Vec::new();
    if report.facts_written > 0 {
        for event in rt.load_history(id)? {
            if let WorkflowEvent::ActivityScheduled { name, input, .. } = event
                && name == WRITE_ACTIVITY
            {
                facts.extend(
                    input["facts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|fact| fact["entry"].as_str())
                        .map(str::to_owned),
                );
            }
        }
    }
    Ok(Pass {
        episodes: report.episodes,
        facts,
        stop,
    })
}

/// `text` on one line, clipped to `max` characters.
fn one_line(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
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
    let added = match facts.len() {
        1 => String::from("1 fact"),
        n => format!("{n} facts"),
    };
    let mut text = format!(
        "Your memory was just consolidated. The pass read {episodes} episodes and added \
         {added} to your memory block:\n"
    );
    for fact in facts.iter().take(MAX_FACTS_SHOWN) {
        text.push_str("- ");
        text.push_str(&one_line(fact, MAX_FACT_CHARS));
        text.push('\n');
    }
    if facts.len() > MAX_FACTS_SHOWN {
        let _ = writeln!(
            text,
            "- and {} more, in your memory block.",
            facts.len() - MAX_FACTS_SHOWN
        );
    }
    text.push_str(crate::wake::REFLECT_PROMPT);
    Some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pass_with_no_fact_gets_no_reflection() {
        let empty = Pass {
            episodes: 20,
            facts: Vec::new(),
            stop: String::from("invalid_reply"),
        };
        assert_eq!(prompt(&[empty]), None);
    }

    #[test]
    fn the_prompt_lists_the_facts_of_every_pass_and_then_what_to_do() {
        let one = Pass {
            episodes: 8,
            facts: vec![String::from("mark likes green tea [episodes: e1]")],
            stop: String::from("completed"),
        };
        let two = Pass {
            episodes: 4,
            facts: vec![String::from("mark works on\nlittleguy [episodes: e9]")],
            stop: String::from("completed"),
        };
        let text = prompt(&[one, two]).unwrap();
        assert!(text.contains("read 12 episodes and added 2 facts"));
        assert!(text.contains("- mark likes green tea [episodes: e1]\n"));
        assert!(
            text.contains("- mark works on littleguy [episodes: e9]\n"),
            "one line"
        );
        assert!(text.ends_with(crate::wake::REFLECT_PROMPT));
        assert!(!text.contains("new facts are in your notes"));
    }

    #[test]
    fn a_long_list_says_how_many_more() {
        let many = Pass {
            episodes: 50,
            facts: (0..MAX_FACTS_SHOWN + 3)
                .map(|n| format!("fact {n}"))
                .collect(),
            stop: String::from("completed"),
        };
        let text = prompt(&[many]).unwrap();
        assert!(text.contains("- and 3 more, in your memory block."));
    }
}
