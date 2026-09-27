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
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::LoadMode;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};

/// Percent-encode a model id for a URL path segment. Plain
/// [`NON_ALPHANUMERIC`] is *too* aggressive here: it also escapes `-`,
/// which is extremely common in real model ids (e.g. `qwen3-8b`) and would
/// turn `/v1/models/qwen3-8b/load` into `/v1/models/qwen3%2D8b/load` — a
/// different path than any real oMLX route table has (caught by this PR's
/// own tests: they use a hyphenated id and initially 404'd against
/// `wiremock`'s literal path match). Matches JS `encodeURIComponent`'s
/// unreserved set instead: alphanumeric plus `- _ . ! ~ * ' ( )`.
static PATH_SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'!')
    .remove(b'~')
    .remove(b'*')
    .remove(b'\'')
    .remove(b'(')
    .remove(b')');

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

    // Thin wrappers binding `http`'s free functions to this instance's
    // client/base_url/api_key/call_timeout — every call site below reads
    // as just "GET/POST/PUT `path`" instead of repeating all four fields.
    async fn get(&self, path: &str) -> Result<serde_json::Value, BackendError> {
        http::get_json(
            &self.client,
            &self.base_url,
            path,
            self.api_key(),
            self.call_timeout,
        )
        .await
    }

    async fn post(&self, path: &str) -> Result<(), BackendError> {
        http::post_empty(
            &self.client,
            &self.base_url,
            path,
            self.api_key(),
            self.call_timeout,
        )
        .await
    }

    async fn put(&self, path: &str, body: &serde_json::Value) -> Result<(), BackendError> {
        http::put_json(
            &self.client,
            &self.base_url,
            path,
            self.api_key(),
            self.call_timeout,
            body,
        )
        .await
    }

    fn encoded_path(&self, prefix: &str, id: &str, suffix: &str) -> String {
        format!("{prefix}{}{suffix}", utf8_percent_encode(id, PATH_SEGMENT))
    }

    /// `GET /v1/models` — every model this instance could route to,
    /// whether or not currently loaded (see `status::parse_list`'s doc).
    pub async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(status::parse_list(&self.get("/v1/models").await?))
    }

    /// `GET /api/status` — engine-wide memory/pressure signal and the
    /// currently-loaded model list (see `status::parse_status`'s doc).
    pub async fn status(&self) -> Result<BackendStatus, BackendError> {
        status::parse_status(&self.get("/api/status").await?)
    }

    /// `POST /v1/models/{id}/load`, then confirm the resulting pin state
    /// matches `policy` (see [`Self::pin`]/[`Self::check_not_unexpectedly_pinned`]).
    /// A `policy` of `None`, or any mode other than `Resident`, takes the
    /// non-resident path.
    pub async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.post(&self.encoded_path("/v1/models/", id, "/load"))
            .await?;
        if policy.map(|p| p.mode) == Some(LoadMode::Resident) {
            self.pin(id).await
        } else {
            self.check_not_unexpectedly_pinned(id).await
        }
    }

    /// `POST /v1/models/{id}/unload`.
    pub async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.post(&self.encoded_path("/v1/models/", id, "/unload"))
            .await
    }

    async fn verify_model_state(&self, id: &str) -> Result<pin::ModelState, BackendError> {
        pin::parse_model_state(&self.get("/v1/models/status").await?, id)
    }

    /// `resident` path: `PUT /admin/api/models/{id}/settings` then confirm
    /// via [`Self::verify_model_state`]. On 0.6.4 the PUT itself always
    /// fails (401, no admin session) — see the crate module doc — so this
    /// reports a confirmed pin failure (model loaded, running unpinned)
    /// rather than silently treating the load as fully successful.
    async fn pin(&self, id: &str) -> Result<(), BackendError> {
        let path = self.encoded_path("/admin/api/models/", id, "/settings");
        let body = serde_json::json!({ "is_pinned": true });
        self.put(&path, &body).await.map_err(|err| {
            upstream_error(format!(
                "oMLX pin unavailable for {id}: {err} (0.6.4 requires an admin \
                     session the inference API key can't provide; model is loaded \
                     but running unpinned)"
            ))
        })?;
        // A 2xx from PUT doesn't itself confirm the pin took effect —
        // re-check via the read-only status endpoint. A failure at *this*
        // step is a different, weaker claim than the one above: we don't
        // know whether the model ended up pinned or not, only that we
        // couldn't verify it.
        let state = self.verify_model_state(id).await.map_err(|err| {
            upstream_error(format!(
                "oMLX pin state unverified for {id}: PUT succeeded but the \
                 follow-up GET /v1/models/status failed ({err}) — pin status is \
                 unknown, not confirmed either way"
            ))
        })?;
        if state.pinned {
            Ok(())
        } else {
            Err(upstream_error(format!(
                "oMLX pin unavailable for {id}: PUT succeeded but GET \
                 /v1/models/status still reports pinned=false"
            )))
        }
    }

    /// Non-resident path: this adapter never pins on this path — [`Self::pin`]
    /// is the only place that ever sets `is_pinned=true`, and it always
    /// either confirms success or fails loudly, never silently. So a model
    /// that shows up pinned here got that way from outside this adapter's
    /// control (the oMLX admin UI, a pin persisted across a restart, ...) —
    /// state drift to report, not something to silently accept.
    async fn check_not_unexpectedly_pinned(&self, id: &str) -> Result<(), BackendError> {
        let state = self.verify_model_state(id).await?;
        if state.pinned {
            Err(upstream_error(format!(
                "oMLX model {id} is unexpectedly pinned: loaded with a \
                 non-resident policy but oMLX reports pinned=true (external \
                 pin state drift)"
            )))
        } else {
            Ok(())
        }
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

    /// Shorthand for the repeated `Mock::given(...).and(...).respond_with
    /// (...).mount(...).await` boilerplate the `load`/`unload`/pin tests
    /// below all need several of per test.
    async fn mount_all(
        server: &MockServer,
        routes: Vec<(&'static str, &'static str, ResponseTemplate)>,
    ) {
        for (m, p, resp) in routes {
            Mock::given(method(m))
                .and(path(p))
                .respond_with(resp)
                .mount(server)
                .await;
        }
    }

    fn status_mock(pinned: bool) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": true, "pinned": pinned}]
        }))
    }

    fn resident_policy() -> LoadPolicy {
        LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: idoris_contracts::load_policy::Keepalive::Pinned { pinned: true },
            admission: idoris_contracts::load_policy::Admission::Coexist,
        }
    }

    const LOAD_METHOD: &str = "POST";
    const LOAD_PATH: &str = "/v1/models/qwen3-8b/load";
    const SETTINGS_PATH: &str = "/admin/api/models/qwen3-8b/settings";
    const VERIFY_PATH: &str = "/v1/models/status";

    #[tokio::test]
    async fn load_on_demand_succeeds_when_not_pinned() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("GET", VERIFY_PATH, status_mock(false)),
            ],
        )
        .await;
        let adapter = adapter_for(&server).await;
        adapter
            .load("qwen3-8b", None)
            .await
            .expect("on-demand load must succeed");
    }

    #[tokio::test]
    async fn load_resident_fails_when_admin_session_is_rejected() {
        // The realistic 0.6.4 outcome: PUT .../settings 401s (no admin
        // session) — the model is loaded but pinning is confirmed to have
        // failed, not silently treated as a full success.
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("PUT", SETTINGS_PATH, ResponseTemplate::new(401)),
            ],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let err = adapter
            .load("qwen3-8b", Some(&resident_policy()))
            .await
            .expect_err("must fail when the admin session is rejected");
        let msg = err.to_string();
        assert!(msg.contains("pin unavailable") && msg.contains("401"));
        assert!(!msg.contains("test-key-should-never-leak"));
    }

    // The remaining scenarios (external-pin detection, PUT+verify both
    // confirming pinned, PUT 2xx but verify still unpinned, `unload`, and
    // the id-encoding regression guard) land in the next PR on this
    // stack — `load`/`unload`/`pin` themselves are `pub`/reachable from
    // `load` (a `pub` method), so leaving them for now doesn't trip
    // `dead_code`; splitting keeps this PR under the 300-line cap.
}
