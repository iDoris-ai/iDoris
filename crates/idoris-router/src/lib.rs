//! Rust skeleton for the iDoris router. R1 scope only: binds to a
//! hardcoded loopback address, serves `GET /health` in the shape PR #46
//! settled on, tags every response with a server-generated
//! `X-iDoris-Record-Id`, and answers every other route with `501` in the
//! unified error envelope from the interface spec §3.11. No routing,
//! backend dispatch, or policy logic is ported here — see the root
//! `README.md`.

/// Control-plane header parsing (R2-D task 1).
pub mod profile;

/// Packaged binary command-line parsing.
pub mod cli;

/// Offline intent embedding primitives (B1 task 09).
pub mod intent;

/// Component card loading from `IDORIS_COMPONENTS_DIR` (R2-D task 2); wired
/// into `AppState`/`/health`'s `components` count in a follow-up PR.
pub mod components;
/// Executable-relative bundled config resolution with explicit-path overrides.
pub mod config;

/// Validated chat-request correlation identifiers for later Event Log use.
pub mod correlation;

/// Routing-policy loading from `IDORIS_ROUTING_POLICY` (R2-D task 2); wired
/// into `AppState` in a follow-up PR.
pub mod routing_policy;

/// The local decision + execution path (R2-D task 3): `decide()` → (if
/// needed) `Supervisor` load → `Supervisor` chat.
pub mod dispatch;

/// Read-only Admin API v0 status facts. HTTP exposure is intentionally
/// deferred until the dedicated loopback + session-token listener slice.
pub mod admin;

/// Per-card runtime construction for lifecycle-managed providers.
pub mod runtime;

/// Atomic reserve/settle/release around a paid candidate (R2-D task 4); not
/// yet wired into `dispatch`/the request path — a follow-up PR does that.
pub mod budget;

/// Per-provider cooldown for models discovery.
pub mod health;

/// `GET /v1/models` (R2-G): aggregates every registered `http_service`
/// card's own model listing.
pub mod models;

/// Metadata-only audit validation and tenant-scoped persistence (B1 task19).
pub mod audit;
mod audit_body;
/// Trusted Authorization bearer -> virtual-key caller identity.
pub mod auth;
/// Injectable `/capabilities` provider boundary (B1 task32). Live capacity
/// aggregation is wired by task33.
pub mod capabilities;
pub mod host_facts;
/// Stable four-way routing/audit reason taxonomy.
pub mod reason;
/// Persistent record/budget storage bootstrap (B1 task18).
pub mod storage;
mod usage;

/// Direct HTTP forwarding for a generic `http_service` component card's
/// `POST /v1/chat/completions` (R2-G) — retries, idempotency cache; wired
/// into `chat_completions`/`AppState` below (see the module's own doc for
/// why this is a genuinely separate path from `dispatch::dispatch_local`).
pub mod proxy;
pub mod queries;

mod sse;
mod supervisor_parameters;
mod supervisor_stream;

/// Per-connection deadlines for stalled HTTP response writes.
pub mod write_timeout;

/// Real TCP connection lifetime and per-request cancellation tokens.
pub mod connection;

/// Subscription registration/runtime/dispatch gates.
pub mod subscription;

use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use axum::body::{Body, Bytes};
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use idoris_backend::{BackendError, ChatMessage};
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_policy::Rejection;
use idoris_tenancy::budget::{BUDGET_EXCEEDED_REASON_CODE, BudgetError};
use serde::Serialize;
use serde_json::json;
use uuid::Uuid;

use dispatch::{DispatchError, DispatchFailure, Selected, dispatch_local, reason_header_value};
use profile::{ParsedProfile, ProfileError, parse_profile};

const HEADER_SERVED_LOCALITY: &str = "X-iDoris-Served-Locality";
const HEADER_REASON: &str = "X-iDoris-Reason";
const HEADER_DEGRADED: &str = "X-iDoris-Degraded";
const HEADER_COST_MINOR: &str = "X-iDoris-Cost-Minor";
const HEADER_REQUEST_ID: &str = "x-idoris-request-id";
const HEADER_CACHED: &str = "X-iDoris-Cached";
const HEADER_ORIGIN_RECORD_ID: &str = "X-iDoris-Origin-Record-Id";

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
    /// Startup-validated YAML routing policy. Tests use an in-memory local
    /// default so `AppState::default()` performs no filesystem I/O.
    pub routing_policy: idoris_contracts::RoutingPolicy,
    /// Lifecycle runtimes keyed by provider id. Missing selected providers
    /// fail closed instead of falling through to another backend.
    pub runtimes: runtime::RuntimeRegistry,
    /// Gates every candidate, including zero cost, through the ledger's
    /// SpendGate. `None` fails paid candidates closed while free candidates
    /// remain usable without a ledger. `Arc`
    /// because `BudgetLedger` (wraps a `Mutex<Connection>`) isn't `Clone`.
    pub budget_ledger: Option<Arc<idoris_tenancy::budget::BudgetLedger>>,
    /// Tenant-scoped audit/usage record store. Startup installs it together
    /// with `budget_ledger` from the same SQLite path.
    pub record_store: Option<Arc<std::sync::Mutex<idoris_tenancy::store::TenantStore>>>,
    /// Production installs the persistent B5 verifier. Library tests may
    /// leave this unset to preserve pre-B5 request behavior.
    pub virtual_key_authenticator: Option<auth::VirtualKeyAuthenticator>,
    /// Persistent B6 Event Log capability. This slice only bootstraps and
    /// carries the handle; request event emission is wired separately.
    pub event_log: Option<Arc<idoris_tenancy::event_log::EventLogStore>>,
    /// Best-effort audit persistence failures. Audit must not alter the HTTP
    /// result already produced by routing/backend execution.
    pub audit_failures: Arc<AtomicU64>,
    /// Outbound HTTP client for `GET /v1/models` (this PR) and the direct
    /// `http_service` chat-forwarding path (follow-up PR) — one client
    /// shared across requests so its connection pool is actually reused,
    /// matching `reqwest::Client`'s own documented cloning contract (cheap,
    /// `Arc`-backed clone, not a new connection pool per clone).
    pub http_client: reqwest::Client,
    /// Shared across requests and AppState clones.
    pub models_health: Arc<health::HealthTracker>,
    /// Capacity surface provider. `None` is deliberately unavailable rather
    /// than a fake static snapshot; task33 installs the live implementation.
    pub capabilities: Option<Arc<dyn capabilities::CapabilitiesProvider>>,
    /// Authorized subscription CLI runtimes. Empty means default-off/disabled.
    pub subscriptions: subscription::runtime::SubscriptionRuntimeRegistry,
    /// R2-G: direct-forward path for a `LoadMode::Resident` `http_service`
    /// candidate (see `dispatch::select`/`is_resident_http_service`'s doc).
    /// `Arc` because `ChatProxy` holds a `Mutex`-guarded cache, same reason
    /// `budget_ledger` above is `Arc`-wrapped rather than `Clone`.
    pub proxy: Arc<proxy::ChatProxy>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("instance_id", &self.instance_id)
            .field("deploy_mode", &self.deploy_mode)
            .field("cards", &self.cards)
            .field("routing_policy", &self.routing_policy)
            .field("runtimes", &self.runtimes)
            .field(
                "budget_ledger",
                &self.budget_ledger.as_ref().map(|_| "BudgetLedger { .. }"),
            )
            .field(
                "record_store",
                &self.record_store.as_ref().map(|_| "TenantStore { .. }"),
            )
            .field("event_log", &self.event_log.as_ref().map(|_| "configured"))
            .field(
                "virtual_key_authenticator",
                &self
                    .virtual_key_authenticator
                    .as_ref()
                    .map(|_| "configured"),
            )
            .field(
                "audit_failures",
                &self.audit_failures.load(Ordering::Relaxed),
            )
            .field("http_client", &self.http_client)
            .field(
                "capabilities",
                &self.capabilities.as_ref().map(|_| "configured"),
            )
            .field("subscriptions", &self.subscriptions.len())
            .field("proxy", &"ChatProxy { .. }")
            .finish()
    }
}

impl Default for AppState {
    fn default() -> Self {
        let http_client = match idoris_upstream::http_client() {
            Ok(client) => client,
            Err(_) => panic!("failed to build the router upstream HTTP client"),
        };
        Self {
            instance_id: Uuid::new_v4().to_string(),
            deploy_mode: profile::deploy_mode_from_env(
                std::env::var("IDORIS_DEPLOY_MODE").ok().as_deref(),
            ),
            cards: Vec::new(),
            routing_policy: idoris_contracts::RoutingPolicy {
                routing_policy: idoris_contracts::routing_policy::RoutingPolicyInner {
                    version: 1,
                    rules: Vec::new(),
                    default: idoris_contracts::routing_policy::Action {
                        tiers: Some(vec![idoris_contracts::common::Tier::Local]),
                        fail_closed: Some(true),
                        ..Default::default()
                    },
                },
            },
            runtimes: runtime::RuntimeRegistry::default(),
            budget_ledger: None,
            record_store: None,
            virtual_key_authenticator: None,
            event_log: None,
            audit_failures: Arc::new(AtomicU64::new(0)),
            models_health: Arc::new(health::HealthTracker::default()),
            capabilities: None,
            subscriptions: subscription::runtime::SubscriptionRuntimeRegistry::default(),
            proxy: Arc::new(proxy::ChatProxy::new(http_client.clone())),
            http_client,
        }
    }
}

/// Builds the full axum app: `GET /health`, `POST /v1/chat/completions`, a
/// `501` fallback for everything else, and the `X-iDoris-Record-Id`
/// middleware applied to every response.
pub fn build_app(state: AppState) -> Router {
    let state = Arc::new(state);
    Router::new()
        .route("/health", get(health))
        .route("/v1/models", get(list_models).fallback(not_found))
        .route("/capabilities", get(get_capabilities).fallback(not_found))
        .route(
            "/idoris/tenants/{tenant_id}/usage",
            get(get_tenant_usage).fallback(not_found),
        )
        .route(
            "/idoris/tenants/{tenant_id}/audit",
            get(get_tenant_audit).fallback(not_found),
        )
        .route(
            "/idoris/tenants/{tenant_id}/budget",
            get(get_tenant_budget).fallback(not_found),
        )
        .route(
            "/v1/chat/completions",
            post(chat_completions).fallback(not_found),
        )
        .route("/v1/embeddings", post(embeddings).fallback(not_found))
        .route("/v1/rerank", post(rerank).fallback(not_found))
        .route("/v1/messages", post(messages).fallback(not_found))
        .fallback(not_found)
        .layer(middleware::from_fn(
            connection::request_lifecycle_middleware,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            record_id_middleware,
        ))
        .with_state(state)
}

async fn embeddings(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    body: Bytes,
) -> Response {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
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
    let model = object.get("model").and_then(serde_json::Value::as_str);
    let mut parsed = match parse_profile(&headers, model, state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    // The protocol endpoint is authoritative for the required capability.
    // Caller headers may further constrain routing elsewhere, but they cannot
    // turn an embeddings request into a chat/rerank dispatch.
    parsed.task.capabilities = Some(vec![Capability::Embedding]);

    let (cards, _) = dispatch::policy_cards(&state.cards, &state.routing_policy, &parsed);
    let cards = cards
        .into_iter()
        .filter(dispatch::is_resident_http_service)
        .collect::<Vec<_>>();
    let selected = match dispatch::select(&cards, &parsed, "") {
        Ok(selected) => selected,
        Err(DispatchError::Rejection(rejection)) => return rejection_response(rejection),
        Err(DispatchError::Internal(message)) => {
            return error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message);
        }
    };

    // B7-02 reuses the direct buffered transport. Paid embeddings need a
    // distinct usage-settlement contract (OpenAI embeddings do not report
    // chat completion_tokens), so this slice must fail closed before egress.
    let cost = &selected.card.provider.cost;
    if cost.input_per_m != 0.0 || cost.output_per_m != 0.0 {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "paid_proxy_unavailable",
            "paid embeddings are unavailable until embeddings usage settlement is defined",
        );
    }
    let _reservation = match dispatch::ReservationGuard::reserve(
        state.budget_ledger.as_deref(),
        parsed.tenant_id.as_deref(),
        &selected.card.provider.id,
        selected.estimated_cost_minor,
    ) {
        Ok(guard) => guard,
        Err(err) => {
            return budget_error_response(
                &err,
                &dispatch::ChatOutcome {
                    decision: selected.decision.clone(),
                    served_locality: selected.served_locality,
                    result: Err(DispatchFailure::Budget(err.clone())),
                    actual_cost_minor: None,
                },
            );
        }
    };

    let opts = proxy::ForwardOpts {
        request_id: headers
            .get(HEADER_REQUEST_ID)
            .and_then(|value| value.to_str().ok()),
        tenant_id: parsed.tenant_id.as_deref(),
        record_id: &record_id,
        provider_id: selected.card.provider.id.as_str(),
        served_locality: selected.served_locality,
        privacy: parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
        require_openai_usage: false,
    };
    let outcome = state
        .proxy
        .forward_buffered_path(
            &selected.card.endpoint,
            proxy::BufferedUpstreamPath::EMBEDDINGS,
            &value,
            &opts,
        )
        .await;
    let status = StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, outcome.body).into_response();
    let content_type = outcome
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(locality_str(
        outcome
            .replayed_served_locality
            .unwrap_or(selected.served_locality),
    )) {
        response.headers_mut().insert(HEADER_SERVED_LOCALITY, value);
    }
    if outcome.cached {
        response.extensions_mut().insert(usage::UsageFact::cached());
        response
            .headers_mut()
            .insert(HEADER_CACHED, HeaderValue::from_static("true"));
        if let Some(origin) = outcome.origin_record_id
            && let Ok(value) = HeaderValue::from_str(&origin)
        {
            response
                .headers_mut()
                .insert(HEADER_ORIGIN_RECORD_ID, value);
        }
    } else if status.is_success() {
        response
            .extensions_mut()
            .insert(usage::UsageFact::inference(Some(0)));
    }
    response
}

async fn rerank(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    body: Bytes,
) -> Response {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
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
    let model = object.get("model").and_then(serde_json::Value::as_str);
    let mut parsed = match parse_profile(&headers, model, state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    parsed.task.capabilities = Some(vec![Capability::Rerank]);

    let (cards, _) = dispatch::policy_cards(&state.cards, &state.routing_policy, &parsed);
    let cards = cards
        .into_iter()
        .filter(dispatch::is_resident_http_service)
        .collect::<Vec<_>>();
    let selected = match dispatch::select(&cards, &parsed, "") {
        Ok(selected) => selected,
        Err(DispatchError::Rejection(rejection)) => return rejection_response(rejection),
        Err(DispatchError::Internal(message)) => {
            return error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message);
        }
    };

    let cost = &selected.card.provider.cost;
    if cost.input_per_m != 0.0 || cost.output_per_m != 0.0 {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "paid_proxy_unavailable",
            "paid rerank is unavailable until rerank usage settlement is defined",
        );
    }
    let _reservation = match dispatch::ReservationGuard::reserve(
        state.budget_ledger.as_deref(),
        parsed.tenant_id.as_deref(),
        &selected.card.provider.id,
        selected.estimated_cost_minor,
    ) {
        Ok(guard) => guard,
        Err(err) => {
            return budget_error_response(
                &err,
                &dispatch::ChatOutcome {
                    decision: selected.decision.clone(),
                    served_locality: selected.served_locality,
                    result: Err(DispatchFailure::Budget(err.clone())),
                    actual_cost_minor: None,
                },
            );
        }
    };

    let opts = proxy::ForwardOpts {
        request_id: headers
            .get(HEADER_REQUEST_ID)
            .and_then(|value| value.to_str().ok()),
        tenant_id: parsed.tenant_id.as_deref(),
        record_id: &record_id,
        provider_id: selected.card.provider.id.as_str(),
        served_locality: selected.served_locality,
        privacy: parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
        require_openai_usage: false,
    };
    let outcome = state
        .proxy
        .forward_buffered_path(
            &selected.card.endpoint,
            proxy::BufferedUpstreamPath::RERANK,
            &value,
            &opts,
        )
        .await;
    let status = StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, outcome.body).into_response();
    let content_type = outcome
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(locality_str(
        outcome
            .replayed_served_locality
            .unwrap_or(selected.served_locality),
    )) {
        response.headers_mut().insert(HEADER_SERVED_LOCALITY, value);
    }
    if outcome.cached {
        response.extensions_mut().insert(usage::UsageFact::cached());
        response
            .headers_mut()
            .insert(HEADER_CACHED, HeaderValue::from_static("true"));
        if let Some(origin) = outcome.origin_record_id
            && let Ok(value) = HeaderValue::from_str(&origin)
        {
            response
                .headers_mut()
                .insert(HEADER_ORIGIN_RECORD_ID, value);
        }
    } else if status.is_success() {
        response
            .extensions_mut()
            .insert(usage::UsageFact::inference(Some(0)));
    }
    response
}

async fn messages(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    body: Bytes,
) -> Response {
    let value: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(value) => value,
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
    if let Some(stream) = object.get("stream") {
        match stream.as_bool() {
            Some(false) => {}
            Some(true) | None => {
                return error_envelope_with_reason(
                    StatusCode::BAD_REQUEST,
                    "unsupported_field",
                    "unsupported_stream",
                    "streaming /v1/messages is not supported by this endpoint yet",
                );
            }
        }
    }
    let model = object.get("model").and_then(serde_json::Value::as_str);
    let mut parsed = match parse_profile(&headers, model, state.deploy_mode) {
        Ok(parsed) => parsed,
        Err(error) => return error.into_response(),
    };
    parsed.task.capabilities = Some(vec![Capability::Chat]);

    let (cards, _) = dispatch::policy_cards(&state.cards, &state.routing_policy, &parsed);
    let cards = cards
        .into_iter()
        .filter(dispatch::is_resident_http_service)
        .collect::<Vec<_>>();
    let selected = match dispatch::select(&cards, &parsed, "") {
        Ok(selected) => selected,
        Err(DispatchError::Rejection(rejection)) => return rejection_response(rejection),
        Err(DispatchError::Internal(message)) => {
            return error_envelope(StatusCode::INTERNAL_SERVER_ERROR, "internal_error", message);
        }
    };

    let cost = &selected.card.provider.cost;
    if cost.input_per_m != 0.0 || cost.output_per_m != 0.0 {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "paid_proxy_unavailable",
            "paid messages are unavailable until Anthropic usage settlement is defined",
        );
    }
    let _reservation = match dispatch::ReservationGuard::reserve(
        state.budget_ledger.as_deref(),
        parsed.tenant_id.as_deref(),
        &selected.card.provider.id,
        selected.estimated_cost_minor,
    ) {
        Ok(guard) => guard,
        Err(err) => {
            return budget_error_response(
                &err,
                &dispatch::ChatOutcome {
                    decision: selected.decision.clone(),
                    served_locality: selected.served_locality,
                    result: Err(DispatchFailure::Budget(err.clone())),
                    actual_cost_minor: None,
                },
            );
        }
    };

    let opts = proxy::ForwardOpts {
        request_id: headers
            .get(HEADER_REQUEST_ID)
            .and_then(|value| value.to_str().ok()),
        tenant_id: parsed.tenant_id.as_deref(),
        record_id: &record_id,
        provider_id: selected.card.provider.id.as_str(),
        served_locality: selected.served_locality,
        privacy: parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly),
        require_openai_usage: false,
    };
    let outcome = state
        .proxy
        .forward_buffered_path(
            &selected.card.endpoint,
            proxy::BufferedUpstreamPath::MESSAGES,
            &value,
            &opts,
        )
        .await;
    let status = StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, outcome.body).into_response();
    let content_type = outcome
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    if let Ok(value) = HeaderValue::from_str(content_type) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(locality_str(
        outcome
            .replayed_served_locality
            .unwrap_or(selected.served_locality),
    )) {
        response.headers_mut().insert(HEADER_SERVED_LOCALITY, value);
    }
    if outcome.cached {
        response.extensions_mut().insert(usage::UsageFact::cached());
        response
            .headers_mut()
            .insert(HEADER_CACHED, HeaderValue::from_static("true"));
        if let Some(origin) = outcome.origin_record_id
            && let Ok(value) = HeaderValue::from_str(&origin)
        {
            response
                .headers_mut()
                .insert(HEADER_ORIGIN_RECORD_ID, value);
        }
    } else if status.is_success() {
        response
            .extensions_mut()
            .insert(usage::UsageFact::inference(Some(0)));
    }
    response
}

async fn get_tenant_usage(
    State(state): State<Arc<AppState>>,
    Path(tenant_id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<queries::usage::UsageQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return error_envelope(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                "usage query must contain only period=YYYY-MM",
            );
        }
    };
    let scope_tenant = query_scope_header(&headers).map(str::to_string);
    let (Some(store), Some(ledger)) = (state.record_store.clone(), state.budget_ledger.clone())
    else {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage_unavailable",
            "tenant usage storage is not configured",
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        let guard = store
            .lock()
            .map_err(|_| queries::usage::UsageQueryError::StorePoisoned)?;
        queries::usage::query_usage(&guard, &ledger, &tenant_id, scope_tenant.as_deref(), &query)
    })
    .await;
    match result {
        Ok(Ok(usage)) => Json(usage).into_response(),
        Ok(Err(
            err @ (queries::usage::UsageQueryError::ScopeRequired
            | queries::usage::UsageQueryError::ScopeMismatch),
        )) => error_envelope(
            StatusCode::BAD_REQUEST,
            "invalid_tenant_scope",
            err.to_string(),
        ),
        Ok(Err(queries::usage::UsageQueryError::Billing(
            idoris_tenancy::billing::BillingAggregateError::Period(err),
        ))) => error_envelope(StatusCode::BAD_REQUEST, "invalid_query", err.to_string()),
        Ok(Err(queries::usage::UsageQueryError::Budget(
            idoris_tenancy::budget::BudgetError::TenantNotConfigured { tenant_id },
        ))) => error_envelope(
            StatusCode::NOT_FOUND,
            "tenant_not_found",
            format!("tenant {tenant_id:?} is not configured"),
        ),
        Ok(Err(err)) => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "usage_unavailable",
            err.to_string(),
        ),
        Err(_) => error_envelope(
            StatusCode::INTERNAL_SERVER_ERROR,
            "usage_unavailable",
            "usage query worker failed",
        ),
    }
}

async fn get_tenant_audit(
    State(state): State<Arc<AppState>>,
    Path(tenant_id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<queries::audit::AuditQuery>, axum::extract::rejection::QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(_) => {
            return error_envelope(
                StatusCode::BAD_REQUEST,
                "invalid_query",
                "audit query accepts only from, to, limit, and record_id",
            );
        }
    };
    let scope_tenant = query_scope_header(&headers).map(str::to_string);
    let Some(store) = state.record_store.clone() else {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_unavailable",
            "tenant audit storage is not configured",
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        let guard = store
            .lock()
            .map_err(|_| queries::audit::AuditQueryError::StorePoisoned)?;
        queries::audit::query_audit(&guard, &tenant_id, scope_tenant.as_deref(), &query)
    })
    .await;
    match result {
        Ok(Ok(audit)) => Json(audit).into_response(),
        Ok(Err(
            err @ (queries::audit::AuditQueryError::ScopeRequired
            | queries::audit::AuditQueryError::ScopeMismatch),
        )) => error_envelope(
            StatusCode::BAD_REQUEST,
            "invalid_tenant_scope",
            err.to_string(),
        ),
        Ok(Err(
            err @ (queries::audit::AuditQueryError::InvalidLimit
            | queries::audit::AuditQueryError::InvalidRange),
        )) => error_envelope(StatusCode::BAD_REQUEST, "invalid_query", err.to_string()),
        Ok(Err(err)) => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit_unavailable",
            err.to_string(),
        ),
        Err(_) => error_envelope(
            StatusCode::INTERNAL_SERVER_ERROR,
            "audit_unavailable",
            "audit query worker failed",
        ),
    }
}

async fn get_tenant_budget(
    State(state): State<Arc<AppState>>,
    Path(tenant_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let scope_tenant = query_scope_header(&headers).map(str::to_string);
    let Some(ledger) = state.budget_ledger.clone() else {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "budget_unavailable",
            "tenant budget storage is not configured",
        );
    };
    let result = tokio::task::spawn_blocking(move || {
        queries::budget::query_budget(&ledger, &tenant_id, scope_tenant.as_deref())
    })
    .await;
    match result {
        Ok(Ok(view)) => Json(view).into_response(),
        Ok(Err(
            err @ (queries::budget::BudgetQueryError::ScopeRequired
            | queries::budget::BudgetQueryError::ScopeMismatch),
        )) => error_envelope(
            StatusCode::BAD_REQUEST,
            "invalid_tenant_scope",
            err.to_string(),
        ),
        Ok(Err(queries::budget::BudgetQueryError::Budget(
            idoris_tenancy::budget::BudgetError::TenantNotConfigured { tenant_id },
        ))) => error_envelope(
            StatusCode::NOT_FOUND,
            "tenant_not_found",
            format!("tenant {tenant_id:?} is not configured"),
        ),
        Ok(Err(err)) => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "budget_unavailable",
            err.to_string(),
        ),
        Err(_) => error_envelope(
            StatusCode::INTERNAL_SERVER_ERROR,
            "budget_unavailable",
            "budget query worker failed",
        ),
    }
}

fn query_scope_header(headers: &HeaderMap) -> Option<&str> {
    let mut values = headers.get_all("x-idoris-tenant").iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    first
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

async fn get_capabilities(State(state): State<Arc<AppState>>) -> Response {
    let Some(provider) = state.capabilities.as_ref() else {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "capabilities_unavailable",
            "capacity provider is not configured",
        );
    };
    match provider.snapshot().await {
        Ok(entries) => Json(entries).into_response(),
        Err(_) => error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "capabilities_unavailable",
            "capacity snapshot is unavailable",
        ),
    }
}

/// `GET /v1/models` (interface spec — `owned_by` is each card's
/// `provider.id`, matching `server.ts`). Delegates to [`models::list_models`];
/// authentication failures surface as structured upstream errors.
async fn list_models(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    match models::list_models_with_subscriptions(
        &state.http_client,
        &state.cards,
        &state.models_health,
        &state.subscriptions,
    )
    .await
    {
        Ok(models) => Json(models).into_response(),
        Err(models::ModelsError::UpstreamAuthenticationFailed { locality, .. }) => {
            let mut response = error_envelope_with_reason(
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "upstream_authentication_failed",
                "Model discovery authentication failed; check the upstream credential configuration",
            );
            response.headers_mut().insert(
                HEADER_SERVED_LOCALITY,
                HeaderValue::from_static(locality_str(locality)),
            );
            response
        }
    }
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

/// Very rough token estimate (`ceil(UTF-16 code units / 4)`), matching the same
/// order-of-magnitude heuristic `packages/adapters/subscription/relay.ts`
/// uses for its own OpenAI-shaped response — a real per-model tokenizer
/// isn't wired in at this layer. Purely informational (`usage` in the
/// response body); no billing decision reads this.
fn rough_token_estimate(text: &str) -> u64 {
    if text.is_empty() {
        0
    } else {
        (text.encode_utf16().count() as u64).div_ceil(4).max(1)
    }
}

/// Builds an OpenAI `chat.completion`-shaped body, mirroring
/// `openAIChatCompletion` (`packages/adapters/subscription/relay.ts`).
/// The returned model is the backend's resolved identity, never the caller's
/// alias or requested value.
fn openai_chat_completion(
    content: &str,
    served_model: &str,
    prompt: &str,
) -> (serde_json::Value, u64, u64) {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let prompt_tokens = rough_token_estimate(prompt);
    let completion_tokens = rough_token_estimate(content);
    let body = json!({
        "id": format!("chatcmpl-idoris-{}", Uuid::new_v4()),
        "object": "chat.completion",
        "created": created,
        "model": served_model,
        "choices": [{"index": 0, "message": {"role": "assistant", "content": content}, "finish_reason": "stop"}],
        "usage": {
            "prompt_tokens": prompt_tokens,
            "completion_tokens": completion_tokens,
            "total_tokens": prompt_tokens + completion_tokens,
        },
    });
    (body, prompt_tokens, completion_tokens)
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

/// A budget-ledger failure gating a selected candidate.
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

fn virtual_key_unauthorized_response() -> Response {
    error_envelope_with_reason(
        StatusCode::UNAUTHORIZED,
        "unauthorized",
        "VIRTUAL_KEY_UNAUTHORIZED",
        "virtual key authentication failed",
    )
}

fn virtual_key_scope_response(error: auth::VirtualKeyScopeError) -> Response {
    error_envelope_with_reason(
        StatusCode::FORBIDDEN,
        "policy_violation",
        error.reason_code(),
        "virtual key scope forbids this request",
    )
}

fn correlation_error_response(error: correlation::CorrelationError) -> Response {
    error_envelope_with_reason(
        StatusCode::BAD_REQUEST,
        "invalid_correlation_header",
        error.reason_code(),
        "invalid correlation header",
    )
}

fn event_log_unavailable_response() -> Response {
    error_envelope_with_reason(
        StatusCode::SERVICE_UNAVAILABLE,
        "event_log_unavailable",
        "EVENT_LOG_APPEND_UNAVAILABLE",
        "event log unavailable",
    )
}

async fn run_event_log_write<F>(write: F) -> Result<(), ()>
where
    F: FnOnce() -> Result<(), idoris_tenancy::event_log::EventLogError> + Send + 'static,
{
    tokio::task::spawn_blocking(write)
        .await
        .map_err(|_| ())?
        .map_err(|_| ())
}

async fn append_request_received(
    store: Arc<idoris_tenancy::event_log::EventLogStore>,
    tenant_id: String,
    record_id: String,
    correlation: &correlation::RequestCorrelation,
) -> Result<(), ()> {
    let ts_utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(())?;
    let event = idoris_tenancy::event_log::NewEvent {
        event_id: Uuid::new_v4().to_string(),
        tenant_id: tenant_id.clone(),
        record_id,
        event_type: idoris_tenancy::event_log::EventType::RequestReceived,
        ts_utc_ms,
        request_id: None,
        session_id: correlation.session_id.clone(),
        trace_id: correlation.trace_id.clone(),
        parent_id: correlation.parent_id.clone(),
        origin_record_id: None,
        metadata: Default::default(),
    };
    run_event_log_write(move || store.append(Some(&tenant_id), &event).map(|_| ())).await
}

async fn append_profiled(
    store: Arc<idoris_tenancy::event_log::EventLogStore>,
    tenant_id: String,
    record_id: String,
    correlation: &correlation::RequestCorrelation,
    parsed: &ParsedProfile,
) -> Result<(), ()> {
    let privacy = match parsed
        .task
        .privacy
        .unwrap_or(idoris_contracts::common::PrivacyClass::LocalOnly)
    {
        idoris_contracts::common::PrivacyClass::LocalOnly => "local_only",
        idoris_contracts::common::PrivacyClass::Any => "any",
    };
    let mut metadata = std::collections::BTreeMap::new();
    metadata.insert("privacy".to_string(), json!(privacy));
    if let Some(intent) = parsed.task.intent.as_deref() {
        metadata.insert("intent".to_string(), json!(intent));
    }
    let ts_utc_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
        .ok_or(())?;
    let event = idoris_tenancy::event_log::NewEvent {
        event_id: Uuid::new_v4().to_string(),
        tenant_id: tenant_id.clone(),
        record_id,
        event_type: idoris_tenancy::event_log::EventType::Profiled,
        ts_utc_ms,
        request_id: None,
        session_id: correlation.session_id.clone(),
        trace_id: correlation.trace_id.clone(),
        parent_id: correlation.parent_id.clone(),
        origin_record_id: None,
        metadata,
    };
    run_event_log_write(move || store.append(Some(&tenant_id), &event).map(|_| ())).await
}

/// `POST /v1/chat/completions`. Order (locked by conformance): non-JSON
/// body -> `invalid_json`; valid JSON that isn't an object -> `invalid_body`;
/// only then are control-plane headers parsed (see [`profile::parse_profile`]).
/// YAML tier policy constrains candidates before selection; privacy and intent
/// remain enforced by the policy pipeline. Resident `http_service` candidates
/// forward directly ([`chat_via_proxy`]); all others use
/// [`dispatch::dispatch_local`].
async fn chat_completions(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Extension(RequestRecordId(record_id)): Extension<RequestRecordId>,
    Extension(lifecycle): Extension<connection::RequestLifecycle>,
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
    let correlation = match correlation::parse(&headers) {
        Ok(context) => context,
        Err(error) => return correlation_error_response(error),
    };
    let event_context = if let Some(event_log) = state.event_log.clone() {
        let tenant_id = match state.deploy_mode {
            idoris_contracts::DeployMode::Personal => budget::PERSONAL_TENANT_ID.to_string(),
            idoris_contracts::DeployMode::Tenant => match parsed.tenant_id.clone() {
                Some(tenant_id) => tenant_id,
                None => return event_log_unavailable_response(),
            },
        };
        if append_request_received(
            event_log.clone(),
            tenant_id.clone(),
            record_id.clone(),
            &correlation,
        )
        .await
        .is_err()
        {
            return event_log_unavailable_response();
        }
        Some((event_log, tenant_id))
    } else {
        None
    };

    if let Some(authenticator) = &state.virtual_key_authenticator {
        let identity = match authenticator.authenticate(&headers).await {
            Ok(identity) => identity,
            Err(_) => return virtual_key_unauthorized_response(),
        };
        if let Err(error) = auth::enforce_scope(&identity, &parsed) {
            return virtual_key_scope_response(error);
        }
    }

    let messages = extract_messages(object);
    let parsed = intent::resolve_profile(parsed, &messages).await;
    if let Some((event_log, tenant_id)) = event_context
        && append_profiled(
            event_log,
            tenant_id,
            record_id.clone(),
            &correlation,
            &parsed,
        )
        .await
        .is_err()
    {
        return event_log_unavailable_response();
    }
    let prompt = messages
        .iter()
        .map(|m| m.content.as_str())
        .collect::<Vec<_>>()
        .join("\n");

    let (cards, fail_closed) = dispatch::policy_cards(&state.cards, &state.routing_policy, &parsed);
    if cards.is_empty() {
        return if fail_closed {
            rejection_response(idoris_policy::Rejection::LocalOnlyUnavailable)
        } else {
            error_envelope(
                StatusCode::SERVICE_UNAVAILABLE,
                "no_candidate",
                "no candidate matches routing policy",
            )
        };
    }

    // R2-G: a Resident-mode http_service candidate (a generic
    // OpenAI-compatible backend, including a conformance fixture pointing
    // at a fake upstream) is forwarded directly -- never through the
    // Supervisor, which only makes sense for a real oMLX-shaped backend
    // with an explicit load/unload lifecycle. See dispatch::select's doc
    // for the accepted double-decide() tradeoff this branch makes.
    if let Ok(selected) = dispatch::select(&cards, &parsed, &prompt) {
        // A present model field must be a non-empty string before any
        // selected backend can execute. Keep selection first so the
        // established error still carries the selected locality/reasons.
        if object.contains_key("model") && model.is_none_or(str::is_empty) {
            let requested_model = model.unwrap_or("");
            let mut response = error_envelope_with_reason(
                StatusCode::BAD_REQUEST,
                "unsupported_field",
                "unsupported_model",
                format!(
                    "model '{requested_model}' must be a non-empty string or a supported idoris/<role> alias"
                ),
            );
            if let Ok(value) = HeaderValue::from_str(locality_str(selected.served_locality)) {
                response.headers_mut().insert(HEADER_SERVED_LOCALITY, value);
            }
            let reason = reason_header_value(&selected.decision.reason_codes);
            if !reason.is_empty()
                && let Ok(value) = HeaderValue::from_str(&reason)
            {
                response.headers_mut().insert(HEADER_REASON, value);
            }
            if selected.decision.is_degraded() {
                response
                    .headers_mut()
                    .insert(HEADER_DEGRADED, HeaderValue::from_static("true"));
            }
            return response;
        }
        if selected.card.provider.id == idoris_policy::SUBSCRIPTION_PROVIDER_ID {
            let privacy = parsed
                .task
                .privacy
                .unwrap_or(idoris_contracts::common::PrivacyClass::LocalOnly);
            return match subscription::dispatch::dispatch_selected(
                &selected,
                privacy,
                lifecycle.peer(),
                &state.subscriptions,
                messages.clone(),
                lifecycle.cancellation_token(),
            )
            .await
            {
                Ok(chat) => subscription::response::success(
                    &chat,
                    &prompt,
                    &record_id,
                    object
                        .get("stream")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                ),
                Err(subscription::dispatch::SubscriptionDispatchError::Source(error)) => {
                    subscription::response::source_rejection(error, &record_id)
                }
                Err(subscription::dispatch::SubscriptionDispatchError::Relay(error)) => {
                    subscription::response::relay_failure(&error, &record_id)
                }
                Err(subscription::dispatch::SubscriptionDispatchError::PrivacyForbidden) => {
                    rejection_response(idoris_policy::Rejection::LocalOnlyUnavailable)
                }
                Err(subscription::dispatch::SubscriptionDispatchError::Runtime(error)) => {
                    error_envelope_with_reason(
                        StatusCode::SERVICE_UNAVAILABLE,
                        "subscription_runtime_unavailable",
                        "SUBSCRIPTION_RUNTIME_UNAVAILABLE",
                        error.to_string(),
                    )
                }
                Err(subscription::dispatch::SubscriptionDispatchError::NotSubscription) => {
                    error_envelope(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "subscription dispatch boundary mismatch",
                    )
                }
            };
        }
        if dispatch::is_resident_http_service(&selected.card) {
            return chat_via_proxy(&state, &selected, &headers, &parsed, &value, &record_id).await;
        }
        if let Err(message) = supervisor_stream::validate(object) {
            let mut response = error_envelope_with_reason(
                StatusCode::BAD_REQUEST,
                "unsupported_field",
                "unsupported_stream",
                message,
            );
            response.headers_mut().insert(
                HEADER_SERVED_LOCALITY,
                HeaderValue::from_static(locality_str(selected.served_locality)),
            );
            return response;
        }
        if let Err(message) = supervisor_parameters::validate(object) {
            let mut response = error_envelope_with_reason(
                StatusCode::BAD_REQUEST,
                "unsupported_field",
                "unsupported_parameter",
                message,
            );
            response.headers_mut().insert(
                HEADER_SERVED_LOCALITY,
                HeaderValue::from_static(locality_str(selected.served_locality)),
            );
            return response;
        }
    }

    // The request token is a fresh child of this TCP connection's lifetime.
    // Completing the request body does not cancel it; EOF/reset/shutdown of
    // the actual connection does.
    let selected_for_dispatch = dispatch::select(&cards, &parsed, &prompt).ok();
    let selected_estimated_cost = selected_for_dispatch
        .as_ref()
        .map(|selected| selected.estimated_cost_minor);
    let supervisor = selected_for_dispatch
        .as_ref()
        .and_then(|selected| state.runtimes.get(&selected.card.provider.id));
    let budget_ledger = state.budget_ledger.as_deref();
    match dispatch_local(
        &cards,
        supervisor,
        budget_ledger,
        &parsed,
        dispatch::DispatchInput::with_model(model, &prompt),
        messages,
        lifecycle.cancellation_token(),
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
                let (body, prompt_tokens, completion_tokens) =
                    openai_chat_completion(&chat_response.content, &chat_response.model, &prompt);
                let mut response = (StatusCode::OK, Json(body)).into_response();
                apply_decision_headers(&mut response, &outcome);
                let usage_cost_minor = match selected_estimated_cost {
                    Some(0) => Some(0),
                    Some(_) => outcome.actual_cost_minor,
                    None => None,
                };
                response
                    .extensions_mut()
                    .insert(usage::UsageFact::inference_with_tokens(
                        usage_cost_minor,
                        prompt_tokens,
                        completion_tokens,
                    ));
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

/// The R2-G direct-forward path: `selected.card` is a `LoadMode::Resident`
/// `http_service` candidate (see `dispatch::select`/`is_resident_http_service`),
/// so the request goes straight to `proxy::ChatProxy` instead of the
/// Supervisor. The upstream response is forwarded byte-for-byte (status +
/// body + its own `content-type`) — **never** re-wrapped into
/// [`openai_chat_completion`]'s shape, matching `proxy.ts`'s own behavior:
/// a transparent proxy, not a backend `RuntimeAdapter` call.
async fn chat_via_proxy(
    state: &AppState,
    selected: &Selected,
    headers: &HeaderMap,
    parsed: &ParsedProfile,
    body_value: &serde_json::Value,
    record_id: &str,
) -> Response {
    let mut reservation = match dispatch::ReservationGuard::reserve(
        state.budget_ledger.as_deref(),
        parsed.tenant_id.as_deref(),
        &selected.card.provider.id,
        selected.estimated_cost_minor,
    ) {
        Ok(guard) => guard,
        Err(err) => {
            return budget_error_response(
                &err,
                &dispatch::ChatOutcome {
                    decision: selected.decision.clone(),
                    served_locality: selected.served_locality,
                    result: Err(DispatchFailure::Budget(err.clone())),
                    actual_cost_minor: None,
                },
            );
        }
    };
    let is_paid = budget::is_paid(Some(selected.estimated_cost_minor));
    let stream_requested = body_value
        .get("stream")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    // Paid streaming still has no trustworthy terminal usage evidence.
    if is_paid && stream_requested {
        return error_envelope(
            StatusCode::SERVICE_UNAVAILABLE,
            "paid_proxy_unavailable",
            "付费流式直连尚不支持可信 usage 结算，请使用非流式请求或支持结算的后端",
        );
    }
    if stream_requested {
        return chat_via_proxy_stream(state, selected, body_value).await;
    }
    chat_via_proxy_buffered(
        state,
        selected,
        headers,
        parsed,
        body_value,
        record_id,
        &mut reservation,
    )
    .await
}

/// The streaming half of [`chat_via_proxy`]. Never touches the idempotency
/// cache or `X-iDoris-Cached`/`X-iDoris-Origin-Record-Id` — matches
/// `proxy.ts`: a streamed response is never a cache candidate.
async fn chat_via_proxy_stream(
    state: &AppState,
    selected: &Selected,
    body_value: &serde_json::Value,
) -> Response {
    match state
        .proxy
        .forward_stream(&selected.card.endpoint, body_value)
        .await
    {
        proxy::StreamOutcome::Buffered {
            status,
            body,
            content_type,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut response = (status, body).into_response();
            let content_type = content_type.as_deref().unwrap_or("application/json");
            if let Ok(v) = HeaderValue::from_str(content_type) {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, v);
            }
            if let Ok(v) = HeaderValue::from_str(locality_str(selected.served_locality)) {
                response.headers_mut().insert(HEADER_SERVED_LOCALITY, v);
            }
            response
        }
        proxy::StreamOutcome::Stream {
            status,
            content_type,
            response: upstream,
        } => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
            // Validate application termination around the permit-owning body.
            // Producer timeouts remain errors; only clean EOF without [DONE]
            // becomes an explicit truncation error.
            let body = terminated_proxy_body(upstream);
            let mut response = Response::builder()
                .status(status)
                .body(body)
                .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response());
            let content_type = content_type.as_deref().unwrap_or("text/event-stream");
            if let Ok(v) = HeaderValue::from_str(content_type) {
                response
                    .headers_mut()
                    .insert(axum::http::header::CONTENT_TYPE, v);
            }
            if let Ok(v) = HeaderValue::from_str(locality_str(selected.served_locality)) {
                response.headers_mut().insert(HEADER_SERVED_LOCALITY, v);
            }
            if status.is_success() {
                response
                    .extensions_mut()
                    .insert(usage::UsageFact::inference(Some(0)));
            }
            response
        }
    }
}

/// Preserve proxy deadlines and ownership while rejecting incomplete SSE.
fn terminated_proxy_body(upstream: Body) -> Body {
    Body::from_stream(sse::ensure_terminated(upstream.into_data_stream()))
}

/// The non-streaming half of [`chat_via_proxy`] (this PR's predecessor —
/// idempotency cache read/write, `X-iDoris-Cached`/`X-iDoris-Origin-Record-Id`).
async fn chat_via_proxy_buffered(
    state: &AppState,
    selected: &Selected,
    headers: &HeaderMap,
    parsed: &ParsedProfile,
    body_value: &serde_json::Value,
    record_id: &str,
    reservation: &mut dispatch::ReservationGuard<'_>,
) -> Response {
    let is_paid = budget::is_paid(Some(selected.estimated_cost_minor));
    let request_id = headers.get(HEADER_REQUEST_ID).and_then(|v| v.to_str().ok());
    let privacy = parsed.task.privacy.unwrap_or(PrivacyClass::LocalOnly);
    let opts = proxy::ForwardOpts {
        request_id,
        tenant_id: parsed.tenant_id.as_deref(),
        record_id,
        provider_id: selected.card.provider.id.as_str(),
        served_locality: selected.served_locality,
        privacy,
        require_openai_usage: is_paid,
    };
    if is_paid {
        // Once the paid POST is allowed to leave the process, dropping this
        // future (client disconnect / cancellation) must not silently release
        // the reservation: execution may already have happened upstream.
        reservation.retain_on_drop();
    }
    let mut paid_settlement = None;
    let outcome = if is_paid {
        state
            .proxy
            .forward_buffered_with_success_gate(
                &selected.card.endpoint,
                body_value,
                &opts,
                |outcome| {
                    let usage = budget::parse_openai_usage(&outcome.body).map_err(|_| {
                        (
                            502,
                            "upstream_usage_invalid",
                            "付费上游成功响应缺少可验证的 usage 证据",
                        )
                    })?;
                    let actual =
                        match budget::actual_cost_from_usage(&selected.card.provider.cost, usage) {
                            Ok(actual) => actual,
                            Err(_) => {
                                reservation.retain_until_expiry();
                                return Err((
                                    500,
                                    "internal_error",
                                    "付费上游 usage 无法转换为安全结算金额",
                                ));
                            }
                        };
                    match reservation.settle_replayable(actual) {
                        Ok(Some(charged)) => {
                            paid_settlement = Some((charged, usage));
                            Ok(())
                        }
                        Ok(None) => Err((500, "internal_error", "付费上游缺少可结算的预算预留")),
                        Err(_) => Err((
                            500,
                            "internal_error",
                            "付费上游结算未能确认，请稍后查询预算账本",
                        )),
                    }
                },
            )
            .await
    } else {
        state
            .proxy
            .forward_buffered(&selected.card.endpoint, body_value, &opts)
            .await
    };

    if is_paid {
        match outcome.execution {
            proxy::ExecutionDisposition::NotExecuted | proxy::ExecutionDisposition::Replay => {
                if reservation.release_now().is_err() {
                    return error_envelope(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "付费上游预算预留释放失败",
                    );
                }
            }
            proxy::ExecutionDisposition::Uncertain => {
                reservation.retain_until_expiry();
            }
            proxy::ExecutionDisposition::Executed => {
                if !(200..300).contains(&outcome.status) && reservation.release_now().is_err() {
                    return error_envelope(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal_error",
                        "付费上游预算预留释放失败",
                    );
                }
            }
        }
    }

    let status = StatusCode::from_u16(outcome.status).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut response = (status, outcome.body).into_response();
    let content_type = outcome
        .content_type
        .as_deref()
        .unwrap_or("application/json");
    if let Ok(v) = HeaderValue::from_str(content_type) {
        response
            .headers_mut()
            .insert(axum::http::header::CONTENT_TYPE, v);
    }
    // C1: on a cache hit, the *replayed* Served-Locality (recorded when the
    // entry was written) wins, never this request's freshly-computed one
    // (proxy.rs's cache-key doc) — `selected.served_locality` is only the
    // fallback for a genuine (non-cached) call.
    let served_locality = outcome
        .replayed_served_locality
        .unwrap_or(selected.served_locality);
    if let Ok(v) = HeaderValue::from_str(locality_str(served_locality)) {
        response.headers_mut().insert(HEADER_SERVED_LOCALITY, v);
    }
    if outcome.cached {
        response.extensions_mut().insert(usage::UsageFact::cached());
        response
            .headers_mut()
            .insert(HEADER_CACHED, HeaderValue::from_static("true"));
        if let Some(origin) = outcome.origin_record_id
            && let Ok(v) = HeaderValue::from_str(&origin)
        {
            response.headers_mut().insert(HEADER_ORIGIN_RECORD_ID, v);
        }
    } else if status.is_success() {
        if let Some((cost_minor, usage)) = paid_settlement {
            response
                .extensions_mut()
                .insert(usage::UsageFact::inference_with_tokens(
                    Some(cost_minor),
                    usage.input_tokens,
                    usage.output_tokens,
                ));
            if let Ok(value) = HeaderValue::from_str(&cost_minor.to_string()) {
                response.headers_mut().insert(HEADER_COST_MINOR, value);
            }
        } else {
            response
                .extensions_mut()
                .insert(usage::UsageFact::inference(Some(0)));
        }
    }
    response
}

/// Request-scoped wrapper so a handler can read *this request's own*
/// server-generated record id via `Extension<RequestRecordId>` — R2-G's
/// `chat_via_proxy` needs it before the response exists (to stash it in a
/// freshly-written idempotency-cache entry, and to know what to compare a
/// cache hit's `X-iDoris-Origin-Record-Id` against). Generated once, here,
/// *before* the handler runs — not re-derived from the response afterward.
#[derive(Clone)]
struct RequestRecordId(String);

struct BufferedAuditMeta {
    tenant_id: Option<String>,
    request_id: Option<String>,
    privacy: Option<String>,
    intent: Option<String>,
    record_id: String,
    status: StatusCode,
    origin_record_id: Option<String>,
    reason: String,
    latency_ms: u64,
}

struct UsageMeta {
    tenant_id: Option<String>,
    request_id: Option<String>,
    record_id: String,
    fact: usage::UsageFact,
}

/// Server-generated on every response, success or error, streaming or not
/// (interface spec §3.12) — never taken from a caller-supplied header.
async fn record_id_middleware(
    State(state): State<Arc<AppState>>,
    mut req: Request<Body>,
    next: Next,
) -> Response {
    let audit_started = Instant::now();
    let record_id = Uuid::new_v4().to_string();
    let audit_inference = req.method() == Method::POST
        && matches!(
            req.uri().path(),
            "/v1/chat/completions" | "/v1/embeddings" | "/v1/rerank" | "/v1/messages"
        );
    let audit_tenant = audit_tenant_id(&state, req.headers());
    let audit_request_id = audit_header(req.headers(), HEADER_REQUEST_ID, None);
    let audit_privacy = audit_header(req.headers(), "x-idoris-privacy", Some("local_only"));
    let audit_intent = audit_header(req.headers(), "x-idoris-intent", None);
    req.extensions_mut()
        .insert(RequestRecordId(record_id.clone()));
    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&record_id) {
        response.headers_mut().insert(HEADER_RECORD_ID, value);
    }
    if audit_inference {
        let usage_meta = response
            .extensions()
            .get::<usage::UsageFact>()
            .copied()
            .map(|fact| UsageMeta {
                tenant_id: audit_tenant.clone(),
                request_id: audit_request_id.clone(),
                record_id: record_id.clone(),
                fact,
            });
        let origin_record_id = response
            .headers()
            .get(HEADER_ORIGIN_RECORD_ID)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let reason = audit_reason(
            response.status(),
            response.headers().contains_key(HEADER_SERVED_LOCALITY),
            response
                .headers()
                .get(HEADER_DEGRADED)
                .is_some_and(|value| value == "true"),
        );
        let mut meta = BufferedAuditMeta {
            tenant_id: audit_tenant,
            request_id: audit_request_id,
            privacy: audit_privacy,
            intent: audit_intent,
            record_id,
            status: response.status(),
            origin_record_id,
            reason,
            latency_ms: 0,
        };
        if is_event_stream(&response) {
            let (parts, body) = response.into_parts();
            let stream_state = state.clone();
            let stream_started = audit_started;
            let stream = audit_body::finalize_stream(body.into_data_stream(), move |end| {
                let mut meta = meta;
                meta.latency_ms = elapsed_ms(stream_started);
                meta.reason = match end {
                    audit_body::StreamEnd::Completed => meta.reason,
                    audit_body::StreamEnd::Error => "degraded: stream_error".into(),
                    audit_body::StreamEnd::Dropped => "degraded: stream_cancelled".into(),
                };
                tokio::spawn(async move {
                    finish_buffered_audit(stream_state.clone(), meta).await;
                    if end == audit_body::StreamEnd::Completed {
                        finish_usage(stream_state, usage_meta).await;
                    }
                });
            });
            response = Response::from_parts(parts, Body::from_stream(stream));
        } else {
            meta.latency_ms = elapsed_ms(audit_started);
            finish_buffered_audit(state.clone(), meta).await;
            finish_usage(state, usage_meta).await;
        }
    }
    response
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u64::MAX as u128) as u64
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

fn audit_header(headers: &HeaderMap, name: &str, default: Option<&str>) -> Option<String> {
    match header_text(headers, name) {
        Some(value) if value.encode_utf16().count() <= audit::MAX_FIELD_UTF16_UNITS => {
            Some(value.to_string())
        }
        Some(_) => None,
        None => default.map(str::to_string),
    }
}

fn audit_tenant_id(state: &AppState, headers: &HeaderMap) -> Option<String> {
    match state.deploy_mode {
        idoris_contracts::DeployMode::Personal => Some(budget::PERSONAL_TENANT_ID.to_string()),
        idoris_contracts::DeployMode::Tenant => {
            header_text(headers, "x-idoris-tenant").map(str::to_string)
        }
    }
}

fn is_event_stream(response: &Response) -> bool {
    response
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("text/event-stream"))
}

async fn finish_buffered_audit(state: Arc<AppState>, meta: BufferedAuditMeta) {
    let (Some(store), Some(tenant_id)) = (state.record_store.clone(), meta.tenant_id) else {
        return;
    };
    let mut payload = serde_json::Map::new();
    payload.insert(
        "request_id".into(),
        json!(meta.request_id.unwrap_or_else(|| meta.record_id.clone())),
    );
    payload.insert("component".into(), json!("router"));
    if let Some(privacy) = meta.privacy {
        payload.insert("privacy".into(), json!(privacy));
    }
    if let Some(intent) = meta.intent {
        payload.insert("intent".into(), json!(intent));
    }
    payload.insert("status".into(), json!(meta.status.as_u16()));
    payload.insert("reason".into(), json!(meta.reason));
    payload.insert("latency_ms".into(), json!(meta.latency_ms));

    let failures = state.audit_failures.clone();
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let guard = store
            .lock()
            .map_err(|_| "audit record store lock poisoned".to_string())?;
        let writer = audit::AuditWriter::new(&guard);
        let write = audit::AuditOnce::default().finish(
            &writer,
            &tenant_id,
            &meta.record_id,
            meta.origin_record_id.as_deref(),
            &payload,
        );
        match write {
            Ok(_) => Ok(()),
            Err(audit::AuditError::Store(err)) => Err(err.to_string()),
            Err(_) => {
                let mut minimal = serde_json::Map::new();
                minimal.insert("request_id".into(), json!(meta.record_id.clone()));
                minimal.insert("component".into(), json!("router"));
                minimal.insert("status".into(), json!(meta.status.as_u16()));
                minimal.insert("reason".into(), json!(meta.reason));
                writer
                    .write_scoped(
                        &tenant_id,
                        Some(&meta.record_id),
                        meta.origin_record_id.as_deref(),
                        &minimal,
                    )
                    .map(|_| ())
                    .map_err(|err| err.to_string())
            }
        }
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            failures.fetch_add(1, Ordering::Relaxed);
            eprintln!("idoris: audit write failed: {message}");
        }
        Err(_) => {
            failures.fetch_add(1, Ordering::Relaxed);
            eprintln!("idoris: audit write worker failed");
        }
    }
}

async fn finish_usage(state: Arc<AppState>, meta: Option<UsageMeta>) {
    let (Some(store), Some(meta)) = (state.record_store.clone(), meta) else {
        return;
    };
    if !meta.fact.inference {
        return;
    }
    let Some(cost_minor) = meta.fact.cost_minor else {
        eprintln!("idoris: usage cost is unknown; refusing to persist a zero-cost guess");
        return;
    };
    let Some(tenant_id) = meta.tenant_id else {
        return;
    };
    let Some(ts_utc) = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|duration| i64::try_from(duration.as_millis()).ok())
    else {
        eprintln!("idoris: usage timestamp is outside the supported UTC epoch range");
        return;
    };
    let result = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let guard = store
            .lock()
            .map_err(|_| "usage record store lock poisoned".to_string())?;
        let entry = idoris_tenancy::usage::UsageEntry {
            ts_utc,
            cost_minor: Some(cost_minor),
            tokens_in: meta.fact.tokens_in,
            tokens_out: meta.fact.tokens_out,
            request_id: meta.request_id,
        };
        idoris_tenancy::usage::write_usage_record(&guard, Some(&tenant_id), &meta.record_id, &entry)
            .map(|_| ())
            .map_err(|err| err.to_string())
    })
    .await;
    match result {
        Ok(Ok(())) => {}
        Ok(Err(message)) => eprintln!("idoris: usage write failed: {message}"),
        Err(_) => eprintln!("idoris: usage write worker failed"),
    }
}

fn audit_reason(status: StatusCode, served_locality: bool, degraded: bool) -> String {
    if status == StatusCode::PAYMENT_REQUIRED {
        return "budget: budget_exceeded".into();
    }
    if degraded {
        return "degraded: routing_fallback".into();
    }
    if status.is_success() {
        return "intent_match: routed".into();
    }
    if status == StatusCode::SERVICE_UNAVAILABLE && !served_locality {
        return "privacy_enforced: no_eligible_candidate".into();
    }
    format!("degraded: http_{}", status.as_u16())
}

#[cfg(test)]
mod local_privacy_tests;

#[cfg(test)]
mod virtual_key_wiring_tests;

#[cfg(test)]
mod correlation_wiring_tests;

#[cfg(test)]
mod request_received_wiring_tests;

#[cfg(test)]
mod profiled_wiring_tests;

#[cfg(test)]
mod embeddings_wiring_tests;

#[cfg(test)]
mod rerank_wiring_tests;

#[cfg(test)]
mod messages_wiring_tests;

#[cfg(test)]
mod policy_wiring_tests;

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
    use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Price, SpendGate};
    use idoris_tenancy::store::{RecordKind, TenantRecord, TenantStore};
    use rusqlite::Connection;
    use serde_json::{Map, json};
    use tower::ServiceExt;

    use super::*;

    pub(super) fn sample_component_card(id: &str) -> ComponentCard {
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

    pub(super) fn paid_component_card(id: &str) -> ComponentCard {
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
    fn default_app_state_keeps_event_log_unconfigured() {
        assert!(AppState::default().event_log.is_none());
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

    pub(super) fn post_chat(body: &str, headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json");
        for (k, v) in headers {
            builder = builder.header(*k, *v);
        }
        builder.body(Body::from(body.to_owned())).unwrap()
    }

    fn memory_record_store() -> Arc<std::sync::Mutex<TenantStore>> {
        Arc::new(std::sync::Mutex::new(
            TenantStore::new(Connection::open_in_memory().unwrap()).unwrap(),
        ))
    }

    fn audit_rows(
        store: &Arc<std::sync::Mutex<TenantStore>>,
    ) -> Vec<idoris_tenancy::store::TenantRecord> {
        store
            .lock()
            .unwrap()
            .list(Some(budget::PERSONAL_TENANT_ID), Some(RecordKind::Audit))
            .unwrap()
    }

    fn usage_rows(
        store: &Arc<std::sync::Mutex<TenantStore>>,
    ) -> Vec<idoris_tenancy::store::TenantRecord> {
        store
            .lock()
            .unwrap()
            .list(Some(budget::PERSONAL_TENANT_ID), Some(RecordKind::Usage))
            .unwrap()
    }

    async fn wait_audit_rows(
        store: &Arc<std::sync::Mutex<TenantStore>>,
        expected: usize,
    ) -> Vec<idoris_tenancy::store::TenantRecord> {
        for _ in 0..100 {
            let rows = audit_rows(store);
            if rows.len() == expected {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let rows = audit_rows(store);
        assert_eq!(rows.len(), expected, "audit finalizer did not settle");
        rows
    }

    async fn wait_usage_rows(
        store: &Arc<std::sync::Mutex<TenantStore>>,
        expected: usize,
    ) -> Vec<idoris_tenancy::store::TenantRecord> {
        for _ in 0..100 {
            let rows = usage_rows(store);
            if rows.len() == expected {
                return rows;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let rows = usage_rows(store);
        assert_eq!(rows.len(), expected, "usage finalizer did not settle");
        rows
    }

    fn usage_query_state() -> (tempfile::TempDir, AppState) {
        let dir = tempfile::TempDir::new().unwrap();
        let db = dir.path().join("usage.sqlite3");
        let store = TenantStore::open(&db).unwrap();
        let ledger = BudgetLedger::open(&db).unwrap();
        ledger
            .configure_tenant("acme", 10_000, "Asia/Bangkok", SpendGate::PaidOnly)
            .unwrap();
        let mut payload = Map::new();
        payload.insert("ts_utc".into(), json!(1_789_430_400_000_i64));
        payload.insert("cost_minor".into(), json!(100));
        payload.insert("tokens_in".into(), json!(1000));
        payload.insert("tokens_out".into(), json!(500));
        store
            .put(
                Some("acme"),
                &TenantRecord {
                    tenant_id: "acme".into(),
                    kind: RecordKind::Usage,
                    record_id: "usage-1".into(),
                    request_id: "request-1".into(),
                    origin_record_id: None,
                    payload,
                },
            )
            .unwrap();
        let state = AppState {
            record_store: Some(Arc::new(std::sync::Mutex::new(store))),
            budget_ledger: Some(Arc::new(ledger)),
            ..AppState::default()
        };
        (dir, state)
    }

    fn get_usage(uri: &str, tenant: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder().method("GET").uri(uri);
        if let Some(tenant) = tenant {
            builder = builder.header("x-idoris-tenant", tenant);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn usage_query_returns_trusted_timezone_range_and_totals() {
        let (_dir, state) = usage_query_state();
        let response = build_app(state)
            .oneshot(get_usage(
                "/idoris/tenants/acme/usage?period=2026-09",
                Some("acme"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["tenant_id"], "acme");
        assert_eq!(json["billing_timezone"], "Asia/Bangkok");
        assert_eq!(json["range_utc"]["from"], "2026-08-31T17:00:00Z");
        assert_eq!(json["range_utc"]["to"], "2026-09-30T17:00:00Z");
        assert_eq!(json["totals"]["cost_minor"], json!(100.0));
        assert_eq!(json["totals"]["tokens_in"], json!(1000.0));
        assert_eq!(json["totals"]["tokens_out"], json!(500.0));
        assert_eq!(json["totals"]["calls"], 1);
    }

    #[tokio::test]
    async fn usage_query_rejects_missing_mismatched_duplicate_scope_and_timezone_override() {
        let (_dir, state) = usage_query_state();
        let app = build_app(state);
        for request in [
            get_usage("/idoris/tenants/acme/usage?period=2026-09", None),
            get_usage("/idoris/tenants/acme/usage?period=2026-09", Some("other")),
            get_usage(
                "/idoris/tenants/acme/usage?period=2026-09&timezone=UTC",
                Some("acme"),
            ),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let duplicate = Request::builder()
            .method("GET")
            .uri("/idoris/tenants/acme/usage?period=2026-09")
            .header("x-idoris-tenant", "acme")
            .header("x-idoris-tenant", "other")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(duplicate).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn usage_query_unknown_tenant_fails_closed() {
        let (_dir, state) = usage_query_state();
        let response = build_app(state)
            .oneshot(get_usage(
                "/idoris/tenants/missing/usage?period=2026-09",
                Some("missing"),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn budget_query_reports_remaining_available_and_is_read_only() {
        let (_dir, state) = usage_query_state();
        let ledger = state.budget_ledger.as_ref().unwrap().clone();
        let scope = BudgetScope::new("acme", "key-1", "omlx", "model-1");
        let reservation = ledger.reserve(&scope, Price::Known(600)).unwrap();
        let before = ledger.tenant_readview("acme").unwrap();
        assert_eq!(before.remaining_minor, 10_000);
        assert_eq!(before.reserved_minor, 600);
        assert_eq!(before.available_minor, 9_400);

        let response = build_app(state)
            .oneshot(get_usage("/idoris/tenants/acme/budget", Some("acme")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["tenant_id"], "acme");
        assert_eq!(json["billing_timezone"], "Asia/Bangkok");
        assert_eq!(json["limit_minor"], 10_000);
        assert_eq!(json["spent_minor"], 0);
        assert_eq!(json["reserved_minor"], 600);
        assert_eq!(json["remaining_minor"], 10_000);
        assert_eq!(json["available_minor"], 9_400);
        assert_eq!(json["scope"], "paid_only");

        let after = ledger.tenant_readview("acme").unwrap();
        assert_eq!(after, before);
        ledger.release("acme", &reservation).unwrap();
    }

    #[tokio::test]
    async fn budget_query_rejects_bad_scope_and_unknown_tenant() {
        let (_dir, state) = usage_query_state();
        let app = build_app(state);
        for request in [
            get_usage("/idoris/tenants/acme/budget", None),
            get_usage("/idoris/tenants/acme/budget", Some("other")),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        }

        let duplicate = Request::builder()
            .method("GET")
            .uri("/idoris/tenants/acme/budget")
            .header("x-idoris-tenant", "acme")
            .header("x-idoris-tenant", "other")
            .body(Body::empty())
            .unwrap();
        let response = app.clone().oneshot(duplicate).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let response = app
            .oneshot(get_usage("/idoris/tenants/missing/budget", Some("missing")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn audit_query_http_scopes_filters_and_rejects_unknown_params() {
        let store = memory_record_store();
        {
            let guard = store.lock().unwrap();
            for (tenant, id, ts) in [
                (budget::PERSONAL_TENANT_ID, "own", 2_000_i64),
                ("other", "private", 1_500_i64),
            ] {
                let mut payload = Map::new();
                payload.insert("ts_utc".into(), json!(ts));
                payload.insert("reason".into(), json!("intent_match"));
                guard
                    .put(
                        Some(tenant),
                        &TenantRecord {
                            tenant_id: tenant.into(),
                            kind: RecordKind::Audit,
                            record_id: id.into(),
                            request_id: format!("request-{id}"),
                            origin_record_id: None,
                            payload,
                        },
                    )
                    .unwrap();
            }
        }
        let app = build_app(AppState {
            record_store: Some(store),
            ..AppState::default()
        });
        let tenant = budget::PERSONAL_TENANT_ID;

        let own = app
            .clone()
            .oneshot(get_usage(
                &format!("/idoris/tenants/{tenant}/audit?record_id=own"),
                Some(tenant),
            ))
            .await
            .unwrap();
        assert_eq!(own.status(), StatusCode::OK);
        let body = own.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["records"].as_array().unwrap().len(), 1);
        assert_eq!(json["records"][0]["record_id"], "own");

        let cross_tenant_id = app
            .clone()
            .oneshot(get_usage(
                &format!("/idoris/tenants/{tenant}/audit?record_id=private"),
                Some(tenant),
            ))
            .await
            .unwrap();
        assert_eq!(cross_tenant_id.status(), StatusCode::OK);
        let body = cross_tenant_id
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["records"].as_array().unwrap().is_empty());

        let mismatch = app
            .clone()
            .oneshot(get_usage(
                &format!("/idoris/tenants/{tenant}/audit"),
                Some("other"),
            ))
            .await
            .unwrap();
        assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);

        let unknown = app
            .oneshot(get_usage(
                &format!("/idoris/tenants/{tenant}/audit?timezone=UTC"),
                Some(tenant),
            ))
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn chat_completions_rejects_invalid_json() {
        let store = memory_record_store();
        let app = build_app(AppState {
            record_store: Some(store.clone()),
            ..AppState::default()
        });
        let response = app
            .oneshot(post_chat("{not valid json", &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response_record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "invalid_json");
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, response_record_id);
        assert_eq!(rows[0].payload["status"], json!(400));
    }

    #[tokio::test]
    async fn oversized_intent_still_records_invalid_json_once() {
        let store = memory_record_store();
        let app = build_app(AppState {
            record_store: Some(store.clone()),
            ..AppState::default()
        });
        let long_intent = "x".repeat(audit::MAX_FIELD_UTF16_UNITS + 1);
        let response = app
            .oneshot(post_chat(
                "{not valid json",
                &[("x-idoris-intent", long_intent.as_str())],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let response_record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, response_record_id);
        assert_eq!(rows[0].payload["request_id"], json!(response_record_id));
        assert!(rows[0].payload.get("intent").is_none());
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
        let store = memory_record_store();
        let app = build_app(AppState {
            record_store: Some(store.clone()),
            ..AppState::default()
        });
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(!response.headers().contains_key(HEADER_SERVED_LOCALITY));
        let response_record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "local_only_unavailable");
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, response_record_id);
        assert_eq!(
            rows[0].payload["reason"],
            json!("privacy_enforced: no_eligible_candidate")
        );
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
        let store = memory_record_store();
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
            cards: vec![card.clone()],
            runtimes: Some(dispatch::BoundSupervisor::new(&card, supervisor)).into(),
            record_store: Some(store.clone()),
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
        assert_eq!(json["model"], "local-1");
        assert!(
            json["choices"][0]["message"]["content"]
                .as_str()
                .unwrap()
                .contains("hello there")
        );
        let usage = usage_rows(&store);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].payload["cost_minor"], json!(0));
        assert_eq!(
            usage[0].payload["tokens_in"],
            json["usage"]["prompt_tokens"]
        );
        assert_eq!(
            usage[0].payload["tokens_out"],
            json["usage"]["completion_tokens"]
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
    pub(super) fn configured_budget_ledger(
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
        let store = memory_record_store();
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
            runtimes: Some(dispatch::BoundSupervisor::new(
                &paid_component_card("paid-1"),
                supervisor,
            ))
            .into(),
            budget_ledger: Some(std::sync::Arc::new(ledger)),
            record_store: Some(store.clone()),
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
        let cost_minor = response
            .headers()
            .get(HEADER_COST_MINOR)
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!(cost_minor > 0);
        let usage = usage_rows(&store);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].payload["cost_minor"], json!(cost_minor));
        assert!(usage[0].payload["tokens_in"].is_number());
        assert!(usage[0].payload["tokens_out"].is_number());
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

    /// A `LoadMode::Resident` `http_service` card (R2-G) — forwarded via
    /// `chat_via_proxy`, never through the (unconfigured, in these tests)
    /// Supervisor.
    pub(super) fn resident_component_card(id: &str, endpoint: &str) -> ComponentCard {
        ComponentCard {
            load_policy: Some(idoris_contracts::load_policy::LoadPolicy {
                mode: idoris_contracts::load_policy::LoadMode::Resident,
                keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
                admission: idoris_contracts::load_policy::Admission::Coexist,
            }),
            endpoint: endpoint.to_string(),
            ..sample_component_card(id)
        }
    }

    #[tokio::test]
    async fn paid_buffered_proxy_settles_explicit_usage_and_cache_replay_does_not_charge_twice() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-paid",
                "choices": [{"message": {"role": "assistant", "content": "ok"}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 11, "total_tokens": 18}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &server.uri());
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let body = r#"{"messages":[]}"#;
        let first = app
            .clone()
            .oneshot(post_chat(body, &[("X-iDoris-Request-Id", "paid-once")]))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::OK);
        let charged = first
            .headers()
            .get(HEADER_COST_MINOR)
            .unwrap()
            .to_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        assert!(charged > 0);
        let settled = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!((settled.spent_minor, settled.reserved_minor), (charged, 0));

        let replay = app
            .oneshot(post_chat(body, &[("X-iDoris-Request-Id", "paid-once")]))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(
            replay.headers().get(HEADER_CACHED).unwrap(),
            HeaderValue::from_static("true")
        );
        assert!(!replay.headers().contains_key(HEADER_COST_MINOR));
        let after_replay = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(
            (after_replay.spent_minor, after_replay.reserved_minor),
            (charged, 0)
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn paid_buffered_proxy_unsettleable_usage_never_publishes_success_for_replay() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-overflow",
                "choices": [{"message": {"role": "assistant", "content": "paid secret"}}],
                "usage": {"prompt_tokens": u64::MAX, "completion_tokens": 1}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &server.uri());
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let request = || {
            post_chat(
                r#"{"messages":[]}"#,
                &[("X-iDoris-Request-Id", "paid-overflow")],
            )
        };

        let first = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!first.headers().contains_key(HEADER_CACHED));
        let first_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(first_view.spent_minor, 0);
        assert!(first_view.reserved_minor > 0);

        let replay = app.oneshot(request()).await.unwrap();
        assert_eq!(replay.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!replay.headers().contains_key(HEADER_CACHED));
        let replay_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(
            (replay_view.spent_minor, replay_view.reserved_minor),
            (0, first_view.reserved_minor)
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn paid_buffered_proxy_settlement_busy_never_publishes_success_for_replay() {
        use axum::{Router, extract::State, routing::post};
        use idoris_tenancy::budget::{DEFAULT_RESERVATION_TTL_MS, SystemClock};
        use rusqlite::TransactionBehavior;
        use tokio::sync::Notify;

        type UpstreamState = (
            std::sync::Arc<Notify>,
            std::sync::Arc<Notify>,
            std::sync::Arc<std::sync::atomic::AtomicUsize>,
        );
        async fn paid_response(
            State((received, release, calls)): State<UpstreamState>,
        ) -> Json<serde_json::Value> {
            if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                received.notify_one();
                release.notified().await;
            }
            Json(json!({
                "id": "chatcmpl-busy",
                "choices": [{"message": {"role": "assistant", "content": "paid secret"}}],
                "usage": {"prompt_tokens": 7, "completion_tokens": 11}
            }))
        }

        let received = std::sync::Arc::new(Notify::new());
        let release = std::sync::Arc::new(Notify::new());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let upstream = Router::new()
            .route("/v1/chat/completions", post(paid_response))
            .with_state((received.clone(), release.clone(), calls.clone()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let dir = tempfile::TempDir::new().unwrap();
        let db_path = dir.path().join("busy.sqlite3");
        let ledger = BudgetLedger::open_with_busy_timeout(
            &db_path,
            std::sync::Arc::new(SystemClock),
            DEFAULT_RESERVATION_TTL_MS,
            std::time::Duration::from_millis(10),
        )
        .unwrap();
        ledger
            .configure_tenant(
                budget::PERSONAL_TENANT_ID,
                1_000_000,
                "UTC",
                SpendGate::PaidOnly,
            )
            .unwrap();
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &endpoint);
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let request = || {
            post_chat(
                r#"{"messages":[]}"#,
                &[("X-iDoris-Request-Id", "paid-busy")],
            )
        };
        let first_app = app.clone();
        let first = tokio::spawn(async move { first_app.oneshot(request()).await.unwrap() });
        tokio::time::timeout(std::time::Duration::from_secs(2), received.notified())
            .await
            .expect("upstream must receive the paid POST");

        let mut blocker = Connection::open(&db_path).unwrap();
        let writer = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        release.notify_one();
        let first = first.await.unwrap();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!first.headers().contains_key(HEADER_CACHED));
        drop(writer);

        let first_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(first_view.spent_minor, 0);
        assert!(first_view.reserved_minor > 0);
        let replay = app.oneshot(request()).await.unwrap();
        assert_eq!(replay.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(!replay.headers().contains_key(HEADER_CACHED));
        let replay_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(replay_view.reserved_minor, first_view.reserved_minor);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        server.abort();
    }

    #[tokio::test]
    async fn paid_buffered_proxy_missing_usage_fails_closed_and_keeps_reservation_held() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(json!({"id": "chatcmpl-no-usage", "choices": []})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &server.uri());
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let first = app
            .clone()
            .oneshot(post_chat(
                r#"{"messages":[]}"#,
                &[("X-iDoris-Request-Id", "missing-usage")],
            ))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::BAD_GATEWAY);
        let first_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(first_view.spent_minor, 0);
        assert!(first_view.reserved_minor > 0);

        let replay = app
            .oneshot(post_chat(
                r#"{"messages":[]}"#,
                &[("X-iDoris-Request-Id", "missing-usage")],
            ))
            .await
            .unwrap();
        assert_eq!(replay.status(), StatusCode::BAD_GATEWAY);
        let replay_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(replay_view.spent_minor, 0);
        assert_eq!(replay_view.reserved_minor, first_view.reserved_minor);
        server.verify().await;
    }

    #[tokio::test]
    async fn paid_buffered_proxy_connect_failure_releases_the_reservation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);

        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &endpoint);
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });

        let response = app
            .oneshot(post_chat(r#"{"messages":[]}"#, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!((view.spent_minor, view.reserved_minor), (0, 0));
    }

    #[tokio::test]
    async fn paid_buffered_proxy_uncertain_500_keeps_only_the_original_reservation() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(500)
                    .set_body_json(json!({"error": "maybe-executed"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &server.uri());
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let request = || {
            post_chat(
                r#"{"messages":[]}"#,
                &[("X-iDoris-Request-Id", "uncertain-500")],
            )
        };

        let first = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(first.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let first_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(first_view.spent_minor, 0);
        assert!(first_view.reserved_minor > 0);

        let replay = app.oneshot(request()).await.unwrap();
        assert_eq!(replay.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let replay_view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(replay_view.spent_minor, 0);
        assert_eq!(replay_view.reserved_minor, first_view.reserved_minor);
        server.verify().await;
    }

    #[tokio::test]
    async fn dropping_paid_buffered_proxy_future_keeps_the_inflight_reservation() {
        use axum::{Router, extract::State, routing::post};
        use tokio::sync::Notify;

        async fn hold(State(received): State<std::sync::Arc<Notify>>, body: Bytes) -> &'static str {
            assert!(!body.is_empty());
            received.notify_one();
            std::future::pending::<()>().await;
            unreachable!()
        }

        let received = std::sync::Arc::new(Notify::new());
        let upstream = Router::new()
            .route("/v1/chat/completions", post(hold))
            .with_state(received.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let ledger = std::sync::Arc::new(ledger);
        let mut card = resident_component_card("paid", &endpoint);
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(ledger.clone()),
            ..AppState::default()
        });
        let request = post_chat(
            r#"{"messages":[{"role":"user","content":"paid in flight"}]}"#,
            &[("X-iDoris-Request-Id", "drop-paid")],
        );
        let task = tokio::spawn(async move { app.oneshot(request).await.unwrap() });
        tokio::time::timeout(std::time::Duration::from_secs(2), received.notified())
            .await
            .expect("upstream must receive the complete paid POST");
        task.abort();
        let _ = task.await;

        let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert_eq!(view.spent_minor, 0);
        assert!(view.reserved_minor > 0);
        server.abort();
    }

    #[tokio::test]
    async fn paid_streaming_proxy_remains_fail_closed_before_forwarding() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(wiremock::ResponseTemplate::new(200))
            .expect(0)
            .mount(&server)
            .await;
        let (_dir, ledger) = configured_budget_ledger(1_000_000);
        let mut card = resident_component_card("paid", &server.uri());
        card.provider.cost = paid_component_card("paid").provider.cost;
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(std::sync::Arc::new(ledger)),
            ..AppState::default()
        });
        let response = app
            .oneshot(post_chat(r#"{"messages":[],"stream":true}"#, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        server.verify().await;
    }

    #[tokio::test]
    async fn unknown_price_resident_proxy_is_rejected_at_pricing_before_egress() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let mut card = resident_component_card("unknown-price", &endpoint);
        card.provider.cost.input_per_m = f64::NAN;
        card.provider.cost.output_per_m = 0.0;
        let app = build_app(AppState {
            cards: vec![card],
            ..AppState::default()
        });

        let response = app
            .oneshot(post_chat(r#"{"messages":[]}"#, &[]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "no_eligible_candidate");
        assert!(
            error["error"]["remediation"]
                .as_str()
                .is_some_and(|text| text.contains("Pricing"))
        );
    }

    #[tokio::test]
    async fn free_proxy_obeys_spend_gate_for_buffered_streaming_and_cached_calls() {
        use idoris_tenancy::budget::SpendGate;
        for stream in [false, true] {
            let server = wiremock::MockServer::start().await;
            let upstream_response = if stream {
                wiremock::ResponseTemplate::new(200).set_body_raw(
                    "data: {\"id\":\"chatcmpl-test\",\"choices\":[]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                )
            } else {
                wiremock::ResponseTemplate::new(200).set_body_json(json!({"ok": true}))
            };
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(upstream_response)
                .expect(1)
                .mount(&server)
                .await;
            let (_dir, ledger) = configured_budget_ledger(0);
            let ledger = std::sync::Arc::new(ledger);
            let app = build_app(AppState {
                cards: vec![resident_component_card("omlx", &server.uri())],
                budget_ledger: Some(ledger.clone()),
                ..AppState::default()
            });
            let body =
                json!({"model": "idoris/daily", "messages": [], "stream": stream}).to_string();
            // Positive control also primes the buffered idempotency cache.
            let response = app
                .clone()
                .oneshot(post_chat(&body, &[("X-iDoris-Request-Id", "same")]))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(!response.headers().contains_key(HEADER_COST_MINOR));
            response.into_body().collect().await.unwrap();
            ledger
                .configure_tenant(budget::PERSONAL_TENANT_ID, 0, "UTC", SpendGate::All)
                .unwrap();
            let response = app
                .oneshot(post_chat(&body, &[("X-iDoris-Request-Id", "same")]))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::PAYMENT_REQUIRED);
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(error["error"]["reason_code"], "budget_exceeded");
            server.verify().await;
        }
    }

    #[tokio::test]
    async fn resident_http_service_candidate_forwards_via_proxy_byte_for_byte() {
        let server = wiremock::MockServer::start().await;
        let store = memory_record_store();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"marker": "proxied"})),
            )
            .mount(&server)
            .await;
        let state = AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            record_store: Some(store.clone()),
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
        let response_record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
            "loopback"
        );
        assert!(response.headers().get(HEADER_CACHED).is_none());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        // Raw pass-through, never re-wrapped into openai_chat_completion's
        // {object: "chat.completion", choices: [...]} shape.
        assert_eq!(json["marker"], "proxied");
        assert!(json.get("object").is_none());
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, response_record_id);
        assert_eq!(rows[0].payload["reason"], json!("intent_match: routed"));
        let usage = usage_rows(&store);
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].payload["cost_minor"], json!(0));
        assert!(
            !usage[0].payload.contains_key("tokens_in")
                && !usage[0].payload.contains_key("tokens_out"),
            "proxy usage without structured token facts must omit token fields"
        );
    }

    #[tokio::test]
    async fn oversized_request_id_still_records_success_once() {
        let server = wiremock::MockServer::start().await;
        let store = memory_record_store();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"ok": true})))
            .expect(1)
            .mount(&server)
            .await;
        let app = build_app(AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            record_store: Some(store.clone()),
            ..AppState::default()
        });
        let long_request_id = "r".repeat(audit::MAX_FIELD_UTF16_UNITS + 1);
        let body =
            "{\"model\":\"idoris/daily\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"}]}";
        let response = app
            .oneshot(post_chat(
                body,
                &[(HEADER_REQUEST_ID, long_request_id.as_str())],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let response_record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, response_record_id);
        assert_eq!(rows[0].payload["request_id"], json!(response_record_id));
        server.verify().await;
    }

    #[tokio::test]
    async fn resident_http_service_proxy_does_not_follow_upstream_redirects() {
        let closed_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let refused_address = closed_listener.local_addr().unwrap();
        drop(closed_listener);

        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "model": "idoris/daily",
                "stream": false,
                "messages": [{"role": "user", "content": "redirect guard payload"}]
            })))
            .respond_with(
                wiremock::ResponseTemplate::new(303)
                    .insert_header("Location", format!("http://{refused_address}/redirected")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let state = AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"redirect guard payload"}]}"#,
                &[],
            ))
            .await
            .unwrap();

        let received = server.received_requests().await.unwrap();
        assert_eq!(received.len(), 1);
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        server.verify().await;
    }

    #[tokio::test]
    async fn resident_http_service_second_call_with_same_request_id_is_cached() {
        let server = wiremock::MockServer::start().await;
        let store = memory_record_store();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({"n": 1})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            record_store: Some(store.clone()),
            ..AppState::default()
        };
        let app = build_app(state);
        let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
        let headers: &[(&str, &str)] = &[("x-idoris-request-id", "lib-cache-1")];
        let first = app.clone().oneshot(post_chat(body, headers)).await.unwrap();
        let first_record_id = first
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let second = app.oneshot(post_chat(body, headers)).await.unwrap();
        let second_record_id = second
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(second.headers().get(HEADER_CACHED).unwrap(), "true");
        assert_eq!(
            second
                .headers()
                .get(HEADER_ORIGIN_RECORD_ID)
                .unwrap()
                .to_str()
                .unwrap(),
            first_record_id
        );
        assert_ne!(second_record_id, first_record_id);
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 2);
        let first_row = rows
            .iter()
            .find(|row| row.record_id == first_record_id)
            .unwrap();
        let second_row = rows
            .iter()
            .find(|row| row.record_id == second_record_id)
            .unwrap();
        assert_eq!(first_row.origin_record_id, None);
        assert_eq!(
            second_row.origin_record_id.as_deref(),
            Some(first_record_id.as_str())
        );
        let usage = usage_rows(&store);
        assert_eq!(
            usage.len(),
            1,
            "cache replay must not duplicate inference usage"
        );
        assert_eq!(usage[0].record_id, first_record_id);
        server.verify().await;
    }

    /// Conformance parity (`streaming.test.ts`'s negative control): a
    /// `stream: true` request whose upstream call returns a 5xx must not
    /// retry and must come back as a plain JSON error, not an SSE stream.
    #[tokio::test]
    async fn resident_http_service_streaming_5xx_is_buffered_json_not_sse() {
        let server = wiremock::MockServer::start().await;
        let store = memory_record_store();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(502)
                    .set_body_json(serde_json::json!({"error": "upstream-down"})),
            )
            .expect(1)
            .mount(&server)
            .await;
        let state = AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            record_store: Some(store.clone()),
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
        let response_record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap(),
            "application/json"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"], "upstream-down");
        let rows = audit_rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, response_record_id);
        assert_eq!(rows[0].payload["status"], json!(502));
        server.verify().await;
    }

    #[tokio::test]
    async fn resident_http_service_stream_rejects_unterminated_eof() {
        for body in [
            "data: partial\n\n",
            "",
            "data: [DONE]",
            ": data: [DONE]\n\n",
            "data: [DONE]\ndata: extra\n\n",
            "data: {\"content\":\"[DONE]\"}\n\n",
        ] {
            let server = wiremock::MockServer::start().await;
            let store = memory_record_store();
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"),
                )
                .mount(&server)
                .await;
            let app = build_app(AppState {
                cards: vec![resident_component_card("omlx", &server.uri())],
                record_store: Some(store.clone()),
                ..AppState::default()
            });
            let response = app.oneshot(post_chat(
                r#"{"model":"idoris/daily","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            )).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(audit_rows(&store).is_empty());
            let err = response
                .into_body()
                .collect()
                .await
                .expect_err("bare EOF must fail");
            assert!(err.to_string().contains("unterminated upstream SSE"));
            let rows = wait_audit_rows(&store, 1).await;
            assert_eq!(rows[0].payload["reason"], json!("degraded: stream_error"));
            assert!(usage_rows(&store).is_empty());
        }
    }

    #[tokio::test]
    async fn resident_http_service_streaming_2xx_passes_sse_bytes_through() {
        let server = wiremock::MockServer::start().await;
        let store = memory_record_store();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/chat/completions"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        "data: SENTINEL-STREAM-CONTENT\n\ndata: [DONE]\n\n",
                        "text/event-stream",
                    ),
            )
            .mount(&server)
            .await;
        let state = AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            record_store: Some(store.clone()),
            ..AppState::default()
        };
        let app = build_app(state);
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            audit_rows(&store).is_empty(),
            "headers must not finalize SSE audit"
        );
        assert!(
            response
                .headers()
                .get(axum::http::header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
                .contains("text/event-stream")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(text.contains("SENTINEL-STREAM-CONTENT"));
        assert!(text.contains("[DONE]"));
        let rows = wait_audit_rows(&store, 1).await;
        assert_eq!(rows[0].record_id, record_id);
        assert_eq!(rows[0].payload["reason"], json!("intent_match: routed"));
        assert!(rows[0].payload["latency_ms"].is_u64());
        assert!(
            !rows[0].payload.values().any(|value| {
                value
                    .as_str()
                    .is_some_and(|text| text.contains("SENTINEL-STREAM-CONTENT"))
            }),
            "stream content must never enter audit metadata"
        );
        let usage = wait_usage_rows(&store, 1).await;
        assert_eq!(usage[0].record_id, record_id);
        assert_eq!(usage[0].payload["cost_minor"], json!(0));
        assert!(!usage[0].payload.contains_key("tokens_in"));
        assert!(!usage[0].payload.contains_key("tokens_out"));
    }

    #[tokio::test]
    async fn dropping_stream_body_finalizes_cancelled_audit_once() {
        let server = wiremock::MockServer::start().await;
        let store = memory_record_store();
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .respond_with(
                wiremock::ResponseTemplate::new(200)
                    .set_body_raw("data: later\n\ndata: [DONE]\n\n", "text/event-stream"),
            )
            .mount(&server)
            .await;
        let app = build_app(AppState {
            cards: vec![resident_component_card("omlx", &server.uri())],
            record_store: Some(store.clone()),
            ..AppState::default()
        });
        let response = app
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        let record_id = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(audit_rows(&store).is_empty());
        drop(response);
        let rows = wait_audit_rows(&store, 1).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].record_id, record_id);
        assert_eq!(
            rows[0].payload["reason"],
            json!("degraded: stream_cancelled")
        );
        assert!(usage_rows(&store).is_empty());
    }
}
