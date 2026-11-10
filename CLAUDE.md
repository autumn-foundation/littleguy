# littleguy: agent instructions

littleguy is an always-on personal agent built on
[autumn-harvest](https://github.com/autumn-foundation/autumn-harvest). The
engine does the durable work: the agent loop, the inbox, timers, memory and
approvals. This repository adds the parts a person touches: channels, models,
tools and one binary, `lg`.

## Workflow

- The default branch is `main`. Open each PR against `main`, ready for review.
- One milestone or issue per PR. Write the spec and the failing test first,
  then the code (SPEC, PROOF, RED, GREEN, REFACTOR).
- Run `cargo xtask ci` before you push. CI runs the same commands.

## Automation

Every gate runs through `cargo xtask`, built on
[smop](https://crates.io/crates/smop). Do not add a shell or Python script.
Add a command to `xtask/src/main.rs`. [`docs/xtask.md`](docs/xtask.md) lists
each command.

## Features

- Each integration is a cargo feature. A new one gets a comment above it
  in `Cargo.toml`, and `cargo xtask feature-docs --write` renders
  `docs/features.md` again. CI fails when the table drifts.
- A config section of a missing feature fails `lg serve` at start
  (`crates/littleguy/src/features.rs`). Never skip it quietly.
- `cargo xtask features` builds each feature alone and none at all. Run it
  when a change touches a `#[cfg(feature = ...)]`.

## Web

- `lg serve` is an autumn-web app (ADR 0014). Build each page with maud and
  htmx on autumn-web. Never add another HTTP server.
- A page sends an owner `Request` through `daemon::Handle`. It never
  touches the runtime or a store itself.
- The pages serve only on a loopback address. A route on a public bind is
  a hook: a bearer token or a signature guards it (ADR 0015).

## Rust rules

- `cargo fmt` and clippy pedantic plus nursery, with warnings denied.
- No `#[allow]` suppression. Fix the code, or use `#[expect(lint, reason = "...")]`
  where the reason is real.
- No `unwrap()` or `expect()` outside tests. Integration test helpers return
  `Result` instead.
- `thiserror` in the library crates, `anyhow` in the binary and xtask only.
- A newtype for every id that crosses a boundary (`littleguy-core::ids`).
- Tokio for all async work.
- Line coverage stays at 85% or more (`cargo xtask coverage`).

## Proofs

`formal/verus` proves the kernels. Each proved function mirrors a Rust
function between `// mirror-begin` and `// mirror-end` markers. The test
`verus_mirror` fails when the two drift. Change both in the same commit.

## Engine facts that bind this repository

- Depend on the published autumn-harvest crates (0.10 today). Work around a
  missing engine piece here, and adopt the engine version at a later upgrade.
- The SQLite runtime is a single writer. It takes `<db>.lock`, so a second
  daemon on one database fails to open it.
- A message id is the idempotency key of its op. Never send an op without one.
- Unattended runs (heartbeats, follow-ups) are read-only by default in the
  engine. Keep that default.

## Comments

Short sentences, active voice, present tense, no contractions. Keep the
reason a comment exists. Split a long explanation into short sentences; do
not delete it.
