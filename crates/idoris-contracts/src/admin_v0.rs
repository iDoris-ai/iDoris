//! Read-only Admin API v0 response contracts used by the Agent24 M4 status card.

use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;
use crate::{component_card::Form, provider::Locality};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminBackend {
    pub provider_id: String,
    pub locality: Locality,
    pub form: Form,
    pub lifecycle_runtime_bound: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdminBackendsResponse(pub Vec<AdminBackend>);

impl Contract for AdminBackendsResponse {
    fn validate(&self) -> Result<(), ContractError> {
        if self
            .0
            .iter()
            .any(|backend| !non_empty(&backend.provider_id))
        {
            return Err(ContractError::new(
                "admin backend provider_id must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRole {
    pub role: String,
    pub aliases: Vec<String>,
    pub catalog_role: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdminRolesResponse(pub Vec<AdminRole>);

impl Contract for AdminRolesResponse {
    fn validate(&self) -> Result<(), ContractError> {
        if self.0.iter().any(|role| {
            !non_empty(&role.role)
                || role.aliases.is_empty()
                || role.aliases.iter().any(|alias| !non_empty(alias))
        }) {
            return Err(ContractError::new(
                "admin role names and aliases must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRuntimeState {
    Observed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRuntimePressure {
    Ok,
    Soft,
    Hard,
    Ceiling,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRuntime {
    pub provider_id: String,
    pub state: AdminRuntimeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<AdminRuntimePressure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_memory_max_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRuntimesResponse {
    pub runtimes: Vec<AdminRuntime>,
}

impl Contract for AdminRuntimesResponse {
    fn validate(&self) -> Result<(), ContractError> {
        for runtime in &self.runtimes {
            if !non_empty(&runtime.provider_id) {
                return Err(ContractError::new(
                    "admin runtime provider_id must not be empty",
                ));
            }
            match runtime.state {
                AdminRuntimeState::Observed => {
                    let (Some(_), Some(used_gb), Some(max_gb), Some(loaded)) = (
                        runtime.pressure,
                        runtime.used_gb,
                        runtime.model_memory_max_gb,
                        runtime.loaded.as_ref(),
                    ) else {
                        return Err(ContractError::new(
                            "observed admin runtime must include all observed facts",
                        ));
                    };
                    if !used_gb.is_finite()
                        || used_gb < 0.0
                        || !max_gb.is_finite()
                        || max_gb < 0.0
                        || loaded.iter().any(|model| !non_empty(model))
                    {
                        return Err(ContractError::new(
                            "admin runtime facts must be finite/non-negative with non-empty model ids",
                        ));
                    }
                }
                AdminRuntimeState::Error => {
                    if runtime.pressure.is_some()
                        || runtime.used_gb.is_some()
                        || runtime.model_memory_max_gb.is_some()
                        || runtime.loaded.is_some()
                    {
                        return Err(ContractError::new(
                            "error admin runtime must not expose partial backend facts",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl SchemaShape for AdminRuntimesResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-runtimes.schema.json";
    const PROPERTIES: &'static [&'static str] = &["runtimes"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceKind {
    HttpModelsEndpoint,
    SubscriptionRegistration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceState {
    Observed,
    Configured,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceError {
    Unavailable,
    AuthenticationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminModelSource {
    pub provider_id: String,
    pub source: AdminModelSourceKind,
    pub state: AdminModelSourceState,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AdminModelSourceError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminModelsResponse {
    pub sources: Vec<AdminModelSource>,
}

impl Contract for AdminModelsResponse {
    fn validate(&self) -> Result<(), ContractError> {
        for source in &self.sources {
            if !non_empty(&source.provider_id) || source.models.iter().any(|id| !non_empty(id)) {
                return Err(ContractError::new(
                    "admin model provider/model ids must not be empty",
                ));
            }
            match (source.source, source.state, source.error) {
                (
                    AdminModelSourceKind::HttpModelsEndpoint,
                    AdminModelSourceState::Observed,
                    None,
                ) => {}
                (
                    AdminModelSourceKind::SubscriptionRegistration,
                    AdminModelSourceState::Configured,
                    None,
                ) if !source.models.is_empty() => {}
                (
                    AdminModelSourceKind::HttpModelsEndpoint,
                    AdminModelSourceState::Error,
                    Some(_),
                ) if source.models.is_empty() => {}
                _ => {
                    return Err(ContractError::new(
                        "admin model source/state/error shape mismatch",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl SchemaShape for AdminModelsResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-models.schema.json";
    const PROPERTIES: &'static [&'static str] = &["sources"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

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
                if entries.iter().any(|entry| {
                    !entry.estimated_memory_gb.is_finite() || entry.estimated_memory_gb < 0.0
                }) {
                    return Err(ContractError::new(
                        "admin capacity memory must be finite and >= 0",
                    ));
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
