# ADR 0039: Past conversations

## Status

Accepted, 2026-11-16.

## Context

A new conversation clears the transcript (ADR 0036), and the chat page
then shows only the new one. The owner had no way back to an old one.

The turns are still in the runtime database. Each op is a signal row of
its run, and the row stays until the purge of the run (ADR 0035). The
engine also records a state snapshot after each op. What was missing was
the boundary. With a run per message, a conversation is a stretch of
runs, and nothing marked where one ended.

## Decision

- **A clear is the boundary.** A conversation is the stretch of op
  signals of the assistant between two clears: the ops whose payload has
  `op.action = "clear_transcript"`. The order is `signal_seq`, which is
  strict and never reused. No `conversation_id` is needed.
- **The id is the first turn.** The id of a conversation is the
  `signal_seq` of its first turn. A purge never shifts it.
- **The web sees only its own turns.** A turn is a user message whose op
  key starts with `cli-` (`client::MESSAGE_ID_PREFIX`), the prefix of the
  web and the CLI. A Slack or Discord message shares the transcript, so a
  clear ends the conversation for it too. But it is no turn here, and a
  stretch with no web turn is not listed.
- **The turns of an op come from the snapshot after it.** The snapshot
  holds the transcript. The page takes it from the user message of the
  op to its end. So each text of the assistant shows, and each tool call
  with its result. The snapshot after an op is the first one in its run
  after the signal arrived. The snapshot that opens a run does not count,
  because a signal that starts a run arrives before it.
- **Tool calls show folded away.** Each call is a closed `<details>`,
  with the tool as its summary and the arguments and the result inside.
  A result is outside text, so it shows as plain text.
- **Two fallbacks.**
  - An op that adds more messages than the window keeps loses its start,
    so only its answer shows.
  - A failed op keeps the state before it, so its failure count grows.
    The page then shows the error in place of an answer.
- **Read only.** `/history` lists the conversations, newest first, and
  `/history/{id}` shows one. The page renders the thread of the chat, with
  no box to write in. The live conversation links to the chat. Continuing
  an old conversation is out of scope: it needs an inverse of
  `Control::ClearTranscript`.
- **The window is stated.** The list says how many days
  `[runs] keep_days` keeps. The oldest entry says when its first turns are
  gone: the oldest run kept began with turns that the file no longer holds.
- **The reads answer beside the loop** (ADR 0038), so the history opens
  while the assistant thinks. `lg history` and `lg history <id>` read the
  same way.
- **The `history` feature** holds it, on by default.

## Consequences

- The listing reads only the signal table and the index of the runs,
  never the events. On the owner's daemon, that table is 0.14 MB of
  155 MB.
- A conversation reads one snapshot for each turn, about 127 KB each. An
  86-turn conversation took about half a second on a copy of the owner's
  database.
- Unprompted turns, from a heartbeat or a follow-up, have no signal, so
  they do not show. A later step can walk every snapshot to add them.
- An op that a run carried to the next run in its checkpoint has no
  signal event there. Its turn shows with no answer.
- The read depends on the event format of the engine, as ADR 0038 does.
