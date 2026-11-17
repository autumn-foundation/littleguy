# ADR 0041: An answered idea leaves the open list

## Status

Accepted, 2026-11-17. It amends ADR 0020.

## Context

An idea is `open`, `promoted` (its goal was accepted) or `dropped`. Only
the owner drops an idea, on the Ideas tab. The assistant has no way to
retire an idea that its research answered and that needs no goal.

So answered ideas stay open. On 2026-11-17, 8 of the 22 open ideas ended
with a note such as "Closing this idea as answered", and none of those
notes changed anything. This costs three things:

- `list_plans` is the working set of each tick. It returns the full body
  and notes of each open idea, so every tick reads the answered ones again.
  A new idea hides among them.
- A note that says "closing" reads as done, and the next tick sees the same
  idea open. The notes and the state disagree.
- The heartbeat precheck runs a tick while any idea is open. An answered
  idea therefore costs a model call on every heartbeat.

A goal does not have this gap: `goal_progress` with `finished` marks a goal
done, on an unattended run.

## Decision

- **A new tool, `conclude_idea`.** It takes an open idea and a closing note,
  adds the note, and sets the status to `answered`. It is `Internal`, as
  `goal_progress` is, so a heartbeat or a reflection may call it.
- **It grants nothing.** ADR 0020 holds that no tool gives the assistant
  work that the owner did not accept. Concluding an idea removes work. It
  starts no goal and no check. So the rule still holds.
- **Nothing is lost.** The row and every note stay. Each read that filters
  on `open` is untouched: the listing, the cap of open ideas, the
  research and proposal checks, the heartbeat precheck. An answered idea
  takes no research note and no goal, and it frees a place under the cap.
- **The owner can undo it.** The Ideas tab lists the newest 20 answered
  ideas under **Answered**, the last concluded first. Each has Reopen and
  Drop. Reopen (`PlanDecision::ReopenIdea`) sets it back to `open`, within
  the cap of open ideas. Drop now takes an open or an answered idea.
- **`list_plans` names them, briefly.** Its reply gains `answered_ideas`:
  the id and title of the same 20, without body or notes. A tick can then
  keep the rule "do not file an idea that exists" without reading the
  answered ideas again.
- **The prompts say when.** The default heartbeat prompt and the reflection
  prompt tell the assistant to conclude an idea that is answered and needs
  no goal.
- **A retry is safe.** The closing note carries the key of the tool call.
  A retried call finds its note and succeeds again, with no second note.

## Consequences

- No schema change: `status` is free text, and the new value fits it.
- The open list means open again, and it shrinks as ideas are answered.
- A heartbeat whose only ideas are answered costs no model call.
- The owner does not have to decide on each answered idea. They can still
  read it, and reopen it when they disagree.
- An idea that the assistant concluded wrongly stays out of its view until
  the owner reopens it. The owner sees it on the Ideas tab, so the cost is
  one click.
- A promoted idea still leaves both lists. That is a separate question:
  how a goal shows the idea it came from.
