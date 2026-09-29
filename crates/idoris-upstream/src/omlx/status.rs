//! Parsing for oMLX's `GET /v1/models` (list) and `GET /api/status`
//! (status) responses — ported 1:1 from `packages/adapters/omlx/
//! omlx-backend.ts`'s `list()`/`status()`/`parseLoaded`/`parseMemoryGb`/
//! `parsePressure` (FU-16, 0.6.4 retest, 5 review rounds on `main`).
//!
//! `status()` is **strict/fail-closed**: a response that doesn't look
//! exactly like what oMLX 0.6.4 is known to send is an error, never a
//! silently-degraded default (a `[]` loaded list masking "we couldn't
//! parse this" is exactly the class of bug FU-16 fixed on the TS side).
//! `list()` is the one exception, matching the TS reference's own
//! leniency there: a missing `data` field is treated as an empty catalog,
//! not an error — that field genuinely comes back absent on some oMLX
//! configurations and isn't itself a parse failure.

use idoris_backend::{BackendError, BackendStatus, ModelInfo, Pressure};

use super::upstream_error;

const BYTES_PER_GIB: f64 = 1024.0 * 1024.0 * 1024.0;
const KNOWN_PRESSURE_VALUES: [&str; 4] = ["ok", "soft", "hard", "ceiling"];

/// Report "what shape did we get" for an error message — distinguishes
/// null/array/object from a bare `typeof`-style description, but never
/// echoes the value itself (H2: no payload in errors/logs).
fn describe_shape(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// `GET /v1/models` → every model this oMLX instance could route to,
/// whether or not currently loaded. `/v1/models` reports no memory size,
/// so `memory_gb` is always `0.0` (same placeholder the TS reference
/// uses); a missing `data` field is an empty catalog, not an error
/// (matches the TS reference's `body.data ?? []`).
pub(super) fn parse_list(raw: &serde_json::Value) -> Vec<ModelInfo> {
    raw.get("data")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|m| m.get("id").and_then(|id| id.as_str()))
        .map(|id| ModelInfo {
            id: id.to_string(),
            memory_gb: 0.0,
        })
        .collect()
}

/// `GET /api/status` → [`BackendStatus`]. Fail-closed at every step; see
/// the module doc.
pub(super) fn parse_status(raw: &serde_json::Value) -> Result<BackendStatus, BackendError> {
    let body = raw.as_object().ok_or_else(|| {
        upstream_error(format!(
            "oMLX GET /api/status response is not a JSON object (got {})",
            describe_shape(raw)
        ))
    })?;
    let loaded = parse_loaded(body)?;
    let used_gb = parse_memory_gb(body.get("model_memory_used"), "model_memory_used")?;
    let model_memory_max_gb = parse_memory_gb(body.get("model_memory_max"), "model_memory_max")?;
    let pressure = parse_pressure(body.get("pressure"));
    Ok(BackendStatus {
        pressure,
        used_gb,
        model_memory_max_gb,
        loaded,
    })
}

/// 0.6.4's loaded-models field is `loaded_models`; `loaded` is the older
/// name some engines may still use. `loaded_models`, when present and not
/// `null`, always wins even if `loaded` is also present. Missing/`null`
/// for both, a non-array value, or a non-string element are all errors —
/// never silently degraded to `[]` (that would make `status()` misreport
/// "no models loaded" instead of "couldn't parse the response").
fn parse_loaded(
    body: &serde_json::Map<String, serde_json::Value>,
) -> Result<Vec<String>, BackendError> {
    let loaded_models = body.get("loaded_models").filter(|v| !v.is_null());
    let raw = loaded_models.or_else(|| body.get("loaded").filter(|v| !v.is_null()));
    let Some(raw) = raw else {
        return Err(upstream_error(
            "oMLX GET /api/status is missing both loaded_models and loaded".to_string(),
        ));
    };
    let Some(items) = raw.as_array() else {
        return Err(upstream_error(format!(
            "oMLX GET /api/status's loaded-models field is not an array (got {})",
            describe_shape(raw)
        )));
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            item.as_str().map(str::to_string).ok_or_else(|| {
                upstream_error(format!(
                    "oMLX GET /api/status's loaded-models[{i}] is not a string (got {})",
                    describe_shape(item)
                ))
            })
        })
        .collect()
}

/// `model_memory_max`/`model_memory_used` are bytes on the wire; the
/// `RuntimeAdapter` contract is GiB. Requires an already-numeric,
/// finite, `>= 0` JSON number — no lenient `Number(value)`-style coercion
/// of strings/bools/arrays, and no default-to-zero on a missing field.
fn parse_memory_gb(value: Option<&serde_json::Value>, field: &str) -> Result<f64, BackendError> {
    let bytes = value
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0);
    match bytes {
        Some(bytes) => Ok(bytes / BYTES_PER_GIB),
        None => Err(upstream_error(format!(
            "oMLX GET /api/status's {field} must be a finite number >= 0 (got {})",
            value.map(describe_shape).unwrap_or("missing")
        ))),
    }
}

/// Missing/`null` → explicit `Unknown` (never fail-open to `Ok`, per
/// `Pressure`'s own doc comment). An unrecognized type/value doesn't fail
/// the whole `status()` call — it degrades to `Unknown` for just this one
/// field, since `pressure` failing to parse shouldn't also discard an
/// already-successfully-parsed `loaded` list.
fn parse_pressure(value: Option<&serde_json::Value>) -> Pressure {
    match value {
        None => Pressure::Unknown,
        Some(serde_json::Value::Null) => Pressure::Unknown,
        Some(serde_json::Value::String(s)) if KNOWN_PRESSURE_VALUES.contains(&s.as_str()) => {
            match s.as_str() {
                "ok" => Pressure::Ok,
                "soft" => Pressure::Soft,
                "hard" => Pressure::Hard,
                "ceiling" => Pressure::Ceiling,
                _ => unreachable!("checked against KNOWN_PRESSURE_VALUES above"),
            }
        }
        Some(_) => Pressure::Unknown,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    #[test]
    fn list_missing_data_is_an_empty_catalog_not_an_error() {
        assert_eq!(parse_list(&json!({})), vec![]);
    }

    #[test]
    fn list_filters_out_entries_without_a_string_id() {
        let raw = json!({"data": [{"id": "a"}, {"id": 5}, {}, {"id": "b"}]});
        let models = parse_list(&raw);
        assert_eq!(
            models,
            vec![
                ModelInfo {
                    id: "a".into(),
                    memory_gb: 0.0
                },
                ModelInfo {
                    id: "b".into(),
                    memory_gb: 0.0
                },
            ]
        );
    }

    #[test]
    fn status_top_level_must_be_an_object() {
        for bad in [json!(null), json!(5), json!("x"), json!([1, 2])] {
            let err = parse_status(&bad).expect_err("non-object top level must fail");
            assert_eq!(err.reason_code(), "upstream_error");
        }
    }

    #[test]
    fn status_prefers_loaded_models_over_loaded_when_both_present() {
        let raw = json!({
            "loaded_models": ["a"],
            "loaded": ["b", "c"],
            "model_memory_max": 0,
            "model_memory_used": 0,
        });
        let status = parse_status(&raw).expect("must parse");
        assert_eq!(status.loaded, vec!["a".to_string()]);
    }

    #[test]
    fn status_falls_back_to_loaded_when_loaded_models_is_null_or_absent() {
        for raw in [
            json!({"loaded": ["b"], "model_memory_max": 0, "model_memory_used": 0}),
            json!({"loaded_models": null, "loaded": ["b"], "model_memory_max": 0, "model_memory_used": 0}),
        ] {
            let status = parse_status(&raw).expect("must parse");
            assert_eq!(status.loaded, vec!["b".to_string()]);
        }
    }

    #[test]
    fn status_missing_both_loaded_fields_is_an_error_not_empty_vec() {
        let raw = json!({"model_memory_max": 0, "model_memory_used": 0});
        let err = parse_status(&raw).expect_err("must fail, not default to []");
        assert_eq!(err.reason_code(), "upstream_error");
    }

    #[test]
    fn status_a_non_string_loaded_element_fails_the_whole_call() {
        let raw = json!({"loaded_models": ["a", 5], "model_memory_max": 0, "model_memory_used": 0});
        parse_status(&raw).expect_err("a non-string element must fail, not be silently dropped");
    }

    #[test]
    fn status_converts_bytes_to_gib() {
        let raw = json!({
            "loaded_models": [],
            "model_memory_max": 1024_f64 * 1024.0 * 1024.0 * 8.0,
            "model_memory_used": 0,
        });
        let status = parse_status(&raw).expect("must parse");
        assert!((status.model_memory_max_gb - 8.0).abs() < 1e-9);
    }

    #[test]
    fn status_rejects_non_numeric_or_negative_memory_fields() {
        for bad in [json!("17179869184"), json!(true), json!([]), json!(-1)] {
            let raw = json!({"loaded_models": [], "model_memory_max": bad, "model_memory_used": 0});
            parse_status(&raw).expect_err("must reject non-numeric/negative memory");
        }
    }

    #[test]
    fn status_pressure_missing_or_null_is_unknown_not_ok() {
        for raw in [
            json!({"loaded_models": [], "model_memory_max": 0, "model_memory_used": 0}),
            json!({"loaded_models": [], "model_memory_max": 0, "model_memory_used": 0, "pressure": null}),
        ] {
            let status = parse_status(&raw).expect("must parse");
            assert_eq!(status.pressure, Pressure::Unknown);
        }
    }

    #[test]
    fn status_pressure_unrecognized_value_degrades_to_unknown_without_failing_the_call() {
        let raw = json!({
            "loaded_models": ["a"],
            "model_memory_max": 0,
            "model_memory_used": 0,
            "pressure": "extreme",
        });
        let status = parse_status(&raw).expect("unknown pressure must not fail the whole call");
        assert_eq!(status.pressure, Pressure::Unknown);
        assert_eq!(status.loaded, vec!["a".to_string()]);
    }

    #[test]
    fn status_pressure_known_values_round_trip() {
        for (raw, expected) in [
            ("ok", Pressure::Ok),
            ("soft", Pressure::Soft),
            ("hard", Pressure::Hard),
            ("ceiling", Pressure::Ceiling),
        ] {
            let body = json!({
                "loaded_models": [],
                "model_memory_max": 0,
                "model_memory_used": 0,
                "pressure": raw,
            });
            assert_eq!(parse_status(&body).expect("must parse").pressure, expected);
        }
    }
}
