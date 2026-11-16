# ADR 0037: A reviewer decides first

## Status

Accepted, 2026-11-16. It extends ADR 0033.

## Context

With `[sandbox] approve = "untrusted"`, code runs unasked until the
conversation reads outside text (ADR 0033, ADR 0036). The assistant reads
outside text often: a web page, a search, a note, its own run log. After
that, each `run_code` call waits for the owner, and most of those calls
only do what the owner just asked. The owner approves call after call and
reads none of them closely.

The danger behind each wait is narrow: outside text steers the model, and
the model writes a call that serves the text, not the owner. A reader who
never sees the outside text cannot be steered by it, and can tell a call
that serves the owner's words from one that does not.

## Decision

- **`approve = "auto"`** runs code unasked as `"untrusted"` does. Each call
  that would wait for the owner goes to a reviewer first: a `run_code`
  call after outside text, and a call that a flow rule holds.
- **The reviewer reads only the owner's words and the call.** It gets the
  text of the last six user turns, with the message whose op is in
  flight, and the tool and arguments of the call. It never reads a tool
  result. So outside text cannot reach it.
- **Three answers.** The reviewer answers JSON: `approve`, `deny` or
  `ask`, with a reason. An approval sends the engine signal, and the run
  goes on, so the next call that waits gets its own review. A denial tells
  the assistant why. `ask`, an answer that does not parse, a model call
  that fails, and a call with no words of the owner wait for the owner,
  and the notice and the chat card carry the reviewer's reason.
- **One review per call.** The daemon marks a call reviewed before it asks,
  so a restart or a later settle never asks twice.
- **The reviewer model** is the model of the assistant, or another model of
  the same provider: `--reviewer-model-name` or `LG_REVIEWER_MODEL_NAME`.
  It is a model call with no tools. TypeSafe's typed judgment API, with
  calibrated probabilities, needs its own key and endpoint. OpenRouter
  offers only its Jev Router, a chat model, so the reviewer stays a chat
  call for now.
- **The `auto-approve` feature** holds it. A config with `approve = "auto"`
  fails at start in a build without it.

## Consequences

- Most waits end without the owner: code that does what the owner asked
  runs after one short model call.
- Each review is one more model call and a few seconds before the call
  runs. A small model as the reviewer keeps both down.
- The reviewer judges from the owner's words. A call that the owner's
  words do not explain waits for the owner, or is denied.
- A user turn that is not the owner's own message, such as a heartbeat
  prompt or a wake that the assistant booked, counts as the owner's words.
  A wake prompt holds the words of the model.
- A reviewer that approves a harmful call lets it run. Code in a run still
  cannot act as the owner (ADR 0032), and `network = false` keeps it from
  sending data out.
