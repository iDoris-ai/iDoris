//! `packages/contracts/schema/tenant.schema.json` (contract-tenancy §4).
//!
//! `billing_timezone` validity is not fully ported here — see
//! [`is_iana_time_zone`] doc comment for why — everything else (structural
//! shape, `budget.scope` default, non-blank `tenant_id`) matches
//! `packages/contracts/src/tenant.ts` exactly.

use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BudgetScope {
    PaidOnly,
    All,
}

fn default_budget_scope() -> BudgetScope {
    BudgetScope::PaidOnly
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub limit_minor: i64,
    pub spent_minor: i64,
    #[serde(default = "default_budget_scope")]
    pub scope: BudgetScope,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Quota {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantContext {
    pub tenant_id: String,
    pub budget: Budget,
    pub billing_timezone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota: Option<Quota>,
}

/// A small, explicitly-not-exhaustive allow-list of canonical IANA zone
/// names, covering the handful exercised by `tests/parity.rs`.
///
/// `packages/contracts/src/tenant.ts` builds its whitelist at runtime from
/// `Intl.supportedValuesOf("timeZone")`, which enumerates every canonical
/// name ICU ships and deliberately excludes legacy/backward aliases
/// (`EST5EDT`, `GMT0`, ...). Rust has no equivalent built in, and pulling in
/// a full tzdata crate (e.g. `chrono-tz`) would *not* reproduce that
/// distinction either — `chrono-tz` includes the legacy aliases the TS side
/// rejects. Porting this faithfully needs either a curated canonical-name
/// data file or a tzdata crate filtered against one, which is business logic
/// for the tenancy migration task, not this skeleton — see
/// `crates/idoris-tenancy/README.md`.
const KNOWN_IANA_TIME_ZONES: &[&str] = &[
    "UTC",
    "Asia/Bangkok",
    "Asia/Shanghai",
    "Asia/Tokyo",
    "Asia/Singapore",
    "Europe/London",
    "Europe/Berlin",
    "America/New_York",
    "America/Los_Angeles",
    "Australia/Sydney",
];

/// See [`KNOWN_IANA_TIME_ZONES`] for the scope of what this actually checks.
pub fn is_iana_time_zone(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    // Reject numeric offsets up front (ASCII +/- and U+2212 MINUS SIGN),
    // same as the TS implementation.
    if value.starts_with('+') || value.starts_with('-') || value.starts_with('\u{2212}') {
        return false;
    }
    KNOWN_IANA_TIME_ZONES.contains(&value)
}

impl Contract for TenantContext {
    fn validate(&self) -> Result<(), ContractError> {
        if self.tenant_id.trim().is_empty() {
            return Err(ContractError::new("tenant.tenant_id must not be blank"));
        }
        if self.budget.limit_minor < 0 {
            return Err(ContractError::new("tenant.budget.limit_minor must be >= 0"));
        }
        if self.budget.spent_minor < 0 {
            return Err(ContractError::new("tenant.budget.spent_minor must be >= 0"));
        }
        if !non_empty(&self.billing_timezone) || !is_iana_time_zone(&self.billing_timezone) {
            return Err(ContractError::new(
                "tenant.billing_timezone must be an explicit IANA time zone name",
            ));
        }
        if let Some(quota) = &self.quota {
            if quota.rpm.is_some_and(|v| v < 1) {
                return Err(ContractError::new("tenant.quota.rpm must be >= 1"));
            }
            if quota.tpm.is_some_and(|v| v < 1) {
                return Err(ContractError::new("tenant.quota.tpm must be >= 1"));
            }
        }
        Ok(())
    }
}

impl SchemaShape for TenantContext {
    const SCHEMA_FILE: &'static str = "tenant.schema.json";
    const PROPERTIES: &'static [&'static str] =
        &["tenant_id", "budget", "billing_timezone", "quota"];
    const REQUIRED: &'static [&'static str] = &["tenant_id", "budget", "billing_timezone"];
}
