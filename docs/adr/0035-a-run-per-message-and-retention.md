# ADR 0035: A run per message, and the retention of ended runs

## Status

Accepted, 2026-11-16. It amends ADR 0018.

## Context

autumn-harvest-agent 0.10 reads the profile of an agent once, at the
start of a run, and records the read. Replay reads the record, so a
restart keeps the old profile too (autumn-harvest #2658). The profile
holds `system`, `window` and `max_steps`.

The run of the assistant ends only when its history grows large: at
10,000 events, or at half of the 50 MiB byte cap. Each op writes a whole
snapshot of the state into the history, so a run reaches the byte mark
after some thousand events. Until then, no change to the config reaches
the assistant.

On the owner's daemon, ADR 0034 took the bound of 8 tool rounds off
`max_steps`, and the owner set `max_steps = 999` as well. Each message
still ran with 8 rounds, the bound that the run read at its start, and it
stopped with "I used every tool round that one message gets".

The engine fixes this with `Control::ReloadProfile` (autumn-harvest PR
#2665). No release holds it yet.

An ended run stays in the database with its history and its ledger rows.
The SQLite runtime deletes none by itself. It has a purge,
`purge_with(PurgePolicy)`, that an embedder calls.

## Decision

- **Each op ends its run.** The assistant profile sets
  `max_ops_per_run(1)`. After each op, the run ends at a checkpoint, and
  the next run starts with the state. The workflow and its id stay the
  same, and so does the conversation. Memory, notes and the workspace are
  in their own stores, and a new run does not touch them.
- **A start and a reload end the run once more.** The next run starts at
  once after a checkpoint, and it reads the profile then, before a later
  change. So after a start and after each reload, the daemon sends one op
  that changes nothing: `Control::Resume`. The assistant is never paused,
  so the op only ends the run. The run after it reads the new profile.
  With no run, the daemon sends nothing, and the first op reads the
  profile. While a call waits for a decision, the op of the run is not
  done, so the daemon sends the op after the decision.
- **The ended runs go after `[runs] keep_days`.** The default is 30 days,
  so the run log tools can still read a month of runs. Once a day, the
  daemon tick purges each run that ended before the window, with its
  history and its ledger rows. A purge does at most four batches of 256
  runs per tick, and the next tick goes on. Zero deletes each ended run at
  the next purge. A run that has not ended never goes. A purge that fails
  leaves the runs for the next tick and does not stop it.

## Consequences

- A change to `system`, `window`, `max_steps` or `tools` reaches the next
  message, also after a restart. `lg reload` says "from the next message".
- A run holds the history of one op, so a replay reads one op, not
  thousands.
- Each message writes the state once more, as the input of the next run.
  The state is about as large as the snapshot that each op writes already.
  It must fit the input cap of a workflow, 2 MiB. On the owner's daemon
  it is about 210 KB.
- The run that runs now recorded no `max_ops_per_run` at its start. It
  ends at its own size mark, and each run after it ends after one op.
- The purge deletes the ledger rows of a run with the run. `model_usage`
  then sees at most `keep_days` of spending.
- At the upgrade to an engine release with `Control::ReloadProfile`, the
  daemon sends that op instead of `Resume`.
