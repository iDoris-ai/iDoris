use idoris_tenancy::event_log::{
    EventLogError, EventLogEvent, EventLogQuery, EventLogStore, EventType,
};
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
    EventLog(#[from] EventLogError),
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

pub fn query_event_audit(
    store: &EventLogStore,
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

    let finalized = store.events_for_tenant_type(
        Some(path_tenant),
        EventType::AuditFinalized,
        &EventLogQuery {
            from_ts_utc_ms: query.from,
            to_ts_utc_ms: query.to,
            record_id: query.record_id.as_deref(),
            limit,
        },
    )?;
    let mut response = AuditResponse {
        tenant_id: path_tenant.to_string(),
        records: Vec::with_capacity(finalized.len()),
    };
    for terminal in finalized {
        let events = store.events_for_record(Some(path_tenant), &terminal.event.record_id)?;
        let mut projected = project_event_audit(path_tenant, &events);
        if let Some(record) = projected.records.pop() {
            response.records.push(record);
        }
    }
    Ok(response)
}

pub fn project_event_audit(path_tenant: &str, events: &[EventLogEvent]) -> AuditResponse {
    let mut records: Vec<AuditRecordView> = Vec::new();
    for row in events {
        let event = &row.event;
        if event.tenant_id != path_tenant {
            continue;
        }
        let index = records
            .iter()
            .position(|record| record.record_id == event.record_id)
            .unwrap_or_else(|| {
                let mut payload = Map::new();
                payload.insert(
                    "request_id".into(),
                    serde_json::json!(event.request_id.as_deref().unwrap_or(&event.record_id)),
                );
                payload.insert("component".into(), serde_json::json!("router"));
                records.push(AuditRecordView {
                    record_id: event.record_id.clone(),
                    request_id: event
                        .request_id
                        .clone()
                        .unwrap_or_else(|| event.record_id.clone()),
                    origin_record_id: event.origin_record_id.clone(),
                    payload,
                });
                records.len() - 1
            });
        let record = &mut records[index];
        if record.origin_record_id.is_none() {
            record.origin_record_id = event.origin_record_id.clone();
        }
        for key in ["intent", "privacy", "tier", "provider_id", "model_id"] {
            if let Some(value) = event.metadata.get(key) {
                record.payload.insert(key.to_string(), value.clone());
            }
        }
        if event.event_type == idoris_tenancy::event_log::EventType::BudgetSettled
            && let Some(value) = event.metadata.get("settled_minor")
        {
            record.payload.insert("cost_minor".into(), value.clone());
        }
        if event.event_type == EventType::AuditFinalized {
            record
                .payload
                .insert("ts_utc".into(), serde_json::json!(event.ts_utc_ms));
            for (source, target) in [
                ("http_status", "status"),
                ("reason", "reason"),
                ("latency_ms", "latency_ms"),
            ] {
                if let Some(value) = event.metadata.get(source) {
                    record.payload.insert(target.to_string(), value.clone());
                }
            }
        }
    }
    AuditResponse {
        tenant_id: path_tenant.to_string(),
        records,
    }
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

    #[test]
    fn event_projection_groups_records_without_inventing_legacy_fields() {
        use idoris_tenancy::event_log::{EventLogEvent, EventType, NewEvent};
        use std::collections::BTreeMap;

        let event = |sequence, record_id: &str, event_type, ts, metadata| EventLogEvent {
            sequence,
            event: NewEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                tenant_id: "acme".into(),
                record_id: record_id.into(),
                event_type,
                ts_utc_ms: ts,
                request_id: None,
                session_id: None,
                trace_id: None,
                parent_id: None,
                origin_record_id: None,
                metadata,
            },
        };
        let rows = vec![
            event(
                1,
                "r1",
                EventType::Profiled,
                100,
                BTreeMap::from([
                    ("privacy".into(), json!("local_only")),
                    ("tokens_in".into(), json!(999)),
                    ("tokens_out".into(), json!(888)),
                    ("latency_ms".into(), json!(777)),
                    ("cost_minor".into(), json!(666)),
                    ("settled_minor".into(), json!(555)),
                ]),
            ),
            event(
                2,
                "r1",
                EventType::BudgetSettled,
                200,
                BTreeMap::from([("settled_minor".into(), json!(7))]),
            ),
            event(
                3,
                "r1",
                EventType::AuditFinalized,
                225,
                BTreeMap::from([
                    ("http_status".into(), json!(200)),
                    ("reason".into(), json!("intent_match: routed")),
                    ("latency_ms".into(), json!(12)),
                ]),
            ),
            event(4, "r2", EventType::RequestReceived, 300, BTreeMap::new()),
        ];

        let projected = project_event_audit("acme", &rows);
        assert_eq!(projected.records.len(), 2);
        assert_eq!(projected.records[0].record_id, "r1");
        assert_eq!(projected.records[0].request_id, "r1");
        assert_eq!(projected.records[0].payload["component"], json!("router"));
        assert_eq!(projected.records[0].payload["privacy"], json!("local_only"));
        assert_eq!(projected.records[0].payload["cost_minor"], json!(7));
        assert_eq!(projected.records[0].payload["ts_utc"], json!(225));
        assert_eq!(projected.records[0].payload["status"], json!(200));
        assert_eq!(
            projected.records[0].payload["reason"],
            json!("intent_match: routed")
        );
        assert!(!projected.records[0].payload.contains_key("tokens_in"));
        assert!(!projected.records[0].payload.contains_key("tokens_out"));
        assert_eq!(projected.records[0].payload["latency_ms"], json!(12));
        assert_eq!(projected.records[1].record_id, "r2");
    }

    #[test]
    fn event_audit_query_limits_finalized_records_not_intermediate_events() {
        use idoris_tenancy::event_log::{EventLogStore, EventType, NewEvent};
        use std::collections::BTreeMap;

        let store = EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let append = |record_id: &str,
                      event_type: EventType,
                      ts_utc_ms: i64,
                      metadata: BTreeMap<String, Value>| {
            let event = NewEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                tenant_id: "acme".into(),
                record_id: record_id.into(),
                event_type,
                ts_utc_ms,
                request_id: Some(format!("request-{record_id}")),
                session_id: None,
                trace_id: None,
                parent_id: None,
                origin_record_id: None,
                metadata,
            };
            store.append(Some("acme"), &event).unwrap();
        };

        for ts in 1..=20 {
            append(
                "noise",
                EventType::StreamChunk,
                ts,
                BTreeMap::from([("status".into(), json!("chunk"))]),
            );
        }
        append(
            "a",
            EventType::Decided,
            150,
            BTreeMap::from([
                ("provider_id".into(), json!("omlx")),
                ("model_id".into(), json!("model-a")),
            ]),
        );
        append(
            "a",
            EventType::BudgetSettled,
            175,
            BTreeMap::from([("settled_minor".into(), json!(9))]),
        );
        for (record_id, ts, status) in [("b", 200, 201), ("a", 200, 202), ("c", 300, 203)] {
            append(
                record_id,
                EventType::AuditFinalized,
                ts,
                BTreeMap::from([
                    ("component".into(), json!("router")),
                    ("http_status".into(), json!(status)),
                    ("reason".into(), json!("intent_match: routed")),
                    ("latency_ms".into(), json!(7)),
                    ("privacy".into(), json!("local_only")),
                    ("intent".into(), json!("chat")),
                ]),
            );
        }
        append(
            "a",
            EventType::FeedbackReceived,
            250,
            BTreeMap::from([("rating".into(), json!(1))]),
        );

        let response = query_event_audit(
            &store,
            "acme",
            Some("acme"),
            &AuditQuery {
                from: Some(100),
                to: Some(300),
                limit: Some(2),
                record_id: None,
            },
        )
        .unwrap();
        assert_eq!(
            response
                .records
                .iter()
                .map(|record| record.record_id.as_str())
                .collect::<Vec<_>>(),
            ["a", "b"]
        );
        assert_eq!(response.records[0].payload["status"], json!(202));
        assert_eq!(response.records[0].payload["latency_ms"], json!(7));
        assert_eq!(response.records[0].payload["privacy"], json!("local_only"));
        assert_eq!(response.records[0].payload["intent"], json!("chat"));
        assert_eq!(response.records[0].payload["provider_id"], json!("omlx"));
        assert_eq!(response.records[0].payload["model_id"], json!("model-a"));
        assert_eq!(response.records[0].payload["cost_minor"], json!(9));
        assert_eq!(response.records[0].payload["ts_utc"], json!(200));
    }

    #[test]
    fn event_audit_query_preserves_scope_and_query_validation() {
        let store =
            idoris_tenancy::event_log::EventLogStore::new(Connection::open_in_memory().unwrap())
                .unwrap();
        let base = AuditQuery {
            from: None,
            to: None,
            limit: None,
            record_id: None,
        };
        assert!(matches!(
            query_event_audit(&store, "acme", None, &base),
            Err(AuditQueryError::ScopeRequired)
        ));
        assert!(matches!(
            query_event_audit(&store, "acme", Some("other"), &base),
            Err(AuditQueryError::ScopeMismatch)
        ));
        assert!(matches!(
            query_event_audit(
                &store,
                "acme",
                Some("acme"),
                &AuditQuery {
                    from: Some(10),
                    to: Some(10),
                    ..base.clone()
                }
            ),
            Err(AuditQueryError::InvalidRange)
        ));
        assert!(matches!(
            query_event_audit(
                &store,
                "acme",
                Some("acme"),
                &AuditQuery {
                    limit: Some(MAX_LIMIT + 1),
                    ..base
                }
            ),
            Err(AuditQueryError::InvalidLimit)
        ));
    }
}
