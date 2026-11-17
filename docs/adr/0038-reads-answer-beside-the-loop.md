# ADR 0038: Reads answer beside the loop

## Status

Accepted, 2026-11-16.

## Context

The daemon handles one job at a time. A chat holds the loop until its
run ends, because the drive of the run borrows the runtime for all of
it, model calls and tool calls included. So every other request waited
behind the run.

A page that only reads waited too. While the assistant thought, the
chat page, the workspace and the manage page did not load. The browser
had locked only the Send button, so the pages looked frozen.

Everything that a read needs is durable or cheap to share:

- the littleguy stores and the overlay store are SQLite files;
- the config, the pairs and the skills folder live in the daemon, and
  they change only in a job;
- the state of the assistant is in the runtime database. The engine
  records a state snapshot at the start of each run and after each op.

## Decision

- **The daemon publishes a view after each job.** A `View` holds shared
  handles on the stores, the overlay store and the San Mateo service,
  plus the config, the pairs and the skills folder. `Reads` keeps the
  newest view.
- **A read answers from the view, never from the loop.** The web handle
  and the socket ask `Reads` first. The reads are:
  - History, Workspace and WorkspaceFile;
  - Approvals, Plans, Skills and Notes;
  - Overlays and OverlayShow;
  - Pairs and Connections.

  Every other request, each chat and each write, goes through the loop
  as before.
- **The state comes from the newest snapshot.** A reader opens `lg.db`
  read only, as the run log tools do. It takes the newest state snapshot
  of the running run of the assistant. While an op runs, that is the
  state before the op, which is what the runtime query gives. When the
  run has no snapshot that decodes, the read goes through the loop.
- **Only the owner reads beside the loop.** For any other uid, the view
  answers nothing, and the loop refuses the request as before.
- **The loop shares the code.** Each read handler is a function over
  the parts of the view. The loop calls the same function with its own
  parts.

## Consequences

- The pages load while the assistant thinks. `lg approvals`,
  `lg workspace` and the other reads of the CLI answer at once too.
- A read during an op shows the state before the op: the workspace and
  the history without the work in flight. The live chat streams that
  work.
- A read sees a change of the config, a pair or a skill once the job
  that made it ends.
- The snapshot read depends on the event format of the engine: a
  `SideEffectRecorded` event named `harvest.entity.state`, whose value
  holds the state. A test checks that the read agrees with the runtime
  query, so an engine upgrade that changes the format fails it.
