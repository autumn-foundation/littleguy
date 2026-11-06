//! The routing kernel: a stranger never reaches an agent.
//!
//! `decide` mirrors `littleguy-core/src/route.rs decide`. The body between
//! the mirror markers must match the Rust source token for token. The spec
//! `decision` is the table, and the ensures clauses state the properties
//! that matter on their own.

use vstd::prelude::*;

verus! {

/// The decision of the kernel.
pub enum Decision {
    /// Deliver the message to the owner's assistant.
    Deliver,
    /// Answer with a pairing prompt. Deliver nothing.
    Pairing,
    /// Drop the message. Answer nothing.
    Drop,
}

/// The decision table.
pub open spec fn decision(enabled: bool, allowed: bool, pairing: bool) -> Decision {
    if enabled && allowed {
        Decision::Deliver
    } else if enabled && pairing {
        Decision::Pairing
    } else {
        Decision::Drop
    }
}

// mirror: crates/littleguy-core/src/route.rs decide
pub fn decide(enabled: bool, allowed: bool, pairing: bool) -> (r: Decision)
    ensures
        r == decision(enabled, allowed, pairing),
        // Only an allowed sender on an enabled channel reaches an agent.
        r == Decision::Deliver <==> (enabled && allowed),
        // A disabled channel answers nothing at all.
        !enabled ==> r == Decision::Drop,
        // A pairing prompt goes only to a stranger on a pairing channel.
        r == Decision::Pairing ==> (enabled && !allowed && pairing),
        // A channel with no pairing never shows a prompt.
        !pairing ==> r != Decision::Pairing,
{
    // mirror-begin
    if !enabled {
        return Decision::Drop;
    }
    if allowed {
        return Decision::Deliver;
    }
    if pairing {
        Decision::Pairing
    } else {
        Decision::Drop
    }
    // mirror-end
}

} // verus!
