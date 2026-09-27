//! Enums whose value sets are shared verbatim by more than one
//! `packages/contracts/schema/*.schema.json` file (e.g. `capabilities` shows
//! up in `provider`, `task-profile` and `routing-policy`). Each schema still
//! declares its own inline `enum: [...]`, so if one of them drifts from the
//! others the parity corpus in `tests/parity.rs` is what catches it — this
//! module just avoids five copies of the same Rust enum.

use serde::{Deserialize, Serialize};

/// `06 §10.8` capability tags.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Chat,
    Reasoning,
    Vision,
    Asr,
    Tts,
    Coding,
    Embedding,
    Rerank,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyClass {
    LocalOnly,
    Any,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Local,
    Remote,
    Lora,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Complexity {
    Simple,
    Complex,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackPolicy {
    FailClosed,
    NextInChain,
}
