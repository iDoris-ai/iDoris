//! Read-only Admin API v0 response contracts used by the Agent24 M4 status card.

use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminCapacityState {
    Observed,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAdmissionStatus {
    Ready,
    RequiresEviction,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminCapacityEntry {
    pub id: String,
    pub capability: String,
    pub resident: bool,
    pub estimated_memory_gb: f64,
    pub ctx_limit: u64,
    pub queue_depth: u64,
    pub admission_status: AdminAdmissionStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminCapacitySnapshot {
    pub state: AdminCapacityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<AdminCapacityEntry>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminStatusResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub contract_version: String,
    pub instance_id: String,
    pub components: usize,
    pub runtimes: usize,
    pub subscriptions: usize,
    pub budget_configured: bool,
    pub audit_configured: bool,
    pub capacity: AdminCapacitySnapshot,
}

impl Contract for AdminStatusResponse {
    fn validate(&self) -> Result<(), ContractError> {
        if self.status != "ok" || self.service != "idoris" {
            return Err(ContractError::new("admin status/service literal mismatch"));
        }
        if !non_empty(&self.version)
            || !non_empty(&self.contract_version)
            || !non_empty(&self.instance_id)
        {
            return Err(ContractError::new(
                "admin status identity fields must not be empty",
            ));
        }
        match (&self.capacity.state, &self.capacity.entries) {
            (AdminCapacityState::Observed, Some(entries)) => {
                if entries.iter().any(|entry| entry.estimated_memory_gb < 0.0) {
                    return Err(ContractError::new("admin capacity memory must be >= 0"));
                }
            }
            (AdminCapacityState::Unavailable | AdminCapacityState::Error, None) => {}
            _ => return Err(ContractError::new("admin capacity state/entries mismatch")),
        }
        Ok(())
    }
}

impl SchemaShape for AdminStatusResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-status.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "status",
        "service",
        "version",
        "contract_version",
        "instance_id",
        "components",
        "runtimes",
        "subscriptions",
        "budget_configured",
        "audit_configured",
        "capacity",
    ];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}
