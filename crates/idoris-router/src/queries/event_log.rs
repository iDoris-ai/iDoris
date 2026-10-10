use std::collections::BTreeMap;

use idoris_tenancy::event_log::{EventLogError, EventLogStore};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestEventView {
    pub sequence: i64,
    pub event_id: String,
    pub event_type: String,
    pub ts_utc_ms: i64,
    pub request_id: Option<String>,
    pub session_id: Option<String>,
    pub trace_id: Option<String>,
    pub parent_id: Option<String>,
    pub origin_record_id: Option<String>,
    pub metadata: BTreeMap<String, Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RequestEventsResponse {
    pub tenant_id: String,
    pub record_id: String,
    pub events: Vec<RequestEventView>,
}

#[derive(Debug, thiserror::Error)]
pub enum RequestEventsQueryError {
    #[error("X-iDoris-Tenant is required for request event queries")]
    ScopeRequired,
    #[error("path tenant does not match X-iDoris-Tenant")]
    ScopeMismatch,
    #[error("record_id is invalid")]
    InvalidRecordId,
    #[error("request record was not found")]
    NotFound,
    #[error(transparent)]
    EventLog(#[from] EventLogError),
}

pub fn query_request_events(
    store: &EventLogStore,
    path_tenant: &str,
    scope_tenant: Option<&str>,
    record_id: &str,
) -> Result<RequestEventsResponse, RequestEventsQueryError> {
    let scope_tenant = scope_tenant
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or(RequestEventsQueryError::ScopeRequired)?;
    if scope_tenant != path_tenant {
        return Err(RequestEventsQueryError::ScopeMismatch);
    }

    let events = store
        .events_for_record(Some(path_tenant), record_id)
        .map_err(|error| match error {
            EventLogError::InvalidRecordId | EventLogError::InvalidIdentifier("record_id") => {
                RequestEventsQueryError::InvalidRecordId
            }
            other => RequestEventsQueryError::EventLog(other),
        })?;
    if events.is_empty() {
        return Err(RequestEventsQueryError::NotFound);
    }

    Ok(RequestEventsResponse {
        tenant_id: path_tenant.to_string(),
        record_id: record_id.to_string(),
        events: events
            .into_iter()
            .map(|stored| RequestEventView {
                sequence: stored.sequence,
                event_id: stored.event.event_id,
                event_type: stored.event.event_type.as_str().to_string(),
                ts_utc_ms: stored.event.ts_utc_ms,
                request_id: stored.event.request_id,
                session_id: stored.event.session_id,
                trace_id: stored.event.trace_id,
                parent_id: stored.event.parent_id,
                origin_record_id: stored.event.origin_record_id,
                metadata: stored.event.metadata,
            })
            .collect(),
    })
}
