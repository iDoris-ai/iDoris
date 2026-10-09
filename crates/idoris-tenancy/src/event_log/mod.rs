use std::{collections::BTreeMap, path::Path, sync::Mutex};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

const MAX_STRING_UTF16_UNITS: usize = 500;
const MAX_ARRAY_ITEMS: usize = 32;
const SCALAR_KEYS: &str = "component,intent,privacy,tier,provider_id,model_id,tokens_in,tokens_out,cost_minor,latency_ms,status,http_status,reason,rule_id,served_locality,degraded,cached,reserved_minor,settled_minor,price_version,rating,outcome,failure_mode,sensitivity,training_eligible,audit_request_id";
const ARRAY_KEYS: &str = "reason_codes,labels";
const OBJECT_ARRAY_KEYS: &str = "rubric";
const CONTENT_KEYS: &str = "prompt,prompts,input,inputs,content,contents,text,texts,body,messages,message,response,responses,output,outputs,completion,completions,corrected_output,query,answer,raw,data";

macro_rules! event_types {
    ($($variant:ident => $name:literal),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum EventType { $($variant),+ }
        impl EventType {
            pub const fn as_str(self) -> &'static str { match self { $(Self::$variant => $name),+ } }
            fn parse(value: &str) -> Result<Self, EventLogError> { match value { $($name => Ok(Self::$variant),)+ _ => Err(EventLogError::UnknownEventType) } }
        }
    };
}
event_types! {
    RequestReceived => "request.received", Inspected => "inspected", Profiled => "profiled",
    Decided => "decided", BudgetReserved => "budget.reserved", Dispatched => "dispatched",
    StreamStarted => "stream.started", StreamChunk => "stream.chunk", StreamCompleted => "stream.completed", Completed => "completed",
    BudgetSettled => "budget.settled", AuditFinalized => "audit.finalized", FeedbackReceived => "feedback.received",
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewEvent {
    pub event_id: String,
    pub tenant_id: String,
    pub record_id: String,
    pub event_type: EventType,
    pub ts_utc_ms: i64,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub trace_id: Option<String>,
    pub parent_id: Option<String>,
    pub origin_record_id: Option<String>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EventLogEvent {
    pub sequence: i64,
    pub event: NewEvent,
}

#[derive(Debug, Clone, Default)]
pub struct EventLogQuery<'a> {
    pub from_ts_utc_ms: Option<i64>,
    pub to_ts_utc_ms: Option<i64>,
    pub record_id: Option<&'a str>,
    pub limit: usize,
}

#[derive(Debug, Error)]
pub enum EventLogError {
    #[error("tenant scope is required")]
    ScopeRequired,
    #[error("event tenant does not match scope")]
    TenantMismatch,
    #[error("event_id must be a version-4 UUID")]
    InvalidEventId,
    #[error("record_id is required")]
    InvalidRecordId,
    #[error("event log query limit must be positive")]
    InvalidLimit,
    #[error("invalid identifier field {0}")]
    InvalidIdentifier(&'static str),
    #[error("event timestamp must be non-negative")]
    InvalidTimestamp,
    #[error("unknown event type")]
    UnknownEventType,
    #[error("event_id replay differs from the stored event")]
    IdempotencyConflict,
    #[error("metadata field {0:?} is not allowed")]
    InvalidMetadata(String),
    #[error("event log connection mutex is poisoned")]
    LockPoisoned,
    #[error("event log schema is incomplete")]
    SchemaIncomplete,
    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

pub struct EventLogStore(Mutex<Connection>);

impl EventLogStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, EventLogError> {
        Self::new(Connection::open(path)?)
    }

    pub fn new(mut conn: Connection) -> Result<Self, EventLogError> {
        migrate(&mut conn)?;
        Ok(Self(Mutex::new(conn)))
    }

    pub fn append(&self, scope: Option<&str>, event: &NewEvent) -> Result<i64, EventLogError> {
        let scope = required_scope(scope)?;
        validate_event(scope, event)?;
        let metadata = serde_json::to_string(&event.metadata)?;
        let mut conn = self.0.lock().map_err(|_| EventLogError::LockPoisoned)?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(existing) = load_by_id(&tx, &event.event_id)? {
            if existing.event != *event {
                return Err(EventLogError::IdempotencyConflict);
            }
            tx.commit()?;
            return Ok(existing.sequence);
        }
        tx.execute(
            "INSERT INTO event_log_events (event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
            rusqlite::params![event.event_id, scope, event.record_id, event.event_type.as_str(), event.ts_utc_ms, event.request_id, event.session_id, event.trace_id, event.parent_id, event.origin_record_id, metadata],
        )?;
        let sequence = tx.last_insert_rowid();
        tx.commit()?;
        Ok(sequence)
    }

    pub fn events_for_record(
        &self,
        scope: Option<&str>,
        record_id: &str,
    ) -> Result<Vec<EventLogEvent>, EventLogError> {
        let scope = required_scope(scope)?;
        if record_id.trim().is_empty() {
            return Err(EventLogError::InvalidRecordId);
        }
        validate_id("record_id", record_id)?;
        let conn = self.0.lock().map_err(|_| EventLogError::LockPoisoned)?;
        let mut stmt = conn.prepare("SELECT sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata FROM event_log_events WHERE tenant_id=?1 AND record_id=?2 ORDER BY sequence")?;
        let rows = stmt.query_map(rusqlite::params![scope, record_id], raw_event)?;
        rows.map(|row| decode(row?)).collect()
    }

    pub fn events_for_tenant(
        &self,
        scope: Option<&str>,
        query: &EventLogQuery<'_>,
    ) -> Result<Vec<EventLogEvent>, EventLogError> {
        let scope = required_scope(scope)?;
        if let Some(record_id) = query.record_id {
            if record_id.trim().is_empty() {
                return Err(EventLogError::InvalidRecordId);
            }
            validate_id("record_id", record_id)?;
        }
        let limit = i64::try_from(query.limit).map_err(|_| EventLogError::InvalidLimit)?;
        if limit <= 0 {
            return Err(EventLogError::InvalidLimit);
        }
        let conn = self.0.lock().map_err(|_| EventLogError::LockPoisoned)?;
        let mut stmt = conn.prepare(
            "SELECT sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata \
             FROM event_log_events \
             WHERE tenant_id=?1 \
               AND (?2 IS NULL OR ts_utc_ms>=?2) \
               AND (?3 IS NULL OR ts_utc_ms<?3) \
               AND (?4 IS NULL OR record_id=?4) \
             ORDER BY sequence \
             LIMIT ?5",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                scope,
                query.from_ts_utc_ms,
                query.to_ts_utc_ms,
                query.record_id,
                limit
            ],
            raw_event,
        )?;
        rows.map(|row| decode(row?)).collect()
    }

    pub fn events_for_tenant_type(
        &self,
        scope: Option<&str>,
        event_type: EventType,
        query: &EventLogQuery<'_>,
    ) -> Result<Vec<EventLogEvent>, EventLogError> {
        let scope = required_scope(scope)?;
        if let Some(record_id) = query.record_id {
            if record_id.trim().is_empty() {
                return Err(EventLogError::InvalidRecordId);
            }
            validate_id("record_id", record_id)?;
        }
        let limit = i64::try_from(query.limit).map_err(|_| EventLogError::InvalidLimit)?;
        if limit <= 0 {
            return Err(EventLogError::InvalidLimit);
        }
        let conn = self.0.lock().map_err(|_| EventLogError::LockPoisoned)?;
        let mut stmt = conn.prepare(
            "WITH ranked AS (\
               SELECT sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata, \
                      ROW_NUMBER() OVER (PARTITION BY record_id ORDER BY sequence DESC) AS rank \
               FROM event_log_events \
               WHERE tenant_id=?1 AND event_type=?2 \
                 AND (?5 IS NULL OR record_id=?5)\
             ) \
             SELECT sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata \
             FROM ranked \
             WHERE rank=1 \
               AND (?3 IS NULL OR ts_utc_ms>=?3) \
               AND (?4 IS NULL OR ts_utc_ms<?4) \
             ORDER BY ts_utc_ms, record_id, sequence \
             LIMIT ?6",
        )?;
        let rows = stmt.query_map(
            rusqlite::params![
                scope,
                event_type.as_str(),
                query.from_ts_utc_ms,
                query.to_ts_utc_ms,
                query.record_id,
                limit
            ],
            raw_event,
        )?;
        rows.map(|row| decode(row?)).collect()
    }
}

fn required_scope(scope: Option<&str>) -> Result<&str, EventLogError> {
    let scope = scope
        .filter(|v| !v.trim().is_empty())
        .ok_or(EventLogError::ScopeRequired)?;
    validate_id("tenant_id", scope)?;
    Ok(scope)
}

fn validate_event(scope: &str, event: &NewEvent) -> Result<(), EventLogError> {
    if event.tenant_id != scope {
        return Err(EventLogError::TenantMismatch);
    }
    validate_stored_event(event)
}

fn validate_stored_event(event: &NewEvent) -> Result<(), EventLogError> {
    let id = Uuid::parse_str(&event.event_id).map_err(|_| EventLogError::InvalidEventId)?;
    if id.get_version_num() != 4 {
        return Err(EventLogError::InvalidEventId);
    }
    validate_id("tenant_id", &event.tenant_id)?;
    if event.record_id.trim().is_empty() {
        return Err(EventLogError::InvalidRecordId);
    }
    validate_id("record_id", &event.record_id)?;
    for (name, value) in [
        ("request_id", &event.request_id),
        ("session_id", &event.session_id),
        ("trace_id", &event.trace_id),
        ("parent_id", &event.parent_id),
        ("origin_record_id", &event.origin_record_id),
    ] {
        if let Some(value) = value {
            validate_id(name, value)?;
        }
    }
    if event.ts_utc_ms < 0 {
        return Err(EventLogError::InvalidTimestamp);
    }
    validate_metadata(&event.metadata)
}

fn validate_id(field: &'static str, value: &str) -> Result<(), EventLogError> {
    if value.trim().is_empty()
        || value.encode_utf16().count() > 128
        || value.chars().any(char::is_control)
    {
        return Err(EventLogError::InvalidIdentifier(field));
    }
    Ok(())
}

fn validate_metadata(metadata: &BTreeMap<String, Value>) -> Result<(), EventLogError> {
    for (key, value) in metadata {
        if key_in(CONTENT_KEYS, key)
            || !key_in(SCALAR_KEYS, key)
                && !key_in(ARRAY_KEYS, key)
                && !key_in(OBJECT_ARRAY_KEYS, key)
        {
            return Err(EventLogError::InvalidMetadata(key.clone()));
        }
        let valid_string = |text: &str| {
            text.encode_utf16().count() <= MAX_STRING_UTF16_UNITS
                && !text.chars().any(char::is_control)
        };
        if key_in(OBJECT_ARRAY_KEYS, key) {
            let ok = value.as_array().is_some_and(|items| {
                items.len() <= MAX_ARRAY_ITEMS
                    && items.iter().all(|item| {
                        let Some(object) = item.as_object() else {
                            return false;
                        };
                        object.len() == 2
                            && object
                                .get("id")
                                .and_then(Value::as_str)
                                .is_some_and(|id| !id.trim().is_empty() && valid_string(id))
                            && object.get("pass").is_some_and(Value::is_boolean)
                    })
            });
            if !ok {
                return Err(EventLogError::InvalidMetadata(key.clone()));
            }
        } else if key_in(ARRAY_KEYS, key) {
            let ok = value.as_array().is_some_and(|items| {
                items.len() <= MAX_ARRAY_ITEMS
                    && items.iter().all(|v| v.as_str().is_some_and(valid_string))
            });
            if !ok {
                return Err(EventLogError::InvalidMetadata(key.clone()));
            }
        } else if matches!(value, Value::Array(_) | Value::Object(_))
            || value.as_str().is_some_and(|s| !valid_string(s))
        {
            return Err(EventLogError::InvalidMetadata(key.clone()));
        }
    }
    Ok(())
}

fn key_in(list: &str, key: &str) -> bool {
    list.split(',').any(|candidate| candidate == key)
}

macro_rules! raw_event {
    ($($field:ident: $ty:ty),+ $(,)?) => {
        struct RawEvent { $($field: $ty),+ }
        fn raw_event(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEvent> {
            Ok(RawEvent { $($field: row.get(stringify!($field))?),+ })
        }
    };
}
raw_event! {
    sequence: i64, event_id: String, tenant_id: String, record_id: String, event_type: String,
    ts_utc_ms: i64, request_id: Option<String>, session_id: Option<String>, trace_id: Option<String>,
    parent_id: Option<String>, origin_record_id: Option<String>, metadata: String,
}

fn decode(raw: RawEvent) -> Result<EventLogEvent, EventLogError> {
    let event = NewEvent {
        event_id: raw.event_id,
        tenant_id: raw.tenant_id,
        record_id: raw.record_id,
        event_type: EventType::parse(&raw.event_type)?,
        ts_utc_ms: raw.ts_utc_ms,
        request_id: raw.request_id,
        session_id: raw.session_id,
        trace_id: raw.trace_id,
        parent_id: raw.parent_id,
        origin_record_id: raw.origin_record_id,
        metadata: serde_json::from_str(&raw.metadata)?,
    };
    validate_stored_event(&event)?;
    Ok(EventLogEvent {
        sequence: raw.sequence,
        event,
    })
}

fn load_by_id(conn: &Connection, event_id: &str) -> Result<Option<EventLogEvent>, EventLogError> {
    conn.query_row("SELECT sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata FROM event_log_events WHERE event_id=?1", [event_id], raw_event).optional()?.map(decode).transpose()
}

fn migrate(conn: &mut Connection) -> Result<(), EventLogError> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS event_log_schema_migrations (version INTEGER PRIMARY KEY)",
    )?;
    let applied = tx
        .query_row(
            "SELECT version FROM event_log_schema_migrations WHERE version=1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if !applied {
        tx.execute_batch(include_str!("migrations/0001_events.sql"))?;
        tx.execute(
            "INSERT INTO event_log_schema_migrations(version) VALUES (1)",
            [],
        )?;
    }
    if tx.prepare("SELECT sequence,event_id,tenant_id,record_id,event_type,ts_utc_ms,request_id,session_id,trace_id,parent_id,origin_record_id,metadata FROM event_log_events LIMIT 0").is_err() {
        return Err(EventLogError::SchemaIncomplete);
    }
    let objects: i64 = tx.query_row("SELECT count(*) FROM sqlite_master WHERE (type='table' AND name='event_log_events' AND upper(sql) LIKE '%SEQUENCE INTEGER PRIMARY KEY AUTOINCREMENT%' AND upper(sql) LIKE '%EVENT_ID TEXT NOT NULL UNIQUE%') OR (type='index' AND name='event_log_tenant_record_sequence' AND sql LIKE '%tenant_id, record_id, sequence%') OR (type='trigger' AND name='event_log_guard_insert' AND upper(sql) LIKE '%BEFORE INSERT ON EVENT_LOG_EVENTS%' AND upper(sql) LIKE '%NEW.SEQUENCE != -1%' AND upper(sql) LIKE '%EVENT_ID = NEW.EVENT_ID%' AND upper(sql) LIKE '%RAISE(ABORT%') OR (type='trigger' AND name='event_log_sequence_positive' AND upper(sql) LIKE '%AFTER INSERT ON EVENT_LOG_EVENTS%' AND upper(sql) LIKE '%NEW.SEQUENCE <= 0%' AND upper(sql) LIKE '%RAISE(ABORT%') OR (type='trigger' AND name='event_log_no_update' AND upper(sql) LIKE '%BEFORE UPDATE ON EVENT_LOG_EVENTS%' AND upper(sql) LIKE '%RAISE(ABORT%') OR (type='trigger' AND name='event_log_no_delete' AND upper(sql) LIKE '%BEFORE DELETE ON EVENT_LOG_EVENTS%' AND upper(sql) LIKE '%RAISE(ABORT%')", [], |r| r.get(0))?;
    if objects != 6 {
        return Err(EventLogError::SchemaIncomplete);
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
#[path = "tests.rs"]
mod tests;
