# ADR 0016: Slack over the Events API

## Status

Accepted, 2026-11-10.

## Context

[ADR 0011](0011-slack-over-socket-mode.md) runs Slack over Socket Mode. The
daemon holds a websocket open, so the machine needs no public URL.
[ADR 0015](0015-hooks-that-wake-the-host.md) measured a Fly Sprite: an
outbound socket does not keep it awake, and it pauses about 60 s after the
last inbound request. A paused Sprite reads no message from the socket.

The Slack Events API turns this around. Slack sends each event to a
request URL, and an inbound request wakes the Sprite. Slack signs each
request with the signing secret of the app. It waits three seconds for
the answer, and sends the event again when the answer is late or fails.

autumn-plugin-slack 0.4 checks that signature with a public `Verifier`. Its
own routes match each `action_id` exactly. littleguy puts the approval id
in the action id, as `lg:approve:3`, so those routes do not fit.

## Decision

- **The variables choose the mode.** `SLACK_BOT_TOKEN` with
  `SLACK_APP_TOKEN` is Socket Mode, as before. `SLACK_BOT_TOKEN` with
  `SLACK_SIGNING_SECRET` is the Events API. Both the app token and the
  secret stop `lg serve` at start, since lg cannot tell which mode the
  owner wants. A part alone logs what is missing, and Slack stays off.
- **The Events API needs `[hooks]`.** Slack calls two hooks on the hooks
  bind: `POST /hooks/slack/events` and `POST /hooks/slack/interactions`.
  Without `[hooks]`, the signing secret logs a warning, and Slack stays
  off.
- **The Slack signature guards both hooks, not the bearer token.** Slack
  cannot send a bearer token. Each hook checks the signature with the
  `Verifier` of autumn-plugin-slack before it parses the body. The clock
  may stray five minutes, which stops a replay of an old request. The
  secret comes from the environment only and never reaches a log.
- **Each hook answers first, then works.** A run takes longer than three
  seconds, so the hook spawns the delivery and answers 200. The message id
  `slack:<channel>:<ts>` is the key of its op, so a retry from Slack runs
  nothing new. A Sprite stays awake about a minute after the request,
  which covers most runs. A run that the pause cuts goes on at the next
  wake.
- **The challenge answers itself.** A signed `url_verification` answers
  with its `challenge`, so Slack accepts the request URL.
- **The daemon reads the missed messages once at start.** Socket Mode
  reads them at each connect. The Events API has no connect, so the
  daemon reads the history after each cursor when it starts.
- **One way in.** The hooks reach the job loop through a `SlackInbox`. It
  runs the same `deliver` and press code as the Socket Mode loop.

```mermaid
sequenceDiagram
    participant Slack
    participant App as lg serve (autumn-web)
    participant Loop as job loop
    Slack->>App: POST /hooks/slack/events (signed)
    App->>App: check the signature
    App-->>Slack: 200
    App->>Loop: Job::Chat (spawned)
    Loop->>Loop: run the turn, keyed by the message id
    Loop-->>App: the reply
    App->>Slack: chat.postMessage
```

## Consequences

- A host that sleeps can chat over Slack. It needs a public URL, which a
  Sprite has.
- Socket Mode stays the default for a machine without a public URL.
- Discord still needs the gateway for direct messages, so a sleeping host
  chats over Slack only.
- A message that Slack gives up on, after its retries, waits for the next
  start of the daemon.
