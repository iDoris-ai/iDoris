//! Read-only Admin API v0 status facts.
//!
//! This module deliberately does not expose an HTTP route. The follow-up
//! listener/auth slice must bind the admin surface to loopback and require a
//! management session token before these facts become remotely reachable.

use serde::Serialize;

use crate::AppState;

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
}
