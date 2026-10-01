//! `packages/contracts/schema/routing-policy.schema.json` (06 §10.6):
//! declarative routing policy so the router itself stays swappable.

use serde::{Deserialize, Serialize};

use crate::common::{Capability, Complexity, PrivacyClass, Tier};
use crate::error::{Contract, ContractError};
use crate::load_policy::LoadMode;
use crate::shape::SchemaShape;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<PrivacyClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<Complexity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<Capability>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Action {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tiers: Option<Vec<Tier>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_closed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capability: Option<Capability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub load: Option<LoadMode>,
}

impl Action {
    /// `minProperties: 1` — mirrors the schema's own way of saying "an
    /// action must actually say something".
    fn is_empty(&self) -> bool {
        self.tiers.is_none()
            && self.fail_closed.is_none()
            && self.capability.is_none()
            && self.load.is_none()
    }

    fn validate(&self, path: &str) -> Result<(), ContractError> {
        if self.is_empty() {
            return Err(ContractError::new(format!(
                "{path} must set at least one of tiers/fail_closed/capability/load"
            )));
        }
        if let Some(tiers) = &self.tiers
            && tiers.is_empty()
        {
            return Err(ContractError::new(format!(
                "{path}.tiers must not be empty"
            )));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    #[serde(rename = "if")]
    pub if_: Condition,
    pub then: Action,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicyInner {
    pub version: i64,
    pub rules: Vec<Rule>,
    pub default: Action,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutingPolicy {
    pub routing_policy: RoutingPolicyInner,
}

impl Contract for RoutingPolicy {
    fn validate(&self) -> Result<(), ContractError> {
        let inner = &self.routing_policy;
        if inner.version < 1 {
            return Err(ContractError::new("routing_policy.version must be >= 1"));
        }
        for (i, rule) in inner.rules.iter().enumerate() {
            rule.then
                .validate(&format!("routing_policy.rules[{i}].then"))?;
        }
        inner.default.validate("routing_policy.default")?;
        Ok(())
    }
}

impl SchemaShape for RoutingPolicy {
    const SCHEMA_FILE: &'static str = "routing-policy.schema.json";
    const PROPERTIES: &'static [&'static str] = &["routing_policy"];
    const REQUIRED: &'static [&'static str] = &["routing_policy"];
}
