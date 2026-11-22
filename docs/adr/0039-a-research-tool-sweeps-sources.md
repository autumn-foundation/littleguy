# ADR 0038: A research tool sweeps sources for the assistant

## Status

Proposed, 2026-11-17. The limits below wait for the owner. The tool
waits for an autumn-harvest release with the `sweep` workflow tool
(autumn-harvest#2716).

## Context

The assistant runs one long agent run, and each tick adds to it. On a
tick it cannot read a source of 40 000 characters. `web_fetch` returns
pages of `fetch_max_chars`, and a second call to the same URL trips the
loop guard. Each page it reads stays in the window.

The engine has `agent_sweep` (autumn-harvest#2448). It runs one strategy
over a fixed set of inputs and returns one short row per item. Each item
is a full read-only agent run with its own tools, and its transcript
stays out of the sweep (`SampleInput::brief`). A row keeps at most 512
bytes of the answer. So 40 sources give about 20 KB of rows.

Three facts bound the design:

- autumn-harvest-agent 0.10.0 has no `agent_sweep`. Only `trunk-dev` has
  it.
- A tool in littleguy cannot wait on a sweep. A tool runs in an activity
  with a 9-minute timeout. The runtime has one writer, and a retried
  activity starts a second sweep after a crash. So the engine runs the
  call itself, as it runs `delegate` (autumn-harvest#2716).
- Each item is an agent, so a sweep of 40 items costs 40 times the steps
  of one item. No sampling policy setting caps the draws of a sweep. Only
  `max_total_tokens` bounds its spend (DESIGN-2448, Known limits).

ADR 0034 removes the caps that littleguy invented. This cap is not one of
those. It is the only bound on a fan-out that the assistant starts
unattended, so the owner sets it.

## Decision

- **A `research` kind.** It has its own short system prompt and the
  read-only tools only: `web_fetch`, `web_search`, `read_file` and the
  note readers. Its profile sets `max_steps = 4` and `max_total_tokens =
  32000` for each item. An item fetches, fetches again or searches, and
  answers. The token cap stops one item that runs long, and the sweep goes
  on.
- **A sampling policy for the kind** (`[sampling.research]`): no strategy,
  so each item is a single draw. `max_draws = 1`,
  `max_concurrent_draws = 8`, `draw_effects = ["read_only"]`, and only the
  `answered` scorer. No judge model and no answer retry.
- **A ceiling on every call** (`[research] max_tokens_per_call = 640000`).
  The engine setting has no default, so no call goes out without it. Items
  that the ceiling does not reach come back `not_run`. The assistant can
  call again with the rest.
- **The tool is `sweep`.** It takes an instruction and a list of
  `{label, source}`. It answers with the joined answers, one row per label
  (`label`, `ok`, `status`, `tokens`) and the spend of the call. The reply
  names the spend, because the ledger rows of a sweep belong to its child
  runs.
- **The result carries `untrusted_input`.** An item reads outside text, so
  the run takes the mark as it does for `web_fetch` (ADR 0033, ADR 0036).
- **The usage view counts the sweep.** The engine adds the spend of the
  sweep tree to the report of the assistant run. `model_usage` also sums
  the ledger rows of the child runs of the assistant, so the owner sees
  the spend in one place.
- **A feature.** `research` gates the kind, the policy and the tool, on by
  default (ADR 0026). A `[research]` section without the feature fails
  `lg serve` at start.

## Consequences

- The window of the assistant grows by the rows of a sweep, not by the
  sources.
- One call can spend up to the ceiling. A tick can call more than once,
  so a tick has no total bound of its own. `max_steps` of the assistant
  bounds the calls of one op when the owner sets it.
- A crash resumes the sweep, because the engine records each item as a
  child run.
- littleguy adopts the tool at the next autumn-harvest release. Until
  then the kind, the policy and the config do nothing, so this ADR stays
  Proposed.
