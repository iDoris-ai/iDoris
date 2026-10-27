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
    /// Opaque content pinned `any` and the text scan found nothing else.
    pub uninspectable: bool,
}

/// Inspects every object key, string and number in `body` — content parts,
/// tool definitions and arguments included — joined by one-unit NUL boundaries so values
/// never merge into a cross-line digit group. `declared` must already have passed the
/// caller's key scope, so remote is allowed unless `kill_switch` is engaged.
///
/// Images, audio, files, documents, `data:…;base64,` payloads, and integer
/// arrays under `input` cannot be read by L0. A *direct* content part (an
/// object that is the value of a `content` key, or an element of a `content`
/// array) is readable only when it has a string `type` that is text,
/// input_text, output_text, tool_use, tool_result, function, thinking,
/// refusal, or one of the Anthropic replay blocks (redacted_thinking,
/// server_tool_use, mcp_tool_use, mcp_tool_result, web_search_tool_result,
/// web_search_result; media inside them is still caught by the global media
/// checks). A direct part with any other `type`, no `type`, a non-string
/// `type`, or a wrong-case `Type` key pins `any` to `local_only`. Objects
/// nested deeper (`cache_control`, annotations) are not parts and are not
/// subject to the allow-list.
///
/// The allow-list is switched off (`suppress_part_check`) inside subtrees
/// where a key named `content` is data, not a message part: `input_schema`,
/// `parameters`, `json_schema` (tool and response schemas) and the `input` of
/// a `tool_use` / `server_tool_use` / `mcp_tool_use` part. Cost: a shape such
/// as `{"content":[{"type":"unknown_media"}]}` hidden inside tool arguments
/// or a schema is no longer caught by the allow-list. Media keys, media type
/// names and `data:…;base64,` URLs are still checked at every depth, in
/// those subtrees too. An object carrying media bytes pins `any` to
/// `local_only` even when the text scan allows the body.
///
/// Coverage is limited to the OpenAI (chat, embeddings, rerank) and
/// Anthropic Messages request shapes; other vendors' media encodings (e.g.
/// Gemini `fileData`) are not recognised. Raw base64 under an unknown key
/// inside an otherwise readable text part stays `any`, and so does an OpenAI
/// assistant `audio: {"id": ..}` reference (an id, no bytes).
pub(crate) fn inspect_body(
    body: &serde_json::Value,
    declared: PrivacyClass,
    kill_switch: bool,
) -> Inspection {
    fn collect(
        value: &serde_json::Value,
        out: &mut String,
        opaque: &mut bool,
        direct: bool,
        suppress_part_check: bool,
    ) {
        match value {
            serde_json::Value::String(text) => {
                *opaque |= is_data_url(text);
                push_line(out, text);
            }
            serde_json::Value::Number(number) => push_line(out, &number.to_string()),
            serde_json::Value::Array(items) => {
                items
                    .iter()
                    .for_each(|item| collect(item, out, opaque, direct, suppress_part_check));
            }
            serde_json::Value::Object(map) => {
                *opaque |= map.keys().any(|key| {
                    OPAQUE_MEDIA_KEYS
                        .iter()
                        .any(|name| key.eq_ignore_ascii_case(name))
                });
                if let Some(kind) = map.get("type").and_then(serde_json::Value::as_str) {
                    *opaque |= MEDIA_TYPE_NAMES
                        .iter()
                        .any(|name| kind.eq_ignore_ascii_case(name));
                }
                let unread_content = direct
                    && !suppress_part_check
                    && !map
                        .get("type")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|kind| {
                            READABLE_PART_TYPES
                                .iter()
                                .any(|name| kind.eq_ignore_ascii_case(name))
                        });
                *opaque |= unread_content;
                let tool_call_part = map
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .is_some_and(|kind| {
                        TOOL_CALL_PART_TYPES
                            .iter()
                            .any(|name| kind.eq_ignore_ascii_case(name))
                    });
                map.iter().for_each(|(key, item)| {
                    push_line(out, key);
                    let suppress = suppress_part_check
                        || SCHEMA_KEYS
                            .iter()
                            .any(|name| key.eq_ignore_ascii_case(name))
                        || (tool_call_part && key.eq_ignore_ascii_case("input"));
                    collect(
                        item,
                        out,
                        opaque,
                        key.eq_ignore_ascii_case("content") && !suppress,
                        suppress,
                    );
                });
            }
            serde_json::Value::Bool(_) | serde_json::Value::Null => {}
        }
    }
    fn push_line(out: &mut String, text: &str) {
        out.push_str(text);
        out.push('\0');
    }
    fn is_data_url(text: &str) -> bool {
        let bytes = text.as_bytes();
        bytes.len() >= 5
            && bytes[..5].eq_ignore_ascii_case(b"data:")
            && bytes
                .windows(b";base64,".len())
                .any(|window| window.eq_ignore_ascii_case(b";base64,"))
    }
    let mut text = String::new();
    let mut opaque = body.get("input").is_some_and(is_integer_token_array);
    collect(body, &mut text, &mut opaque, false, false);
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
        failed: false,
        uninspectable: tightened && opaque && !kill_switch && response.findings.is_empty(),
        tightened,
        response,
        effective,
    }
}

const READABLE_PART_TYPES: &[&str] = &[
    "text",
    "input_text",
    "output_text",
    "tool_use",
    "tool_result",
    "function",
    "thinking",
    "refusal",
    "redacted_thinking",
    "server_tool_use",
    "mcp_tool_use",
    "mcp_tool_result",
    "web_search_tool_result",
    "web_search_result",
];

/// Part types whose `input` holds arbitrary tool arguments.
const TOOL_CALL_PART_TYPES: &[&str] = &["tool_use", "server_tool_use", "mcp_tool_use"];

/// Keys whose subtree is a tool / response schema.
const SCHEMA_KEYS: &[&str] = &["input_schema", "parameters", "json_schema"];

const OPAQUE_MEDIA_KEYS: &[&str] = &[
    "image_url",
    "audio_url",
    "video_url",
    "input_audio",
    "inline_data",
    "file",
];

const MEDIA_TYPE_NAMES: &[&str] = &[
    "image_url",
    "input_image",
    "image",
    "image_file",
    "input_audio",
    "audio",
    "audio_url",
    "video",
    "video_url",
    "input_video",
    "file",
    "input_file",
    "document",
    "document_url",
    "inline_data",
];

fn is_integer_token_array(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Array(items) => {
            (!items.is_empty() && items.iter().all(|item| item.is_i64() || item.is_u64()))
                || items.iter().any(is_integer_token_array)
        }
        serde_json::Value::Object(map) => map.values().any(is_integer_token_array),
        _ => false,
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
        uninspectable: false,
    }
}

/// Appends the `inspected` fact: sensitivity, effective privacy, the
/// distinct finding kinds, and a reason for failed scans or when inspection
/// tightened the caller's `any`. Never spans, never content.
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
    let reason = if inspection.failed {
        Some("inspect_failed")
    } else if inspection.killed {
        Some("remote_kill_switch")
    } else if inspection.uninspectable {
        Some("uninspectable_content")
    } else if inspection.tightened {
        Some("privacy_tightened_by_content")
    } else {
        None
    };
    if let Some(reason) = reason {
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

    async fn persisted_inspected_metadata(
        inspection: &Inspection,
    ) -> std::collections::BTreeMap<String, serde_json::Value> {
        let store = Arc::new(
            idoris_tenancy::event_log::EventLogStore::new(
                rusqlite::Connection::open_in_memory().unwrap(),
            )
            .unwrap(),
        );
        append_inspected(
            store.clone(),
            "tenant".to_string(),
            "record".to_string(),
            &crate::correlation::RequestCorrelation::default(),
            inspection,
        )
        .await
        .unwrap();
        let events = store.events_for_record(Some("tenant"), "record").unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event.event_type,
            idoris_tenancy::event_log::EventType::Inspected
        );
        events[0].event.metadata.clone()
    }

    #[tokio::test]
    async fn failed_inspection_reason_is_persisted_for_any_and_local_only_without_content() {
        for declared in [PrivacyClass::Any, PrivacyClass::LocalOnly] {
            let failure = failed_inspection(declared);
            let metadata = persisted_inspected_metadata(&failure).await;
            assert_eq!(
                metadata.get("reason"),
                Some(&serde_json::json!("inspect_failed"))
            );
            assert_eq!(metadata["sensitivity"], "S3");
            assert_eq!(metadata["privacy"], "local_only");
            assert_eq!(metadata["labels"], serde_json::json!([]));
            assert_eq!(metadata.len(), 4);
            assert!(
                !serde_json::to_string(&metadata)
                    .unwrap()
                    .contains("safe-looking")
            );
        }
    }

    #[tokio::test]
    async fn successful_local_only_inspection_has_no_failure_reason() {
        let inspection = inspect_body(
            &serde_json::json!({"messages": [{"content": "safe-looking"}]}),
            PrivacyClass::LocalOnly,
            false,
        );
        let metadata = persisted_inspected_metadata(&inspection).await;
        assert_eq!(metadata["privacy"], "local_only");
        assert_eq!(metadata["sensitivity"], "S0");
        assert_eq!(metadata["labels"], serde_json::json!([]));
        assert!(!metadata.contains_key("reason"));
        assert_eq!(metadata.len(), 3);
        assert!(
            !serde_json::to_string(&metadata)
                .unwrap()
                .contains("safe-looking")
        );
    }

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

    #[test]
    fn opaque_parts_pin_any_without_a_text_finding() {
        let image = inspect_body(
            &serde_json::json!({
                "messages": [{
                    "role": "user",
                    "content": [{
                        "type": "image_url",
                        "image_url": {"url": "data:image/png;base64,iVBORw0KGgo="}
                    }]
                }]
            }),
            PrivacyClass::Any,
            false,
        );
        assert_eq!(image.effective, PrivacyClass::LocalOnly);
        assert!(image.uninspectable && image.tightened);
        assert!(image.response.findings.is_empty());

        let text = inspect_body(
            &serde_json::json!({"messages": [{"type": "text", "content": "hello"}]}),
            PrivacyClass::Any,
            false,
        );
        assert_eq!(text.effective, PrivacyClass::Any);
        assert!(!text.uninspectable);

        let tokens = inspect_body(
            &serde_json::json!({"input": [1, 2, 3]}),
            PrivacyClass::Any,
            false,
        );
        assert!(tokens.uninspectable);
        let elsewhere = inspect_body(
            &serde_json::json!({"metadata": [1, 2, 3]}),
            PrivacyClass::Any,
            false,
        );
        assert!(!elsewhere.uninspectable);
        let empty_input = inspect_body(&serde_json::json!({"input": []}), PrivacyClass::Any, false);
        assert!(!empty_input.uninspectable);

        for kind in [
            "video_url",
            "audio_url",
            "image_file",
            "input_video",
            "document_url",
            "Image_URL",
        ] {
            let part = inspect_body(
                &serde_json::json!({"messages": [{"content": [{"type": kind}]}]}),
                PrivacyClass::Any,
                false,
            );
            assert!(part.uninspectable, "{kind}");
        }
        let typeless = inspect_body(
            &serde_json::json!({"messages": [{"content": [{"image_url": {"url": "https://example.test/a.png"}}]}]}),
            PrivacyClass::Any,
            false,
        );
        assert!(typeless.uninspectable);
        let data_only = inspect_body(
            &serde_json::json!({"url": "DATA:image/png;BASE64,aaaa"}),
            PrivacyClass::Any,
            false,
        );
        assert!(data_only.uninspectable);
        let inline = inspect_body(
            &serde_json::json!({"inline_data": {"mime_type": "image/png", "data": "aaaa"}}),
            PrivacyClass::Any,
            false,
        );
        assert!(inline.uninspectable);

        let beside_id = inspect_body(
            &serde_json::json!({
                "messages": [{
                    "type": "image_url",
                    "text": "11010519491231002X"
                }]
            }),
            PrivacyClass::Any,
            false,
        );
        assert!(beside_id.tightened && !beside_id.uninspectable);
        assert!(!beside_id.response.findings.is_empty());
    }

    fn pins_uninspectable(body: serde_json::Value) -> bool {
        let got = inspect_body(&body, PrivacyClass::Any, false);
        got.tightened && got.uninspectable
    }

    fn stays_any(body: serde_json::Value) -> bool {
        let got = inspect_body(&body, PrivacyClass::Any, false);
        !got.tightened && got.effective == PrivacyClass::Any
    }

    #[test]
    fn unknown_or_typeless_direct_part_pins_any() {
        let bodies = [
            serde_json::json!({"content": [{"type": "some_future_media"}]}),
            serde_json::json!({"content": {"type": "some_future_media"}}),
            serde_json::json!({"messages": [{"role": "user", "content": [{"type": "some_future_media"}]}]}),
            serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "content": [{"type": "some_future_media"}]}
            ]}]}),
            serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "content": [{"type": "image", "source": {"type": "base64", "data": "aaaa"}}]}
            ]}]}),
            serde_json::json!({"messages": [{"role": "user", "content": [{"text": "x"}]}]}),
            serde_json::json!({"messages": [{"content": [{"source": {"type": "base64", "data": "aaaa"}}]}]}),
            serde_json::json!({"messages": [{"content": [
                {"type": "document", "content": [{"source": {"data": "aaaa"}}]}
            ]}]}),
            serde_json::json!({"messages": [{"content": [{"Type": "some_future_media"}]}]}),
            serde_json::json!({"messages": [{"content": [{"type": ["some_future_media"]}]}]}),
            serde_json::json!({"messages": [{"content": [{"type": 7}]}]}),
        ];
        for body in bodies {
            assert!(pins_uninspectable(body.clone()), "{body}");
        }
    }

    #[test]
    fn readable_parts_and_non_part_types_stay_any() {
        for kind in [
            "text",
            "input_text",
            "output_text",
            "tool_use",
            "tool_result",
            "function",
            "thinking",
            "refusal",
        ] {
            let body = serde_json::json!({"messages": [{"role": "user", "content": [{"type": kind, "text": "hi"}]}]});
            assert!(stays_any(body), "{kind}");
        }
        let bodies = [
            serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
            ]}]}),
            serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "t", "content": "ok", "cache_control": {"type": "ephemeral"}}
            ]}]}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "hmm", "signature": "sig"}
            ]}]}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "refusal", "refusal": "no"}
            ]}]}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "output_text", "text": "see", "annotations": [{"type": "url_citation", "url": "https://example.test"}]}
            ]}]}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "f", "input": {"type": "bug"}}
            ]}]}),
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}],
                "tools": [{"type": "function", "function": {"name": "f", "parameters": {"type": "object", "properties": {"a": {"type": "string"}}}}}]}),
            serde_json::json!({"messages": [{"role": "user", "content": [
                {"type": "text", "text": "x", "data": "iVBORw0KGgo"}
            ]}]}),
        ];
        for body in bodies {
            assert!(stays_any(body.clone()), "{body}");
        }
    }

    #[test]
    fn content_named_data_in_schemas_and_tool_input_stays_any() {
        let bodies = [
            serde_json::json!({"model": "m", "tools": [{"name": "Write", "input_schema": {"type": "object",
                "properties": {"file_path": {"type": "string"}, "content": {"type": "string"}}}}]}),
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}], "tools": [{"type": "function",
                "function": {"name": "w", "parameters": {"type": "object",
                "properties": {"content": {"type": "string", "description": "x"}}}}}]}),
            serde_json::json!({"messages": [{"role": "user", "content": "hi"}], "response_format": {
                "type": "json_schema", "json_schema": {"name": "r", "schema": {"type": "object",
                "properties": {"content": {"type": "string"}}}}}}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "w", "input": {"content": {"a": 1}}}]}]}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "tool_use", "id": "t", "name": "w", "input": {"content": [{"k": "v"}]}}]}]}),
            serde_json::json!({"messages": [{"role": "assistant", "content": [
                {"type": "server_tool_use", "id": "t", "name": "web_search", "input": {"content": [{"k": "v"}]}}]}]}),
        ];
        for body in bodies {
            assert!(stays_any(body.clone()), "{body}");
        }
        for kind in [
            "redacted_thinking",
            "server_tool_use",
            "mcp_tool_use",
            "web_search_tool_result",
        ] {
            let body = serde_json::json!({"messages": [{"role": "assistant", "content": [{"type": kind, "data": "x"}]}]});
            assert!(stays_any(body), "{kind}");
        }
        // Media checks stay global inside the suppressed subtrees.
        assert!(pins_uninspectable(
            serde_json::json!({"messages": [{"role": "assistant", "content": [
            {"type": "tool_use", "id": "t", "name": "w", "input": {"image_url": "https://example.test/a.png"}}]}]})
        ));
        assert!(pins_uninspectable(
            serde_json::json!({"tools": [{"input_schema": {"default": "data:image/png;base64,aaaa"}}]})
        ));
        // Outside the suppressed subtrees the allow-list still applies.
        assert!(pins_uninspectable(
            serde_json::json!({"content": [{"source": {"type": "base64", "data": "aaaa"}}]})
        ));
    }

    #[test]
    fn nested_integer_token_array_pins_any() {
        assert!(pins_uninspectable(
            serde_json::json!({"input": [[1, 2, 3]]})
        ));
    }

    #[tokio::test]
    async fn opaque_inspection_records_uninspectable_content() {
        let inspection = inspect_body(
            &serde_json::json!({"messages": [{"type": "input_audio", "content": "hello"}]}),
            PrivacyClass::Any,
            false,
        );
        let metadata = persisted_inspected_metadata(&inspection).await;
        assert_eq!(metadata["reason"], "uninspectable_content");
        assert_eq!(metadata["privacy"], "local_only");
    }
}
