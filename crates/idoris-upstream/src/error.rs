//! [`UpstreamError`] — the stable error surface every upstream client in
//! this crate returns, whether the upstream is a local runtime engine
//! (oMLX, via an `idoris_backend::RuntimeAdapter` impl landing in a
//! follow-up PR) or a remote HTTP provider (a `RemoteChat` client, same).
//!
//! **Why this exists:** a well-known OpenRouter incident folded an auth
//! rejection and a genuine upstream dependency failure into one generic
//! "unavailable" signal, so callers retried a 401 as if it were a
//! transient outage. `reason_code()` gives callers a stable string to
//! match on instead of `Display` text, and the categories are chosen so
//! **auth failure and dependency failure can never collapse into the same
//! code**: `timeout`, `upstream_client_error`/`upstream_server_error`
//! (4xx/5xx, 401/403 excluded), `network_error` (never reached the
//! upstream at all), `auth_failed` (401/403, or no credential resolved).
//! [`UpstreamError::from_status`] is the single *intended* call site that
//! classifies a raw status code, and it routes 401/403 to `auth_failed`
//! unconditionally — no other path should build `ClientError`/`ServerError`
//! for what is actually an auth rejection.
//!
//! **Known limitation (accepted, same trade-off `idoris_backend::BackendError`
//! documents for its own Supervisor-only variants):** `ClientError`/
//! `ServerError`'s fields are public, so nothing in the type system stops a
//! call site from constructing `UpstreamError::ClientError { status: 401 }`
//! directly instead of going through `from_status` — the guarantee above is
//! enforced by convention (route every raw status through `from_status`),
//! not by the compiler. Every call site added by this crate's own PRs
//! follows that convention; revisit with a stricter (e.g. private-field)
//! encoding if an external caller is ever found constructing these variants
//! directly.
//!
//! **No response bodies in `Display`/logs** — every variant carries only
//! machine-safe metadata (a status code, a fixed reason string), never the
//! upstream's response text.

use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum UpstreamError {
    #[error("upstream call timed out before its deadline")]
    Timeout,

    /// Upstream reached; responded with a 4xx other than 401/403.
    #[error("upstream rejected the request: HTTP {status}")]
    ClientError { status: u16 },

    /// Upstream reached; responded with a 5xx.
    #[error("upstream server error: HTTP {status}")]
    ServerError { status: u16 },

    /// Upstream never reached (DNS/connect/reset/TLS) — a statement about
    /// *our* path to it, not its own health; kept apart from `ServerError`.
    #[error("network error reaching upstream")]
    Network,

    /// Credentials rejected (401/403), or none could be resolved before
    /// the call was attempted. Must never surface as `Network`/`ServerError`.
    #[error("authentication with upstream failed")]
    AuthFailed,

    /// 2xx response whose body didn't have the shape this client requires.
    #[error("upstream response could not be parsed")]
    MalformedResponse,

    /// Caller's request was invalid independent of any upstream call;
    /// retrying the identical request will never succeed.
    #[error("invalid request: {message}")]
    InvalidRequest { message: String },

    /// Doesn't fit the buckets above (e.g. a client builder/config error) —
    /// kept out of them so it is never mistaken for timeout/network/auth.
    #[error("internal upstream client error: {message}")]
    Internal { message: String },
}

impl UpstreamError {
    /// Stable machine-readable code, derived from the variant (a fixed
    /// `&'static str`), not stored as a field — so nothing can construct
    /// e.g. `ClientError` while claiming the `auth_failed` code.
    pub fn reason_code(&self) -> &'static str {
        match self {
            UpstreamError::Timeout => "timeout",
            UpstreamError::ClientError { .. } => "upstream_client_error",
            UpstreamError::ServerError { .. } => "upstream_server_error",
            UpstreamError::Network => "network_error",
            UpstreamError::AuthFailed => "auth_failed",
            UpstreamError::MalformedResponse => "malformed_response",
            UpstreamError::InvalidRequest { .. } => "invalid_request",
            UpstreamError::Internal { .. } => "internal",
        }
    }

    /// `true` iff this is specifically an authentication failure (like
    /// `BackendError::is_oom`, a plain type check, not a retry verdict).
    pub fn is_auth_failed(&self) -> bool {
        matches!(self, UpstreamError::AuthFailed)
    }

    /// Classify a raw HTTP status from a reached upstream. The one
    /// intended call site for turning a status code into an
    /// [`UpstreamError`] — callers must route through here instead of
    /// constructing `ClientError`/`ServerError` directly, so the
    /// 401/403-is-always-`AuthFailed` rule can't be bypassed by a call
    /// site that forgot about it.
    pub fn from_status(status: u16) -> Self {
        match status {
            401 | 403 => UpstreamError::AuthFailed,
            400..=499 => UpstreamError::ClientError { status },
            500..=599 => UpstreamError::ServerError { status },
            // Non-standard status: fail closed as `Internal`, don't guess
            // which of the four core buckets it "probably" belongs to.
            _ => UpstreamError::Internal {
                message: format!("unexpected HTTP status {status}"),
            },
        }
    }

    pub fn timeout() -> Self {
        UpstreamError::Timeout
    }

    pub fn network() -> Self {
        UpstreamError::Network
    }

    pub fn auth_failed() -> Self {
        UpstreamError::AuthFailed
    }

    pub fn malformed_response() -> Self {
        UpstreamError::MalformedResponse
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        UpstreamError::InvalidRequest {
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        UpstreamError::Internal {
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_401_and_403_are_auth_failed_never_client_or_server_error() {
        for status in [401u16, 403] {
            let err = UpstreamError::from_status(status);
            assert_eq!(err.reason_code(), "auth_failed");
            assert!(err.is_auth_failed());
        }
        for status in [404u16, 429, 500, 503] {
            let err = UpstreamError::from_status(status);
            assert!(!err.is_auth_failed());
            assert_ne!(err.reason_code(), "auth_failed");
        }
        assert_eq!(
            UpstreamError::from_status(404).reason_code(),
            "upstream_client_error"
        );
        assert_eq!(
            UpstreamError::from_status(500).reason_code(),
            "upstream_server_error"
        );
    }

    #[test]
    fn out_of_range_status_fails_closed_as_internal() {
        assert_eq!(UpstreamError::from_status(0).reason_code(), "internal");
        assert_eq!(UpstreamError::from_status(600).reason_code(), "internal");
    }

    /// The regression this module exists to prevent: reason codes must
    /// stay pairwise distinct, and network/auth failures in particular
    /// must never be able to impersonate each other.
    #[test]
    fn reason_codes_are_pairwise_distinct_and_network_auth_never_collide() {
        let variants = [
            UpstreamError::Timeout,
            UpstreamError::ClientError { status: 400 },
            UpstreamError::ServerError { status: 500 },
            UpstreamError::Network,
            UpstreamError::AuthFailed,
            UpstreamError::MalformedResponse,
            UpstreamError::InvalidRequest {
                message: "x".into(),
            },
            UpstreamError::Internal {
                message: "x".into(),
            },
        ];
        let codes: Vec<&str> = variants.iter().map(UpstreamError::reason_code).collect();
        for i in 0..codes.len() {
            for j in (i + 1)..codes.len() {
                assert_ne!(codes[i], codes[j], "codes for indices {i} and {j} collided");
            }
        }
        assert!(!UpstreamError::network().is_auth_failed());
        assert!(UpstreamError::auth_failed().is_auth_failed());
    }
}
