# ADR 0034: No arbitrary caps on the work of the model

## Status

Accepted, 2026-11-16. It amends ADR 0030. The second decision, on the
caps of stores, tools and prompts, amends ADRs 0010, 0013, 0031 and 0037.

## Context

autumn-harvest-agent 0.10 sends the model call of a consolidation pass
with a fixed output cap of 2048 tokens (`consolidate::PASS_OUTPUT_TOKENS`).
No setting changes it (autumn-harvest #2695).

A model that reasons spends its hidden reasoning from the same cap. On the
owner's daemon, two nightly passes in a row used all 2048 tokens. The
input was small: 4422 tokens one night and 1509 the next. So a smaller
batch of episodes (ADR 0030) does not help. The reply was cut off, the
pass stopped with `invalid_reply`, it wrote no fact, and no reflection
followed. So the assistant proposed no goals and no skills.

Less reasoning is no fix either. The same model does no tool calls with
reasoning off, and a low effort can still fill 2048 tokens.

The same kind of guess bounded other work too:

- Each message stopped after 8 tool rounds (`max_steps`).
- The engine cut each tool result to 8000 characters. It did that after
  the tool cut its own result by a setting of the owner, such as
  `fetch_max_chars = 20000`. So the setting did not hold.
- Each HTTP request to a model failed after 10 minutes, below the
  13-minute budget of the engine for one model call.

Each of these drops work that the model did or would do. The harness
already bounds what must be bounded: the engine times out a model call,
the store bounds the size of one activity result, and each tool bounds
its own output.

## Decision

- **A pass goes with no cap of its own.** The limit of the model applies.
  `[memory] pass_tokens` sets a cap when the owner wants one. Only 0 is
  refused.
- **A model wrapper applies it** (`memory::PassTokens`). The daemon wraps
  the model of the harness at each config load. A request is a pass when
  its system prompt is `consolidate::PASS_SYSTEM`, its cap is
  `consolidate::PASS_OUTPUT_TOKENS` and it offers no tools. The wrapper
  sets the cap of such a request and sends every other request
  unchanged. Both constants are public in the engine, so a change there
  shows up as a pass that the wrapper no longer knows, not as a wrong
  request. The wrapper passes the model id and the cost through.
- **An op has no bound on its tool rounds.** `max_steps` is unset by
  default, and the daemon gives the engine `u32::MAX`. An owner who wants
  a bound sets one.
- **The engine cuts no tool result.** The harness gets a tool output
  limit of `usize::MAX`. Each tool cuts its own result.
- **A model request has no time bound of its own.** The HTTP client keeps
  only a connect timeout. The engine bounds each model call.

## Consequences

- A pass on a model that reasons gets room for its reasoning and its
  reply.
- A message runs until the model answers. A model that loops costs model
  calls until the owner stops it or sets `max_steps`.
- A long tool result goes to the model whole, and it costs input tokens
  for as long as it stays in the window.
- The engine still bounds one model call at 13 minutes, under the fixed
  15-minute bound of its model-turn activity. That is an engine matter.
- The pass wrapper is a workaround. Remove it when the engine takes the
  cap as a setting, and map `pass_tokens` to that setting.

## Update, 2026-11-22

autumn-harvest-agent 0.11 takes the cap as a setting,
`EpisodeSettings::max_pass_output_tokens`, and sends no cap by default.
The wrapper is gone. `pass_tokens` sets that field.

## Second decision: the caps of stores, tools and prompts

A sweep found more invented bounds. Each one dropped or refused work, or
hid part of what the owner or the model read:

- The Claude model sent a cap of 16 000 tokens when a request named none,
  and a council vote sent 300. A model that reasons can spend that on its
  reasoning before it answers.
- The plans store refused a title over 200 characters and a text over
  4000, and kept only the newest 50 notes of a goal or an idea. A wake
  refused a text over 2000 characters.
- `[refine] messages` refused a count over 500, and the evidence was cut
  to 64 KiB and to 500 characters a tool part.
- An `api_get` answer and an MCP result were cut at 20 000 characters, and
  a mail at 20 000 with no way to read on.
- A web search took at most 10 results, a Google list 25, a grep 200
  matches with each line cut at 300 characters, and the run log 50 rows
  with each error cut at 500 characters.
- A reflection listed 40 facts at 240 characters each, and the reviewer
  of ADR 0037 read only the last six messages of the owner.
- The web card and `lg approvals` showed 1500 characters of the code that
  the owner approved.

So:

- **Where an API needs a number, it gets the API's own maximum.** A
  Claude request that names no cap asks for the most output of the
  model: 128 000 tokens on a current model, less on the models before the
  4.6 line. A search asks for up to 20 results, the Brave maximum, a
  Gmail search up to 500 and a Calendar list up to 2500.
- **A council vote has no output cap.**
- **A store refuses only empty text.** No length bound, and every note
  stays.
- **A setting refuses only a value that cannot work.** `[refine]
  messages` refuses 0.
- **A result that a tool can read again comes in pages.** A mail reads on
  from an `offset`, as `read_file` and `web_fetch` do. An `api_get` answer
  and an MCP result come whole: a second call could repeat what the call
  did. `[api] max_chars` is unset by default, and an owner who wants a
  bound sets one.
- **A prompt gets its input whole.** The reflection lists every fact, the
  refine evidence holds each message whole, and the reviewer reads every
  message of the owner in the conversation.
- **The owner sees the whole call.** The web card and `lg approvals` show
  all of it. Only a Discord or Slack notice cuts the code, because those
  services split a long message and a split breaks the code frame. The
  notice then names the places that show all of it.

The bounds against a runaway loop stay: open ideas, proposed and active
goals, skill drafts, pending wakes, the chain of wakes and how far ahead
a wake may be. So do the limits of outside services and of security,
such as the size of a Discord message, the length of a tool name, a body
of 2 MiB from a fetch, and the size of a compiled pattern.

### Consequences

- A long result, mail or prompt costs input tokens, and a long call
  costs output tokens, as the model chooses.
- A Claude request without streaming can now run long. The HTTP client
  has no read timeout, and the engine bounds the model call.
