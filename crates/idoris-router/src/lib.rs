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

/// Routing-policy loading from `IDORIS_ROUTING_POLICY` (R2-D task 2); wired
/// into `AppState` in a follow-up PR.
pub mod routing_policy;

/// The local decision + execution path (R2-D task 3): `decide()` → (if
/// needed) `Supervisor` load → `Supervisor` chat.
pub mod dispatch;

/// Atomic reserve/settle/release around a paid candidate (R2-D task 4); not
/// yet wired into `dispatch`/the request path — a follow-up PR does that.
pub mod budget;

use std::net::IpAddr;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use idoris_backend::{BackendError, ChatMessage};
use idoris_policy::Rejection;
use idoris_tenancy::budget::{BUDGET_EXCEEDED_REASON_CODE, BudgetError};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use dispatch::{DispatchError, DispatchFailure, dispatch_local, reason_header_value};
use profile::{ProfileError, parse_profile};

const HEADER_SERVED_LOCALITY: &str = "X-iDoris-Served-Locality";
const HEADER_REASON: &str = "X-iDoris-Reason";
const HEADER_DEGRADED: &str = "X-iDoris-Degraded";
const HEADER_COST_MINOR: &str = "X-iDoris-Cost-Minor";

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
/// `Debug` is hand-written (below) since `BudgetLedger` doesn't implement
/// it.
#[derive(Clone)]
pub struct AppState {
    pub instance_id: String,
    /// Resolved once at construction (from `IDORIS_DEPLOY_MODE`), not
    /// re-read per request — mirrors `RouterOptions.env` in `server.ts`:
    /// production reads real process env exactly once; tests override this
    /// field directly instead of mutating process-global env (which would
    /// race across parallel tests).
    pub deploy_mode: idoris_contracts::DeployMode,
    /// Loaded once at startup via [`components::load_components`]; empty by
    /// default (`AppState::default()` does no filesystem/env I/O, matching
    /// every existing test's expectation of a `components: 0` `/health`
    /// response with no real directory involved). `/health`'s `components`
    /// count is always `cards.len()` — a single source of truth instead of
    /// a separately-tracked counter that could drift from this list.
    pub cards: Vec<idoris_contracts::ComponentCard>,
    /// Backs the local dispatch path (R2-D task 3); `None` means no local
    /// backend is wired — dispatch then fails closed as
    /// `local_only_unavailable` rather than panicking on a missing handle.
    pub supervisor: Option<idoris_backend::SupervisorHandle>,
    /// Backs atomic reserve/settle/release for a *paid* candidate (R2-D
    /// task 4); `None` fails a paid candidate closed identically to an
    /// unconfigured ledger scope (free candidates unaffected). `Arc`
    /// because `BudgetLedger` (wraps a `Mutex<Connection>`) isn't `Clone`.
    pub budget_ledger: Option<Arc<idoris_tenancy::budget::BudgetLedger>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("instance_id", &self.instance_id)
            .field("deploy_mode", &self.deploy_mode)
            .field("cards", &self.cards)
            .field("supervisor", &self.supervisor)
            .field(
                "budget_ledger",
                &self.budget_ledger.as_ref().map(|_| "BudgetLedger { .. }"),
            )
            .finish()
    }
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            instance_id: Uuid::new_v4().to_string(),
            deploy_mode: profile::deploy_mode_from_env(
                std::env::var("IDORIS_DEPLOY_MODE").ok().as_deref(),
            ),
            cards: Vec::new(),
            supervisor: None,
            budget_ledger: None,
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
            post(chat_completions).fallback(not_found),
        )
        .fallback(not_found)
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
        components: state.cards.len(),
    })
}

/// Every unmatched route/method combination (interface spec's routes are
/// exhaustively wired below; there is no separate "known but not
/// implemented yet" tier). Mirrors `packages/router/src/server.ts`'s own
/// catch-all exactly: `json(res, 404, { error: { type: "not_found" } })` —
/// plain `404`, no `rule_id`/`reason_code`/`evidence`/`remediation` fields
/// (`conformance/tests/response-headers.test.ts` locks `404` for an unknown
/// path). This used to be a Rust-only `501 not_implemented` placeholder for
/// routes the R1 skeleton hadn't ported yet; every route the TS reference
/// actually has is now wired (R2-G), so the only thing left to fall through
/// to this handler is genuinely "no such route", same as TS.
async fn not_found() -> impl IntoResponse {
    (
        StatusCode::NOT_FOUND,
        Json(json!({ "error": { "type": "not_found" } })),
    )
        .into_response()
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
    error_envelope_with_reason(status, error_type, error_type, remediation)
}

/// As [`error_envelope`], but with a `reason_code` distinct from `type` —
/// for a local backend failure, `type` stays the spec-level
/// `local_only_unavailable` while `reason_code` carries the backend's own
/// more granular [`BackendError::reason_code`] (e.g. `model_not_found`,
/// `oom`) for diagnostics.
fn error_envelope_with_reason(
    status: StatusCode,
    error_type: &'static str,
    reason_code: &str,
    remediation: impl Into<String>,
) -> Response {
    let body = json!({
        "error": {
            "type": error_type,
            "rule_id": null,
            "reason_code": reason_code,
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

/// Request body `messages` → backend [`ChatMessage`]s — only entries whose
/// `role`/`content` are both strings are kept, matching `server.ts`'s
/// `toMessages` (malformed entries are silently dropped, not a 400).
fn extract_messages(object: &serde_json::Map<String, serde_json::Value>) -> Vec<ChatMessage> {
    let Some(raw) = object.get("messages").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    raw.iter()
        .filter_map(|item| {
            let role = item.get("role")?.as_str()?.to_string();
            let content = item.get("content")?.as_str()?.to_string();
            Some(ChatMessage { role, content })
        })
        .collect()
}

/// Very rough token estimate (`ceil(chars / 4)`), matching the same
/// order-of-magnitude heuristic `packages/adapters/subscription/relay.ts`
/// uses for its own OpenAI-shaped response — a real per-model tokenizer
/// isn't wired in at this layer. Purely informational (`usage` in the
/// response body); no billing decision reads this.
fn rough_token_estimate(text: &str) -> u64 {
    if text.is_empty() {
        0
    } else {
        (text.chars().count() as u64).div_ceil(4).max(1)
    }
}

/// Builds an OpenAI `chat.completion`-shaped body, mirroring
/// `openAIChatCompletion` (`packages/adapters/subscription/relay.ts`).
/// `requested_model` is the caller's own `model` field when present (echoed
/// back, matching that TS helper's call site for a locally-served model),
/// falling back to the resolved backend id.
fn openai_chat_completion(content: &str, requested_model: &str, prompt: &str) -> serde_json::Value {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let prompt_tokens = rough_token_estimate(prompt);
    let completion_tokens = rough_token_estimate(content);
    json!({
        "id": format!("chatcmpl-idoris-{}", Uuid::new_v4()),
        "object": "chat.completion",
        "created": created,
        "model": requested_model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    })
}

/// Sets `X-iDoris-Served-Locality` (always, once a candidate is chosen —
/// interface spec §3.12) plus best-effort `X-iDoris-Reason`/`X-iDoris-Degraded`
/// (observability, not yet a formal wire contract) on `response`.
fn apply_decision_headers(response: &mut Response, outcome: &dispatch::ChatOutcome) {
    let headers = response.headers_mut();
    if let Ok(v) = HeaderValue::from_str(locality_str(outcome.served_locality)) {
        headers.insert(HEADER_SERVED_LOCALITY, v);
    }
    let reason = reason_header_value(&outcome.decision.reason_codes);
    if !reason.is_empty()
        && let Ok(v) = HeaderValue::from_str(&reason)
    {
        headers.insert(HEADER_REASON, v);
    }
    if outcome.decision.is_degraded() {
        headers.insert(HEADER_DEGRADED, HeaderValue::from_static("true"));
    }
}

fn locality_str(locality: idoris_contracts::provider::Locality) -> &'static str {
    match locality {
        idoris_contracts::provider::Locality::Loopback => "loopback",
        idoris_contracts::provider::Locality::Lan => "lan",
        idoris_contracts::provider::Locality::Remote => "remote",
    }
}

/// A local backend failure after a candidate was already chosen: per R2-D
/// task 3, this always surfaces as 503 `local_only_unavailable` (the
/// spec-level outcome from the caller's point of view is indistinguishable
/// from "no usable local candidate"), carrying the backend's own
/// `reason_code()` for diagnostics and — critically — still setting
/// `X-iDoris-Served-Locality`, since a candidate genuinely was selected.
fn backend_error_response(err: &BackendError, outcome: &dispatch::ChatOutcome) -> Response {
    let mut response = error_envelope_with_reason(
        StatusCode::SERVICE_UNAVAILABLE,
        "local_only_unavailable",
        err.reason_code(),
        err.to_string(),
    );
    apply_decision_headers(&mut response, outcome);
    response
}

/// A budget-ledger failure gating a *paid* candidate (R2-D task 4).
/// `BudgetError::Exceeded` is the one genuine spend decision — 402
/// `budget_exceeded` with `Budget402Body`'s fields folded in (contract-
/// tenancy §4's "结构化" requirement). Every other variant is a setup/
/// defensive gap, not expected against a correctly configured ledger, so
/// it maps to a generic 500. `X-iDoris-Served-Locality` is still set.
fn budget_error_response(err: &BudgetError, outcome: &dispatch::ChatOutcome) -> Response {
    let mut response = match err.to_402_body() {
        Some(body) => {
            #[allow(clippy::unwrap_used)] // Budget402Body's fields all serialize infallibly
            let mut value = serde_json::to_value(body).unwrap();
            if let Some(obj) = value.as_object_mut() {
                obj.insert("type".to_string(), json!(BUDGET_EXCEEDED_REASON_CODE));
                obj.insert("rule_id".to_string(), json!(null));
                obj.insert("evidence".to_string(), json!(null));
                obj.insert("remediation".to_string(), json!(err.to_string()));
            }
            (
                StatusCode::PAYMENT_REQUIRED,
                Json(json!({ "error": value })),
            )
                .into_response()
        }
        None => error_envelope_with_reason(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "budget_ledger_error",
            err.to_string(),
        ),
    };
    apply_decision_headers(&mut response, outcome);
    response
}

fn dispatch_failure_response(
    failure: &DispatchFailure,
    outcome: &dispatch::ChatOutcome,
) -> Response {
    match failure {
        DispatchFailure::Backend(err) => backend_error_response(err, outcome),
        DispatchFailure::Budget(err) => budget_error_response(err, outcome),
    }
}

fn rejection_response(rejection: Rejection) -> Response {
    error_envelope(
        StatusCode::from_u16(rejection.http_status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        rejection.error_type(),
        format!("{rejection:?}"),
    )
}

/// `POST /v1/chat/completions`. Order (locked by conformance): non-JSON
/// body -> `invalid_json`; valid JSON that isn't an object -> `invalid_body`;
/// only then are control-plane headers parsed (see [`profile::parse_profile`]),
/// followed by the local decision + execution path
/// ([`dispatch::dispatch_local`]).
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

    let parsed = match parse_profile(&headers, model, state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(err) => return err.into_response(),
    };

    let messages = extract_messages(object);
    let prompt = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    // R0 finding: TS's cancellation propagation (server.ts's req.on("close"))
    // never actually fires -- by the time it's attached, the request body
    // (and with it, that stream's own "close") has already completed. This
    // token has no client-disconnect signal wired to it from axum/hyper
    // yet either, but dispatch_local's own drop-based guards (see its doc)
    // still correctly release a budget reservation and propagate
    // cancellation into the Supervisor call if *this handler's own future*
    // is dropped mid-request (e.g. a future connection-level timeout or
    // abort layered on top) -- genuinely different from, and strictly
    // better than, a listener that structurally can never fire.
    let budget_ledger = state.budget_ledger.as_deref();
    match dispatch_local(
        &state.cards,
        state.supervisor.as_ref(),
        budget_ledger,
        &parsed,
        &prompt,
        messages,
        tokio_util::sync::CancellationToken::new(),
    )
    .await
    {
        Err(DispatchError::Rejection(rejection)) => rejection_response(rejection),
        Err(DispatchError::Internal(message)) => {
            error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message)
        }
        Ok(outcome) => match &outcome.result {
            Err(failure) => dispatch_failure_response(failure, &outcome),
            Ok(chat_response) => {
                let requested_model = model.unwrap_or(chat_response.model.as_str());
                let body = openai_chat_completion(&chat_response.content, requested_model, &prompt);
                let mut response = (StatusCode::OK, Json(body)).into_response();
                apply_decision_headers(&mut response, &outcome);
                // X-iDoris-Cost-Minor: only set for a genuinely paid,
                // settled candidate -- omitted for free/local calls.
                if let Some(cost_minor) = outcome.actual_cost_minor
                    && let Ok(v) = HeaderValue::from_str(&cost_minor.to_string())
                {
                    response.headers_mut().insert(HEADER_COST_MINOR, v);
                }
                response
            }
        },
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
    use idoris_contracts::ComponentCard;
    use idoris_contracts::common::{FallbackPolicy, PrivacyClass};
    use idoris_contracts::component_card::{Egress, Form};
    use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
    use tower::ServiceExt;

    use super::*;

    fn sample_component_card(id: &str) -> ComponentCard {
        ComponentCard {
            provider: ProviderDescriptor {
                id: id.to_string(),
                family: Family::Local,
                tier: idoris_contracts::common::Tier::Local,
                capabilities: vec![idoris_contracts::common::Capability::Chat],
                privacy_class: PrivacyClass::LocalOnly,
                cost: Cost {
                    input_per_m: 0.0,
                    output_per_m: 0.0,
                },
                locality: Locality::Loopback,
                extensions: None,
            },
            form: Form::HttpService,
            endpoint: "http://127.0.0.1:8740".to_string(),
            version_pin: "0.0.0".to_string(),
            privacy_class: PrivacyClass::LocalOnly,
            allowed_egress: vec![Egress::Loopback],
            fallback_policy: FallbackPolicy::FailClosed,
            fail_closed: true,
            load_policy: None,
            extensions: None,
        }
    }

    fn paid_component_card(id: &str) -> ComponentCard {
        let mut card = sample_component_card(id);
        card.provider.cost = Cost {
            input_per_m: 1_000_000.0,
            output_per_m: 2_000_000.0,
        };
        card
    }

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
    async fn health_components_count_reflects_loaded_cards() {
        let state = AppState {
            cards: vec![sample_component_card("a"), sample_component_card("b")],
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["components"], 2);
    }

    #[tokio::test]
    async fn unknown_routes_return_404_not_found() {
        // Matches `packages/router/src/server.ts`'s own catch-all exactly
        // (`conformance/tests/response-headers.test.ts` locks 404 for an
        // unknown path); `/v1/models` has its own real-route tests
        // elsewhere and is no longer a stand-in for "genuinely unimplemented".
        let app = build_app(AppState::default());
        let response = app
            .oneshot(Request::builder().uri("/nope").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "not_found");
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
    async fn chat_completions_no_components_is_local_only_unavailable() {
        // No cards at all: decide() rejects before any candidate is
        // chosen, so there's no X-iDoris-Served-Locality to set, and per
        // R2-D task 3, zero remote egress is trivially true (no remote
        // path exists yet).
        let app = build_app(AppState::default());
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response.headers().contains_key(HEADER_SERVED_LOCALITY));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "local_only_unavailable");
    }

    /// A candidate was chosen (decide() succeeded) but no Supervisor is
    /// wired: still 503 local_only_unavailable, but now WITH
    /// X-iDoris-Served-Locality set, since a candidate genuinely was
    /// selected before the failure.
    #[tokio::test]
    async fn chat_completions_candidate_chosen_but_no_supervisor_still_sets_served_locality() {
        let state = AppState {
            cards: vec![sample_component_card("local-1")],
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
            "loopback"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "local_only_unavailable");
        // reason_code carries the specific backend error, distinct from
        // the spec-level `type` -- here it's supervisor_unavailable, not
        // some other reason a real backend call could fail for.
        assert_eq!(json["error"]["reason_code"], "supervisor_unavailable");
    }

    #[tokio::test]
    async fn chat_completions_succeeds_against_a_mock_supervisor() {
        let card = sample_component_card("local-1");
        let adapter = std::sync::Arc::new(idoris_backend::MockAdapter::new(vec![
            idoris_backend::ModelInfo {
                id: "local-1".to_string(),
                memory_gb: 1.0,
            },
        ]));
        let supervisor =
            idoris_backend::Supervisor::spawn(adapter, idoris_backend::SupervisorConfig::default())
                .unwrap();
        let state = AppState {
            cards: vec![card],
            supervisor: Some(supervisor),
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hello there"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
            "loopback"
        );
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
        assert!(!response.headers().contains_key(HEADER_COST_MINOR)); // free: no charge
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["object"], "chat.completion");
        assert_eq!(json["model"], "idoris/daily");
        assert!(
            json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .contains("hello there")
        );
    }

    #[tokio::test]
    async fn chat_completions_paid_candidate_without_a_ledger_is_a_server_error() {
        let state = AppState {
            cards: vec![paid_component_card("paid-1")],
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        // Not 402: no ledger at all is a setup gap, not a spend decision.
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(
            response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
            "loopback"
        );
    }

    // TempDir must outlive the BudgetLedger using its path.
    fn configured_budget_ledger(
        limit_minor: i64,
    ) -> (tempfile::TempDir, idoris_tenancy::budget::BudgetLedger) {
        let dir = tempfile::TempDir::new().unwrap();
        let ledger =
            idoris_tenancy::budget::BudgetLedger::open(dir.path().join("b.sqlite3")).unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                limit_minor,
                "UTC",
                idoris_tenancy::budget::SpendGate::PaidOnly,
            )
            .unwrap();
        (dir, ledger)
    }

    #[tokio::test]
    async fn chat_completions_paid_candidate_over_budget_is_402_with_a_structured_body() {
        let (_dir, ledger) = configured_budget_ledger(1);
        let state = AppState {
            cards: vec![paid_component_card("paid-1")],
            budget_ledger: Some(std::sync::Arc::new(ledger)),
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
        assert_eq!(
            response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
            "loopback"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "budget_exceeded");
        assert_eq!(json["error"]["reason_code"], "budget_exceeded");
        assert_eq!(json["error"]["tenant_id"], budget::PERSONAL_TENANT_ID);
        assert!(json["error"]["topup_hint"].is_string());
    }

    #[tokio::test]
    async fn chat_completions_paid_candidate_settles_and_sets_the_cost_header() {
        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let adapter = std::sync::Arc::new(idoris_backend::MockAdapter::new(vec![
            idoris_backend::ModelInfo {
                id: "paid-1".to_string(),
                memory_gb: 1.0,
            },
        ]));
        let supervisor =
            idoris_backend::Supervisor::spawn(adapter, idoris_backend::SupervisorConfig::default())
                .unwrap();
        let state = AppState {
            cards: vec![paid_component_card("paid-1")],
            supervisor: Some(supervisor),
            budget_ledger: Some(std::sync::Arc::new(ledger)),
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let cost_header = response
            .headers()
            .get(HEADER_COST_MINOR)
            .unwrap()
            .to_str()
            .unwrap();
        assert!(cost_header.parse::<i64>().unwrap() > 0);
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
    /// wrong method is a bare 405 with no body and no envelope at all —
    /// `build_app` wires `.fallback(not_found)` onto this specific route to
    /// override that default, matching `server.ts`'s own behavior exactly
    /// (its sequential `if` checks never match `GET /v1/chat/completions`
    /// either, so it falls through to the same 404 catch-all a genuinely
    /// unknown path gets — TS has no separate "405 wrong method" case).
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
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert!(response.headers().contains_key(HEADER_RECORD_ID));
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "not_found");
    }
}
