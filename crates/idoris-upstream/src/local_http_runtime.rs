//! Single-model process-owned OpenAI-compatible local HTTP runtime.
//!
//! mlx_lm.server and llama.cpp differ in launch argv (owned by
//! LocalRuntimeLaunch) but share the small HTTP surface this adapter needs:
//! GET /v1/models for readiness and POST /v1/chat/completions for inference.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use idoris_backend::{
    BackendError, BackendStatus, ChatRequest, ChatResponse, ModelInfo, Pressure, RuntimeAdapter,
};
use idoris_contracts::LoadPolicy;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::{LocalRuntimeLaunch, ManagedRuntimeProcess};

const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(10);
const DEFAULT_SHUTDOWN_GRACE: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub struct LocalHttpRuntimeConfig {
    pub launch: LocalRuntimeLaunch,
    pub model_id: String,
    pub memory_gb: f64,
    pub load_fence_path: PathBuf,
    pub call_timeout: Duration,
    pub shutdown_grace: Duration,
}

impl LocalHttpRuntimeConfig {
    pub fn new(
        launch: LocalRuntimeLaunch,
        model_id: impl Into<String>,
        memory_gb: f64,
        load_fence_path: PathBuf,
    ) -> Result<Self, BackendError> {
        let model_id = model_id.into();
        if model_id.trim().is_empty() {
            return Err(BackendError::internal("local runtime model id is empty"));
        }
        if !memory_gb.is_finite() || memory_gb <= 0.0 {
            return Err(BackendError::invalid_request(
                "local runtime memory_gb must be finite and > 0",
            ));
        }
        if !load_fence_path.is_absolute() {
            return Err(BackendError::internal(
                "local runtime load fence path must be absolute",
            ));
        }
        Ok(Self {
            launch,
            model_id,
            memory_gb,
            load_fence_path,
            call_timeout: DEFAULT_CALL_TIMEOUT,
            shutdown_grace: DEFAULT_SHUTDOWN_GRACE,
        })
    }
}

pub struct LocalHttpRuntimeAdapter {
    config: LocalHttpRuntimeConfig,
    client: reqwest::Client,
    process: Mutex<Option<ManagedRuntimeProcess>>,
}

impl LocalHttpRuntimeAdapter {
    pub fn new(config: LocalHttpRuntimeConfig) -> Result<Self, BackendError> {
        if config.call_timeout.is_zero() || config.shutdown_grace.is_zero() {
            return Err(BackendError::internal(
                "local runtime timeouts must be non-zero",
            ));
        }
        let client = crate::http_client()
            .map_err(|_| BackendError::internal("failed to build local runtime HTTP client"))?;
        Ok(Self {
            config,
            client,
            process: Mutex::new(None),
        })
    }

    fn ensure_id(&self, id: &str) -> Result<(), BackendError> {
        if id == self.config.model_id {
            Ok(())
        } else {
            Err(BackendError::model_not_found(id))
        }
    }

    async fn process_running(&self) -> Result<bool, BackendError> {
        let stale = {
            let mut guard = self.process.lock().await;
            let Some(process) = guard.as_mut() else {
                return Ok(false);
            };
            if process.is_running()? {
                return Ok(true);
            }
            guard.take()
        };
        if let Some(process) = stale {
            process.shutdown(self.config.shutdown_grace).await?;
        }
        Ok(false)
    }

    async fn stop_process(&self) -> Result<(), BackendError> {
        let process = self.process.lock().await.take();
        match process {
            Some(process) => process.shutdown(self.config.shutdown_grace).await,
            None => Ok(()),
        }
    }

    async fn probe_models_endpoint(&self) -> Result<bool, BackendError> {
        let url = format!("{}/v1/models", self.config.launch.endpoint());
        let attempt = async {
            let response = match self.client.get(url).send().await {
                Ok(response) => response,
                Err(_) => return Ok(false),
            };
            if response.status().is_server_error() {
                return Ok(false);
            }
            if !response.status().is_success() {
                return Err(BackendError::Upstream {
                    message: format!(
                        "local runtime GET /v1/models returned HTTP {}",
                        response.status().as_u16()
                    ),
                });
            }
            let body = match response.json::<serde_json::Value>().await {
                Ok(body) => body,
                Err(_) => return Ok(false),
            };
            Ok(body
                .get("data")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|models| !models.is_empty()))
        };
        tokio::time::timeout(self.config.call_timeout, attempt)
            .await
            .unwrap_or(Ok(false))
    }

    async fn chat_inner(&self, req: ChatRequest) -> Result<ChatResponse, BackendError> {
        if !self.probe_ready(&req.model).await? {
            return Err(BackendError::model_unavailable(&req.model));
        }
        let url = format!("{}/v1/chat/completions", self.config.launch.endpoint());
        let messages = req
            .messages
            .iter()
            .map(|message| {
                serde_json::json!({
                    "role": message.role,
                    "content": message.content,
                })
            })
            .collect::<Vec<_>>();
        let body = serde_json::json!({"model": req.model, "messages": messages});
        let attempt = async {
            let response = self
                .client
                .post(url)
                .json(&body)
                .send()
                .await
                .map_err(|_| BackendError::Upstream {
                    message: "local runtime chat transport failed".into(),
                })?;
            if !response.status().is_success() {
                return Err(BackendError::Upstream {
                    message: format!(
                        "local runtime POST /v1/chat/completions returned HTTP {}",
                        response.status().as_u16()
                    ),
                });
            }
            response
                .json::<serde_json::Value>()
                .await
                .map_err(|_| BackendError::Upstream {
                    message: "local runtime chat response was not valid JSON".into(),
                })
        };
        let raw = tokio::time::timeout(self.config.call_timeout, attempt)
            .await
            .map_err(|_| BackendError::Upstream {
                message: "local runtime chat timed out".into(),
            })??;
        let content = raw
            .pointer("/choices/0/message/content")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| BackendError::Upstream {
                message: "local runtime response missing completion content".into(),
            })?
            .to_string();
        Ok(ChatResponse {
            model: self.config.model_id.clone(),
            content,
        })
    }
}

#[async_trait]
impl RuntimeAdapter for LocalHttpRuntimeAdapter {
    fn load_fence_path(&self) -> Result<PathBuf, BackendError> {
        Ok(self.config.load_fence_path.clone())
    }

    async fn list(&self) -> Result<Vec<ModelInfo>, BackendError> {
        Ok(vec![ModelInfo {
            id: self.config.model_id.clone(),
            memory_gb: self.config.memory_gb,
        }])
    }

    async fn load(&self, id: &str, _policy: Option<&LoadPolicy>) -> Result<(), BackendError> {
        self.ensure_id(id)?;
        let stale = {
            let mut guard = self.process.lock().await;
            if let Some(process) = guard.as_mut()
                && process.is_running()?
            {
                return Ok(());
            }
            guard.take()
        };
        if let Some(process) = stale {
            process.shutdown(self.config.shutdown_grace).await?;
        }
        *self.process.lock().await = Some(ManagedRuntimeProcess::spawn(&self.config.launch)?);
        Ok(())
    }

    async fn unload(&self, id: &str) -> Result<(), BackendError> {
        self.ensure_id(id)?;
        self.stop_process().await
    }

    async fn status(&self) -> Result<BackendStatus, BackendError> {
        let running = self.process_running().await?;
        let loaded = if running {
            vec![self.config.model_id.clone()]
        } else {
            Vec::new()
        };
        Ok(BackendStatus {
            pressure: Pressure::Unknown,
            used_gb: if running { self.config.memory_gb } else { 0.0 },
            model_memory_max_gb: self.config.memory_gb,
            loaded,
        })
    }

    async fn probe_ready(&self, id: &str) -> Result<bool, BackendError> {
        self.ensure_id(id)?;
        if !self.process_running().await? {
            return Ok(false);
        }
        self.probe_models_endpoint().await
    }

    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
    ) -> Result<ChatResponse, BackendError> {
        self.ensure_id(&req.model)?;
        if cancel.is_cancelled() {
            let _ = self.stop_process().await;
            return Err(BackendError::cancelled());
        }
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = self.stop_process().await;
                Err(BackendError::cancelled())
            }
            result = self.chat_inner(req) => result,
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::ffi::OsString;
    use std::path::Path;

    use idoris_backend::{ChatMessage, RuntimeAdapter};
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::LocalRuntimeKind;

    fn config(server: &MockServer) -> LocalHttpRuntimeConfig {
        let launch = LocalRuntimeLaunch::new(
            LocalRuntimeKind::MlxLmServer,
            "/usr/bin/false",
            "/models/qwen-mlx",
            server.address().port(),
        )
        .unwrap();
        LocalHttpRuntimeConfig::new(
            launch,
            "single-model",
            2.5,
            std::env::temp_dir().join("idoris-test-load.pending"),
        )
        .unwrap()
    }

    #[cfg(unix)]
    async fn with_running_process(server: &MockServer) -> LocalHttpRuntimeAdapter {
        let adapter = LocalHttpRuntimeAdapter::new(config(server)).unwrap();
        let process =
            ManagedRuntimeProcess::spawn_test(Path::new("/bin/sleep"), vec![OsString::from("30")])
                .unwrap();
        *adapter.process.lock().await = Some(process);
        adapter
    }

    #[tokio::test]
    async fn stopped_adapter_lists_only_its_configured_model() {
        let server = MockServer::start().await;
        let adapter = LocalHttpRuntimeAdapter::new(config(&server)).unwrap();
        assert_eq!(
            adapter.list().await.unwrap(),
            vec![ModelInfo {
                id: "single-model".into(),
                memory_gb: 2.5,
            }]
        );
        let status = adapter.status().await.unwrap();
        assert_eq!(status.pressure, Pressure::Unknown);
        assert_eq!(status.used_gb, 0.0);
        assert!(status.loaded.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ready_single_model_forwards_openai_chat_and_unloads_process() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "engine-specific-name"}]
            })))
            .expect(2)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_json(serde_json::json!({
                "model": "single-model",
                "messages": [{"role": "user", "content": "hello"}]
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "world"}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let adapter = with_running_process(&server).await;
        assert!(adapter.probe_ready("single-model").await.unwrap());
        let response = adapter
            .chat(
                ChatRequest {
                    model: "single-model".into(),
                    messages: vec![ChatMessage {
                        role: "user".into(),
                        content: "hello".into(),
                    }],
                },
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(response.content, "world");
        adapter.unload("single-model").await.unwrap();
        assert!(!adapter.process_running().await.unwrap());
        server.verify().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn readiness_is_fail_closed_and_unknown_ids_are_terminal() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({})))
            .expect(1)
            .mount(&server)
            .await;
        let adapter = with_running_process(&server).await;
        let status = adapter.status().await.unwrap();
        assert_eq!(status.loaded, vec!["single-model"]);
        assert_eq!(status.used_gb, 2.5);
        assert!(!adapter.probe_ready("single-model").await.unwrap());
        assert!(matches!(
            adapter.probe_ready("other").await,
            Err(BackendError::ModelNotFound { .. })
        ));
        adapter.unload("single-model").await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn missing_completion_content_fails_closed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "single-model"}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {}}]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let adapter = with_running_process(&server).await;
        let error = adapter
            .chat(
                ChatRequest {
                    model: "single-model".into(),
                    messages: vec![],
                },
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert_eq!(error.reason_code(), "upstream_error");
        adapter.unload("single-model").await.unwrap();
        server.verify().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn chat_cancellation_terminates_the_owned_runtime_process() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": [{"id": "anything"}]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(Duration::from_secs(5))
                    .set_body_json(serde_json::json!({"choices": []})),
            )
            .mount(&server)
            .await;
        let adapter = with_running_process(&server).await;
        let cancel = CancellationToken::new();
        let cancel_later = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            cancel_later.cancel();
        });
        let error = adapter
            .chat(
                ChatRequest {
                    model: "single-model".into(),
                    messages: vec![],
                },
                cancel,
            )
            .await
            .unwrap_err();
        assert!(matches!(error, BackendError::Cancelled));
        assert!(!adapter.process_running().await.unwrap());
    }
}
