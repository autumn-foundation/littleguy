# ADR 0036: The mark of outside text belongs to the conversation

## Status

Accepted, 2026-11-16. It amends ADR 0033.

## Context

With `[sandbox] approve = "untrusted"`, a `run_code` call runs at once
while the conversation holds only the owner's words (ADR 0033). The daemon
took the mark from the flow labels of the engine (`AgentState.flow_labels`)
at the start of each op.

Those labels do not follow what the model sees:

- The engine cuts the transcript to `window`, but the labels stay. A page
  that left the window long ago still holds `untrusted_input`.
- `Control::ClearTranscript` clears the labels of the transcript, but it
  keeps the labels of the workspace and of the memory notes. One file that
  the assistant wrote after a page holds the label for good.
- littleguy had no way to clear the transcript at all.

On the owner's daemon, the assistant read one page, and each `run_code`
call after it waited, in every later conversation.

## Decision

- **The transcript sets the mark.** At the start of each op, the mark
  holds when the transcript holds a call of a tool with `untrusted_input`.
  The engine keeps the transcript cut to `window`, so the mark follows what
  the model sees. Within an op, the gate marks the conversation before a
  call of such a tool runs, as before.
- **The gate learns the tools.** Each build of the tools adds the names of
  the tools with `untrusted_input` to a set. A reload never removes a name,
  so a call of a tool that a reload turned off still counts.
- **Flow rules are not needed.** The mark no longer reads the labels of
  the engine, so `rule_of_two = false` no longer makes each call wait.
- **A new conversation clears the transcript.** `lg new`, the
  `new_conversation` request and the New conversation button of the chat
  page send `Control::ClearTranscript`. Memory, notes and the workspace
  stay. While a call waits for a decision, the daemon answers `busy`, and
  the owner decides first. The idempotency key names the last segment, so
  a retry of one clear clears once.

## Consequences

- A conversation that read a page asks for code until the page call
  leaves the window, or until the owner starts a new conversation.
- A file or a memory note that the assistant wrote from a page does not
  mark a later conversation. ADR 0033 accepts this for files already. The
  note tools still mark a conversation that reads a note.
- The engine Rule of Two still reads its own labels, with those of the
  workspace and the memory notes. A call that it holds waits as before.
- History shows the transcript, so a new conversation empties the chat
  page.
