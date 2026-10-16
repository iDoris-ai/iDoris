//! `POST /v1/inspect` wire contract (M5-01, gate one `INSPECTED`).
//!
//! The endpoint only classifies; it never rewrites the input. Its response is
//! deliberately content-free: every string in it is a closed enum value and a
//! finding locates a hit only by numeric offsets, so neither a response body
//! nor a log line built from one can carry the matched substring or a secret.
//!
//! Span offsets count **UTF-16 code units** into [`InspectRequest::input`],
//! half-open `[start, start + length)`. UTF-16 is what the TS side's
//! `string.length`/`slice` use, so both implementations address the same
//! characters for non-BMP input (emoji, rare CJK) without a conversion table.
//! A span is `start`+`length` rather than `start`+`end` so every rule is
//! expressible in the JSON Schema itself and Rust/TS cannot disagree on an
//! inverted range.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};

use crate::error::{Contract, ContractError};
use crate::shape::SchemaShape;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectRequest {
    pub input: String,
}

/// Never prints the input itself: a request that ends up in a log or panic
/// message must not leak the content it was asking about.
impl fmt::Debug for InspectRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InspectRequest")
            .field("input_bytes", &self.input.len())
            .finish_non_exhaustive()
    }
}

impl Contract for InspectRequest {
    fn validate(&self) -> Result<(), ContractError> {
        Ok(())
    }
}

impl SchemaShape for InspectRequest {
    const SCHEMA_FILE: &'static str = "inspect-request.schema.json";
    const PROPERTIES: &'static [&'static str] = &["input"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

/// S0 public / S1 internal / S2 personal / S3 special category (health,
/// finance, identity documents, credentials, minors). Ordered so the
/// inspector can only ever tighten with `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Sensitivity {
    S0,
    S1,
    S2,
    S3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum InspectAction {
    Block,
    Redact,
    Allow,
}

/// Closed set of L0 finding kinds (docs/15 §1.2 and §1.7). A new kind is a
/// contract change, never a free-form string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingKind {
    CnResidentId,
    CnMobile,
    BankCard,
    CnUnifiedSocialCreditCode,
    CnLicensePlate,
    CnPassport,
    HkIdentityCard,
    HkMacauTravelPermit,
    EmailAddress,
    ApiKey,
    PrivateKey,
    HighEntropySecret,
    CredentialPath,
    /// The input exceeded the inspector's limit and was not scanned at all
    /// (docs/15 X-1: refuse, never scan a prefix and allow). Carries no span.
    InputLimitExceeded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectSpan {
    pub start: u64,
    pub length: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectFinding {
    pub kind: FindingKind,
    #[serde(
        default,
        deserialize_with = "present_span",
        skip_serializing_if = "Option::is_none"
    )]
    pub span: Option<InspectSpan>,
}

/// `span` is optional but never `null`: the schema only allows it absent or
/// an object, so an explicit `null` must fail here too.
fn present_span<'de, D: Deserializer<'de>>(de: D) -> Result<Option<InspectSpan>, D::Error> {
    InspectSpan::deserialize(de).map(Some)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InspectResponse {
    pub sensitivity: Sensitivity,
    pub findings: Vec<InspectFinding>,
    pub action: InspectAction,
}

impl Contract for InspectResponse {
    fn validate(&self) -> Result<(), ContractError> {
        for finding in &self.findings {
            if finding.kind == FindingKind::InputLimitExceeded {
                if finding.span.is_some() {
                    return Err(ContractError::new("unscanned input must not carry a span"));
                }
                if self.action != InspectAction::Block {
                    return Err(ContractError::new("unscanned input requires BLOCK"));
                }
            }
        }
        let empty_span = self
            .findings
            .iter()
            .filter_map(|finding| finding.span)
            .any(|span| span.length == 0);
        if empty_span {
            return Err(ContractError::new("inspect span length must be >= 1"));
        }
        Ok(())
    }
}

impl SchemaShape for InspectResponse {
    const SCHEMA_FILE: &'static str = "inspect-response.schema.json";
    const PROPERTIES: &'static [&'static str] = &["sensitivity", "findings", "action"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

/// Wire values of [`FindingKind`], pinned against the schema enum by
/// `tests/contract_drift.rs`.
pub const FINDING_KIND_VALUES: &[&str] = &[
    "cn_resident_id",
    "cn_mobile",
    "bank_card",
    "cn_unified_social_credit_code",
    "cn_license_plate",
    "cn_passport",
    "hk_identity_card",
    "hk_macau_travel_permit",
    "email_address",
    "api_key",
    "private_key",
    "high_entropy_secret",
    "credential_path",
    "input_limit_exceeded",
];

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn request_debug_does_not_print_input() {
        let request = InspectRequest {
            input: "sk-sentinel-4f1c9a".to_string(),
        };
        let printed = format!("{request:?}");
        assert!(!printed.contains("sentinel"), "{printed}");
        assert!(printed.contains("input_bytes: 18"), "{printed}");
    }

    #[test]
    fn finding_kind_values_round_trip() {
        for value in FINDING_KIND_VALUES {
            let kind: FindingKind = serde_json::from_value(serde_json::json!(value)).unwrap();
            assert_eq!(
                serde_json::to_value(kind).unwrap(),
                serde_json::json!(value)
            );
        }
    }

    #[test]
    fn sensitivity_orders_from_public_to_special_category() {
        assert!(Sensitivity::S0 < Sensitivity::S1);
        assert!(Sensitivity::S2 < Sensitivity::S3);
        assert_eq!(Sensitivity::S1.max(Sensitivity::S3), Sensitivity::S3);
    }

    #[test]
    fn spanless_finding_omits_span_on_the_wire() {
        let finding = InspectFinding {
            kind: FindingKind::InputLimitExceeded,
            span: None,
        };
        assert_eq!(
            serde_json::to_value(finding).unwrap(),
            serde_json::json!({"kind": "input_limit_exceeded"})
        );
    }
}
