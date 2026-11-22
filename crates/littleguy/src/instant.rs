//! Converts between the instants of littleguy and of the engine.
//!
//! littleguy keeps time as a `chrono` `DateTime<Utc>`. autumn-harvest 0.11
//! keeps it as a `jiff` `Timestamp` (its ADR 0018). Each call into the
//! engine converts here, at microsecond precision, which both types hold
//! exactly.

use autumn_harvest::time::Timestamp;
use chrono::{DateTime, Utc};

/// `at` as an engine timestamp. A time outside the range of the engine
/// takes the nearest end of that range.
#[must_use]
pub fn to_engine(at: DateTime<Utc>) -> Timestamp {
    let micros = at.timestamp_micros();
    Timestamp::from_microsecond(micros).unwrap_or(if micros < 0 {
        Timestamp::MIN
    } else {
        Timestamp::MAX
    })
}

/// The engine timestamp `at` as a `chrono` time. Every engine timestamp
/// fits, because the engine range is narrower than the `chrono` range.
#[must_use]
pub fn from_engine(at: Timestamp) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(at.as_microsecond()).unwrap_or_default()
}

/// The next fire time of `schedule` after `at`, as the engine computes it.
#[must_use]
pub fn next_fire_after(
    schedule: &autumn_harvest::policy::Schedule,
    at: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    autumn_harvest::schedule_plan::next_fire_after(schedule, to_engine(at)).map(from_engine)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_instant_goes_to_the_engine_and_back_unchanged() {
        let at = DateTime::parse_from_rfc3339("2026-11-22T03:16:53.123456Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(from_engine(to_engine(at)), at);
        assert_eq!(to_engine(at).to_string(), "2026-11-22T03:16:53.123456Z");
    }

    #[test]
    fn a_time_past_the_engine_range_takes_its_end() {
        assert_eq!(to_engine(DateTime::<Utc>::MAX_UTC), Timestamp::MAX);
        assert_eq!(to_engine(DateTime::<Utc>::MIN_UTC), Timestamp::MIN);
    }
}
