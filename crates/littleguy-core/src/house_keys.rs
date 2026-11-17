//! The rules of the house keys (ADR 0038).
//!
//! A house key is a secret that littleguy sends itself, such as a model
//! key or the search key. The binary seals each one with the key ring of
//! `LG_SAN_MATEO_KEY` and keeps it in `house-keys.json`. This module holds
//! the two pure rules of that store, with no I/O:
//!
//! - [`is_sealable`] says which names the store takes. A name is a
//!   variable name, and no name sets up the ring itself.
//! - [`source`] says where the daemon reads a secret: a sealed value wins
//!   over the environment.
//!
//! `formal/verus/src/house_keys.rs` proves both kernels. The test
//! `verus_mirror` fails when a mirrored body drifts from its proof.

/// The prefix of the variables that set up the key ring. The ring never
/// seals its own key, so no name with this prefix is sealable.
pub const RESERVED_PREFIX: &str = "LG_SAN_MATEO_KEY";

/// The longest sealable name, in bytes.
pub const MAX_NAME: usize = 64;

/// Where the daemon reads a secret.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The sealed store holds the name.
    Sealed,
    /// Only the environment holds the name.
    Env,
    /// Neither one holds the name.
    Missing,
}

/// Where a secret comes from, given whether the sealed store holds it and
/// whether the environment holds it. A sealed value wins, so a variable
/// that the owner forgot to unset cannot shadow a sealed key.
#[must_use]
pub const fn source(sealed: bool, env: bool) -> Source {
    // mirror-begin source
    if sealed {
        Source::Sealed
    } else if env {
        Source::Env
    } else {
        Source::Missing
    }
    // mirror-end source
}

/// Whether the store takes `name`: 1 to [`MAX_NAME`] bytes of `[A-Z0-9_]`,
/// no digit first, and no [`RESERVED_PREFIX`].
#[must_use]
pub const fn is_sealable(name: &str) -> bool {
    is_sealable_bytes(name.as_bytes())
}

/// Whether `b` is an ASCII digit.
const fn is_digit(b: u8) -> bool {
    // mirror-begin is_digit
    b'0' <= b && b <= b'9'
    // mirror-end is_digit
}

/// Whether `b` is a byte of a variable name: `A-Z`, `0-9` or `_`.
const fn is_name_byte(b: u8) -> bool {
    // mirror-begin is_name_byte
    (b'A' <= b && b <= b'Z') || is_digit(b) || b == b'_'
    // mirror-end is_name_byte
}

/// Whether each byte of `name` from `i` on is a name byte. The recursion
/// is as deep as the name is long, and the caller bounds the length first.
const fn all_name_bytes(name: &[u8], i: usize) -> bool {
    // mirror-begin all_name_bytes
    i >= name.len() || (is_name_byte(name[i]) && all_name_bytes(name, i + 1))
    // mirror-end all_name_bytes
}

/// Whether `name` starts with [`RESERVED_PREFIX`]. The bytes are spelled
/// out, so the proof reads them without a loop.
const fn has_reserved_prefix(name: &[u8]) -> bool {
    // mirror-begin has_reserved_prefix
    name.len() >= 16
        && name[0] == b'L'
        && name[1] == b'G'
        && name[2] == b'_'
        && name[3] == b'S'
        && name[4] == b'A'
        && name[5] == b'N'
        && name[6] == b'_'
        && name[7] == b'M'
        && name[8] == b'A'
        && name[9] == b'T'
        && name[10] == b'E'
        && name[11] == b'O'
        && name[12] == b'_'
        && name[13] == b'K'
        && name[14] == b'E'
        && name[15] == b'Y'
    // mirror-end has_reserved_prefix
}

/// [`is_sealable`] on the bytes of a name.
#[must_use]
pub const fn is_sealable_bytes(name: &[u8]) -> bool {
    // mirror-begin is_sealable_bytes
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !is_digit(name[0])
        && all_name_bytes(name, 0)
        && !has_reserved_prefix(name)
    // mirror-end is_sealable_bytes
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn a_sealed_value_wins_over_the_environment() {
        assert_eq!(source(true, true), Source::Sealed);
        assert_eq!(source(true, false), Source::Sealed);
        assert_eq!(source(false, true), Source::Env);
        assert_eq!(source(false, false), Source::Missing);
    }

    #[test]
    fn the_house_keys_are_sealable() {
        for name in [
            "ANTHROPIC_API_KEY",
            "OPENAI_API_KEY",
            "OPENROUTER_API_KEY",
            "LG_BRAVE_API_KEY",
            "DISCORD_BOT_TOKEN",
            "SLACK_SIGNING_SECRET",
            "_X",
            "A",
            "LG_SAN_MATEO",
            "LG_SAN_MATEO_KE",
            "LG_SAN_MATEO_OTHER",
        ] {
            assert!(is_sealable(name), "{name}");
        }
        assert!(is_sealable(&"A".repeat(MAX_NAME)));
    }

    #[test]
    fn the_ring_variables_are_not_sealable() {
        for name in [
            "LG_SAN_MATEO_KEY",
            "LG_SAN_MATEO_KEY_ID",
            "LG_SAN_MATEO_KEY_RETIRED",
            "LG_SAN_MATEO_KEYS",
        ] {
            assert!(!is_sealable(name), "{name}");
        }
    }

    #[test]
    fn a_name_is_a_variable_name() {
        for name in [
            "",
            "1PASSWORD",
            "lower_case",
            "HAS SPACE",
            "HAS-DASH",
            "DOT.TED",
            "NUL\0",
            "\u{c9}T\u{c9}",
        ] {
            assert!(!is_sealable(name), "{name:?}");
        }
        assert!(!is_sealable(&"A".repeat(MAX_NAME + 1)));
    }

    #[test]
    fn the_spelled_prefix_matches_the_constant() {
        assert!(has_reserved_prefix(RESERVED_PREFIX.as_bytes()));
        assert_eq!(RESERVED_PREFIX.len(), 16);
    }

    proptest! {
        #[test]
        fn sealable_matches_its_plain_reading(name in "[A-Za-z0-9_ .-]{0,70}") {
            let plain = !name.is_empty()
                && name.len() <= MAX_NAME
                && !name.as_bytes()[0].is_ascii_digit()
                && name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
                && !name.starts_with(RESERVED_PREFIX);
            prop_assert_eq!(is_sealable(&name), plain);
        }

        #[test]
        fn no_name_with_the_prefix_is_sealable(rest in "[A-Z0-9_]{0,48}") {
            let name = format!("{RESERVED_PREFIX}{rest}");
            prop_assert!(!is_sealable(&name));
        }
    }
}
