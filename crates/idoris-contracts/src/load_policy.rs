//! `packages/contracts/schema/load-policy.schema.json` (06 §10.3):
//! engine-agnostic `LoadPolicy` / `ModelLease` abstraction.

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LoadMode {
    Resident,
    OnDemand,
    EvictToLoad,
}

/// Note: this is a *separate* Rust type from `idoris_backend::Admission`,
/// deliberately — the TS side has the same duplication: `LoadPolicy.admission`
/// (`@idoris/contracts`) and `ModelBackend`'s `Admission` (`backend.ts`) are
/// two independent string-union declarations that happen to share the same
/// two values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Admission {
    Coexist,
    RequiresEviction,
}

/// `keepalive` is a JSON Schema `oneOf` between `{pinned: bool}` and
/// `{idle_ttl_s: integer >= 1}`, each with `additionalProperties: false`.
/// serde's derive macros don't express "exactly one of these two shapes, and
/// reject a value that satisfies neither or both" cleanly for an untagged
/// enum with `deny_unknown_fields`, so this has a hand-written
/// [`Deserialize`] impl instead.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(untagged)]
pub enum Keepalive {
    Pinned { pinned: bool },
    IdleTtl { idle_ttl_s: u32 },
}

impl<'de> Deserialize<'de> for Keepalive {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let obj = value
            .as_object()
            .ok_or_else(|| de::Error::custom("keepalive must be a JSON object"))?;
        let pinned = obj.get("pinned");
        let idle_ttl_s = obj.get("idle_ttl_s");
        match (pinned, idle_ttl_s, obj.len()) {
            (Some(p), None, 1) => {
                let pinned = p
                    .as_bool()
                    .ok_or_else(|| de::Error::custom("keepalive.pinned must be a boolean"))?;
                Ok(Keepalive::Pinned { pinned })
            }
            (None, Some(t), 1) => {
                let n = t
                    .as_u64()
                    .ok_or_else(|| de::Error::custom("keepalive.idle_ttl_s must be an integer"))?;
                if n < 1 {
                    return Err(de::Error::custom("keepalive.idle_ttl_s must be >= 1"));
                }
                let n = u32::try_from(n)
                    .map_err(|_| de::Error::custom("keepalive.idle_ttl_s is out of range"))?;
                Ok(Keepalive::IdleTtl { idle_ttl_s: n })
            }
            // Neither key present, both present, or an unknown extra key —
            // in every case the object matches neither branch of the
            // `oneOf`, exactly like the TS zod union rejecting it.
            _ => Err(de::Error::custom(
                "keepalive must have exactly one of `pinned` or `idle_ttl_s`, no other keys",
            )),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoadPolicy {
    pub mode: LoadMode,
    pub keepalive: Keepalive,
    pub admission: Admission,
}

impl Contract for LoadPolicy {
    fn validate(&self) -> Result<(), ContractError> {
        // All of load-policy's constraints (oneOf keepalive shape, idle_ttl_s
        // minimum) are already enforced during deserialize.
        Ok(())
    }
}

impl SchemaShape for LoadPolicy {
    const SCHEMA_FILE: &'static str = "load-policy.schema.json";
    const PROPERTIES: &'static [&'static str] = &["mode", "keepalive", "admission"];
    const REQUIRED: &'static [&'static str] = &["mode", "keepalive", "admission"];
}
