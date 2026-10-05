//! Resolve a tenant billing month to its UTC half-open interval.

use chrono::{NaiveDate, TimeZone as _};
use chrono_tz::Tz;
use std::str::FromStr as _;

/// UTC epoch-millisecond interval for one billing month, including `from` and excluding `to`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BillingPeriodRange {
    pub from: i64,
    pub to: i64,
}

#[derive(Debug, thiserror::Error)]
pub enum BillingError {
    #[error("billing period must be ASCII YYYY-MM (got {period:?})")]
    InvalidPeriod { period: String },
    #[error("invalid billing time zone {billing_timezone:?}")]
    InvalidTimeZone { billing_timezone: String },
    #[error("could not resolve billing period boundary for {period:?} in {billing_timezone:?}")]
    InvalidBoundary {
        period: String,
        billing_timezone: String,
    },
}

fn boundary(year: i32, month: u32, tz: Tz) -> Option<i64> {
    let date = NaiveDate::from_ymd_opt(year, month, 1)?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    tz.from_local_datetime(&midnight)
        .single()
        .map(|value| value.timestamp_millis())
}

/// Resolve strict `YYYY-MM` using the tenant's explicit IANA zone.
/// Both local midnights must exist unambiguously; otherwise return an error.
pub fn resolve_billing_period_range(
    period: &str,
    billing_timezone: &str,
) -> Result<BillingPeriodRange, BillingError> {
    let bytes = period.as_bytes();
    if bytes.len() != 7
        || bytes[0..4].iter().any(|b| !b.is_ascii_digit())
        || bytes[4] != b'-'
        || bytes[5..7].iter().any(|b| !b.is_ascii_digit())
    {
        return Err(BillingError::InvalidPeriod {
            period: period.to_string(),
        });
    }
    let year = period[0..4]
        .parse::<i32>()
        .map_err(|_| BillingError::InvalidPeriod {
            period: period.to_string(),
        })?;
    let month = period[5..7]
        .parse::<u32>()
        .map_err(|_| BillingError::InvalidPeriod {
            period: period.to_string(),
        })?;
    if !(1..=12).contains(&month) {
        return Err(BillingError::InvalidPeriod {
            period: period.to_string(),
        });
    }
    let tz = Tz::from_str(billing_timezone).map_err(|_| BillingError::InvalidTimeZone {
        billing_timezone: billing_timezone.to_string(),
    })?;
    let next = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    let from = boundary(year, month, tz);
    let to = boundary(next.0, next.1, tz);
    match (from, to) {
        (Some(from), Some(to)) => Ok(BillingPeriodRange { from, to }),
        _ => Err(BillingError::InvalidBoundary {
            period: period.to_string(),
            billing_timezone: billing_timezone.to_string(),
        }),
    }
}
