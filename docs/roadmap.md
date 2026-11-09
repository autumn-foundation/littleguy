# Roadmap

The design proposal lives in the project's Claude Doc "Always-on agent on
Harvest: design proposal". Each milestone ends in something the owner can
use.

| Milestone | Delivers | Status |
|---|---|---|
| M0 | `lg serve` and `lg chat` on SQLite, the offline model, the routing kernel and its proof, CI | Done (#1) |
| M0.5 | A Claude model, ported from `examples/autoresearch/src/claude.rs` in autumn-harvest, with thinking-block replay | Done (#2) |
| M0.6 | OpenAI and OpenRouter over one Chat Completions adapter | Done (#3) |
| M1 | Discord direct messages over the gateway, pairing codes, replies sent once | Done (#4) |
| M1.6 | Camel routes (file and timer) that feed the assistant through harvest-camel on SQLite | Done (#5) |
| M1.7 | OAuth connections kept by an embedded San Mateo on SQLite: `lg connect`, `lg connections`, `lg disconnect` | Done (#6) |
| M1.5 | Slack over Socket Mode, when autumn-plugin-slack 0.4 is on crates.io | Planned |
| M2 | Durable memory and an episode log in `memory.db`, and a nightly consolidation pass on a cron schedule | This PR |
| M2.1 | A weekday morning brief that stays silent when there is nothing to say | Planned |
| M3 | Tools: web fetch and search, workspace files, sandboxed shell, MCP client, flow rules, approve and deny buttons | Planned |
| M4 | Mail and calendar tools on the M1.7 connections, with drafts that wait for approval | Planned |
| M5 | A web UI over the management API, Postgres mode, camel request and reply, the refine loop | Planned |

Each engine gap gets a workaround here first. The engine issues are
autumn-harvest #2603 (approval notices), #2604 (durable episode and
transcript stores) and #2605 (a shared Claude model). `littleguy-stores`
is the #2604 workaround for memory and episodes.

The Discord channel uses `autumn-plugin-discord` 0.3 for the gateway. Three
gaps get a workaround in `littleguy-channels`: the plugin has no call to
send a channel message or open a DM, a failed connect ends its run, and it
does not resume a session.
