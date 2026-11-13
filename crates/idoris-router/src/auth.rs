//! Virtual-key authentication boundary for B5.
//!
//! Caller-supplied metadata headers are deliberately ignored here. The only
//! trusted caller identity comes from a valid iDoris bearer token resolved
//! through the virtual-key store.

use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::{HeaderMap, header::AUTHORIZATION};
use idoris_contracts::common::{FallbackPolicy, PrivacyClass};
use idoris_policy::ROLES;
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
    TenantForbidden,
    PrivacyForbidden,
    RoleForbidden,
    FallbackForbidden,
}

impl VirtualKeyScopeError {
    pub const fn reason_code(self) -> &'static str {
        match self {
            Self::TenantForbidden => "VIRTUAL_KEY_TENANT_FORBIDDEN",
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
    let full_role_authority = ROLES.iter().all(|role| {
        identity
            .scope
            .allowed_roles
            .iter()
            .any(|allowed| allowed == role.as_str())
    });
    if matches!(profile.task.fallback, Some(FallbackPolicy::NextInChain)) && !full_role_authority {
        return Err(VirtualKeyScopeError::FallbackForbidden);
    }
    let Some(role) = profile.role else {
        return if full_role_authority {
            Ok(())
        } else {
            Err(VirtualKeyScopeError::RoleForbidden)
        };
    };
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

/// The last gate of data-plane authentication (compute service S2): after a bearer secret has been
/// matched to an active, unexpired virtual key, ask whether the *credential* behind that key is still
/// usable right now. Implemented by the account store in the organization and cloud forms; absent
/// (`None`) in the personal form and in every legacy tenant deployment, where authentication is exactly
/// what it was before.
///
/// The check runs on **every** request and nothing is cached, so revoking a credential, freezing it,
/// revoking its membership or principal, or freezing/closing any account above it takes effect on the
/// very next request. `Ok(())` lets the request through (including keys the gate does not manage, such
/// as local data keys issued with `idoris key issue`); `Err(Unauthorized)` refuses it, with the same
/// opaque answer as an unknown key; `Err(Unavailable)` means the answer could not be determined and the
/// request fails closed.
pub trait CredentialGate: Send + Sync {
    fn check(&self, key_id: &str, now_ms: i64) -> Result<(), VirtualKeyAuthError>;
}

#[derive(Clone)]
pub struct VirtualKeyAuthenticator {
    store: Arc<Mutex<VirtualKeyStore>>,
    gate: Option<Arc<dyn CredentialGate>>,
    #[cfg(test)]
    before_store_lock: Option<Arc<tokio::sync::Notify>>,
}

impl VirtualKeyAuthenticator {
    pub fn new(store: Arc<Mutex<VirtualKeyStore>>) -> Self {
        Self {
            store,
            gate: None,
            #[cfg(test)]
            before_store_lock: None,
        }
    }

    /// Installs the credential gate (organization and cloud forms). Without it authentication is
    /// byte-for-byte the pre-S2 behaviour.
    pub fn with_gate(mut self, gate: Arc<dyn CredentialGate>) -> Self {
        self.gate = Some(gate);
        self
    }

    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
    ) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
        self.authenticate_holding(headers, ()).await
    }

    /// 同 [`Self::authenticate`]，另外把 `holder` **搬进真正执行认证查询的阻塞任务**：查询跑完（或者闭包被丢弃，
    /// 比如运行时关闭）才丢弃它，**调用方取消了 `await`（请求 future 被丢弃）也不会提前丢弃**。
    ///
    /// 为什么：云入口的「认证并发名额」（`AuthSlot`）要限制的是同时在库里排队、查询的**阻塞任务**个数。名额如果挂在请求
    /// future 上，请求一被取消（客户端断开、超时）名额就还回去了，而 `spawn_blocking` 出去的任务不会因此停下，
    /// 还在等认证库的锁、查 SQLite：反复「发起再取消」就能攒出远超上限的阻塞任务，把阻塞线程池占满、拖累别的库操作
    /// （R3 N2）。名额跟着阻塞任务走，上限才是真的。
    ///
    /// 凭证格式不对（没有发起任何查询）时 `holder` 当场丢弃。
    pub async fn authenticate_holding<H: Send + 'static>(
        &self,
        headers: &HeaderMap,
        holder: H,
    ) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
        let secret = bearer_secret(headers)?;
        let store = self.store.clone();
        let gate = self.gate.clone();
        #[cfg(test)]
        let before_store_lock = self.before_store_lock.clone();
        tokio::task::spawn_blocking(move || {
            // 注意是具名绑定：`let _ = holder` 会当场丢弃。它活到这个闭包返回
            let _holder = holder;
            #[cfg(test)]
            if let Some(notify) = before_store_lock {
                notify.notify_one();
            }
            authenticate_secret(&store, gate.as_deref(), secret)
        })
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
        authenticate_secret_with_clock(&self.store, self.gate.as_deref(), secret, || Ok(now_ms))
    }

    #[cfg(test)]
    pub(crate) fn notify_before_store_lock(mut self, notify: Arc<tokio::sync::Notify>) -> Self {
        self.before_store_lock = Some(notify);
        self
    }
}

fn authenticate_secret(
    store: &Arc<Mutex<VirtualKeyStore>>,
    gate: Option<&dyn CredentialGate>,
    secret: VirtualKeySecret,
) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
    authenticate_secret_with_clock(store, gate, secret, server_now_ms)
}

fn authenticate_secret_with_clock(
    store: &Arc<Mutex<VirtualKeyStore>>,
    gate: Option<&dyn CredentialGate>,
    secret: VirtualKeySecret,
    mut now_ms: impl FnMut() -> Result<i64, VirtualKeyAuthError>,
) -> Result<AuthenticatedVirtualKey, VirtualKeyAuthError> {
    // The key store lock is released before the gate runs: the gate reads another store.
    let (identity, decision_now) = {
        let store = store.lock().map_err(|_| VirtualKeyAuthError::Unavailable)?;
        let lookup_now = now_ms()?;
        let identity = store
            .authenticate(&secret, lookup_now)
            .map_err(|_| VirtualKeyAuthError::Unavailable)?
            .ok_or(VirtualKeyAuthError::Unauthorized)?;
        let decision_now = now_ms()?;
        if decision_now < lookup_now {
            return Err(VirtualKeyAuthError::Unavailable);
        }
        if identity
            .scope
            .expires_at_ms
            .is_some_and(|expires| expires <= decision_now)
        {
            return Err(VirtualKeyAuthError::Unauthorized);
        }
        (identity, decision_now)
    };
    if let Some(gate) = gate {
        gate.check(&identity.key_id, decision_now)?;
    }
    Ok(identity)
}

fn server_now_ms() -> Result<i64, VirtualKeyAuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| VirtualKeyAuthError::Unavailable)?
        .as_millis()
        .try_into()
        .map_err(|_| VirtualKeyAuthError::Unavailable)
}

pub(crate) fn bearer_secret(headers: &HeaderMap) -> Result<VirtualKeySecret, VirtualKeyAuthError> {
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
            tenant_id: None,
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

    /// 计数「被丢弃了几次」的守卫（代替云入口的认证并发名额）。
    struct Probe(Arc<std::sync::atomic::AtomicUsize>);

    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// R3 N2：交给 `authenticate_holding` 的守卫（云入口的认证并发名额）跟着**阻塞查询**走：调用方取消了 `await`，
    /// 查询还卡在认证库的锁上，守卫就不能被丢弃；查询跑完才丢弃。
    #[tokio::test]
    async fn a_holder_lives_until_the_blocking_query_finishes_even_when_the_caller_gives_up() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::time::Duration;

        let key = MintedVirtualKey::mint();
        let started = Arc::new(tokio::sync::Notify::new());
        let auth = authenticator(&key, None).notify_before_store_lock(started.clone());
        // 另一个线程占着认证库的锁：查询会卡在锁上
        let store = auth.store.clone();
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let lock_thread = std::thread::spawn(move || {
            let _guard = store.lock().unwrap();
            locked_tx.send(()).unwrap();
            let _ = release_rx.recv();
        });
        locked_rx.recv().unwrap();

        let dropped = Arc::new(AtomicUsize::new(0));
        let headers = bearer(&format!("Bearer {}", key.secret.expose_secret()));
        let probe = Probe(dropped.clone());
        let caller = tokio::spawn({
            let auth = auth.clone();
            async move { auth.authenticate_holding(&headers, probe).await }
        });
        // 阻塞任务已经开始（马上要等库的锁），然后调用方放弃等待
        started.notified().await;
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            0,
            "the query is still waiting for the store, so its holder must still be alive"
        );
        // 库有空了：查询跑完，守卫才丢弃
        release_tx.send(()).unwrap();
        lock_thread.join().unwrap();
        for _ in 0..500 {
            if dropped.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }

    /// 凭证格式不对：根本没有发起任何查询，守卫当场丢弃（不会被一个永远不会跑的闭包扣着）。
    #[tokio::test]
    async fn a_holder_given_with_an_unusable_credential_is_released_at_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let key = MintedVirtualKey::mint();
        let auth = authenticator(&key, None);
        let dropped = Arc::new(AtomicUsize::new(0));
        let error = auth
            .authenticate_holding(&HeaderMap::new(), Probe(dropped.clone()))
            .await
            .unwrap_err();
        assert_eq!(error, VirtualKeyAuthError::Unauthorized);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        // 对照：正常的一次认证，守卫在查询之后丢弃，恰好一次
        let headers = bearer(&format!("Bearer {}", key.secret.expose_secret()));
        auth.authenticate_holding(&headers, Probe(dropped.clone()))
            .await
            .unwrap();
        assert_eq!(dropped.load(Ordering::SeqCst), 2);
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
                tenant_id: None,
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

        let roles = ROLES.iter().map(|role| role.as_str()).collect::<Vec<_>>();
        let unrestricted = identity(vec![PrivacyClass::LocalOnly], &roles);
        assert!(enforce_scope(&unrestricted, &profile("backend-concrete-model", None)).is_ok());
    }

    #[test]
    fn missing_internal_privacy_is_treated_as_local_only_not_any() {
        let local_fast = identity(vec![PrivacyClass::LocalOnly], &["fast"]);
        let mut parsed = profile("idoris/fast", None);
        parsed.task.privacy = None;
        assert!(enforce_scope(&local_fast, &parsed).is_ok());
    }

    #[test]
    fn fallback_requires_full_role_authority() {
        let mut parsed = profile("idoris/fast", None);
        parsed.task.fallback = Some(FallbackPolicy::NextInChain);

        let limited = identity(vec![PrivacyClass::LocalOnly], &["fast"]);
        assert_eq!(
            enforce_scope(&limited, &parsed).unwrap_err(),
            VirtualKeyScopeError::FallbackForbidden
        );

        let roles = ROLES.iter().map(|role| role.as_str()).collect::<Vec<_>>();
        let unrestricted = identity(vec![PrivacyClass::LocalOnly], &roles);
        assert!(enforce_scope(&unrestricted, &parsed).is_ok());
    }
}
