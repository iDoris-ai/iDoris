use serde_json::{Map, Value, json};
use thiserror::Error;

use crate::store::{RecordKind, StoreError, TenantRecord, TenantStore};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageEntry {
    pub ts_utc: i64,
    pub cost_minor: Option<i64>,
    pub tokens_in: Option<u64>,
    pub tokens_out: Option<u64>,
    pub request_id: Option<String>,
}

#[derive(Debug, Error)]
pub enum UsageError {
    #[error("usage cost_minor must be >= 0 when known")]
    InvalidCost,
    #[error(transparent)]
    Store(#[from] StoreError),
}

pub fn write_usage_record(
    store: &TenantStore,
    tenant_id: Option<&str>,
    record_id: &str,
    entry: &UsageEntry,
) -> Result<TenantRecord, UsageError> {
    if entry.cost_minor.is_some_and(|value| value < 0) {
        return Err(UsageError::InvalidCost);
    }
    let tenant_id = tenant_id
        .filter(|value| !value.trim().is_empty())
        .ok_or(StoreError::ScopeRequired)?;
    let request_id = entry
        .request_id
        .clone()
        .unwrap_or_else(|| record_id.to_string());
    let mut payload = Map::new();
    payload.insert("ts_utc".into(), json!(entry.ts_utc));
    if let Some(value) = entry.cost_minor {
        payload.insert("cost_minor".into(), json!(value));
    }
    if let Some(value) = entry.tokens_in {
        payload.insert("tokens_in".into(), json!(value));
    }
    if let Some(value) = entry.tokens_out {
        payload.insert("tokens_out".into(), json!(value));
    }
    if let Some(value) = &entry.request_id {
        payload.insert("request_id".into(), Value::String(value.clone()));
    }
    let record = TenantRecord {
        tenant_id: tenant_id.to_string(),
        kind: RecordKind::Usage,
        record_id: record_id.to_string(),
        request_id,
        origin_record_id: None,
        payload,
    };
    store.put(Some(tenant_id), &record)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use rusqlite::Connection;

    use super::*;

    #[test]
    fn unknown_tokens_are_omitted_not_written_as_zero() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let entry = UsageEntry {
            ts_utc: 1,
            cost_minor: Some(0),
            tokens_in: None,
            tokens_out: None,
            request_id: Some("req".into()),
        };
        let row = write_usage_record(&store, Some("acme"), "record", &entry).unwrap();
        assert_eq!(row.payload["cost_minor"], json!(0));
        assert!(!row.payload.contains_key("tokens_in"));
        assert!(!row.payload.contains_key("tokens_out"));
    }
}
