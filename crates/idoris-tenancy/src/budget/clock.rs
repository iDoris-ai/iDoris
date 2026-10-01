//! Injectable wall clock, so billing-period bucketing and TTL expiry are
//! deterministic under test (no sleeping real time, no globally mocking
//! `SystemTime`) while production code just uses [`SystemClock`].

use std::time::{SystemTime, UNIX_EPOCH};

/// Milliseconds since the Unix epoch, UTC. Never local time — callers
/// convert to a tenant's explicit `billing_timezone` only at the point they
/// need a period key (see `budget::period::billing_period_key`).
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> i64;
}

/// Real wall-clock time; what [`crate::budget::BudgetLedger::open`] uses.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

/// Saturating `u128` (milliseconds) -> `i64` conversion shared by both
/// branches below, instead of an unchecked `as i64` cast that would wrap
/// around to a bogus negative value once `millis` exceeds `i64::MAX`
/// (Codex review) — not reachable before the year 292471209, but a clamp
/// costs nothing and turns "silently wrong" into "clamped, still ordered".
fn millis_to_i64_saturating(millis: u128) -> i64 {
    i64::try_from(millis).unwrap_or(i64::MAX)
}

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        // `unwrap_or_default()` here would silently treat "system clock is
        // before the Unix epoch" as "it is exactly 1970-01-01T00:00:00Z" —
        // a real value, not an absence of one, and one that would make
        // every billing-period/TTL calculation downstream silently wrong in
        // a fail-*open* way (Codex review: e.g. spend recorded into the
        // 1970-01 period would never show up in any real tenant's current
        // period, understating their spend). Representing "before epoch" as
        // a negative millisecond count is both correct (epoch millis are
        // well-defined for pre-1970 instants) and keeps `now_ms` infallible
        // without inventing a `ClockError` variant for what would indicate
        // a severely misconfigured host clock, not a normal runtime error.
        match SystemTime::now().duration_since(UNIX_EPOCH) {
            Ok(elapsed) => millis_to_i64_saturating(elapsed.as_millis()),
            Err(before_epoch) => -millis_to_i64_saturating(before_epoch.duration().as_millis()),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn system_clock_moves_forward_and_matches_wall_time() {
        let clock = SystemClock;
        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_millis() as i64;
        let sampled = clock.now_ms();
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before epoch")
            .as_millis() as i64;
        assert!(sampled >= before && sampled <= after);
    }
}
