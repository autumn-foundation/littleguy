# ADR 0026: Every capability is a feature

## Status

Accepted, 2026-11-15.

## Context

Each integration of littleguy is a cargo feature: a channel, a model, a
tool family that brings in a crate. The capabilities that came later have
no feature: goals and ideas (ADR 0020), the heartbeat (ADR 0021), the
reflection (ADR 0022), booked wakes (ADR 0023), skills (ADR 0024) and the
self-debug tools (ADR 0019, ADR 0025). A config switch turns some of them
off, but every build still compiles them. An owner who does not want a
part pays for it at each build.

## Decision

- **Each capability is a cargo feature, on by default.** A capability is a
  part that an owner can do without. Its feature gates its code, its
  tools, its store module and its tests.
- **A config switch stays.** It turns the part off in a build that has
  it. When the build lacks the part, a config section of it fails
  `lg serve` at start, as for an integration. A switch that is on by
  default only logs that the part is off, since the owner did not ask for
  it.
- **An owner request of a missing part answers, it does not fail.** A
  listing answers `Off` and an action answers `Error`, each naming the
  feature, as the empty states of PR #49 do. The pages show that.
- **The capabilities get their features one PR each.** `plans` comes
  first, because the heartbeat and the reflection build on it.

## Consequences

- `plans` gates the goal and idea tools, their fields in `list_plans`, the
  owner's goal and idea requests, the store module in `littleguy-stores`,
  and the heartbeat precheck. Without it every heartbeat runs, as with
  `plans = false` today, and no reflection runs.
- `heartbeat` gates the heartbeat part of the wake plan, its precheck and
  its verdict in the tick log. It needs `plans`, since the precheck reads
  the goals and ideas. A build without it refuses `[heartbeat]` at start
  and at a reload. The wake plan stays: the brief, the reflection and the
  booked wakes use it.
- `followups` gates `wake_me`, `cancel_wake`, the wakes in `list_plans`,
  the follow-up part of the wake plan and the `wakes` module of
  `littleguy-stores`. A tick of a build without it ends no booked wake.
- `reflection` gates the pass watch of the daemon and the reflection part
  of the wake plan. It needs `plans`, since a reflection files ideas.
  `reflect = true` under `[memory]` is the default, so a build without the
  feature only logs that the reflection is off.
- `skills` gates the skills folder and its sync into the overlay,
  `propose_skill`, the skill drafts in `list_plans`, the owner's skills
  requests and the `skill_drafts` module of `littleguy-stores`. It needs
  `refine`, since skills reach the assistant through the overlay. A build
  without it refuses `[skills] dir` at start. A reflection then does not
  ask for a skill draft. A build with none of `plans`, `followups` and
  `skills` has no `list_plans`.
- `[tools] plans` still switches the whole set of the assistant's own
  tools, also `wake_me`, `cancel_wake`, `propose_skill` and
  `recent_wakes`. Their own features split that switch.
- `cargo xtask features` builds each new feature alone, so a gate that
  leaks fails CI.
