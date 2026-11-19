# ADR 0043: Compaction through the context reset of the engine

## Status

Accepted, 2026-11-18.

## Context

An op runs until the model answers (ADR 0034). Each tool round adds its
call and its result to the window. A long op on many pages or files can
grow the window until the next request passes the request cap. The engine
then does not send it, and the op stops with `transcript_full`. The owner
reads "the conversation got too long", and the work of the op is lost.

autumn-harvest-agent 0.10 already has compaction: the context reset of
issue #2453 (`autumn_harvest_agent::context_reset`). littleguy did not use
it. It works so:

- A run with a `ContextReset` setting offers the model `save_progress`.
  The workflow handles the call itself, and the call is in the recorded
  model turn. So replay rebuilds the notes from history.
- Before each request, the loop measures it against the request cap. When
  a request reaches `reset_at_percent` of the cap, and a reset is left,
  the loop first sends a save turn: the window and a fixed notice. Only
  `save_progress` runs in that round.
- Then the loop resets the window. The new window holds the system
  prompt, the task, the follow-up bookings, the notes and the steers. A
  reset that frees no room does not run.
- The notes are bounded in bytes. They never go into a user message. The
  window shows them as the model's own `save_progress` call, with a fixed
  result that the engine controls.
- A durable agent keeps the notes in its state, with their flow labels,
  and shows them again in each op.

Two facts bind the setting:

- **The reset measures bytes against the request cap.** The cap is
  `max_request_bytes` of the profile, and the engine cap of 2 MiB without
  one. A model with a context window under 2 MiB of JSON fails at the
  provider before a reset at 80% of 2 MiB is due. So compaction needs the
  owner to set the cap under the window of the model.
- **A save turn needs room.** The engine sends the save turn only when the
  request, the notice and the bound on the notes fit under the cap.
  Without room it resets at once, and the model saves nothing. At
  `reset_at_percent = 100` that is always so.

## Decision

- **Compaction is a cargo feature, `compaction`, on by default** (ADR
  0026). It adds no crate. A build without it refuses `[compaction]` at
  start and at a reload.
- **The `[compaction]` section turns it on.** Without the section the
  assistant gets no `save_progress` tool, as before. The section maps to
  the `ContextReset` of the assistant profile:
  - `at_percent` is `reset_at_percent`. The default is 80. Only 1 to 100
    work.
  - `max_resets` is `max_resets`. It has no bound by default, as
    `max_steps` has none (ADR 0034). The engine runs a reset only when it
    frees room. 0 is refused: remove the section instead.
  - `notes_bytes` is `max_notes_bytes`. The default is the engine
    default, 8192. 0 is refused.
- **`max_request_bytes` sets the request cap.** It is a top-level key, not
  part of the feature, because the cap bounds every op. It also bounds a
  tool result in the engine, to about a sixth of the cap. With no value
  the engine cap applies. 0 is refused.
- **The config refuses a setting whose save turn never fits.** The kernel
  `compaction::save_turn_fits` in `littleguy-core` checks that the smallest
  request that makes a reset due, the notice and the notes stay within the
  cap. The notice is measured as the engine sends it.
  `formal/verus/src/compaction.rs` proves the kernel for every input, with
  no overflow.
- **Both keys shape the profile.** A reload applies them from the next
  message (ADR 0035).
- **`config_summary` shows both**, so the assistant can say whether
  compaction is on.

## Consequences

- A long op saves its notes, resets, and goes on. It no longer stops at
  the cap while resets are left.
- The progress notes outlive the op. Each later op shows them after its
  input, until the model saves new notes. They are not the notes of
  `write_note` (ADR 0029): they live in the agent state, not in
  `memory.db`.
- An unattended segment, such as a heartbeat tick or a booked wake, may
  not write memory. The engine gives such a segment no reset and no
  `save_progress`. It still reads the notes. A long tick can still stop
  at the cap.
- A reset drops the tool results of the op from the window. The model
  keeps only what it saved. A model that saves too little repeats work.
- The flow labels belong to the run, so a reset clears no taint. The
  notes carry the labels of the run that saved them.
- A lower `max_request_bytes` also cuts each tool result harder in the
  engine. An owner sets it to the window of the model, not lower.
