# ADR 0033: Code runs unasked until outside text arrives

## Status

Accepted, 2026-11-15. ADR 0036 amends the mark of outside text.

## Context

Each `run_code` call waits for the owner (ADR 0011). In the container of
`compose.yaml`, the code runs in a new user namespace with no network, and
the container is the boundary of the code. The owner approves call after
call for one task, often four or more, and reads code that the assistant
wrote from the owner's own words.

The approval exists for one danger: a page, a mail or another outside text
steers the model, and the model writes code that does what the text wants.
Code that only the owner's words led to carries no such steer.

The engine keeps the flow labels of the run (`AgentState.flow_labels`). A
run that read outside text carries `untrusted_input` until its transcript
resets. The council already reads them to keep a tainted proposal for the
owner (ADR 0027).

An engine flow rule cannot say "code after outside text". A rule trips
on any call that completes its label set, so a rule on `untrusted_input` and
code would also hold a page read after code. And the policy cannot see the
labels of the run.

## Decision

- **`[sandbox] approve`** takes `"always"`, the default, or
  `"untrusted"`.

  ```toml
  [sandbox]
  backend = "process"
  approve = "untrusted"
  ```

- **With `"untrusted"`, a call runs at once while the conversation holds
  only the owner's words.** Once it holds untrusted input, each later call
  waits, as with `"always"`. An approval releases one call, not the rest.
- **The gate keeps the mark.** The policy cannot read the labels of the
  run. So the approval gate marks the conversation when it lets a call with
  `untrusted_input` run, and the daemon sets the mark from the engine's
  labels at the start of each op. The mark holds within an op, across ops
  and across a restart, and it clears only when the transcript resets.
- **No labels, no trust.** The engine keeps labels only with flow rules.
  With `rule_of_two = false` and no other rule, nothing says that the
  conversation is clean, so each call waits.
- **Code in a run cannot act as the owner** (ADR 0032). That is why this
  can be safe: code that runs unasked cannot approve its own next call or
  rewrite the config.

## Consequences

- A task with code and no outside text needs no approval at all.
- A conversation that read one web page asks for each call until its
  transcript resets. That is the point.
- Code that runs unasked can still read and write `lg.db`, `memory.db` and
  the skills folder (ADR 0032, autumn-harvest#2697). The bound is that only
  the owner's words led to it.
- The mark is only as good as the labels. `web_fetch`, `web_search`, the
  mail and calendar reads, `api_get`, `read_note`, `list_notes` and the
  self-debug reads carry `untrusted_input`. An MCP tool carries only the
  labels that its `[[mcp.server]]` names, and the default is none, so a
  server that brings outside text needs `labels = ["untrusted_input"]`.
- `read_file`, `grep_files` and `search_notes` carry no label. A file that
  the assistant wrote from a page in an earlier conversation does not mark
  a new one when it reads the file again. Notes do, and the note store is
  the place for text from outside (ADR 0029).
