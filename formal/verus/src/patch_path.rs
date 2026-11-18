//! The path kernel of a patch (ADR 0042): which repository paths a patch
//! of the assistant may write.
//!
//! Each function mirrors `littleguy-core/src/patch_path.rs`. The bodies
//! between the mirror markers must match the Rust source token for token.

use vstd::prelude::*;

verus! {

/// Whether `b` may not appear in a path: a control byte, DEL or `\`.
pub open spec fn bad_byte(b: u8) -> bool {
    b < 32 || b == 127 || b == 92
}

/// `b` in ASCII lower case.
pub open spec fn low(b: u8) -> u8 {
    if 65 <= b && b <= 90 { (b + 32) as u8 } else { b }
}

/// A segment ends at `i`: the end of the path or a `/`.
pub open spec fn ends(p: Seq<u8>, i: int) -> bool {
    i == p.len() || (0 <= i < p.len() && p[i] == 47)
}

/// A segment starts at `i`.
pub open spec fn starts(p: Seq<u8>, i: int) -> bool {
    i == 0 || (1 <= i <= p.len() && p[i - 1] == 47)
}

/// The segment at `i` is `.` or `..`.
pub open spec fn dot_segment(p: Seq<u8>, i: int) -> bool {
    0 <= i < p.len() && p[i] == 46
        && (ends(p, i + 1) || (i + 1 < p.len() && p[i + 1] == 46 && ends(p, i + 2)))
}

/// The segment at `i` is `.git` in any case.
pub open spec fn git_segment(p: Seq<u8>, i: int) -> bool {
    0 <= i && i + 3 < p.len() && p[i] == 46 && low(p[i + 1]) == 103 && low(p[i + 2]) == 105
        && low(p[i + 3]) == 116 && ends(p, i + 4)
}

/// The `/` at `i` makes an empty segment.
pub open spec fn empty_segment(p: Seq<u8>, i: int) -> bool {
    0 <= i < p.len() && p[i] == 47 && (i == 0 || i + 1 == p.len() || p[i + 1] == 47)
}

/// The byte at `i` keeps the path valid.
pub open spec fn local_ok(p: Seq<u8>, i: int) -> bool {
    !bad_byte(p[i]) && !empty_segment(p, i) && !(starts(p, i) && (dot_segment(p, i)
        || git_segment(p, i)))
}

/// The first segment is `.github` in any case.
pub open spec fn github_first(p: Seq<u8>) -> bool {
    p.len() >= 7 && p[0] == 46 && low(p[1]) == 103 && low(p[2]) == 105 && low(p[3]) == 116
        && low(p[4]) == 104 && low(p[5]) == 117 && low(p[6]) == 98 && ends(p, 7)
}

/// What a patch path is, read plainly.
pub open spec fn patch_path(p: Seq<u8>) -> bool {
    p.len() >= 1 && !github_first(p) && (forall|i: int| 0 <= i < p.len() ==> #[trigger] local_ok(p, i))
}

// mirror: crates/littleguy-core/src/patch_path.rs is_bad_byte
fn is_bad_byte(b: u8) -> (r: bool)
    ensures
        r <==> bad_byte(b),
{
    // mirror-begin is_bad_byte
    b < 32 || b == 127 || b == 92
    // mirror-end is_bad_byte
}

// mirror: crates/littleguy-core/src/patch_path.rs lower
fn lower(b: u8) -> (r: u8)
    ensures
        r == low(b),
{
    // mirror-begin lower
    if 65 <= b && b <= 90 { b + 32 } else { b }
    // mirror-end lower
}

// mirror: crates/littleguy-core/src/patch_path.rs ends_at
fn ends_at(p: &[u8], i: usize) -> (r: bool)
    ensures
        r <==> ends(p@, i as int),
{
    // mirror-begin ends_at
    i == p.len() || (i < p.len() && p[i] == 47)
    // mirror-end ends_at
}

// mirror: crates/littleguy-core/src/patch_path.rs starts_at
fn starts_at(p: &[u8], i: usize) -> (r: bool)
    ensures
        r <==> starts(p@, i as int),
{
    // mirror-begin starts_at
    i == 0 || (i <= p.len() && p[i - 1] == 47)
    // mirror-end starts_at
}

// mirror: crates/littleguy-core/src/patch_path.rs is_dot_segment
fn is_dot_segment(p: &[u8], i: usize) -> (r: bool)
    requires
        p@.len() < usize::MAX,
    ensures
        r <==> dot_segment(p@, i as int),
{
    // mirror-begin is_dot_segment
    i < p.len()
        && p[i] == 46
        && (ends_at(p, i + 1) || (i + 1 < p.len() && p[i + 1] == 46 && ends_at(p, i + 2)))
    // mirror-end is_dot_segment
}

// mirror: crates/littleguy-core/src/patch_path.rs is_git_segment
fn is_git_segment(p: &[u8], i: usize) -> (r: bool)
    requires
        p@.len() < usize::MAX - 4,
        i < p@.len(),
    ensures
        r <==> git_segment(p@, i as int),
{
    // mirror-begin is_git_segment
    i + 3 < p.len()
        && p[i] == 46
        && lower(p[i + 1]) == 103
        && lower(p[i + 2]) == 105
        && lower(p[i + 3]) == 116
        && ends_at(p, i + 4)
    // mirror-end is_git_segment
}

// mirror: crates/littleguy-core/src/patch_path.rs is_empty_segment
fn is_empty_segment(p: &[u8], i: usize) -> (r: bool)
    requires
        p@.len() < usize::MAX,
    ensures
        r <==> empty_segment(p@, i as int),
{
    // mirror-begin is_empty_segment
    i < p.len() && p[i] == 47 && (i == 0 || i + 1 == p.len() || p[i + 1] == 47)
    // mirror-end is_empty_segment
}

// mirror: crates/littleguy-core/src/patch_path.rs byte_ok
fn byte_ok(p: &[u8], i: usize) -> (r: bool)
    requires
        p@.len() < usize::MAX - 4,
        i < p@.len(),
    ensures
        r <==> local_ok(p@, i as int),
{
    // mirror-begin byte_ok
    !is_bad_byte(p[i])
        && !is_empty_segment(p, i)
        && !(starts_at(p, i) && (is_dot_segment(p, i) || is_git_segment(p, i)))
    // mirror-end byte_ok
}

// mirror: crates/littleguy-core/src/patch_path.rs all_ok
fn all_ok(p: &[u8], i: usize) -> (r: bool)
    requires
        p@.len() < usize::MAX - 4,
    ensures
        r <==> (forall|j: int| i <= j < p@.len() ==> #[trigger] local_ok(p@, j)),
    decreases p@.len() - i,
{
    // mirror-begin all_ok
    i >= p.len() || (byte_ok(p, i) && all_ok(p, i + 1))
    // mirror-end all_ok
}

// mirror: crates/littleguy-core/src/patch_path.rs is_github_first
fn is_github_first(p: &[u8]) -> (r: bool)
    ensures
        r <==> github_first(p@),
{
    // mirror-begin is_github_first
    p.len() >= 7
        && p[0] == 46
        && lower(p[1]) == 103
        && lower(p[2]) == 105
        && lower(p[3]) == 116
        && lower(p[4]) == 104
        && lower(p[5]) == 117
        && lower(p[6]) == 98
        && ends_at(p, 7)
    // mirror-end is_github_first
}

// mirror: crates/littleguy-core/src/patch_path.rs is_patch_path_bytes
pub fn is_patch_path_bytes(p: &[u8]) -> (r: bool)
    requires
        p@.len() < usize::MAX - 4,
    ensures
        // The kernel is exactly the plain reading of a patch path.
        r <==> patch_path(p@),
        // So an accepted path is relative,
        r ==> p@[0] != 47,
        // never writes under .github,
        r ==> !github_first(p@),
        // and never names a `..` segment, which would leave the tree.
        r ==> (forall|i: int| 0 <= i < p@.len() && #[trigger] starts(p@, i) ==> !dot_segment(p@, i)),
{
    let r =
    // mirror-begin is_patch_path_bytes
    !p.is_empty() && !is_github_first(p) && all_ok(p, 0)
    // mirror-end is_patch_path_bytes
    ;
    proof {
        if r {
            assert(local_ok(p@, 0));
            assert forall|i: int| 0 <= i < p@.len() && #[trigger] starts(p@, i) implies !dot_segment(p@, i) by {
                assert(local_ok(p@, i));
            }
        }
    }
    r
}

} // verus!
