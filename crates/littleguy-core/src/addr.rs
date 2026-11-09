//! Which network addresses a web tool may reach.
//!
//! A web tool runs on the machine of the owner. A page or a model can name
//! any URL, so a tool that fetches it could read the router, a cloud
//! metadata service or a local daemon. Each fetch therefore resolves the
//! host first, and refuses it when one address is not public.
//!
//! [`is_public_v4`] is the kernel. `formal/verus/src/addr.rs` proves that it
//! refuses every address of each blocked range, and allows every other one.

use std::net::{IpAddr, Ipv6Addr};

/// Whether the IPv4 address `a`, as a big-endian number, is public.
///
/// It refuses this-network (`0/8`), private (`10/8`, `172.16/12`,
/// `192.168/16`), shared (`100.64/10`), loopback (`127/8`), link-local
/// (`169.254/16`), the protocol block (`192.0.0/24`), the documentation and
/// benchmark blocks, and multicast, reserved and broadcast (`224/3`).
#[must_use]
pub const fn is_public_v4(a: u32) -> bool {
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

/// Whether `ip` is a public address.
///
/// An IPv6 address that carries an IPv4 address (mapped, NAT64 or 6to4)
/// is public only when that IPv4 address is. Loopback, unspecified,
/// unique-local, link-local, site-local, multicast, discard, documentation and the old
/// IPv4-compatible block are not public.
#[must_use]
pub fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_public_v4(u32::from(v4)),
        IpAddr::V6(v6) => is_public_v6(v6),
    }
}

fn is_public_v6(v6: Ipv6Addr) -> bool {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return is_public_v4(u32::from(v4));
    }
    let s = v6.segments();
    let low = embedded_v4(s[6], s[7]);
    match s[0] {
        // NAT64 `64:ff9b::/96` carries an IPv4 address in its low bits.
        0x0064 if s[1] == 0xff9b && s[2..6] == [0; 4] => is_public_v4(low),
        // 6to4 `2002::/16` carries one in its second and third groups.
        0x2002 => is_public_v4(embedded_v4(s[1], s[2])),
        // The documentation block `2001:db8::/32`.
        0x2001 if s[1] == 0x0db8 => false,
        // `::/96` holds loopback, unspecified and IPv4-compatible.
        0 if s[1..6] == [0; 5] => false,
        // The discard block `100::/64`.
        0x0100 if s[1..4] == [0; 3] => false,
        // Unique local `fc00::/7`, link-local and the old site-local
        // `fe80::/9`, and multicast `ff00::/8`.
        first => !(first & 0xfe00 == 0xfc00 || first & 0xff80 == 0xfe80 || first >= 0xff00),
    }
}

fn embedded_v4(high: u16, low: u16) -> u32 {
    (u32::from(high) << 16) | u32::from(low)
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use std::net::Ipv4Addr;

    use proptest::prelude::*;

    use super::*;

    fn v4(a: u8, b: u8, c: u8, d: u8) -> u32 {
        u32::from_be_bytes([a, b, c, d])
    }

    fn public(text: &str) -> bool {
        is_public(IpAddr::from_str(text).unwrap())
    }

    #[test]
    fn each_blocked_range_is_refused_at_both_ends() {
        let blocked = [
            ("0.0.0.0", "0.255.255.255"),
            ("10.0.0.0", "10.255.255.255"),
            ("100.64.0.0", "100.127.255.255"),
            ("127.0.0.0", "127.255.255.255"),
            ("169.254.0.0", "169.254.255.255"),
            ("172.16.0.0", "172.31.255.255"),
            ("192.0.0.0", "192.0.0.255"),
            ("192.0.2.0", "192.0.2.255"),
            ("192.168.0.0", "192.168.255.255"),
            ("198.18.0.0", "198.19.255.255"),
            ("198.51.100.0", "198.51.100.255"),
            ("203.0.113.0", "203.0.113.255"),
            ("224.0.0.0", "255.255.255.255"),
        ];
        for (low, high) in blocked {
            assert!(!public(low), "{low}");
            assert!(!public(high), "{high}");
        }
    }

    #[test]
    fn the_neighbours_of_a_blocked_range_are_public() {
        for text in [
            "1.0.0.0",
            "9.255.255.255",
            "11.0.0.0",
            "100.63.255.255",
            "100.128.0.0",
            "172.15.255.255",
            "172.32.0.0",
            "192.0.1.0",
            "192.167.255.255",
            "192.169.0.0",
            "198.17.255.255",
            "198.20.0.0",
            "223.255.255.255",
            "8.8.8.8",
            "93.184.216.34",
        ] {
            assert!(public(text), "{text}");
        }
    }

    #[test]
    fn ipv6_follows_the_carried_ipv4_address() {
        assert!(!public("::1"));
        assert!(!public("::"));
        assert!(!public("::ffff:127.0.0.1"));
        assert!(!public("::ffff:169.254.169.254"));
        assert!(public("::ffff:8.8.8.8"));
        assert!(!public("64:ff9b::a00:1"));
        assert!(public("64:ff9b::808:808"));
        assert!(!public("2002:c0a8:0101::1"));
        assert!(public("2002:0808:0808::1"));
        assert!(!public("::127.0.0.1"));
    }

    #[test]
    fn local_ipv6_blocks_are_refused() {
        for text in [
            "fc00::1",
            "fdff:ffff::1",
            "fe80::1",
            "febf::1",
            "fec0::1",
            "ff02::1",
            "2001:db8::1",
            "100::1",
        ] {
            assert!(!public(text), "{text}");
        }
        for text in ["2606:4700:4700::1111", "2a00:1450:4009::200e", "100:1::1"] {
            assert!(public(text), "{text}");
        }
    }

    /// A plain model of the blocked ranges, by prefix.
    fn blocked_by_prefix(a: u32) -> bool {
        let net = |base: u32, bits: u32| a >> (32 - bits) == base >> (32 - bits);
        net(v4(0, 0, 0, 0), 8)
            || net(v4(10, 0, 0, 0), 8)
            || net(v4(100, 64, 0, 0), 10)
            || net(v4(127, 0, 0, 0), 8)
            || net(v4(169, 254, 0, 0), 16)
            || net(v4(172, 16, 0, 0), 12)
            || net(v4(192, 0, 0, 0), 24)
            || net(v4(192, 0, 2, 0), 24)
            || net(v4(192, 168, 0, 0), 16)
            || net(v4(198, 18, 0, 0), 15)
            || net(v4(198, 51, 100, 0), 24)
            || net(v4(203, 0, 113, 0), 24)
            || net(v4(224, 0, 0, 0), 3)
    }

    proptest! {
        #[test]
        fn the_kernel_matches_the_prefix_model(a in any::<u32>()) {
            prop_assert_eq!(is_public_v4(a), !blocked_by_prefix(a));
        }

        #[test]
        fn a_mapped_address_matches_its_ipv4_address(a in any::<u32>()) {
            let mapped = Ipv4Addr::from(a).to_ipv6_mapped();
            prop_assert_eq!(is_public(IpAddr::V6(mapped)), is_public_v4(a));
        }
    }
}
