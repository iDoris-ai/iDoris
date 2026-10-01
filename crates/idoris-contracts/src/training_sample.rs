//! `packages/contracts/schema/training-sample.schema.json`.

use serde::{Deserialize, Serialize};

use crate::adapter_manifest::DataClass;
use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub role: Role,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceKind {
    SyntheticSeed,
    RefinedFromSynthetic,
    HumanReviewed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub kind: SourceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingSample {
    pub sample_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    pub data_class: DataClass,
    pub source: Source,
    /// `minItems: 2`.
    pub messages: Vec<Message>,
}

impl Contract for TrainingSample {
    fn validate(&self) -> Result<(), ContractError> {
        if !non_empty(&self.sample_id) {
            return Err(ContractError::new(
                "training_sample.sample_id must not be empty",
            ));
        }
        if let Some(group_id) = &self.group_id
            && !non_empty(group_id)
        {
            return Err(ContractError::new(
                "training_sample.group_id must not be empty when present",
            ));
        }
        if self.messages.len() < 2 {
            return Err(ContractError::new(
                "training_sample.messages must have at least 2 entries",
            ));
        }
        for (i, message) in self.messages.iter().enumerate() {
            if !non_empty(&message.content) {
                return Err(ContractError::new(format!(
                    "training_sample.messages[{i}].content must not be empty"
                )));
            }
        }
        if let Some(generator) = &self.source.generator
            && !non_empty(generator)
        {
            return Err(ContractError::new(
                "training_sample.source.generator must not be empty when present",
            ));
        }
        if let Some(intent) = &self.source.intent
            && !non_empty(intent)
        {
            return Err(ContractError::new(
                "training_sample.source.intent must not be empty when present",
            ));
        }
        Ok(())
    }
}

impl SchemaShape for TrainingSample {
    const SCHEMA_FILE: &'static str = "training-sample.schema.json";
    const PROPERTIES: &'static [&'static str] =
        &["sample_id", "group_id", "data_class", "source", "messages"];
    const REQUIRED: &'static [&'static str] = &["sample_id", "data_class", "source", "messages"];
}
