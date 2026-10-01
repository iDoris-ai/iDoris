//! oMLX `RuntimeAdapter` — mirrors `packages/adapters/omlx/
//! omlx-backend.ts` on `main` (FU-16) behavior, ported onto
//! `idoris_backend::RuntimeAdapter`. Talks to `http://127.0.0.1:8088` by
//! default. Split across submodules that landed across several PRs on this
//! stack: [`http`] (GET/POST/PUT + timeout + safe errors), [`status`]
//! (`list`/`status` parsing), [`pin`] (resident/admin-session gap), and
//! [`OmlxAdapter`], which wires all of it into a full
//! [`idoris_backend::RuntimeAdapter`] impl (this PR) — `OmlxAdapter`'s own
//! inherent methods (used directly by this module's tests throughout the
//! stack) and the trait impl are the same logic; the trait impl exists so
//! `OmlxAdapter` can be used as `Box<dyn RuntimeAdapter>` by the
//! Supervisor.
//!
//! **The API key is read from an env var and never logged** — see
//! [`OMLX_API_KEY_ENV`] and `http`'s module doc.

mod http;
mod pin;
mod status;

use std::time::Duration;

use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, ModelInfo, RuntimeAdapter,
};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::LoadMode;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC, utf8_percent_encode};
use tokio_util::sync::CancellationToken;

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
        let client = crate::http_client()
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

    async fn post_parse(
        &self,
        path: &str,
        body: &serde_json::Value,
    ) -> Result<serde_json::Value, BackendError> {
        http::post_and_parse(
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
    ///
    /// If the `POST` result is unknown, or it succeeds and a later step
    /// fails, the model may be resident. Such failures are reported as
    /// [`BackendError::LoadUnconfirmed`], never as a plain rejection — see
    /// `RuntimeAdapter::load`'s error contract.
    pub async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        http::post_load(
            &self.client,
            &self.base_url,
            &self.encoded_path("/v1/models/", id, "/load"),
            self.api_key(),
            self.call_timeout,
            id,
        )
        .await?;
        let confirmed = if policy.map(|p| p.mode) == Some(LoadMode::Resident) {
            self.pin(id).await
        } else {
            self.check_not_unexpectedly_pinned(id).await
        };
        confirmed.map_err(|err| BackendError::load_unconfirmed(id, err.to_string()))
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
    /// reports a pin failure (model loaded, pin state unconfirmed)
    /// rather than silently treating the load as fully successful.
    async fn pin(&self, id: &str) -> Result<(), BackendError> {
        let path = self.encoded_path("/admin/api/models/", id, "/settings");
        let body = serde_json::json!({ "is_pinned": true });
        self.put(&path, &body).await.map_err(|err| {
            upstream_error(format!(
                "oMLX pin unavailable for {id}: {err} (0.6.4 requires an admin \
                     session the inference API key can't provide; model is loaded \
                     but pin state is unconfirmed)"
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

    /// `RuntimeAdapter::probe_ready` support: polls `GET
    /// /v1/models/status` for `id`'s `loaded` field.
    ///
    /// Deliberately **lenient**, the opposite of
    /// [`pin::parse_model_state`]: that function is fail-closed because by
    /// the time it's called the caller already believes the model is
    /// loaded, so an unparseable response is itself a problem. Here, a
    /// `Loading` model may legitimately not appear in the list yet, or the
    /// call may hit a transient blip — per `RuntimeAdapter::probe_ready`'s
    /// contract, `Ok(false)` ("not ready yet, keep polling") is correct for
    /// all of those, and only a condition polling again truly cannot fix
    /// should be `Err`.
    ///
    /// **Known limitation (not silently accepted):** this adapter cannot
    /// currently distinguish a terminal failure (auth rejected, id
    /// genuinely unknown to oMLX) from a transient one using only the
    /// boolean success/failure [`Self::get`] exposes — doing so needs a
    /// structured status code, which would mean widening `http`'s
    /// boundary. Until then, every failure here is `Ok(false)`; a
    /// misconfigured API key surfaces as `ProbeTimedOut` once
    /// `probe_max_attempts` is exhausted, rather than failing immediately.
    /// This is the documented safer-wrong direction (see
    /// `RuntimeAdapter::probe_ready`'s own doc: treating a transient blip
    /// as `Err` aborts a load that was actually still in progress, which
    /// is worse than one extra retry cycle).
    pub async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        match self.get("/v1/models/status").await {
            Ok(raw) => Ok(is_ready(&raw, id)),
            Err(_) => Ok(false),
        }
    }

    /// `POST /v1/chat/completions`. `cancel` races the HTTP call itself:
    /// whichever resolves first wins, and the loser is dropped — dropping
    /// the HTTP call future aborts the in-flight request (no separate
    /// "kill the process group" step applies here, unlike a spawn-type
    /// adapter, since this is a plain HTTP client with nothing else to
    /// clean up). Does not itself check whether `id` is loaded: the real
    /// oMLX server rejects a request for a model it hasn't loaded on its
    /// own, and that rejection propagates as an ordinary `Upstream` error
    /// — there is no local ledger here to consult instead.
    pub async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        if cancel.is_cancelled() {
            return Err(BackendError::cancelled());
        }
        let messages: Vec<serde_json::Value> = req
            .messages
            .iter()
            .map(|m| serde_json::json!({"role": m.role, "content": m.content}))
            .collect();
        let body = serde_json::json!({"model": req.model.clone(), "messages": messages});
        let call = self.post_parse("/v1/chat/completions", &body);
        tokio::select! {
            _ = cancel.cancelled() => Err(BackendError::cancelled()),
            result = call => {
                let raw = result?;
                Ok(ChatResponse { model: req.model, content: extract_content(&raw) })
            }
        }
    }
}

/// `body.choices[0].message.content`, defaulting to `""` when any step of
/// that path is missing or the wrong type — matches the TS reference's own
/// `body.choices?.[0]?.message?.content ?? ""` leniency (deliberately not
/// fail-closed here, unlike `status`/`pin`'s parsing: an empty completion
/// is a valid, if unhelpful, chat response, not a broken one).
fn extract_content(raw: &serde_json::Value) -> String {
    raw.get("choices")
        .and_then(|c| c.as_array())
        .and_then(|choices| choices.first())
        .and_then(|choice| choice.get("message"))
        .and_then(|message| message.get("content"))
        .and_then(|content| content.as_str())
        .unwrap_or("")
        .to_string()
}

/// `true` iff `raw`'s `models` array has an entry for `id` with
/// `loaded: true`. Any other shape (missing/malformed `models`, no
/// matching entry, `loaded` absent or not `true`) is `false`, not an
/// error — see [`OmlxAdapter::probe_ready`]'s doc for why leniency is
/// correct here specifically.
fn is_ready(raw: &serde_json::Value, id: &str) -> bool {
    raw.get("models")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .any(|m| {
            m.get("id").and_then(|v| v.as_str()) == Some(id)
                && m.get("loaded").and_then(|v| v.as_bool()) == Some(true)
        })
}

/// Thin delegation to `OmlxAdapter`'s own inherent methods (used directly,
/// throughout this module's tests, by every PR on this branch stack) —
/// this impl exists so a `Box<dyn RuntimeAdapter>` can hold an
/// `OmlxAdapter`. Delegating through `Self::method(self, ...)` rather than
/// `self.method(...)` is not just style: an inherent method always shadows
/// a trait method of the same name in method-call syntax, so `self.list()`
/// here would in fact resolve to the inherent `list` anyway — spelling it
/// as `Self::list(self)` makes that explicit instead of relying on that
/// shadowing rule silently doing the right thing.
#[async_trait::async_trait]
impl RuntimeAdapter for OmlxAdapter {
    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Self::list(self).await
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        Self::load(self, id, policy).await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        Self::unload(self, id).await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        Self::status(self).await
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        Self::probe_ready(self, id).await
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        Self::chat(self, req, cancel).await
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

    #[tokio::test]
    async fn load_on_demand_fails_when_externally_pinned() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("GET", VERIFY_PATH, status_mock(true)),
            ],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let err = adapter
            .load("qwen3-8b", None)
            .await
            .expect_err("must fail on undeclared external pin");
        assert!(err.to_string().contains("unexpectedly pinned"));
    }

    #[tokio::test]
    async fn load_resident_succeeds_when_put_and_verify_both_confirm_pinned() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("PUT", SETTINGS_PATH, ResponseTemplate::new(200)),
                ("GET", VERIFY_PATH, status_mock(true)),
            ],
        )
        .await;
        let adapter = adapter_for(&server).await;
        adapter
            .load("qwen3-8b", Some(&resident_policy()))
            .await
            .expect("must succeed when PUT and verify agree it's pinned");
    }

    #[tokio::test]
    async fn load_resident_fails_when_verify_still_reports_unpinned() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("PUT", SETTINGS_PATH, ResponseTemplate::new(200)),
                ("GET", VERIFY_PATH, status_mock(false)),
            ],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let err = adapter
            .load("qwen3-8b", Some(&resident_policy()))
            .await
            .expect_err("PUT 2xx must not be trusted without verification");
        assert!(err.to_string().contains("still reports pinned=false"));
    }

    /// prdaemon #48 round 2, M1: once `POST .../load` has succeeded, a
    /// failing follow-up check must be reported as `LoadUnconfirmed` (the
    /// model may be resident), not as a plain rejection.
    #[tokio::test]
    async fn a_failure_after_the_load_post_is_reported_as_unconfirmed() {
        for (verify, label) in [
            (ResponseTemplate::new(503), "status 503"),
            (status_mock(true), "external pin drift"),
        ] {
            let server = MockServer::start().await;
            mount_all(
                &server,
                vec![
                    (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                    ("GET", VERIFY_PATH, verify),
                ],
            )
            .await;
            let err = adapter_for(&server)
                .await
                .load("qwen3-8b", None)
                .await
                .expect_err(label);
            assert_eq!(err.reason_code(), "load_unconfirmed", "{label}: {err}");
        }
    }

    /// Negative contrast: a rejected `POST .../load` allocated nothing and
    /// stays a plain upstream error.
    #[tokio::test]
    async fn a_rejected_load_post_is_not_reported_as_unconfirmed() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(400))],
        )
        .await;
        let err = adapter_for(&server)
            .await
            .load("qwen3-8b", None)
            .await
            .expect_err("a 400 from POST load must fail");
        assert_eq!(err.reason_code(), "upstream_error");
    }

    #[tokio::test]
    async fn k09_load_and_pin_timeouts_are_unconfirmed() {
        for pin_timeout in [false, true] {
            let server = MockServer::start().await;
            let slow = ResponseTemplate::new(200).set_delay(Duration::from_secs(1));
            mount_all(
                &server,
                vec![
                    (
                        LOAD_METHOD,
                        LOAD_PATH,
                        if pin_timeout {
                            ResponseTemplate::new(200)
                        } else {
                            slow.clone()
                        },
                    ),
                    ("PUT", SETTINGS_PATH, slow),
                ],
            )
            .await;
            let adapter = OmlxAdapter::new(OmlxAdapterConfig {
                base_url: server.uri(),
                api_key: Some("test-key-should-never-leak".into()),
                call_timeout: Duration::from_millis(100),
            })
            .expect("adapter");
            let policy = pin_timeout.then(resident_policy);
            let err = adapter
                .load("qwen3-8b", policy.as_ref())
                .await
                .expect_err("timeout");
            assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
            assert!(!err.to_string().contains("test-key-should-never-leak"));
            let requests = server.received_requests().await.expect("requests");
            assert_eq!(requests.len(), if pin_timeout { 2 } else { 1 });
        }
    }

    #[tokio::test]
    async fn k09_load_and_pin_connections_closed_after_receipt_are_unconfirmed() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for pin_disconnect in [false, true] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let addr = listener.local_addr().expect("address");
            let received = tokio::spawn(async move {
                let paths: &[&[u8]] = if pin_disconnect {
                    &[
                        b"POST /v1/models/qwen3-8b/load ",
                        b"PUT /admin/api/models/qwen3-8b/settings ",
                    ]
                } else {
                    &[b"POST /v1/models/qwen3-8b/load "]
                };
                for (index, prefix) in paths.iter().enumerate() {
                    let (mut socket, _) = listener.accept().await.expect("accept");
                    let mut request = Vec::new();
                    while !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await.expect("request headers"));
                    }
                    assert!(request.starts_with(prefix));
                    if pin_disconnect && index == 0 {
                        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.expect("load response");
                    }
                    // Close the mutation connection before returning headers.
                }
            });
            let adapter = OmlxAdapter::new(OmlxAdapterConfig {
                base_url: format!("http://{addr}"),
                api_key: None,
                call_timeout: Duration::from_secs(1),
            })
            .expect("adapter");
            let policy = pin_disconnect.then(resident_policy);
            let err = adapter
                .load("qwen3-8b", policy.as_ref())
                .await
                .expect_err("connection closed");
            received.await.expect("server received the mutation");
            assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
        }
    }

    #[tokio::test]
    async fn k09_ambiguous_load_http_status_is_unconfirmed() {
        for status in [408, 500, 503] {
            let server = MockServer::start().await;
            mount_all(
                &server,
                vec![(
                    LOAD_METHOD,
                    LOAD_PATH,
                    ResponseTemplate::new(status).set_body_string("do-not-leak-body"),
                )],
            )
            .await;
            let err = adapter_for(&server)
                .await
                .load("qwen3-8b", None)
                .await
                .expect_err("ambiguous response");
            assert_eq!(err.reason_code(), "load_unconfirmed", "{status}: {err}");
            assert!(!err.to_string().contains("do-not-leak-body"));
        }
    }

    #[tokio::test]
    async fn unload_calls_the_unload_endpoint() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(
                LOAD_METHOD,
                "/v1/models/qwen3-8b/unload",
                ResponseTemplate::new(200),
            )],
        )
        .await;
        let adapter = adapter_for(&server).await;
        adapter
            .unload("qwen3-8b")
            .await
            .expect("unload must succeed");
    }

    /// Regression guard: a hyphen (common in real model ids, e.g.
    /// `qwen3-8b`) must NOT be escaped, but a character that would
    /// actually break the path (`/`) still must be — caught by the
    /// previous PR's own tests initially 404ing against a hyphenated id
    /// before the `PATH_SEGMENT` fix (plain `NON_ALPHANUMERIC` over-escapes `-`).
    #[tokio::test]
    async fn model_id_encoding_leaves_hyphens_alone_but_escapes_slashes() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(
                LOAD_METHOD,
                "/v1/models/weird%2Fid/unload",
                ResponseTemplate::new(200),
            )],
        )
        .await;
        let adapter = adapter_for(&server).await;
        adapter
            .unload("weird/id")
            .await
            .expect("a '/' in the id must be percent-encoded, not split the path");
    }

    #[tokio::test]
    async fn probe_ready_is_false_while_still_loading() {
        let server = MockServer::start().await;
        let resp = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": false, "pinned": false}]
        }));
        mount_all(&server, vec![("GET", VERIFY_PATH, resp)]).await;
        let adapter = adapter_for(&server).await;
        assert!(
            !adapter
                .probe_ready("qwen3-8b")
                .await
                .expect("must not error")
        );
    }

    #[tokio::test]
    async fn probe_ready_is_true_once_loaded() {
        let server = MockServer::start().await;
        let resp = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": true, "pinned": false}]
        }));
        mount_all(&server, vec![("GET", VERIFY_PATH, resp)]).await;
        let adapter = adapter_for(&server).await;
        assert!(
            adapter
                .probe_ready("qwen3-8b")
                .await
                .expect("must not error")
        );
    }

    /// Not-yet-registered id, a malformed response, and an unreachable
    /// server are all "can't tell yet" — none of them may surface as
    /// `Err`, which the Supervisor would treat as a terminal load failure
    /// rather than "poll again".
    #[tokio::test]
    async fn probe_ready_never_errors_on_missing_entry_malformed_body_or_5xx() {
        for resp in [
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            ResponseTemplate::new(200).set_body_string("not json"),
            ResponseTemplate::new(500),
        ] {
            let server = MockServer::start().await;
            mount_all(&server, vec![("GET", VERIFY_PATH, resp)]).await;
            let adapter = adapter_for(&server).await;
            assert_eq!(
                adapter.probe_ready("qwen3-8b").await,
                Ok(false),
                "must report not-ready, never Err, while still possibly loading"
            );
        }
    }

    fn chat_request() -> ChatRequest {
        ChatRequest {
            model: "qwen3-8b".to_string(),
            messages: vec![idoris_backend::ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
        }
    }

    #[tokio::test]
    async fn chat_returns_the_completion_content() {
        let server = MockServer::start().await;
        let resp = ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "hello there"}}]
        }));
        mount_all(&server, vec![("POST", "/v1/chat/completions", resp)]).await;
        let adapter = adapter_for(&server).await;
        let out = adapter
            .chat(chat_request(), CancellationToken::new())
            .await
            .expect("chat must succeed");
        assert_eq!(out.model, "qwen3-8b");
        assert_eq!(out.content, "hello there");
    }

    /// Matches the TS reference's own leniency: a response missing the
    /// `choices`/`message`/`content` path is a valid (if unhelpful) empty
    /// completion, not a parse failure.
    #[tokio::test]
    async fn chat_defaults_to_empty_content_when_choices_are_missing() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(
                "POST",
                "/v1/chat/completions",
                ResponseTemplate::new(200).set_body_json(serde_json::json!({})),
            )],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let out = adapter
            .chat(chat_request(), CancellationToken::new())
            .await
            .expect("must still succeed");
        assert_eq!(out.content, "");
    }

    #[tokio::test]
    async fn chat_fails_immediately_on_an_already_cancelled_token() {
        let server = MockServer::start().await;
        let adapter = adapter_for(&server).await;
        let token = CancellationToken::new();
        token.cancel();
        let err = adapter
            .chat(chat_request(), token)
            .await
            .expect_err("a pre-cancelled token must short-circuit chat");
        assert_eq!(err.reason_code(), "cancelled");
    }

    #[tokio::test]
    async fn chat_is_cancelled_mid_flight_instead_of_waiting_for_a_slow_response() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(
                "POST",
                "/v1/chat/completions",
                ResponseTemplate::new(200).set_delay(Duration::from_secs(5)),
            )],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let token = CancellationToken::new();
        let child = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            child.cancel();
        });
        let err = tokio::time::timeout(Duration::from_secs(1), adapter.chat(chat_request(), token))
            .await
            .expect("select! must not itself hang waiting on the slow response")
            .expect_err("must report cancellation, not wait out the 5s delay");
        assert_eq!(err.reason_code(), "cancelled");
    }

    #[tokio::test]
    async fn chat_on_4xx_reports_upstream_error_without_leaking_the_api_key() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(
                "POST",
                "/v1/chat/completions",
                ResponseTemplate::new(400).set_body_string("bad-request-body-should-not-leak"),
            )],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let err = adapter
            .chat(chat_request(), CancellationToken::new())
            .await
            .expect_err("4xx must fail");
        let msg = err.to_string();
        assert!(msg.contains("400"));
        assert!(!msg.contains("bad-request-body-should-not-leak"));
        assert!(!msg.contains("test-key-should-never-leak"));
    }

    /// `OmlxAdapter` must be usable as `Box<dyn RuntimeAdapter>` — the
    /// whole point of the trait impl added in this PR — and the trait
    /// method must produce the same result as calling the inherent method
    /// directly (they delegate to the same code).
    #[tokio::test]
    async fn omlx_adapter_is_usable_as_a_dyn_runtime_adapter() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![(
                "GET",
                "/v1/models",
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "data": [{"id": "qwen3-8b"}]
                })),
            )],
        )
        .await;
        let adapter: Box<dyn RuntimeAdapter> = Box::new(adapter_for(&server).await);
        let models = adapter
            .list()
            .await
            .expect("list via trait object must succeed");
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "qwen3-8b");
    }
}
