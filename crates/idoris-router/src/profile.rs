//! Control-plane header parsing: builds a [`TaskProfile`] + tenant id from
//! `X-iDoris-*` headers, mirroring `packages/router/src/profile.ts`'s
//! header→`TaskProfile` mapping (T1.3.2) — **privacy defaults to
//! `local_only`, non-`personal` deploy modes require `X-iDoris-Tenant`, no
//! default tenant fallback**. `idoris/<role>` model resolution (T4.2,
//! Rust-only, no TS equivalent) is a separate follow-up PR on top of this.
//!
//! Order matters (locked by the conformance suite's `顺序锁定` cases plus
//! `profile.ts`'s own precedence): callers must run [`parse_profile`] only
//! *after* the request body has been confirmed to be a JSON object —
//! `invalid_json`/`invalid_body` outrank every header-derived 400 here.

use axum::http::{HeaderMap, StatusCode};
use idoris_contracts::Contract;
use idoris_contracts::DeployMode;
use idoris_contracts::common::{Capability, Complexity, FallbackPolicy, PrivacyClass};
use idoris_contracts::task_profile::TaskProfile;

/// One control-plane parse failure: a fixed HTTP status plus the unified
/// error envelope's `type` (interface spec §3.11).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileError {
    pub status: StatusCode,
    pub error_type: &'static str,
    pub message: String,
}

impl ProfileError {
    fn new(status: StatusCode, error_type: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            error_type,
            message: message.into(),
        }
    }
}

fn invalid_header(message: impl Into<String>) -> ProfileError {
    ProfileError::new(StatusCode::BAD_REQUEST, "invalid_header", message)
}

/// Parsed control-plane profile: task metadata + optional tenant id.
#[derive(Debug, Clone)]
pub struct ParsedProfile {
    pub task: TaskProfile,
    pub tenant_id: Option<String>,
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?.trim().to_string();
    (!raw.is_empty()).then_some(raw)
}

/// `IDORIS_DEPLOY_MODE` → tenant-mode gate, mirroring
/// `isPersonalDeployMode`/`currentDeployMode`: empty or `personal`
/// (case-insensitive, trimmed) is personal; **every other value —
/// `tenant`/`community`/`city`/anything unrecognized — collapses to tenant
/// mode**. Fail-closed on purpose: an unknown value must never be silently
/// treated as personal, since that would make a misconfigured tenant
/// deployment skip the `X-iDoris-Tenant` requirement entirely.
pub fn deploy_mode_from_env(raw: Option<&str>) -> DeployMode {
    let normalized = raw.unwrap_or("").trim().to_lowercase();
    if normalized.is_empty() || normalized == "personal" {
        DeployMode::Personal
    } else {
        DeployMode::Tenant
    }
}

fn parse_privacy(headers: &HeaderMap) -> Result<PrivacyClass, ProfileError> {
    match header_str(headers, "x-idoris-privacy").as_deref() {
        None => Ok(PrivacyClass::LocalOnly),
        Some("local_only") => Ok(PrivacyClass::LocalOnly),
        Some("any") => Ok(PrivacyClass::Any),
        Some(_) => Err(ProfileError::new(
            StatusCode::BAD_REQUEST,
            "invalid_privacy",
            "X-iDoris-Privacy must be local_only|any",
        )),
    }
}

fn parse_complexity(headers: &HeaderMap) -> Result<Complexity, ProfileError> {
    match header_str(headers, "x-idoris-complexity").as_deref() {
        None => Ok(Complexity::Simple),
        Some("simple") => Ok(Complexity::Simple),
        Some("complex") => Ok(Complexity::Complex),
        Some(_) => Err(invalid_header("X-iDoris-Complexity must be simple|complex")),
    }
}

fn parse_capability(raw: &str) -> Option<Capability> {
    Some(match raw {
        "chat" => Capability::Chat,
        "reasoning" => Capability::Reasoning,
        "vision" => Capability::Vision,
        "asr" => Capability::Asr,
        "tts" => Capability::Tts,
        "coding" => Capability::Coding,
        "embedding" => Capability::Embedding,
        "rerank" => Capability::Rerank,
        _ => return None,
    })
}

fn parse_capabilities(headers: &HeaderMap) -> Result<Vec<Capability>, ProfileError> {
    match header_str(headers, "x-idoris-capabilities") {
        None => Ok(vec![Capability::Chat]),
        Some(raw) => {
            let mut out = Vec::new();
            for part in raw.split(',') {
                let part = part.trim();
                if part.is_empty() {
                    continue;
                }
                match parse_capability(part) {
                    Some(cap) => out.push(cap),
                    None => return Err(invalid_header(format!("unknown capability {part:?}"))),
                }
            }
            if out.is_empty() {
                return Err(invalid_header(
                    "X-iDoris-Capabilities must not resolve to an empty list",
                ));
            }
            Ok(out)
        }
    }
}

fn parse_fallback(headers: &HeaderMap) -> Result<Option<FallbackPolicy>, ProfileError> {
    match header_str(headers, "x-idoris-fallback").as_deref() {
        None => Ok(None),
        Some("fail_closed") => Ok(Some(FallbackPolicy::FailClosed)),
        Some("next_in_chain") => Ok(Some(FallbackPolicy::NextInChain)),
        Some(_) => Err(invalid_header(
            "X-iDoris-Fallback must be fail_closed|next_in_chain",
        )),
    }
}

/// Builds the control-plane profile from headers. **Callers must have
/// already rejected invalid JSON / a non-object body** — that ordering is
/// enforced by the caller, not here. Internal order: privacy →
/// complexity/capabilities/fallback → tenant (`profile.ts`'s own
/// precedence).
pub fn parse_profile(
    headers: &HeaderMap,
    deploy_mode: DeployMode,
) -> Result<ParsedProfile, ProfileError> {
    let privacy = parse_privacy(headers)?;
    let intent = header_str(headers, "x-idoris-intent").unwrap_or_else(|| "chat".to_string());
    let complexity = parse_complexity(headers)?;
    let capabilities = parse_capabilities(headers)?;
    let fallback = parse_fallback(headers)?;

    let task = TaskProfile {
        privacy: Some(privacy),
        intent: Some(intent),
        complexity: Some(complexity),
        capabilities: Some(capabilities),
        fallback,
    };
    task.validate()
        .map_err(|err| invalid_header(err.to_string()))?;

    let tenant_id = match deploy_mode {
        DeployMode::Personal => None,
        DeployMode::Tenant => {
            let tenant = header_str(headers, "x-idoris-tenant").ok_or_else(|| {
                ProfileError::new(
                    StatusCode::BAD_REQUEST,
                    "tenant_missing",
                    "deploy_mode=tenant requires X-iDoris-Tenant; no default tenant fallback",
                )
            })?;
            Some(tenant)
        }
    };

    Ok(ParsedProfile { task, tenant_id })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use axum::http::HeaderMap;

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                axum::http::HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    #[test]
    fn defaults_privacy_to_local_only_when_header_absent() {
        let parsed = parse_profile(&headers(&[]), DeployMode::Personal).unwrap();
        assert_eq!(parsed.task.privacy, Some(PrivacyClass::LocalOnly));
        assert_eq!(parsed.task.intent, Some("chat".to_string()));
        assert_eq!(parsed.task.complexity, Some(Complexity::Simple));
        assert_eq!(parsed.task.capabilities, Some(vec![Capability::Chat]));
        assert_eq!(parsed.task.fallback, None);
        assert_eq!(parsed.tenant_id, None);
    }

    #[test]
    fn rejects_invalid_privacy() {
        let err = parse_profile(
            &headers(&[("x-idoris-privacy", "bogus")]),
            DeployMode::Personal,
        )
        .unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error_type, "invalid_privacy");
    }

    #[test]
    fn accepts_any_privacy() {
        let parsed = parse_profile(
            &headers(&[("x-idoris-privacy", "any")]),
            DeployMode::Personal,
        )
        .unwrap();
        assert_eq!(parsed.task.privacy, Some(PrivacyClass::Any));
    }

    #[test]
    fn rejects_invalid_complexity_capabilities_and_fallback() {
        for (header, value) in [
            ("x-idoris-complexity", "ultra"),
            ("x-idoris-capabilities", "chat,teleport"),
            ("x-idoris-fallback", "yolo"),
        ] {
            let err = parse_profile(&headers(&[(header, value)]), DeployMode::Personal)
                .expect_err(&format!("{header}={value} must be rejected"));
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
            assert_eq!(err.error_type, "invalid_header");
        }
    }

    #[test]
    fn accepts_legal_complexity_capabilities_and_fallback() {
        let parsed = parse_profile(
            &headers(&[
                ("x-idoris-complexity", "complex"),
                ("x-idoris-capabilities", "chat, coding"),
                ("x-idoris-fallback", "next_in_chain"),
            ]),
            DeployMode::Personal,
        )
        .unwrap();
        assert_eq!(parsed.task.complexity, Some(Complexity::Complex));
        assert_eq!(
            parsed.task.capabilities,
            Some(vec![Capability::Chat, Capability::Coding])
        );
        assert_eq!(parsed.task.fallback, Some(FallbackPolicy::NextInChain));
    }

    #[test]
    fn accepts_any_non_empty_intent_without_enum_validation() {
        let parsed = parse_profile(
            &headers(&[("x-idoris-intent", "totally-made-up-intent")]),
            DeployMode::Personal,
        )
        .unwrap();
        assert_eq!(
            parsed.task.intent,
            Some("totally-made-up-intent".to_string())
        );
    }

    #[test]
    fn tenant_mode_requires_tenant_header() {
        let err = parse_profile(&headers(&[]), DeployMode::Tenant).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error_type, "tenant_missing");
    }

    #[test]
    fn tenant_mode_passes_through_the_declared_tenant() {
        let parsed =
            parse_profile(&headers(&[("x-idoris-tenant", "acme")]), DeployMode::Tenant).unwrap();
        assert_eq!(parsed.tenant_id, Some("acme".to_string()));
    }
}
