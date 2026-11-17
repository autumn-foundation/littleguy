# ADR 0038: House keys sealed with the key ring

## Status

Accepted, 2026-11-17.

## Context

The owner wants every secret of littleguy in one place, with one secret to
guard. ADR 0031 moved the provider credentials and the provider calls into
San Mateo. The rest are the house keys: the keys that littleguy itself
sends.

- The model keys: `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
  `OPENROUTER_API_KEY`.
- The search key `LG_BRAVE_API_KEY`.
- The chat tokens: `DISCORD_BOT_TOKEN`, `SLACK_BOT_TOKEN`,
  `SLACK_APP_TOKEN`, `SLACK_SIGNING_SECRET`.
- The variables that the config names: the hook token (`[hooks]
  token_env`), each client secret (`client_secret_env`), and the MCP
  variables (`bearer_env` and the `env` of a stdio server).

Each one is an environment variable today. So a host keeps N secrets in
its environment, its unit file or its compose file, and each process that
inherits that environment can read all of them.

San Mateo has no home for these keys. Its tables hold integrations,
connections and records. The `custom` field of a connection is plain JSON.
`sm_records` is sealed, but each record belongs to a real connection.

The crypto of San Mateo does not need a San Mateo change.
`san_mateo::crypto::KeyRing` is public and general. It seals with
AES-256-GCM under the active key and binds each value to an AAD. A sealed
value is `sm1.<key id>.<nonce>.<ciphertext>`, and `open` uses the key that
the value names. `with_key` and `activate` rotate the ring. `crypto::aad`
builds an AAD with a length prefix on each part. `Secret` keeps a value
out of a `Debug` line. littleguy already builds a ring from
`LG_SAN_MATEO_KEY` in `oauth.rs`.

Two shapes do not fit:

- **The San Mateo proxy.** A model endpoint streams its answer, and the
  proxy does not stream. Brave takes its key in a header of a plain `GET`,
  so a proxy adds a hop and no protection.
- **A key for `run_code`.** The code in the sandbox cannot act as the owner
  (ADR 0032). The daemon makes each call that needs a key, so no key goes
  into the sandbox. The engine clears the environment of each sandbox
  process, so `LG_SAN_MATEO_KEY` does not reach it either.

## Decision

- **One bootstrap secret.** `LG_SAN_MATEO_KEY` stays the one secret in the
  environment. It builds the ring, and the ring seals every house key.

- **A sealed file beside the database.** `house-keys.json` lives next to
  `lg.db`, with mode `0600`:

  ```json
  {
    "version": 1,
    "keys": {
      "ANTHROPIC_API_KEY": "sm1.k1.<nonce>.<ciphertext>",
      "LG_BRAVE_API_KEY": "sm1.k1.<nonce>.<ciphertext>"
    }
  }
  ```

  Each value is sealed with the AAD `aad(["littleguy/house-key/v1",
  <name>])`. A value moved to another name does not open. A changed byte
  does not open. The file alone opens nothing. A write goes to a temporary
  file, then a rename replaces the file, under an exclusive lock on
  `house-keys.json.lock`. So a reader never sees half a file, and two
  writers never lose a key.

- **A name is a variable name.** A sealable name is 1 to 64 bytes of
  `[A-Z0-9_]`, with no digit first. No name that starts with
  `LG_SAN_MATEO_KEY` is sealable, so the ring can never seal its own key
  or the variables that set it up. `littleguy_core::house_keys` holds the
  rule, and `formal/verus/src/house_keys.rs` proves it.

- **One lookup for every secret.** The daemon reads each secret by its
  variable name through `HouseKeys`. A sealed value wins over the
  environment. A name that has no sealed value comes from the environment,
  as before. Each place that read `std::env::var` for a secret uses the
  lookup instead: the model and the reviewer model, the search key, the
  chat tokens, the hook token, the client secrets and the MCP variables.
  `littleguy_core::house_keys::source` is the precedence kernel, and its
  proof is beside the name rule.

- **A name in both places is a warning.** At start, the log names each
  variable that is sealed and also set in the environment. The sealed
  value runs. The owner removes the variable from the environment.

- **A value that does not open fails the start.** A sealed key whose key
  id is not in the ring, or whose bytes changed, stops `lg serve` and names
  the key and the key id. A silent fallback to the environment would hide
  a lost key. A file with keys and no `LG_SAN_MATEO_KEY` fails the start
  too.

- **The owner writes the file with `lg keys`.** The commands open the file
  directly, so they work while the daemon is down.

  | Command | What it does |
  |---|---|
  | `lg keys set <NAME>` | Seals one line of standard input as `NAME` |
  | `lg keys import [NAME...]` | Seals each named variable of the environment. With no name, each known house key and each variable that the config names |
  | `lg keys list` | Prints each name with the id of its key. Never a value |
  | `lg keys remove <NAME>` | Removes one key |
  | `lg keys reseal` | Seals each key again under the active key |

  A value never comes from an argument, so it never shows in a process
  list or a shell history. The daemon reads the file at start, so a change
  applies at the next start of `lg serve`.

- **Rotation names its key id.** Two more variables set up the ring. They
  exist only to rotate:

  | Variable | Default | Meaning |
  |---|---|---|
  | `LG_SAN_MATEO_KEY_ID` | `k1` | The id of `LG_SAN_MATEO_KEY`, the active key |
  | `LG_SAN_MATEO_KEY_RETIRED` | unset | Retired keys that still open: `id=base64,id=base64` |

  To rotate, the owner moves the old key to `LG_SAN_MATEO_KEY_RETIRED`
  under its id, sets a new key with a new id, and runs `lg keys reseal`.
  `open` reads the id in each value and finds that key in the ring. When
  `lg keys list` shows the new id on every key, the retired key can go.

  San Mateo builds its ring from the same variables, so a rotation reaches
  the OAuth tokens too. San Mateo seals each new or refreshed token under
  the active key. It has no reseal, so a connection that predates the
  rotation still opens with the retired key. Keep the retired key until
  each such connection has refreshed or been connected again. The lookup
  hashes of San Mateo follow the active key. A connect link made before a
  rotation must be made again. The record syncs, which store lookup
  hashes, are off in littleguy.

- **Migration.** A setup with keys in the environment keeps working with
  no change. With no `house-keys.json`, every secret comes from the
  environment, and `LG_SAN_MATEO_KEY` is needed only for `[san_mateo]`.
  To move:

  ```sh
  export LG_SAN_MATEO_KEY="$(openssl rand -base64 32)"   # skip if you have one
  lg keys import          # seals each house key that the environment holds
  lg keys list
  # remove the sealed variables from the environment, then restart lg serve
  ```

- **Feature `sealed-keys`**, on by default (ADR 0026). It needs the
  `san-mateo` crate and no SQLite store or HTTP server. `oauth` needs it,
  because both build the ring the same way. A build without it reads every
  secret from the environment and has no `lg keys`. Such a build refuses
  to start beside a `house-keys.json`, because it would ignore the keys
  that the file holds.

## Consequences

- One secret to guard: `LG_SAN_MATEO_KEY`. A lost key loses every sealed
  house key and every OAuth token. The owner sets each house key again.
- A process that inherits the environment of the daemon, such as an MCP
  server, no longer sees the house keys. An MCP server gets only the
  variables that its `env` names, and the lookup gives those.
- The file holds no plaintext, so a backup or a replica of the data
  folder holds no key that opens without `LG_SAN_MATEO_KEY`.
- A rotation needs a restart and, for San Mateo, a wait for its tokens.
- The daemon keeps the opened keys in memory for its whole run, as it kept
  the environment before. A core dump can still show a key.
- The engine, San Mateo and the sandbox do not change.
