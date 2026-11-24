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
//!
//! A pass can also replace and remove an entry (autumn-harvest#2797, engine
//! ADR 0020). The prompt lists each edit with the entry that it targets. The
//! write drops an edit whose target changed, is gone, or has a label that
//! the pass lacks. A dropped edit consumes its sources, so the prompt says
//! that no pass sends it again. The engine names a dropped edit by the hash
//! of its target, and the daemon matches that hash to the edit that it sent.

use std::fmt::Write as _;
use std::str::FromStr;

use autumn_harvest::{ExecutionId, WorkflowEvent};
use autumn_harvest_agent::episode::fact_hash;
use autumn_harvest_agent::{ConsolidationReport, ConsolidationStop};
use autumn_harvest_sqlite::{ExecutionOutcome, SqliteRuntime};
use jiff::Timestamp;

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
     the store kept, and no longer holds an entry that a pass replaced or removed. The count is what the passes sent, not what changed: the store skips \
     a fact that the block held before, and refuses one that does not fit. So a fact \
     above is new from these passes unless the block held it before.";

/// What an edit does to an entry of the block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    /// The edit writes a new text over the entry.
    Replace,
    /// The edit takes the entry out of the block.
    Remove,
}

impl Verb {
    /// The verb as the engine spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Remove => "remove",
        }
    }
}

/// A replace or a remove that a pass sent to the memory store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// What the edit does.
    pub verb: Verb,
    /// The whole block entry that the edit changes.
    pub target: String,
    /// The new text of a replace. Empty for a remove.
    pub entry: String,
    /// Why the write dropped the edit, or `None` when it did not.
    pub dropped: Option<String>,
}

/// What one pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pass {
    /// The execution id of the pass. It names this pass instance.
    pub exec: String,
    /// When the pass started, or `None` when its history has no start.
    pub started: Option<Timestamp>,
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
    /// The facts that it added, as the memory block holds them.
    pub facts: Vec<String>,
    /// The replaces and the removes that it sent.
    pub edits: Vec<Edit>,
    /// How many entries its removes took out of the block.
    pub removed: u64,
    /// The reason of each edit that the write dropped. A dropped edit
    /// consumes its source episodes, unlike a refused fact.
    pub edit_drops: Vec<String>,
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
    // Engine 0.11.0 has no field for a remove or a dropped edit. A pass of a
    // later engine reports them, so the daemon reads them from the output.
    let removed = output["facts_removed"].as_u64().unwrap_or(0);
    let edit_drops: Vec<String> = output["edit_drops"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(serde_json::Value::as_str)
        .map(str::to_owned)
        .collect();
    let mut pass = Pass {
        exec: exec.to_owned(),
        episodes: report.episodes,
        // Only a completed pass applies its mark. Every other stop leaves
        // the episodes pending.
        marked: report.stop == ConsolidationStop::Completed,
        refused: report.facts_refused,
        refusals: report.refusals,
        removed,
        edit_drops,
        stop: output["stop"].as_str().unwrap_or("unknown").to_owned(),
        ..Pass::default()
    };
    let mut read = None;
    for event in rt.load_history(id)? {
        match event {
            WorkflowEvent::WorkflowStarted { timestamp, .. } => {
                pass.started.get_or_insert(timestamp);
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
                if name == WRITE_ACTIVITY && (report.facts_written > 0 || removed > 0) =>
            {
                let (adds, edits) = sent(&input["facts"], &pass.edit_drops);
                pass.facts.extend(adds);
                pass.edits.extend(edits);
            }
            _ => {}
        }
    }
    Ok(pass)
}

/// The adds and the edits in the fact list of a write input.
///
/// An add carries no verb. A replace or a remove carries its verb and its
/// target. An edit gets the reason in `drops` that names its verb and the
/// hash of its target.
fn sent(facts: &serde_json::Value, drops: &[String]) -> (Vec<String>, Vec<Edit>) {
    let mut adds = Vec::new();
    let mut edits = Vec::new();
    for fact in facts.as_array().into_iter().flatten() {
        let entry = fact["entry"].as_str().unwrap_or_default();
        let verb = match fact["verb"].as_str() {
            None | Some("add") => {
                adds.push(entry.to_owned());
                continue;
            }
            Some("replace") => Verb::Replace,
            Some("remove") => Verb::Remove,
            Some(_) => continue,
        };
        let Some(target) = fact["target"].as_str() else {
            continue;
        };
        let prefix = format!("{} of fact {} dropped", verb.as_str(), fact_hash(target));
        let dropped = drops
            .iter()
            .find(|why| why.starts_with(&prefix))
            .map(|why| {
                why[prefix.len()..]
                    .trim_start_matches(':')
                    .trim()
                    .to_owned()
            });
        edits.push(Edit {
            verb,
            target: target.to_owned(),
            entry: entry.to_owned(),
            dropped,
        });
    }
    (adds, edits)
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
        let _ = write!(line, ", started {}", started.strftime("%Y-%m-%dT%H:%M:%SZ"));
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
    if pass.removed > 0 {
        let _ = write!(
            line,
            " Its removes took {} out of the block.",
            count(pass.removed, "fact")
        );
    }
    if !pass.edit_drops.is_empty() {
        let _ = write!(
            line,
            " The write dropped {}",
            count(pass.edit_drops.len() as u64, "edit")
        );
        // A drop that matches a listed edit shows beside it. Any other
        // drop shows here, so no reason is lost.
        let unmatched: Vec<String> = pass
            .edit_drops
            .iter()
            .filter(|why| !pass.edits.iter().any(|edit| matches(edit, why)))
            .map(|why| one_line(why))
            .collect();
        if !unmatched.is_empty() {
            let _ = write!(line, " ({})", unmatched.join("; "));
        }
        line.push_str(
            ", and a dropped edit consumes its source episodes, so no pass sends it again.",
        );
    }
    line
}

/// The drop reason `why` names `edit`.
fn matches(edit: &Edit, why: &str) -> bool {
    edit.dropped.is_some()
        && why.starts_with(&format!(
            "{} of fact {} dropped",
            edit.verb.as_str(),
            fact_hash(&edit.target)
        ))
}

/// One line on what `edit` does, and why the write dropped it.
fn edit_line(edit: &Edit) -> String {
    let mut line = match edit.verb {
        Verb::Replace => format!(
            "- replace \"{}\" with \"{}\"",
            one_line(&edit.target),
            one_line(&edit.entry)
        ),
        Verb::Remove => format!("- remove \"{}\"", one_line(&edit.target)),
    };
    if let Some(why) = &edit.dropped {
        let _ = write!(line, " (dropped: {})", one_line(why));
    }
    line
}

/// `n` of `noun` for each part that is not 0, such as "2 facts and 1
/// remove".
fn tally(parts: &[(u64, &str)]) -> String {
    let named: Vec<String> = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, noun)| count(*n, noun))
        .collect();
    match named.as_slice() {
        [] => count(0, "fact"),
        [one] => one.clone(),
        [head @ .., last] => format!("{} and {last}", head.join(", ")),
    }
}

/// The prompt of the reflection on `passes`, or `None` when they sent no
/// fact and no edit, so there is nothing to reflect on.
#[must_use]
pub fn prompt(passes: &[Pass]) -> Option<String> {
    let facts: Vec<&String> = passes.iter().flat_map(|pass| &pass.facts).collect();
    let edits: Vec<&Edit> = passes.iter().flat_map(|pass| &pass.edits).collect();
    if facts.is_empty() && edits.is_empty() {
        return None;
    }
    let episodes: u64 = passes.iter().map(|pass| pass.episodes).sum();
    let of = |verb: Verb| edits.iter().filter(|edit| edit.verb == verb).count() as u64;
    // The count is what the passes sent. The store skips a fact that the
    // block holds already, so the prompt never says the block changed.
    let mut text = format!(
        "Your memory was just consolidated. The passes read {} and sent {} to your \
         memory store:\n",
        count(episodes, "episode"),
        tally(&[
            (facts.len() as u64, "fact"),
            (of(Verb::Replace), "replace"),
            (of(Verb::Remove), "remove"),
        ])
    );
    // Every fact, whole: the reflection judges what the passes sent.
    for fact in &facts {
        text.push_str("- ");
        text.push_str(&one_line(fact));
        text.push('\n');
    }
    // Every edit, with the entry that it targets, so the reflection can
    // judge a remove as well as an add.
    for edit in &edits {
        text.push_str(&edit_line(edit));
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
            edits: Vec::new(),
            removed: 0,
            edit_drops: Vec::new(),
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
        let started = "2026-11-17T09:00:03.250Z".parse::<Timestamp>().unwrap();
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

    fn edit(verb: Verb, target: &str, entry: &str) -> Edit {
        Edit {
            verb,
            target: target.to_owned(),
            entry: entry.to_owned(),
            dropped: None,
        }
    }

    #[test]
    fn the_write_input_splits_into_adds_and_edits() {
        // The shape that engine ADR 0020 records for a write input.
        let input = serde_json::json!([
            {"entry": "mark likes tea", "sources": ["r:s0"]},
            {"entry": "mark drinks green tea", "sources": ["r:s1"],
             "verb": "replace", "target": "mark likes tea"},
            {"entry": "", "sources": [], "verb": "remove", "target": "mark lives\nin Ohio"},
        ]);
        let (adds, edits) = sent(&input, &[]);
        assert_eq!(adds, vec![String::from("mark likes tea")]);
        assert_eq!(
            edits,
            vec![
                edit(Verb::Replace, "mark likes tea", "mark drinks green tea"),
                edit(Verb::Remove, "mark lives\nin Ohio", ""),
            ]
        );
    }

    #[test]
    fn a_drop_reason_finds_its_edit_by_the_hash_of_the_target() {
        let input = serde_json::json!([
            {"entry": "", "sources": [], "verb": "remove", "target": "old"},
            {"entry": "", "sources": [], "verb": "remove", "target": "kept"},
            {"entry": "new", "sources": [], "verb": "replace", "target": "other"},
        ]);
        let drops = vec![
            format!(
                "remove of fact {} dropped: the entry has the labels [secret] that the pass lacks",
                fact_hash("old")
            ),
            // A replace reason never matches a remove of the same entry.
            format!(
                "replace of fact {} dropped: the entry changed during the pass",
                fact_hash("kept")
            ),
        ];
        let (_, edits) = sent(&input, &drops);
        assert_eq!(
            edits[0].dropped.as_deref(),
            Some("the entry has the labels [secret] that the pass lacks")
        );
        assert_eq!(edits[1].dropped, None);
        assert_eq!(edits[2].dropped, None);
    }

    #[test]
    fn an_unknown_verb_or_an_edit_with_no_target_is_left_out() {
        let input = serde_json::json!([
            {"entry": "x", "sources": [], "verb": "unknown"},
            {"entry": "", "sources": [], "verb": "remove"},
        ]);
        assert_eq!(sent(&input, &[]), (Vec::new(), Vec::new()));
    }

    #[test]
    fn the_prompt_lists_each_replace_and_remove_with_its_target() {
        let edits = Pass {
            edits: vec![
                edit(Verb::Replace, "mark likes\ntea", "mark likes green tea"),
                edit(Verb::Remove, "mark lives in Ohio", ""),
            ],
            removed: 1,
            ..pass(
                "pass-a",
                &["r:s0"],
                &["mark works on littleguy"],
                "completed",
            )
        };
        let text = prompt(&[edits]).unwrap();
        assert!(
            text.contains("sent 1 fact, 1 replace and 1 remove to your memory store:\n"),
            "{text}"
        );
        assert!(text.contains("- mark works on littleguy\n"));
        assert!(
            text.contains("- replace \"mark likes tea\" with \"mark likes green tea\"\n"),
            "one line: {text}"
        );
        assert!(text.contains("- remove \"mark lives in Ohio\"\n"), "{text}");
        assert!(
            text.contains(" Its removes took 1 fact out of the block."),
            "{text}"
        );
        assert!(text.contains("no longer holds an entry that a pass replaced or removed"));
    }

    #[test]
    fn a_pass_with_only_removes_gets_a_reflection() {
        let only = Pass {
            edits: vec![edit(Verb::Remove, "stale", "")],
            removed: 1,
            ..pass("pass-a", &["r:s0"], &[], "completed")
        };
        let text = prompt(&[only]).unwrap();
        assert!(text.contains("sent 1 remove to your memory store:\n- remove \"stale\"\n"));
    }

    #[test]
    fn a_dropped_edit_shows_its_reason_and_that_its_sources_are_consumed() {
        let why = format!(
            "remove of fact {} dropped: no single entry of the block matches it",
            fact_hash("gone")
        );
        let stray = String::from("replace of fact 0000000000000000 dropped: the entry changed");
        let dropped = Pass {
            edits: vec![Edit {
                dropped: Some(String::from("no single entry of the block matches it")),
                ..edit(Verb::Remove, "gone", "")
            }],
            edit_drops: vec![why, stray],
            ..pass("pass-a", &["r:s0"], &["mark likes tea"], "completed")
        };
        let text = prompt(&[dropped]).unwrap();
        assert!(
            text.contains("- remove \"gone\" (dropped: no single entry of the block matches it)\n"),
            "{text}"
        );
        // The matched reason shows beside its edit, and only the stray one
        // shows on the pass line.
        assert!(
            text.contains(
                " The write dropped 2 edits (replace of fact 0000000000000000 dropped: the \
                 entry changed), and a dropped edit consumes its source episodes, so no pass \
                 sends it again."
            ),
            "{text}"
        );
        assert!(!text.contains("Its removes took"));
    }

    #[test]
    fn the_tally_names_each_kind_that_is_not_zero() {
        assert_eq!(tally(&[(0, "fact"), (0, "remove")]), "0 facts");
        assert_eq!(
            tally(&[(2, "fact"), (0, "replace"), (3, "remove")]),
            "2 facts and 3 removes"
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
