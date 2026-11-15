# ADR 0019: The assistant reads its own runs

## Status

Accepted, 2026-11-14.

## Context

The owner debugs littleguy with littleguy. To answer "why did that fail?"
or "what did today cost?", the assistant needs to see its own runs. The
runtime records them in `lg.db`: each run and how it ended, each failed
attempt of a model call or a tool call, and each model call in the cost
ledger.

`lg.db` also holds the conversation. Run inputs, outputs and history
events carry message text, also the messages of paired senders on Discord
and Slack. The flow rules label data by the tool that read it. A query
tool over the whole file would return that text with no label, so a raw
SQL tool would go around the flow rules.

## Decision

- **Three fixed tools, no SQL.** `recent_runs`, `failed_attempts` and
  `model_usage`. Each runs one fixed query, and its input is only a row
  limit, a filter or a window. The model cannot reach a column outside
  the three queries.
- **The queries read no message.** They read the run id, workflow, state,
  times, attempt, schedule and error of `harvest_executions`; the name,
  attempt and error of the failed rows of `harvest_activity_attempts`;
  and the sums of `harvest_llm_ledger`. They never select an input, an
  output or an event.
- **Error text is untrusted.** An error can quote what failed, for example
  a fetched page. So `recent_runs` and `failed_attempts` carry the
  `untrusted_input` label, as the web tools do, and each error is clipped
  to 500 characters. `model_usage` holds no text and carries no label.
- **Read only, twice.** `littleguy_stores::RunLog` opens the file with
  `SQLITE_OPEN_READ_ONLY` and turns on `query_only`. The runtime stays the
  one writer (ADR 0001). Its single-writer lock is a sidecar file, and the
  file is in WAL mode, so a reader and the writer never block each other.
- **On by default.** `[tools] runs = true`. The tools are `ReadOnly`, and
  the harness offers every tool it has to a run with no mandate, so a
  restart is enough to add or remove them.

## Consequences

- The tools depend on the column names of the runtime schema. A test
  creates a database with `SqliteRuntime::open` and seeds rows, so an
  engine upgrade that renames a column fails the test, not the owner.
- Run inputs stay out of reach. A question such as "what did I say before
  that run failed?" goes through the conversation and memory, not
  through these tools.
