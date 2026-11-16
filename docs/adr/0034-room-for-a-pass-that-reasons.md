# ADR 0034: Room for a pass that reasons

## Status

Accepted, 2026-11-16. It amends ADR 0030.

## Context

autumn-harvest-agent 0.10 sends the model call of a consolidation pass
with a fixed output cap of 2048 tokens (`consolidate::PASS_OUTPUT_TOKENS`).
No setting changes it (autumn-harvest #2695).

A model that reasons spends its hidden reasoning from the same cap. On the
owner's daemon, two nightly passes in a row used all 2048 tokens. The
input was small: 4422 tokens one night and 1509 the next. So a smaller
batch of episodes (ADR 0030) does not help. The reply was cut off, the
pass stopped with `invalid_reply`, it wrote no fact, and no reflection
followed. So the assistant proposed no goals and no skills.

Less reasoning is no fix either. The same model does no tool calls with
reasoning off, and a low effort can still fill 2048 tokens.

## Decision

- **`[memory] pass_tokens` sets the output cap of a pass**, 16 000 by
  default. Only 0 is refused.
- **A model wrapper applies it** (`memory::PassTokens`). The daemon wraps
  the model of the harness at each config load. A request is a pass when
  its system prompt is `consolidate::PASS_SYSTEM`, its cap is
  `consolidate::PASS_OUTPUT_TOKENS` and it offers no tools. The wrapper
  raises the cap of such a request and sends every other request
  unchanged. Both constants are public in the engine, so a change there
  shows up as a pass that the wrapper no longer knows, not as a wrong
  request.
- **The wrapper passes the model id and the cost through**, so the ledger
  and the response cache see the same model.

## Consequences

- A pass on a model that reasons gets room for its reasoning and its
  reply.
- A larger cap can cost more on a model that uses it. A pass runs once a
  night, so the cost stays small.
- The wrapper is a workaround. Remove it when the engine takes the cap as
  a setting, and map `pass_tokens` to that setting.
