//! Tenant monthly billing period resolution in the tenant's explicit time zone.
//!
//! Periods are strict `YYYY-MM` values and resolve to UTC epoch milliseconds
//! in a half-open `[from, to)` range. Boundaries follow local month starts.

pub mod period;
pub use period::{BillingError, BillingPeriodRange, resolve_billing_period_range};

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn bangkok_boundary_is_exact_and_differs_from_utc() {
        let bangkok = resolve_billing_period_range("2026-09", "Asia/Bangkok").unwrap();
        let utc = resolve_billing_period_range("2026-09", "UTC").unwrap();
        assert_eq!(bangkok.from, 1_788_195_600_000); // 2026-08-31T17:00:00Z
        assert_eq!(bangkok.to, 1_790_787_600_000); // 2026-09-30T17:00:00Z
        assert_eq!(utc.from, 1_788_220_800_000); // 2026-09-01T00:00:00Z
        assert_eq!(utc.to, 1_790_812_800_000); // 2026-10-01T00:00:00Z
        assert_ne!(bangkok, utc);
    }

    #[test]
    fn year_rollover_and_leap_month_use_next_month_start() {
        let december = resolve_billing_period_range("2026-12", "Asia/Bangkok").unwrap();
        assert_eq!(december.from, 1_796_058_000_000); // 2026-11-30T17:00Z
        assert_eq!(december.to, 1_798_736_400_000); // 2026-12-31T17:00Z
        let february = resolve_billing_period_range("2024-02", "UTC").unwrap();
        assert_eq!(february.from, 1_706_745_600_000); // 2024-02-01T00:00Z
        assert_eq!(february.to, 1_709_251_200_000); // 2024-03-01T00:00Z
    }

    #[test]
    fn utc_small_and_large_years_keep_their_literal_values() {
        let year_zero = resolve_billing_period_range("0000-01", "UTC").unwrap();
        assert_eq!(year_zero.from, -62_167_219_200_000);
        assert_eq!(year_zero.to, -62_164_540_800_000);
        let year_99 = resolve_billing_period_range("0099-12", "UTC").unwrap();
        assert_eq!(year_99.from, -59_014_137_600_000);
        assert_eq!(year_99.to, -59_011_459_200_000);
        let year_9999 = resolve_billing_period_range("9999-12", "UTC").unwrap();
        assert_eq!(year_9999.from, 253_399_622_400_000);
        assert_eq!(year_9999.to, 253_402_300_800_000);
    }

    #[test]
    fn new_york_dst_month_lengths_use_each_boundary_offset() {
        let spring = resolve_billing_period_range("2026-03", "America/New_York").unwrap();
        assert_eq!(spring.from, 1_772_341_200_000); // 2026-03-01T05:00Z
        assert_eq!(spring.to, 1_775_016_000_000); // 2026-04-01T04:00Z
        assert_eq!(spring.to - spring.from, (31 * 24 - 1) * 60 * 60 * 1000);
        let fall = resolve_billing_period_range("2026-11", "America/New_York").unwrap();
        assert_eq!(fall.from, 1_793_505_600_000); // 2026-11-01T04:00Z
        assert_eq!(fall.to, 1_796_101_200_000); // 2026-12-01T05:00Z
        assert_eq!(fall.to - fall.from, (30 * 24 + 1) * 60 * 60 * 1000);
    }

    #[test]
    fn rejects_non_ascii_or_noncanonical_periods() {
        for period in [
            "",
            "2026",
            "2026-09\n",
            "2026-09-01",
            "2026-9",
            "2026-13",
            "2026-00",
            " 2026-09",
            "2026-09 ",
            "２０２６-09",
            "2026－09",
        ] {
            assert!(
                matches!(
                    resolve_billing_period_range(period, "UTC"),
                    Err(BillingError::InvalidPeriod { .. })
                ),
                "accepted {period:?}"
            );
        }
    }

    #[test]
    fn rejects_invalid_time_zones_without_falling_back_to_utc() {
        for zone in ["", "Mars/Phobos", "+07:00", " Asia/Bangkok", "UTC "] {
            assert!(matches!(
                resolve_billing_period_range("2026-09", zone),
                Err(BillingError::InvalidTimeZone { .. })
            ));
        }
    }

    #[test]
    fn rejects_missing_or_ambiguous_midnight_at_either_boundary() {
        // DST skips Asuncion's 2017-10-01 midnight and repeats Havana's
        // 2015-11-01 midnight. Test both the current and previous month.
        for (period, zone) in [
            ("2017-10", "America/Asuncion"),
            ("2017-09", "America/Asuncion"),
            ("2015-11", "America/Havana"),
            ("2015-10", "America/Havana"),
        ] {
            assert!(matches!(
                resolve_billing_period_range(period, zone),
                Err(BillingError::InvalidBoundary { .. })
            ));
        }
    }
}
