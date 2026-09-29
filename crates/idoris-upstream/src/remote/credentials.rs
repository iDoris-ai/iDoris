//! [`CredentialSource`] — how a remote provider client obtains its API
//! key, kept as its own trait (rather than a bare `String`/env-var lookup
//! baked into the client) specifically so a later backend (a secrets
//! vault, a rotating-token service, ...) can be dropped in without
//! changing the client's own code — only [`EnvCredentialSource`] (reading
//! a fixed env var) is implemented so far.
//!
//! **The resolved key must never be logged or embedded in an error
//! message.** [`CredentialSource::api_key`] returns
//! [`crate::UpstreamError`], whose `auth_failed` variant carries no key
//! material — see its module doc.

use async_trait::async_trait;

use crate::UpstreamError;

#[async_trait]
pub trait CredentialSource: Send + Sync {
    /// Resolve the API key. `provider` is a short, informational label
    /// (e.g. `"openai"`, `"anthropic"`) for implementations that key
    /// credentials by provider — it is not validated against any fixed
    /// set, and a source backing a single provider may ignore it.
    ///
    /// `Err(UpstreamError::AuthFailed)` when no credential is available —
    /// this is deliberately the same variant a rejected request would
    /// produce (see `UpstreamError`'s module doc): "no credential to send"
    /// and "the credential we sent was rejected" are both, from the
    /// caller's perspective, "this call cannot proceed for auth reasons".
    async fn api_key(&self, provider: &str) -> Result<String, UpstreamError>;
}

/// Reads a fixed environment variable, resolved fresh on every
/// [`CredentialSource::api_key`] call (not cached at construction) so a
/// key rotated by updating the process's environment takes effect on the
/// next call rather than requiring a new client.
#[derive(Debug, Clone)]
pub struct EnvCredentialSource {
    env_var: String,
}

impl EnvCredentialSource {
    pub fn new(env_var: impl Into<String>) -> Self {
        Self {
            env_var: env_var.into(),
        }
    }
}

#[async_trait]
impl CredentialSource for EnvCredentialSource {
    async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
        std::env::var(&self.env_var).map_err(|_| UpstreamError::auth_failed())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    // Deliberately does NOT call `std::env::set_var`/`remove_var`: this
    // workspace `forbid`s `unsafe_code` outright (those two functions are
    // `unsafe fn`, and mutating process-global env vars from parallel
    // tests is exactly the kind of hazard that lint exists to keep out —
    // no `#[allow]` can locally override a `forbid`). Instead: the
    // "missing" case uses a var name guaranteed never to be set, and the
    // "present" case uses `PATH`, which is present in every test
    // environment this crate runs in — both exercise this module's own
    // mapping logic (`Ok` passthrough / `Err` on absence) without this
    // crate needing to touch the environment itself.

    #[tokio::test]
    async fn resolves_the_value_of_an_env_var_that_is_actually_set() {
        let source = EnvCredentialSource::new("PATH");
        let key = source
            .api_key("openai")
            .await
            .expect("PATH must be set in any test environment");
        assert_eq!(key, std::env::var("PATH").expect("PATH must be set"));
    }

    #[tokio::test]
    async fn missing_env_var_is_auth_failed_not_a_panic_or_empty_string() {
        let var = "IDORIS_UPSTREAM_TEST_CREDENTIAL_DOES_NOT_EXIST_XYZ123";
        assert!(
            std::env::var(var).is_err(),
            "test precondition: {var} must not be set"
        );
        let source = EnvCredentialSource::new(var);
        let err = source
            .api_key("anthropic")
            .await
            .expect_err("missing env var must fail, not silently succeed");
        assert!(err.is_auth_failed());
        assert_eq!(err.reason_code(), "auth_failed");
    }

    #[tokio::test]
    async fn provider_label_does_not_affect_resolution() {
        let source = EnvCredentialSource::new("PATH");
        let a = source.api_key("openai").await.unwrap();
        let b = source.api_key("anything-else").await.unwrap();
        assert_eq!(a, b);
    }
}
