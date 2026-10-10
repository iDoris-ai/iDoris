//! oMLX `RuntimeAdapter` — mirrors `packages/adapters/omlx/
//! omlx-backend.ts` on `main` (FU-16) behavior, ported onto
//! `idoris_backend::RuntimeAdapter`. Talks to `http://127.0.0.1:8088` by
//! default. Split across submodules that landed across several PRs on this
//! stack: [`http`] (GET/POST + timeout + safe errors), [`status`]
//! (`list`/`status` parsing), [`pin`] (pin verification), [`admin`]
//! (cached admin session), and
//! [`OmlxAdapter`], which wires all of it into a full
//! [`idoris_backend::RuntimeAdapter`] impl (this PR) — `OmlxAdapter`'s own
//! inherent methods (used directly by this module's tests throughout the
//! stack) and the trait impl are the same logic; the trait impl exists so
//! `OmlxAdapter` can be used as `Box<dyn RuntimeAdapter>` by the
//! Supervisor.
//!
//! **The API key is read from an env var and never logged** — see
//! [`OMLX_API_KEY_ENV`] and `http`'s module doc.

mod admin;
#[cfg(test)]
mod admin_tests;
mod http;
mod pin;
mod status;

use std::time::Duration;

use admin::AdminRequestError;

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

#[derive(Clone)]
pub struct OmlxAdapterConfig {
    pub base_url: String,
    pub api_key: Option<String>,
    pub call_timeout: Duration,
}

impl std::fmt::Debug for OmlxAdapterConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OmlxAdapterConfig")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("call_timeout", &self.call_timeout)
            .finish()
    }
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
    admin_session: tokio::sync::Mutex<Option<reqwest::header::HeaderValue>>,
    load_fence_path: Option<std::path::PathBuf>,
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
            admin_session: tokio::sync::Mutex::new(None),
            load_fence_path: None,
        })
    }

    /// Override the durable Supervisor load marker location. Every adapter
    /// controlling the same engine must use the same persistent path, including
    /// after a Router restart. A fresh path is only safe for a fresh engine.
    pub fn with_load_fence_path(mut self, path: std::path::PathBuf) -> Self {
        self.load_fence_path = Some(path);
        self
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

    /// `GET /v1/models/status` — every model this instance could route to,
    /// with oMLX's own pre-load memory estimate (see `status::parse_list`).
    pub async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        status::parse_list(&self.get("/v1/models/status").await?)
    }

    /// Memory/loaded models from `/api/status`; pressure prefers admin
    /// activity. Missing main key, login rejection or activity failure
    /// preserves the legacy pressure (missing/invalid → `Unknown`).
    pub async fn status(&self) -> Result<BackendStatus, BackendError> {
        let mut result = status::parse_status(&self.get("/api/status").await?)?;
        if let Ok(activity) = self
            .admin_request(reqwest::Method::GET, "/admin/api/activity", None)
            .await
        {
            result.pressure = status::parse_activity_pressure(&activity);
        }
        Ok(result)
    }

    /// `POST /v1/models/{id}/load`, then confirm the resulting pin state
    /// matches `policy` (see [`Self::pin`]/[`Self::check_not_unexpectedly_pinned`]).
    /// A `policy` of `None`, or any mode other than `Resident`, takes the
    /// non-resident path.
    ///
    /// If the `POST` result is unknown, or a later check cannot establish
    /// loaded state, the model may be resident and the error is
    /// [`BackendError::LoadUnconfirmed`]. Once loaded state is confirmed,
    /// deterministic policy failures use `LoadPostconditionFailed`.
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
        confirmed.map_err(|err| match err {
            BackendError::LoadPostconditionFailed { .. } => err,
            other => BackendError::load_unconfirmed(id, other.to_string()),
        })
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
    /// via [`Self::verify_model_state`]. Uses a lazy admin session; only
    /// the main API key can log in (sub keys cannot).
    async fn pin(&self, id: &str) -> Result<(), BackendError> {
        let path = self.encoded_path("/admin/api/models/", id, "/settings");
        let body = serde_json::json!({ "is_pinned": true });
        match self
            .admin_request_classified(reqwest::Method::PUT, &path, Some(&body))
            .await
        {
            Ok(_) => {}
            Err(AdminRequestError::Definite(err)) => {
                self.verify_model_state(id).await.map_err(|verify_err| {
                    upstream_error(format!(
                        "oMLX pin rejection was definite ({err}), \
                         but loaded state could not be verified ({verify_err})"
                    ))
                })?;
                return Err(BackendError::load_postcondition_failed(
                    id,
                    format!("oMLX confirmed {id} loaded, but pin policy was rejected: {err}"),
                ));
            }
            Err(AdminRequestError::Unknown(err)) => {
                return Err(upstream_error(format!(
                    "oMLX pin state unconfirmed for {id}: {err} (admin mutation outcome is unknown)"
                )));
            }
        }
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
            Err(BackendError::load_postcondition_failed(
                id,
                format!(
                    "oMLX pin unavailable for {id}: PUT succeeded but GET \
                 /v1/models/status still reports pinned=false"
                ),
            ))
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
            Err(BackendError::load_postcondition_failed(
                id,
                format!(
                    "oMLX model {id} is unexpectedly pinned: loaded with a \
                 non-resident policy but oMLX reports pinned=true (external \
                 pin state drift)"
                ),
            ))
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
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        if let Some(path) = &self.load_fence_path {
            return Ok(path.clone());
        }
        let root = std::env::var_os("IDORIS_STATE_DIR")
            .map(std::path::PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .map(|home| std::path::PathBuf::from(home).join(".local/state/idoris"))
            })
            .filter(|path| path.is_absolute())
            .ok_or_else(|| {
                BackendError::internal(
                    "a persistent absolute IDORIS_STATE_DIR or HOME is required for the load fence",
                )
            })?;
        let mut endpoint = reqwest::Url::parse(&self.base_url)
            .map_err(|_| BackendError::internal("invalid oMLX endpoint for load fence"))?;
        // Credentials and fragments do not identify a different engine.
        let _ = endpoint.set_username("");
        let _ = endpoint.set_password(None);
        endpoint.set_fragment(None);
        let key = utf8_percent_encode(endpoint.as_str().trim_end_matches('/'), NON_ALPHANUMERIC);
        Ok(root.join("omlx").join(key.to_string()).join("load.pending"))
    }

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

    #[test]
    fn load_fence_identity_survives_adapter_reconstruction() {
        let make = |base_url: &str| {
            OmlxAdapter::new(OmlxAdapterConfig {
                base_url: base_url.into(),
                api_key: None,
                call_timeout: DEFAULT_CALL_TIMEOUT,
            })
            .unwrap()
        };
        let first = make("http://localhost:8088").load_fence_path().unwrap();
        let restarted = make("http://LOCALHOST:8088/").load_fence_path().unwrap();
        assert_eq!(first, restarted);
        assert_ne!(
            first,
            make("http://localhost:8089").load_fence_path().unwrap()
        );
        let explicit = idoris_backend::mock::temporary_load_fence_path();
        assert_eq!(
            make("http://localhost:8088")
                .with_load_fence_path(explicit.clone())
                .load_fence_path()
                .unwrap(),
            explicit
        );
    }

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
    async fn list_returns_truthful_preload_footprints() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "models": [
                    {"id": "qwen3-8b", "estimated_size": 6_u64 * 1024 * 1024 * 1024},
                    {"id": "MarkItDown", "estimated_size": 0}
                ]
            })))
            .mount(&server)
            .await;
        let adapter = adapter_for(&server).await;
        let models = adapter.list().await.expect("list must succeed");
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "qwen3-8b");
        assert_eq!(models[0].memory_gb, 6.0);
        assert_eq!(models[1].memory_gb, 0.0);
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
        if routes.iter().any(|(m, _, _)| *m == "PUT") {
            Mock::given(method("POST"))
                .and(path("/admin/api/login"))
                .respond_with(
                    ResponseTemplate::new(200)
                        .insert_header("set-cookie", "omlx_admin_session=test-session; HttpOnly")
                        .set_body_json(serde_json::json!({"success": true})),
                )
                .mount(server)
                .await;
        }
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
        // A sub key cannot establish the required admin session.
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("POST", "/admin/api/login", ResponseTemplate::new(401)),
                ("GET", VERIFY_PATH, status_mock(false)),
            ],
        )
        .await;
        let adapter = adapter_for(&server).await;
        let err = adapter
            .load("qwen3-8b", Some(&resident_policy()))
            .await
            .expect_err("must fail when the admin session is rejected");
        let msg = err.to_string();
        assert_eq!(err.reason_code(), "load_postcondition_failed");
        assert!(msg.contains("pin policy was rejected") && msg.contains("401"));
        assert!(!msg.contains("test-key-should-never-leak"));
    }

    #[tokio::test]
    async fn deterministic_pin_rejections_require_strict_loaded_confirmation() {
        let rejected_login = ResponseTemplate::new(403);
        let rejected_put = ResponseTemplate::new(404);
        for (case, loaded_status) in [
            ("login 401", status_mock(false)),
            ("login 403", status_mock(true)),
            ("missing key", status_mock(false)),
            ("PUT 404", status_mock(true)),
        ] {
            let server = MockServer::start().await;
            let mut routes = vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("GET", VERIFY_PATH, loaded_status),
            ];
            match case {
                "login 401" => {
                    routes.push(("POST", "/admin/api/login", ResponseTemplate::new(401)))
                }
                "login 403" => routes.push(("POST", "/admin/api/login", rejected_login.clone())),
                "PUT 404" => {
                    routes.push(("PUT", SETTINGS_PATH, rejected_put.clone()));
                }
                _ => {}
            }
            mount_all(&server, routes).await;
            let mut adapter = adapter_for(&server).await;
            if case == "missing key" {
                adapter.api_key = None;
            }
            let err = adapter
                .load("qwen3-8b", Some(&resident_policy()))
                .await
                .unwrap_err();
            assert_eq!(
                err.reason_code(),
                "load_postcondition_failed",
                "{case}: {err}"
            );
        }

        for verification in [
            ResponseTemplate::new(404),
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"models": [{"id":"another-model", "loaded":true, "pinned":false}]})),
            ResponseTemplate::new(200).set_body_json(
                serde_json::json!({"models": [{"id":"qwen3-8b", "loaded":false, "pinned":false}]}),
            ),
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": [
                {"id":"qwen3-8b", "loaded":true, "pinned":false},
                {"id":"qwen3-8b", "loaded":true, "pinned":true}
            ]})),
            ResponseTemplate::new(200).set_body_string("not-json"),
        ] {
            let server = MockServer::start().await;
            mount_all(
                &server,
                vec![
                    (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                    ("POST", "/admin/api/login", ResponseTemplate::new(401)),
                    ("GET", VERIFY_PATH, verification),
                ],
            )
            .await;
            let err = adapter_for(&server)
                .await
                .load("qwen3-8b", Some(&resident_policy()))
                .await
                .unwrap_err();
            assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
        }
    }

    #[tokio::test]
    async fn ambiguous_pin_mutations_stay_unconfirmed_even_if_status_is_loaded() {
        for response in [
            ResponseTemplate::new(408),
            ResponseTemplate::new(500),
            ResponseTemplate::new(200).set_delay(Duration::from_secs(1)),
        ] {
            let server = MockServer::start().await;
            mount_all(
                &server,
                vec![
                    (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                    ("PUT", SETTINGS_PATH, response),
                    ("GET", VERIFY_PATH, status_mock(true)),
                ],
            )
            .await;
            let adapter = adapter_for(&server).await;
            let err = adapter
                .load("qwen3-8b", Some(&resident_policy()))
                .await
                .unwrap_err();
            assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
            let requests = server.received_requests().await.unwrap();
            assert!(
                !requests
                    .iter()
                    .any(|request| request.url.path() == VERIFY_PATH)
            );
        }
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
        assert_eq!(err.reason_code(), "load_postcondition_failed");
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
        assert_eq!(err.reason_code(), "load_postcondition_failed");
    }

    /// Once `POST .../load` has succeeded, unknown verification results
    /// remain unconfirmed; a fully parsed
    /// loaded entry with externally pinned state is a known policy failure.
    #[tokio::test]
    async fn a_failure_after_the_load_post_is_reported_as_unconfirmed() {
        let server = MockServer::start().await;
        mount_all(
            &server,
            vec![
                (LOAD_METHOD, LOAD_PATH, ResponseTemplate::new(200)),
                ("GET", VERIFY_PATH, ResponseTemplate::new(503)),
            ],
        )
        .await;
        let err = adapter_for(&server)
            .await
            .load("qwen3-8b", None)
            .await
            .unwrap_err();
        assert_eq!(err.reason_code(), "load_unconfirmed");
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
            assert_eq!(requests.len(), if pin_timeout { 3 } else { 1 });
            if pin_timeout {
                assert_eq!(requests[1].url.path(), "/admin/api/login");
                assert_eq!(requests[2].url.path(), SETTINGS_PATH);
            }
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
                        b"POST /admin/api/login ",
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
                    if pin_disconnect && index == 1 {
                        let headers = String::from_utf8_lossy(&request);
                        let content_length = headers
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .expect("login content length");
                        let body_start = request.len();
                        while request.len() - body_start < content_length {
                            request.push(socket.read_u8().await.expect("login body"));
                        }
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\nSet-Cookie: omlx_admin_session=test-cookie; Path=/\r\nConnection: close\r\n\r\n{\"success\":true}",
                            )
                            .await
                            .expect("login response");
                    } else if pin_disconnect && index == 0 {
                        socket
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            )
                            .await
                            .expect("load response");
                    }
                    // Close the mutation connection before returning headers.
                }
            });
            let adapter = OmlxAdapter::new(OmlxAdapterConfig {
                base_url: format!("http://{addr}"),
                api_key: Some("test-admin-key".into()),
                call_timeout: Duration::from_secs(1),
            })
            .expect("adapter");
            let policy = pin_disconnect.then(resident_policy);
            let err = adapter
                .load("qwen3-8b", policy.as_ref())
                .await
                .expect_err("connection closed");
            tokio::time::timeout(Duration::from_secs(3), received)
                .await
                .expect("server request sequence timed out")
                .expect("server received the mutation");
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
                "/v1/models/status",
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "models": [{"id": "qwen3-8b", "estimated_size": 1024_u64 * 1024 * 1024}]
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
