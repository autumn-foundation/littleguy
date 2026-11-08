//! The pairing kernel: a bounded number of live codes, and an honest clock.
//!
//! `issue_decision` and `is_live` mirror `littleguy-core/src/pairing.rs`.
//! The bodies between the mirror markers must match the Rust source token
//! for token.

use vstd::prelude::*;

verus! {

/// What the registry does with a request for a code.
pub enum Issue {
    /// Answer with the live code that the sender holds already.
    Reuse,
    /// Make a new code.
    New,
    /// Refuse: too many codes are live.
    Full,
}

// mirror: crates/littleguy-core/src/pairing.rs issue_decision
pub fn issue_decision(has_live_code: bool, live_codes: usize, cap: usize) -> (r: Issue)
    ensures
        // A sender that holds a code never gets a second one.
        has_live_code <==> r == Issue::Reuse,
        // A new code only ever goes into a free slot.
        r == Issue::New ==> live_codes < cap,
        // A refusal happens only when every slot is taken.
        r == Issue::Full ==> (!has_live_code && live_codes >= cap),
        // A free slot is never refused.
        (!has_live_code && live_codes < cap) ==> r == Issue::New,
{
    // mirror-begin issue_decision
    if has_live_code {
        return Issue::Reuse;
    }
    if live_codes < cap {
        Issue::New
    } else {
        Issue::Full
    }
    // mirror-end issue_decision
}

// mirror: crates/littleguy-core/src/pairing.rs is_live
pub fn is_live(issued_at: u64, now: u64, ttl: u64) -> (r: bool)
    ensures
        // A code lives exactly in the window [issued_at, issued_at + ttl).
        r <==> (issued_at <= now && (now as int) < (issued_at as int) + (ttl as int)),
        // A clock that went back never revives a code.
        now < issued_at ==> !r,
        // A zero lifetime keeps no code alive.
        ttl == 0 ==> !r,
{
    // mirror-begin is_live
    if now < issued_at {
        return false;
    }
    now - issued_at < ttl
    // mirror-end is_live
}

} // verus!
