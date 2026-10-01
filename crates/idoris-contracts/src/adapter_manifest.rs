//! `packages/contracts/schema/adapter-manifest.schema.json` (06 §10.4):
//! LoRA/adapter provenance, pinned to an exact base model + tokenizer digest
//! so a swapped base can never silently apply an incompatible adapter.

use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataClass {
    Synthetic,
    Anonymized,
    Real,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdapterManifest {
    pub adapter_id: String,
    pub base_model_id: String,
    pub base_digest: String,
    pub tokenizer_digest: String,
    /// `integer`, `minimum: 1`, `maximum: 256`.
    pub rank: i64,
    pub data_class: DataClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,
    /// `minItems: 1` when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_modules: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<serde_json::Map<String, serde_json::Value>>,
}

/// `^sha256:[a-f0-9]{64}$`.
pub fn is_sha256_digest(value: &str) -> bool {
    match value.strip_prefix("sha256:") {
        Some(hex) => {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }
        None => false,
    }
}

impl Contract for AdapterManifest {
    fn validate(&self) -> Result<(), ContractError> {
        if !non_empty(&self.adapter_id) {
            return Err(ContractError::new(
                "adapter_manifest.adapter_id must not be empty",
            ));
        }
        if !non_empty(&self.base_model_id) {
            return Err(ContractError::new(
                "adapter_manifest.base_model_id must not be empty",
            ));
        }
        if !is_sha256_digest(&self.base_digest) {
            return Err(ContractError::new(
                "adapter_manifest.base_digest must match ^sha256:[a-f0-9]{64}$",
            ));
        }
        if !is_sha256_digest(&self.tokenizer_digest) {
            return Err(ContractError::new(
                "adapter_manifest.tokenizer_digest must match ^sha256:[a-f0-9]{64}$",
            ));
        }
        if !(1..=256).contains(&self.rank) {
            return Err(ContractError::new(
                "adapter_manifest.rank must be between 1 and 256",
            ));
        }
        if let Some(target_modules) = &self.target_modules {
            if target_modules.is_empty() {
                return Err(ContractError::new(
                    "adapter_manifest.target_modules must not be empty when present",
                ));
            }
            if target_modules.iter().any(|m| !non_empty(m)) {
                return Err(ContractError::new(
                    "adapter_manifest.target_modules entries must not be empty",
                ));
            }
        }
        Ok(())
    }
}

impl SchemaShape for AdapterManifest {
    const SCHEMA_FILE: &'static str = "adapter-manifest.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "adapter_id",
        "base_model_id",
        "base_digest",
        "tokenizer_digest",
        "rank",
        "data_class",
        "created_at",
        "target_modules",
        "metrics",
    ];
    const REQUIRED: &'static [&'static str] = &[
        "adapter_id",
        "base_model_id",
        "base_digest",
        "tokenizer_digest",
        "rank",
        "data_class",
    ];
}
