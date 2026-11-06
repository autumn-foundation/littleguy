# Roadmap

The design proposal lives in the project's Claude Doc "Always-on agent on
Harvest: design proposal". Each milestone ends in something the owner can
use.

| Milestone | Delivers | Status |
|---|---|---|
| M0 | `lg serve` and `lg chat` on SQLite, the offline model, the routing kernel and its proof, CI | Done (#1) |
| M0.5 | A Claude model, ported from `examples/autoresearch/src/claude.rs` in autumn-harvest, with thinking-block replay | This PR |
| M1 | Telegram over long polling, pairing codes, approve and deny buttons | Planned |
| M2 | Durable memory, nightly consolidation, a weekday morning brief | Planned |
| M3 | Tools: web fetch and search, workspace files, sandboxed shell, MCP client, flow rules | Planned |
| M4 | Mail and calendar, with drafts that wait for approval | Planned |
| M5 | A web UI over the management API, Postgres mode, the refine loop | Planned |

Each engine gap gets a workaround here first. The engine issues are
autumn-harvest #2603 (approval notices), #2604 (durable episode and
transcript stores) and #2605 (a shared Claude model).
