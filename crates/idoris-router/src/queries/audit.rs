use idoris_tenancy::store::{RecordKind, StoreError, TenantRecord, TenantStore};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuditQuery {
    pub from: Option<i64>,
    pub to: Option<i64>,
    pub limit: Option<usize>,
    pub record_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditRecordView {
    pub record_id: String,
    pub request_id: String,
    pub origin_record_id: Option<String>,
    pub payload: Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AuditResponse {
    pub tenant_id: String,
    pub records: Vec<AuditRecordView>,
}

#[derive(Debug, thiserror::Error)]
pub enum AuditQueryError {
    #[error("X-iDoris-Tenant is required for tenant audit queries")]
    ScopeRequired,
    #[error("path tenant does not match X-iDoris-Tenant")]
    ScopeMismatch,
    #[error("audit query limit must be between 1 and {MAX_LIMIT}")]
    InvalidLimit,
    #[error("audit query requires from < to when both are present")]
    InvalidRange,
    #[error("record {record_id:?} has invalid ts_utc")]
    InvalidRecord { record_id: String },
    #[error("tenant audit record store lock is poisoned")]
    StorePoisoned,
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn query_audit(
    store: &TenantStore,
    path_tenant: &str,
    scope_tenant: Option<&str>,
    query: &AuditQuery,
) -> Result<AuditResponse, AuditQueryError> {
    let scope_tenant = scope_tenant
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(AuditQueryError::ScopeRequired)?;
    if scope_tenant != path_tenant {
        return Err(AuditQueryError::ScopeMismatch);
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(AuditQueryError::InvalidLimit);
    }
    if let (Some(from), Some(to)) = (query.from, query.to)
        && from >= to
    {
        return Err(AuditQueryError::InvalidRange);
    }

    let records = store.list(Some(path_tenant), Some(RecordKind::Audit))?;
    let mut filtered = Vec::new();
    for record in records {
        if query
            .record_id
            .as_deref()
            .is_some_and(|wanted| wanted != record.record_id)
        {
            continue;
        }
        let ts = audit_timestamp(&record)?;
        if query.from.is_some_and(|from| ts < from as f64)
            || query.to.is_some_and(|to| ts >= to as f64)
        {
            continue;
        }
        filtered.push((ts, record));
    }
    filtered.sort_by(|(left_ts, left), (right_ts, right)| {
        left_ts
            .total_cmp(right_ts)
            .then_with(|| left.record_id.cmp(&right.record_id))
    });
    filtered.truncate(limit);

    Ok(AuditResponse {
        tenant_id: path_tenant.to_string(),
        records: filtered
            .into_iter()
            .map(|(_, record)| AuditRecordView {
                record_id: record.record_id,
                request_id: record.request_id,
                origin_record_id: record.origin_record_id,
                payload: record.payload,
            })
            .collect(),
    })
}

fn audit_timestamp(record: &TenantRecord) -> Result<f64, AuditQueryError> {
    record
        .payload
        .get("ts_utc")
        .and_then(Value::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| AuditQueryError::InvalidRecord {
            record_id: record.record_id.clone(),
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use rusqlite::Connection;
    use serde_json::json;

    use super::*;

    fn record(tenant: &str, id: &str, ts: Value) -> TenantRecord {
        let mut payload = Map::new();
        payload.insert("ts_utc".into(), ts);
        payload.insert("reason".into(), json!("intent_match"));
        TenantRecord {
            tenant_id: tenant.into(),
            kind: RecordKind::Audit,
            record_id: id.into(),
            request_id: format!("request-{id}"),
            origin_record_id: None,
            payload,
        }
    }

    fn store() -> TenantStore {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        for row in [
            record("acme", "b", json!(2000)),
            record("acme", "a", json!(2000.5)),
            record("acme", "c", json!(3000)),
            record("beta", "private", json!(1500)),
        ] {
            let tenant = row.tenant_id.clone();
            store.put(Some(&tenant), &row).unwrap();
        }
        store
    }

    #[test]
    fn range_record_id_limit_and_order_are_stable() {
        let store = store();
        let query = AuditQuery {
            from: Some(1500),
            to: Some(3000),
            limit: Some(2),
            record_id: None,
        };
        let response = query_audit(&store, "acme", Some("acme"), &query).unwrap();
        assert_eq!(response.tenant_id, "acme");
        assert_eq!(
            response
                .records
                .iter()
                .map(|record| record.record_id.as_str())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );

        let exact = query_audit(
            &store,
            "acme",
            Some("acme"),
            &AuditQuery {
                from: None,
                to: None,
                limit: None,
                record_id: Some("c".into()),
            },
        )
        .unwrap();
        assert_eq!(exact.records.len(), 1);
        assert_eq!(exact.records[0].record_id, "c");
    }

    #[test]
    fn tenant_scope_and_query_bounds_fail_closed() {
        let store = store();
        let base = AuditQuery {
            from: None,
            to: None,
            limit: None,
            record_id: None,
        };
        assert!(matches!(
            query_audit(&store, "acme", None, &base),
            Err(AuditQueryError::ScopeRequired)
        ));
        assert!(matches!(
            query_audit(&store, "acme", Some("beta"), &base),
            Err(AuditQueryError::ScopeMismatch)
        ));
        for limit in [0, MAX_LIMIT + 1] {
            assert!(matches!(
                query_audit(
                    &store,
                    "acme",
                    Some("acme"),
                    &AuditQuery {
                        limit: Some(limit),
                        ..base.clone()
                    },
                ),
                Err(AuditQueryError::InvalidLimit)
            ));
        }
        assert!(matches!(
            query_audit(
                &store,
                "acme",
                Some("acme"),
                &AuditQuery {
                    from: Some(10),
                    to: Some(10),
                    ..base
                },
            ),
            Err(AuditQueryError::InvalidRange)
        ));
    }

    #[test]
    fn malformed_timestamp_is_loud_not_silently_skipped() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        store
            .put(Some("acme"), &record("acme", "bad", json!("not-a-time")))
            .unwrap();
        let error = query_audit(
            &store,
            "acme",
            Some("acme"),
            &AuditQuery {
                from: None,
                to: None,
                limit: None,
                record_id: None,
            },
        )
        .unwrap_err();
        assert!(matches!(error, AuditQueryError::InvalidRecord { .. }));
    }
}
