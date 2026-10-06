use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use idoris_tenancy::event_log::{EventLogError, EventLogStore, EventType, NewEvent};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

const MAX_STRING_UTF16_UNITS: usize = 500;
const MAX_LABELS: usize = 32;
const MAX_IDENTIFIER_UTF16_UNITS: usize = 128;

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Rating {
    Up,
    Down,
}

impl Rating {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RubricItem {
    pub id: String,
    pub pass: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FeedbackRequest {
    pub record_id: String,
    pub rating: Option<Rating>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub outcome: Option<String>,
    #[serde(default)]
    pub rubric: Vec<RubricItem>,
    pub corrected_output: Option<Value>,
}

impl FeedbackRequest {
    pub(crate) fn validate(&self) -> Result<(), FeedbackError> {
        if self.record_id.trim().is_empty() {
            return Err(FeedbackError::InvalidRecordId);
        }
        if self.corrected_output.is_some() {
            return Err(FeedbackError::Invalid(
                "corrected_output requires content storage",
            ));
        }
        if self.rating.is_none()
            && self.outcome.is_none()
            && self.labels.is_empty()
            && self.rubric.is_empty()
        {
            return Err(FeedbackError::Invalid(
                "feedback contains no metadata signal",
            ));
        }
        if self.labels.len() > MAX_LABELS
            || self.labels.iter().any(|label| !valid_string(label))
            || self
                .outcome
                .as_deref()
                .is_some_and(|value| !valid_string(value))
            || self.rubric.len() > MAX_LABELS
            || self.rubric.iter().any(|item| !valid_string(&item.id))
        {
            return Err(FeedbackError::Invalid(
                "feedback metadata exceeds safe bounds",
            ));
        }
        Ok(())
    }
}

fn valid_string(value: &str) -> bool {
    !value.trim().is_empty()
        && value.encode_utf16().count() <= MAX_STRING_UTF16_UNITS
        && !value.chars().any(char::is_control)
}

pub(crate) fn valid_event_identifier(value: &str) -> bool {
    !value.trim().is_empty()
        && value.encode_utf16().count() <= MAX_IDENTIFIER_UTF16_UNITS
        && !value.chars().any(char::is_control)
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum FeedbackError {
    #[error("record_id is invalid")]
    InvalidRecordId,
    #[error("request record was not found")]
    NotFound,
    #[error("{0}")]
    Invalid(&'static str),
    #[error(transparent)]
    EventLog(#[from] EventLogError),
}

pub(crate) fn append_metadata_feedback(
    store: &EventLogStore,
    tenant_id: &str,
    request: FeedbackRequest,
) -> Result<(), FeedbackError> {
    request.validate()?;
    let prior = store
        .events_for_record(Some(tenant_id), &request.record_id)
        .map_err(|error| match error {
            EventLogError::InvalidRecordId | EventLogError::InvalidIdentifier("record_id") => {
                FeedbackError::InvalidRecordId
            }
            other => FeedbackError::EventLog(other),
        })?;
    let Some(first) = prior.first() else {
        return Err(FeedbackError::NotFound);
    };
    let mut metadata = BTreeMap::new();
    if let Some(rating) = request.rating {
        metadata.insert("rating".to_string(), json!(rating.as_str()));
    }
    if let Some(outcome) = request.outcome {
        metadata.insert("outcome".to_string(), json!(outcome));
    }
    if !request.labels.is_empty() {
        metadata.insert("labels".to_string(), json!(request.labels));
    }
    if !request.rubric.is_empty() {
        metadata.insert(
            "rubric".to_string(),
            json!(
                request
                    .rubric
                    .iter()
                    .map(|item| json!({"id": item.id, "pass": item.pass}))
                    .collect::<Vec<_>>()
            ),
        );
    }
    let ts_utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(FeedbackError::Invalid("system clock is before unix epoch"))?;
    let event = NewEvent {
        event_id: Uuid::new_v4().to_string(),
        tenant_id: tenant_id.to_string(),
        record_id: request.record_id,
        event_type: EventType::FeedbackReceived,
        ts_utc_ms,
        request_id: None,
        session_id: first.event.session_id.clone(),
        trace_id: first.event.trace_id.clone(),
        parent_id: first.event.parent_id.clone(),
        origin_record_id: None,
        metadata,
    };
    store.append(Some(tenant_id), &event)?;
    Ok(())
}
