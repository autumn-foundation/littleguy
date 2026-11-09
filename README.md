# littleguy

An always-on personal agent on
[autumn-harvest](https://github.com/autumn-foundation/autumn-harvest).

littleguy runs as one daemon on one SQLite file. The assistant is a durable
agent, `assistant/<owner>`. Every committed turn is in the event log, so a
crash or a `kill -9` costs at most the one step in flight. A restart replays
history. It does not pay for a finished model call again.

## Try it

```sh
cargo run --bin lg -- serve &
cargo run --bin lg -- chat "hello"
# littleguy (offline): you said 'hello'
```

The default model is an offline echo, so it needs no key and no network.
The owner is `$USER` unless a config file names one.

To talk to Claude, set the key and pick the model:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
cargo run --bin lg -- serve --model claude
```

The default is `claude-opus-5-5`. `--model-name` (or `LG_MODEL_NAME`)
picks another one. The key comes from the environment only, so it never
shows in a process list. Claude returns thinking blocks that the API wants
back unchanged. The daemon keeps them in `claude-turns/` next to the
database, so a restart replays them.

OpenAI and OpenRouter share one adapter over the Chat Completions API.
Each needs its key and a model name:

```sh
export OPENAI_API_KEY=sk-...
cargo run --bin lg -- serve --model openai --model-name <an OpenAI model>

export OPENROUTER_API_KEY=sk-or-...
cargo run --bin lg -- serve --model openrouter --model-name <vendor>/<model>
```

`--base-url` (or `LG_BASE_URL`) points `--model openai` at any other server
that speaks the same API, such as a local one.

## Discord

Create a bot in the Discord developer portal. Under Privileged Gateway
Intents it needs nothing: littleguy reads direct messages only. Then set the
token and start the daemon:

```sh
export DISCORD_BOT_TOKEN=...
cargo run --bin lg -- serve
```

The token comes from the environment only. With no token, the daemon runs
the local socket alone.

A stranger who sends the bot a direct message gets a pairing code, such as
`ABCD-1234`. The owner lets them in on the machine that runs the daemon:

```sh
lg pair ABCD-1234   # paired discord:123456789012345678
lg pairs            # every allowed sender, with its source
lg unpair discord:123456789012345678
```

A code lasts 10 minutes and works once. Only the user who runs the daemon
may pair. Pairings live in `pairings.json` next to the database.

Each Discord message runs once, and its reply goes out once. After a
reconnect, the daemon reads the direct messages it missed and answers each
one. [ADR 0002](docs/adr/0002-pairing-and-delivery.md) explains how.

## Camel routes

littleguy runs [harvest-camel](https://github.com/autumn-foundation/harvest-camel)
routes on its own SQLite file. Each route reads from a camel component and
hands each message to the assistant. Add one per source in `lg.toml`:

```toml
[[camel.route]]
id = "inbox"                                  # the sender is camel:inbox
from = "file:/home/mark/lg-inbox?delete=true" # file or timer
prompt = "A file arrived. Summarize it."      # goes before each body
```

- A file goes to the assistant once. The route deletes it, or with
  `noop=true` leaves it, only after the message is durable in the database.
- A body that is not UTF-8 text is a dead letter in
  `harvest_connector_dead_letters`, in `lg.db`. It never reaches the model.
- The owner writes the routes, so their messages need no pairing.
  [ADR 0003](docs/adr/0003-camel-routes.md) explains the design.

## OAuth connections

littleguy embeds [San Mateo](https://github.com/autumn-foundation/san-mateo)
to keep OAuth tokens for later tools, such as mail and calendar. San Mateo
seals each token with AES-256-GCM in `san-mateo.db`, next to `lg.db`, and
refreshes it before it expires. Name each integration in `lg.toml`:

```toml
[san_mateo]
bind = "127.0.0.1:3003"        # loopback only

[[san_mateo.integration]]
key = "google"
provider = "google"            # a provider of the San Mateo catalog
client_id = "1234.apps.googleusercontent.com"
client_secret_env = "LG_GOOGLE_CLIENT_SECRET"
scopes = ["https://www.googleapis.com/auth/gmail.readonly"]
```

The sealing key and each client secret come from the environment only:

```sh
export LG_SAN_MATEO_KEY="$(openssl rand -base64 32)"   # keep it: it opens the tokens
export LG_GOOGLE_CLIENT_SECRET=...
lg serve &
lg connect google      # prints a link: open it in a browser on this machine
lg connections         # integration, connection id, provider, date, state
lg disconnect google <connection id>
```

Register `http://127.0.0.1:3003/oauth/callback` as the redirect URI of the
OAuth app. [ADR 0004](docs/adr/0004-oauth-with-san-mateo.md) explains the
design.

## Config

`lg serve` reads `<config home>/littleguy/lg.toml` when it exists:

```toml
owner = "mark"
system = "You are littleguy, a small and helpful personal assistant."
window = 40

[discord]
pairing = true                        # strangers get a code; false drops them
allowed = ["123456789012345678"]      # Discord user ids allowed from the start

[[camel.route]]
id = "inbox"
from = "file:/home/mark/lg-inbox?delete=true"
```

| Flag | Variable | Default |
|---|---|---|
| `--socket` | `LG_SOCKET` | `<data home>/littleguy/lg.sock` |
| `--db` | `LG_DB` | `<data home>/littleguy/lg.db` |
| `--config` | `LG_CONFIG` | `<config home>/littleguy/lg.toml` |
| none | `DISCORD_BOT_TOKEN` | unset: no Discord |
| none | `LG_SAN_MATEO_KEY` | required with `[san_mateo]` |

`<data home>` is `$XDG_DATA_HOME`, or `~/.local/share`. `<config home>` is
`$XDG_CONFIG_HOME`, or `~/.config`.

## Who can talk to it

The socket has mode 0600. The daemon also reads the peer credentials of each
connection. Only the user who runs the daemon reaches the assistant from the
socket, and only that user may pair, list or unpair.

On Discord, a sender reaches the assistant when the config file allows them
or a pairing code added them. Anyone else gets a code, or nothing when
`pairing = false`. The routing kernel makes each decision, and
`formal/verus/src/route.rs` proves it. On the camel channel, only the routes
of the config file are allowed senders. `formal/verus/src/pairing.rs` proves
when a code is live and when a new one is issued.

## Layout

| Path | What it holds |
|---|---|
| `crates/littleguy-core` | Ids, config, the routing kernel and pairing codes. No I/O. |
| `crates/littleguy-channels` | Discord: the gateway supervisor, REST calls, and a fake Discord for tests. |
| `crates/littleguy-models` | `AgentModel` implementations: the offline model, Claude, and OpenAI or OpenRouter. |
| `crates/littleguy` | The `lg` binary: `serve`, `chat`, `pair`, `pairs`, `unpair`, `connect`, `connections` and `disconnect`, the camel routes and San Mateo. |
| `formal/verus` | Verus proofs of the kernels. |
| `xtask` | Every CI gate: `cargo xtask ci`. |

[`docs/roadmap.md`](docs/roadmap.md) lists the milestones.
