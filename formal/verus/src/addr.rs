//! The address kernel: a web tool reaches public IPv4 addresses only.
//!
//! `is_public_v4` mirrors `littleguy-core/src/addr.rs`. The body between
//! the mirror markers must match the Rust source token for token.

use vstd::prelude::*;

verus! {

/// Whether `a` is in the network at `base` of `size` addresses.
pub open spec fn in_net(a: u32, base: int, size: int) -> bool {
    base <= a as int && (a as int) < base + size
}

/// The blocked ranges, by network and size: `/8` is `0x0100_0000`.
pub open spec fn blocked(a: u32) -> bool {
    in_net(a, 0x0000_0000, 0x0100_0000)        // this network, /8
    || in_net(a, 0x0A00_0000, 0x0100_0000)     // private, /8
    || in_net(a, 0x6440_0000, 0x0040_0000)     // shared address space, /10
    || in_net(a, 0x7F00_0000, 0x0100_0000)     // loopback, /8
    || in_net(a, 0xA9FE_0000, 0x0001_0000)     // link-local and cloud metadata, /16
    || in_net(a, 0xAC10_0000, 0x0010_0000)     // private, /12
    || in_net(a, 0xC000_0000, 0x0100)          // protocol assignments, /24
    || in_net(a, 0xC000_0200, 0x0100)          // documentation, /24
    || in_net(a, 0xC0A8_0000, 0x0001_0000)     // private, /16
    || in_net(a, 0xC612_0000, 0x0002_0000)     // benchmarking, /15
    || in_net(a, 0xC633_6400, 0x0100)          // documentation, /24
    || in_net(a, 0xCB00_7100, 0x0100)          // documentation, /24
    || in_net(a, 0xE000_0000, 0x2000_0000)     // multicast, reserved, broadcast, /3
}

// mirror: crates/littleguy-core/src/addr.rs is_public_v4
pub fn is_public_v4(a: u32) -> (r: bool)
    ensures
        // An address is public exactly when no blocked range holds it.
        r <==> !blocked(a),
{
    // mirror-begin
    !(a <= 0x00FF_FFFF
        || (0x0A00_0000 <= a && a <= 0x0AFF_FFFF)
        || (0x6440_0000 <= a && a <= 0x647F_FFFF)
        || (0x7F00_0000 <= a && a <= 0x7FFF_FFFF)
        || (0xA9FE_0000 <= a && a <= 0xA9FE_FFFF)
        || (0xAC10_0000 <= a && a <= 0xAC1F_FFFF)
        || (0xC000_0000 <= a && a <= 0xC000_00FF)
        || (0xC000_0200 <= a && a <= 0xC000_02FF)
        || (0xC0A8_0000 <= a && a <= 0xC0A8_FFFF)
        || (0xC612_0000 <= a && a <= 0xC613_FFFF)
        || (0xC633_6400 <= a && a <= 0xC633_64FF)
        || (0xCB00_7100 <= a && a <= 0xCB00_71FF)
        || 0xE000_0000 <= a)
    // mirror-end
}

} // verus!
