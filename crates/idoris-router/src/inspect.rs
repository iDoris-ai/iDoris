//! `POST /v1/inspect` (M5-05): gate one's classifier exposed to callers that
//! must decide whether to send content out themselves. It only classifies —
//! nothing is rewritten, dispatched, or stored.
//!
//! - The body is parsed with the frozen M5-01 contract (`{input}` only,
//!   unknown fields rejected). Parse errors never echo serde's message,
//!   which can quote the offending value.
//! - Privacy comes from `X-iDoris-Privacy` (absent = `local_only`), tenant
//!   scope and authentication from the same profile + virtual-key path the
//!   protocol endpoints use. Only the key's privacy ceiling applies: a key
//!   limited to `local_only` cannot ask for an `any` verdict, but a
//!   role-limited key may still inspect (inspection is not role-shaped).
//! - Redact consent has no wire spelling yet; until one is decided it is
//!   always `false`, so this endpoint answers BLOCK where a consented
//!   request would get REDACT. Fail closed rather than invent a header.

use std::sync::Arc;

use axum::{
    Extension, Json,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use idoris_contracts::InspectRequest;
use idoris_contracts::common::PrivacyClass;
use idoris_policy::inspect::{DEFAULT_MAX_INPUT_UTF16, InspectCtx, inspect};

use crate::auth::VirtualKeyScopeError;
use crate::{AppState, enforce_key_scope_with, error_envelope, profile::parse_profile};

pub(crate) async fn post_inspect(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    remote_identity: Option<Extension<idoris_tenancy::virtual_key::store::AuthenticatedVirtualKey>>,
    body: Bytes,
) -> Response {
    let request = match serde_json::from_slice::<serde_json::Value>(&body)
        .ok()
        .and_then(|value| idoris_contracts::parse::<InspectRequest>(&value).ok())
    {
        Some(request) => request,
        None => {
            return error_envelope(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                "inspect body must be a JSON object with exactly one string field `input`",
            );
        }
    };
    let parsed = match parse_profile(&headers, None, state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    let declared = parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly);
    if let Err(response) = enforce_key_scope_with(
        &state,
        &headers,
        remote_identity.as_ref().map(|identity| &identity.0),
        declared,
        |identity| {
            if identity.scope.allowed_privacy.contains(&declared) {
                Ok(())
            } else {
                Err(VirtualKeyScopeError::PrivacyForbidden)
            }
        },
    )
    .await
    {
        return response;
    }
    // Reaching here means the caller's key scope already admits the
    // declared privacy, so tenant policy allows remote for `any` — unless the
    // remote kill switch is engaged.
    let ctx = InspectCtx {
        declared,
        remote_allowed: !state
            .remote_kill_switch
            .load(std::sync::atomic::Ordering::SeqCst),
        redact_consent: false,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    };
    (StatusCode::OK, Json(inspect(&request.input, &ctx))).into_response()
}

/// The chat path's `INSPECTED` stage (docs/15 §1.5): the whole request body
/// is classified before any routing decision or remote intent detection, and
/// anything short of an ALLOW verdict pins the request to `local_only`.
pub(crate) struct Inspection {
    pub response: idoris_contracts::InspectResponse,
    pub effective: PrivacyClass,
    pub tightened: bool,
    /// The remote kill switch, not the content, is what pinned `any`.
    pub killed: bool,
    /// The scan itself did not complete (docs/15 §1.5 `inspect_failed`).
    pub failed: bool,
}

/// Inspects every object key, string and number in `body` — content parts,
/// tool definitions and arguments included — joined by one-unit NUL boundaries so values
/// never merge into a cross-line digit group. `declared` must already have passed the
/// caller's key scope, so remote is allowed unless `kill_switch` is engaged.
pub(crate) fn inspect_body(
    body: &serde_json::Value,
    declared: PrivacyClass,
    kill_switch: bool,
) -> Inspection {
    fn collect(value: &serde_json::Value, out: &mut String) {
        match value {
            serde_json::Value::String(text) => push_line(out, text),
            serde_json::Value::Number(number) => push_line(out, &number.to_string()),
            serde_json::Value::Array(items) => items.iter().for_each(|item| collect(item, out)),
            serde_json::Value::Object(map) => map.iter().for_each(|(key, item)| {
                push_line(out, key);
                collect(item, out);
            }),
            serde_json::Value::Bool(_) | serde_json::Value::Null => {}
        }
    }
    fn push_line(out: &mut String, text: &str) {
        out.push_str(text);
        out.push('\0');
    }
    let mut text = String::new();
    collect(body, &mut text);
    let ctx = InspectCtx {
        declared,
        remote_allowed: !kill_switch,
        redact_consent: false,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    };
    let response = inspect(&text, &ctx);
    let tightening = (response.action != idoris_contracts::inspect::InspectAction::Allow)
        .then_some(PrivacyClass::LocalOnly);
    let effective = idoris_policy::effective_privacy(declared, tightening);
    let tightened = declared == PrivacyClass::Any && effective == PrivacyClass::LocalOnly;
    Inspection {
        killed: tightened && kill_switch,
        failed: false,
        tightened,
        response,
        effective,
    }
}

/// Bodies this large are scanned away from the async request worker.
const OFF_WORKER_BYTES: usize = 32 * 1024;

/// Inspects a request body inline when small and on Tokio's blocking pool when large.
/// A blocking-task failure pins the request to local_only with S3/BLOCK.
pub(crate) async fn inspect_body_on_request_path(
    body: &serde_json::Value,
    raw_len: usize,
    declared: PrivacyClass,
    kill_switch: bool,
) -> Inspection {
    inspect_body_on_request_path_with(body, raw_len, declared, kill_switch, inspect_body).await
}

async fn inspect_body_on_request_path_with<F>(
    body: &serde_json::Value,
    raw_len: usize,
    declared: PrivacyClass,
    kill_switch: bool,
    scanner: F,
) -> Inspection
where
    F: FnOnce(&serde_json::Value, PrivacyClass, bool) -> Inspection + Send + 'static,
{
    if raw_len < OFF_WORKER_BYTES {
        return scanner(body, declared, kill_switch);
    }
    let owned = body.clone();
    tokio::task::spawn_blocking(move || scanner(&owned, declared, kill_switch))
        .await
        .unwrap_or_else(|_| failed_inspection(declared))
}

fn failed_inspection(declared: PrivacyClass) -> Inspection {
    Inspection {
        response: idoris_contracts::InspectResponse {
            sensitivity: idoris_contracts::inspect::Sensitivity::S3,
            findings: Vec::new(),
            action: idoris_contracts::inspect::InspectAction::Block,
        },
        effective: PrivacyClass::LocalOnly,
        tightened: declared == PrivacyClass::Any,
        killed: false,
        failed: true,
    }
}

/// Appends the `inspected` fact: sensitivity, effective privacy, the
/// distinct finding kinds, and a reason only when content tightened the
/// caller's `any`. Never spans, never content.
pub(crate) async fn append_inspected(
    store: Arc<idoris_tenancy::event_log::EventLogStore>,
    tenant_id: String,
    record_id: String,
    correlation: &crate::correlation::RequestCorrelation,
    inspection: &Inspection,
) -> Result<(), ()> {
    let mut labels: Vec<serde_json::Value> = Vec::new();
    for finding in &inspection.response.findings {
        let label = serde_json::to_value(finding.kind).map_err(|_| ())?;
        if !labels.contains(&label) {
            labels.push(label);
        }
    }
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert(
        "sensitivity".to_string(),
        serde_json::to_value(inspection.response.sensitivity).map_err(|_| ())?,
    );
    metadata.insert(
        "privacy".to_string(),
        serde_json::to_value(inspection.effective).map_err(|_| ())?,
    );
    metadata.insert("labels".to_string(), serde_json::Value::Array(labels));
    if inspection.tightened {
        let reason = if inspection.failed {
            "inspect_failed"
        } else if inspection.killed {
            "remote_kill_switch"
        } else {
            "privacy_tightened_by_content"
        };
        metadata.insert("reason".to_string(), serde_json::Value::from(reason));
    }
    let ts_utc_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(())?;
    let event = idoris_tenancy::event_log::NewEvent {
        event_id: uuid::Uuid::new_v4().to_string(),
        tenant_id: tenant_id.clone(),
        record_id,
        event_type: idoris_tenancy::event_log::EventType::Inspected,
        ts_utc_ms,
        request_id: None,
        session_id: correlation.session_id.clone(),
        trace_id: correlation.trace_id.clone(),
        parent_id: correlation.parent_id.clone(),
        origin_record_id: None,
        metadata,
    };
    crate::run_event_log_write(move || store.append(Some(&tenant_id), &event).map(|_| ())).await
}

#[cfg(test)]
mod request_path_tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::mpsc;

    #[tokio::test]
    async fn request_path_uses_inline_below_and_blocking_pool_at_and_above_threshold() {
        let body = serde_json::json!({"messages": []});
        let caller = std::thread::current().id();
        for (raw_len, expected_inline) in [
            (OFF_WORKER_BYTES - 1, true),
            (OFF_WORKER_BYTES, false),
            (OFF_WORKER_BYTES + 1, false),
        ] {
            let (tx, rx) = mpsc::channel();
            let scanner = move |value: &serde_json::Value, declared, kill_switch| {
                tx.send(std::thread::current().id()).unwrap();
                inspect_body(value, declared, kill_switch)
            };
            let result = inspect_body_on_request_path_with(
                &body,
                raw_len,
                PrivacyClass::Any,
                false,
                scanner,
            )
            .await;
            let scanner_thread = rx.recv().unwrap();
            assert_eq!(
                scanner_thread == caller,
                expected_inline,
                "raw_len={raw_len}"
            );
            assert!(!result.failed);
        }
    }

    #[tokio::test]
    async fn blocking_scanner_failure_fails_closed() {
        let result = inspect_body_on_request_path_with(
            &serde_json::json!({"content": "safe-looking"}),
            OFF_WORKER_BYTES,
            PrivacyClass::Any,
            false,
            |_, _, _| panic!("injected scanner task failure"),
        )
        .await;
        assert_eq!(result.effective, PrivacyClass::LocalOnly);
        assert!(result.tightened && result.failed);
        assert_eq!(
            result.response.sensitivity,
            idoris_contracts::inspect::Sensitivity::S3
        );
        assert_eq!(
            result.response.action,
            idoris_contracts::inspect::InspectAction::Block
        );
        let local_only = failed_inspection(PrivacyClass::LocalOnly);
        assert_eq!(local_only.effective, PrivacyClass::LocalOnly);
        assert!(!local_only.tightened);
        assert!(local_only.failed);
    }

    #[tokio::test]
    async fn large_request_path_preserves_inline_verdict_and_json_value_boundaries() {
        let body = serde_json::json!({
            "messages": [{"content": format!("{}电话 13812345678", "x".repeat(OFF_WORKER_BYTES))}],
            "number": 13812345678_i64,
        });
        let raw_len = serde_json::to_vec(&body).unwrap().len();
        let off = inspect_body_on_request_path(&body, raw_len, PrivacyClass::Any, false).await;
        let inline = inspect_body(&body, PrivacyClass::Any, false);
        assert_eq!(off.response, inline.response);
        assert_eq!(off.effective, inline.effective);
        assert_eq!(off.tightened, inline.tightened);
        assert_eq!(off.failed, inline.failed);
        assert!(off.tightened && !off.failed);

        let boundary = serde_json::json!({
            "padding": "x".repeat(OFF_WORKER_BYTES),
            "left": "1101051949",
            "right": "1231002X",
        });
        let raw_len = serde_json::to_vec(&boundary).unwrap().len();
        let async_result =
            inspect_body_on_request_path(&boundary, raw_len, PrivacyClass::Any, false).await;
        let inline_result = inspect_body(&boundary, PrivacyClass::Any, false);
        assert_eq!(async_result.response, inline_result.response);
        assert_eq!(
            async_result.response.action,
            idoris_contracts::inspect::InspectAction::Allow
        );
        assert!(!async_result
            .response
            .findings
            .iter()
            .any(|finding| finding.kind == idoris_contracts::inspect::FindingKind::CnResidentId));
    }
}
