//! Virtual-key authentication boundary for B5.
//!
//! Caller-supplied metadata headers are deliberately ignored here. The only
//! trusted caller identity comes from a valid iDoris bearer token resolved
//! through the virtual-key store.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use idoris_tenancy::virtual_key::VirtualKeySecret;
use idoris_tenancy::virtual_key::store::{AuthenticatedVirtualKey, VirtualKeyStore};

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

#[derive(Clone)]
pub struct VirtualKeyAuthenticator {
    store: Arc<Mutex<VirtualKeyStore>>,
}

impl VirtualKeyAuthenticator {
    pub fn new(store: Arc<Mutex<VirtualKeyStore>>) -> Self {
        Self { store }
    }

    pub fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| VirtualKeyAuthError::Unavailable)?
            .as_millis()
            .try_into()
            .map_err(|_| VirtualKeyAuthError::Unavailable)?;
        self.authenticate_at(headers, now_ms)
    }

    fn authenticate_at(
        &self,
        headers: &HeaderMap,
        now_ms: i64,
    ) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
        let secret = bearer_secret(headers)?;
        let store = self
            .store
            .lock()
            .map_err(|_| VirtualKeyAuthError::Unavailable)?;
        store
            .authenticate(&secret, now_ms)
            .map_err(|_| VirtualKeyAuthError::Unavailable)?
            .ok_or(VirtualKeyAuthError::Unauthorized)
    }
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

    use idoris_contracts::common::PrivacyClass;
    use idoris_tenancy::virtual_key::MintedVirtualKey;
    use idoris_tenancy::virtual_key::store::VirtualKeyScope;
    use rusqlite::Connection;

    use super::*;

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
}
