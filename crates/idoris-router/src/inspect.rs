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
    /// Unreadable content, with no L0 finding, is what pinned `any`.
    pub uninspectable: bool,
    /// The scan itself did not complete (docs/15 §1.5 `inspect_failed`).
    pub failed: bool,
}

/// Inspects every object key, string and number in `body` — content parts,
/// tool definitions and arguments included — one value per line. Lines are
/// separators between values, never part of a digit group. `declared` must
/// already have passed the caller's key scope, so remote is allowed unless
/// `kill_switch` is engaged.
///
/// Content L0 cannot read (images, audio, files, `data:…;base64,` payloads,
/// token ids under `input`) is not provably clean (docs/15 §1.1b), so its presence
/// alone pins the request to `local_only`.
pub(crate) fn inspect_body(
    body: &serde_json::Value,
    declared: PrivacyClass,
    kill_switch: bool,
) -> Inspection {
    // `in_input`: inside an `input` value, where the embeddings API accepts
    // token ids — any integer array there is unreadable to L0, however
    // short (a phone number is ~4 tokens). Elsewhere integer arrays are
    // ordinary data (tool-schema enums, seeds) and stay readable.
    fn collect(value: &serde_json::Value, out: &mut String, opaque: &mut bool, in_input: bool) {
        match value {
            serde_json::Value::String(text) => {
                *opaque |= is_data_url(text);
                push_line(out, text);
            }
            serde_json::Value::Number(number) => push_line(out, &number.to_string()),
            serde_json::Value::Array(items) => {
                *opaque |= in_input
                    && !items.is_empty()
                    && items.iter().all(|item| item.is_i64() || item.is_u64());
                items
                    .iter()
                    .for_each(|item| collect(item, out, opaque, in_input));
            }
            serde_json::Value::Object(map) => {
                *opaque |= map
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| {
                        OPAQUE_PART_TYPES
                            .iter()
                            .any(|opaque| opaque.eq_ignore_ascii_case(kind))
                    });
                map.iter().for_each(|(key, item)| {
                    push_line(out, key);
                    collect(item, out, opaque, key == "input");
                });
            }
            serde_json::Value::Bool(_) | serde_json::Value::Null => {}
        }
    }
    fn push_line(out: &mut String, text: &str) {
        out.push_str(text);
        out.push('\n');
    }
    fn is_data_url(text: &str) -> bool {
        text.get(..5)
            .is_some_and(|scheme| scheme.eq_ignore_ascii_case("data:"))
            && text.contains(";base64,")
    }
    let mut text = String::new();
    let mut opaque = false;
    collect(body, &mut text, &mut opaque, false);
    let ctx = InspectCtx {
        declared,
        remote_allowed: !kill_switch,
        redact_consent: false,
        max_input_utf16: DEFAULT_MAX_INPUT_UTF16,
    };
    let response = inspect(&text, &ctx);
    let tightening = (opaque || response.action != idoris_contracts::inspect::InspectAction::Allow)
        .then_some(PrivacyClass::LocalOnly);
    let effective = idoris_policy::effective_privacy(declared, tightening);
    let tightened = declared == PrivacyClass::Any && effective == PrivacyClass::LocalOnly;
    Inspection {
        killed: tightened && kill_switch,
        uninspectable: tightened && opaque && !kill_switch && response.findings.is_empty(),
        failed: false,
        tightened,
        response,
        effective,
    }
}

/// Bodies at least this large are scanned on the blocking pool: a 256K
/// adversarial body takes ~150 ms, too long to hold an async worker.
const OFF_WORKER_BYTES: usize = 32 * 1024;

/// [`inspect_body`] for a request path: small bodies inline, large ones on
/// the blocking pool. If the blocking task fails the request is pinned to
/// `local_only` with `reason=inspect_failed` (docs/15 §1.5) — a scanner
/// failure never lets content out.
pub(crate) async fn inspect_body_on_request_path(
    body: &serde_json::Value,
    raw_len: usize,
    declared: PrivacyClass,
    kill_switch: bool,
) -> Inspection {
    if raw_len < OFF_WORKER_BYTES {
        return inspect_body(body, declared, kill_switch);
    }
    let owned = body.clone();
    tokio::task::spawn_blocking(move || inspect_body(&owned, declared, kill_switch))
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
        uninspectable: false,
        failed: true,
    }
}

/// Content-part types whose payload L0 cannot read (OpenAI chat/responses,
/// Anthropic messages).
const OPAQUE_PART_TYPES: &[&str] = &[
    "image_url",
    "input_image",
    "image",
    "input_audio",
    "audio",
    "file",
    "input_file",
    "document",
];

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
        } else if inspection.uninspectable {
            "uninspectable_content"
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
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[tokio::test]
    async fn large_bodies_are_scanned_off_the_worker_with_the_same_verdict() {
        let filler = "x".repeat(OFF_WORKER_BYTES);
        let body =
            serde_json::json!({"messages": [{"content": filler}, {"content": "电话 13812345678"}]});
        let raw_len = serde_json::to_vec(&body).unwrap().len();
        assert!(raw_len >= OFF_WORKER_BYTES);
        let off = inspect_body_on_request_path(&body, raw_len, PrivacyClass::Any, false).await;
        let inline = inspect_body(&body, PrivacyClass::Any, false);
        assert_eq!(off.response, inline.response);
        assert_eq!(off.effective, PrivacyClass::LocalOnly);
        assert!(off.tightened && !off.failed);
    }

    #[test]
    fn a_failed_scan_pins_local_only() {
        let failed = failed_inspection(PrivacyClass::Any);
        assert_eq!(failed.effective, PrivacyClass::LocalOnly);
        assert!(failed.tightened && failed.failed);
        assert_eq!(
            failed.response.action,
            idoris_contracts::inspect::InspectAction::Block
        );
        assert!(!failed_inspection(PrivacyClass::LocalOnly).tightened);
    }
}
