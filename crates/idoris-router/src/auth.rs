//! Virtual-key authentication boundary for B5.
//!
//! Caller-supplied metadata headers are deliberately ignored here. The only
//! trusted caller identity comes from a valid iDoris bearer token resolved
//! through the virtual-key store.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use idoris_contracts::common::{FallbackPolicy, PrivacyClass};
use idoris_tenancy::virtual_key::VirtualKeySecret;
use idoris_tenancy::virtual_key::store::{AuthenticatedVirtualKey, VirtualKeyStore};

use crate::profile::ParsedProfile;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKeyAuthError {
    Unauthorized,
    Unavailable,
}

impl std::fmt::Display for VirtualKeyAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unauthorized => f.write_str("virtual key unauthorized"),
            Self::Unavailable => f.write_str("virtual key authentication unavailable"),
        }
    }
}

impl std::error::Error for VirtualKeyAuthError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VirtualKeyScopeError {
    PrivacyForbidden,
    RoleForbidden,
    FallbackForbidden,
}

impl VirtualKeyScopeError {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::PrivacyForbidden => "VIRTUAL_KEY_PRIVACY_FORBIDDEN",
            Self::RoleForbidden => "VIRTUAL_KEY_ROLE_FORBIDDEN",
            Self::FallbackForbidden => "VIRTUAL_KEY_FALLBACK_FORBIDDEN",
        }
    }
}

impl std::fmt::Display for VirtualKeyScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.reason_code())
    }
}

impl std::error::Error for VirtualKeyScopeError {}

/// Applies the authenticated key as an authorization ceiling over the
/// already-parsed request profile. Scope can only reject, never widen.
pub fn enforce_scope(
    identity: &AuthenticatedVirtualKey,
    profile: &ParsedProfile,
) -> Result<(), VirtualKeyScopeError> {
    let privacy = profile.task.privacy.unwrap_or(PrivacyClass::LocalOnly);
    if !identity.scope.allowed_privacy.contains(&privacy) {
        return Err(VirtualKeyScopeError::PrivacyForbidden);
    }
    if matches!(profile.task.fallback, Some(FallbackPolicy::NextInChain)) {
        return Err(VirtualKeyScopeError::FallbackForbidden);
    }
    let role = profile.role.ok_or(VirtualKeyScopeError::RoleForbidden)?;
    if !identity
        .scope
        .allowed_roles
        .iter()
        .any(|allowed| allowed == role.as_str())
    {
        return Err(VirtualKeyScopeError::RoleForbidden);
    }
    Ok(())
}

#[derive(Clone)]
pub struct VirtualKeyAuthenticator {
    store: Arc<Mutex<VirtualKeyStore>>,
}

impl VirtualKeyAuthenticator {
    pub fn new(store: Arc<Mutex<VirtualKeyStore>>) -> Self {
        Self { store }
    }

    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
        let secret = bearer_secret(headers)?;
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| VirtualKeyAuthError::Unavailable)?
            .as_millis()
            .try_into()
            .map_err(|_| VirtualKeyAuthError::Unavailable)?;
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || authenticate_secret(&store, secret, now_ms))
            .await
            .map_err(|_| VirtualKeyAuthError::Unavailable)?
    }

    #[cfg(test)]
    fn authenticate_at(
        &self,
        headers: &HeaderMap,
        now_ms: i64,
    ) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
        let secret = bearer_secret(headers)?;
        authenticate_secret(&self.store, secret, now_ms)
    }
}

fn authenticate_secret(
    store: &Arc<Mutex<VirtualKeyStore>>,
    secret: VirtualKeySecret,
    now_ms: i64,
) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
    let store = store.lock().map_err(|_| VirtualKeyAuthError::Unavailable)?;
    store
        .authenticate(&secret, now_ms)
        .map_err(|_| VirtualKeyAuthError::Unavailable)?
        .ok_or(VirtualKeyAuthError::Unauthorized)
}

fn bearer_secret(headers: &HeaderMap) -> Result<VirtualKeySecret, VirtualKeyAuthError> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let first = values.next().ok_or(VirtualKeyAuthError::Unauthorized)?;
    if values.next().is_some() {
        return Err(VirtualKeyAuthError::Unauthorized);
    }
    let raw = first
        .to_str()
        .map_err(|_| VirtualKeyAuthError::Unauthorized)?;
    let mut parts = raw.split_ascii_whitespace();
    let scheme = parts.next().ok_or(VirtualKeyAuthError::Unauthorized)?;
    let token = parts.next().ok_or(VirtualKeyAuthError::Unauthorized)?;
    if parts.next().is_some() || !scheme.eq_ignore_ascii_case("bearer") {
        return Err(VirtualKeyAuthError::Unauthorized);
    }
    VirtualKeySecret::parse(token).map_err(|_| VirtualKeyAuthError::Unauthorized)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use idoris_contracts::DeployMode;
    use idoris_contracts::common::PrivacyClass;
    use idoris_tenancy::virtual_key::MintedVirtualKey;
    use idoris_tenancy::virtual_key::store::VirtualKeyScope;
    use rusqlite::Connection;

    use super::*;
    use crate::profile::parse_profile;

    fn scope(expires_at_ms: Option<i64>) -> VirtualKeyScope {
        VirtualKeyScope {
            owner: "agent24-instance".into(),
            allowed_privacy: vec![PrivacyClass::LocalOnly],
            allowed_roles: vec!["fast".into()],
            budget_ref: None,
            expires_at_ms,
            admin_scopes: Vec::new(),
        }
    }

    fn authenticator(
        key: &MintedVirtualKey,
        expires_at_ms: Option<i64>,
    ) -> VirtualKeyAuthenticator {
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        store
            .insert_active(&key.key_id, key.hash, &scope(expires_at_ms))
            .unwrap();
        VirtualKeyAuthenticator::new(Arc::new(Mutex::new(store)))
    }

    fn bearer(raw: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, raw.parse().unwrap());
        headers
    }

    #[test]
    fn valid_bearer_produces_trusted_identity_and_ignores_caller_metadata() {
        let key = MintedVirtualKey::mint();
        let auth = authenticator(&key, None);
        let mut headers = bearer(&format!("Bearer {}", key.secret.expose_secret()));
        headers.insert("x-idoris-caller", "attacker/self-asserted".parse().unwrap());
        let identity = auth.authenticate_at(&headers, 10).unwrap();
        assert_eq!(identity.key_id, key.key_id);
        assert_eq!(identity.scope.owner, "agent24-instance");
        assert_eq!(
            identity.scope.allowed_privacy,
            vec![PrivacyClass::LocalOnly]
        );
    }

    #[test]
    fn missing_malformed_wrong_revoked_and_expired_all_collapse_to_unauthorized() {
        let key = MintedVirtualKey::mint();
        let store = VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap();
        store
            .insert_active(&key.key_id, key.hash, &scope(Some(100)))
            .unwrap();
        let auth = VirtualKeyAuthenticator::new(Arc::new(Mutex::new(store)));
        let wrong = MintedVirtualKey::mint();

        let cases = [
            HeaderMap::new(),
            bearer("Basic abc"),
            bearer("Bearer malformed"),
            bearer(&format!("Bearer {}", wrong.secret.expose_secret())),
        ];
        for headers in cases {
            assert_eq!(
                auth.authenticate_at(&headers, 50).unwrap_err(),
                VirtualKeyAuthError::Unauthorized
            );
        }
        assert_eq!(
            auth.authenticate_at(
                &bearer(&format!("Bearer {}", key.secret.expose_secret())),
                100
            )
            .unwrap_err(),
            VirtualKeyAuthError::Unauthorized
        );
        auth.store.lock().unwrap().revoke(&key.key_id, 101).unwrap();
        assert_eq!(
            auth.authenticate_at(
                &bearer(&format!("Bearer {}", key.secret.expose_secret())),
                102
            )
            .unwrap_err(),
            VirtualKeyAuthError::Unauthorized
        );
    }

    #[test]
    fn repeated_authorization_and_secret_sentinels_never_escape_errors() {
        let key = MintedVirtualKey::mint();
        let auth = authenticator(&key, None);
        let sentinel = key.secret.expose_secret();
        let mut headers = bearer(&format!("Bearer {sentinel}"));
        headers.append(AUTHORIZATION, format!("Bearer {sentinel}").parse().unwrap());
        let error = auth.authenticate_at(&headers, 1).unwrap_err();
        assert_eq!(error, VirtualKeyAuthError::Unauthorized);
        let rendered = format!("{error} {error:?}");
        assert!(!rendered.contains(sentinel));
    }

    fn identity(privacy: Vec<PrivacyClass>, roles: &[&str]) -> AuthenticatedVirtualKey {
        AuthenticatedVirtualKey {
            key_id: "vk-test".into(),
            scope: VirtualKeyScope {
                owner: "agent24-instance".into(),
                allowed_privacy: privacy,
                allowed_roles: roles.iter().map(|role| (*role).to_string()).collect(),
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        }
    }

    fn profile(model: &str, privacy: Option<&str>) -> crate::profile::ParsedProfile {
        let mut headers = HeaderMap::new();
        if let Some(privacy) = privacy {
            headers.insert("x-idoris-privacy", privacy.parse().unwrap());
        }
        parse_profile(&headers, Some(model), DeployMode::Personal).unwrap()
    }

    #[test]
    fn privacy_and_role_scopes_are_explicit_ceilings() {
        let local_fast = identity(vec![PrivacyClass::LocalOnly], &["fast"]);
        assert!(enforce_scope(&local_fast, &profile("idoris/fast", None)).is_ok());
        assert_eq!(
            enforce_scope(&local_fast, &profile("idoris/fast", Some("any"))).unwrap_err(),
            VirtualKeyScopeError::PrivacyForbidden
        );
        assert_eq!(
            enforce_scope(&local_fast, &profile("idoris/daily", None)).unwrap_err(),
            VirtualKeyScopeError::RoleForbidden
        );

        let any_fast = identity(vec![PrivacyClass::LocalOnly, PrivacyClass::Any], &["fast"]);
        assert!(enforce_scope(&any_fast, &profile("idoris/fast", Some("any"))).is_ok());
        assert!(enforce_scope(&any_fast, &profile("idoris/fast", None)).is_ok());
    }

    #[test]
    fn empty_roles_or_concrete_model_ids_never_bypass_role_scope() {
        let no_roles = identity(vec![PrivacyClass::LocalOnly], &[]);
        assert_eq!(
            enforce_scope(&no_roles, &profile("idoris/fast", None)).unwrap_err(),
            VirtualKeyScopeError::RoleForbidden
        );

        let fast = identity(vec![PrivacyClass::LocalOnly], &["fast"]);
        assert_eq!(
            enforce_scope(&fast, &profile("backend-concrete-model", None)).unwrap_err(),
            VirtualKeyScopeError::RoleForbidden
        );
    }

    #[test]
    fn missing_internal_privacy_is_treated_as_local_only_not_any() {
        let local_fast = identity(vec![PrivacyClass::LocalOnly], &["fast"]);
        let mut parsed = profile("idoris/fast", None);
        parsed.task.privacy = None;
        assert!(enforce_scope(&local_fast, &parsed).is_ok());
    }
}
