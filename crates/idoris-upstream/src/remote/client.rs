//! [`RemoteClient`] — a [`RemoteChat`] implementation for
//! OpenAI/Anthropic-compatible HTTP endpoints, built on the `genai` crate.
//!
//! **Deadline**: every call enforces `deadline` via `tokio::time::timeout`
//! around the whole `genai` call (analogous to `omlx::http`'s
//! `send_and_parse` — the timeout must cover the response body read, not
//! just connect/send).
//!
//! **Credentials**: resolved per call through a [`CredentialSource`]
//! (never read from the environment directly by this module) and bridged
//! into `genai`'s own `AuthResolver` — see [`RemoteClient::new`].
//!
//! **De-association**: this client sends no user- or tenant-identifying
//! data to the upstream provider — only `model` and `messages` from the
//! caller's [`ChatRequest`], nothing this crate adds itself (no tenant id,
//! no request id, no `X-*` tracking headers).
//!
//! **No response bodies or raw `genai::Error` text in errors/logs**:
//! [`map_genai_error`] never calls `Display`/`Debug` on the pieces of a
//! `genai::Error` that can carry response content (`webc::Error`'s
//! `ResponseFailedStatus.body`, `ChatResponseGeneration`'s captured
//! payload, ...) — it only inspects structural facts (an HTTP status
//! code, `reqwest::Error::is_timeout`/`is_connect`, which enum variant).

use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use genai::Client;
use genai::adapter::AdapterKind;
use genai::chat::{ChatMessage as GenaiChatMessage, ChatRequest as GenaiChatRequest, ChatRole};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};

use crate::chat::{ChatChunkStream, ChatRequest, ChatResponse, RemoteChat};
use crate::error::UpstreamError;
use crate::remote::CredentialSource;

/// Which upstream protocol family a [`RemoteClient`] speaks. Each value is
/// a distinct, single adapter kind — a `RemoteClient` is bound to exactly
/// one via `genai`'s `with_adapter_kind` (see [`RemoteClient::new`]), not
/// left to per-call model-name inference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteProviderKind {
    OpenAiCompatible,
    AnthropicCompatible,
}

impl RemoteProviderKind {
    fn adapter_kind(self) -> AdapterKind {
        match self {
            RemoteProviderKind::OpenAiCompatible => AdapterKind::OpenAI,
            RemoteProviderKind::AnthropicCompatible => AdapterKind::Anthropic,
        }
    }
}

#[derive(Debug, Clone)]
pub struct RemoteClientConfig {
    pub kind: RemoteProviderKind,
    /// The endpoint's base URL — always used; this client never falls back
    /// to a provider's public default endpoint implicitly.
    pub base_url: String,
    /// Passed to [`CredentialSource::api_key`] as the `provider` label.
    pub provider_label: String,
}

pub struct RemoteClient {
    client: Client,
    adapter_kind: AdapterKind,
}

impl RemoteClient {
    pub fn new(config: RemoteClientConfig, credentials: Arc<dyn CredentialSource>) -> Self {
        let adapter_kind = config.kind.adapter_kind();
        // `genai`'s own adapters build the request URL either via
        // `Url::join` (OpenAI) or plain string concatenation (Anthropic) —
        // both require the base to end in `/` (e.g. `.../v1/`), or the
        // last path segment gets silently dropped (`join`) or the path
        // gets mashed together without a separator (concatenation).
        // Normalizing here means a caller passing `".../v1"` (no trailing
        // slash — an easy, otherwise-silent mistake) still works.
        let mut base_url = config.base_url;
        if !base_url.ends_with('/') {
            base_url.push('/');
        }
        let provider_label = config.provider_label;

        // Bridges `CredentialSource` (this crate's abstraction) into
        // `genai`'s own resolver mechanism. `Ok(None)` — not an `Err` —
        // when the source fails: that is exactly how you tell `genai`
        // "no credential available", which it then reports as its own
        // `NoAuthData` error, mapped back to `UpstreamError::AuthFailed`
        // by `map_genai_error`. The credential itself never appears in a
        // log line here — it flows straight from `CredentialSource` into
        // `AuthData::from_single`, never through a `{}`/`{:?}` format.
        let auth_resolver = AuthResolver::from_resolver_async_fn(move |_model_iden| {
            let credentials = Arc::clone(&credentials);
            let provider_label = provider_label.clone();
            Box::pin(async move {
                match credentials.api_key(&provider_label).await {
                    Ok(key) => Ok(Some(AuthData::from_single(key))),
                    Err(_) => Ok(None),
                }
            })
                as Pin<Box<dyn Future<Output = genai::resolver::Result<Option<AuthData>>> + Send>>
        });

        // Always overrides the endpoint to `base_url` — this client never
        // silently falls back to a provider's public default endpoint.
        let target_resolver =
            ServiceTargetResolver::from_resolver_fn(move |mut target: genai::ServiceTarget| {
                target.endpoint = Endpoint::from_owned(base_url.clone());
                Ok(target)
            });

        let client = Client::builder()
            .with_adapter_kind(adapter_kind)
            .with_auth_resolver(auth_resolver)
            .with_service_target_resolver(target_resolver)
            .build();

        Self {
            client,
            adapter_kind,
        }
    }
}

fn to_genai_role(role: &str) -> ChatRole {
    // `idoris_backend::ChatMessage::role` is a free-form `String` (mirrors
    // the TS `ChatMessage` interface, which never validated it either);
    // `genai::chat::ChatRole` is a closed enum, so *some* mapping decision
    // is unavoidable here. Recognized roles map directly; anything else
    // (including plain `"user"`) defaults to `User` — a visible, documented
    // choice, not a silent guess, and the safest default since `User` is
    // never rejected as "last message must be user" the way `System`/
    // `Assistant` as a trailing role can be.
    match role {
        "system" => ChatRole::System,
        "assistant" => ChatRole::Assistant,
        "tool" => ChatRole::Tool,
        _ => ChatRole::User,
    }
}

#[async_trait]
impl RemoteChat for RemoteClient {
    async fn chat(
        &self,
        req: ChatRequest,
        deadline: Instant,
    ) -> Result<ChatResponse, UpstreamError> {
        if Instant::now() >= deadline {
            return Err(UpstreamError::timeout());
        }
        let model_iden = genai::ModelIden::new(self.adapter_kind, req.model.clone());
        let messages: Vec<GenaiChatMessage> = req
            .messages
            .iter()
            .map(|m| GenaiChatMessage::new(to_genai_role(&m.role), m.content.clone()))
            .collect();
        let genai_req = GenaiChatRequest::new(messages);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let call = self.client.exec_chat(model_iden, genai_req, None);
        match tokio::time::timeout(remaining, call).await {
            Err(_elapsed) => Err(UpstreamError::timeout()),
            Ok(Err(err)) => Err(map_genai_error(err)),
            Ok(Ok(resp)) => Ok(ChatResponse {
                model: req.model,
                content: resp.first_text().unwrap_or("").to_string(),
            }),
        }
    }

    async fn chat_stream(
        &self,
        _req: ChatRequest,
        _deadline: Instant,
    ) -> Result<ChatChunkStream, UpstreamError> {
        // Streaming lands in a follow-up PR on this branch stack (the
        // `RemoteChat` trait's own PR landed the signature specifically so
        // this could be built incrementally — see `chat.rs`'s module doc).
        Err(UpstreamError::internal(
            "RemoteClient::chat_stream is not implemented yet",
        ))
    }
}

/// See this module's doc comment's "No response bodies or raw
/// `genai::Error` text" section — every arm here is built from structural
/// facts only, never from `Display`/`Debug` on a piece of `err` that could
/// carry response content.
fn map_genai_error(err: genai::Error) -> UpstreamError {
    match err {
        genai::Error::RequiresApiKey { .. }
        | genai::Error::NoAuthResolver { .. }
        | genai::Error::NoAuthData { .. } => UpstreamError::auth_failed(),

        genai::Error::WebModelCall { webc_error, .. }
        | genai::Error::WebAdapterCall { webc_error, .. } => map_webc_error(webc_error),

        genai::Error::HttpError { status, .. } => UpstreamError::from_status(status.as_u16()),

        genai::Error::Resolver { resolver_error, .. } => match resolver_error {
            genai::resolver::Error::ApiKeyEnvNotFound { .. } => UpstreamError::auth_failed(),
            _ => UpstreamError::internal("upstream credential resolver failed"),
        },

        genai::Error::ChatResponseGeneration { .. }
        | genai::Error::ChatResponse { .. }
        | genai::Error::StreamParse { .. }
        | genai::Error::NoChatResponse { .. }
        | genai::Error::InvalidJsonResponseElement { .. } => UpstreamError::malformed_response(),

        genai::Error::WebStream { .. } => UpstreamError::network(),

        // Everything else (missing/malformed chat request shape, adapter
        // mismatch, model mapping failure, unsupported feature, ...) is a
        // problem with the request or this client's own configuration,
        // not a dependency failure — retrying against a different upstream
        // state would not fix it.
        _ => UpstreamError::invalid_request("upstream client rejected the request shape"),
    }
}

fn map_webc_error(err: genai::webc::Error) -> UpstreamError {
    match err {
        genai::webc::Error::ResponseFailedStatus { status, .. } => {
            UpstreamError::from_status(status.as_u16())
        }
        genai::webc::Error::Reqwest(e) => {
            if e.is_timeout() {
                UpstreamError::timeout()
            } else {
                UpstreamError::network()
            }
        }
        genai::webc::Error::ResponseFailedNotJson { .. }
        | genai::webc::Error::ResponseFailedInvalidJson { .. }
        | genai::webc::Error::JsonValueExt(_) => UpstreamError::malformed_response(),
    }
}
