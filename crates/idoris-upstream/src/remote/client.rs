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
use std::task::{Context, Poll};
use std::time::Instant;

use async_trait::async_trait;
use futures_core::Stream;
use genai::Client;
use genai::adapter::AdapterKind;
use genai::chat::{
    ChatMessage as GenaiChatMessage, ChatRequest as GenaiChatRequest, ChatRole, ChatStream,
    ChatStreamEvent,
};
use genai::resolver::{AuthData, AuthResolver, Endpoint, ServiceTargetResolver};

use crate::chat::{ChatChunk, ChatChunkStream, ChatRequest, ChatResponse, RemoteChat};
use crate::error::UpstreamError;
use crate::remote::CredentialSource;

const CREDENTIAL_SOURCE_FAILED: &str = "upstream credential source failed";

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
        // `genai`'s own resolver mechanism. A source failure must return
        // `Err`: `Ok(None)` makes genai fall back to its default environment
        // credentials. Use a fixed marker mapped to `AuthFailed` below,
        // never the source's error text (which may contain secret data).
        // The key flows straight into `AuthData::from_single`, never
        // through a `{}`/`{:?}` format.
        let auth_resolver = AuthResolver::from_resolver_async_fn(move |_model_iden| {
            let credentials = Arc::clone(&credentials);
            let provider_label = provider_label.clone();
            Box::pin(async move {
                match credentials.api_key(&provider_label).await {
                    Ok(key) => Ok(Some(AuthData::from_single(key))),
                    Err(_) => Err(genai::resolver::Error::Custom(
                        CREDENTIAL_SOURCE_FAILED.to_string(),
                    )),
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

impl RemoteClient {
    fn to_genai_request(&self, req: &ChatRequest) -> (genai::ModelIden, GenaiChatRequest) {
        let model_iden = genai::ModelIden::new(self.adapter_kind, req.model.clone());
        let messages: Vec<GenaiChatMessage> = req
            .messages
            .iter()
            .map(|m| GenaiChatMessage::new(to_genai_role(&m.role), m.content.clone()))
            .collect();
        (model_iden, GenaiChatRequest::new(messages))
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
        let (model_iden, genai_req) = self.to_genai_request(&req);
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

    /// Sets up the stream, itself bounded by `deadline` like [`Self::chat`];
    /// once streaming, [`DeadlineStream`] takes over enforcing the same
    /// `deadline` against the whole rest of the stream — see its doc
    /// comment.
    async fn chat_stream(
        &self,
        req: ChatRequest,
        deadline: Instant,
    ) -> Result<ChatChunkStream, UpstreamError> {
        if Instant::now() >= deadline {
            return Err(UpstreamError::timeout());
        }
        let (model_iden, genai_req) = self.to_genai_request(&req);
        let remaining = deadline.saturating_duration_since(Instant::now());
        let setup = self.client.exec_chat_stream(model_iden, genai_req, None);
        let stream_response = tokio::time::timeout(remaining, setup)
            .await
            .map_err(|_elapsed| UpstreamError::timeout())?
            .map_err(map_genai_error)?;
        Ok(Box::pin(DeadlineStream::new(
            stream_response.stream,
            deadline,
        )))
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
            genai::resolver::Error::Custom(message) if message == CREDENTIAL_SOURCE_FAILED => {
                UpstreamError::auth_failed()
            }
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

/// Maps one `genai` stream event to zero or one [`ChatChunk`]s. `None`
/// means "filtered, poll again" — [`ChatStreamEvent::Start`] carries no
/// text, and reasoning/thought-signature/tool-call chunks have no
/// representation in [`ChatChunk`] yet ([`crate::chat`]'s `ChatChunk` is
/// deliberately minimal — text-only — as landed in its own PR; extending it
/// to carry richer event kinds is a separate, later change, not something
/// to smuggle in here by inventing an ad hoc encoding).
fn event_to_chunk(event: ChatStreamEvent) -> Option<ChatChunk> {
    match event {
        ChatStreamEvent::Chunk(c) => Some(ChatChunk {
            delta: c.content,
            done: false,
        }),
        ChatStreamEvent::End(_) => Some(ChatChunk {
            delta: String::new(),
            done: true,
        }),
        ChatStreamEvent::Start
        | ChatStreamEvent::ReasoningChunk(_)
        | ChatStreamEvent::ThoughtSignatureChunk(_)
        | ChatStreamEvent::ToolCallChunk(_) => None,
    }
}

/// Wraps a `genai` [`ChatStream`], translating each event via
/// [`event_to_chunk`] and enforcing `deadline` against the **whole
/// stream**, not per item — matches [`crate::chat::RemoteChat::chat_stream`]'s
/// documented contract. Checked on every poll (including between
/// internally-filtered events, so a stream that yields nothing but
/// `Start`/reasoning chunks forever still can't dodge the deadline by
/// never producing a chunk this type surfaces).
struct DeadlineStream {
    inner: ChatStream,
    /// A real timer, not just a `deadline: Instant` field checked at poll
    /// time. **This is load-bearing, not decoration**: an `Instant`-only
    /// check only fires when something else happens to poll this stream
    /// again — and if `inner` goes `Pending` waiting on a slow/hung
    /// upstream and never wakes the task again before the deadline, a
    /// bare-`Instant` version would never get polled again either, and
    /// would wait forever instead of timing out. Polling this `Sleep`
    /// registers its own waker with the tokio timer, so the task gets
    /// woken purely from the deadline elapsing, independent of `inner`.
    sleep: Pin<Box<tokio::time::Sleep>>,
    /// Once `true`, every subsequent poll returns `Ready(None)` — a stream
    /// must not keep yielding items after it has already reported a fatal
    /// error or its own natural end.
    finished: bool,
}

impl DeadlineStream {
    fn new(inner: ChatStream, deadline: Instant) -> Self {
        Self {
            inner,
            sleep: Box::pin(tokio::time::sleep_until(tokio::time::Instant::from_std(
                deadline,
            ))),
            finished: false,
        }
    }
}

impl Stream for DeadlineStream {
    type Item = Result<ChatChunk, UpstreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        use std::future::Future;

        if self.finished {
            return Poll::Ready(None);
        }
        loop {
            // Poll the timer *every* iteration, including after a filtered
            // event — see the field doc: this is what actually wakes the
            // task when `inner` itself never does.
            if self.sleep.as_mut().poll(cx).is_ready() {
                self.finished = true;
                return Poll::Ready(Some(Err(UpstreamError::timeout())));
            }
            match Pin::new(&mut self.inner).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    self.finished = true;
                    return Poll::Ready(None);
                }
                Poll::Ready(Some(Err(err))) => {
                    self.finished = true;
                    return Poll::Ready(Some(Err(map_genai_error(err))));
                }
                Poll::Ready(Some(Ok(event))) => match event_to_chunk(event) {
                    Some(chunk) => {
                        if chunk.done {
                            self.finished = true;
                        }
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    // Filtered event (Start/reasoning/...) — poll `inner`
                    // again rather than returning `Pending`: it may have
                    // more already-buffered events ready right now.
                    None => continue,
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::time::Duration;

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::chat::ChatMessage;

    struct FixedKey(&'static str);

    #[async_trait]
    impl CredentialSource for FixedKey {
        async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
            Ok(self.0.to_string())
        }
    }

    struct AlwaysFails;

    #[async_trait]
    impl CredentialSource for AlwaysFails {
        async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
            Err(UpstreamError::auth_failed())
        }
    }

    struct SourceError(UpstreamError);

    #[async_trait]
    impl CredentialSource for SourceError {
        async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
            Err(self.0.clone())
        }
    }

    async fn assert_source_failure_ignores_environment(
        test_name: &str,
        kind: RemoteProviderKind,
        streaming: bool,
    ) {
        const CHILD: &str = "IDORIS_CREDENTIAL_REGRESSION_CHILD";
        const HOST_KEY: &str = "host-key-must-not-be-used";
        // Set env only in a fresh process: Rust 2024 env mutation is
        // unsafe and would race the other credential tests.
        if std::env::var(CHILD).as_deref() != Ok(test_name) {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", test_name, "--nocapture"])
                .env(CHILD, test_name)
                .env("OPENAI_API_KEY", HOST_KEY)
                .env("ANTHROPIC_API_KEY", HOST_KEY)
                .status()
                .expect("credential regression child must run");
            assert!(status.success(), "credential regression child failed");
            return;
        }
        assert_eq!(std::env::var("OPENAI_API_KEY").unwrap(), HOST_KEY);
        assert_eq!(std::env::var("ANTHROPIC_API_KEY").unwrap(), HOST_KEY);
        for source_error in [
            UpstreamError::auth_failed(),
            UpstreamError::internal("credential-store-detail-must-not-leak"),
        ] {
            let server = MockServer::start().await;
            // Even if the upstream would reject the host key, reaching
            // it has already crossed the credential source's boundary.
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&server)
                .await;
            let client = RemoteClient::new(
                RemoteClientConfig {
                    kind,
                    base_url: format!("{}/v1/", server.uri()),
                    provider_label: "test-provider".to_string(),
                },
                Arc::new(SourceError(source_error)),
            );
            let result = if streaming {
                client
                    .chat_stream(request(), far_future_deadline())
                    .await
                    .map(|_| ())
            } else {
                client
                    .chat(request(), far_future_deadline())
                    .await
                    .map(|_| ())
            };
            assert_eq!(
                server.received_requests().await.unwrap().len(),
                0,
                "credential source rejection must prevent all outbound requests"
            );
            let err = result.expect_err("credential source failure must reject the call");
            assert_eq!(err, UpstreamError::AuthFailed);
            assert!(!err.to_string().contains(HOST_KEY));
            assert!(!err.to_string().contains("credential-store-detail"));
        }
    }

    #[tokio::test]
    async fn openai_chat_source_failure_ignores_environment() {
        assert_source_failure_ignores_environment(
            "remote::client::tests::openai_chat_source_failure_ignores_environment",
            RemoteProviderKind::OpenAiCompatible,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn openai_stream_source_failure_ignores_environment() {
        assert_source_failure_ignores_environment(
            "remote::client::tests::openai_stream_source_failure_ignores_environment",
            RemoteProviderKind::OpenAiCompatible,
            true,
        )
        .await;
    }

    #[tokio::test]
    async fn anthropic_chat_source_failure_ignores_environment() {
        assert_source_failure_ignores_environment(
            "remote::client::tests::anthropic_chat_source_failure_ignores_environment",
            RemoteProviderKind::AnthropicCompatible,
            false,
        )
        .await;
    }

    #[tokio::test]
    async fn anthropic_stream_source_failure_ignores_environment() {
        assert_source_failure_ignores_environment(
            "remote::client::tests::anthropic_stream_source_failure_ignores_environment",
            RemoteProviderKind::AnthropicCompatible,
            true,
        )
        .await;
    }

    fn client_for(server: &MockServer, credentials: Arc<dyn CredentialSource>) -> RemoteClient {
        RemoteClient::new(
            RemoteClientConfig {
                kind: RemoteProviderKind::OpenAiCompatible,
                base_url: format!("{}/v1/", server.uri()),
                provider_label: "test-provider".to_string(),
            },
            credentials,
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: "test-model".to_string(),
            messages: vec![ChatMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            }],
        }
    }

    fn far_future_deadline() -> Instant {
        Instant::now() + Duration::from_secs(30)
    }

    #[tokio::test]
    async fn chat_returns_content_and_sends_only_model_and_messages() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "model": "test-model",
                "choices": [{"message": {"content": "hello there"}, "finish_reason": "stop"}]
            })))
            .mount(&server)
            .await;
        let client = client_for(&server, Arc::new(FixedKey("test-key-should-never-leak")));
        let resp = client
            .chat(request(), far_future_deadline())
            .await
            .expect("chat must succeed");
        assert_eq!(resp.content, "hello there");

        // De-association + credential wiring: exactly one request, carrying
        // the auth header and the request's own model/messages — nothing
        // this client adds itself (no tenant/user id, no extra headers).
        let received = server
            .received_requests()
            .await
            .expect("recording must be enabled");
        assert_eq!(received.len(), 1);
        let req = &received[0];
        assert_eq!(
            req.headers
                .get("authorization")
                .map(|v| v.to_str().unwrap()),
            Some("Bearer test-key-should-never-leak")
        );
        let body: serde_json::Value = serde_json::from_slice(&req.body).expect("body must be JSON");
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["messages"][0]["content"], "hi");
        // Every top-level body field must be an ordinary chat-completion
        // request field — none of them a tenant/user identifier. Not an
        // exact-field-count check: `genai` is free to add ordinary
        // protocol fields (e.g. `stream`) as it evolves; what must never
        // appear is anything identity-shaped.
        const ALLOWED_BODY_FIELDS: &[&str] = &["model", "messages", "stream", "stream_options"];
        for key in body.as_object().expect("body must be an object").keys() {
            assert!(
                ALLOWED_BODY_FIELDS.contains(&key.as_str()),
                "unexpected request body field {key:?} — every field sent must be accounted for here"
            );
        }
        // Every header name is a plain transport/protocol header — none of
        // them encode a tenant/user identity (the de-association
        // requirement: "出站请求不携带用户或租户标识").
        const ALLOWED_HEADERS: &[&str] = &[
            "authorization",
            "content-type",
            "content-length",
            "host",
            "accept",
            "accept-encoding",
            "user-agent",
        ];
        for name in req.headers.keys() {
            let name = name.as_str().to_ascii_lowercase();
            assert!(
                ALLOWED_HEADERS.contains(&name.as_str()),
                "unexpected outbound header {name:?} — every header sent must be accounted for here, \
                 to catch a future change accidentally adding a tenant/user-identifying one"
            );
        }
    }

    #[tokio::test]
    async fn chat_maps_401_to_auth_failed_without_leaking_the_body_or_key() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(401).set_body_string("unauthorized-body-should-not-leak"),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Arc::new(FixedKey("test-key-should-never-leak")));
        let err = client
            .chat(request(), far_future_deadline())
            .await
            .expect_err("401 must fail");
        assert_eq!(err.reason_code(), "auth_failed");
        let msg = err.to_string();
        assert!(!msg.contains("unauthorized-body-should-not-leak"));
        assert!(!msg.contains("test-key-should-never-leak"));
    }

    #[tokio::test]
    async fn chat_maps_500_to_server_error() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let client = client_for(&server, Arc::new(FixedKey("k")));
        let err = client
            .chat(request(), far_future_deadline())
            .await
            .expect_err("500 must fail");
        assert_eq!(err.reason_code(), "upstream_server_error");
    }

    #[tokio::test]
    async fn chat_reports_auth_failed_when_the_credential_source_itself_fails() {
        let server = MockServer::start().await;
        // No mock mounted: a credential failure must short-circuit before
        // any HTTP call is attempted.
        let client = client_for(&server, Arc::new(AlwaysFails));
        let err = client
            .chat(request(), far_future_deadline())
            .await
            .expect_err("must fail when no credential is available");
        assert_eq!(err.reason_code(), "auth_failed");
        assert!(
            server
                .received_requests()
                .await
                .expect("recording enabled")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn chat_fails_fast_on_an_already_passed_deadline_without_calling_out() {
        let server = MockServer::start().await;
        let client = client_for(&server, Arc::new(FixedKey("k")));
        let past = Instant::now() - Duration::from_secs(1);
        let err = client
            .chat(request(), past)
            .await
            .expect_err("must time out immediately");
        assert_eq!(err.reason_code(), "timeout");
        assert!(
            server
                .received_requests()
                .await
                .expect("recording enabled")
                .is_empty()
        );
    }

    /// A minimal OpenAI-shaped SSE body: two content chunks then a
    /// `finish_reason` chunk, terminated by the spec's `data: [DONE]`.
    fn sse_body(chunks: &[&str]) -> String {
        let mut body = String::new();
        for chunk in chunks {
            body.push_str(&format!(
                "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{chunk}\"}},\"finish_reason\":null}}]}}\n\n"
            ));
        }
        body.push_str("data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n");
        body.push_str("data: [DONE]\n\n");
        body
    }

    async fn drain(mut stream: ChatChunkStream) -> Result<Vec<ChatChunk>, UpstreamError> {
        let mut items = Vec::new();
        loop {
            match std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
                Some(Ok(chunk)) => items.push(chunk),
                Some(Err(err)) => return Err(err),
                None => return Ok(items),
            }
        }
    }

    #[tokio::test]
    async fn chat_stream_yields_text_chunks_then_a_done_marker() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body(&["Hello", " world"]), "text/event-stream"),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Arc::new(FixedKey("k")));
        let stream = client
            .chat_stream(request(), far_future_deadline())
            .await
            .expect("stream setup must succeed");
        let chunks = drain(stream).await.expect("stream must not error");
        let text: String = chunks.iter().map(|c| c.delta.as_str()).collect();
        assert_eq!(text, "Hello world");
        assert!(chunks.last().expect("at least one chunk").done);
    }

    #[tokio::test]
    async fn chat_stream_fails_fast_on_an_already_passed_deadline_without_calling_out() {
        let server = MockServer::start().await;
        let client = client_for(&server, Arc::new(FixedKey("k")));
        let past = Instant::now() - Duration::from_secs(1);
        client
            .chat_stream(request(), past)
            .await
            .map(|_stream| ())
            .expect_err("must time out immediately");
        assert!(
            server
                .received_requests()
                .await
                .expect("recording enabled")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn chat_stream_reports_timeout_when_the_server_never_responds_in_time() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(sse_body(&["late"]), "text/event-stream")
                    .set_delay(Duration::from_secs(5)),
            )
            .mount(&server)
            .await;
        let client = client_for(&server, Arc::new(FixedKey("k")));
        let deadline = Instant::now() + Duration::from_millis(50);
        let outcome = tokio::time::timeout(Duration::from_secs(2), async {
            match client.chat_stream(request(), deadline).await {
                Err(err) => Err(err),
                Ok(stream) => drain(stream).await,
            }
        })
        .await
        .expect("the test itself must not hang");
        let err = outcome.expect_err("must report timeout, not hang for the full 5s delay");
        assert_eq!(err.reason_code(), "timeout");
    }
}
