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

## Build features

Each integration is a cargo feature, so a build carries only the crates it
uses. The default set is `discord`, `slack`, `claude`, `openai`, `web`,
`mcp`, `sandbox` and `refine`. `camel`, `oauth` and `google` are opt-in,
and `full` turns on every one:

```sh
cargo install --path crates/littleguy --features full
cargo install --path crates/littleguy --no-default-features --features discord,claude
```

A config section of a feature that the build leaves out stops `lg serve`
at start, and the error names the feature.
[`docs/features.md`](docs/features.md) lists each feature and the crates
it adds. CI renders that table from the manifests, so it cannot drift.
[ADR 0012](docs/adr/0012-feature-flags.md) explains the design.

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

## Slack

littleguy talks to Slack over Socket Mode, so the machine needs no public
URL. Create a Slack app, then:

1. Turn on **Socket Mode**, and make an app-level token with the
   `connections:write` scope. It starts with `xapp-`.
2. Under **OAuth & Permissions**, add the bot scopes `chat:write`,
   `im:history` and `im:write`, and install the app. The bot token starts
   with `xoxb-`.
3. Under **Event Subscriptions**, subscribe the bot to `message.im`.
4. Under **App Home**, let users send messages from the Messages tab.
5. Turn on **Interactivity**. Socket Mode needs no request URL.

```sh
export SLACK_BOT_TOKEN=xoxb-...
export SLACK_APP_TOKEN=xapp-...
cargo run --bin lg -- serve
```

Both tokens come from the environment only. With one of them missing, the
log says so and Slack stays off. Pairing works as on Discord, with Slack
user ids:

```toml
[slack]
pairing = true            # strangers get a code; false drops them
allowed = ["U012AB3CD"]   # Slack user ids allowed from the start
```

```sh
lg pair ABCD-1234         # paired slack:U012AB3CD
lg unpair slack:U012AB3CD
```

Each Slack message runs once, and its reply goes out once, as on Discord.
Every reply escapes `&`, `<` and `>`, so no answer can mention `@channel`
or a person. Discord and Slack can run in one daemon.
[ADR 0011](docs/adr/0011-slack-over-socket-mode.md) explains the design.

## Camel routes

Feature `camel`, off by default.

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

## Memory

The assistant keeps notes across runs, as the `memory` and `user` blocks of
its prompt. It edits them itself with the `memory` tool, and a run reads
them once, at its start. Ask it to remember something, and the next run
knows it.

Each answer also goes into an episode log. Each night a consolidation pass
reads the new episodes, asks the model for durable facts, and adds each one
to the `memory` block, such as `mark likes tea [episodes: ...]`. An idle
night costs no model call.

```toml
[memory]
enabled = true                  # false keeps no notes
consolidate = "0 3 * * *"       # cron, in the time zone below
timezone = "Europe/London"      # IANA name; the default is UTC
```

The notes and the log live in `memory.db`, next to `lg.db`. The pass runs
only while `lg serve` runs. After a night off it runs once, at the next
start. [ADR 0005](docs/adr/0005-memory-and-consolidation.md) explains the
design.

## Morning brief

A `[brief]` section turns on a short brief on weekday mornings. At each
slot the assistant reads its notes and the recent conversation, and lists
what needs you today. It sends the list as a direct message. When nothing needs
you, it answers `HEARTBEAT_OK`, and you get no message.

```toml
[brief]
schedule = "30 7 * * 1-5"       # cron; the default is 07:30, Monday to Friday
timezone = "Europe/London"      # IANA name; the default is the memory time zone
to = "123456789012345678"       # your Discord user id, or "slack:U012AB3CD"; without it, the log only
# prompt = "..."                # the instruction of each brief
```

The brief is read-only: it calls no tool that acts, and it writes no note.
Each slot sends at most one DM, also across a restart. A slot that passes
while `lg serve` is down runs once, at the next start. The brief needs the
Discord or Slack channel to reach you. [ADR 0006](docs/adr/0006-morning-brief.md)
explains the design.

## Tools

The assistant can read a public web page, search the web, and keep files
in its own workspace. Each tool is on by default:

```toml
[tools]
web_fetch = true                # read one http or https page as text
web_search = true               # needs LG_BRAVE_API_KEY
files = true                    # read_file, write_file, list_files, delete_file
fetch_max_chars = 20000         # the most text of one page
```

- `web_fetch` resolves the host first and refuses a private, loopback,
  link-local or reserved address, also after a redirect. So a page cannot
  aim it at your router or a local service. A proof in `formal/verus`
  covers the IPv4 check. It does not use an HTTP proxy.
- `web_search` uses the [Brave Search API](https://brave.com/search/api/).
  Put the key in `LG_BRAVE_API_KEY`; without it there is no search tool.
- The workspace keeps its files in `workspace/`, next to `lg.db`, and they
  survive a restart. A path stays inside the workspace.

The web tools only read, and their results count as untrusted input. The
file tools change only the private workspace. A morning brief can read but
not write. [ADR 0007](docs/adr/0007-first-tools.md) explains the design.

## Running code

`[sandbox]` gives the assistant a `run_code` tool for python3, node, bash
and sh. No call runs until a person approves it:

```toml
[sandbox]
backend = "firecracker"         # or "process", for development only
network = false                 # the code gets no network

[approvals]
approver = "123456789012345678" # the Discord user who gets the buttons, or "slack:U012AB3CD"
timeout_secs = 3600             # no decision by then denies the call

[sandbox.firecracker]
firecracker = "/usr/local/bin/firecracker"
jailer = "/usr/local/bin/jailer"
kernel = "/var/lib/littleguy/vmlinux"
rootfs = "/var/lib/littleguy/rootfs.ext4"   # with the engine guest agent
uid = 990
gid = 990
netns = "/run/netns/littleguy"              # optional, an empty namespace
```

When the assistant wants to run code, the approver gets a direct message on
Discord or Slack with the code and **Approve** and **Deny** buttons. The owner can also decide
from the shell:

```sh
lg approvals            # each call that waits, with its code
lg approve 3
lg deny 3 --reason "not on this machine"
```

The call runs with exactly the code that the approver read. While it
waits, the assistant takes no new work: `lg chat` says it is busy, and a
Discord message waits and runs after the decision. The `firecracker`
backend runs each call in a new microVM. The `process` backend runs code as
a host process, so use it only for development.
[ADR 0008](docs/adr/0008-sandbox-and-approvals.md) explains the design.

## MCP servers

Each `[[mcp.server]]` gives the assistant the tools of one MCP server. A
server runs as a child process, or answers at a URL:

```toml
[[mcp.server]]
id = "github"                           # each tool is github__<tool>
command = "github-mcp-server"
args = ["stdio"]
env = ["GITHUB_PERSONAL_ACCESS_TOKEN"]  # the only variables it gets, with PATH and HOME
labels = ["untrusted_input", "private_data"]
effect = "external"                     # read_only, internal, write or external (the default)

[[mcp.server]]
id = "notes"
url = "http://127.0.0.1:8808/mcp"       # https, or http to this machine
bearer_env = "LG_NOTES_TOKEN"           # the variable that holds the bearer token
labels = ["private_data"]
effect = "write"
tools = ["search", "append"]            # keep only these
timeout_secs = 60                       # the bound on one call
```

A server that does not start costs only its own tools, and the log says
why. You set the labels and the effect, not the server. Label a server
`untrusted_input` when strangers can write what it returns, such as mail or
issues. Label it `private_data` when it returns what only you should see.

### Flow rules

The Rule of Two is on by default. A call waits for a person when the
conversation would then hold untrusted input, private data, and a write or
an outside effect, all three. The decision works as for code: the approver
gets the call in a DM with buttons, or you run `lg approve`.

```toml
[flow]
rule_of_two = true          # false turns the rule off
recheck = "each_call"       # or "until_approved": one yes per op
```

The labels stay with the conversation. After the assistant reads your
private mail, each later send asks first. The web tools count as untrusted
input. [ADR 0009](docs/adr/0009-mcp-and-flow-rules.md) explains the design.

## OAuth connections

Feature `oauth`, off by default.

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
scopes = [
  "https://www.googleapis.com/auth/gmail.modify",
  "https://www.googleapis.com/auth/calendar.events",
]
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

## Mail and calendar

Feature `google`, off by default. It turns `oauth` on.

With a Google connection, the assistant reads and writes your Gmail and
Google Calendar. Name the San Mateo integration in `[google]`:

```toml
[[san_mateo.integration]]
key = "google"
provider = "google"
client_id = "1234.apps.googleusercontent.com"
client_secret_env = "LG_GOOGLE_CLIENT_SECRET"
scopes = [
  "https://www.googleapis.com/auth/gmail.modify",
  "https://www.googleapis.com/auth/calendar.events",
]

[google]
integration = "google"   # the key of the integration above
mail = true              # mail_search, mail_read, mail_draft, mail_send
calendar = true          # calendar_events, calendar_create
```

Run `lg connect google` once to connect the account.

Each send waits for a person, and so does each event that invites someone.
The approver sees the addresses, the subject and the whole body before
anything goes out. Mail and events count as untrusted input and private
data, so after a read the Rule of Two also holds each draft. Tokens stay in
San Mateo, and no tool sees one.
[ADR 0010](docs/adr/0010-mail-and-calendar.md) explains the design.

## Web pages

`lg serve` is an autumn-web app. Its pages show the chat, the calls that
wait for approval, who can talk to the assistant, the refine overlays and
the OAuth connections. They serve on a loopback address only:

```toml
[ui]
bind = "127.0.0.1:3004"   # the default; a loopback address is required
```

To sign a browser in, run `lg ui` and open the link that it prints. The
link works once, for five minutes. Each action needs the session and the
CSRF token. [ADR 0014](docs/adr/0014-autumn-web-app.md) explains the
design.

## Hooks

A host that sleeps, such as a Fly Sprite, fires no timer while it is
paused, and an outbound socket does not keep it awake. A hook wakes it:

```toml
[hooks]
bind = "0.0.0.0:8080"        # the default; a public address is allowed
token_env = "LG_HOOK_TOKEN"  # the default; at least 32 characters
```

`POST /hooks/tick` with `Authorization: Bearer $LG_HOOK_TOKEN` runs the
due timers and answers once the runs block, so the host stays awake while
they work. Call it from a cron. With `[hooks]`, the app serves on the
hooks address, and the pages serve only when that address is loopback.
[ADR 0015](docs/adr/0015-hooks-that-wake-the-host.md) explains the design.

## Refine loop

The assistant can propose changes to its own profile. Add a `[refine]`
section, talk to the assistant for a while, then ask for a proposal:

```toml
[refine]
messages = 40      # the transcript tail that a proposal reads, 1 to 500
dimension = "inv"  # "inv": how it works; "dom": what it knows of your domain
```

```sh
lg refine propose      # the model writes an overlay; it is not active
lg refine list         # every version; * marks the active one
lg refine show 1       # the overlay JSON
lg refine activate 1   # the next message runs on it
lg refine rollback     # back to the version before
lg refine put base.json   # your own overlay; {} is the config profile
```

An overlay can change the system prompt, guardrails, tool guidance and
skills, and narrow the bounds. It cannot raise a bound or pick another
model. Nothing activates without you, and no restart is needed. The
versions live in `overlays.db` next to the database.
[ADR 0013](docs/adr/0013-refine-loop.md) explains the design.

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
| none | `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN` | unset: no Slack |
| none | `LG_SAN_MATEO_KEY` | required with `[san_mateo]` |
| none | `LG_HOOK_TOKEN` | required with `[hooks]` |

`<data home>` is `$XDG_DATA_HOME`, or `~/.local/share`. `<config home>` is
`$XDG_CONFIG_HOME`, or `~/.config`.

## Who can talk to it

The socket has mode 0600. The daemon also reads the peer credentials of each
connection. Only the user who runs the daemon reaches the assistant from the
socket, and only that user may pair, list or unpair.

On Discord and Slack, a sender reaches the assistant when the config file allows them
or a pairing code added them. Anyone else gets a code, or nothing when
`pairing = false`. The routing kernel makes each decision, and
`formal/verus/src/route.rs` proves it. On the camel channel, only the routes
of the config file are allowed senders. `formal/verus/src/pairing.rs` proves
when a code is live and when a new one is issued.

## Layout

| Path | What it holds |
|---|---|
| `crates/littleguy-core` | Ids, config, the routing kernel and pairing codes. No I/O. |
| `crates/littleguy-channels` | Discord and Slack: the gateway supervisor, the Socket Mode client, their REST calls, and a fake of each for tests. |
| `crates/littleguy-models` | `AgentModel` implementations: the offline model, Claude, and OpenAI or OpenRouter. |
| `crates/littleguy-tools` | The tools: web fetch with its address guard, web search, the workspace files, the MCP client, and Gmail and Calendar. |
| `crates/littleguy-stores` | The SQLite memory store and episode store, until the engine has its own, and the calls that wait for approval. |
| `crates/littleguy` | The `lg` binary: `serve`, `chat`, `pair`, `pairs`, `unpair`, `connect`, `connections`, `disconnect`, `approvals`, `approve` and `deny`, the camel routes, San Mateo, the sandbox and the approval gate. |
| `formal/verus` | Verus proofs of the kernels. |
| `xtask` | Every CI gate: `cargo xtask ci`, and the feature matrix: `cargo xtask features`. |

[`docs/roadmap.md`](docs/roadmap.md) lists the milestones.
