//! The name kernel of a saved script (ADR 0045): which names the assistant
//! may give a script.
//!
//! Each function mirrors `littleguy-core/src/script_name.rs`. The bodies
//! between the mirror markers must match the Rust source token for token.

use vstd::prelude::*;

verus! {

/// `b` may appear in a name: `a`-`z`, `0`-`9` or `-`.
pub open spec fn name_byte(b: u8) -> bool {
    (97 <= b && b <= 122) || (48 <= b && b <= 57) || b == 45
}

/// What a script name is, read plainly.
pub open spec fn script_name(p: Seq<u8>) -> bool {
    1 <= p.len() <= 64 && p[0] != 45 && (forall|i: int| 0 <= i < p.len() ==> #[trigger] name_byte(p[i]))
}

// mirror: crates/littleguy-core/src/script_name.rs is_name_byte
fn is_name_byte(b: u8) -> (r: bool)
    ensures
        r <==> name_byte(b),
{
    // mirror-begin is_name_byte
    (97 <= b && b <= 122) || (48 <= b && b <= 57) || b == 45
    // mirror-end is_name_byte
}

// mirror: crates/littleguy-core/src/script_name.rs all_name_bytes
fn all_name_bytes(p: &[u8], i: usize) -> (r: bool)
    requires
        p@.len() < usize::MAX,
    ensures
        r <==> (forall|j: int| i <= j < p@.len() ==> #[trigger] name_byte(p@[j])),
    decreases p@.len() - i,
{
    // mirror-begin all_name_bytes
    i >= p.len() || (is_name_byte(p[i]) && all_name_bytes(p, i + 1))
    // mirror-end all_name_bytes
}

// mirror: crates/littleguy-core/src/script_name.rs is_script_name_bytes
pub fn is_script_name_bytes(p: &[u8]) -> (r: bool)
    requires
        p@.len() < usize::MAX,
    ensures
        // The kernel is exactly the plain reading of a script name.
        r <==> script_name(p@),
        // So an accepted name holds no `/`, which would leave the folder,
        r ==> (forall|i: int| 0 <= i < p@.len() ==> #[trigger] p@[i] != 47),
        // no `.`, so it is never hidden and never `..`,
        r ==> (forall|i: int| 0 <= i < p@.len() ==> #[trigger] p@[i] != 46),
        // no `\` and no control byte,
        r ==> (forall|i: int| 0 <= i < p@.len() ==> #[trigger] p@[i] >= 32 && p@[i] != 92),
        // and never starts as a flag.
        r ==> p@[0] != 45,
{
    let r =
    // mirror-begin is_script_name_bytes
    !p.is_empty() && p.len() <= 64 && p[0] != 45 && all_name_bytes(p, 0)
    // mirror-end is_script_name_bytes
    ;
    proof {
        if r {
            assert forall|i: int| 0 <= i < p@.len() implies #[trigger] p@[i] != 47 by {
                assert(name_byte(p@[i]));
            }
            assert forall|i: int| 0 <= i < p@.len() implies #[trigger] p@[i] != 46 by {
                assert(name_byte(p@[i]));
            }
            assert forall|i: int| 0 <= i < p@.len() implies #[trigger] p@[i] >= 32 && p@[i] != 92 by {
                assert(name_byte(p@[i]));
            }
        }
    }
    r
}

} // verus!
