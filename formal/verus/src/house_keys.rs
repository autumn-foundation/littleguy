//! The house key kernels (ADR 0038): which names the sealed store takes,
//! and where the daemon reads a secret.
//!
//! Each function mirrors `littleguy-core/src/house_keys.rs`. The bodies
//! between the mirror markers must match the Rust source token for token.

use vstd::prelude::*;

verus! {

/// The longest sealable name, in bytes.
pub const MAX_NAME: usize = 64;

/// Where the daemon reads a secret.
pub enum Source {
    /// The sealed store holds the name.
    Sealed,
    /// Only the environment holds the name.
    Env,
    /// Neither one holds the name.
    Missing,
}

/// The bytes of `LG_SAN_MATEO_KEY`, the prefix of the ring variables.
pub open spec fn prefix() -> Seq<u8> {
    seq![76u8, 71u8, 95u8, 83u8, 65u8, 78u8, 95u8, 77u8, 65u8, 84u8, 69u8, 79u8, 95u8, 75u8, 69u8, 89u8]
}

/// Whether `s` starts with the ring prefix.
pub open spec fn reserved(s: Seq<u8>) -> bool {
    s.len() >= 16 && s.subrange(0, 16) == prefix()
}

/// Whether `b` is a byte of a variable name.
pub open spec fn name_byte(b: u8) -> bool {
    (65 <= b && b <= 90) || (48 <= b && b <= 57) || b == 95
}

/// What a sealable name is, read plainly.
pub open spec fn sealable(s: Seq<u8>) -> bool {
    1 <= s.len() && s.len() <= 64
        && !(48 <= s[0] && s[0] <= 57)
        && (forall|j: int| 0 <= j < s.len() ==> name_byte(#[trigger] s[j]))
        && !reserved(s)
}

// mirror: crates/littleguy-core/src/house_keys.rs source
pub fn source(sealed: bool, env: bool) -> (r: Source)
    ensures
        // A sealed value always wins.
        sealed <==> r == Source::Sealed,
        // The environment answers only for a name with no sealed value.
        r == Source::Env <==> (!sealed && env),
        // A secret is missing only when neither place holds it.
        r == Source::Missing <==> (!sealed && !env),
{
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

// mirror: crates/littleguy-core/src/house_keys.rs is_digit
fn is_digit(b: u8) -> (r: bool)
    ensures
        r <==> (48 <= b && b <= 57),
{
    // mirror-begin is_digit
    b'0' <= b && b <= b'9'
    // mirror-end is_digit
}

// mirror: crates/littleguy-core/src/house_keys.rs is_name_byte
fn is_name_byte(b: u8) -> (r: bool)
    ensures
        r <==> name_byte(b),
{
    // mirror-begin is_name_byte
    (b'A' <= b && b <= b'Z') || is_digit(b) || b == b'_'
    // mirror-end is_name_byte
}

// mirror: crates/littleguy-core/src/house_keys.rs all_name_bytes
fn all_name_bytes(name: &[u8], i: usize) -> (r: bool)
    ensures
        r <==> (forall|j: int| i <= j < name@.len() ==> name_byte(#[trigger] name@[j])),
    decreases name@.len() - i,
{
    // mirror-begin all_name_bytes
    i >= name.len() || (is_name_byte(name[i]) && all_name_bytes(name, i + 1))
    // mirror-end all_name_bytes
}

// mirror: crates/littleguy-core/src/house_keys.rs has_reserved_prefix
fn has_reserved_prefix(name: &[u8]) -> (r: bool)
    ensures
        r <==> reserved(name@),
{
    proof {
        if name@.len() >= 16 {
            let s = name@.subrange(0, 16);
            assert(s == prefix() <==> (
                s[0] == 76u8 && s[1] == 71u8 && s[2] == 95u8 && s[3] == 83u8
                && s[4] == 65u8 && s[5] == 78u8 && s[6] == 95u8 && s[7] == 77u8
                && s[8] == 65u8 && s[9] == 84u8 && s[10] == 69u8 && s[11] == 79u8
                && s[12] == 95u8 && s[13] == 75u8 && s[14] == 69u8 && s[15] == 89u8
            )) by {
                if s[0] == 76u8 && s[1] == 71u8 && s[2] == 95u8 && s[3] == 83u8
                    && s[4] == 65u8 && s[5] == 78u8 && s[6] == 95u8 && s[7] == 77u8
                    && s[8] == 65u8 && s[9] == 84u8 && s[10] == 69u8 && s[11] == 79u8
                    && s[12] == 95u8 && s[13] == 75u8 && s[14] == 69u8 && s[15] == 89u8 {
                    assert(s =~= prefix());
                }
            }
        }
    }
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

// mirror: crates/littleguy-core/src/house_keys.rs is_sealable_bytes
pub fn is_sealable_bytes(name: &[u8]) -> (r: bool)
    ensures
        // The kernel is exactly the plain reading of a sealable name.
        r <==> sealable(name@),
        // So no variable that sets up the ring can ever be sealed.
        r ==> !reserved(name@),
{
    // mirror-begin is_sealable_bytes
    !name.is_empty()
        && name.len() <= MAX_NAME
        && !is_digit(name[0])
        && all_name_bytes(name, 0)
        && !has_reserved_prefix(name)
    // mirror-end is_sealable_bytes
}

/// The three ring variables are not sealable.
proof fn lemma_ring_variables_are_reserved(s: Seq<u8>)
    requires
        s.len() >= 16,
        s.subrange(0, 16) == prefix(),
    ensures
        !sealable(s),
{
}

} // verus!
