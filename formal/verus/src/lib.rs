//! Verus proofs of the littleguy kernels.
//!
//! `cargo xtask verus` verifies this crate. Each proved function mirrors a
//! function in the Rust source. The guard test `verus_mirror` in
//! `littleguy-core` checks that the mirrored bodies match.

use vstd::prelude::*;

pub mod addr;
pub mod compaction;
pub mod house_keys;
pub mod pairing;
pub mod patch_path;
pub mod route;
pub mod script_name;

verus! {

fn main() {
}

} // verus!
