//! Read-only Admin API v0 status facts.
//!
//! This module deliberately does not expose an HTTP route. The follow-up
//! listener/auth slice must bind the admin surface to loopback and require a
//! management session token before these facts become remotely reachable.

use serde::Serialize;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;
use idoris_policy::ROLES;

pub const ADMIN_PORT_ENV: &str = "IDORIS_ADMIN_PORT";
pub const ADMIN_BIND_HOST: std::net::Ipv4Addr = std::net::Ipv4Addr::LOCALHOST;

const SESSION_TOKEN_LEN: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminPortError(String);

impl std::fmt::Display for AdminPortError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for AdminPortError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdminBindConfig {
    port: u16,
}

impl AdminBindConfig {
    /// Parse the dedicated Admin listener port. There is deliberately no
    /// implicit default until the public Admin port contract assigns one.
    pub fn parse(raw: Option<&str>) -> Result<Self, AdminPortError> {
        let raw = raw
            .ok_or_else(|| AdminPortError(format!("{ADMIN_PORT_ENV} 必须显式设置为 1-65535")))?
            .trim();
        if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(AdminPortError(format!(
                "{ADMIN_PORT_ENV} 不是合法的端口号：'{raw}'。请设置为 1-65535 之间的整数"
            )));
        }
        let port: u64 = raw.parse().map_err(|_| {
            AdminPortError(format!(
                "{ADMIN_PORT_ENV} 不是合法的端口号：'{raw}'。请设置为 1-65535 之间的整数"
            ))
        })?;
        if !(1..=u16::MAX as u64).contains(&port) {
            return Err(AdminPortError(format!(
                "{ADMIN_PORT_ENV} 不是合法的端口号：'{raw}'。请设置为 1-65535 之间的整数"
            )));
        }
        Ok(Self { port: port as u16 })
    }

    pub fn port(self) -> u16 {
        self.port
    }

    pub fn addr(self) -> std::net::SocketAddr {
        std::net::SocketAddr::from((ADMIN_BIND_HOST, self.port))
    }

    pub async fn bind(self) -> std::io::Result<tokio::net::TcpListener> {
        tokio::net::TcpListener::bind(self.addr()).await
    }
}

pub struct AdminSessionToken(String);

impl AdminSessionToken {
    pub fn mint() -> Self {
        Self(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        ))
    }

    /// Explicit one-time exposure for delivering the token to the local
    /// management client. Never log or persist this value.
    pub fn expose_secret(&self) -> &str {
        &self.0
    }

    pub fn matches(&self, candidate: &str) -> bool {
        if candidate.len() != SESSION_TOKEN_LEN {
            return false;
        }
        bool::from(self.0.as_bytes().ct_eq(candidate.as_bytes()))
    }
}

impl std::fmt::Debug for AdminSessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AdminSessionToken([REDACTED])")
    }
}

impl std::fmt::Display for AdminSessionToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminStatus {
    pub status: &'static str,
    pub service: &'static str,
    pub version: String,
    pub contract_version: &'static str,
    pub instance_id: String,
    pub components: usize,
    pub runtimes: usize,
    pub subscriptions: usize,
    pub budget_configured: bool,
    pub audit_configured: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminBackend {
    pub provider_id: String,
    pub locality: idoris_contracts::provider::Locality,
    pub form: idoris_contracts::component_card::Form,
    pub lifecycle_runtime_bound: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceKind {
    HttpModelsEndpoint,
    SubscriptionRegistration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceState {
    Observed,
    Configured,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceError {
    Unavailable,
    AuthenticationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminModelSource {
    pub provider_id: String,
    pub source: AdminModelSourceKind,
    pub state: AdminModelSourceState,
    pub models: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<AdminModelSourceError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminModelsSnapshot {
    pub sources: Vec<AdminModelSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AdminRole {
    pub role: &'static str,
    pub aliases: Vec<String>,
    pub catalog_role: bool,
}

pub fn status(state: &AppState) -> AdminStatus {
    AdminStatus {
        status: "ok",
        service: "idoris",
        version: env!("CARGO_PKG_VERSION").to_string(),
        contract_version: idoris_contracts::CONTRACT_VERSION,
        instance_id: state.instance_id.clone(),
        components: state.cards.len(),
        runtimes: state.runtimes.len(),
        subscriptions: state.subscriptions.len(),
        budget_configured: state.budget_ledger.is_some(),
        audit_configured: state.record_store.is_some(),
    }
}

/// Snapshot startup-validated backend configuration and local lifecycle
/// bindings only. This deliberately does not probe runtime health, models,
/// memory pressure, or network reachability.
pub fn backends(state: &AppState) -> Vec<AdminBackend> {
    state
        .cards
        .iter()
        .map(|card| AdminBackend {
            provider_id: card.provider.id.clone(),
            locality: card.provider.locality,
            form: card.form,
            lifecycle_runtime_bound: state.runtimes.get(&card.provider.id).is_some(),
        })
        .collect()
}

/// Snapshot the stable public role contract only. The aliases are accepted
/// model=idoris/<role> request names; they do not claim any configured or
/// currently available model serves the role.
pub fn roles() -> Vec<AdminRole> {
    ROLES
        .iter()
        .copied()
        .map(|role| AdminRole {
            role: role.as_str(),
            aliases: vec![format!("idoris/{}", role.as_str())],
            catalog_role: role.is_catalog_role(),
        })
        .collect()
}

/// Observe each active model source without collapsing failures into an
/// apparently complete flat list. HTTP model ids are runtime observations;
/// subscription ids are registration facts, not backend health claims.
pub async fn models(state: &AppState) -> AdminModelsSnapshot {
    let mut sources = Vec::new();
    for card in &state.cards {
        if card.form != idoris_contracts::component_card::Form::HttpService {
            continue;
        }
        let (state_kind, models, error) =
            match crate::models::observe_http_models(&state.http_client, card).await {
                Ok(crate::models::HttpModelObservation::Observed(entries)) => (
                    AdminModelSourceState::Observed,
                    entries.into_iter().map(|entry| entry.id).collect(),
                    None,
                ),
                Ok(crate::models::HttpModelObservation::Unavailable) => (
                    AdminModelSourceState::Error,
                    Vec::new(),
                    Some(AdminModelSourceError::Unavailable),
                ),
                Err(crate::models::ModelsError::UpstreamAuthenticationFailed { .. }) => (
                    AdminModelSourceState::Error,
                    Vec::new(),
                    Some(AdminModelSourceError::AuthenticationFailed),
                ),
            };
        sources.push(AdminModelSource {
            provider_id: card.provider.id.clone(),
            source: AdminModelSourceKind::HttpModelsEndpoint,
            state: state_kind,
            models,
            error,
        });
    }
    sources.extend(
        state
            .subscriptions
            .discovery()
            .into_iter()
            .map(|entry| AdminModelSource {
                provider_id: entry.provider_id,
                source: AdminModelSourceKind::SubscriptionRegistration,
                state: AdminModelSourceState::Configured,
                models: vec![entry.model_id],
                error: None,
            }),
    );
    AdminModelsSnapshot { sources }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;

    #[test]
    fn admin_port_requires_explicit_valid_tcp_port() {
        for raw in [
            None,
            Some(""),
            Some("   "),
            Some("0"),
            Some("65536"),
            Some("-1"),
            Some("+9"),
            Some("8x"),
        ] {
            assert!(AdminBindConfig::parse(raw).is_err(), "{raw:?}");
        }
        assert_eq!(AdminBindConfig::parse(Some(" 1 ")).unwrap().port(), 1);
        assert_eq!(AdminBindConfig::parse(Some("65535")).unwrap().port(), 65535);
    }

    #[tokio::test]
    async fn admin_bind_is_ipv4_loopback_only_and_conflicts_fail_closed() {
        let reservation = std::net::TcpListener::bind((ADMIN_BIND_HOST, 0)).unwrap();
        let port = reservation.local_addr().unwrap().port();
        drop(reservation);
        let config = AdminBindConfig::parse(Some(&port.to_string())).unwrap();

        assert_eq!(config.addr().ip(), std::net::IpAddr::V4(ADMIN_BIND_HOST));
        let listener = config.bind().await.unwrap();
        assert_eq!(listener.local_addr().unwrap(), config.addr());
        assert!(
            config.bind().await.is_err(),
            "an occupied Admin port must be rejected, never replaced"
        );
    }

    #[tokio::test]
    async fn data_plane_router_does_not_expose_admin_paths() {
        for path in [
            "/admin/api/v1/status",
            "/admin/api/v1/models",
            "/admin/api/v1/roles",
        ] {
            let response = crate::build_app(AppState::default())
                .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND);
        }
    }

    #[tokio::test]
    async fn backends_report_only_configured_facts_and_runtime_binding() {
        use std::sync::Arc;

        use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
        use idoris_contracts::Contract;
        use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};

        let mut lifecycle: idoris_contracts::ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        lifecycle.provider.id = "lifecycle".into();
        let mut direct = lifecycle.clone();
        direct.provider.id = "direct".into();
        direct.load_policy = Some(LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        });
        assert!(lifecycle.validate().is_ok());
        assert!(direct.validate().is_ok());
        assert!(crate::dispatch::is_resident_http_service(&direct));

        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "ignored-model".into(),
            memory_gb: 42.0,
        }]));
        let handle = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
        let runtimes = crate::runtime::RuntimeRegistry::from(
            crate::dispatch::BoundSupervisor::new(&lifecycle, handle),
        );
        let state = AppState {
            cards: vec![lifecycle, direct],
            runtimes,
            ..AppState::default()
        };

        let snapshot = backends(&state);
        assert_eq!(snapshot.len(), 2);
        assert_eq!(snapshot[0].provider_id, "lifecycle");
        assert_eq!(
            snapshot[0].locality,
            idoris_contracts::provider::Locality::Loopback
        );
        assert_eq!(
            snapshot[0].form,
            idoris_contracts::component_card::Form::HttpService
        );
        assert!(snapshot[0].lifecycle_runtime_bound);
        assert_eq!(snapshot[1].provider_id, "direct");
        assert_eq!(
            snapshot[1].locality,
            idoris_contracts::provider::Locality::Loopback
        );
        assert_eq!(
            snapshot[1].form,
            idoris_contracts::component_card::Form::HttpService
        );
        assert!(!snapshot[1].lifecycle_runtime_bound);

        let value = serde_json::to_value(&snapshot[0]).unwrap();
        assert_eq!(
            value.as_object().unwrap().keys().collect::<Vec<_>>(),
            ["form", "lifecycle_runtime_bound", "locality", "provider_id"]
        );
        assert!(value.get("health").is_none());
        assert!(value.get("models").is_none());
        assert!(value.get("memory_pressure").is_none());
        assert!(value.get("endpoint").is_none());
    }

    #[tokio::test]
    async fn models_snapshot_keeps_observed_configured_and_error_sources_distinct() {
        use crate::subscription::config::{SANDBOX_PROFILE_ID, SubscriptionConfig};
        use crate::subscription::runtime::{
            SubscriptionRuntimeHandle, SubscriptionRuntimeRegistry, authorize_subscription,
        };

        let observed_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "observed-a"}, {"id": "observed-b"}]
            })))
            .expect(1)
            .mount(&observed_server)
            .await;
        let failed_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(500))
            .expect(1)
            .mount(&failed_server)
            .await;
        let auth_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401))
            .expect(1)
            .mount(&auth_server)
            .await;

        let mut observed: idoris_contracts::ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        observed.provider.id = "observed".into();
        observed.endpoint = observed_server.uri();
        observed.version_pin = "test@1".into();
        let mut failed = observed.clone();
        failed.provider.id = "failed".into();
        failed.endpoint = failed_server.uri();
        let mut auth_failed = observed.clone();
        auth_failed.provider.id = "auth-failed".into();
        auth_failed.endpoint = auth_server.uri();

        let subscription: idoris_contracts::ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml"))
                .unwrap();
        let config = SubscriptionConfig::snapshot(
            Some("personal"),
            Some("1"),
            None,
            Some(SANDBOX_PROFILE_ID),
            Some("codex"),
        );
        let authorized = authorize_subscription(&config, &subscription)
            .unwrap()
            .unwrap();
        let handle = SubscriptionRuntimeHandle::build(authorized, &subscription).unwrap();
        let mut subscriptions = SubscriptionRuntimeRegistry::default();
        subscriptions.insert(handle).unwrap();

        let state = AppState {
            cards: vec![observed, failed, auth_failed, subscription],
            subscriptions,
            ..AppState::default()
        };
        let snapshot = models(&state).await;
        assert_eq!(snapshot.sources.len(), 4);
        assert_eq!(
            snapshot.sources[0],
            AdminModelSource {
                provider_id: "observed".into(),
                source: AdminModelSourceKind::HttpModelsEndpoint,
                state: AdminModelSourceState::Observed,
                models: vec!["observed-a".into(), "observed-b".into()],
                error: None,
            }
        );
        assert_eq!(
            snapshot.sources[1],
            AdminModelSource {
                provider_id: "failed".into(),
                source: AdminModelSourceKind::HttpModelsEndpoint,
                state: AdminModelSourceState::Error,
                models: vec![],
                error: Some(AdminModelSourceError::Unavailable),
            }
        );
        assert_eq!(
            snapshot.sources[2],
            AdminModelSource {
                provider_id: "auth-failed".into(),
                source: AdminModelSourceKind::HttpModelsEndpoint,
                state: AdminModelSourceState::Error,
                models: vec![],
                error: Some(AdminModelSourceError::AuthenticationFailed),
            }
        );
        assert_eq!(
            snapshot.sources[3],
            AdminModelSource {
                provider_id: "subscription".into(),
                source: AdminModelSourceKind::SubscriptionRegistration,
                state: AdminModelSourceState::Configured,
                models: vec!["codex-subscription".into()],
                error: None,
            }
        );
        let value = serde_json::to_value(snapshot).unwrap();
        assert!(
            value.get("data").is_none(),
            "must not publish a flat complete-looking list"
        );
        observed_server.verify().await;
        failed_server.verify().await;
        auth_server.verify().await;
    }

    #[tokio::test]
    async fn models_snapshot_rejects_partial_http_observations_but_accepts_empty() {
        let mixed_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "valid"}, {"id": 7}]
            })))
            .expect(1)
            .mount(&mixed_server)
            .await;
        let invalid_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": 7}, {"not_id": "x"}]
            })))
            .expect(1)
            .mount(&invalid_server)
            .await;
        let empty_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": []
            })))
            .expect(1)
            .mount(&empty_server)
            .await;

        let mut mixed: idoris_contracts::ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        mixed.provider.id = "mixed".into();
        mixed.endpoint = mixed_server.uri();
        mixed.version_pin = "test@1".into();
        let mut invalid = mixed.clone();
        invalid.provider.id = "invalid".into();
        invalid.endpoint = invalid_server.uri();
        let mut empty = mixed.clone();
        empty.provider.id = "empty".into();
        empty.endpoint = empty_server.uri();

        let snapshot = models(&AppState {
            cards: vec![mixed, invalid, empty],
            ..AppState::default()
        })
        .await;
        assert_eq!(snapshot.sources.len(), 3);
        for source in &snapshot.sources[..2] {
            assert_eq!(source.state, AdminModelSourceState::Error);
            assert_eq!(source.error, Some(AdminModelSourceError::Unavailable));
            assert!(source.models.is_empty());
        }
        assert_eq!(snapshot.sources[2].state, AdminModelSourceState::Observed);
        assert_eq!(snapshot.sources[2].error, None);
        assert!(snapshot.sources[2].models.is_empty());

        mixed_server.verify().await;
        invalid_server.verify().await;
        empty_server.verify().await;
    }

    #[test]
    fn roles_snapshot_locks_contract_order_aliases_and_catalog_semantics() {
        let expected = [
            ("fast", "idoris/fast", true),
            ("daily", "idoris/daily", true),
            ("deep", "idoris/deep", true),
            ("vision", "idoris/vision", true),
            ("embed", "idoris/embed", true),
            ("rerank", "idoris/rerank", true),
            ("decide", "idoris/decide", true),
            ("auto", "idoris/auto", false),
        ];
        let snapshot = roles();

        assert_eq!(snapshot.len(), expected.len());
        for (entry, (role, alias, catalog_role)) in snapshot.iter().zip(expected) {
            assert_eq!(entry.role, role);
            assert_eq!(entry.aliases, [alias]);
            assert_eq!(entry.catalog_role, catalog_role);
        }

        let value = serde_json::to_value(snapshot).unwrap();
        assert_eq!(
            value,
            serde_json::json!([
                {"role":"fast","aliases":["idoris/fast"],"catalog_role":true},
                {"role":"daily","aliases":["idoris/daily"],"catalog_role":true},
                {"role":"deep","aliases":["idoris/deep"],"catalog_role":true},
                {"role":"vision","aliases":["idoris/vision"],"catalog_role":true},
                {"role":"embed","aliases":["idoris/embed"],"catalog_role":true},
                {"role":"rerank","aliases":["idoris/rerank"],"catalog_role":true},
                {"role":"decide","aliases":["idoris/decide"],"catalog_role":true},
                {"role":"auto","aliases":["idoris/auto"],"catalog_role":false},
            ])
        );
    }

    #[test]
    fn default_status_reports_only_real_configured_surfaces() {
        let state = AppState::default();
        let status = status(&state);

        assert_eq!(status.status, "ok");
        assert_eq!(status.service, "idoris");
        assert_eq!(status.contract_version, idoris_contracts::CONTRACT_VERSION);
        assert_eq!(status.instance_id, state.instance_id);
        assert_eq!(status.components, 0);
        assert_eq!(status.runtimes, 0);
        assert_eq!(status.subscriptions, 0);
        assert!(!status.budget_configured);
        assert!(!status.audit_configured);
    }

    #[test]
    fn session_tokens_are_fresh_redacted_and_constant_time_verifiable() {
        let first = AdminSessionToken::mint();
        let second = AdminSessionToken::mint();

        assert_eq!(first.expose_secret().len(), SESSION_TOKEN_LEN);
        assert_ne!(first.expose_secret(), second.expose_secret());
        assert!(first.matches(first.expose_secret()));
        assert!(!first.matches(second.expose_secret()));
        assert!(!first.matches("short"));
        assert_eq!(format!("{first}"), "[REDACTED]");
        assert_eq!(format!("{first:?}"), "AdminSessionToken([REDACTED])");
    }
}
