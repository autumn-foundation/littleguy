//! The instants of littleguy, and their text forms.
//!
//! littleguy keeps time as a `jiff` `Timestamp`, as autumn-harvest does
//! (its ADR 0018). The text forms here keep the output of earlier
//! releases byte for byte. Wake keys hold [`rfc3339`], so a change there
//! gives a booked wake a new key, and the wake could run twice.

use std::fmt::Write as _;

use jiff::Timestamp;

/// `at` in RFC 3339 with a `+00:00` offset. The fraction has 0, 3, 6 or 9
/// digits: the fewest of those that hold it exactly.
#[must_use]
pub fn rfc3339(at: Timestamp) -> String {
    let mut text = at.strftime("%Y-%m-%dT%H:%M:%S").to_string();
    let nanos = at.subsec_nanosecond().unsigned_abs();
    let _ = match nanos {
        0 => Ok(()),
        n if n % 1_000_000 == 0 => write!(text, ".{:03}", n / 1_000_000),
        n if n % 1_000 == 0 => write!(text, ".{:06}", n / 1_000),
        n => write!(text, ".{n:09}"),
    };
    text.push_str("+00:00");
    text
}

/// The instant of `ms` milliseconds after the Unix epoch, when it is in
/// range.
#[must_use]
pub fn from_millis(ms: i64) -> Option<Timestamp> {
    Timestamp::from_millisecond(ms).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    /// The strings that chrono `to_rfc3339` gave for the same instants.
    #[test]
    fn the_key_form_keeps_the_text_of_earlier_releases() {
        assert_eq!(
            rfc3339(at("2026-11-17T03:00:00Z")),
            "2026-11-17T03:00:00+00:00"
        );
        assert_eq!(
            rfc3339(at("2026-11-17T03:00:00.25Z")),
            "2026-11-17T03:00:00.250+00:00"
        );
        assert_eq!(
            rfc3339(at("2026-11-17T03:00:00.000250Z")),
            "2026-11-17T03:00:00.000250+00:00"
        );
        assert_eq!(
            rfc3339(at("2026-11-17T03:00:00.000000250Z")),
            "2026-11-17T03:00:00.000000250+00:00"
        );
        assert_eq!(
            rfc3339(at("1969-12-31T23:59:59.5Z")),
            "1969-12-31T23:59:59.500+00:00"
        );
    }

    #[test]
    fn milliseconds_out_of_range_give_none() {
        assert_eq!(from_millis(0), Some(Timestamp::UNIX_EPOCH));
        assert_eq!(from_millis(i64::MAX), None);
    }
}
