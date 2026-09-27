//! Resident/pin admission logic. oMLX 0.6.4 moved setting `is_pinned`
//! behind `PUT /admin/api/models/{id}/settings`, which requires a separate
//! admin session this adapter doesn't have — the plain inference API key
//! gets a bare 401. `GET /v1/models/status`, by contrast, is confirmed
//! readable with just the inference key (read-only, no admin session
//! needed) — [`parse_model_state`] is this adapter's only way to find out
//! whether a model actually ended up pinned, and the only way to detect
//! that a model got pinned by something outside this adapter's control
//! (the oMLX admin UI, a persisted pin surviving a restart, ...).
//!
//! **Fail-closed, never fail-open**: a response this can't cleanly parse
//! (missing/duplicate entries, wrong field types, or `loaded=false` —
//! evicted between `load` and this check) is an error, never guessed at as
//! "probably not pinned". Ported from `packages/adapters/omlx/
//! omlx-backend.ts`'s `verifyModelState`.
//!
//! `#![allow(dead_code)]`: wired into `OmlxAdapter::load` in a follow-up
//! PR; exercised directly by this module's own tests until then.
#![allow(dead_code)]

use idoris_backend::BackendError;

use super::upstream_error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ModelState {
    pub pinned: bool,
}

/// Parse `GET /v1/models/status`'s response, strictly matched against
/// `id`. `raw` must be a plain object with a `models` array containing
/// exactly one entry whose `id` matches, `loaded: true`, and a boolean
/// `pinned`.
pub(super) fn parse_model_state(
    raw: &serde_json::Value,
    id: &str,
) -> Result<ModelState, BackendError> {
    let models = raw
        .get("models")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            upstream_error(format!(
                "oMLX GET /v1/models/status response is missing a models array (checking {id})"
            ))
        })?;
    let matches: Vec<&serde_json::Value> = models
        .iter()
        .filter(|m| m.get("id").and_then(|v| v.as_str()) == Some(id))
        .collect();
    let entry = match matches.as_slice() {
        [one] => *one,
        [] => {
            return Err(upstream_error(format!(
                "oMLX GET /v1/models/status has no entry for {id}"
            )));
        }
        _ => {
            return Err(upstream_error(format!(
                "oMLX GET /v1/models/status has duplicate entries for {id}"
            )));
        }
    };
    let loaded = entry
        .get("loaded")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| {
            upstream_error(format!(
                "oMLX GET /v1/models/status's loaded field for {id} is not a boolean"
            ))
        })?;
    if !loaded {
        // The model was evicted/unloaded concurrently, between our own
        // `load` succeeding and this check running — a verification
        // failure in its own right, not "not pinned".
        return Err(upstream_error(format!(
            "oMLX GET /v1/models/status reports {id} as not loaded (evicted concurrently?)"
        )));
    }
    let pinned = entry
        .get("pinned")
        .and_then(|v| v.as_bool())
        .ok_or_else(|| {
            upstream_error(format!(
                "oMLX GET /v1/models/status's pinned field for {id} is not a boolean"
            ))
        })?;
    Ok(ModelState { pinned })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use serde_json::json;

    use super::*;

    fn status_with(entries: serde_json::Value) -> serde_json::Value {
        json!({ "models": entries })
    }

    #[test]
    fn parses_a_loaded_pinned_entry() {
        let raw = status_with(json!([{"id": "a", "loaded": true, "pinned": true}]));
        assert!(parse_model_state(&raw, "a").unwrap().pinned);
    }

    #[test]
    fn parses_a_loaded_unpinned_entry() {
        let raw = status_with(json!([{"id": "a", "loaded": true, "pinned": false}]));
        assert!(!parse_model_state(&raw, "a").unwrap().pinned);
    }

    #[test]
    fn missing_models_array_is_an_error() {
        let raw = json!({});
        parse_model_state(&raw, "a").expect_err("must fail, not default");
    }

    #[test]
    fn no_matching_entry_is_an_error() {
        let raw = status_with(json!([{"id": "other", "loaded": true, "pinned": false}]));
        parse_model_state(&raw, "a").expect_err("must fail, no silent 'not pinned'");
    }

    #[test]
    fn duplicate_matching_entries_is_an_error() {
        let raw = status_with(json!([
            {"id": "a", "loaded": true, "pinned": false},
            {"id": "a", "loaded": true, "pinned": true},
        ]));
        parse_model_state(&raw, "a").expect_err("ambiguous entries must fail");
    }

    #[test]
    fn not_loaded_is_an_error_not_unpinned() {
        let raw = status_with(json!([{"id": "a", "loaded": false, "pinned": false}]));
        let err = parse_model_state(&raw, "a").expect_err("must fail, not report unpinned");
        assert!(err.to_string().contains("not loaded"));
    }

    #[test]
    fn non_boolean_pinned_field_is_an_error() {
        let raw = status_with(json!([{"id": "a", "loaded": true, "pinned": "yes"}]));
        parse_model_state(&raw, "a").expect_err("non-boolean pinned must fail, not coerce");
    }

    #[test]
    fn non_boolean_loaded_field_is_an_error() {
        let raw = status_with(json!([{"id": "a", "loaded": "yes", "pinned": true}]));
        parse_model_state(&raw, "a").expect_err("non-boolean loaded must fail, not coerce");
    }
}
