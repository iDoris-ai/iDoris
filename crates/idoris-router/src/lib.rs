//! Rust skeleton for the iDoris router. R1 scope only: binds to a
//! hardcoded loopback address, serves `GET /health` in the shape PR #46
//! settled on, tags every response with a server-generated
//! `X-iDoris-Record-Id`, and answers every other route with `501` in the
//! unified error envelope from the interface spec §3.11. No routing,
//! backend dispatch, or policy logic is ported here — see the root
//! `README.md`.

/// Control-plane header parsing (R2-D task 1).
pub mod profile;

/// Component card loading from `IDORIS_COMPONENTS_DIR` (R2-D task 2); wired
/// into `AppState`/`/health`'s `components` count in a follow-up PR.
pub mod components;

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use profile::{ProfileError, parse_profile};

/// Production default port (T4.1/FU-13, PR #46): `IDORIS_PORT` unset/blank
/// falls back to this. `8765`/`8796`/`8088`/`11434` were all already taken by
/// other local daemons on the reference dev machine.
pub const DEFAULT_PORT: u16 = 8740;

/// Bind address is hardcoded loopback — never configurable, this touches
/// security (see `packages/router/src/server.ts`'s `BIND_HOST` comment).
pub const BIND_HOST: IpAddr = IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);

const HEADER_RECORD_ID: &str = "X-iDoris-Record-Id";

/// Everything that can go wrong turning `IDORIS_PORT` into a port number.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortError(pub String);

impl std::fmt::Display for PortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PortError {}

/// Parses `IDORIS_PORT`. Mirrors `packages/router/src/cli.ts`'s `parsePort`
/// (PR #46) exactly: unset or blank (after trimming) means [`DEFAULT_PORT`];
/// anything else must be a plain base-10 integer in `1..=65535`.
///
/// **Never silently falls back to the default on an invalid value** — a
/// silent fallback would make an operator believe the service is listening
/// on the port they configured when it's actually on [`DEFAULT_PORT`], and a
/// downstream client would then fail to connect with no clue why.
pub fn parse_port(raw: Option<&str>) -> Result<u16, PortError> {
    let raw = match raw {
        None => return Ok(DEFAULT_PORT),
        Some(raw) => raw.trim(),
    };
    if raw.is_empty() {
        return Ok(DEFAULT_PORT);
    }
    let out_of_range = || {
        PortError(format!(
            "IDORIS_PORT 不是合法的端口号：\"{raw}\"。请设置为 1-65535 之间的整数，或不设置以使用默认值 {DEFAULT_PORT}。"
        ))
    };
    if !raw.bytes().all(|b| b.is_ascii_digit()) {
        return Err(out_of_range());
    }
    let port: u64 = raw.parse().map_err(|_| out_of_range())?;
    if !(1..=65535).contains(&port) {
        return Err(out_of_range());
    }
    #[allow(clippy::unwrap_used)] // just range-checked against 65535 above
    Ok(u16::try_from(port).unwrap())
}

#[derive(Debug, Clone, Serialize)]
pub struct HealthResponse {
    pub status: &'static str,
    pub service: &'static str,
    pub version: String,
    pub contract_version: &'static str,
    pub instance_id: String,
    pub components: usize,
}

/// Shared server state. `instance_id` is generated once per process and
/// reported by `/health` — Agent24 uses it to detect a router restart.
#[derive(Debug, Clone)]
pub struct AppState {
    pub instance_id: String,
    /// Number of registered components. Always `0` in the R1 skeleton —
    /// nothing loads `config/components/*.yaml` yet.
    pub components: usize,
    /// Resolved once at construction (from `IDORIS_DEPLOY_MODE`), not
    /// re-read per request — mirrors `RouterOptions.env` in `server.ts`:
    /// production reads real process env exactly once; tests override this
    /// field directly instead of mutating process-global env (which would
    /// race across parallel tests).
    pub deploy_mode: idoris_contracts::DeployMode,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            instance_id: Uuid::new_v4().to_string(),
            components: 0,
            deploy_mode: profile::deploy_mode_from_env(
                std::env::var("IDORIS_DEPLOY_MODE").ok().as_deref(),
            ),
        }
    }
}

/// Builds the full axum app: `GET /health`, `POST /v1/chat/completions`, a
/// `501` fallback for everything else, and the `X-iDoris-Record-Id`
/// middleware applied to every response.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route(
            "/v1/chat/completions",
            post(chat_completions).fallback(not_implemented),
        )
        .fallback(not_implemented)
        .with_state(Arc::new(state))
        .layer(middleware::from_fn(record_id_middleware))
}

async fn health(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    Json(HealthResponse {
        status: "ok",
        service: "idoris",
        version: env!("CARGO_PKG_VERSION").to_string(),
        contract_version: idoris_contracts::CONTRACT_VERSION,
        instance_id: state.instance_id.clone(),
        components: state.components,
    })
}

/// Every not-yet-implemented route (i.e. everything but `/health` in this
/// skeleton) answers `501` with the unified error envelope from the
/// interface spec §3.11 (`{error: {type, rule_id, reason_code, evidence,
/// remediation}}`). `not_implemented` isn't one of the spec's own `type`
/// values (`local_only_unavailable`, `budget_exceeded`, ...) — those describe
/// runtime routing/policy outcomes, whereas this route genuinely doesn't
/// exist yet in the Rust build. Same envelope shape, a type value scoped to
/// this skeleton.
async fn not_implemented() -> impl IntoResponse {
    error_envelope(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        "This route is not implemented yet in the Rust skeleton (R1); packages/router (TS) is the reference implementation.",
    )
}

/// The unified error envelope from the interface spec §3.11:
/// `{error: {type, rule_id, reason_code, evidence, remediation}}`.
/// `reason_code` mirrors `error_type` here — this crate doesn't yet have a
/// richer machine-readable code distinct from the `type` value itself, and
/// `rule_id`/`evidence` stay `null` until a rule engine produces them.
fn error_envelope(
    status: StatusCode,
    error_type: &'static str,
    remediation: impl Into<String>,
) -> Response {
    let body = json!({
        "error": {
            "type": error_type,
            "rule_id": null,
            "reason_code": error_type,
            "evidence": null,
            "remediation": remediation.into(),
        }
    });
    (status, Json(body)).into_response()
}

impl IntoResponse for ProfileError {
    fn into_response(self) -> Response {
        error_envelope(self.status, self.error_type, self.message)
    }
}

/// `POST /v1/chat/completions` — request-profile parsing only for now
/// (component loading, decision, and backend dispatch land in follow-up
/// R2-D PRs). Order (locked by conformance): non-JSON body -> `invalid_json`;
/// valid JSON that isn't an object -> `invalid_body`; only then are
/// control-plane headers parsed (see [`profile::parse_profile`]).
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => {
            return error_envelope(
                StatusCode::BAD_REQUEST,
                "invalid_json",
                "request body is not valid JSON",
            );
        }
    };
    let Some(object) = value.as_object() else {
        return error_envelope(
            StatusCode::BAD_REQUEST,
            "invalid_body",
            "request body must be a JSON object",
        );
    };
    let model = object.get("model").and_then(|v| v.as_str());

    match parse_profile(&headers, model, state.deploy_mode) {
        Ok(_parsed) => not_implemented().await.into_response(),
        Err(err) => err.into_response(),
    }
}

/// Server-generated on every response, success or error, streaming or not
/// (interface spec §3.12) — never taken from a caller-supplied header.
async fn record_id_middleware(req: Request<Body>, next: Next) -> Response {
    let mut response = next.run(req).await;
    let record_id = Uuid::new_v4().to_string();
    if let Ok(value) = HeaderValue::from_str(&record_id) {
        response.headers_mut().insert(HEADER_RECORD_ID, value);
    }
    response
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[test]
    fn parse_port_defaults_when_unset_or_blank() {
        assert_eq!(parse_port(None).unwrap(), DEFAULT_PORT);
        assert_eq!(parse_port(Some("")).unwrap(), DEFAULT_PORT);
        assert_eq!(parse_port(Some("   ")).unwrap(), DEFAULT_PORT);
    }

    #[test]
    fn parse_port_accepts_legal_integers() {
        assert_eq!(parse_port(Some("9001")).unwrap(), 9001);
        assert_eq!(parse_port(Some("1")).unwrap(), 1);
        assert_eq!(parse_port(Some("65535")).unwrap(), 65535);
    }

    #[test]
    fn parse_port_rejects_non_integers() {
        assert!(parse_port(Some("abc")).is_err());
        assert!(parse_port(Some("8080x")).is_err());
        assert!(parse_port(Some("8080.5")).is_err());
    }

    #[test]
    fn parse_port_rejects_out_of_range() {
        for bad in ["0", "-1", "70000"] {
            assert!(parse_port(Some(bad)).is_err(), "{bad} should be rejected");
        }
    }

    #[tokio::test]
    async fn health_has_the_pr46_shape_and_a_record_id_header() {
        let app = build_app(AppState::default());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["status"], "ok");
        assert_eq!(json["service"], "idoris");
        assert_eq!(json["contract_version"], idoris_contracts::CONTRACT_VERSION);
        assert!(json["version"].is_string());
        assert!(json["instance_id"].is_string());
        assert_eq!(json["components"], 0);
    }

    #[tokio::test]
    async fn unimplemented_routes_return_501_with_the_unified_envelope() {
        // `/v1/chat/completions` is now a real route (profile parsing —
        // see the `chat_completions_*` tests below); `/v1/models` is still
        // genuinely unimplemented and exercises the `fallback` path.
        let app = build_app(AppState::default());
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/v1/models")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "not_implemented");
        assert_eq!(json["error"]["reason_code"], "not_implemented");
    }

    fn post_chat(body: &'static str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(Body::from(body)).unwrap()
    }

    #[tokio::test]
    async fn chat_completions_rejects_invalid_json() {
        let app = build_app(AppState::default());
        let response = app
            .oneshot(post_chat("{not valid json", &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "invalid_json");
    }

    #[tokio::test]
    async fn chat_completions_rejects_a_non_object_body() {
        let app = build_app(AppState::default());
        for body in ["null", "[]", "1"] {
            let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"]["type"], "invalid_body");
        }
    }

    /// Ordering lock: an invalid body shape must be reported before header
    /// validation runs, even when a header is also invalid.
    #[tokio::test]
    async fn chat_completions_invalid_body_outranks_invalid_headers() {
        let app = build_app(AppState::default());
        let response = app
            .oneshot(post_chat("[]", &[("x-idoris-privacy", "bogus")]))
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "invalid_body");
    }

    #[tokio::test]
    async fn chat_completions_rejects_invalid_privacy_header() {
        let app = build_app(AppState::default());
        let response = app
            .oneshot(post_chat("{}", &[("x-idoris-privacy", "bogus")]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "invalid_privacy");
    }

    #[tokio::test]
    async fn chat_completions_rejects_unknown_role() {
        let app = build_app(AppState::default());
        let response = app
            .oneshot(post_chat(r#"{"model":"idoris/nope"}"#, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "unknown_role");
    }

    #[tokio::test]
    async fn chat_completions_tenant_mode_requires_tenant_header() {
        let state = AppState {
            deploy_mode: idoris_contracts::DeployMode::Tenant,
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app.oneshot(post_chat("{}", &[])).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "tenant_missing");
    }

    #[tokio::test]
    async fn chat_completions_valid_request_is_not_yet_implemented_but_well_formed() {
        // R2-D task 1 scope: profile parsing only. A well-formed request
        // still answers 501 (dispatch lands in a follow-up PR) but must
        // have already passed every 400-producing check.
        let app = build_app(AppState::default());
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
    }

    #[tokio::test]
    async fn every_response_gets_a_distinct_record_id() {
        let app = build_app(AppState::default());
        let first = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let second = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let id_of = |r: &Response| {
            r.headers()
                .get(HEADER_RECORD_ID)
                .unwrap()
                .to_str()
                .unwrap()
                .to_string()
        };
        assert_ne!(id_of(&first), id_of(&second));
    }

    /// Regression: axum's default behavior for a matched path with the
    /// wrong method is a bare 405 with no body and no unified envelope —
    /// that would violate "every response uses the §3.11 envelope"
    /// (`build_app` wires `.fallback(not_implemented)` onto this specific
    /// route to override that default, see below). Locking status *and*
    /// envelope shape here catches a regression to axum's default if that
    /// wiring is ever accidentally dropped.
    #[tokio::test]
    async fn wrong_method_on_chat_completions_still_uses_the_unified_envelope() {
        let app = build_app(AppState::default());
        let response = app
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/chat/completions")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "not_implemented");
    }
}
