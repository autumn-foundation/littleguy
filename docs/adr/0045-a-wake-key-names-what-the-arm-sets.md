# ADR 0045: A wake key names what the arm sets

## Status

Accepted, 2026-11-23. It changes how ADR 0021 keys an arm of the engine
timer, and how ADR 0022 bounds a reflection.

## Context

The engine keeps one heartbeat timer per agent. The daemon aims it at the
next wake with one `Control::SetHeartbeat` op, and the op key is the
idempotency key. The engine drops an op whose key it has seen.

Each key held the time of the wake only: `heartbeat:{at}`, `brief:{at}`,
`followup:{at}`, `reflect:{pass}`. `arm_wake` sent the op and discarded
`signal_delivered`. The plan marked the wake armed in either case.

That dropped a real arm every night. The nightly consolidation ends in
quiet hours, while the next heartbeat is armed at the end of the window.
The reflection arms 60 seconds on top of it. After the reflection tick,
the next arm aims at the same heartbeat, under the key of its first arm.
The engine dropped it, and the 60-second reflection setting stayed. Until
the window ended, the engine ran the reflection prompt every minute. The
plan waited for a wake in the future, so no tick log row named the
repeats.

A fresh key per retry is no fix. A retry of an arm then goes out again,
and the engine books the tick again from now. Each retry must keep its
key.

## Decision

1. **The key names what the arm sets.** It is the kind, the time of the
   wake and a digest of the setting, less its interval:
   `heartbeat:{at}#{digest}`. The interval follows from the time of the
   wake and the time of the arm, so the key leaves it out. A retry keeps
   its key, also across a restart. Two arms at one time that set
   different prompts or flags get two keys. A reflection key also names
   its pass: `reflect:{pass}@{at}#{digest}`.
2. **A dropped arm is checked.** On `signal_delivered: false`, the daemon
   reads the setting that the engine holds. When it holds what the arm
   sets (prompt and precheck, since the engine narrows the rest), the
   drop is a retry, and the arm stands. Otherwise a later arm replaced
   it. The daemon sends the arm again under `{key}~{digest of the held
   setting}`. That key is stable, so a retry of the re-arm is dropped as
   it should be. A second drop logs an error and clears the plan's arm,
   so the next tick arms again. A drop is never reported as an arm.
3. **A reflection runs only while its pass is pending.** The reflection
   arm asks the precheck. `Wake::reflect` sets the pass in a shared
   `PendingPass`. The precheck takes it, and runs the tick only when it
   took a pass. The daemon also clears it when it sees the reflection
   tick ran. So a timer that keeps a reflection setting by mistake costs
   one model call per pass at most, whatever the arming does. The daemon
   installs the precheck whenever the `reflection` or the `heartbeat`
   feature is on.

## Consequences

- The first arm after the upgrade has a new key, so the engine applies it
  once more. It aims at the same wake, so no tick runs twice.
- A drop costs one state read, which replays the run. Drops are rare: a
  restart before a booked wake, or an arm that a later one replaced.
- A reflection that fails after its precheck is not retried. A failed
  tick was not retried before either.
- `crate::fnv` holds the FNV-1a hash that the camel ids and the wake keys
  share. Its output must never change.
