//! What each run of the assistant did, in memory: the names of its tool
//! calls and the text of its last model turn (ADR 0028).
//!
//! The tick log (ADR 0025) says that a tick ran. To say what it did, the
//! daemon needs the tools that the tick called and the answer that it
//! gave. Neither is in the engine state for one op: a tick is an op inside
//! the long run of the assistant. So the live sink feeds each piece of
//! model text here, by run id, and a wrapper around each tool records each
//! call. The daemon marks the run of each message of a person. When a wake
//! ends, the newest run that no person started is the tick.
//!
//! Nothing here is durable. After a restart, a tick that ran before it
//! says only that it ran.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use autumn_harvest_agent::{AgentError, BoxFuture, Tool, ToolContext, ToolEffect};

/// How many runs the traces keep. A tick is read within a second of its
/// end, so a few are plenty.
const KEPT: usize = 16;

/// The most characters of an answer that a trace keeps.
pub const MAX_ANSWER_CHARS: usize = 280;

/// What one run did.
#[derive(Debug, Default)]
struct RunTrace {
    run_id: String,
    /// A person started it.
    person: bool,
    tools: Vec<String>,
    /// The text of the last model turn.
    text: String,
}

/// What a tick did: its tool calls, in order, and its answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TickTrace {
    /// The run id of the tick.
    pub run_id: String,
    /// The name of each tool call.
    pub tools: Vec<String>,
    /// The answer, on one line, clipped to [`MAX_ANSWER_CHARS`].
    pub answer: String,
}

/// The traces of the newest runs.
#[derive(Debug, Default)]
pub struct Traces {
    runs: Mutex<VecDeque<RunTrace>>,
}

impl Traces {
    fn runs(&self) -> MutexGuard<'_, VecDeque<RunTrace>> {
        // A panic while the lock is held leaves a list that is still whole.
        self.runs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Calls `f` on the trace of `run_id`, made when it is new.
    fn with(&self, run_id: &str, f: impl FnOnce(&mut RunTrace)) {
        let mut runs = self.runs();
        if let Some(run) = runs.iter_mut().find(|run| run.run_id == run_id) {
            f(run);
            return;
        }
        if runs.len() >= KEPT {
            runs.pop_front();
        }
        let mut run = RunTrace {
            run_id: run_id.to_owned(),
            ..RunTrace::default()
        };
        f(&mut run);
        runs.push_back(run);
    }

    /// Marks `run_id` as the run of a message of a person.
    pub fn person(&self, run_id: &str) {
        self.with(run_id, |run| run.person = true);
    }

    /// A model turn of `run_id` starts: the text of an earlier turn goes.
    pub fn started(&self, run_id: &str) {
        self.with(run_id, |run| run.text.clear());
    }

    /// The model of `run_id` wrote `text`.
    pub fn wrote(&self, run_id: &str, text: &str) {
        self.with(run_id, |run| run.text.push_str(text));
    }

    /// `run_id` called the tool `name`.
    pub fn called(&self, run_id: &str, name: &str) {
        self.with(run_id, |run| run.tools.push(name.to_owned()));
    }

    /// What the newest run that no person started did, if one ran since the
    /// last call. Every such run leaves the traces, so a tick is told once.
    pub fn take_unprompted(&self) -> Option<TickTrace> {
        let run = {
            let mut runs = self.runs();
            let newest = runs.iter().rposition(|run| !run.person)?;
            let run = runs.remove(newest)?;
            runs.retain(|run| run.person);
            run
        };
        Some(TickTrace {
            answer: one_line(&run.text),
            run_id: run.run_id,
            tools: run.tools,
        })
    }
}

/// `text` on one line, clipped to [`MAX_ANSWER_CHARS`].
#[must_use]
pub fn one_line(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.char_indices().nth(MAX_ANSWER_CHARS) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text,
    }
}

/// A tool that records each of its calls in the traces, then runs.
#[derive(Debug)]
pub struct Traced {
    inner: Arc<dyn Tool>,
    traces: Arc<Traces>,
}

impl Traced {
    /// `inner`, with each call recorded in `traces`.
    #[must_use]
    pub fn wrap(inner: Arc<dyn Tool>, traces: &Arc<Traces>) -> Arc<dyn Tool> {
        Arc::new(Self {
            inner,
            traces: Arc::clone(traces),
        })
    }
}

impl Tool for Traced {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn description(&self) -> &str {
        self.inner.description()
    }

    fn input_schema(&self) -> serde_json::Value {
        self.inner.input_schema()
    }

    fn execute<'a>(
        &'a self,
        input: serde_json::Value,
        ctx: &'a ToolContext,
    ) -> BoxFuture<'a, Result<serde_json::Value, AgentError>> {
        self.traces.called(ctx.run_id.as_str(), self.inner.name());
        self.inner.execute(input, ctx)
    }

    fn effect(&self) -> ToolEffect {
        self.inner.effect()
    }

    fn labels(&self) -> &[String] {
        self.inner.labels()
    }

    fn interruptible(&self) -> bool {
        self.inner.interruptible()
    }

    fn uses_workspace(&self) -> bool {
        self.inner.uses_workspace()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_newest_run_of_no_person_is_the_tick_and_is_told_once() {
        let traces = Traces::default();
        traces.person("r:s1");
        traces.started("r:s1");
        traces.wrote("r:s1", "hello person");
        traces.called("r:s2", "list_plans");
        traces.started("r:s2");
        traces.wrote("r:s2", "Narration before a call.");
        traces.called("r:s2", "research_idea");
        traces.started("r:s2");
        traces.wrote("r:s2", "Idea 3\nhas   news.");
        let tick = traces.take_unprompted().unwrap();
        assert_eq!(tick.tools, ["list_plans", "research_idea"]);
        assert_eq!(
            tick.answer, "Idea 3 has news.",
            "the last turn, on one line"
        );
        assert_eq!(traces.take_unprompted(), None, "a tick is told once");
    }

    #[test]
    fn a_long_answer_is_clipped() {
        let traces = Traces::default();
        traces.wrote("t", &"word ".repeat(200));
        let tick = traces.take_unprompted().unwrap();
        assert_eq!(tick.answer.chars().count(), MAX_ANSWER_CHARS + 1);
        assert!(tick.answer.ends_with('…'));
    }
}
