//! `packages/contracts/schema/provider.schema.json` — canonical
//! `ProviderDescriptor` (06 §10.1).

use serde::{Deserialize, Serialize};

use crate::common::{Capability, PrivacyClass, Tier};
use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Family {
    Idoris,
    Claude,
    Openai,
    Local,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Locality {
    Loopback,
    Lan,
    Remote,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cost {
    pub input_per_m: f64,
    pub output_per_m: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderDescriptor {
    pub id: String,
    pub family: Family,
    pub tier: Tier,
    /// `minItems: 1` — enforced in [`Contract::validate`], not by the type.
    pub capabilities: Vec<Capability>,
    pub privacy_class: PrivacyClass,
    pub cost: Cost,
    pub locality: Locality,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Contract for ProviderDescriptor {
    fn validate(&self) -> Result<(), ContractError> {
        if !non_empty(&self.id) {
            return Err(ContractError::new("provider.id must not be empty"));
        }
        if self.capabilities.is_empty() {
            return Err(ContractError::new(
                "provider.capabilities must have at least one entry",
            ));
        }
        if self.cost.input_per_m < 0.0 {
            return Err(ContractError::new("provider.cost.input_per_m must be >= 0"));
        }
        if self.cost.output_per_m < 0.0 {
            return Err(ContractError::new(
                "provider.cost.output_per_m must be >= 0",
            ));
        }
        Ok(())
    }
}

impl SchemaShape for ProviderDescriptor {
    const SCHEMA_FILE: &'static str = "provider.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "id",
        "family",
        "tier",
        "capabilities",
        "privacy_class",
        "cost",
        "locality",
        "extensions",
    ];
    const REQUIRED: &'static [&'static str] = &[
        "id",
        "family",
        "tier",
        "capabilities",
        "privacy_class",
        "cost",
        "locality",
    ];
}
