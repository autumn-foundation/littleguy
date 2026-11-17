# ADR 0031: API calls through San Mateo

## Status

Accepted, 2026-11-15.

## Context

San Mateo keeps the OAuth connections of the owner (`[san_mateo]`, feature
`oauth`). Its proxy sends a call to the provider of a connection and adds
the credential, so no token reaches a tool, a log or the model. Its catalog
is the Nango `providers.yaml` format, with about 35 built-in providers.

Each provider still needs code in littleguy. The Google tools
(`[google]`) are one wrapper on that proxy, with a fixed set of calls. The
owner wants the assistant to use any connected provider and to work out the
API itself: read its docs, try the calls, and keep what it learns.

The proxy holds two properties that a generic tool needs:

- Without a base URL override, the URL is the base URL of the provider in
  the catalog plus the path. The path cannot change the host.
- The credential goes only to that host. San Mateo refuses internal
  addresses.

So the catalog is what decides where a credential can go. Whoever can
write the catalog can send a token to any host.

## Decision

- **Two tools on the proxy**, under an `[api]` section and the `api`
  feature, which needs `oauth`:

  | Tool | Effect | Labels | Waits for a person |
  |---|---|---|---|
  | `api_get` | read only | untrusted input, private data | no |
  | `api_send` | external | none | always |

  `api_get` sends a `GET`. `api_send` sends a `POST`, `PUT`, `PATCH` or
  `DELETE`, with a JSON body or a form. The split is by method, so a read
  costs no question, and each call that can change something waits for the
  owner. A tick is read-only, so it can call `api_get` and never
  `api_send`.

- **The owner picks the integrations.**

  ```toml
  [api]
  integrations = ["github", "notion"]   # the default is every [[san_mateo.integration]]
  # max_chars = 20000                   # cut each answer here; unset, it comes whole
  ```

  Each name must be a `[[san_mateo.integration]]`. The tool descriptions
  list each integration with its provider and base URL. A call uses the
  newest connection of the owner. With no connection, the tool says
  `lg connect <integration>`.

- **The model names a path, never a host.** The path starts with `/`. It
  holds no scheme, no `//` at its start, no `\`, no `#`, and no space or
  control character. The tools send no base URL override. A header may not
  be `Authorization`, `Proxy-Authorization`, `Cookie`, `Host` or
  `Content-Length`, because San Mateo sets the credential.

- **An answer comes back whole, with its status.** A non-2xx status is an
  answer, not an error, so the model can learn the API from it. The body
  is JSON when it parses, text when it does not, and a byte count when it
  is not text. When the owner sets `max_chars`, a long body is cut there,
  and `cut` says so. Unset, the body comes whole (ADR 0034). A few
  headers come back for paging and rate limits: `Link`, `Location`,
  `Retry-After` and the `X-RateLimit-*` headers.

- **The owner can add providers.** `[san_mateo] catalog = "<path>"` names a
  `providers.yaml` in the Nango format. Its providers join the built-in
  ones, and one with the same name replaces the built-in one. Only the
  owner writes the file. The assistant has no tool that writes it. A file
  that does not read fails `lg serve` at start.

- **The client is a skill.** The assistant keeps what it learns about an
  API in a `SKILL.md`: the endpoints, the parameters, the paging. It
  proposes the skill as usual, and the owner or the council accepts it
  (ADR 0024, ADR 0027). No code is added for each provider.

## Consequences

- A provider in the catalog with a connection needs no new code. GitHub,
  Linear, Notion and the other OAuth providers work once the owner runs
  `lg connect`.
- A provider that authenticates with an API key needs a connect flow for
  that key. `lg connect` gives one: for any provider that is not plain
  OAuth, its link opens the connect page of San Mateo, where the owner
  types the key.
- Some APIs change state on a `GET`. The owner bounds that by the list of
  integrations.
- Answers from an API are untrusted and private. After an `api_get`, the
  Rule of Two holds each later write, and `api_send` waits anyway.
- The Google tools stay. They do work that a raw call does not, such as
  building a MIME mail, and they can become a skill on `api_send` later.
- Code in `run_code` cannot call an API yet. A guest must never hold the
  San Mateo key, so that needs a broker in the daemon, a later change.
