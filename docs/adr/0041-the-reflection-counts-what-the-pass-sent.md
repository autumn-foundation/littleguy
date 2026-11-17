# ADR 0041: The reflection counts what the pass sent

## Status

Accepted, 2026-11-17. It amends ADR 0030.

## Context

Since ADR 0030 the reflection prompt said: "The pass read 8 episodes and
added 3 facts to your memory block:". littleguy could not find those
facts in its block, and spent three ticks on the gap.

The count was wrong. The engine counts a fact as written when the memory
store accepts the op. The `Add` arm of `apply_op` returns `Ok` and pushes
nothing when the block holds the same entry already. So a pass over
episodes that an earlier pass read sends the same facts again, reports
each one as written, and changes nothing. A refused fact, such as one that
does not fit a full block, also showed in the list as added.

littleguy also suspected a block frozen for the life of its run. It is
not. The engine reads the memory snapshot when each op starts a segment
(`open_segment` in autumn-harvest-agent 0.10). A reflection is a heartbeat
op that the daemon arms after the pass ended, so its snapshot holds every
fact that the store kept. A daemon test checks this: the system prompt of
the reflection op holds the fact of the pass.

A pass that ends without a mark, such as on `invalid_reply` or a spent
budget, leaves its episodes for the next pass, which can fail the same
way. The prompt never said so. When no pass sent a fact, only the tick log
spoke, and it did not say that the episodes wait.

## Decision

- **The prompt counts what each pass sent.** It says what the store does
  with a fact: it adds one that the block lacks, skips one that it holds
  already, and refuses one that does not fit. It never says the block
  changed.
- **The prompt says when the block was read.** The block in the prompt
  was read when the tick started, after the passes ended.
- **Each pass has its own lines:** its execution id, its start time, and
  the ids of the episodes it read. The daemon reads them in the walk of
  the pass history that it already does. The ids come from the mark in the
  input of `agent_consolidate_write`. A pass with no write takes them from
  the input of `agent_consolidate_turn`.
- **A refusal is named with its reason**, from the report of the pass.
- **A pass that ended without a mark says so.** Only `completed` and
  `nothing_to_do` mark. The prompt says the episodes stay unconsolidated
  and that the next pass reads them again. With `self-debug`, the tick log
  row of a skipped reflection says how many episodes wait.
- A pass that sent no fact still wakes no reflection (ADR 0030).

## Consequences

- The prompt states nothing that the assistant cannot check.
- The prompt still cannot say which sent facts are new. That needs the
  engine: `FactsWritten` would count the adds that changed the block apart
  from the duplicates that were there already. littleguy can ask for it
  upstream, or have its own `SqliteStores::apply_now` record when an `Add`
  left the blocks unchanged.
