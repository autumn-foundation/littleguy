# ADR 0025: The tick log

## Status

Accepted, 2026-11-15.

## Context

littleguy asked whether a wake that it booked had fired. Its run log could
not say. `recent_runs` lists workflow runs, and the assistant has one: the
`agent` run, which runs for as long as the agent lives. Every message,
heartbeat tick, brief, reflection and booked wake is an op inside that run.
`model_usage` counts the model calls of a tick, but not what started them.

The wake had fired: the cost ledger showed five model calls six seconds
after its time, and a restart came twenty minutes later. But the assistant
could not find that out, and a pending wake across a restart had no test.

## Decision

- **The daemon logs each tick that it sees end.** The wake plan (ADR 0021)
  already notices when an armed wake ran, to mark the booked wakes done. At
  that point the daemon writes one row to `tick_log` in `memory.db`: the
  kind (`heartbeat`, `brief`, `reflection` or `follow-up`), when it was
  due, when the daemon saw it end, and how it went. The log keeps the
  newest 500 rows.
- **A heartbeat row says what the precheck decided.** The precheck stores
  its verdict for the daemon. A skipped tick reads "skipped by the
  precheck: no goal due and no idea open; no model call". A tick that ran
  reads "ran". A tick that ended booked wakes says how many.
- **`recent_wakes`** (`ReadOnly`) lists the newest rows, newest first. A row
  holds no message text, so the tool carries no flow label.
- **A test restarts the daemon while a wake waits.** The wake comes back at
  its booked time and fires once.

## Consequences

- The assistant can answer "did my wake fire?" and "is my heartbeat
  running?" by itself.
- The "seen" time is when the daemon noticed the end of the tick, at its
  next arm, which is within a second of the end on a running daemon.
