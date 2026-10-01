//! `packages/contracts/schema/task-profile.schema.json` — control-plane
//! task metadata (06 §10.5). Every field is optional at the wire level;
//! defaults ("chat", "simple", ["chat"], "local_only") are a TS/router
//! concern, not enforced here.

use serde::{Deserialize, Serialize};

use crate::common::{Capability, Complexity, FallbackPolicy, PrivacyClass};
use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TaskProfile {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub privacy: Option<PrivacyClass>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub complexity: Option<Complexity>,
    /// `minItems: 1` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<Vec<Capability>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fallback: Option<FallbackPolicy>,
}

impl Contract for TaskProfile {
    fn validate(&self) -> Result<(), ContractError> {
        if let Some(intent) = &self.intent
            && !non_empty(intent)
        {
            return Err(ContractError::new("task_profile.intent must not be empty"));
        }
        if let Some(capabilities) = &self.capabilities
            && capabilities.is_empty()
        {
            return Err(ContractError::new(
                "task_profile.capabilities must have at least one entry when present",
            ));
        }
        Ok(())
    }
}

impl SchemaShape for TaskProfile {
    const SCHEMA_FILE: &'static str = "task-profile.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "privacy",
        "intent",
        "complexity",
        "capabilities",
        "fallback",
    ];
    // task-profile.schema.json declares no top-level `required` array at all.
    const REQUIRED: &'static [&'static str] = &[];
}
