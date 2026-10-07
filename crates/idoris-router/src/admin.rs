//! Read-only Admin API v0 status facts.
//!
//! This module deliberately does not expose an HTTP route. The follow-up
//! listener/auth slice must bind the admin surface to loopback and require a
//! management session token before these facts become remotely reachable.

use serde::Serialize;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;

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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

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
        let response = crate::build_app(AppState::default())
            .oneshot(
                Request::builder()
                    .uri("/admin/api/v1/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn backends_report_only_configured_facts_and_runtime_binding() {
        use std::sync::Arc;

        use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};

        let mut lifecycle: idoris_contracts::ComponentCard =
            serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
        lifecycle.provider.id = "lifecycle".into();
        let mut direct = lifecycle.clone();
        direct.provider.id = "direct".into();
        direct.form = idoris_contracts::component_card::Form::BundledBinary;
        direct.provider.locality = idoris_contracts::provider::Locality::Remote;

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
            idoris_contracts::provider::Locality::Remote
        );
        assert_eq!(
            snapshot[1].form,
            idoris_contracts::component_card::Form::BundledBinary
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
