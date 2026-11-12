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
| M1.5 | Slack over Socket Mode, with pairing, replies sent once and approval buttons | Done (#13) |
| M1.8 | A cargo feature for each integration, a feature table rendered from the manifests, and a CI matrix over the features | Done (#14) |
| M2 | Durable memory and an episode log in `memory.db`, and a nightly consolidation pass on a cron schedule | Done (#7) |
| M2.1 | A weekday morning brief that stays silent when there is nothing to say | Done (#8) |
| M3.1 | Tools: web fetch with a public-address guard, Brave web search, persistent workspace files | Done (#9) |
| M3.2 | A sandboxed shell, and approve and deny buttons in Discord | Done (#10) |
| M3.3 | An MCP client and flow rules | Done (#11) |
| M4 | Mail and calendar tools on the M1.7 connections, with drafts that wait for approval | Done (#12) |
| M5.1 | The refine loop: profile overlays that the model proposes and the owner activates, `lg refine` | Done (#15) |
| M5.2 | `lg serve` as an autumn-web app, with maud and htmx pages on loopback and `lg ui` sign-in links | Done (#16) |
| M5.3 | Hooks that wake a sleeping host: `[hooks]` and `POST /hooks/tick`, after a Fly Sprite spike | Done (#17) |
| M5.4 | Slack over the Events API, as a hook, for a host that sleeps | Done (#18) |
| F1 | `[fleet]`: the memory, the episodes and the approvals in a tenant database, replicated by autumn-web to a directory or an S3-compatible store, and restored on a new volume | Done (#19) |
| R1 | Reload the config without a restart: a file watch and `lg reload`, camel routes added and removed in place | This PR |
| F2 | An agent per tenant on `DatabaseFleet` and `HarvestPlugin::sqlite_per_tenant` | Waits on autumn-harvest 0.11 |
| F3 | The leader: one always-on host that takes Slack and forwards each event to the follower of its owner | Planned |
| F4 | The wake sweep: the leader wakes a closed tenant at its next timer | Waits on autumn-harvest #2650 |
| F5 | A takeover drill with slot ownership and fencing | Waits on autumn-harvest #2649 |
| F6 | Operator write tools behind the approval gate | Planned |
| M5 | Postgres mode and camel request and reply | Planned |

The fleet milestones F1 to F6 follow the Claude Doc "littleguy on a fleet:
design". F1 keeps two stores out of the tenant database. The refine
overlay log waits on autumn-harvest #2656, and the San Mateo store waits on
san-mateo #10. Each keeps one connection open, which the replicator of the
fleet cannot allow ([ADR 0017](adr/0017-the-tenant-database-on-a-fleet.md)).

Each engine gap gets a workaround here first. The engine issues are
autumn-harvest #2603 (approval notices), #2604 (durable episode and
transcript stores) and #2605 (a shared Claude model). `littleguy-stores`
is the #2604 workaround for memory and episodes. The approval records in
`memory.db` and the Discord notice are the #2603 workaround.

The Slack channel uses `autumn-plugin-slack` 0.4 for the signature check
of the Events API only. The plugin has no Socket Mode, its Web API client
cannot be built outside an autumn-web app, and its action routes match a
fixed `action_id`. `littleguy-channels` has its own Socket Mode and Web API
client until the plugin has them.

The Discord channel uses `autumn-plugin-discord` 0.3 for the gateway. Three
gaps get a workaround in `littleguy-channels`: the plugin has no call to
send a channel message or open a DM, a failed connect ends its run, and it
does not resume a session.
