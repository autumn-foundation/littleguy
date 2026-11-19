# Verus proofs

`cargo xtask verus` verifies `src/lib.rs`. Set `VERUS` to a local binary to
skip the download.

| Proof | Mirrors | Property |
|---|---|---|
| `src/addr.rs` `is_public_v4` | `crates/littleguy-core/src/addr.rs` `is_public_v4` | A web tool reaches no private, loopback, link-local or reserved IPv4 address |
| `src/compaction.rs` `engine_percent`, `save_turn_fits` | `crates/littleguy-core/src/compaction.rs` | A compaction setting passes only when the smallest request that makes a reset due, the notice and the notes fit under the request cap, with no overflow |
| `src/house_keys.rs` `is_sealable_bytes`, `source` | `crates/littleguy-core/src/house_keys.rs` | No variable of the key ring is ever sealed, and a sealed value wins over the environment |
| `src/patch_path.rs` `is_patch_path_bytes` | `crates/littleguy-core/src/patch_path.rs` | A patch path is relative, never names a `.` or `..` segment, and never writes under `.github/` or a `.git` folder |
| `src/route.rs` `decide` | `crates/littleguy-core/src/route.rs` `decide` | Only an allowed sender on an enabled channel reaches an agent |

The test `verus_mirror` in `littleguy-core` fails when a mirrored body drifts
from its Rust source.
