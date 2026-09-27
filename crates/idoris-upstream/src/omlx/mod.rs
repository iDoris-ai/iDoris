//! oMLX `RuntimeAdapter` — mirrors `packages/adapters/omlx/
//! omlx-backend.ts` on `main` (FU-16) behavior, ported onto
//! `idoris_backend::RuntimeAdapter`. Talks to `http://127.0.0.1:8088` by
//! default. Split across submodules landing across several PRs on this
//! stack: [`http`] (GET/POST/PUT + timeout + safe errors), [`status`]
//! (`list`/`status` parsing), [`pin`] (this PR, resident/admin-session
//! gap), and [`OmlxAdapter`] wiring `list`/`status` together —
//! `load`/`unload`/`probe_ready`/`chat`, and the actual `RuntimeAdapter`
//! impl, follow in the next PRs.
//!
//! **The API key is read from an env var and never logged** — see
//! [`OMLX_API_KEY_ENV`] and `http`'s module doc.

mod http;
mod pin;
mod status;

use std::time::Duration;

use idoris_backend::{BackendError, BackendStatus, ModelInfo};

/// Default oMLX base URL (local, custom port — see the TS reference's
/// design-doc citation, D7).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:8088";

/// Default per-call timeout. Every oMLX call must have one — a hung local
/// process must not hang this adapter (and, transitively, the Supervisor's
/// single-flighted load/evict mutex) forever.
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Env var oMLX's inference API key is read from. Never logged, never put
/// in an error message.
pub const OMLX_API_KEY_ENV: &str = "IDORIS_OMLX_API_KEY";

/// `BackendError` has no `upstream()` constructor (only `Upstream
/// { message }`) — shorthand shared by `http` and `status`.
pub(crate) fn upstream_error(message: impl Into<String>) -> BackendError {
    BackendError::Upstream {
        message: message.into(),
    }
}

#[derive(Debug, Clone)]
pub struct OmlxAdapterConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub call_timeout: Duration,
}

impl Default for OmlxAdapterConfig {
    /// Reads the API key from [`OMLX_API_KEY_ENV`] — the one place this
    /// adapter touches the environment for it; every other constructor
    /// path takes the key as an explicit value instead of reaching for the
    /// environment itself.
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: std::env::var(OMLX_API_KEY_ENV).ok(),
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

pub struct OmlxAdapter {
    base_url: String,
    api_key: Option<String>,
    call_timeout: Duration,
    client: reqwest::Client,
}

impl OmlxAdapter {
    pub fn new(config: OmlxAdapterConfig) -> Result<Self, BackendError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|_| BackendError::internal("failed to build the oMLX HTTP client"))?;
        Ok(Self {
            base_url: config.base_url.trim_end_matches('/').to_string(),
            api_key: config.api_key,
            call_timeout: config.call_timeout,
            client,
        })
    }

    fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// `GET /v1/models` — every model this instance could route to,
    /// whether or not currently loaded (see `status::parse_list`'s doc).
    pub async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        let raw = http::get_json(
            &self.client,
            &self.base_url,
            "/v1/models",
            self.api_key(),
            self.call_timeout,
        )
        .await?;
        Ok(status::parse_list(&raw))
    }

    /// `GET /api/status` — engine-wide memory/pressure signal and the
    /// currently-loaded model list (see `status::parse_status`'s doc).
    pub async fn status(&self) -> Result<BackendStatus, BackendError> {
        let raw = http::get_json(
            &self.client,
            &self.base_url,
            "/api/status",
            self.api_key(),
            self.call_timeout,
        )
        .await?;
        status::parse_status(&raw)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;

    async fn adapter_for(server: &MockServer) -> OmlxAdapter {
        OmlxAdapter::new(OmlxAdapterConfig {
            base_url: server.uri(),
            api_key: Some("test-key-should-never-leak".to_string()),
            call_timeout: Duration::from_millis(500),
        })
        .expect("adapter must build")
    }

    // Per-failure-mode coverage (4xx/5xx/timeout/malformed body) already
    // lives in `http`'s tests (at the `get_json` layer) and `status`'s
    // tests (at the parsing layer) — these integration tests check only
    // that `OmlxAdapter` actually wires those pieces together correctly,
    // end to end, including the API key.

    #[tokio::test]
    async fn list_returns_models_with_string_ids() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "qwen3-8b"}, {"id": "qwen3-4b"}]
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server).await;
        let models = adapter.list().await.expect("list must succeed");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "qwen3-8b");
    }

    #[tokio::test]
    async fn status_parses_loaded_models_and_converts_bytes_to_gib() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "loaded_models": ["qwen3-8b"],
                "model_memory_max": 1024_f64 * 1024.0 * 1024.0 * 16.0,
                "model_memory_used": 0,
                "pressure": "soft",
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server).await;
        let status = adapter.status().await.expect("status must succeed");
        assert_eq!(status.loaded, vec!["qwen3-8b".to_string()]);
        assert!((status.model_memory_max_gb - 16.0).abs() < 1e-9);
        assert_eq!(status.pressure, idoris_backend::Pressure::Soft);
    }

    #[tokio::test]
    async fn status_on_4xx_reports_upstream_error_without_leaking_the_api_key() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/status"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string("unauthorized-body-should-not-leak"),
            )
            .mount(&server)
            .await;
        let adapter = adapter_for(&server).await;
        let err = adapter.status().await.expect_err("4xx must fail");
        assert_eq!(err.reason_code(), "upstream_error");
        let msg = err.to_string();
        assert!(msg.contains("401"));
        assert!(!msg.contains("unauthorized-body-should-not-leak"));
        assert!(!msg.contains("test-key-should-never-leak"));
    }
}
