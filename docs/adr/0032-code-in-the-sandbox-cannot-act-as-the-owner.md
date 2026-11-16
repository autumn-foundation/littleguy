# ADR 0032: Code in the sandbox cannot act as the owner

## Status

Accepted, 2026-11-15.

## Context

The `process` sandbox backend runs each `run_code` call as a child process
of the daemon, with the uid of the daemon. When the daemon is not root,
each call gets a new user namespace, and a network namespace when the code
gets no network. It gets no mount namespace, so the code sees the files of
the daemon (autumn-harvest#2697).

We checked what code in a run can reach in the container of `compose.yaml`:

- It cannot read `/proc/<daemon>/environ`, so the API keys stay out of reach.
- It can connect to `lg.sock`. The socket checks only the uid of its
  peer, and the uid is the daemon's, so the code counts as the owner. It
  could approve a call that waits, pair a sender, or change a skill.
  A Unix socket works from inside a network namespace.
- It can write `lg.toml`. The daemon watches the file and applies a change
  live, so the code could rewrite the rules that hold it.
- It can read and write the databases and the skills folder.

Today the owner reads each call before it runs, which bounds all of this.
A call that runs with no approval (ADR 0033) needs the first two closed.

## Decision

- **The socket refuses a caller from a sandbox.** A peer with the uid of
  the daemon must be in the user namespace of the daemon, by
  `/proc/<pid>/ns/user`. A peer in another namespace, or one that cannot be
  checked, gets an error and nothing else. A peer with another uid keeps
  the uid rules, so it is never the owner. On a system with no user
  namespaces there is no process sandbox, and the check passes.
- **The config folder belongs to root in the image.** `lg.toml` and its
  folder are not writable by `lg`. The owner changes the config with
  `docker compose cp`, which writes as root. The skills folder stays
  writable by `lg`, because the daemon writes accepted skills there.
- **`cargo xtask container` checks both.** It starts a daemon in the
  container, and `lg pairs` must answer outside the namespaces of a run
  and be refused inside them. The config folder must not be writable by
  `lg`, and the skills folder must be.

## Consequences

- Code in a run cannot act as the owner, and it cannot change the config.
- Code in a run can still read and write `lg.db`, `memory.db` and the
  skills folder. So it can read the history and memory of the assistant,
  and write a memory fact or a skill that a later run reads. The real fix
  is a mount namespace for each run in the engine (autumn-harvest#2697).
  Until then, ADR 0033 runs a call with no approval only when the run read
  no outside text.
- An existing volume keeps its old modes. The README says how to fix
  them.
