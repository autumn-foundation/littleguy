# ADR 0028: The tick log says what a tick did

## Status

Accepted, 2026-11-15. It amends ADR 0025.

## Context

The tick log (ADR 0025) says that a tick ran, and when. littleguy asked to
know what a tick looked at and what it concluded: without that, it cannot
audit its own heartbeat, and it spent ticks on the question instead of on
the work. The run log cannot help: a tick is an op inside the long run of
the assistant, not an execution of its own.

The engine state holds the answer of the last op only, and no tool names.
A message of a person can run between a tick and the moment the daemon
writes its row.

## Decision

- **The daemon traces each run in memory.** The live sink records the
  text of the last model turn of each run, by run id. A wrapper around each
  tool records the name of each call, by the run id of the call. The run of
  each message of a person is marked, by the run id that #64 gives it.
- **A tick is the newest run that no person started.** When the daemon sees
  a wake end, it takes that run. When the run is the last segment of the
  state, its answer comes from the state, so a model that does not stream
  works too. Otherwise the streamed text stands.
- **The row says it in its outcome.** For example `ran; called list_plans,
  research_idea; answered: Idea 3 has news…`, or `ran; no tool calls;
  nothing to tell (HEARTBEAT_OK)`. The answer is on one line, clipped to 280
  characters. A skipped tick still reads "skipped by the precheck". No
  schema change.
- **The row holds no message text but the tick's own answer.** The prompt
  of the tick and the conversation stay out. A tick's answer can repeat what
  a page said, so `recent_wakes` now carries the `untrusted_input` label.
- It comes with the `self-debug` feature, as the tick log does.

## Consequences

- `recent_wakes` answers "what did my heartbeat find?".
- The trace is not durable. A tick that ended before a restart says only
  that it ran.
- A run that no person started and that is not a tick, such as a camel
  batch, could be taken for the tick that ends next. Camel batches are rare
  next to ticks, and the worst case is a wrong summary in a log row.
