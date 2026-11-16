# ADR 0034: No arbitrary caps on the work of the model

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

The same kind of guess bounded other work too:

- Each message stopped after 8 tool rounds (`max_steps`).
- The engine cut each tool result to 8000 characters. It did that after
  the tool cut its own result by a setting of the owner, such as
  `fetch_max_chars = 20000`. So the setting did not hold.
- Each HTTP request to a model failed after 10 minutes, below the
  13-minute budget of the engine for one model call.

Each of these drops work that the model did or would do. The harness
already bounds what must be bounded: the engine times out a model call,
the store bounds the size of one activity result, and each tool bounds
its own output.

## Decision

- **A pass goes with no cap of its own.** The limit of the model applies.
  `[memory] pass_tokens` sets a cap when the owner wants one. Only 0 is
  refused.
- **A model wrapper applies it** (`memory::PassTokens`). The daemon wraps
  the model of the harness at each config load. A request is a pass when
  its system prompt is `consolidate::PASS_SYSTEM`, its cap is
  `consolidate::PASS_OUTPUT_TOKENS` and it offers no tools. The wrapper
  sets the cap of such a request and sends every other request
  unchanged. Both constants are public in the engine, so a change there
  shows up as a pass that the wrapper no longer knows, not as a wrong
  request. The wrapper passes the model id and the cost through.
- **An op has no bound on its tool rounds.** `max_steps` is unset by
  default, and the daemon gives the engine `u32::MAX`. An owner who wants
  a bound sets one.
- **The engine cuts no tool result.** The harness gets a tool output
  limit of `usize::MAX`. Each tool cuts its own result.
- **A model request has no time bound of its own.** The HTTP client keeps
  only a connect timeout. The engine bounds each model call.

## Consequences

- A pass on a model that reasons gets room for its reasoning and its
  reply.
- A message runs until the model answers. A model that loops costs model
  calls until the owner stops it or sets `max_steps`.
- A long tool result goes to the model whole, and it costs input tokens
  for as long as it stays in the window.
- The engine still bounds one model call at 13 minutes, under the fixed
  15-minute bound of its model-turn activity. That is an engine matter.
- The pass wrapper is a workaround. Remove it when the engine takes the
  cap as a setting, and map `pass_tokens` to that setting.
- The Claude model sends a fixed cap of 16 000 tokens when a request names
  none, because the Messages API needs one. A later change asks the API
  for the cap of the model instead.
