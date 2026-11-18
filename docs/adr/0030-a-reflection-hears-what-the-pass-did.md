# ADR 0030: A reflection hears what the pass did

## Status

Accepted, 2026-11-15. It amends ADR 0022. ADR 0034 raises the output cap
of a pass. Amended 2026-11-17: the prompt names each pass and its episodes.

## Context

After a consolidation pass, a reflection tick (ADR 0022) told the
assistant: "Your memory was just consolidated: new facts are in your notes."
littleguy saw its memory block unchanged after the pass, and suspected a
block frozen for the life of its run. That is not it: the engine reads the
memory snapshot once per segment, so each op sees the block as it is.

The run log of the owner's daemon showed the cause. The only pass read 20
episodes, its model reply used all 2048 output tokens, the cap that
autumn-harvest-agent 0.10 fixes for a pass (`PASS_OUTPUT_TOKENS`), and the
cut reply did not parse. The pass stopped with `invalid_reply`, wrote no
fact, and ended as completed. The daemon woke a reflection for it anyway,
and the prompt claimed facts that did not exist. A pass that writes nothing
also marks no episode as read, so the next pass reads the same 20 and fails
the same way.

## Decision

- **The daemon reads what each pass did.** From the runtime it reads the
  report of the pass (`ConsolidationReport`), and from its history the
  input of its fact write, which holds each entry as the memory block gets
  it.
- **A pass that wrote no fact wakes no reflection.** The daemon logs the
  stop of the pass, and with `self-debug` the tick log gets a row:
  "skipped: the consolidation pass wrote no facts (invalid_reply); no model
  call". The assistant sees it with `recent_wakes`.
- **The reflection prompt lists the facts.** "The pass read 8 episodes and
  added 3 facts to your memory block:", then each fact on one line, at most
  40, then what to do with them. The prompt no longer claims facts that it
  cannot name. The facts are already in the memory snapshot of the same
  run, with their flow labels, so the run is tainted by them either way.
- **The prompt names each pass** (idea #25, amended 2026-11-17). Each pass
  gets one line: its execution id, its start time, its stop, the ids of the
  episodes that it read, and whether it marked them. Only a `completed`
  pass marks. Any other stop leaves its episodes pending, and the line says
  that the next pass reads them again. A fact that the store refused keeps
  its source episodes pending, and the line says so. The episode ids come
  from the output of the pass read activity, `agent_consolidate_read`.
- **The prompt says that the block already holds the facts.** The engine
  reads the memory block once per segment, and each op is a new segment.
  The reflection starts after the pass, so its block already holds what the
  pass wrote. Without this note, littleguy read the new facts as old ones
  that the pass derived again, and suspected a pass stuck on one batch.
- **The prompt counts what the passes sent, not what changed** (goal #11,
  amended 2026-11-18). The `Add` arm of the engine's `apply_op` returns
  `Ok` and pushes nothing when the block holds the same entry already, and
  the engine still counts the fact as written. So a pass over episodes that
  an earlier pass read sends its facts again and changes nothing. The
  prompt says "sent N facts to your memory store", never "added". The note
  on the block says a fact is new unless the block held it before. A
  refused fact shows with its reason. With `self-debug`, the tick log row
  of a skipped reflection says how many episodes of an unmarked pass wait.
  Only the engine can count the duplicates: `FactsWritten` would need a
  field for the facts that the block held already.
- **`[memory] pass_episodes` sets the batch of a pass**, 8 by default
  instead of the engine's 20, so a reply fits the cap. Only 0 is refused.

## Consequences

- A reflection goes straight to what the pass learned.
- Two reflections that name the same episode ids under two pass ids show a
  pass that does not mark. Two different batches show a backlog that
  drains. The assistant can tell the two apart without reading code.
- A model that cannot fit 8 episodes in 2048 tokens still fails. The owner
  can lower `pass_episodes`. The cap itself and the report of a cut reply as
  `invalid_reply` are engine matters.
