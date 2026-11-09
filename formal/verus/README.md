# Verus proofs

`cargo xtask verus` verifies `src/lib.rs`. Set `VERUS` to a local binary to
skip the download.

| Proof | Mirrors | Property |
|---|---|---|
| `src/addr.rs` `is_public_v4` | `crates/littleguy-core/src/addr.rs` `is_public_v4` | A web tool reaches no private, loopback, link-local or reserved IPv4 address |
| `src/route.rs` `decide` | `crates/littleguy-core/src/route.rs` `decide` | Only an allowed sender on an enabled channel reaches an agent |

The test `verus_mirror` in `littleguy-core` fails when a mirrored body drifts
from its Rust source.
