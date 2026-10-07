//! Read-only Admin API v0 status facts.
//!
//! This module deliberately does not expose an HTTP route. The follow-up
//! listener/auth slice must bind the admin surface to loopback and require a
//! management session token before these facts become remotely reachable.

use serde::Serialize;
use subtle::ConstantTimeEq;
use uuid::Uuid;

use crate::AppState;

const SESSION_TOKEN_LEN: usize = 64;

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

#[cfg(test)]
mod tests {
    use super::*;

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
