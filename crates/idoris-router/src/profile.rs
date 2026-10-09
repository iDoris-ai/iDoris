//! Control-plane header parsing: builds a [`TaskProfile`] + tenant id from
//! `X-iDoris-*` headers, mirroring `packages/router/src/profile.ts`'s
//! header→`TaskProfile` mapping (T1.3.2) — **privacy defaults to
//! `local_only`, non-`personal` deploy modes require `X-iDoris-Tenant`, no
//! default tenant fallback** — plus the Rust-only `idoris/<role>` model
//! resolution (T4.2); `profile.ts` has no equivalent since role-based
//! routing hasn't landed on the TS side.
//!
//! Order matters (locked by the conformance suite's `顺序锁定` cases plus
//! `profile.ts`'s own precedence): callers must run [`parse_profile`] only
//! *after* the request body has been confirmed to be a JSON object —
//! `invalid_json`/`invalid_body` outrank every header-derived 400 here.
//!
//! **A header that is present but unusable is a 400, never a silent
//! default** (prdaemon review on PR #125): a header missing entirely is
//! one thing (default applies), but a header the caller *did* send with a
//! non-UTF-8-visible-ASCII value, a blank/whitespace-only value, or more
//! than one value for the same name, is a client bug that must be
//! surfaced, not quietly downgraded (e.g. `Complexity: <mangled>` silently
//! becoming `simple`) or dropped (e.g. a garbled `Intent` silently
//! becoming `"chat"`). See `header_state`.

use axum::http::{HeaderMap, StatusCode};
use idoris_contracts::Contract;
use idoris_contracts::DeployMode;
use idoris_contracts::common::{Capability, Complexity, FallbackPolicy, PrivacyClass};
use idoris_contracts::task_profile::TaskProfile;
use idoris_policy::{Role, RoleParseError, parse_model_role};

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

/// Whether `X-iDoris-Intent` came from the request or the default —
/// mirrors `profile.ts`'s `ProfileParseResult.intentSource` ("显式声明永远
/// 优先于识别结果").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentSource {
    Header,
    Detected,
    Default,
}

/// Parsed control-plane profile: task metadata + optional resolved role +
/// optional tenant id.
#[derive(Debug, Clone)]
pub struct ParsedProfile {
    pub task: TaskProfile,
    pub role: Option<Role>,
    pub tenant_id: Option<String>,
    pub intent_source: IntentSource,
}

/// One header's state: outright absent, present but unusable, or present
/// with a genuine (trimmed, non-empty) value.
///
/// "Unusable" covers three cases, all folded together because every
/// caller below treats them identically (400, never a default):
/// - more than one value for the same header name (`get_all().count() >
///   1`) — Node's `http` module merges repeated header values into one
///   comma-joined string before the TS reference ever sees them, so a
///   request with two `X-iDoris-*` values for the same name only behaves
///   consistently across both implementations if this errors too, instead
///   of silently keeping just the first value and ignoring the rest;
/// - a value that isn't visible-ASCII UTF-8 (`HeaderValue::to_str` fails);
/// - a value that's empty or all whitespace after trimming.
enum HeaderState<'a> {
    Absent,
    Invalid,
    Present(&'a str),
}

fn header_state<'a>(headers: &'a HeaderMap, name: &str) -> HeaderState<'a> {
    let mut values = headers.get_all(name).iter();
    let Some(first) = values.next() else {
        return HeaderState::Absent;
    };
    if values.next().is_some() {
        return HeaderState::Invalid;
    }
    match first.to_str() {
        Ok(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                HeaderState::Invalid
            } else {
                HeaderState::Present(trimmed)
            }
        }
        Err(_) => HeaderState::Invalid,
    }
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

fn invalid_privacy() -> ProfileError {
    ProfileError::new(
        StatusCode::BAD_REQUEST,
        "invalid_privacy",
        "X-iDoris-Privacy must be a single, non-blank, UTF-8 value: local_only|any",
    )
}

fn parse_privacy(headers: &HeaderMap) -> Result<PrivacyClass, ProfileError> {
    match header_state(headers, "x-idoris-privacy") {
        HeaderState::Absent => Ok(PrivacyClass::LocalOnly),
        HeaderState::Invalid => Err(invalid_privacy()),
        HeaderState::Present("local_only") => Ok(PrivacyClass::LocalOnly),
        HeaderState::Present("any") => Ok(PrivacyClass::Any),
        HeaderState::Present(_) => Err(invalid_privacy()),
    }
}

fn parse_intent(headers: &HeaderMap) -> Result<(String, IntentSource), ProfileError> {
    match header_state(headers, "x-idoris-intent") {
        HeaderState::Absent => Ok(("chat".to_string(), IntentSource::Default)),
        HeaderState::Invalid => Err(invalid_header(
            "X-iDoris-Intent must be a single, non-blank, UTF-8 value",
        )),
        HeaderState::Present(s) => Ok((s.to_string(), IntentSource::Header)),
    }
}

fn parse_complexity(headers: &HeaderMap) -> Result<Complexity, ProfileError> {
    match header_state(headers, "x-idoris-complexity") {
        HeaderState::Absent => Ok(Complexity::Simple),
        HeaderState::Invalid => Err(invalid_header(
            "X-iDoris-Complexity must be a single, non-blank, UTF-8 value: simple|complex",
        )),
        HeaderState::Present("simple") => Ok(Complexity::Simple),
        HeaderState::Present("complex") => Ok(Complexity::Complex),
        HeaderState::Present(_) => {
            Err(invalid_header("X-iDoris-Complexity must be simple|complex"))
        }
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
    let raw = match header_state(headers, "x-idoris-capabilities") {
        HeaderState::Absent => return Ok(vec![Capability::Chat]),
        HeaderState::Invalid => {
            return Err(invalid_header(
                "X-iDoris-Capabilities must be a single, non-blank, UTF-8 value",
            ));
        }
        HeaderState::Present(s) => s,
    };
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

fn parse_fallback(headers: &HeaderMap) -> Result<Option<FallbackPolicy>, ProfileError> {
    match header_state(headers, "x-idoris-fallback") {
        HeaderState::Absent => Ok(None),
        HeaderState::Invalid => Err(invalid_header(
            "X-iDoris-Fallback must be a single, non-blank, UTF-8 value: fail_closed|next_in_chain",
        )),
        HeaderState::Present("fail_closed") => Ok(Some(FallbackPolicy::FailClosed)),
        HeaderState::Present("next_in_chain") => Ok(Some(FallbackPolicy::NextInChain)),
        HeaderState::Present(_) => Err(invalid_header(
            "X-iDoris-Fallback must be fail_closed|next_in_chain",
        )),
    }
}

/// `model` request-body field → optional resolved [`Role`]. `Ok(None)`
/// means "not an `idoris/<role>` model name" — an opaque model id, passed
/// through unconstrained by role.
fn parse_role(model: Option<&str>) -> Result<Option<Role>, ProfileError> {
    let Some(model) = model else {
        return Ok(None);
    };
    parse_model_role(model).map_err(|err: RoleParseError| {
        ProfileError::new(StatusCode::BAD_REQUEST, "unknown_role", err.to_string())
    })
}

/// Builds the control-plane profile from headers + the parsed request
/// body's `model` field. **Callers must have already rejected invalid JSON
/// / a non-object body** — that ordering is enforced by the caller, not
/// here. Internal order: privacy → intent/complexity/capabilities/fallback
/// → role → tenant (`profile.ts`'s own precedence, with role slotted in
/// just before the tenant check since it has nothing on the TS side to
/// stay ordered against).
pub fn parse_profile(
    headers: &HeaderMap,
    model: Option<&str>,
    deploy_mode: DeployMode,
) -> Result<ParsedProfile, ProfileError> {
    let privacy = parse_privacy(headers)?;
    let (intent, intent_source) = parse_intent(headers)?;
    let complexity = parse_complexity(headers)?;
    let capabilities = parse_capabilities(headers)?;
    let fallback = parse_fallback(headers)?;
    let role = parse_role(model)?;

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
            let tenant = match header_state(headers, "x-idoris-tenant") {
                HeaderState::Present(s) if crate::event_identifier::valid(s) => s.to_string(),
                HeaderState::Absent | HeaderState::Invalid => {
                    return Err(ProfileError::new(
                        StatusCode::BAD_REQUEST,
                        "tenant_missing",
                        "deploy_mode=tenant requires a single, non-blank X-iDoris-Tenant; no default tenant fallback",
                    ));
                }
                HeaderState::Present(_) => {
                    return Err(ProfileError::new(
                        StatusCode::BAD_REQUEST,
                        "tenant_missing",
                        "deploy_mode=tenant requires a valid Event Log tenant identifier",
                    ));
                }
            };
            Some(tenant)
        }
    };

    Ok(ParsedProfile {
        task,
        role,
        tenant_id,
        intent_source,
    })
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

    /// A header with two distinct values for the same name (via `append`,
    /// not `insert` — `insert` would just replace the first one).
    fn headers_with_duplicate(name: &str, first: &str, second: &str) -> HeaderMap {
        let mut map = HeaderMap::new();
        let header_name = axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap();
        map.append(
            header_name.clone(),
            axum::http::HeaderValue::from_str(first).unwrap(),
        );
        map.append(
            header_name,
            axum::http::HeaderValue::from_str(second).unwrap(),
        );
        map
    }

    #[test]
    fn defaults_privacy_to_local_only_when_header_absent() {
        let parsed = parse_profile(&headers(&[]), None, DeployMode::Personal).unwrap();
        assert_eq!(parsed.task.privacy, Some(PrivacyClass::LocalOnly));
        assert_eq!(parsed.task.intent, Some("chat".to_string()));
        assert_eq!(parsed.intent_source, IntentSource::Default);
        assert_eq!(parsed.task.complexity, Some(Complexity::Simple));
        assert_eq!(parsed.task.capabilities, Some(vec![Capability::Chat]));
        assert_eq!(parsed.task.fallback, None);
        assert_eq!(parsed.tenant_id, None);
    }

    #[test]
    fn rejects_invalid_privacy() {
        let err = parse_profile(
            &headers(&[("x-idoris-privacy", "bogus")]),
            None,
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
            None,
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
            let err = parse_profile(&headers(&[(header, value)]), None, DeployMode::Personal)
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
            None,
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
            None,
            DeployMode::Personal,
        )
        .unwrap();
        assert_eq!(
            parsed.task.intent,
            Some("totally-made-up-intent".to_string())
        );
        assert_eq!(parsed.intent_source, IntentSource::Header);
    }

    #[test]
    fn tenant_mode_requires_tenant_header() {
        let err = parse_profile(&headers(&[]), None, DeployMode::Tenant).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error_type, "tenant_missing");
    }

    #[test]
    fn tenant_mode_passes_through_the_declared_tenant() {
        let parsed = parse_profile(
            &headers(&[("x-idoris-tenant", "acme")]),
            None,
            DeployMode::Tenant,
        )
        .unwrap();
        assert_eq!(parsed.tenant_id, Some("acme".to_string()));
    }

    #[test]
    fn tenant_identifier_matches_event_log_boundary() {
        let accepted = "t".repeat(128);
        let parsed = parse_profile(
            &headers(&[("x-idoris-tenant", accepted.as_str())]),
            None,
            DeployMode::Tenant,
        )
        .unwrap();
        assert_eq!(parsed.tenant_id.as_deref(), Some(accepted.as_str()));

        for rejected in ["t".repeat(129), "left\tright".to_string()] {
            let err = parse_profile(
                &headers(&[("x-idoris-tenant", rejected.as_str())]),
                None,
                DeployMode::Tenant,
            )
            .expect_err("tenant identifiers rejected by Event Log must fail before routing");
            assert_eq!(err.status, StatusCode::BAD_REQUEST);
            assert_eq!(err.error_type, "tenant_missing");
        }
    }

    /// prdaemon review (PR #125): a header the caller *did* send, but with
    /// an unusable value, must 400 -- never silently fall back to the
    /// default the way a genuinely *absent* header does. Table-driven over
    /// every field, each checked with both a whitespace-only value and a
    /// non-ASCII one (`HeaderValue` accepts non-ASCII "obs-text" bytes at
    /// construction time, but `to_str()` rejects them, which is exactly
    /// the case this fix must catch instead of treating as "absent").
    #[test]
    fn a_present_but_blank_or_non_ascii_header_400s_instead_of_defaulting() {
        let cases: &[(&str, &str)] = &[
            ("x-idoris-privacy", "invalid_privacy"),
            ("x-idoris-intent", "invalid_header"),
            ("x-idoris-complexity", "invalid_header"),
            ("x-idoris-capabilities", "invalid_header"),
            ("x-idoris-fallback", "invalid_header"),
        ];
        for (header, expected_type) in cases {
            for bad_value in ["   ", "café"] {
                let err =
                    parse_profile(&headers(&[(header, bad_value)]), None, DeployMode::Personal)
                        .expect_err(&format!(
                            "{header}={bad_value:?} must be rejected, not defaulted"
                        ));
                assert_eq!(
                    err.status,
                    StatusCode::BAD_REQUEST,
                    "{header}={bad_value:?}"
                );
                assert_eq!(err.error_type, *expected_type, "{header}={bad_value:?}");
            }
        }
    }

    #[test]
    fn tenant_header_present_but_blank_or_non_ascii_is_tenant_missing_not_a_blank_tenant() {
        for bad_value in ["   ", "café"] {
            let err = parse_profile(
                &headers(&[("x-idoris-tenant", bad_value)]),
                None,
                DeployMode::Tenant,
            )
            .expect_err(&format!(
                "blank/non-ASCII tenant {bad_value:?} must be rejected"
            ));
            assert_eq!(err.error_type, "tenant_missing");
        }
    }

    /// prdaemon review (PR #125): two values for the same header name must
    /// 400, matching Node's http module merging repeated headers into one
    /// string before the TS reference ever sees them (so silently keeping
    /// just the first value, as a bare `HeaderMap::get` would, is a real
    /// behavioral divergence, not just a theoretical one).
    #[test]
    fn duplicate_header_values_400_instead_of_silently_using_the_first() {
        let cases: &[(&str, &str, &str, &str)] = &[
            ("x-idoris-privacy", "local_only", "any", "invalid_privacy"),
            ("x-idoris-intent", "chat", "coding", "invalid_header"),
            ("x-idoris-complexity", "simple", "complex", "invalid_header"),
            ("x-idoris-capabilities", "chat", "coding", "invalid_header"),
            (
                "x-idoris-fallback",
                "fail_closed",
                "next_in_chain",
                "invalid_header",
            ),
        ];
        for (header, first, second, expected_type) in cases {
            let err = parse_profile(
                &headers_with_duplicate(header, first, second),
                None,
                DeployMode::Personal,
            )
            .expect_err(&format!("duplicate {header} must be rejected"));
            assert_eq!(err.error_type, *expected_type, "{header}");
        }
    }

    #[test]
    fn duplicate_tenant_header_is_tenant_missing() {
        let err = parse_profile(
            &headers_with_duplicate("x-idoris-tenant", "acme", "beta"),
            None,
            DeployMode::Tenant,
        )
        .expect_err("duplicate X-iDoris-Tenant must be rejected");
        assert_eq!(err.error_type, "tenant_missing");
    }

    #[test]
    fn resolves_idoris_role_from_model() {
        let parsed =
            parse_profile(&headers(&[]), Some("idoris/daily"), DeployMode::Personal).unwrap();
        assert_eq!(parsed.role, Some(Role::Daily));
    }

    #[test]
    fn non_idoris_model_has_no_role_constraint() {
        let parsed = parse_profile(&headers(&[]), Some("gpt-4o"), DeployMode::Personal).unwrap();
        assert_eq!(parsed.role, None);
    }

    #[test]
    fn unknown_role_is_a_400() {
        let err =
            parse_profile(&headers(&[]), Some("idoris/nope"), DeployMode::Personal).unwrap_err();
        assert_eq!(err.status, StatusCode::BAD_REQUEST);
        assert_eq!(err.error_type, "unknown_role");
    }
}
