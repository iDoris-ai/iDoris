//! Billing-period bucketing (总体规划 §4.6: "额度按账期计算，时区规则沿用
//! TS 实现"; TS reference: `packages/tenancy/src/billing.ts`). This module
//! only computes the period *key* (`YYYY-MM`) a UTC instant falls into
//! under a tenant's explicit `billing_timezone` — the wider
//! usage-aggregation / `range_utc`-echoing query from contract-tenancy §6
//! is `billing.rs`'s job (T2.6.1, out of scope for this ledger).
//!
//! Same rule as the TS side: the tenant's IANA zone decides the local wall
//! clock that determines the period — never the server's local time, never
//! a caller-supplied override. The TS implementation reads that wall clock
//! from `Intl.DateTimeFormat` (backed by the host's ICU tzdata); this one
//! uses `chrono-tz`'s bundled IANA tzdata (public domain). Both track the
//! same real-world DST rules for every canonical zone name, **as of each
//! side's own tzdata snapshot** — `chrono-tz`'s data is pinned at its
//! release (see the root `Cargo.toml` dependency comment) and only updates
//! when that crate is bumped, so this is not a live/self-updating guarantee
//! if IANA ships a rule change after that snapshot.
//! `idoris_contracts::tenant::is_iana_time_zone` stays the single source of
//! truth for *which* zone names this crate accepts (see
//! `BudgetLedger::configure`); this function only computes with a
//! zone already known to be valid.

use chrono::{DateTime, Datelike, Utc};
use chrono_tz::Tz;
use std::str::FromStr as _;

use super::error::BudgetError;

/// The `YYYY-MM` billing period `now_ms` (UTC epoch milliseconds) falls into
/// when read as a local wall-clock timestamp in `billing_timezone`.
pub fn billing_period_key(now_ms: i64, billing_timezone: &str) -> Result<String, BudgetError> {
    let tz = Tz::from_str(billing_timezone).map_err(|_| BudgetError::InvalidTimeZone {
        billing_timezone: billing_timezone.to_string(),
    })?;
    let utc: DateTime<Utc> =
        DateTime::from_timestamp_millis(now_ms).ok_or(BudgetError::InvalidTimestamp { now_ms })?;
    let local = utc.with_timezone(&tz);
    Ok(format!("{:04}-{:02}", local.year(), local.month()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    const MS_PER_DAY: i64 = 24 * 60 * 60 * 1000;

    fn ms(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("valid rfc3339 fixture")
            .timestamp_millis()
    }

    #[test]
    fn same_utc_instant_can_fall_in_different_periods_by_zone() {
        // 2026-08-31T17:30:00Z is 2026-09-01 00:30 in Asia/Bangkok (UTC+7,
        // no DST) but still 2026-08-31 in UTC itself — exactly the
        // "换台机器账单就变" trap the TS billing.rs doc comment warns about.
        let ts = ms("2026-08-31T17:30:00Z");
        assert_eq!(
            billing_period_key(ts, "UTC").expect("UTC is valid"),
            "2026-08"
        );
        assert_eq!(
            billing_period_key(ts, "Asia/Bangkok").expect("Asia/Bangkok is valid"),
            "2026-09"
        );
    }

    #[test]
    fn timestamps_a_day_apart_within_the_same_month_share_a_period() {
        let ts = ms("2026-09-15T00:00:00Z");
        let a = billing_period_key(ts, "Asia/Bangkok").expect("valid tz");
        let b = billing_period_key(ts + MS_PER_DAY, "Asia/Bangkok").expect("valid tz");
        assert_eq!(a, b, "same month, one day apart, should share a period");
    }

    /// Negative control: an unrecognized zone name must be rejected, never
    /// silently treated as UTC or the process's local time.
    #[test]
    fn unknown_time_zone_is_rejected_not_guessed() {
        let err = billing_period_key(0, "Not/AZone");
        assert!(matches!(err, Err(BudgetError::InvalidTimeZone { .. })));
    }
}
