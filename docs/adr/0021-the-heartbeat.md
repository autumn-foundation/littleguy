# ADR 0021: The heartbeat

## Status

Accepted, 2026-11-14. Amended 2026-11-23: a tick may keep its progress
notes (ADR 0043).

## Context

Goals and ideas (ADR 0020) give the assistant work that it can do on its
own. Something must wake it to do that work. The engine has a heartbeat
for this: a tick is an unattended, read-only op that the engine books on a
timer. A tick can ask a cheap precheck first, and an answer of
`HEARTBEAT_OK` keeps it silent.

Three facts shape the design:

- **The engine keeps one heartbeat timer per agent**, and it runs on an
  interval only. The morning brief (ADR 0006) already uses that timer: the
  daemon aims it at each brief slot with `Control::SetHeartbeat`.
- `Control::SetHeartbeat` changes a running agent. A profile setting would
  not reach the running assistant (autumn-harvest #2658).
- A tick that speaks stays in the transcript with its prompt. The web chat
  would show the prompt as a message from the owner.

## Decision

- **One wake plan for both.** `crate::wake::Wake` plans the brief slots and
  the heartbeat, and aims the one engine timer at the earliest wake. A
  brief slot wins a tie. After each tick, the daemon arms the next wake.
  The old brief calendar becomes the brief half of this plan.
- **A `[heartbeat]` section**: `every_minutes` (60 by default), `quiet`
  hours such as `"22:00-08:00"` in `timezone` (the memory time zone by
  default), `research` (true), `deliver` and `prompt`. A beat counts from
  its last arm. A beat inside the quiet hours moves to their end, so no
  tick is booked at night. The quiet window is two daily cron times in the
  time zone, through the engine's cron code, so DST needs no code here.
- **The precheck spends no model call on an idle tick.** Each heartbeat
  arm asks the precheck (`crate::wake::Due`). It runs the tick when a goal
  is due, or, with `research`, when an idea is open. A brief arm never asks
  it, so a brief always runs. With goals and ideas off, every heartbeat
  runs.
- **Ticks carry a mark.** A brief prompt starts with `[littleguy brief]`,
  a heartbeat prompt with `[littleguy heartbeat]`. The history hides the
  marked prompt and shows the answer after it as `Speaker::Unprompted`.
  The web chat labels it "littleguy, on its own".
- **`deliver = "web"`** is the one delivery for now: a heartbeat that has
  news speaks in the web chat, where it waits for the owner. The brief
  delivery sends only a tick that it armed as a brief (a shared flag), so
  a heartbeat never goes out as a brief DM. Other values fail at load with
  a message that names `web`.
- **A plan that goes away stops the timer.** With no brief and no
  heartbeat, the daemon checks once, at start and after a reload, whether
  the engine timer is still booked, and sends `HeartbeatSetting::Off`.
  Before this, removing `[brief]` left the last armed tick re-booking
  itself at the old interval.
- `[heartbeat]` is a live section: a reload applies it at once.

## Consequences

- A heartbeat every hour with nothing to do costs one recorded tick and no
  tokens. A heartbeat with open ideas costs one model turn per tick; set
  `research = false` to wake only for due goals.
- Discord and Slack delivery for the heartbeat is a later change. It needs
  the profile `deliver` flag, which a running assistant gets only with
  autumn-harvest #2658, so it waits for that.
- A tick has no model of its own yet. `AgentHeartbeat::model` can name a
  cheaper harness model once the harness holds more than one.
- A tick may keep its progress notes, and it stays read-only (amended
  2026-11-23, ADR 0043). With a `[compaction]` section, a long tick saves
  its notes and resets its window, as an attended op does. The flag that
  allows it, `allow_memory_writes`, is not `allow_actions`. A tick still
  refuses each tool that writes or acts outside.
