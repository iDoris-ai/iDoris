//! `packages/contracts/schema/deploy-mode.schema.json`.

use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeployMode {
    Personal,
    Tenant,
}

impl Contract for DeployMode {
    fn validate(&self) -> Result<(), ContractError> {
        Ok(())
    }
}

/// The schema's flat `enum: [...]` list — checked against the live schema
/// file by `tests/contract_drift.rs` (this type has no `properties`/
/// `required`, so it doesn't implement [`crate::shape::SchemaShape`]).
pub const DEPLOY_MODE_VALUES: &[&str] = &["personal", "tenant"];
