# ADR 0024: Skills as SKILL.md files

## Status

Accepted, 2026-11-15.

## Context

The owner wants littleguy to have skills: know-how that it loads when a
task needs it, not in every prompt. The owner wants them in the format that
Claude Code and Codex use, a folder per skill with a `SKILL.md`, and wants
littleguy to write skills for itself.

autumn-harvest has skills (issue #2373 there). A skill has a name, a
one-line summary and a body. The system prompt lists each name and summary
under `## Skills`, and the `load_skill` tool returns a body. A profile and a
profile overlay can carry skills. A running assistant reads its profile
once per run (autumn-harvest #2658), but it reads the active overlay at
each op. The refine loop (ADR 0013) keeps that overlay, and its proposer
may also write skills.

A skill is standing instructions to the assistant. A page that the
assistant read must not be able to plant one.

## Decision

- **The skills folder is the one source.** Each skill is
  `<dir>/<name>/SKILL.md`: frontmatter with `name` and `description`
  (and an optional `author`), then the body in markdown. `dir` is
  `[skills] dir`, or `skills/` next to the config file. The frontmatter
  reader takes plain, quoted and block (`>`, `|`) values of those keys and
  skips every other key, so a skill from another tool loads. littleguy adds
  no YAML dependency for it.
- **The folder reaches the running assistant through the overlay.** At
  start, after a reload, and whenever the folder changes (a stamp of each
  `SKILL.md`, checked on each tick), the daemon stores a new overlay version
  with the folder's skills and every other part kept, and activates it.
  Nothing changes when the skills are already active. So skills need a
  `[refine]` section; without one, the listing is off and an action fails.
- **The engine's limits hold.** A file that the engine refuses (a bad
  name, a description that is not one line of at most 200 characters, a
  body over 32 KiB, more than 32 skills) does not load. The Skills tab and
  the log name it and the reason. The other skills still load.
- **littleguy drafts, the owner accepts.** `propose_skill` (`Internal`, so
  a heartbeat or a reflection may call it) stores a draft in `memory.db`.
  The owner accepts it on the Skills tab of the Workspace page, which
  writes the `SKILL.md` with `author: littleguy`, or declines it. A draft
  with the name of a skill revises it. At most 20 drafts wait, a bound on
  the agent, not on the owner. No tool can accept a draft.
- **Nothing is deleted.** Retiring a skill, or replacing one, moves its
  folder to `<dir>/.retired/<name>-<time>/`.
- **Refine cannot change skills.** A refine proposal keeps the active
  skills, whatever the proposer says. Skills change only through the
  folder, so the folder and the overlay never drift apart.
- The reflection after a consolidation pass (ADR 0022) asks the assistant
  to draft a skill when it found a way of doing something that it will
  need again.

## Consequences

- An owner can write or copy a `SKILL.md` into the folder, and the running
  assistant has it within a second, with no restart.
- Each change of the skills is an overlay version by `operator:lg-skills`,
  so `lg refine list` shows the history and a rollback works.
- Only `SKILL.md` loads. Other files in a skill folder (scripts, extra
  references) do not reach the assistant yet.
