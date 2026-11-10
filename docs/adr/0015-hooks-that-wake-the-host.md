# ADR 0015: Hooks that wake the host

## Status

Accepted, 2026-11-10.

## Context

A Fly Sprite costs nothing while it sleeps. A spike on 2026-11-10 measured
what that means for `lg serve` on one Sprite:

- The Sprite paused about 60 s after the last inbound request.
- An outbound websocket did not keep it awake, with a ping every 41 s or
  every 5 s. So the Discord gateway and Slack Socket Mode cannot hold a
  sleeping host awake.
- A timer did not fire while the Sprite was paused. Every missed tick
  fired in one burst at the next wake.
- A wake through the API took 430 to 590 ms after a short idle, and 17.2 s
  once after 30 minutes.

So a sleeping littleguy needs something outside to wake it, and it must
stay awake until the work that the wake starts is done. Its heartbeats,
follow-ups and morning brief are timers inside the runtime.

## Decision

- **A `[hooks]` section turns the hooks on.** The app then serves on
  `hooks.bind`, which can be public. The default is `0.0.0.0:8080`, the
  port that a Sprite routes its URL to. There is still one HTTP server.
- **The pages serve only on a loopback address.** On a public bind the app
  serves the hooks and the health routes, not the pages, and the daemon
  makes no sign-in links, so `lg ui` refuses.
- **`POST /hooks/tick` runs one tick of the daemon** and answers once the
  runs block. The request stays open while a due timer runs the model, so
  the host does not pause in the middle of a run. A cron outside, or an
  always-on member of a fleet, calls it.
- **Each hook needs a bearer token.** It comes from the variable that
  `hooks.token_env` names, `LG_HOOK_TOKEN` by default, and has at least 32
  characters. A missing or short token stops `lg serve` before the
  database opens. The token is compared in constant time and never logged.
- **The hooks are exempt from CSRF.** A machine calls them with the token,
  not a browser with a session.

```mermaid
sequenceDiagram
    participant Cron as cron or fleet
    participant Sprite as Sprite proxy
    participant App as lg serve (autumn-web)
    participant Loop as job loop
    Cron->>Sprite: POST /hooks/tick (Bearer token)
    Sprite->>App: wakes the Sprite, forwards
    App->>Loop: Job::Tick
    Loop->>Loop: poll due timers, drive the runs until they block
    Loop-->>App: ticked
    App-->>Cron: 200 ticked
    Note over Sprite: idle again, pauses about 60 s later
```

## Consequences

- A host that sleeps gets its timers through a tick. The tick decides how
  late a timer can be: a tick every 15 minutes makes a brief up to 15
  minutes late.
- Chat over the gateway or Socket Mode still needs a host that stays
  awake. The Slack Events API is a hook that the Slack signature guards
  ([ADR 0016](0016-slack-over-the-events-api.md)). Discord interactions cover slash commands only, so
  Discord direct messages keep the gateway.
- The pages and the hooks share one bind. To use the pages and the hooks
  on one machine, bind the hooks to a loopback address.
