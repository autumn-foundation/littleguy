//! The room kernel of compaction (ADR 0043): whether a save turn fits under
//! the request cap.
//!
//! autumn-harvest compacts a long op with a context reset (issue #2453).
//! When a request reaches `at_percent` of the request cap, the loop first
//! sends a save turn: the window, a fixed notice, and room for the model to
//! call `save_progress`. Then it resets the window to the task and the
//! notes. The save turn needs room for the notice and the notes. Without
//! that room the engine resets at once, and the model saves nothing. So a
//! setting whose save turn never fits drops the work of the op at each
//! reset, and the config refuses it.
//!
//! The smallest request that makes a reset due has `ceil(cap * p / 100)`
//! bytes, where `p` is the percent as the engine reads it: 0 counts as 1,
//! and a value over 100 counts as 100. The save turn fits when that request,
//! the notice and the notes stay within the cap.
//!
//! `formal/verus/src/compaction.rs` proves that [`save_turn_fits`] is
//! exactly that rule, for every input, with no overflow. The test
//! `verus_mirror` fails when a mirrored body drifts from its proof.

use autumn_harvest_agent::context_reset::RESET_NOTICE;
use autumn_harvest_agent::{ChatMessage, ChatRole};

/// The request cap of the engine when the config sets none, in bytes.
pub const DEFAULT_REQUEST_CAP: u64 = autumn_harvest::builder::DEFAULT_MAX_ACTIVITY_INPUT_BYTES;

/// The percent as the engine reads it: 0 counts as 1, and a value over 100
/// counts as 100.
#[must_use]
pub const fn engine_percent(percent: u8) -> u8 {
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

/// Whether a save turn fits under `cap`: the smallest request that reaches
/// `percent` of `cap`, with `notice` bytes of notice and `notes` bytes of
/// notes, stays within `cap`. A cap of zero is no cap, so no reset is ever
/// due, and no save turn fits.
///
/// The sum runs in `u128`, so it never overflows.
#[must_use]
#[expect(
    clippy::cast_lossless,
    reason = "u128::from is not const, and Verus proves these casts"
)]
pub const fn save_turn_fits(cap: u64, percent: u8, notes: u32, notice: u32) -> bool {
    // mirror-begin save_turn_fits
    let p = engine_percent(percent);
    let due = (cap as u128 * p as u128 + 99) / 100;
    cap > 0 && due + notes as u128 + notice as u128 <= cap as u128
    // mirror-end save_turn_fits
}

/// The bytes that the notice of a save turn adds to a request: the notice
/// message as JSON, and the comma before it.
#[must_use]
pub fn notice_bytes() -> u32 {
    let message = ChatMessage::text(ChatRole::User, RESET_NOTICE);
    let len = serde_json::to_vec(&message).map_or(RESET_NOTICE.len(), |json| json.len());
    u32::try_from(len.saturating_add(1)).unwrap_or(u32::MAX)
}

/// The bytes of a request that has just reached `percent` of `cap`.
#[must_use]
pub fn due_bytes(cap: u64, percent: u8) -> u64 {
    let due = (u128::from(cap) * u128::from(engine_percent(percent))).div_ceil(100);
    u64::try_from(due).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;

    use super::*;

    /// The rule, read plainly with big integers.
    fn rule(cap: u64, percent: u8, notes: u32, notice: u32) -> bool {
        let p = u128::from(percent.clamp(1, 100));
        let due = (u128::from(cap) * p).div_ceil(100);
        cap > 0 && due + u128::from(notes) + u128::from(notice) <= u128::from(cap)
    }

    #[test]
    fn the_engine_reads_the_percent_as_the_kernel_does() {
        assert_eq!(engine_percent(0), 1);
        assert_eq!(engine_percent(1), 1);
        assert_eq!(engine_percent(80), 80);
        assert_eq!(engine_percent(100), 100);
        assert_eq!(engine_percent(255), 100);
    }

    #[test]
    fn the_default_setting_has_room_under_the_default_cap() {
        assert!(save_turn_fits(
            DEFAULT_REQUEST_CAP,
            80,
            8_192,
            notice_bytes()
        ));
    }

    #[test]
    fn a_save_turn_never_fits_at_the_whole_cap() {
        assert!(!save_turn_fits(DEFAULT_REQUEST_CAP, 100, 1, notice_bytes()));
        assert!(!save_turn_fits(u64::MAX, 100, 1, 0));
    }

    #[test]
    fn the_boundary_is_exact() {
        // 80% of 1000 is 800. 800 + 150 + 50 is the whole cap.
        assert!(save_turn_fits(1_000, 80, 150, 50));
        assert!(!save_turn_fits(1_000, 80, 151, 50));
        // 1% of 101 is 1.01 bytes, so the due request has 2 bytes.
        assert!(save_turn_fits(101, 1, 99, 0));
        assert!(!save_turn_fits(101, 1, 100, 0));
    }

    #[test]
    fn no_cap_has_no_save_turn() {
        assert!(!save_turn_fits(0, 80, 0, 0));
    }

    #[test]
    fn the_notice_is_measured_as_json() {
        let notice = notice_bytes();
        let text = u32::try_from(RESET_NOTICE.len()).unwrap();
        assert!(notice > text, "{notice} > {text}");
        assert!(notice < text + 64, "{notice}");
    }

    #[test]
    fn the_due_request_is_the_smallest_that_reaches_the_percent() {
        assert_eq!(due_bytes(1_000, 80), 800);
        assert_eq!(due_bytes(101, 1), 2);
        assert_eq!(due_bytes(u64::MAX, 100), u64::MAX);
        assert_eq!(due_bytes(0, 80), 0);
    }

    proptest! {
        #[test]
        fn the_kernel_is_the_rule(cap: u64, percent: u8, notes: u32, notice: u32) {
            prop_assert_eq!(
                save_turn_fits(cap, percent, notes, notice),
                rule(cap, percent, notes, notice)
            );
        }

        #[test]
        fn the_due_request_reaches_the_percent_and_one_byte_less_does_not(
            cap in 1_u64..,
            percent: u8,
        ) {
            let due = due_bytes(cap, percent);
            let p = u128::from(engine_percent(percent));
            prop_assert!(u128::from(due) * 100 >= u128::from(cap) * p);
            prop_assert!(u128::from(due - 1) * 100 < u128::from(cap) * p);
        }

        #[test]
        fn less_notes_still_fit(cap: u64, percent: u8, notes: u32, notice: u32, less: u32) {
            if save_turn_fits(cap, percent, notes, notice) {
                prop_assert!(save_turn_fits(cap, percent, notes.min(less), notice));
            }
        }
    }
}
