//! The room kernel of compaction (ADR 0043): whether a save turn fits under
//! the request cap.
//!
//! Each function mirrors `littleguy-core/src/compaction.rs`. The bodies
//! between the mirror markers must match the Rust source token for token.

use vstd::arithmetic::div_mod::{lemma_fundamental_div_mod, lemma_mod_pos_bound};
use vstd::arithmetic::mul::lemma_mul_upper_bound;
use vstd::prelude::*;

verus! {

/// The percent as the engine reads it: 0 counts as 1, and a value over 100
/// counts as 100.
pub open spec fn eng(p: u8) -> int {
    if p == 0 {
        1
    } else if p > 100 {
        100
    } else {
        p as int
    }
}

/// A request of `len` bytes reaches `p` percent of `cap`, as the engine
/// reads it (`bounds::reaches_percent`).
pub open spec fn reaches(len: int, cap: int, p: int) -> bool {
    len * 100 >= cap * p
}

/// `due` is the smallest request that reaches `p` percent of `cap`.
pub open spec fn smallest_due(due: int, cap: int, p: int) -> bool {
    reaches(due, cap, p) && !reaches(due - 1, cap, p)
}

/// A save turn fits: a cap is set, and the smallest request that makes a
/// reset due, with the notes and the notice, stays within the cap.
pub open spec fn fits(cap: int, p: int, notes: int, notice: int) -> bool {
    cap > 0 && forall|due: int| #[trigger] smallest_due(due, cap, p) ==> due + notes + notice
        <= cap
}

/// `(cap * p + 99) / 100` is the one smallest request that reaches `p`
/// percent of `cap`.
proof fn lemma_due(cap: int, p: int)
    requires
        0 <= cap,
        1 <= p <= 100,
    ensures
        smallest_due((cap * p + 99) / 100, cap, p),
        forall|due: int| #[trigger] smallest_due(due, cap, p) ==> due == (cap * p + 99) / 100,
{
    let x = cap * p;
    assert(x >= 0) by (nonlinear_arith)
        requires
            0 <= cap,
            1 <= p,
            x == cap * p,
    ;
    let d = (x + 99) / 100;
    lemma_fundamental_div_mod(x + 99, 100);
    lemma_mod_pos_bound(x + 99, 100);
    // So x <= 100 * d <= x + 99.
    assert(x + 99 == 100 * d + (x + 99) % 100);
    assert(reaches(d, cap, p));
    assert(!reaches(d - 1, cap, p));
    assert forall|due: int| #[trigger] smallest_due(due, cap, p) implies due == d by {
        // Both 100 * due and 100 * d lie in [x, x + 99].
        assert(due * 100 >= x);
        assert((due - 1) * 100 < x);
    }
}

// mirror: crates/littleguy-core/src/compaction.rs engine_percent
pub fn engine_percent(percent: u8) -> (r: u8)
    ensures
        r as int == eng(percent),
        1 <= r <= 100,
{
    // mirror-begin engine_percent
    if percent == 0 {
        1
    } else if percent > 100 {
        100
    } else {
        percent
    }
    // mirror-end engine_percent
}

// mirror: crates/littleguy-core/src/compaction.rs save_turn_fits
pub fn save_turn_fits(cap: u64, percent: u8, notes: u32, notice: u32) -> (r: bool)
    ensures
        r <==> fits(cap as int, eng(percent), notes as int, notice as int),
{
    proof {
        lemma_due(cap as int, eng(percent));
        // The product fits a u128.
        lemma_mul_upper_bound(cap as int, u64::MAX as int, eng(percent), 100);
    }
    // mirror-begin save_turn_fits
    let p = engine_percent(percent);
    let due = (cap as u128 * p as u128 + 99) / 100;
    cap > 0 && due + notes as u128 + notice as u128 <= cap as u128
    // mirror-end save_turn_fits
}

} // verus!
