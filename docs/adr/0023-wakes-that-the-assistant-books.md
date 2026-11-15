# ADR 0023: Wakes that the assistant books

## Status

Accepted, 2026-11-14. Closes #45.

## Context

The assistant needs to say "look at this again in 20 minutes": a deploy, a
reply, a page that changes. The engine has a tool for this,
`schedule_followup`, but the tool is offered only when the task has
`followups` set. `task.followups` comes from the agent profile, and a
running assistant reads its profile once per run (autumn-harvest #2658).
A profile overlay is read per op, but it has no `followups` field. So the
engine tool cannot reach the running assistant.

The wake plan (ADR 0021, ADR 0022) already aims the one engine timer at
the earliest of several wakes, through `Control::SetHeartbeat`, which does
reach a running agent.

## Decision

- **A `wake_me` tool books a wake** in `memory.db`: a time, from one minute
  to seven days ahead, and a text that says what to look at.
  `cancel_wake` cancels one. Both are `Internal`, so a heartbeat or a
  reflection can book a wake too. `list_plans` shows the pending wakes,
  with `due` set once their time has come.
- **The wake plan arms the next pending wake** as a fourth kind,
  `Followup`, at that time or at the engine minimum from now, whichever is
  later. A sooner wake takes the arm from a later heartbeat. Its tick asks
  no precheck.
- **The prompt of a wake is fixed.** It says that a booked wake is due and
  to call `list_plans`. The text of the wake stays in the store, so a text
  that the model wrote, maybe under the influence of a page, never becomes
  a user message. The engine keeps the prompt of a follow-up out of the
  user message for the same reason.
- **A tick ends the wakes before it.** When the daemon sees that an armed
  wake's tick ran, it marks every pending wake up to that time done, and
  plans the next one. Any tick after a wake's time sees it in `list_plans`
  and can act on it.
- **Bounds:** 20 pending wakes; and, as the engine caps a chain of
  follow-ups, 10 wakes booked since the last message of a person. A
  message from a person starts a new chain. A retried call books one wake.
- The tools share two things with the daemon: a flag that says the wakes
  changed, so the daemon reads the next one, and the time a person last
  wrote. The flag starts set, so the wakes of an earlier start come back.

## Consequences

- Follow-ups work on the running assistant now, with no engine change.
  #2658 is not needed for them any more.
- A wake runs read-only, as a heartbeat does. A follow-up that needs to act
  outside still needs the owner.
- Wakes need `[tools] plans`, because they live with the goals and ideas.
