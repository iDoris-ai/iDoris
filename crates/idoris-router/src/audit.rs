use std::time::{SystemTime, UNIX_EPOCH};

use idoris_contracts::tenant::TenantContext;
use idoris_tenancy::store::{RecordKind, StoreError, TenantRecord, TenantStore};
use serde_json::{Map, Number, Value};

use crate::reason::ReasonKind;

pub const AUDIT_FIELDS: [&str; 15] = [
    "request_id",
    "tenant_id",
    "component",
    "intent",
    "privacy",
    "tier",
    "provider_id",
    "model_id",
    "tokens_in",
    "tokens_out",
    "cost_minor",
    "latency_ms",
    "status",
    "reason",
    "ts_utc",
];

const CONTENT_FIELDS: [&str; 32] = [
    "prompt",
    "prompts",
    "input",
    "inputs",
    "content",
    "contents",
    "text",
    "texts",
    "body",
    "messages",
    "message",
    "document",
    "documents",
    "file",
    "files",
    "payload",
    "completion",
    "completions",
    "response",
    "responses",
    "output",
    "outputs",
    "query",
    "answer",
    "raw",
    "attachment",
    "attachments",
    "image",
    "images",
    "audio",
    "transcript",
    "data",
];

pub const MAX_FIELD_UTF16_UNITS: usize = 500;

#[derive(Debug)]
pub enum AuditError {
    ScopeRequired,
    TenantMismatch,
    ContentField(String),
    UnknownField(String),
    NonScalar(String),
    FieldTooLong { field: String, units: usize },
    InvalidReason,
    InvalidTimestamp,
    Store(StoreError),
}

impl std::fmt::Display for AuditError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ScopeRequired => write!(f, "audit writes require a tenant context"),
            Self::TenantMismatch => write!(f, "audit tenant_id must match the tenant context"),
            Self::ContentField(field) => write!(f, "refusing content field {field:?}"),
            Self::UnknownField(field) => write!(f, "unknown audit field {field:?}"),
            Self::NonScalar(field) => write!(f, "audit field {field:?} must be scalar"),
            Self::FieldTooLong { field, units } => {
                write!(f, "audit field {field:?} is {units} UTF-16 units (> 500)")
            }
            Self::InvalidReason => write!(f, "audit reason must identify a supported reason kind"),
            Self::InvalidTimestamp => write!(f, "ts_utc must be a finite UTC epoch number"),
            Self::Store(err) => err.fmt(f),
        }
    }
}

impl std::error::Error for AuditError {}

impl From<StoreError> for AuditError {
    fn from(value: StoreError) -> Self {
        Self::Store(value)
    }
}

pub struct AuditWriter<'a> {
    store: &'a TenantStore,
}

impl<'a> AuditWriter<'a> {
    pub fn new(store: &'a TenantStore) -> Self {
        Self { store }
    }

    pub fn write(
        &self,
        context: Option<&TenantContext>,
        input: &Map<String, Value>,
    ) -> Result<TenantRecord, AuditError> {
        let tenant_id = context
            .map(|ctx| js_trim(&ctx.tenant_id))
            .filter(|value| !value.is_empty())
            .ok_or(AuditError::ScopeRequired)?;
        let mut payload = Map::new();
        for (field, value) in input {
            let lower = field.to_ascii_lowercase();
            if CONTENT_FIELDS.contains(&lower.as_str()) {
                return Err(AuditError::ContentField(field.clone()));
            }
            if !AUDIT_FIELDS.contains(&field.as_str()) {
                return Err(AuditError::UnknownField(field.clone()));
            }
            if matches!(value, Value::Array(_) | Value::Object(_)) {
                return Err(AuditError::NonScalar(field.clone()));
            }
            if let Value::String(text) = value {
                let units = text.encode_utf16().count();
                if units > MAX_FIELD_UTF16_UNITS {
                    return Err(AuditError::FieldTooLong {
                        field: field.clone(),
                        units,
                    });
                }
            }
            payload.insert(field.clone(), value.clone());
        }

        let reason = payload
            .get("reason")
            .and_then(Value::as_str)
            .ok_or(AuditError::InvalidReason)?;
        validate_reason(reason)?;
        if let Some(value) = payload.get("tenant_id")
            && value.as_str() != Some(tenant_id)
        {
            return Err(AuditError::TenantMismatch);
        }
        payload.insert("tenant_id".into(), Value::String(tenant_id.to_string()));

        let ts = match payload.get("ts_utc") {
            Some(Value::Number(number)) if number.as_f64().is_some_and(f64::is_finite) => {
                number.clone()
            }
            Some(_) => return Err(AuditError::InvalidTimestamp),
            None => Number::from(now_epoch_ms()?),
        };
        payload.insert("ts_utc".into(), Value::Number(ts.clone()));

        let fallback_id = ts.to_string();
        let request_id = payload
            .get("request_id")
            .and_then(record_id_scalar)
            .unwrap_or_else(|| fallback_id.clone());
        let record = TenantRecord {
            tenant_id: tenant_id.to_string(),
            kind: RecordKind::Audit,
            record_id: request_id.clone(),
            request_id,
            origin_record_id: None,
            payload,
        };
        self.store.put(Some(tenant_id), &record)?;
        Ok(record)
    }
}

fn record_id_scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value.clone()),
        Value::Number(value) => Some(value.to_string()),
        Value::Bool(_) | Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

fn validate_reason(reason: &str) -> Result<ReasonKind, AuditError> {
    let trimmed = js_trim(reason);
    if trimmed.is_empty() {
        return Err(AuditError::InvalidReason);
    }
    let mut parts = trimmed.splitn(2, ':');
    let kind =
        ReasonKind::parse(js_trim(parts.next().unwrap_or(""))).ok_or(AuditError::InvalidReason)?;
    if parts
        .next()
        .is_some_and(|detail| js_trim(detail).is_empty())
    {
        return Err(AuditError::InvalidReason);
    }
    Ok(kind)
}

fn js_trim(value: &str) -> &str {
    value.trim_matches(|c: char| (c.is_whitespace() && c != '\u{0085}') || c == '\u{feff}')
}

fn now_epoch_ms() -> Result<i64, AuditError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(i64::MAX as u128) as i64)
        .map_err(|_| AuditError::InvalidTimestamp)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use idoris_contracts::tenant::{Budget, BudgetScope};
    use rusqlite::Connection;
    use serde_json::json;

    use super::*;

    fn context(id: &str) -> TenantContext {
        TenantContext {
            tenant_id: id.into(),
            budget: Budget {
                limit_minor: 0,
                spent_minor: 0,
                scope: BudgetScope::PaidOnly,
            },
            billing_timezone: "UTC".into(),
            quota: None,
        }
    }

    fn input(reason: &str) -> Map<String, Value> {
        let mut map = Map::new();
        map.insert("request_id".into(), json!("r1"));
        map.insert("reason".into(), json!(reason));
        map
    }

    #[test]
    fn writes_only_valid_scoped_metadata() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let writer = AuditWriter::new(&store);
        let row = writer
            .write(Some(&context("acme")), &input("intent_match: rule-1"))
            .unwrap();
        assert_eq!(row.tenant_id, "acme");
        assert_eq!(
            store
                .list(Some("acme"), Some(RecordKind::Audit))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn content_keys_are_rejected_before_any_write() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let writer = AuditWriter::new(&store);
        let mut record = input("intent_match");
        record.insert("CONTENT".into(), json!("SENTINEL-SECRET"));
        assert!(matches!(
            writer.write(Some(&context("acme")), &record),
            Err(AuditError::ContentField(_))
        ));
        assert!(
            store
                .list(Some("acme"), Some(RecordKind::Audit))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn reason_tenant_scalar_and_timestamp_validation_fail_closed() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let writer = AuditWriter::new(&store);
        assert!(matches!(
            writer.write(None, &input("budget")),
            Err(AuditError::ScopeRequired)
        ));
        for bad_reason in ["", "routed", "budget:"] {
            assert!(matches!(
                writer.write(Some(&context("acme")), &input(bad_reason)),
                Err(AuditError::InvalidReason)
            ));
        }
        let mut unknown = input("budget");
        unknown.insert("surprise".into(), json!(1));
        assert!(matches!(
            writer.write(Some(&context("acme")), &unknown),
            Err(AuditError::UnknownField(_))
        ));
        let mut wrong_tenant = input("budget");
        wrong_tenant.insert("tenant_id".into(), json!("other"));
        assert!(matches!(
            writer.write(Some(&context("acme")), &wrong_tenant),
            Err(AuditError::TenantMismatch)
        ));
        let mut object = input("budget");
        object.insert("status".into(), json!({"bad": true}));
        assert!(matches!(
            writer.write(Some(&context("acme")), &object),
            Err(AuditError::NonScalar(_))
        ));
        let mut timestamp = input("budget");
        timestamp.insert("ts_utc".into(), json!("now"));
        assert!(matches!(
            writer.write(Some(&context("acme")), &timestamp),
            Err(AuditError::InvalidTimestamp)
        ));
    }

    #[test]
    fn field_limit_matches_javascript_utf16_length() {
        let store = TenantStore::new(Connection::open_in_memory().unwrap()).unwrap();
        let writer = AuditWriter::new(&store);
        let mut accepted = input("degraded");
        accepted.insert("component".into(), json!("😀".repeat(250)));
        writer.write(Some(&context("acme")), &accepted).unwrap();
        let mut ascii_500 = input("degraded");
        ascii_500.insert("request_id".into(), json!("ascii-500"));
        ascii_500.insert("component".into(), json!("a".repeat(500)));
        writer.write(Some(&context("acme")), &ascii_500).unwrap();

        let mut rejected = input("degraded");
        rejected.insert("request_id".into(), json!("r2"));
        rejected.insert("component".into(), json!("😀".repeat(251)));
        assert!(matches!(
            writer.write(Some(&context("acme")), &rejected),
            Err(AuditError::FieldTooLong { .. })
        ));
        let mut ascii_501 = input("degraded");
        ascii_501.insert("request_id".into(), json!("ascii-501"));
        ascii_501.insert("component".into(), json!("a".repeat(501)));
        assert!(matches!(
            writer.write(Some(&context("acme")), &ascii_501),
            Err(AuditError::FieldTooLong { .. })
        ));
    }
}
