# ADR 0027: A council accepts for the owner

## Status

Accepted, 2026-11-15.

## Context

littleguy proposes and the owner accepts: goals (ADR 0020) and skills
(ADR 0024). The rule exists because a page that the assistant read must not
set its agenda or plant standing instructions.

mercer runs a table of littleguy daemons: a DM, the player characters and
the major NPCs. The owner wants each of them to plan and to grow skills, but
not to decide each proposal of each character by hand. Without a decision,
proposals pile up to their caps (20 goals, 20 drafts), and then the agent
can propose nothing more.

A character of mercer reads only what mercer tells it. littleguy itself reads
the web. So a substitute for the owner must hold for both: it decides fast
where the input is clean, and it leaves the owner the decisions where an
outside page could have written the proposal.

The engine keeps the flow labels of the conversation in `AgentState`
(`flow_labels`). A run that read a web page, or a memory note from one,
carries `untrusted_input` until its transcript resets. A tool call cannot see
the labels, but the daemon can read them after the op. The engine records
labels only when the harness has flow rules, and littleguy gives it the
rules only with its approval guard.

## Decision

- **A `[council]` section turns the council on**, per daemon. Without it,
  nothing changes: the owner decides everything.

  ```toml
  [council]
  accept = ["goals", "skills"]   # what the council decides; the default is both
  members = 3                    # independent model calls per proposal
  charter = "..."                # what this agent is for; the default is the system prompt
  ```

  Only `members = 0` is refused: a config refuses only what cannot work.
  It comes with the `council` feature, which needs `plans` and `skills`.

- **A tainted proposal waits for the owner.** When an op ends, the daemon
  marks each proposal that the op filed: tainted when the run carried
  `untrusted_input` at the end of the op. The mark is conservative: a taint
  that came after the proposal in the same op still counts, and a proposal
  older than the op, or a draft revised since its mark, gets a new one. The
  council never sees a tainted proposal. So a page that the assistant read
  can, at most, put a proposal in front of the owner, as today.
- **The council needs the labels.** With `[council]`, the harness gets the
  flow rules even without the approval guard, so the run records its taint.
  No tool can then complete a rule that asks, or the guard would be on, so no
  call changes. With `rule_of_two = false` there are no rules and no labels,
  so every proposal counts as tainted, and the log says why.

- **The council is model calls with no tools.** Each member reads the
  charter, the proposal and the open goals and skills, and answers JSON:
  `{"accept": true|false, "reason": "..."}`. The members do not see each
  other. More than half must accept, or the proposal is declined. An answer
  that does not parse is a vote to decline. The council runs on the model of
  the assistant.

- **The council decides on a tick**, one proposal at a time, oldest first.
  A decision is final for the council: an accepted goal becomes active, an
  accepted draft becomes a `SKILL.md` with `author: council`, and a declined
  one closes. An accepted goal gets a note with the reasons of the members
  who accepted. Each decision is one row in `council_log` in `memory.db`: what,
  the votes, each reason, and when. A proposal is decided once, also across a
  restart.

- **The owner still has the last word.** The Goals tab shows the note of the
  council, and the Skills tab shows `council` as the author. The owner can drop a goal or retire a skill that the
  council accepted, and decide any proposal that waits.

- **The council does not touch refine overlays.** An overlay changes the
  profile itself, so it stays with the owner. A later ADR can revisit that.

## Consequences

- A mercer character with no web tools never taints its proposals, so the
  council decides all of them, and the owner manages none.
- littleguy reads the web, so many of its proposals stay with the owner. That
  is the point.
- Each decision costs `members` model calls. They run outside the engine
  run, as the refine proposer does, so they are not in the cost ledger or in
  `model_usage`. `council_log` records each decision instead.
- A council can be wrong. A goal that it accepts runs on read-only ticks. A
  skill that it accepts is standing instructions, also on runs where a
  person is present and outward tools work. The taint rule is what bounds
  that: a skill that the council accepts came from a run with no outside
  text in it, so only the assistant and the council wrote it.
