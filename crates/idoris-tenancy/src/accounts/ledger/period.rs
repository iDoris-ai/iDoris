//! 账期（月）与时刻之间的换算，时区是账本配置里的 IANA 时区。
//!
//! 和 `crate::budget::billing_period_key` 同一套规则（本地墙上时钟决定账期，不看服务器时区），这里在它之上
//! 多做一件事：知道一个账期从哪一毫秒开始、到哪一毫秒结束（半开区间 `[start, end)`），给免费额度的有效期、
//! 补贴池按月的消耗窗口用。

use std::str::FromStr;

use chrono::{DateTime, Datelike, TimeZone, Utc};
use chrono_tz::Tz;
use idoris_compute_contracts::ids::BillingMonth;

use super::error::LedgerError;

fn tz_of(timezone: &str) -> Result<Tz, LedgerError> {
    Tz::from_str(timezone)
        .map_err(|_| LedgerError::Invalid(format!("unknown billing time zone {timezone:?}")))
}

/// 一个时刻落在哪个账期。
pub(super) fn period_of(now_ms: i64, timezone: &str) -> Result<BillingMonth, LedgerError> {
    let tz = tz_of(timezone)?;
    let utc: DateTime<Utc> = DateTime::from_timestamp_millis(now_ms)
        .ok_or_else(|| LedgerError::Invalid(format!("timestamp {now_ms} is out of range")))?;
    let local = utc.with_timezone(&tz);
    let year = u16::try_from(local.year())
        .map_err(|_| LedgerError::Invalid(format!("year {} is out of range", local.year())))?;
    let month = u8::try_from(local.month())
        .map_err(|_| LedgerError::Invalid(format!("month {} is out of range", local.month())))?;
    Ok(BillingMonth::new(year, month)?)
}

/// 本地时区里某年某月 1 日 00:00 的 UTC 毫秒。遇到夏令时空洞（那个时刻不存在）取之后最近的合法时刻。
fn month_start_ms(tz: &Tz, year: i32, month: u32) -> Result<i64, LedgerError> {
    for hour in 0..=3 {
        if let Some(start) = tz.with_ymd_and_hms(year, month, 1, hour, 0, 0).earliest() {
            return Ok(start.timestamp_millis());
        }
    }
    Err(LedgerError::Invalid(format!(
        "cannot resolve the start of {year:04}-{month:02}"
    )))
}

/// 账期的 `[start_ms, end_ms)`。
pub(super) fn period_bounds(
    period: BillingMonth,
    timezone: &str,
) -> Result<(i64, i64), LedgerError> {
    let tz = tz_of(timezone)?;
    let year = i32::from(period.year());
    let month = u32::from(period.month());
    let start = month_start_ms(&tz, year, month)?;
    let (next_year, next_month) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let end = month_start_ms(&tz, next_year, next_month)?;
    Ok((start, end))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ms(rfc3339: &str) -> i64 {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .timestamp_millis()
    }

    #[test]
    fn the_same_instant_is_in_different_periods_in_different_zones() {
        let ts = ms("2026-10-31T18:00:00Z");
        assert_eq!(period_of(ts, "UTC").unwrap().to_string(), "2026-10");
        assert_eq!(
            period_of(ts, "Asia/Bangkok").unwrap().to_string(),
            "2026-11"
        );
    }

    #[test]
    fn bounds_are_half_open_and_contiguous() {
        let november = BillingMonth::new(2026, 11).unwrap();
        let december = BillingMonth::new(2026, 12).unwrap();
        for zone in ["UTC", "Asia/Bangkok", "America/New_York"] {
            let (start, end) = period_bounds(november, zone).unwrap();
            let (next_start, _) = period_bounds(december, zone).unwrap();
            assert!(start < end);
            assert_eq!(
                end, next_start,
                "{zone}: the next month starts where this one ends"
            );
            assert_eq!(period_of(start, zone).unwrap(), november, "{zone}");
            assert_eq!(period_of(end - 1, zone).unwrap(), november, "{zone}");
            assert_eq!(period_of(end, zone).unwrap(), december, "{zone}");
        }
        // 12 月到下一年 1 月
        let (_, end) = period_bounds(december, "UTC").unwrap();
        assert_eq!(
            period_of(end, "UTC").unwrap(),
            BillingMonth::new(2027, 1).unwrap()
        );
    }

    #[test]
    fn an_unknown_zone_is_an_error_not_utc() {
        assert!(period_of(0, "Not/AZone").is_err());
        assert!(period_bounds(BillingMonth::new(2026, 11).unwrap(), "Not/AZone").is_err());
    }
}
