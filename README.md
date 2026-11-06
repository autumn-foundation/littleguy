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

Milestone 0 ships the offline echo model only, so it needs no key and no
network. The owner is `$USER` unless a config file names one.

## Config

`lg serve` reads `<config home>/littleguy/lg.toml` when it exists:

```toml
owner = "mark"
system = "You are littleguy, a small and helpful personal assistant."
window = 40
```

| Flag | Variable | Default |
|---|---|---|
| `--socket` | `LG_SOCKET` | `<data home>/littleguy/lg.sock` |
| `--db` | `LG_DB` | `<data home>/littleguy/lg.db` |
| `--config` | `LG_CONFIG` | `<config home>/littleguy/lg.toml` |

`<data home>` is `$XDG_DATA_HOME`, or `~/.local/share`. `<config home>` is
`$XDG_CONFIG_HOME`, or `~/.config`.

## Who can talk to it

The socket has mode 0600. The daemon also reads the peer credentials of each
connection. Only the user who runs the daemon reaches the assistant. The
routing kernel makes that decision, and `formal/verus/src/route.rs` proves
it.

## Layout

| Path | What it holds |
|---|---|
| `crates/littleguy-core` | Ids, config and the routing kernel. No I/O. |
| `crates/littleguy-models` | `AgentModel` implementations. The offline model today. |
| `crates/littleguy` | The `lg` binary: `serve` and `chat`. |
| `formal/verus` | Verus proofs of the kernels. |
| `xtask` | Every CI gate: `cargo xtask ci`. |

[`docs/roadmap.md`](docs/roadmap.md) lists the milestones.
