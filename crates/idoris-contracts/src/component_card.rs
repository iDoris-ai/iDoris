//! `packages/contracts/schema/component-card.schema.json` (06 §10.2):
//! every pluggable component (backend, router policy source, ...)
//! self-describes with one of these.

use serde::{Deserialize, Serialize};

use crate::common::{FallbackPolicy, PrivacyClass};
use crate::error::{Contract, ContractError, non_empty};
use crate::load_policy::LoadPolicy;
use crate::provider::ProviderDescriptor;
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    HttpService,
    SpawnCli,
    BundledBinary,
    NostrNode,
    MitmProxy,
    BatchJob,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Egress {
    None,
    Loopback,
    Lan,
    Internet,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentCard {
    pub provider: ProviderDescriptor,
    pub form: Form,
    pub endpoint: String,
    pub version_pin: String,
    pub privacy_class: PrivacyClass,
    /// `minItems: 1`.
    pub allowed_egress: Vec<Egress>,
    pub fallback_policy: FallbackPolicy,
    pub fail_closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load_policy: Option<LoadPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub extensions: Option<serde_json::Map<String, serde_json::Value>>,
}

impl Contract for ComponentCard {
    fn validate(&self) -> Result<(), ContractError> {
        self.provider.validate()?;
        if let Some(load_policy) = &self.load_policy {
            load_policy.validate()?;
        }
        if !non_empty(&self.endpoint) {
            return Err(ContractError::new(
                "component_card.endpoint must not be empty",
            ));
        }
        if !non_empty(&self.version_pin) {
            return Err(ContractError::new(
                "component_card.version_pin must not be empty",
            ));
        }
        if self.allowed_egress.is_empty() {
            return Err(ContractError::new(
                "component_card.allowed_egress must have at least one entry",
            ));
        }
        Ok(())
    }
}

impl SchemaShape for ComponentCard {
    const SCHEMA_FILE: &'static str = "component-card.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "provider",
        "form",
        "endpoint",
        "version_pin",
        "privacy_class",
        "allowed_egress",
        "fallback_policy",
        "fail_closed",
        "load_policy",
        "extensions",
    ];
    const REQUIRED: &'static [&'static str] = &[
        "provider",
        "form",
        "endpoint",
        "version_pin",
        "privacy_class",
        "allowed_egress",
        "fallback_policy",
        "fail_closed",
    ];
}
