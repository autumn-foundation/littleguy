//! What a consolidation pass did, for the reflection after it (ADR 0030,
//! ADR 0041).
//!
//! The reflection used to say "new facts are in your notes" after every
//! pass. A pass whose model reply did not parse writes no fact, and still
//! ends as completed, so the prompt told a false premise. Now the daemon
//! reads the report of each pass and the facts that it sent, and the
//! prompt lists them. A pass that sent no fact gets no reflection.
//!
//! The prompt counts what a pass sent, not what changed. The memory store
//! skips a fact that its block holds already, and refuses one that does not
//! fit. The engine counts both a skip and a new entry as written, so no
//! count here can say which facts are new (ADR 0041).

use std::fmt::Write as _;
use std::str::FromStr;

use autumn_harvest::{ExecutionId, WorkflowEvent};
use autumn_harvest_agent::{ConsolidationReport, ConsolidationStop};
use autumn_harvest_sqlite::{ExecutionOutcome, SqliteRuntime};
use chrono::{DateTime, Utc};

/// The activity of a pass that writes its facts and its mark. Its input
/// holds each entry as the memory block gets it, and the episode ids of
/// the mark.
const WRITE_ACTIVITY: &str = "agent_consolidate_write";

/// The activity of a pass that calls the model. Its input holds the
/// episodes that the pass read. A pass that ends without a mark has no
/// write, so its episode ids come from here.
const TURN_ACTIVITY: &str = "agent_consolidate_turn";

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// The execution id of the pass.
    pub exec: String,
    /// When the pass started, as its history records it.
    pub started: Option<DateTime<Utc>>,
    /// How many episodes it read.
    pub episodes: u64,
    /// The ids of the episodes that it read.
    pub episode_ids: Vec<String>,
    /// The facts that it sent to the memory store, as the memory block
    /// would hold them. The store skips one that is there already.
    pub sent: Vec<String>,
    /// The reason of each fact that the store refused, such as a full
    /// block. The sources of a refused fact stay unconsolidated.
    pub refusals: Vec<String>,
    /// Why it stopped, as the engine names it, such as `invalid_reply`.
    pub stop: String,
}

impl Pass {
    /// Whether the pass marked its episodes as read. A pass that ends
    /// without a mark, such as on `invalid_reply` or a spent budget,
    /// leaves them for the next pass, which reads them again.
    #[must_use]
    pub fn marked(&self) -> bool {
        matches!(
            serde_json::from_value(serde_json::Value::from(self.stop.as_str())),
            Ok(ConsolidationStop::Completed | ConsolidationStop::NothingToDo)
        )
    }
}

/// What the pass `exec` did. A pass that did not complete wrote nothing.
///
/// # Errors
///
/// Returns an error when the runtime cannot read the pass.
pub fn read_pass(rt: &SqliteRuntime, exec: &str) -> anyhow::Result<Pass> {
    let id = ExecutionId::from_str(exec)
        .map_err(|_| anyhow::anyhow!("`{exec}` is not an execution id"))?;
    let mut pass = Pass {
        exec: exec.to_owned(),
        ..Pass::default()
    };
    let ExecutionOutcome::Completed(output) = rt.outcome(id)? else {
        pass.stop = String::from("not completed");
        return Ok(pass);
    };
    let report: ConsolidationReport = serde_json::from_value(output.clone()).unwrap_or_default();
    output["stop"]
        .as_str()
        .unwrap_or("unknown")
        .clone_into(&mut pass.stop);
    pass.episodes = report.episodes;
    pass.refusals = report.refusals;
    let mut marked = None;
    let mut read = Vec::new();
    for event in rt.load_history(id)? {
        match event {
            WorkflowEvent::WorkflowStarted { timestamp, .. } => pass.started = Some(timestamp),
            WorkflowEvent::ActivityScheduled { name, input, .. } if name == WRITE_ACTIVITY => {
                pass.sent.extend(strings(&input["facts"], "entry"));
                marked
                    .get_or_insert_with(Vec::new)
                    .extend(strings(&input["mark"]["episodes"], ""));
            }
            WorkflowEvent::ActivityScheduled { name, input, .. } if name == TURN_ACTIVITY => {
                read.extend(strings(&input["episodes"], "id"));
            }
            _ => {}
        }
    }
    // A write that started before a reset keeps its facts in the history,
    // but the pass ended without them.
    if report.stop == ConsolidationStop::Reset {
        pass.sent.clear();
    }
    pass.episode_ids = marked.unwrap_or(read);
    pass.episode_ids.dedup();
    Ok(pass)
}

/// The strings of the array `list`: the field `key` of each item, or each
/// item itself when `key` is empty.
fn strings<'a>(list: &'a serde_json::Value, key: &'a str) -> impl Iterator<Item = String> + 'a {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(move |item| {
            if key.is_empty() {
                item.as_str()
            } else {
                item[key].as_str()
            }
        })
        .map(str::to_owned)
}

/// `text` on one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `n` of `what`, with a plural `s` when `n` is not 1.
fn count(n: usize, what: &str) -> String {
    if n == 1 {
        format!("1 {what}")
    } else {
        format!("{n} {what}s")
    }
}

/// The head line of `pass`: its id, its start and what it read.
fn head(pass: &Pass) -> String {
    let started = pass.started.map_or_else(
        || String::from("at an unknown time"),
        |at| at.format("%Y-%m-%d %H:%M UTC").to_string(),
    );
    let ids = if pass.episode_ids.is_empty() {
        String::new()
    } else {
        format!(" ({})", pass.episode_ids.join(", "))
    };
    format!(
        "Pass {} started {started}, read {}{ids}",
        pass.exec,
        count(
            usize::try_from(pass.episodes).unwrap_or(usize::MAX),
            "episode"
        ),
    )
}

/// The line that says `pass` ended without a mark, or `None` when it
/// marked its episodes.
fn unmarked(pass: &Pass) -> Option<String> {
    (!pass.marked()).then(|| {
        format!(
            "It ended {} without a mark. Its episodes stay unconsolidated, and the next \
             pass reads them again.\n",
            pass.stop
        )
    })
}

/// The prompt of the reflection on `passes`, or `None` when they sent no
/// fact, so there is nothing to reflect on.
#[must_use]
pub fn prompt(passes: &[Pass]) -> Option<String> {
    if passes.iter().all(|pass| pass.sent.is_empty()) {
        return None;
    }
    // The prompt says what the store does with a fact, and when this
    // prompt read the block. It never says the block changed.
    let mut text = String::from(
        "Your memory was just consolidated. A pass sends its facts to your memory store. \
         The store adds a fact that the block lacks, skips one that it holds already, and \
         refuses one that does not fit. So a count here is what a pass sent, not what \
         changed. Your memory block in this prompt was read when this tick started, after \
         these passes ended.\n",
    );
    for pass in passes {
        text.push('\n');
        text.push_str(&head(pass));
        if pass.sent.is_empty() {
            text.push_str(", and sent no fact.\n");
        } else {
            // A write to a String cannot fail.
            let _ = writeln!(text, ", and sent {}:", count(pass.sent.len(), "fact"));
            // Every fact, whole: the reflection judges what the pass sent.
            for fact in &pass.sent {
                text.push_str("- ");
                text.push_str(&one_line(fact));
                text.push('\n');
            }
        }
        if !pass.refusals.is_empty() {
            let reasons: Vec<String> = pass
                .refusals
                .iter()
                .map(|reason| one_line(reason))
                .collect();
            let _ = writeln!(
                text,
                "The store refused {}: {}. The episodes of a refused fact stay unconsolidated.",
                pass.refusals.len(),
                reasons.join("; ")
            );
        }
        if let Some(line) = unmarked(pass) {
            text.push_str(&line);
        }
    }
    text.push('\n');
    text.push_str(crate::wake::REFLECT_PROMPT);
    Some(text)
}

/// The tick log row of `passes` when they sent no fact, so no reflection
/// runs. A pass that ended without a mark says so, because its episodes
/// wait for the next pass, which can fail the same way.
#[must_use]
pub fn skipped(passes: &[Pass]) -> String {
    let Some(first) = passes.first() else {
        return String::from("skipped: no consolidation pass; no model call");
    };
    if first.marked() {
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

    fn pass(exec: &str, stop: &str, ids: &[&str], sent: &[&str]) -> Pass {
        Pass {
            exec: exec.to_owned(),
            started: Some(
                DateTime::parse_from_rfc3339("2026-11-17T03:00:00Z")
                    .unwrap()
                    .with_timezone(&Utc),
            ),
            episodes: ids.len() as u64,
            episode_ids: ids.iter().map(|id| (*id).to_owned()).collect(),
            sent: sent.iter().map(|fact| (*fact).to_owned()).collect(),
            refusals: Vec::new(),
            stop: stop.to_owned(),
        }
    }

    #[test]
    fn a_pass_with_no_fact_gets_no_reflection() {
        let empty = pass("p1", "invalid_reply", &["e1"; 20], &[]);
        assert_eq!(prompt(&[empty]), None);
    }

    #[test]
    fn the_prompt_lists_the_facts_of_every_pass_and_then_what_to_do() {
        let one = pass(
            "p1",
            "completed",
            &["e1"],
            &["mark likes green tea [episodes: e1]"],
        );
        let two = pass(
            "p2",
            "completed",
            &["e9"],
            &["mark works on\nlittleguy [episodes: e9]"],
        );
        let text = prompt(&[one, two]).unwrap();
        assert!(text.contains("read 1 episode (e1), and sent 1 fact:\n"));
        assert!(text.contains("- mark likes green tea [episodes: e1]\n"));
        assert!(
            text.contains("- mark works on littleguy [episodes: e9]\n"),
            "one line"
        );
        assert!(text.ends_with(crate::wake::REFLECT_PROMPT));
        assert!(!text.contains("new facts are in your notes"));
    }

    #[test]
    fn the_prompt_counts_what_was_sent_and_never_claims_the_block_changed() {
        // A drain of facts that the block holds already: the store keeps
        // none of them, and the prompt must not say it added them.
        let drain = pass("p1", "completed", &["e1", "e2"], &["a [episodes: e1]"; 12]);
        let text = prompt(&[drain]).unwrap();
        assert!(text.contains("sent 12 facts:"));
        assert!(!text.contains("added"), "no claim of a change: {text}");
        assert!(!text.contains("to your memory block"));
        assert!(text.contains("skips one that it holds already"));
        assert!(text.contains("what a pass sent, not what changed"));
    }

    #[test]
    fn the_prompt_names_each_pass_its_start_and_its_episodes() {
        let one = pass("01JPASS", "completed", &["i1:s4", "i1:s5"], &["f"]);
        let text = prompt(&[one]).unwrap();
        assert!(
            text.contains(
                "Pass 01JPASS started 2026-11-17 03:00 UTC, read 2 episodes (i1:s4, i1:s5)"
            ),
            "{text}"
        );
    }

    #[test]
    fn a_pass_with_no_start_or_ids_still_reads() {
        let mut bare = pass("p1", "completed", &[], &["f"]);
        bare.started = None;
        bare.episodes = 3;
        let text = prompt(&[bare]).unwrap();
        assert!(
            text.contains("Pass p1 started at an unknown time, read 3 episodes, and sent 1 fact:")
        );
    }

    #[test]
    fn a_pass_that_ended_without_a_mark_says_so_out_loud() {
        let good = pass("p1", "completed", &["e1"], &["f [episodes: e1]"]);
        let stuck = pass("p2", "invalid_reply", &["e7", "e8"], &[]);
        let text = prompt(&[good, stuck]).unwrap();
        assert!(text.contains(
            "Pass p2 started 2026-11-17 03:00 UTC, read 2 episodes (e7, e8), and sent no fact.\n"
        ));
        assert!(text.contains("It ended invalid_reply without a mark."));
        assert!(text.contains("the next pass reads them again"));
        assert_eq!(
            text.matches("without a mark").count(),
            1,
            "only the stuck pass"
        );
    }

    #[test]
    fn a_refused_fact_is_named_with_its_reason() {
        let mut full = pass("p1", "completed", &["e1"], &["f1", "f2"]);
        full.refusals = vec![String::from("the block \"memory\" is\nfull")];
        let text = prompt(&[full]).unwrap();
        assert!(text.contains("The store refused 1: the block \"memory\" is full."));
    }

    #[test]
    fn only_completed_and_nothing_to_do_count_as_marked() {
        for stop in ["completed", "nothing_to_do"] {
            assert!(pass("p", stop, &[], &[]).marked(), "{stop}");
        }
        for stop in [
            "invalid_reply",
            "budget_exceeded",
            "disabled",
            "reset",
            "not completed",
            "unknown",
        ] {
            assert!(!pass("p", stop, &[], &[]).marked(), "{stop}");
        }
    }

    #[test]
    fn a_skipped_reflection_says_when_the_episodes_wait() {
        let stuck = pass("p1", "invalid_reply", &["e1"; 20], &[]);
        assert_eq!(
            skipped(&[stuck]),
            "skipped: the consolidation pass sent no facts (invalid_reply) and ended without \
             a mark, so 20 episodes wait for the next pass; no model call"
        );
        let idle = pass("p2", "nothing_to_do", &[], &[]);
        assert_eq!(
            skipped(&[idle]),
            "skipped: the consolidation pass sent no facts (nothing_to_do); no model call"
        );
        assert_eq!(
            skipped(&[]),
            "skipped: no consolidation pass; no model call"
        );
    }

    #[test]
    fn a_long_list_shows_every_fact_whole() {
        let long = "y".repeat(1_000);
        let mut facts: Vec<String> = (0..100).map(|n| format!("fact {n}")).collect();
        facts.push(long.clone());
        let mut many = pass("p1", "completed", &[], &[]);
        many.sent = facts;
        let text = prompt(&[many]).unwrap();
        assert!(text.contains("- fact 99\n"));
        assert!(text.contains(&format!("- {long}\n")));
        assert!(!text.contains("more, in your memory block"));
    }
}
