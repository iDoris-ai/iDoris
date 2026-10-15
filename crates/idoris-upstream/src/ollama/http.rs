//! HTTP calls for the Ollama adapter. Every call has a timeout. Mutating
//! calls (`POST /api/generate`) map a lost response to an unconfirmed result.
//! Error text is method, path, and status only.

use std::time::Duration;

use idoris_backend::BackendError;

use super::upstream_error;

#[derive(Debug)]
pub(super) enum GetFailure {
    Transient,
    Terminal(BackendError),
}

#[derive(Debug)]
pub(super) enum MutationFailure {
    /// The engine rejected the call before it could change residency.
    Definite(BackendError),
    /// The engine may already have changed residency.
    Unconfirmed(BackendError),
}

pub(super) async fn get_json(
    client: &reqwest::Client,
    base_url: &str,
    path: &str,
    call_timeout: Duration,
) -> Result<serde_json::Value, GetFailure> {
    let request = client.get(format!("{base_url}{path}"));
    let attempt = async {
        let response = request.send().await.map_err(|_| GetFailure::Transient)?;
        let status = response.status();
        if status.is_success() {
            return response.json().await.map_err(|_| {
                GetFailure::Terminal(upstream_error(format!(
                    "Ollama GET {path} returned a body that was not valid JSON"
                )))
            });
        }
        Err(classify_get(path, status))
    };
    match tokio::time::timeout(call_timeout, attempt).await {
        Ok(result) => result,
        Err(_elapsed) => Err(GetFailure::Transient),
    }
}

pub(super) async fn post_generate(
    client: &reqwest::Client,
    base_url: &str,
    body: &serde_json::Value,
    call_timeout: Duration,
    model_id: &str,
) -> Result<(), MutationFailure> {
    let request = client.post(format!("{base_url}/api/generate")).json(body);
    let attempt = async {
        let response = request.send().await.map_err(|err| {
            MutationFailure::Unconfirmed(upstream_error(format!(
                "Ollama POST /api/generate transport failed ({})",
                transport_kind(&err)
            )))
        })?;
        let status = response.status();
        if !status.is_success() {
            return Err(classify_mutation(status, model_id));
        }
        let parsed = response.json::<serde_json::Value>().await.map_err(|_| {
            MutationFailure::Unconfirmed(upstream_error(
                "Ollama POST /api/generate returned a body that was not valid JSON",
            ))
        })?;
        if parsed.get("done").and_then(serde_json::Value::as_bool) == Some(true) {
            Ok(())
        } else {
            Err(MutationFailure::Unconfirmed(upstream_error(
                "Ollama POST /api/generate did not report done=true",
            )))
        }
    };
    match tokio::time::timeout(call_timeout, attempt).await {
        Ok(result) => result,
        Err(_elapsed) => Err(MutationFailure::Unconfirmed(upstream_error(
            "Ollama POST /api/generate timed out",
        ))),
    }
}

pub(super) async fn post_chat(
    client: &reqwest::Client,
    base_url: &str,
    body: &serde_json::Value,
    call_timeout: Duration,
) -> Result<serde_json::Value, BackendError> {
    let request = client
        .post(format!("{base_url}/v1/chat/completions"))
        .json(body);
    let attempt = async {
        let response = request
            .send()
            .await
            .map_err(|_| upstream_error("Ollama POST /v1/chat/completions transport failed"))?;
        let status = response.status();
        if !status.is_success() {
            return Err(upstream_error(format!(
                "Ollama POST /v1/chat/completions failed: HTTP {}",
                status.as_u16()
            )));
        }
        response.json().await.map_err(|_| {
            upstream_error(
                "Ollama POST /v1/chat/completions returned a body that was not valid JSON",
            )
        })
    };
    match tokio::time::timeout(call_timeout, attempt).await {
        Ok(result) => result,
        Err(_elapsed) => Err(upstream_error("Ollama POST /v1/chat/completions timed out")),
    }
}

fn transport_kind(err: &reqwest::Error) -> &'static str {
    if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "could not connect"
    } else {
        "failed"
    }
}

fn classify_get(path: &str, status: reqwest::StatusCode) -> GetFailure {
    if status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
    {
        GetFailure::Transient
    } else {
        GetFailure::Terminal(upstream_error(format!(
            "Ollama GET {path} failed: HTTP {}",
            status.as_u16()
        )))
    }
}

fn classify_mutation(status: reqwest::StatusCode, model_id: &str) -> MutationFailure {
    if status == reqwest::StatusCode::NOT_FOUND {
        MutationFailure::Definite(BackendError::model_not_found(model_id))
    } else if status.is_client_error() && status != reqwest::StatusCode::REQUEST_TIMEOUT {
        MutationFailure::Definite(upstream_error(format!(
            "Ollama POST /api/generate failed: HTTP {}",
            status.as_u16()
        )))
    } else {
        MutationFailure::Unconfirmed(upstream_error(format!(
            "Ollama POST /api/generate failed: HTTP {}",
            status.as_u16()
        )))
    }
}
