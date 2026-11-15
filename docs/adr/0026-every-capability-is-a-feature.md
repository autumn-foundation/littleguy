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
- `[tools] plans` still switches the whole set of the assistant's own
  tools, also `wake_me`, `cancel_wake`, `propose_skill` and
  `recent_wakes`. Their own features split that switch.
- `cargo xtask features` builds each new feature alone, so a gate that
  leaks fails CI.
