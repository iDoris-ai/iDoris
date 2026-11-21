//! Admin API v0 response contracts used by the Agent24 M4 status card, plus the `upstreams` family
//! (IDORIS-CORE S6, interface spec §2.11.4: a read and the three restricted writes `start|stop|restart`,
//! whose `202` body is one [`AdminUpstream`]).

use serde::{Deserialize, Serialize};

use crate::error::{Contract, ContractError, non_empty};
use crate::shape::SchemaShape;
use crate::{component_card::Form, provider::Locality};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminBackend {
    pub provider_id: String,
    pub locality: Locality,
    pub form: Form,
    pub lifecycle_runtime_bound: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdminBackendsResponse(pub Vec<AdminBackend>);

impl Contract for AdminBackendsResponse {
    fn validate(&self) -> Result<(), ContractError> {
        if self
            .0
            .iter()
            .any(|backend| !non_empty(&backend.provider_id))
        {
            return Err(ContractError::new(
                "admin backend provider_id must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRole {
    pub role: String,
    pub aliases: Vec<String>,
    pub catalog_role: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AdminRolesResponse(pub Vec<AdminRole>);

impl Contract for AdminRolesResponse {
    fn validate(&self) -> Result<(), ContractError> {
        if self.0.iter().any(|role| {
            !non_empty(&role.role)
                || role.aliases.is_empty()
                || role.aliases.iter().any(|alias| !non_empty(alias))
        }) {
            return Err(ContractError::new(
                "admin role names and aliases must not be empty",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRuntimeState {
    Observed,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminRuntimePressure {
    Ok,
    Soft,
    Hard,
    Ceiling,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRuntime {
    pub provider_id: String,
    pub state: AdminRuntimeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pressure: Option<AdminRuntimePressure>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_memory_max_gb: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loaded: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminRuntimesResponse {
    pub runtimes: Vec<AdminRuntime>,
}

impl Contract for AdminRuntimesResponse {
    fn validate(&self) -> Result<(), ContractError> {
        for runtime in &self.runtimes {
            if !non_empty(&runtime.provider_id) {
                return Err(ContractError::new(
                    "admin runtime provider_id must not be empty",
                ));
            }
            match runtime.state {
                AdminRuntimeState::Observed => {
                    let (Some(_), Some(used_gb), Some(max_gb), Some(loaded)) = (
                        runtime.pressure,
                        runtime.used_gb,
                        runtime.model_memory_max_gb,
                        runtime.loaded.as_ref(),
                    ) else {
                        return Err(ContractError::new(
                            "observed admin runtime must include all observed facts",
                        ));
                    };
                    if !used_gb.is_finite()
                        || used_gb < 0.0
                        || !max_gb.is_finite()
                        || max_gb < 0.0
                        || loaded.iter().any(|model| !non_empty(model))
                    {
                        return Err(ContractError::new(
                            "admin runtime facts must be finite/non-negative with non-empty model ids",
                        ));
                    }
                }
                AdminRuntimeState::Error => {
                    if runtime.pressure.is_some()
                        || runtime.used_gb.is_some()
                        || runtime.model_memory_max_gb.is_some()
                        || runtime.loaded.is_some()
                    {
                        return Err(ContractError::new(
                            "error admin runtime must not expose partial backend facts",
                        ));
                    }
                }
            }
        }
        Ok(())
    }
}

impl SchemaShape for AdminRuntimesResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-runtimes.schema.json";
    const PROPERTIES: &'static [&'static str] = &["runtimes"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceKind {
    HttpModelsEndpoint,
    SubscriptionRegistration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceState {
    Observed,
    Configured,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminModelSourceError {
    Unavailable,
    AuthenticationFailed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminModelSource {
    pub provider_id: String,
    pub source: AdminModelSourceKind,
    pub state: AdminModelSourceState,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<AdminModelSourceError>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminModelsResponse {
    pub sources: Vec<AdminModelSource>,
}

impl Contract for AdminModelsResponse {
    fn validate(&self) -> Result<(), ContractError> {
        for source in &self.sources {
            if !non_empty(&source.provider_id) || source.models.iter().any(|id| !non_empty(id)) {
                return Err(ContractError::new(
                    "admin model provider/model ids must not be empty",
                ));
            }
            match (source.source, source.state, source.error) {
                (
                    AdminModelSourceKind::HttpModelsEndpoint,
                    AdminModelSourceState::Observed,
                    None,
                ) => {}
                (
                    AdminModelSourceKind::SubscriptionRegistration,
                    AdminModelSourceState::Configured,
                    None,
                ) if !source.models.is_empty() => {}
                (
                    AdminModelSourceKind::HttpModelsEndpoint,
                    AdminModelSourceState::Error,
                    Some(_),
                ) if source.models.is_empty() => {}
                _ => {
                    return Err(ContractError::new(
                        "admin model source/state/error shape mismatch",
                    ));
                }
            }
        }
        Ok(())
    }
}

impl SchemaShape for AdminModelsResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-models.schema.json";
    const PROPERTIES: &'static [&'static str] = &["sources"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminCapacityState {
    Observed,
    Unavailable,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminAdmissionStatus {
    Ready,
    RequiresEviction,
    Blocked,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminCapacityEntry {
    pub id: String,
    pub capability: String,
    pub resident: bool,
    pub estimated_memory_gb: f64,
    pub ctx_limit: u64,
    pub queue_depth: u64,
    pub admission_status: AdminAdmissionStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminCapacitySnapshot {
    pub state: AdminCapacityState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entries: Option<Vec<AdminCapacityEntry>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminStatusResponse {
    pub status: String,
    pub service: String,
    pub version: String,
    pub contract_version: String,
    pub instance_id: String,
    pub components: usize,
    pub runtimes: usize,
    pub subscriptions: usize,
    pub budget_configured: bool,
    pub audit_configured: bool,
    pub capacity: AdminCapacitySnapshot,
}

impl Contract for AdminStatusResponse {
    fn validate(&self) -> Result<(), ContractError> {
        if self.status != "ok" || self.service != "idoris" {
            return Err(ContractError::new("admin status/service literal mismatch"));
        }
        if !non_empty(&self.version)
            || !non_empty(&self.contract_version)
            || !non_empty(&self.instance_id)
        {
            return Err(ContractError::new(
                "admin status identity fields must not be empty",
            ));
        }
        match (&self.capacity.state, &self.capacity.entries) {
            (AdminCapacityState::Observed, Some(entries)) => {
                if entries.iter().any(|entry| {
                    !entry.estimated_memory_gb.is_finite() || entry.estimated_memory_gb < 0.0
                }) {
                    return Err(ContractError::new(
                        "admin capacity memory must be finite and >= 0",
                    ));
                }
            }
            (AdminCapacityState::Unavailable | AdminCapacityState::Error, None) => {}
            _ => return Err(ContractError::new("admin capacity state/entries mismatch")),
        }
        Ok(())
    }
}

impl SchemaShape for AdminStatusResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-status.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "status",
        "service",
        "version",
        "contract_version",
        "instance_id",
        "components",
        "runtimes",
        "subscriptions",
        "budget_configured",
        "audit_configured",
        "capacity",
    ];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

/// `upstreams[].state` (interface spec §2.11.1). The seven values are frozen; the router maps its own
/// state enum onto this one with an exhaustive `match`, so a new state cannot be added without
/// touching the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminUpstreamState {
    Ready,
    Starting,
    Unavailable,
    Restarting,
    Failed,
    Stopped,
    Absent,
}

/// `upstreams[].reason` (interface spec §2.11.1): stable reason codes. Human-readable text is not part
/// of the contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdminUpstreamReason {
    EngineNotFound,
    PortInUse,
    AuthRequired,
    AuthFailed,
    StartupTimeout,
    CrashLoop,
    FencePending,
    BudgetZero,
    BaseUnavailable,
    StartupLatched,
}

impl AdminUpstreamState {
    /// The wire string (the same one serde writes), for logs and for the drift test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Starting => "starting",
            Self::Unavailable => "unavailable",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
            Self::Absent => "absent",
        }
    }
}

impl AdminUpstreamReason {
    /// The wire string (the same one serde writes), for logs and for the drift test.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EngineNotFound => "engine_not_found",
            Self::PortInUse => "port_in_use",
            Self::AuthRequired => "auth_required",
            Self::AuthFailed => "auth_failed",
            Self::StartupTimeout => "startup_timeout",
            Self::CrashLoop => "crash_loop",
            Self::FencePending => "fence_pending",
            Self::BudgetZero => "budget_zero",
            Self::BaseUnavailable => "base_unavailable",
            Self::StartupLatched => "startup_latched",
        }
    }
}

/// The schema's flat `enum` lists, in schema order: checked against the live schema file by
/// `tests/contract_drift.rs`, and against the variants themselves by this module's tests (a new variant
/// stops compiling there until it is listed).
pub const ADMIN_UPSTREAM_STATE_VALUES: &[AdminUpstreamState] = &[
    AdminUpstreamState::Ready,
    AdminUpstreamState::Starting,
    AdminUpstreamState::Unavailable,
    AdminUpstreamState::Restarting,
    AdminUpstreamState::Failed,
    AdminUpstreamState::Stopped,
    AdminUpstreamState::Absent,
];
pub const ADMIN_UPSTREAM_REASON_VALUES: &[AdminUpstreamReason] = &[
    AdminUpstreamReason::EngineNotFound,
    AdminUpstreamReason::PortInUse,
    AdminUpstreamReason::AuthRequired,
    AdminUpstreamReason::AuthFailed,
    AdminUpstreamReason::StartupTimeout,
    AdminUpstreamReason::CrashLoop,
    AdminUpstreamReason::FencePending,
    AdminUpstreamReason::BudgetZero,
    AdminUpstreamReason::BaseUnavailable,
    AdminUpstreamReason::StartupLatched,
];

/// A field that may be absent but, when present, must not be `null` (JSON Schema `enum`/`integer`
/// reject `null`, while `Option<T>` alone would quietly read it as absent).
fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

/// One element of `GET /admin/api/v1/upstreams`, and the `202` body of `POST
/// /admin/api/v1/upstreams/{id}/start|stop|restart` (interface spec §2.11.4).
///
/// The first six fields are exactly `/health.upstreams[]` (§2.11.1; same source, same wording). The
/// rest are Admin-only read facts, only ever added: `port` (the loopback port of the card's endpoint),
/// `pid` (only while `owned`), `restart_required`, and the managed-only failure facts. **No field can
/// carry a key, a token or a nonce**; `deny_unknown_fields` (and the schema's `additionalProperties:
/// false`) keep it that way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminUpstream {
    pub id: String,
    pub engine_kind: String,
    pub state: AdminUpstreamState,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub reason: Option<AdminUpstreamReason>,
    pub managed: bool,
    pub owned: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub port: Option<u16>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub pid: Option<u32>,
    pub restart_required: bool,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub failure_count: Option<u32>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present"
    )]
    pub last_failure_at_ms: Option<u64>,
}

/// The largest integer every consumer reads exactly: 2^53 - 1, JavaScript's `Number.MAX_SAFE_INTEGER`. The
/// schema's `maximum` for `last_failure_at_ms` (a `u64` here), so a value the generated Zod/JS side would
/// round is rejected by Rust, the schema and Zod alike (`pid` and `failure_count` are `u32`: their
/// `maximum` is `4294967295`, which serde already enforces on this side).
pub const JS_MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

/// `^[a-z][a-z0-9_]{0,31}$` (interface spec §2.12.1): the engine kind grammar.
fn is_engine_kind(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next().is_some_and(|first| first.is_ascii_lowercase())
        && value.len() <= 32
        && bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
}

impl Contract for AdminUpstream {
    fn validate(&self) -> Result<(), ContractError> {
        if !non_empty(&self.id) {
            return Err(ContractError::new("admin upstream id must not be empty"));
        }
        if !is_engine_kind(&self.engine_kind) {
            return Err(ContractError::new(
                "admin upstream engine_kind must match ^[a-z][a-z0-9_]{0,31}$",
            ));
        }
        if self.port == Some(0) {
            return Err(ContractError::new("admin upstream port must be 1..=65535"));
        }
        if self.pid == Some(0) {
            return Err(ContractError::new("admin upstream pid must be >= 1"));
        }
        if self
            .last_failure_at_ms
            .is_some_and(|millis| millis > JS_MAX_SAFE_INTEGER)
        {
            return Err(ContractError::new(
                "admin upstream last_failure_at_ms must be <= 9007199254740991 (2^53 - 1)",
            ));
        }
        Ok(())
    }
}

impl SchemaShape for AdminUpstream {
    const SCHEMA_FILE: &'static str = "admin-v0-upstream.schema.json";
    const PROPERTIES: &'static [&'static str] = &[
        "id",
        "engine_kind",
        "state",
        "reason",
        "managed",
        "owned",
        "port",
        "pid",
        "restart_required",
        "failure_count",
        "last_failure_at_ms",
    ];
    const REQUIRED: &'static [&'static str] = &[
        "id",
        "engine_kind",
        "state",
        "managed",
        "owned",
        "restart_required",
    ];
}

/// `GET /admin/api/v1/upstreams` (interface spec §2.11.4).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminUpstreamsResponse {
    pub upstreams: Vec<AdminUpstream>,
}

impl Contract for AdminUpstreamsResponse {
    fn validate(&self) -> Result<(), ContractError> {
        self.upstreams.iter().try_for_each(Contract::validate)
    }
}

impl SchemaShape for AdminUpstreamsResponse {
    const SCHEMA_FILE: &'static str = "admin-v0-upstreams.schema.json";
    const PROPERTIES: &'static [&'static str] = &["upstreams"];
    const REQUIRED: &'static [&'static str] = Self::PROPERTIES;
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    // An exhaustive `match` on purpose: a new variant stops compiling here until the matching value list
    // (and the schema's `enum`, which `tests/contract_drift.rs` compares) is updated too.
    fn state_position(state: AdminUpstreamState) -> usize {
        match state {
            AdminUpstreamState::Ready => 0,
            AdminUpstreamState::Starting => 1,
            AdminUpstreamState::Unavailable => 2,
            AdminUpstreamState::Restarting => 3,
            AdminUpstreamState::Failed => 4,
            AdminUpstreamState::Stopped => 5,
            AdminUpstreamState::Absent => 6,
        }
    }

    fn reason_position(reason: AdminUpstreamReason) -> usize {
        match reason {
            AdminUpstreamReason::EngineNotFound => 0,
            AdminUpstreamReason::PortInUse => 1,
            AdminUpstreamReason::AuthRequired => 2,
            AdminUpstreamReason::AuthFailed => 3,
            AdminUpstreamReason::StartupTimeout => 4,
            AdminUpstreamReason::CrashLoop => 5,
            AdminUpstreamReason::FencePending => 6,
            AdminUpstreamReason::BudgetZero => 7,
            AdminUpstreamReason::BaseUnavailable => 8,
            AdminUpstreamReason::StartupLatched => 9,
        }
    }

    #[test]
    fn the_value_lists_are_every_variant_in_wire_order() {
        for (position, state) in ADMIN_UPSTREAM_STATE_VALUES.iter().enumerate() {
            assert_eq!(state_position(*state), position);
        }
        assert_eq!(ADMIN_UPSTREAM_STATE_VALUES.len(), 7);
        for (position, reason) in ADMIN_UPSTREAM_REASON_VALUES.iter().enumerate() {
            assert_eq!(reason_position(*reason), position);
        }
        assert_eq!(ADMIN_UPSTREAM_REASON_VALUES.len(), 10);
    }

    #[test]
    fn as_str_is_exactly_the_string_serde_writes() {
        for state in ADMIN_UPSTREAM_STATE_VALUES {
            assert_eq!(
                serde_json::to_value(state).unwrap(),
                serde_json::json!(state.as_str())
            );
        }
        for reason in ADMIN_UPSTREAM_REASON_VALUES {
            assert_eq!(
                serde_json::to_value(reason).unwrap(),
                serde_json::json!(reason.as_str())
            );
        }
    }

    #[test]
    fn engine_kind_grammar_is_lowercase_start_then_lowercase_digits_underscore_up_to_32() {
        for good in ["omlx", "llama_cpp", "openai_compat", "unknown", "a", "a1_"] {
            assert!(is_engine_kind(good), "{good}");
        }
        assert!(is_engine_kind(&"a".repeat(32)));
        for bad in [
            "", "1omlx", "_omlx", "OMLX", "omlx-x", "omlx x", "ömlx", "a.b",
        ] {
            assert!(!is_engine_kind(bad), "{bad}");
        }
        assert!(!is_engine_kind(&"a".repeat(33)));
    }
}
