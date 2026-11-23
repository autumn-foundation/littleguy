# ADR 0044: A research tool sweeps sources for the assistant

## Status

Accepted, 2026-11-23. It follows ADR 0034: the owner sets every bound,
and littleguy invents none.

## Context

The assistant runs one long agent run, and each tick adds to it. On a
tick it cannot read a source of 40 000 characters. `web_fetch` returns
pages of `fetch_max_chars`, and a second call to the same URL trips the
loop guard. Each page it reads stays in the window.

The engine has `agent_sweep` (autumn-harvest#2448). It runs one strategy
over a fixed set of inputs and returns one short row per item. Each item
is a full agent run with its own tools, and its transcript stays out of
the sweep (`SampleInput::brief`). A row keeps at most 512 bytes of the
answer. So 40 sources give about 20 KB of rows.

A tool in littleguy cannot wait on a sweep. A tool runs in an activity
with a 9-minute timeout. The runtime has one writer, and a retried
activity starts a second sweep after a crash. So autumn-harvest-agent 0.11
runs the call itself as a workflow tool, `sweep`, as it runs `delegate`
(autumn-harvest#2716). An operator gives a profile the `SweepTool`
setting. The model picks only the instruction and the sources.

Each item is an agent, so a sweep of 40 items costs 40 times the steps
of one item. The `SweepTool` setting has a required token ceiling, and
the engine has no default for it. ADR 0034 removes the caps that
littleguy invented, and the owner chose to keep this one out too
(2026-11-23). A ceiling is the owner's to set, not ours to require.

## Decision

- **A `[research]` section turns the tool on.** No bound has a default
  of its own:
  - `max_tokens_per_call` is the token ceiling of one call over every
    source. With no value, littleguy gives the engine the largest ceiling
    it takes (`u32::MAX`), so the call has no ceiling in practice.
  - `max_tokens_per_item` caps one source run. With no value, it has no
    cap of its own.
  - `max_steps_per_item` bounds the tool rounds of one source run. With
    no value, it has no bound, as for the assistant.
  - `concurrency` runs that many sources at the same time. It is 1 by
    default, as in the engine. The engine bound is 16.
  - `max_items` bounds the sources of one call. With no value, the engine
    bound of 1000 applies.
- **A `research` kind.** Each source runs as a read-only agent of that
  kind, with its own short system prompt. It has the tools of the
  assistant, and the engine runs only the read-only ones. It sees the
  workspace when the assistant does. A source run never offers `sweep`.
- **A sampling policy from the config.** `ConfigPolicies` gives the
  research kind single draws (no strategy, `max_draws = 1`), at most
  `concurrency` at the same time, `draw_effects = ["read_only"]`, the
  scorers that call no model, no judge model and no answer retry. Every
  other kind reads every default. The config is the only writer, so the
  store refuses a set and a reset.
- **The `answered` scorer.** A row is `ok` when its source run completed
  with a non-empty answer. It calls no model.
- **The result carries `untrusted_input`.** A source run reads outside
  text, so each result marks the run, as `web_fetch` does (ADR 0033, ADR
  0036).
- **The usage of an op counts the sweep.** The usage view now sums the
  ledger rows of every run newer than its mark, in any workflow, and the
  source runs are child runs with their own rows. `model_usage` already
  sums every ledger row of its window. The result of each call also names
  its ceiling and its spend.
- **A feature.** `research` gates the kind, the policy and the tool, on by
  default (ADR 0026). A `[research]` section without the feature fails
  `lg serve` at start and at a reload.

The example config shows the values that the owner picked for their own
setup (2026-11-23): a ceiling of 1 280 000 tokens, 32 000 tokens and 4
steps per source, and 8 sources at a time. So 40 sources fit in one
call, even when each one takes its full share. The values live in the
config, not in the code.

## Consequences

- The window of the assistant grows by the rows of a sweep, not by the
  sources.
- With no bound in the config, a sweep is bounded only by its sources: up
  to 1000 per call, each running until it answers. The limits of the
  provider and the bill are the only backstop. With a ceiling, one call
  spends up to it.
- A tick can call more than once, so a tick has no total bound of its
  own. `max_steps` of the assistant bounds the calls of one op when the
  owner sets it.
- The first wave of a sweep can take more than its fair share of the
  ceiling, because the engine splits what is left over the wave.
  `max_tokens_per_item` bounds that share.
- A crash resumes the sweep, because the engine records each source as a
  child run.
- The usage of an op now also counts a run of another workflow that
  starts during its drive, such as a consolidation pass. The drive spent
  it, so the count is true.
