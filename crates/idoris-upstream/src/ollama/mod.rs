//! Ollama [`RuntimeAdapter`](idoris_backend::RuntimeAdapter).
//!
//! `list` reads `GET /api/tags`. `status` and readiness read `GET /api/ps`.
//! `load` and `unload` post `POST /api/generate` with an empty prompt.
//! There is no `/api/pull` and no rerank endpoint.

mod http;
mod residency;
mod status;

use std::time::Duration;

use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, ModelInfo, RuntimeAdapter,
};
use idoris_contracts::LoadPolicy;
use idoris_contracts::common::Capability;
use percent_encoding::{NON_ALPHANUMERIC, utf8_percent_encode};
use tokio_util::sync::CancellationToken;

use residency::{idle_residency, keep_alive_seconds};

pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);

/// Chat only. Ollama has no rerank endpoint, so [`Capability::Rerank`] is absent.
pub const CAPABILITIES: &[Capability] = &[Capability::Chat];

pub(crate) fn upstream_error(message: impl Into<String>) -> BackendError {
    BackendError::Upstream {
        message: message.into(),
    }
}

#[derive(Debug, Clone)]
pub struct OllamaAdapterConfig {
    pub base_url: String,
    pub call_timeout: Duration,
}

impl Default for OllamaAdapterConfig {
    fn default() -> Self {
        Self {
            base_url: DEFAULT_BASE_URL.to_string(),
            call_timeout: DEFAULT_CALL_TIMEOUT,
        }
    }
}

pub struct OllamaAdapter {
    base_url: String,
    call_timeout: Duration,
    client: reqwest::Client,
    load_fence_path: Option<std::path::PathBuf>,
}

impl OllamaAdapter {
    pub fn new(config: OllamaAdapterConfig) -> Result<Self, BackendError> {
        if config.call_timeout.is_zero() {
            return Err(BackendError::invalid_request(
                "Ollama call timeout must be non-zero",
            ));
        }
        let base_url = config.base_url.trim().trim_end_matches('/').to_string();
        if reqwest::Url::parse(&base_url).is_err() || base_url.is_empty() {
            return Err(BackendError::invalid_request(
                "Ollama endpoint must be an absolute URL",
            ));
        }
        let client = crate::http_client()
            .map_err(|_| BackendError::internal("failed to build the Ollama HTTP client"))?;
        Ok(Self {
            base_url,
            call_timeout: config.call_timeout,
            client,
            load_fence_path: None,
        })
    }

    pub fn with_load_fence_path(mut self, path: std::path::PathBuf) -> Self {
        self.load_fence_path = Some(path);
        self
    }

    async fn mutate(&self, id: &str, body: &serde_json::Value) -> Result<(), BackendError> {
        match http::post_generate(&self.client, &self.base_url, body, self.call_timeout, id).await {
            Ok(()) => Ok(()),
            Err(http::MutationFailure::Definite(err)) => Err(err),
            Err(http::MutationFailure::Unconfirmed(err)) => {
                Err(BackendError::load_unconfirmed(id, err.to_string()))
            }
        }
    }

    async fn ps(&self) -> Result<Vec<status::RunningModel>, BackendError> {
        let raw = self.read_json("/api/ps").await?;
        status::parse_ps(&raw)
    }

    async fn read_json(&self, path: &str) -> Result<serde_json::Value, BackendError> {
        match http::get_json(&self.client, &self.base_url, path, self.call_timeout).await {
            Ok(body) => Ok(body),
            Err(http::GetFailure::Terminal(err)) => Err(err),
            Err(http::GetFailure::Transient) => Err(upstream_error(format!(
                "Ollama GET {path} failed before a usable response"
            ))),
        }
    }
}

fn ensure_id(id: &str) -> Result<(), BackendError> {
    if id.is_empty() {
        Err(BackendError::invalid_request("Ollama model id is empty"))
    } else {
        Ok(())
    }
}

fn generate_body(model: &str, keep_alive: Option<u64>) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": model,
        "prompt": "",
        "stream": false,
    });
    if let Some(seconds) = keep_alive {
        body["keep_alive"] = serde_json::json!(seconds);
    }
    body
}

fn completion_content(raw: &serde_json::Value) -> String {
    raw.pointer("/choices/0/message/content")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("")
        .to_string()
}

#[async_trait::async_trait]
impl RuntimeAdapter for OllamaAdapter {
    fn load_fence_path(&self) -> Result<std::path::PathBuf, BackendError> {
        if let Some(path) = &self.load_fence_path {
            if path.is_absolute() {
                return Ok(path.clone());
            }
            return Err(BackendError::invalid_request(
                "Ollama load fence path must be absolute",
            ));
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
            .map_err(|_| BackendError::internal("invalid Ollama endpoint for load fence"))?;
        let _ = endpoint.set_username("");
        let _ = endpoint.set_password(None);
        endpoint.set_fragment(None);
        let key = utf8_percent_encode(endpoint.as_str().trim_end_matches('/'), NON_ALPHANUMERIC);
        Ok(root
            .join("ollama")
            .join(key.to_string())
            .join("load.pending"))
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        status::parse_tags(&self.read_json("/api/tags").await?)
    }

    async fn load(&self, id: &str, policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        ensure_id(id)?;
        let residency = idle_residency(policy)?;
        self.mutate(id, &generate_body(id, keep_alive_seconds(residency)))
            .await
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        ensure_id(id)?;
        match self.mutate(id, &generate_body(id, Some(0))).await {
            Ok(()) => {}
            Err(err) if err.reason_code() == "model_not_found" => {}
            Err(err) => return Err(err),
        }
        let running = match self.ps().await {
            Ok(running) => running,
            Err(err) => return Err(BackendError::load_unconfirmed(id, err.to_string())),
        };
        if status::contains_id(&running, id) {
            Err(BackendError::load_unconfirmed(
                id,
                format!("Ollama GET /api/ps still lists {id} after keep_alive 0"),
            ))
        } else {
            Ok(())
        }
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        status::status_from_ps(&self.ps().await?)
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        ensure_id(id)?;
        match http::get_json(&self.client, &self.base_url, "/api/ps", self.call_timeout).await {
            Ok(raw) => match status::parse_ps(&raw) {
                Ok(running) => Ok(status::contains_id(&running, id)),
                Err(_) => Ok(false),
            },
            Err(http::GetFailure::Transient) => Ok(false),
            Err(http::GetFailure::Terminal(err)) => Err(err),
        }
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        if cancel.is_cancelled() {
            return Err(BackendError::cancelled());
        }
        ensure_id(&req.model)?;
        let running = self.ps().await?;
        if !status::contains_id(&running, &req.model) {
            return Err(BackendError::model_unavailable(&req.model));
        }
        let messages = req
            .messages
            .iter()
            .map(|message| serde_json::json!({"role": message.role, "content": message.content}))
            .collect::<Vec<_>>();
        let body = serde_json::json!({"model": req.model, "messages": messages});
        let call = http::post_chat(&self.client, &self.base_url, &body, self.call_timeout);
        let raw = tokio::select! {
            _ = cancel.cancelled() => return Err(BackendError::cancelled()),
            result = call => result?,
        };
        Ok(ChatResponse {
            model: req.model,
            content: completion_content(&raw),
        })
    }
}
