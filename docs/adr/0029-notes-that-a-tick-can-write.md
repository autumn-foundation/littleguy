# ADR 0029: Notes that a tick can write

## Status

Accepted, 2026-11-15.

## Context

littleguy keeps notes as files in its workspace, such as
`notes/ui-punchlist.md`. A tick runs read-only, and a read-only run never
saves the workspace (autumn-harvest-agent 0.10: `keep_files =
!request.read_only`). So a note that a tick learns is wrong stays wrong
until a person talks to the assistant. `write_file` used to answer success
on a tick and lose the write. It is now refused there (PR #79), and the
engine side is autumn-harvest#2692.

The assistant's own bookkeeping already persists on a tick: `add_idea` and
the other plan tools are `Internal`, and they write to `memory.db`
themselves, not through the workspace.

## Decision

- **Notes are rows in `memory.db`**, beside the ideas and goals (schema 9):
  a name, the text, and when it last changed. A name is one line of at most
  120 characters. A note holds at most 100,000 characters, and the store at
  most 1000 notes. Those are bounds on the agent, not on the owner.
- **Four tools.** `write_note` (replaces a note of the same name) and
  `delete_note` are `Internal`, so a tick may call them. `read_note` and
  `list_notes` only read. `list_notes` with a query ranks the notes as
  `search_notes` ranks the workspace files: the same `rank`, shared from
  `littleguy-tools`. A note can hold what a page said, so `read_note` and
  `list_notes` carry `untrusted_input`, as `list_plans` does.
- **The owner reads them** on a Notes tab of the Workspace page, as
  markdown. littleguy writes them.
- **No automatic move.** The notes in the workspace stay where they are.
  littleguy can move one with `read_file` and `write_note` when a person is
  present, so the move is in the conversation.
- `[tools] files` switches the note tools too, and they need memory on:
  notes are memory, and a daemon with memory off opens no memory file. The
  `notes` feature gates them, on by default. It needs `search-notes`, for
  the ranking.

## Consequences

- A tick can fix a note that it learns is wrong.
- Notes and workspace files are two places. The tool descriptions say that
  notes are for what the assistant learns, and they work on a tick.
