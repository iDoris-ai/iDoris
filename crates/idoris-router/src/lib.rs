//! Rust skeleton for the iDoris router. R1 scope only: binds to a
//! hardcoded loopback address, serves `GET /health` in the shape PR #46
//! settled on, tags every response with a server-generated
//! `X-iDoris-Record-Id`, and answers every other route with `501` in the
//! unified error envelope from the interface spec §3.11. No routing,
//! backend dispatch, or policy logic is ported here — see the root
//! `README.md`.

/// Control-plane header parsing (R2-D task 1); wired into a route in a follow-up PR.
pub mod profile;

use std::net::IpAddr;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

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
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            instance_id: Uuid::new_v4().to_string(),
            components: 0,
        }
    }
}

/// Builds the full axum app: `GET /health`, a `501` fallback for everything
/// else, and the `X-iDoris-Record-Id` middleware applied to every response.
pub fn build_app(state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
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
    let body = json!({
        "error": {
            "type": "not_implemented",
            "rule_id": null,
            "reason_code": "NOT_IMPLEMENTED",
            "evidence": null,
            "remediation": "This route is not implemented yet in the Rust skeleton (R1); packages/router (TS) is the reference implementation.",
        }
    });
    (StatusCode::NOT_IMPLEMENTED, Json(body))
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
        let app = build_app(AppState::default());
        let response = app
            .oneshot(
                Request::builder()
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
        assert_eq!(json["error"]["reason_code"], "NOT_IMPLEMENTED");
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
}
