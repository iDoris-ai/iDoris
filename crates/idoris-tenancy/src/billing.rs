//! Tenant monthly billing period resolution in the tenant's explicit time zone.
//!
//! Periods are strict `YYYY-MM` values and resolve to UTC epoch milliseconds
//! in a half-open `[from, to)` range. Boundaries follow local month starts.

pub mod period;
pub use period::{BillingError, BillingPeriodRange, resolve_billing_period_range};

use chrono::{DateTime, SecondsFormat, Utc};
use idoris_contracts::tenant::TenantContext;
use serde::Serialize;
use serde_json::Value;

use crate::store::{RecordKind, StoreError, TenantRecord, TenantStore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageSource {
    Usage,
    Audit,
}

impl UsageSource {
    fn record_kind(self) -> RecordKind {
        match self {
            Self::Usage => RecordKind::Usage,
            Self::Audit => RecordKind::Audit,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct UsageTotals {
    pub cost_minor: f64,
    pub tokens_in: f64,
    pub tokens_out: f64,
    pub calls: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RangeUtc {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MonthlyUsage {
    pub tenant_id: String,
    pub period: String,
    pub billing_timezone: String,
    pub range_utc: RangeUtc,
    pub totals: UsageTotals,
}

#[derive(Debug, thiserror::Error)]
pub enum BillingAggregateError {
    #[error(transparent)]
    Period(#[from] BillingError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("billing query requires tenant context")]
    ScopeRequired,
    #[error("record {record_id:?} field {field:?} must be a finite number")]
    InvalidRecord {
        record_id: String,
        field: &'static str,
    },
    #[error("billing range is outside the supported UTC timestamp range")]
    InvalidRange,
}

pub fn aggregate_usage_records(
    records: &[TenantRecord],
    range: BillingPeriodRange,
) -> Result<UsageTotals, BillingAggregateError> {
    let mut totals = UsageTotals {
        cost_minor: 0.0,
        tokens_in: 0.0,
        tokens_out: 0.0,
        calls: 0,
    };
    for record in records {
        let ts = required_number(record, "ts_utc")?;
        if ts < range.from as f64 || ts >= range.to as f64 {
            continue;
        }
        totals.calls = totals.calls.saturating_add(1);
        totals.cost_minor += optional_number(record, "cost_minor")?;
        totals.tokens_in += optional_number(record, "tokens_in")?;
        totals.tokens_out += optional_number(record, "tokens_out")?;
    }
    Ok(totals)
}

pub fn query_monthly_usage(
    store: &TenantStore,
    context: Option<&TenantContext>,
    period: &str,
    source: Option<UsageSource>,
) -> Result<MonthlyUsage, BillingAggregateError> {
    let context = context.ok_or(BillingAggregateError::ScopeRequired)?;
    let source = source.unwrap_or(UsageSource::Usage);
    let records = store.list(Some(&context.tenant_id), Some(source.record_kind()))?;
    let range = resolve_billing_period_range(period, &context.billing_timezone)?;
    Ok(MonthlyUsage {
        tenant_id: context.tenant_id.clone(),
        period: period.to_string(),
        billing_timezone: context.billing_timezone.clone(),
        range_utc: RangeUtc {
            from: iso_utc(range.from)?,
            to: iso_utc(range.to)?,
        },
        totals: aggregate_usage_records(&records, range)?,
    })
}

fn required_number(
    record: &TenantRecord,
    field: &'static str,
) -> Result<f64, BillingAggregateError> {
    record
        .payload
        .get(field)
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| BillingAggregateError::InvalidRecord {
            record_id: record.record_id.clone(),
            field,
        })
}

fn optional_number(
    record: &TenantRecord,
    field: &'static str,
) -> Result<f64, BillingAggregateError> {
    match record.payload.get(field) {
        None | Some(Value::Null) => Ok(0.0),
        Some(value) => value
            .as_f64()
            .filter(|value| value.is_finite())
            .ok_or_else(|| BillingAggregateError::InvalidRecord {
                record_id: record.record_id.clone(),
                field,
            }),
    }
}

fn iso_utc(epoch_ms: i64) -> Result<String, BillingAggregateError> {
    DateTime::<Utc>::from_timestamp_millis(epoch_ms)
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, true))
        .ok_or(BillingAggregateError::InvalidRange)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::store::{RecordKind, TenantRecord};
    use idoris_contracts::tenant::{Budget, BudgetScope};
    use rusqlite::Connection;
    use serde_json::{Map, json};

    fn tenant() -> TenantContext {
        TenantContext {
            tenant_id: "acme-co".into(),
            budget: Budget {
                limit_minor: 5_000_000,
                spent_minor: 1_234_567,
                scope: BudgetScope::PaidOnly,
            },
            billing_timezone: "Asia/Bangkok".into(),
            quota: None,
        }
    }

    fn record(
        kind: RecordKind,
        id: &str,
        ts: i64,
        cost: i64,
        input: i64,
        output: i64,
    ) -> TenantRecord {
        let mut payload = Map::new();
        payload.insert("ts_utc".into(), json!(ts));
        payload.insert("cost_minor".into(), json!(cost));
        payload.insert("tokens_in".into(), json!(input));
        payload.insert("tokens_out".into(), json!(output));
        TenantRecord {
            tenant_id: "acme-co".into(),
            kind,
            record_id: id.into(),
            request_id: id.into(),
            origin_record_id: None,
            payload,
        }
    }

    fn put_fixture(store: &TenantStore, kind: RecordKind) {
        for row in [
            record(kind, "in-month", 1_789_430_400_000, 100, 1000, 500),
            record(kind, "boundary-start", 1_788_197_400_000, 1, 10, 1),
            record(kind, "boundary-end", 1_790_785_800_000, 2, 20, 2),
            record(kind, "just-after-oct", 1_790_789_400_000, 1000, 9999, 9999),
            record(kind, "just-before-sep", 1_788_193_800_000, 1000, 9999, 9999),
        ] {
            store.put(Some("acme-co"), &row).unwrap();
        }
    }

    #[test]
    fn monthly_usage_matches_the_ts_bangkok_fixture_and_half_open_edges() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        put_fixture(&store, RecordKind::Usage);
        let usage = query_monthly_usage(&store, Some(&tenant()), "2026-09", None).unwrap();
        assert_eq!(usage.tenant_id, "acme-co");
        assert_eq!(usage.period, "2026-09");
        assert_eq!(usage.billing_timezone, "Asia/Bangkok");
        assert_eq!(usage.range_utc.from, "2026-08-31T17:00:00Z");
        assert_eq!(usage.range_utc.to, "2026-09-30T17:00:00Z");
        assert_eq!(
            usage.totals,
            UsageTotals {
                cost_minor: 103.0,
                tokens_in: 1030.0,
                tokens_out: 503.0,
                calls: 3
            }
        );
    }

    #[test]
    fn explicit_audit_source_never_double_counts_usage_rows() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        put_fixture(&store, RecordKind::Usage);
        put_fixture(&store, RecordKind::Audit);
        let usage = query_monthly_usage(&store, Some(&tenant()), "2026-09", None).unwrap();
        let audit =
            query_monthly_usage(&store, Some(&tenant()), "2026-09", Some(UsageSource::Audit))
                .unwrap();
        assert_eq!(usage.totals.cost_minor, 103.0);
        assert_eq!(audit.totals, usage.totals);
    }

    #[test]
    fn exact_from_is_included_and_exact_to_is_excluded() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let range = resolve_billing_period_range("2026-09", "Asia/Bangkok").unwrap();
        for row in [
            record(RecordKind::Usage, "at-from", range.from, 7, 1, 1),
            record(RecordKind::Usage, "at-to", range.to, 99, 9, 9),
        ] {
            store.put(Some("acme-co"), &row).unwrap();
        }
        let usage = query_monthly_usage(&store, Some(&tenant()), "2026-09", None).unwrap();
        assert_eq!(usage.totals.cost_minor, 7.0);
        assert_eq!(usage.totals.calls, 1);
    }

    #[test]
    fn malformed_time_and_in_range_numeric_fields_fail_closed() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let mut bad_time = record(RecordKind::Usage, "bad-time", 1_789_430_400_000, 1, 1, 1);
        bad_time
            .payload
            .insert("ts_utc".into(), json!("not-a-time"));
        store.put(Some("acme-co"), &bad_time).unwrap();
        assert!(matches!(
            query_monthly_usage(&store, Some(&tenant()), "2026-09", None),
            Err(BillingAggregateError::InvalidRecord {
                field: "ts_utc",
                ..
            })
        ));

        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let mut bad_cost = record(RecordKind::Usage, "bad-cost", 1_789_430_400_000, 1, 1, 1);
        bad_cost.payload.insert("cost_minor".into(), json!("1"));
        store.put(Some("acme-co"), &bad_cost).unwrap();
        assert!(matches!(
            query_monthly_usage(&store, Some(&tenant()), "2026-09", None),
            Err(BillingAggregateError::InvalidRecord {
                field: "cost_minor",
                ..
            })
        ));
    }

    #[test]
    fn missing_scope_is_rejected_before_any_unscoped_query() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        assert!(matches!(
            query_monthly_usage(&store, None, "2026-09", None),
            Err(BillingAggregateError::ScopeRequired)
        ));
    }

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
