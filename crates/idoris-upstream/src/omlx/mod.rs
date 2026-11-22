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
//!
//! [`engine`] is oMLX as an *engine*: the process-level side (find the executable, how to launch
//! it, how to tell it is ready), written against the engine-agnostic layer in
//! [`crate::engine`] (IDORIS-CORE S3). `OmlxAdapter` itself is unchanged; it keeps talking HTTP.

mod admin;
#[cfg(test)]
mod admin_tests;
#[cfg(unix)]
pub mod engine;
mod http;
pub mod key;
mod pin;
mod status;

use std::time::Duration;

use admin::AdminRequestError;

use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, LOAD_CONFIRMATION_FLOOR, ModelInfo,
    RuntimeAdapter,
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

/// Default deadline of one whole load (IDORIS-CORE S8, interface spec §2.11.2): the `POST /v1/models/{id}/load`
/// **and** the pin and state verification that follow it, which must all finish inside it (PR-Daemon's #634
/// review, W-3; the confirmation is given at least [`LOAD_CONFIRMATION_FLOOR`], so a load accepted
/// at the very end of the budget can still be confirmed). Loading a model is not a management call: Qwen3-8B takes about 4 s cold
/// (`spike/u0/U0-LOG.md`) and the 27B the 48 GiB+ tier recommends is expected to take much longer, but under
/// [`DEFAULT_CALL_TIMEOUT`] such a load would be reported as an unknown outcome and leave the durable
/// fence behind — exactly what a first-start warmup of a big model must not do. The adapter only uses
/// this once [`OmlxAdapter::with_load_timeout`] opts in (the production factory does); without it a
/// load keeps [`OmlxAdapterConfig::call_timeout`] as its deadline, as before. `status`, `unload`,
/// `probe_ready` and `list` always keep `call_timeout`.
pub const DEFAULT_LOAD_TIMEOUT: Duration = Duration::from_secs(180);

/// Environment variable that overrides [`DEFAULT_LOAD_TIMEOUT`] for the production adapter
/// (`factory::create_adapter`): a whole number of seconds, [`MIN_LOAD_TIMEOUT_SECS`] to
/// [`MAX_LOAD_TIMEOUT_SECS`]. Unset or blank means the default; anything else that does not parse is an
/// error at construction (start-up), never a silent fall-back to the default.
pub const LOAD_TIMEOUT_ENV: &str = "IDORIS_LOAD_TIMEOUT_S";
/// Lower bound of [`LOAD_TIMEOUT_ENV`].
pub const MIN_LOAD_TIMEOUT_SECS: u64 = 1;
/// Upper bound of [`LOAD_TIMEOUT_ENV`]: one load holds the Supervisor's single load/evict mutex for as
/// long as it runs, so a typo must not be able to hold it for days.
pub const MAX_LOAD_TIMEOUT_SECS: u64 = 3600;

/// What the confirmation after an accepted load (pin and state verification) may still spend, given the
/// load `budget`, how long the `POST` has `spent`, and the confirmation `floor` (the adapter's
/// [`LOAD_CONFIRMATION_FLOOR`], or its `call_timeout` if that is shorter) — local review of #634, L-5:
///
/// - `None` when the request itself used the whole budget: nothing is sent after that, the outcome is
///   already unknown;
/// - otherwise what the budget has left, but never less than `floor`: a load the engine accepted with
///   100 ms to spare must still get a fair chance to be confirmed, instead of being reported unknown —
///   fence retained — although the model is loaded.
///
/// The whole `load` therefore takes at most `budget + floor`, and the floor is never above
/// [`LOAD_CONFIRMATION_FLOOR`], which the Supervisor's backstop accounts for.
pub(crate) fn confirmation_allowance(
    budget: Duration,
    spent: Duration,
    floor: Duration,
) -> Option<Duration> {
    let remaining = budget.saturating_sub(spent);
    if remaining.is_zero() {
        return None;
    }
    Some(remaining.max(floor))
}

/// Parses the raw value of [`LOAD_TIMEOUT_ENV`] (`None` = not set).
pub fn load_timeout_from_env(raw: Option<&std::ffi::OsStr>) -> Result<Duration, BackendError> {
    let Some(raw) = raw else {
        return Ok(DEFAULT_LOAD_TIMEOUT);
    };
    let invalid = || {
        BackendError::invalid_request(format!(
            "{LOAD_TIMEOUT_ENV} 不是合法的加载超时：请设置为 {MIN_LOAD_TIMEOUT_SECS} 到 {MAX_LOAD_TIMEOUT_SECS} 之间的整数秒，或不设置以使用默认值 {} 秒",
            DEFAULT_LOAD_TIMEOUT.as_secs()
        ))
    };
    let text = raw.to_str().ok_or_else(invalid)?.trim();
    if text.is_empty() {
        return Ok(DEFAULT_LOAD_TIMEOUT);
    }
    if !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(invalid());
    }
    let seconds: u64 = text.parse().map_err(|_| invalid())?;
    if !(MIN_LOAD_TIMEOUT_SECS..=MAX_LOAD_TIMEOUT_SECS).contains(&seconds) {
        return Err(invalid());
    }
    Ok(Duration::from_secs(seconds))
}

/// Default deadline for `POST /v1/chat/completions` only. Generation can
/// legitimately take far longer than a management call, so chat is bounded
/// separately (finite, never unlimited); load/status/pin keep
/// [`DEFAULT_CALL_TIMEOUT`].
pub const DEFAULT_CHAT_TIMEOUT: Duration = Duration::from_secs(120);

/// Env var oMLX's inference API key is read from. Never logged, never put
/// in an error message.
pub const OMLX_API_KEY_ENV: &str = "IDORIS_OMLX_API_KEY";

/// Where the durable load fence of the engine at `base_url` lives under the state directory `root`:
/// `<root>/omlx/<the endpoint, percent-encoded as a whole>/load.pending`. [`OmlxAdapter`]'s fence path is this
/// (with `root` resolved from `IDORIS_STATE_DIR` / `HOME`); the engine manager clears the very same file when it
/// has just spawned (or killed and respawned) an engine it owns, so both sides must agree on the path: there is
/// one implementation, here. Credentials and fragments in the URL do not identify a different engine.
pub fn load_fence_path_in(
    root: &std::path::Path,
    base_url: &str,
) -> Result<std::path::PathBuf, BackendError> {
    let mut endpoint = reqwest::Url::parse(base_url)
        .map_err(|_| BackendError::internal("invalid oMLX endpoint for load fence"))?;
    let _ = endpoint.set_username("");
    let _ = endpoint.set_password(None);
    endpoint.set_fragment(None);
    let key = utf8_percent_encode(endpoint.as_str().trim_end_matches('/'), NON_ALPHANUMERIC);
    Ok(root.join("omlx").join(key.to_string()).join("load.pending"))
}

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

impl OmlxAdapterConfig {
    /// Default config pointed at `base_url`; the settings.json key is attached
    /// only when `base_url` is loopback (the env key keeps its old behaviour).
    pub fn for_base_url(base_url: impl Into<String>) -> Self {
        let base_url = base_url.into();
        Self {
            api_key: key::resolve_omlx_api_key_for(&base_url),
            base_url,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

impl Default for OmlxAdapterConfig {
    /// Resolves the API key via [`key::resolve_omlx_api_key_for`] (env, then settings.json for loopback only) — the one place this
    /// adapter touches the environment for it; every other constructor
    /// path takes the key as an explicit value instead of reaching for the
    /// environment itself.
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            api_key: key::resolve_omlx_api_key_for(DEFAULT_BASE_URL),
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

pub struct OmlxAdapter {
    base_url: String,
    /// The key given at construction (explicitly, or resolved by [`OmlxAdapterConfig::for_base_url`] at that
    /// moment). Used only while `per_request_key` is off.
    api_key: Option<String>,
    /// Look the key up on **every request** ([`Self::with_per_request_key`]) instead of using `api_key`.
    per_request_key: bool,
    call_timeout: Duration,
    /// Deadline of one whole load (the `POST .../load` and the pin/state verification after it); `None` =
    /// use `call_timeout` for the POST and, as before, `call_timeout` for each later call (see
    /// [`DEFAULT_LOAD_TIMEOUT`]).
    load_timeout: Option<Duration>,
    /// How long the confirmation after an accepted load is given at least, however little of the load budget
    /// the POST left (never above [`LOAD_CONFIRMATION_FLOOR`], and never above `call_timeout`).
    confirmation_floor: Duration,
    chat_timeout: Duration,
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
            per_request_key: false,
            call_timeout: config.call_timeout,
            load_timeout: None,
            confirmation_floor: LOAD_CONFIRMATION_FLOOR,
            chat_timeout: DEFAULT_CHAT_TIMEOUT,
            client,
            admin_session: tokio::sync::Mutex::new(None),
            load_fence_path: None,
        })
    }

    /// Override the chat-only deadline (default [`DEFAULT_CHAT_TIMEOUT`]).
    pub fn with_chat_timeout(mut self, chat_timeout: Duration) -> Self {
        self.chat_timeout = chat_timeout;
        self
    }

    /// Give a load its own deadline, independent of `call_timeout` (IDORIS-CORE S8; production uses
    /// [`DEFAULT_LOAD_TIMEOUT`]). Without this a load is bounded by `call_timeout` like every other call.
    ///
    /// The deadline covers the **whole** load: the `POST /v1/models/{id}/load` and then the pin and state
    /// verification (login, `PUT` settings, `GET` status) that confirm it. Each of those calls still has
    /// its own `call_timeout`, but together with the POST they must fit in this one budget; the follow-up
    /// gets what the POST left over, and at least [`LOAD_CONFIRMATION_FLOOR`] (or `call_timeout`, if that is
    /// shorter) so that a load accepted at the end of the budget can still be confirmed; when the POST used
    /// the whole budget nothing is sent after it. The whole call therefore returns within this budget plus
    /// at most the floor, which the Supervisor's backstop accounts for: the adapter's own answer comes
    /// first, so a load whose outcome is unknown is reported as [`BackendError::LoadUnconfirmed`] and not
    /// as the Supervisor's `adapter_timed_out`. `status`, `unload`, `probe_ready` and `list` stay under
    /// `call_timeout`. A load that outlasts the budget is an unknown outcome
    /// ([`BackendError::LoadUnconfirmed`]) and keeps the durable fence.
    pub fn with_load_timeout(mut self, load_timeout: Duration) -> Self {
        self.load_timeout = Some(load_timeout);
        self
    }

    /// Shorten the confirmation floor (default [`LOAD_CONFIRMATION_FLOOR`], which is also the most it can be:
    /// the Supervisor's backstop adds exactly that). Zero makes the load budget exact. Production keeps the
    /// default; tests use this to keep their timings short.
    pub fn with_confirmation_floor(mut self, floor: Duration) -> Self {
        self.confirmation_floor = floor.min(LOAD_CONFIRMATION_FLOOR);
        self
    }

    /// Override the durable Supervisor load marker location. Every adapter
    /// controlling the same engine must use the same persistent path, including
    /// after a Router restart. A fresh path is only safe for a fresh engine.
    pub fn with_load_fence_path(mut self, path: std::path::PathBuf) -> Self {
        self.load_fence_path = Some(path);
        self
    }

    /// Resolve the key **on every request**, from what the engine manager has registered for this endpoint
    /// *now* (interface spec §2.7.7; IDORIS-CORE S6): an engine iDoris owns gets only its current private
    /// key, an external engine's `settings.json` key goes out only while its listener is verified, and an
    /// endpoint the manager has released (engine stopped, replaced, port changed hands) falls back to the
    /// ordinary order of §2.9. A key read once at construction goes stale in all three cases: after a
    /// restart it is the old engine's private key (every request is a 401), and after the port changed owner
    /// it would still carry the old verification. Construction-time `api_key` is ignored in this mode.
    ///
    /// This is what the engine manager's adapters use (`create_adapter`, `BoundSupervisor::spawn_omlx`);
    /// the proxy and the model listing already resolve per request. An adapter built from an explicit
    /// [`OmlxAdapterConfig`] keeps its fixed key unless this is called.
    pub fn with_per_request_key(mut self) -> Self {
        self.per_request_key = true;
        // Cache no key (IDORIS-CORE S6, Codex audit H2): the adapter reads the endpoint's current authorization, and
        // its generation, right before each send. A key string read at construction is the old engine's the moment
        // the engine is replaced, and must not even be held.
        self.api_key = None;
        self
    }

    /// Test-only: the key string this adapter holds (always `None` in per-request mode).
    #[cfg(test)]
    fn held_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// The credential for the request about to be made — the **one place** this adapter resolves one.
    ///
    /// In per-request mode the request's engine generation (bound by the Supervisor at admission, see
    /// [`idoris_backend::generation`]) is checked against the endpoint's current one in the same read that
    /// yields the key: work admitted for an engine that has since been replaced is **refused**, and so is
    /// anything aimed at a blocked endpoint — nothing is sent, so neither the new engine's credentials nor
    /// the request body (a prompt) can reach a process the work was not admitted for. An adapter with a
    /// fixed key is unchanged.
    fn api_key(&self) -> Result<Option<String>, BackendError> {
        if !self.per_request_key {
            return Ok(self.api_key.clone());
        }
        key::resolve_omlx_api_key_checked(&self.base_url, idoris_backend::pinned_generation())
            .map_err(|refusal| upstream_error(refusal.to_string()))
    }

    // Thin wrappers binding `http`'s free functions to this instance's
    // client/base_url/api_key/call_timeout — every call site below reads
    // as just "GET/POST/PUT `path`" instead of repeating all four fields.
    async fn get(&self, path: &str) -> Result<serde_json::Value, BackendError> {
        let key = self.api_key()?;
        http::get_json(
            &self.client,
            &self.base_url,
            path,
            key.as_deref(),
            self.call_timeout,
        )
        .await
    }

    async fn post(&self, path: &str) -> Result<(), BackendError> {
        let key = self.api_key()?;
        http::post_empty(
            &self.client,
            &self.base_url,
            path,
            key.as_deref(),
            self.call_timeout,
        )
        .await
    }

    async fn post_parse(
        &self,
        path: &str,
        body: &serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, BackendError> {
        let key = self.api_key()?;
        http::post_and_parse(
            &self.client,
            &self.base_url,
            path,
            key.as_deref(),
            timeout,
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
    ///
    /// With a load budget ([`Self::with_load_timeout`]) the whole call — the POST and the confirmation
    /// after it — returns within that budget (plus at most [`LOAD_CONFIRMATION_FLOOR`] when the POST left
    /// less than that for the confirmation), and running out of it is `LoadUnconfirmed` too.
    pub async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        let budget = self.load_timeout.unwrap_or(self.call_timeout);
        let started = tokio::time::Instant::now();
        let key = self.api_key()?;
        http::post_load(
            &self.client,
            &self.base_url,
            &self.encoded_path("/v1/models/", id, "/load"),
            key.as_deref(),
            budget,
            id,
        )
        .await?;
        let confirm = async {
            if policy.map(|p| p.mode) == Some(LoadMode::Resident) {
                self.pin(id).await
            } else {
                self.check_not_unexpectedly_pinned(id).await
            }
        };
        let confirmed = if self.load_timeout.is_some() {
            let floor = self.confirmation_floor.min(self.call_timeout);
            let Some(allowance) = confirmation_allowance(budget, started.elapsed(), floor) else {
                // The POST itself used the whole budget. Pinning and verifying now would send requests after
                // the outcome is already "unknown", so none is sent (the `confirm` future above is never
                // polled).
                return Err(BackendError::load_unconfirmed(
                    id,
                    format!(
                        "oMLX accepted the load of {id}, but the request itself used the whole load budget \
                         of {budget:?}: pinning and verification were not attempted"
                    ),
                ));
            };
            // The follow-up gets what the POST left of the budget, and at least the confirmation floor.
            // Dropping it at the deadline is safe: the outcome is already "unknown", which is what we
            // report (a half-sent admin PUT is exactly the `AdminRequestError::Unknown` case `pin` already
            // treats this way).
            match tokio::time::timeout(allowance, confirm).await {
                Ok(confirmed) => confirmed,
                Err(_) => {
                    return Err(BackendError::load_unconfirmed(
                        id,
                        format!(
                            "oMLX accepted the load of {id}, but pinning and verifying it did not finish \
                             within {allowance:?} (what the request left of the load budget of {budget:?}, \
                             and at least the confirmation floor)"
                        ),
                    ));
                }
            }
        } else {
            confirm.await
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
        let body = crate::native_completion_wire::request_body(&req, &req.model);
        let call = self.post_parse("/v1/chat/completions", &body, self.chat_timeout);
        tokio::select! {
            _ = cancel.cancelled() => Err(BackendError::cancelled()),
            result = call => {
                let raw = result?;
                if req.native.is_some() {
                    crate::native_completion_wire::response(raw)
                } else {
                    Ok(ChatResponse { native: None, model: req.model, content: extract_content(&raw) })
                }
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
    /// In per-request mode: the generation of the engine behind this endpoint *now* (the engine manager
    /// bumps it whenever the credentials it registered for the endpoint change). The Supervisor binds
    /// every admitted operation to it; see [`Self::api_key`].
    fn engine_generation(&self) -> Option<u64> {
        self.per_request_key
            .then(|| key::endpoint_generation(&self.base_url))
    }

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
        load_fence_path_in(&root, &self.base_url)
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Self::list(self).await
    }

    fn load_timeout(&self) -> Option<Duration> {
        self.load_timeout
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

    /// The `Authorization` header of the `index`-th request the mock server saw (`None` = no header).
    async fn authorization_of(server: &MockServer, index: usize) -> Option<String> {
        let requests = server.received_requests().await.expect("recording is on");
        requests[index]
            .headers
            .get("authorization")
            .map(|value| value.to_str().expect("ascii").to_owned())
    }

    async fn models_server() -> MockServer {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!({"models": []})),
            )
            .mount(&server)
            .await;
        server
    }

    /// IDORIS-CORE S6 (G1): the engine manager's adapters look the key up on every request. A key read once at
    /// construction is the old engine's private key after a restart (every request a 401) and still carries the
    /// old verification after the port changed hands.
    #[tokio::test]
    async fn a_per_request_adapter_follows_the_registry_on_every_request_and_a_fixed_one_does_not()
    {
        let server = models_server().await;
        let endpoint = server.uri();
        let config = |key: &str| OmlxAdapterConfig {
            base_url: endpoint.clone(),
            api_key: Some(key.to_owned()),
            call_timeout: Duration::from_millis(500),
        };
        let per_request = OmlxAdapter::new(config("constructed"))
            .unwrap()
            .with_per_request_key();
        let fixed = OmlxAdapter::new(config("constructed")).unwrap();

        // The engine manager registers the engine's private key: the adapter built *before* it existed uses it.
        key::govern_endpoint(&endpoint, key::EndpointKey::Private("first-engine".into()));
        per_request.list().await.unwrap();
        fixed.list().await.unwrap();
        assert_eq!(
            authorization_of(&server, 0).await.as_deref(),
            Some("Bearer first-engine")
        );
        assert_eq!(
            authorization_of(&server, 1).await.as_deref(),
            Some("Bearer constructed"),
            "an adapter built from an explicit config keeps its fixed key"
        );

        // The engine was replaced: a new private key. The same adapter instance sends the new one.
        key::govern_endpoint(&endpoint, key::EndpointKey::Private("second-engine".into()));
        per_request.list().await.unwrap();
        assert_eq!(
            authorization_of(&server, 2).await.as_deref(),
            Some("Bearer second-engine")
        );

        // The manager released the endpoint (engine stopped): the dead engine's key does not go to whatever
        // listens there next, nor does the construction-time key.
        key::release_endpoint(&endpoint);
        per_request.list().await.unwrap();
        let after_release = authorization_of(&server, 3).await;
        assert_ne!(after_release.as_deref(), Some("Bearer second-engine"));
        assert_ne!(after_release.as_deref(), Some("Bearer first-engine"));
        assert_ne!(after_release.as_deref(), Some("Bearer constructed"));
    }

    /// IDORIS-CORE S6 (Codex audit H2b): a per-request adapter holds no key at all, whatever it was constructed with
    /// (a string read at construction is the old engine's after a restart); a fixed adapter keeps its key.
    #[tokio::test]
    async fn a_per_request_adapter_holds_no_key_string() {
        let config = || OmlxAdapterConfig {
            base_url: "http://127.0.0.1:1".to_owned(),
            api_key: Some("read-at-construction".to_owned()),
            call_timeout: Duration::from_millis(500),
        };
        assert_eq!(
            OmlxAdapter::new(config())
                .unwrap()
                .with_per_request_key()
                .held_key(),
            None
        );
        assert_eq!(
            OmlxAdapter::new(config()).unwrap().held_key(),
            Some("read-at-construction")
        );
    }

    /// The registry's verdict decides on every request, in both directions: an external engine whose listener
    /// stopped passing the owner check is no longer sent the `settings.json` key, and a private key takes over
    /// from an unverified external one the moment it is registered.
    #[tokio::test]
    async fn a_per_request_adapter_never_sends_a_key_the_registry_withdrew() {
        let server = models_server().await;
        let endpoint = server.uri();
        let adapter = OmlxAdapter::new(OmlxAdapterConfig {
            base_url: endpoint.clone(),
            api_key: Some("settings-key-read-at-construction".to_owned()),
            call_timeout: Duration::from_millis(500),
        })
        .unwrap()
        .with_per_request_key();

        // Unverified external: whatever else may go out, never the construction-time key.
        key::govern_endpoint(
            &endpoint,
            key::EndpointKey::External {
                settings_key_verified: false,
            },
        );
        adapter.list().await.unwrap();
        assert_ne!(
            authorization_of(&server, 0).await.as_deref(),
            Some("Bearer settings-key-read-at-construction")
        );

        key::govern_endpoint(&endpoint, key::EndpointKey::Private("owned".into()));
        adapter.list().await.unwrap();
        assert_eq!(
            authorization_of(&server, 1).await.as_deref(),
            Some("Bearer owned")
        );
        key::release_endpoint(&endpoint);
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
            native: None,
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
    async fn chat_deadline_is_independent_of_the_management_call_timeout() {
        let server = MockServer::start().await;
        let slow = ResponseTemplate::new(200)
            .set_delay(Duration::from_millis(800))
            .set_body_json(serde_json::json!({"choices": [{"message": {"content": "late"}}]}));
        mount_all(
            &server,
            vec![
                ("POST", "/v1/chat/completions", slow.clone()),
                ("GET", "/v1/models/status", slow),
            ],
        )
        .await;
        // call_timeout is 500ms: a management GET with the same 800ms delay
        // must still time out, while chat (default 120s) must succeed.
        let adapter = adapter_for(&server).await;
        let list_err = adapter.list().await.expect_err("management stays at 500ms");
        assert!(list_err.to_string().contains("timed out"), "{list_err}");
        let reply = adapter
            .chat(chat_request(), CancellationToken::new())
            .await
            .expect("chat must outlive call_timeout");
        assert_eq!(reply.content, "late");
    }

    #[tokio::test]
    async fn default_management_deadline_remains_ten_seconds() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models/status"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(11)))
            .expect(1)
            .mount(&server)
            .await;
        let config = OmlxAdapterConfig {
            base_url: server.uri(),
            ..OmlxAdapterConfig::default()
        };
        assert_eq!(config.call_timeout, Duration::from_secs(10));
        let adapter = OmlxAdapter::new(config).expect("adapter");
        let started = std::time::Instant::now();
        let error = tokio::time::timeout(Duration::from_secs(15), adapter.list())
            .await
            .expect("default management deadline must be bounded")
            .expect_err("eleven-second management response must exceed the ten-second bound");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() >= Duration::from_secs(9));
        assert_eq!(server.received_requests().await.expect("requests").len(), 1);
    }

    #[tokio::test]
    async fn short_chat_timeout_rejects_a_slow_completion() {
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
        let adapter = adapter_for(&server)
            .await
            .with_chat_timeout(Duration::from_millis(100));
        let err = tokio::time::timeout(
            Duration::from_secs(2),
            adapter.chat(chat_request(), CancellationToken::new()),
        )
        .await
        .expect("chat_timeout must fire well before the 5s response")
        .expect_err("must time out");
        assert!(err.to_string().contains("timed out"), "{err}");
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

    /// 确认还能花多少时间（本地预审 L-5）：POST 用掉了整个预算就不发了；否则是预算剩下的，但不少于下限。
    #[test]
    fn the_confirmation_gets_what_the_budget_left_but_never_less_than_the_floor_and_nothing_when_nothing_is_left()
     {
        let s = Duration::from_secs;
        let ms = Duration::from_millis;
        let floor = s(5);
        // 剩得多：就是剩下的。
        assert_eq!(confirmation_allowance(s(180), s(4), floor), Some(s(176)));
        // 剩得比下限少：下限。预算快用完才接受的加载，核对仍有下限那么久。
        assert_eq!(
            confirmation_allowance(s(180), ms(179_900), floor),
            Some(floor)
        );
        assert_eq!(confirmation_allowance(s(3), s(1), floor), Some(floor));
        // 刚好一点也不剩（或已经超了）：没有，什么也不发。
        assert_eq!(confirmation_allowance(s(180), s(180), floor), None);
        assert_eq!(confirmation_allowance(s(180), s(181), floor), None);
        assert_eq!(
            confirmation_allowance(Duration::ZERO, Duration::ZERO, floor),
            None
        );
        // 下限为零：预算本身，一点不多。
        assert_eq!(
            confirmation_allowance(s(3), s(1), Duration::ZERO),
            Some(s(2))
        );
        assert_eq!(confirmation_allowance(s(3), s(3), Duration::ZERO), None);
        // 剩下的刚好一毫秒也还是有，并且拿到下限。
        assert_eq!(confirmation_allowance(s(3), ms(2_999), floor), Some(floor));
    }

    #[test]
    fn the_adapters_floor_is_never_above_the_one_the_supervisors_backstop_accounts_for() {
        let adapter = OmlxAdapter::new(OmlxAdapterConfig {
            base_url: "http://127.0.0.1:1".into(),
            api_key: None,
            call_timeout: DEFAULT_CALL_TIMEOUT,
        })
        .unwrap();
        assert_eq!(adapter.confirmation_floor, LOAD_CONFIRMATION_FLOOR);
        let shorter = adapter.with_confirmation_floor(Duration::from_millis(300));
        assert_eq!(shorter.confirmation_floor, Duration::from_millis(300));
        let capped = shorter.with_confirmation_floor(Duration::from_secs(600));
        assert_eq!(capped.confirmation_floor, LOAD_CONFIRMATION_FLOOR);
    }
}
