# ADR 0013: The refine loop

## Status

Accepted, 2026-11-10. Amended 2026-11-25: an activation can name the
pointer it read, and it moves only when no move came in between.

## Context

The assistant runs on one profile: the system prompt of the config, its
tools and its bounds. An owner who sees the assistant answer badly edits
the config and restarts the daemon. The engine has a better tool for this.
A profile overlay is a versioned change to a profile: a new system prompt,
guardrails, tool guidance, skills or tighter bounds. A store keeps each
version, and a pointer names the active one. A durable agent reads the
active overlay at the start of each op, so a change needs no restart.

The engine also ships a refine workflow, `agent_refine`. It reads ended
runs, asks a proposer for candidate overlays, composes them, probes each
one by replay and picks a winner. littleguy cannot use it as it stands:

- Its replay probe reads `agent_loop` runs only. The assistant is the
  durable agent `assistant/<owner>`, and `run_start` refuses such a run
  as `SourceNotReplayable`.
- It needs an `OverlayComposer`, and the engine ships none.
- It registers only under the agent feature `eval`, which turns on
  `autumn-harvest/testing`. A daemon should not ship test support.

## Decision

- **The overlay store is real; the refine workflow waits.** A `[refine]`
  section gives the assistant harness a `SqliteOverlayStore` in
  `overlays.db`, next to `lg.db`. The harness itself validates each
  overlay, so a version above the ceilings of the refine policy is never
  stored or activated.
- **One proposer call, outside a workflow.** `lg refine propose` reads the
  last `messages` messages of the assistant transcript (40 by default;
  ADR 0034 removed a bound of 500 and of 64 KiB). It asks the engine `ModelProposer`, over
  the model of the assistant, for one overlay of the configured dimension.
  A reply that does not decode, or does not fit the ceilings, goes back
  with its reason, up to three attempts. `KEEP_BASE` stores nothing.
- **A person activates each version.** A proposal is stored as an inactive
  version by the operator `lg-refine`. Only `lg refine activate` moves the
  pointer, as the owner. `lg refine rollback` moves it back. There is no
  auto adoption, so the engine's daily cap and approval timeout do not
  apply yet.
- **An activation can name the pointer it read** (amended 2026-11-25).
  A program that owns the daemon, such as a game that refines its
  characters, reads `overlays` and then activates. A person can roll back
  or activate on the web pages in between, and the person's choice must
  win. So `overlays` also answers `head`, the `seq` of the last move of
  the pointer, and `overlay_activate` takes an optional
  `expected: {"active": A, "head": H}` copied from that answer. Both keys
  are required, and `null` means no active version or no move yet, as in
  `overlays`. The daemon calls the store's `activate_guarded`, which
  checks both in the swap transaction. Any move after the read refuses
  the activation, also a move away and back to the same version, because
  each move raises the head. A refusal is `overlay_moved` with the
  pointer as it is now, and nothing changes. A guarded move answers
  `overlay_active` with `guarded: true`. An older daemon drops
  `expected` and moves the pointer unchecked, so a caller that sent
  `expected` and gets no `guarded` knows that the move was not checked.
  With no `expected`, the daemon reads the active version itself just
  before the swap, as before, and a move after the caller read
  `overlays` goes unseen. `lg refine activate` and the web pages send no
  `expected`: a person acts on the page in front of them.
- **The owner can write a version too.** `lg refine put <file>` stores an
  overlay JSON file. `{}` is the config profile with no change, so an owner
  turns every overlay off by putting and activating it.
- **Every command goes through the daemon.** The daemon holds the store,
  so the socket checks that the owner sends each request, as it does for
  approvals.
- **The feature `refine` is on by default.** It adds no crate. The store
  opens only with a `[refine]` section, so a config without one runs as
  before, with no overlay read per op.

```mermaid
sequenceDiagram
  participant O as owner
  participant D as lg serve
  participant M as model
  participant S as overlays.db
  participant A as assistant
  O->>D: lg refine propose
  D->>A: read the transcript tail
  D->>M: ModelProposer request
  M-->>D: overlay JSON
  D->>S: put (validated, inactive)
  O->>D: lg refine activate N
  D->>S: move the pointer
  A->>S: read the active overlay at the next op
```

## Consequences

- A proposal is as good as one model call over the transcript. Nothing
  replays it before the owner sees it. The owner reads it with
  `lg refine show` and decides.
- The transcript can hold private data. The proposal goes to the model of
  the assistant, which saw the same data. An overlay can quote that data
  into the system prompt, and the owner sees it before activation.
- When the engine probes durable agent runs and ships a composer outside
  `eval`, `lg refine` can start the `agent_refine` workflow instead. The
  store, the commands and the activation stay as they are.
