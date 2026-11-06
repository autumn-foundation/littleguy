//! Verus proofs of the littleguy kernels.
//!
//! `cargo xtask verus` verifies this crate. Each proved function mirrors a
//! function in the Rust source. The guard test `verus_mirror` in
//! `littleguy-core` checks that the mirrored bodies match.

use vstd::prelude::*;

pub mod route;

verus! {

fn main() {
}

} // verus!
