# ADR 0030: A reflection hears what the pass did

## Status

Accepted, 2026-11-15. It amends ADR 0022. ADR 0034 raises the output cap
of a pass.

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
- **`[memory] pass_episodes` sets the batch of a pass**, 8 by default
  instead of the engine's 20, so a reply fits the cap. Only 0 is refused.

## Consequences

- A reflection goes straight to what the pass learned.
- A model that cannot fit 8 episodes in 2048 tokens still fails. The owner
  can lower `pass_episodes`. The cap itself and the report of a cut reply as
  `invalid_reply` are engine matters.
