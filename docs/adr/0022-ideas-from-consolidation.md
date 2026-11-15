# ADR 0022: Ideas from memory consolidation

## Status

Accepted, 2026-11-14.

## Context

Each night a consolidation pass turns the episodes of the day into facts
in the assistant's notes (ADR 0005). The owner wants ideas (ADR 0020) to
come from that too, not only from a conversation: the pass is when the
assistant has just learned what mattered in a day.

The pass itself is an engine workflow with a fixed prompt. It writes
facts, and it has no tools. So it cannot file an idea. The assistant can,
on a run of its own.

## Decision

- **A reflection follows each pass.** On its tick, at most once a minute,
  the daemon lists the `agent_consolidate` runs that completed in the
  last two days. A pass with no reflection yet plans one: a one-shot wake
  in the wake plan (ADR 0021), at the engine minimum from now. Several new
  passes plan one reflection.
- **A pass counts once.** The key `reflect:<run id>` goes into
  `memory.db` (the `sent_reports` table) before the wake is planned, so a
  restart never reflects twice. A firing with no new episode starts no
  pass, so a quiet day costs nothing.
- **The reflection is a tick.** Its prompt starts with
  `[littleguy reflection]`. It reads the notes and `list_plans`, files new
  ideas with `add_idea`, adds to related ideas with `research_idea`, and
  proposes a goal only when an idea is clearly worth doing. It is
  read-only like every tick, and every tool it needs is `Internal` at
  most. It asks no precheck: it runs because a pass just ran. Its answer is
  `HEARTBEAT_OK` unless the owner should know something, and news shows in
  the web chat as "littleguy, on its own".
- **The timer stops after a one-shot wake.** With no brief and no
  heartbeat, the plan exists only for the reflection. Once its tick ran,
  the plan sends `HeartbeatSetting::Off`, so the engine does not book the
  same tick again. A reflection that waits for its arm survives a config
  reload.
- `[memory] reflect = true` turns it on. It is on by default, and it needs
  memory on and `[tools] plans` on.

## Consequences

- An idea from a reflection is as safe as an idea from a heartbeat: it can
  only file and note, and the owner accepts every goal.
- A pass older than two days when the daemon starts gets no reflection.
- The listing does not filter by agent. littleguy runs one assistant per
  daemon; a daemon with more agents would need the workflow id.
