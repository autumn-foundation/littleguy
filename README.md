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
The owner is `$USER` unless a config file names one. To set one up, copy
[`lg.toml.example`](lg.toml.example) to `~/.config/littleguy/lg.toml`
(see [Config](#config)).

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

A reasoning model writes hidden reasoning before each answer, and that
output takes time: on a slow turn it can be half of what the model writes.
`--reasoning` (or `LG_REASONING`) sets how much, for `--model openai` and
`--model openrouter`: `none`, `minimal`, `low`, `medium` or `high`. Without
it, the model's own default holds. OpenAI gets `reasoning_effort`, and
OpenRouter gets its `reasoning` object, where `none` turns reasoning off:

```sh
cargo run --bin lg -- serve --model openrouter --model-name <vendor>/<model> --reasoning low
```

## Build features

Each integration is a cargo feature, so a build carries only the crates it
uses. The default set is `discord`, `slack`, `claude`, `openai`, `web`,
`mcp`, `sandbox` and `refine`. `camel`, `oauth`, `google` and `api` are
opt-in, and `full` turns on every one:

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

### Slack over the Events API

A host that sleeps cannot hold a socket open. Slack can call it instead,
on the hooks (see [Hooks](#hooks)). Give the bot token and the signing
secret, not the app token:

```sh
export SLACK_BOT_TOKEN=xoxb-...
export SLACK_SIGNING_SECRET=...   # Basic Information > App Credentials
export LG_HOOK_TOKEN=...          # [hooks] needs it too
cargo run --bin lg -- serve
```

In the Slack app, leave Socket Mode off. Set the request URL of **Event
Subscriptions** to `https://<host>/hooks/slack/events`, and the one of
**Interactivity** to `https://<host>/hooks/slack/interactions`. Each
request needs a good Slack signature. Both the app token and the signing
secret stop `lg serve` at start.
[ADR 0016](docs/adr/0016-slack-over-the-events-api.md) explains the design.

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
reflect = true                  # after a pass, file ideas from what it learned
memory_chars = 16000            # the size of the `memory` block
user_chars = 4000               # the size of the `user` block
```

Both blocks go into every model call, about a token for each four
characters. The engine default of 2,200 characters fills in a day, so
littleguy starts at 16,000 and 4,000. Set any size but 0. A reload
resizes the blocks at once, and a smaller size never drops an entry: a
block keeps at least what it holds.

After each pass that learned something, littleguy reflects once: the
reflection hears the facts that the pass added, and files
[ideas](#goals-and-ideas) for anything worth planning or researching. A
pass that added no fact wakes no reflection, and `recent_wakes` says why.
`pass_episodes = 8` under `[memory]` sets how many episodes one pass reads
([ADR 0030](docs/adr/0030-a-reflection-hears-what-the-pass-did.md)), and
`pass_tokens` can cap the output of its model call. By default a pass has
no cap of its own, so the limit of the provider applies. The engine fixes
the cap at 2048, which a model that reasons fills before it writes the
reply ([ADR 0034](docs/adr/0034-no-arbitrary-caps-on-model-work.md)). The reflection is read-only, and it stays silent unless
you should know something. [ADR 0022](docs/adr/0022-ideas-from-consolidation.md)
explains the design. The reflection comes with the `reflection` feature,
which is on by default and needs `plans`.

The notes and the log live in `memory.db`, next to `lg.db`, or in the
tenant database of a [fleet](#fleet). The pass runs
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
files = true                    # read_file, write_file, list_files, delete_file, search_notes, grep_files
runs = true                     # recent_runs, failed_attempts, model_usage
plans = true                    # goals and ideas; see below
fetch_max_chars = 20000         # the most text of one page
```

- `web_fetch` resolves the host first and refuses a private, loopback,
  link-local or reserved address, also after a redirect. So a page cannot
  aim it at your router or a local service. A proof in `formal/verus`
  covers the IPv4 check. It does not use an HTTP proxy.
- `web_search` uses the [Brave Search API](https://brave.com/search/api/).
  Put the key in `LG_BRAVE_API_KEY`; without it there is no search tool.
- The workspace keeps its files in `workspace/`, next to `lg.db`, and they
  survive a restart. A path stays inside the workspace. `search_notes`
  finds the files that hold some words, so littleguy can recall a note
  whose path it forgot. It only reads, so a heartbeat may use it. It comes
  with the `search-notes` feature, on by default.
- Notes are littleguy's own memory of what it learned: `write_note`,
  `read_note`, `list_notes` (with a query to search) and `delete_note`.
  They live in `memory.db`, not in the workspace, so a heartbeat can write
  them too, where the workspace is read-only. You read them on the
  **Notes** tab of the Workspace page.
  [ADR 0029](docs/adr/0029-notes-that-a-tick-can-write.md) explains the
  design. `grep_files` finds the
  lines that match a pattern, with their paths and line numbers, as `rg`
  does, so littleguy can find where code is defined or used. It comes with
  the `grep-files` feature, on by default.
- `read_file` and `web_fetch` read a long text in windows. Each result
  gives the `length` of the whole text in characters, and an `offset`
  starts a window further in. An offset past the end gives an empty text
  and the length.
- `runs` lets the assistant debug itself: its newest runs and how each
  ended, the model and tool calls that failed and why, and its model
  usage (tokens, cost and latency) over a window. Each tool runs one fixed
  query on `lg.db`, read only, and reads no message. `config_summary`
  shows littleguy the config that the daemon loaded and the features of
  the build, so it can tell a part that is off from one that is broken. It
  is an allow list of fields: no user id, prompt, path, URL or key, only
  whether a key is set.
  [ADR 0019](docs/adr/0019-the-assistant-reads-its-own-runs.md) explains
  the design. These tools and `recent_wakes` come with the `self-debug`
  feature, which is on by default.

## Goals and ideas

littleguy keeps **ideas**, things to plan and research, and **goals**,
things to do. It files an idea when you mention something worth exploring,
or when it notices one, and it adds research notes to it. When an idea is
worth doing, it proposes a goal. A proposed goal does nothing until you
accept it on the **Goals** tab of the Workspace page, as a refine overlay does nothing until
you activate it. An accepted goal can ask for a check every so often, and
littleguy notes its progress and marks it done.

littleguy can also book a **wake** with `wake_me`: "look at the deploy
again in 20 minutes". At that time it wakes on its own, read-only, looks,
and tells you only what you should know. It can book at most 10 wakes in a
row before you write again. `recent_wakes` shows each time it
woke on its own (heartbeats, briefs, reflections and booked wakes) and how
it went: the tools it called and the start of its answer;
[ADR 0025](docs/adr/0025-the-tick-log.md) and [ADR 0028](docs/adr/0028-the-tick-log-says-what-a-tick-did.md). [ADR 0023](docs/adr/0023-wakes-that-the-assistant-books.md)
explains the design. Booked wakes come with the `followups` feature, which
is on by default.

littleguy cannot accept, decline or drop anything; only you can. Every
tool it has works on a run with no one present, so a later heartbeat can
research ideas and check goals on its own. [ADR 0020](docs/adr/0020-goals-and-ideas.md)
explains the design.

Goals and ideas come with the `plans` feature, which is on by default. A
build without it has no goal or idea tools, and the Goals and Ideas tabs
say so. `plans = false` under `[tools]` turns them off in a build that
has them.

The web tools only read, and their results count as untrusted input. The
file tools change only the private workspace. A morning brief can read but
not write. [ADR 0007](docs/adr/0007-first-tools.md) explains the design.


## Skills

Skills are know-how that littleguy loads when a task needs it, in the
format Claude Code and Codex use: a folder per skill with a `SKILL.md`.

```text
~/.config/littleguy/skills/triage-inbox/SKILL.md
---
name: triage-inbox
description: How Mark wants his inbox sorted. Load it before you triage.
---
1. Read the subject lines first. ...
```

```toml
[skills]
dir = "~/notes/skills"    # optional; the default is skills/ next to lg.toml
```

Put a `SKILL.md` in the folder and the running littleguy has it within a
second: its prompt lists each name and description, and it loads a body
when it needs one. littleguy also drafts skills for itself when it finds a
way of doing something that it will need again. A draft waits on the
**Skills** tab of the Workspace page; accepting it writes the `SKILL.md`.
Retiring a skill moves its folder to `.retired/`. `list_plans` shows
littleguy its skills and its drafts. A draft with the name of a skill or
of a waiting draft revises it, and a name close to another one, such as
`read-a-web-page` next to `read-a-page`, is refused unless littleguy says
that the two differ. Skills reach the
assistant through the refine overlay, so they need a `[refine]` section.
[ADR 0024](docs/adr/0024-skills.md) explains the design. Skills come with
the `skills` feature, which is on by default and needs `refine`.

## Council

A `[council]` section lets model calls decide littleguy's proposals for
you, when no outside text could have written them. It suits a daemon that
plans on its own, such as a character at a table:

```toml
[council]
accept = ["goals", "skills"]   # the default
members = 3                    # independent votes; more than half accept
charter = "Zara, a rogue who wants the crown."   # the default is the system prompt
```

When an op ends, each new proposal is marked tainted if the run had read a
web page or another outside text. A tainted proposal waits for you, as
before. On each tick, the council votes on the oldest clean one: an
accepted goal becomes active with a note of why, an accepted draft becomes
a skill by `council`, and a declined one closes. You can still drop a goal
or retire a skill. The council needs the Rule of Two (`[flow]`, on by
default) to see taint. Its votes are model calls outside the run, so they
are not in `model_usage`; `memory.db` logs each decision.
[ADR 0027](docs/adr/0027-a-council-accepts-for-the-owner.md) explains the
design.

## Heartbeat

A `[heartbeat]` section wakes littleguy on its own. On each tick it checks
the goals that are due and researches an open idea, and it tells you only
what you should know. A tick with nothing due costs no model call: a
check in code skips it first.

```toml
[heartbeat]
every_minutes = 60              # the default
quiet = "22:00-08:00"           # no ticks in these hours; optional
timezone = "America/Chicago"    # of `quiet`; the default is the memory time zone
research = true                 # research open ideas when no goal is due
deliver = "web"                 # where news goes: the web chat, for now
# prompt = "..."                # the instruction of each tick
```

A tick runs with no one present. Its own bookkeeping persists: it can
file ideas, add notes to them, propose goals and book wakes. The
workspace is read-only on a tick: `write_file` and `delete_file` are
refused there with "this run may only read data", rather than answering
success for a change that the engine would not keep. It cannot act
outside or write anything that people see. When it has news, the answer waits in the web chat, labelled
"littleguy, on its own". The heartbeat and the morning brief share the
engine timer, and a brief slot always comes first.
[ADR 0021](docs/adr/0021-the-heartbeat.md) explains the design.

The heartbeat comes with the `heartbeat` feature, which is on by default
and needs `plans`. A build without it refuses a `[heartbeat]` section at
start and at a reload.

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

To stop approving code that only your own words led to:

```toml
[sandbox]
backend = "process"
approve = "untrusted"   # the default is "always"
```

Then a call runs at once while the conversation holds only your words.
Once the assistant reads outside text, such as a web page, a mail, an API
answer or a note, each later call waits again. The wait lasts while that
read is in the conversation: until it leaves the `window`, or until you
start a new conversation with `lg new` or the New conversation button of
the chat page. Memory, notes and the workspace stay. An MCP server counts
only with `labels = ["untrusted_input"]`, and a workspace file read with
`read_file` does not count. Code in
a run cannot use the socket of `lg` or change the config
([ADR 0032](docs/adr/0032-code-in-the-sandbox-cannot-act-as-the-owner.md)),
but it can still read and write the databases.
[ADR 0033](docs/adr/0033-code-runs-unasked-until-outside-text.md) and
[ADR 0036](docs/adr/0036-the-mark-of-outside-text-belongs-to-the-conversation.md)
explain the design.

## Run it in a container

The `process` sandbox backend runs only on Linux. On a Mac, the container
in this repository runs littleguy on Linux, so `run_code` works, and the
container is the boundary of the code it runs. The same image suits a
Linux host such as a Fly Sprite.

```sh
docker compose up -d --build                   # LG_MODEL picks the model, default offline
docker compose cp lg.toml lg:/data/config/littleguy/lg.toml
docker compose exec lg lg reload
docker compose exec lg lg ui                   # a sign-in link for the pages
docker compose exec lg lg chat "hi"
```

- **Keys.** Compose passes `OPENROUTER_API_KEY`, `ANTHROPIC_API_KEY`,
  `OPENAI_API_KEY`, `LG_BRAVE_API_KEY` and the Discord and Slack tokens from
  your shell by name, and `LG_MODEL` and `LG_MODEL_NAME` pick the model
  (`LG_REASONING` sets the reasoning effort):
  `LG_MODEL=openrouter LG_MODEL_NAME=deepseek/deepseek-v4.1-flash-1017
  docker compose up -d`. No file holds a key.
- **Data.** The config, `lg.db`, `memory.db`, the skills and the workspace
  live in the `lg-data` volume, under `/data`. SQLite does not lock well
  across the file sharing of Docker Desktop, so the data stays in the volume
  and not in a folder of the Mac. Only one daemon may run on one database.
  To move a littleguy in, stop `lg serve` on the Mac first, and copy before
  the container first starts, so it never opens an empty database:

  ```sh
  docker compose build && docker compose create
  # Your own config, or the example to start from: lg.toml.example
  docker compose cp ~/.config/littleguy/lg.toml lg:/data/config/littleguy/lg.toml
  # macOS tar adds Apple attributes that the volume refuses, so leave them out.
  tar --no-xattrs --no-mac-metadata --no-acls -C ~/.local/share/littleguy \
    --exclude lg.db.lock --exclude lg.sock -cf - . \
    | docker cp - "$(docker compose ps -aq lg)":/data/share/littleguy
  # The skills live next to the config, so they come over too.
  tar --no-xattrs --no-mac-metadata --no-acls --exclude .DS_Store \
    -C ~/.config/littleguy -cf - skills \
    | docker cp - "$(docker compose ps -aq lg)":/data/config/littleguy
  # The data and the skills belong to lg. The config stays root's, so code
  # in the sandbox cannot rewrite it (ADR 0032).
  docker compose run --rm --no-deps --user root --entrypoint sh lg -c \
    'chown -R lg:lg /data/share /data/config/littleguy/skills; chown root:root /data/config/littleguy /data/config/littleguy/lg.toml'
  docker compose up -d
  ```

  A volume made before ADR 0032 has a config folder that `lg` owns. The same
  `docker compose run` line fixes it. Later config changes go in with
  `docker compose cp`, which writes as root.
- **Pages.** They serve on the loopback address of the container. A small
  `pages` service shares its network and forwards to them, and compose
  publishes it on `127.0.0.1:3004` of the Mac only. So the pages stay on
  this machine, and the sign-in link from `lg ui` works in your browser.
  `LG_PAGES_PORT` picks another port, for example while `lg serve` still
  runs on the Mac; set the same port in `[ui] bind`.
- **The CLI.** A Unix socket does not cross the file sharing of Docker
  Desktop, so `lg chat`, `lg approve` and the rest run inside the container
  with `docker compose exec lg lg ...`.
- **Code.** Add `[sandbox]` with `backend = "process"` and restart the
  container: the sandbox starts only at start, so `lg reload` does not add
  it. Each call still waits for your approval. The sandbox runs each call
  in a new user namespace, and the default seccomp profile of Docker blocks
  that, so `compose.yaml` turns seccomp off for the `lg` service and sets
  `no-new-privileges`. The code still runs as the `lg` user with no
  capabilities, and with no network unless `network = true`. The container
  is the boundary of the code. `cargo xtask container` checks that the
  image can run code.

On a host that sleeps, such as a Sprite, timers do not fire while it
sleeps, so the heartbeat, the brief and booked wakes need a cron outside
that calls `/hooks/tick` (see [Hooks](#hooks)).

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

San Mateo knows about 35 providers. To add one, write it in a
`providers.yaml` in the [Nango format](https://github.com/NangoHQ/nango/blob/master/packages/providers/providers.yaml)
and name the file. Its providers join the built-in ones, and one with the
same name replaces the built-in one:

```toml
[san_mateo]
catalog = "/etc/littleguy/providers.yaml"
```

The catalog decides where a token can go, so only you write it.

A provider that takes an API key, such as `openai` or `airtable-pat`,
needs no client id or secret in the config:

```toml
[[san_mateo.integration]]
key = "airtable"
provider = "airtable-pat"
```

For such a provider, `lg connect airtable` prints a link to the connect
page of San Mateo. Type the key there, in a browser on this machine. The
key goes from the page to San Mateo, which seals it like a token, so it
is never in the config, the environment or a shell history. A plain OAuth
provider skips the page and goes straight to its sign-in.

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

## Any API

Feature `api`, off by default. It turns `oauth` on.

With `[api]`, the assistant calls the API of any connected San Mateo
integration. It learns the API itself, and it keeps what it learns in a
skill:

```toml
[[san_mateo.integration]]
key = "github"
provider = "github"
client_id = "Iv1.0123456789abcdef"
client_secret_env = "LG_GITHUB_CLIENT_SECRET"

[api]
integrations = ["github"]   # leave it out for every integration
max_chars = 20000           # the most characters of one answer
```

- `api_get` sends a `GET` and runs at once. Its answers count as
  untrusted input and private data.
- `api_send` sends a `POST`, `PUT`, `PATCH` or `DELETE`. Each call waits
  for you, and you see the method, the path and the body first. A tick
  never sends one.

The model names a path, never a host. San Mateo sends the call to the base
URL of the provider in its catalog and adds the token, so no tool sees a
token and no call can carry one elsewhere.
[ADR 0031](docs/adr/0031-api-calls-through-san-mateo.md) explains the
design.

## Web pages

`lg serve` is an autumn-web app. The home page is the chat: the
conversation so far, with each answer rendered as Markdown by
[markstream](https://crates.io/crates/autumn-plugin-markstream). Enter
sends a message, and Shift+Enter adds a line. New conversation starts the
thread over, as `lg new` does. An answer streams in as the
model writes it, over Server-Sent Events, and the whole answer takes its
place at the end. Claude, OpenAI and OpenRouter stream. The workspace page
lists the files that the assistant keeps with its file tools, and opens
each one: Markdown renders, and other text shows as it is. The manage page shows the
calls that wait for approval, who can talk to the assistant, the refine
overlays and the OAuth connections. An Approve or Deny button answers at
once: the run goes on behind it, and its answer shows in the section when
the next model turn ends. The binary holds every script and style. The pages serve on a loopback address only:

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
With `SLACK_SIGNING_SECRET`, Slack calls its own two hooks, and the Slack
signature guards them.
[ADR 0015](docs/adr/0015-hooks-that-wake-the-host.md) explains the design.

## Fleet

A host can lose its volume. With the `fleet` feature and a `[fleet]`
section, the memory, the episode log and the approvals live in a tenant
database, `<owner>.db`, and the replicator of autumn-web ships its WAL to a
replica:

```toml
[fleet]
root = "/data/fleet"       # the default is `fleet` next to the database
restore_missing = true     # a new volume restores the file from the replica
rpo_secs = 30              # the default; it ships every 15 s

[fleet.replica.s3]         # or: [fleet.replica] path = "/mnt/replica"
bucket = "littleguy"
region = "auto"
endpoint = "https://<account>.r2.cloudflarestorage.com"
force_path_style = true
# access_key_id_env = "LG_REPLICA_ACCESS_KEY_ID"          # the defaults
# secret_access_key_env = "LG_REPLICA_SECRET_ACCESS_KEY"
```

Any S3-compatible store fits: R2, S3 or Tigris. The first start moves the
rows of `memory.db` into the tenant database and renames the old file
`memory.db.imported`. Serve one owner from one host only: nothing fences a
second host yet. The refine overlays, the OAuth connections and `lg.db`
stay on the volume for now.
[ADR 0017](docs/adr/0017-the-tenant-database-on-a-fleet.md) explains the
design.

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

`lg serve` reads `<config home>/littleguy/lg.toml` when it exists. Start
from [`lg.toml.example`](lg.toml.example): it has every section, with
each one explained, the defaults on and the rest commented out. Delete the
`# ` in front of a setting to turn it on.

```sh
mkdir -p ~/.config/littleguy
cp lg.toml.example ~/.config/littleguy/lg.toml
```

A test parses the example as it ships and with every setting turned on,
so it always matches what `lg serve` reads. The essentials:

```toml
owner = "mark"
system = "You are littleguy, a small and helpful personal assistant."
window = 40
# max_steps = 50                      # tool rounds per message or tick; no bound by default

[discord]
pairing = true                        # strangers get a code; false drops them
allowed = ["123456789012345678"]      # Discord user ids allowed from the start

[[camel.route]]
id = "inbox"
from = "file:/home/mark/lg-inbox?delete=true"
```

By default a message or a tick runs until the model answers, however many
tool rounds it takes. Set `max_steps` to bound it: an op that needs more
rounds stops and says so. Each round is a model call. A tool result is cut
only by the tool's own setting, such as `fetch_max_chars`, not by the
engine ([ADR 0034](docs/adr/0034-no-arbitrary-caps-on-model-work.md)).

| Flag | Variable | Default |
|---|---|---|
| `--socket` | `LG_SOCKET` | `<data home>/littleguy/lg.sock` |
| `--db` | `LG_DB` | `<data home>/littleguy/lg.db` |
| `--config` | `LG_CONFIG` | `<config home>/littleguy/lg.toml` |
| none | `DISCORD_BOT_TOKEN` | unset: no Discord |
| none | `SLACK_BOT_TOKEN`, `SLACK_APP_TOKEN` | unset: no Slack |
| none | `SLACK_SIGNING_SECRET` | unset: Slack over Socket Mode, if at all |
| none | `LG_SAN_MATEO_KEY` | required with `[san_mateo]` |
| none | `LG_HOOK_TOKEN` | required with `[hooks]` |
| none | `LG_REPLICA_ACCESS_KEY_ID`, `LG_REPLICA_SECRET_ACCESS_KEY` | required with `[fleet.replica.s3]` |

`<data home>` is `$XDG_DATA_HOME`, or `~/.local/share`. `<config home>` is
`$XDG_CONFIG_HOME`, or `~/.config`.

### Reload

The daemon watches the config file and reads it again within 2 s of a
save. `lg reload` does the same at once and prints what changed:

```sh
lg reload
# applied: camel, discord
# at the next start of `lg serve`: ui
```

A new camel route starts, and a removed one stops. The chat allow lists,
the memory and brief schedules, the approvals, the flow rules and the
refine policy apply at once. `owner`, `san_mateo`, `google`, `sandbox`,
`mcp`, `ui`, `hooks` and `fleet` wait for the next start. A file that does
not parse, or a route that does not build, changes nothing.

`system`, `window` and `tools` are stored, but the running assistant keeps
the profile it started with. autumn-harvest reads the profile once per run
(autumn-harvest #2658). Use `lg refine` to change the prompt live.
[ADR 0018](docs/adr/0018-reload-the-config.md) explains the design.

## Who can talk to it

The socket has mode 0600. The daemon also reads the peer credentials of each
connection. Only the user who runs the daemon reaches the assistant from the
socket, and only that user may pair, list or unpair.

A `reply` on the socket carries the `usage` of its op: the model calls that
its drive recorded in the cost ledger, their tokens, the cost of the priced
calls in millionths of a dollar, and the count of unpriced calls. A program
that drives the assistant over the socket can cap its spend with it. A
`duplicate` ran nothing, so it has no usage. A reply after an approval spans
two drives, so it leaves the usage out.

A `chat_stream` request is a `chat` that also gets the answer while the model
writes it: a `{"kind": "delta", "text": ...}` line for each piece, and a
`{"kind": "restart"}` line when a new model call starts, then the response
line. The pieces are a preview, as on the web page. The response line is the
answer that counts. A plain `chat` still gets one line.

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
