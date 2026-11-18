# ADR 0020: Goals and ideas

## Status

Accepted, 2026-11-14. ADR 0041 adds a sixth tool, `conclude_idea`, and
the `answered` status of an idea.

## Context

littleguy runs all the time, but until now it only thinks when someone
writes to it. A heartbeat can wake it, but a heartbeat needs something to
do. The owner wants two kinds of things:

- **Ideas**: things to plan and research. They come up in a conversation,
  in the assistant's own research, and later out of memory consolidation.
- **Goals**: things to do. An idea that the research makes worth it
  becomes a goal. Some goals mean watching something over time, so they
  need a check every so often.

An unattended run (a heartbeat tick, a follow-up, a consolidation) is
read-only in the engine: it may call tools up to `Internal`, never `Write`
or `External`. Whatever the assistant does on its own must fit in that.
And what the assistant reads on its own can be untrusted, so what it reads
must not set its own agenda.

## Decision

- **The assistant proposes, the owner accepts.** A goal starts as
  `proposed` and does nothing: no checks, and its idea stays open. Only
  the owner accepts, declines or drops, on the Workspace page or over the
  socket (`Request::Decide`). This is the refine pattern (ADR 0013): the
  model proposes an overlay, and the owner activates it.
- **No tool can accept.** The assistant has five tools: `list_plans`,
  `add_idea`, `research_idea`, `propose_goal` and `goal_progress` (which
  can also mark a goal done). Each is `ReadOnly` or `Internal`, so each
  works on an unattended run, and none of them can give the assistant work
  that the owner did not accept. A page that a tick read can at most file
  an idea or a proposal, and the owner sees both.
- **The store holds the rules.** `littleguy_stores::plans` keeps ideas,
  goals and their notes in `memory.db`. It refuses empty and long text, a
  check under a minute, and a proposal from an idea that is not open. It
  caps open ideas (100), proposed goals (20), active goals (50) and notes
  per item (50), so a run that loops stops at a cap.
- **A retried call adds one row.** Each insert carries the key of its tool
  call, the run id and the provider call id. A retry has the same key.
- **`list_plans` is untrusted.** An idea or a note can hold what a
  research read, so the listing carries the `untrusted_input` label.
- **The Workspace page shows them.** Goals and ideas sit above the files.
  A proposed goal says it waits for the owner, with Accept and Decline.
- `[tools] plans = true` turns them on. It is on by default.

## Consequences

- With `plans` on, the daemon opens `memory.db` even with memory off.
- The next steps build on `due_goals`: a heartbeat whose precheck skips a
  tick with no model call when no goal is due, and a pass after memory
  consolidation that files ideas from what the consolidation learned. Both
  run unattended, and both fit inside the tools above.
- A conversation with the owner adds a click: the assistant proposes, and
  the owner accepts on the page. This is the price of keeping one rule for
  every run.
