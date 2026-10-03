//! Public-API regressions for remote SSE terminal frames at EOF.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use idoris_upstream::remote::{
    CredentialSource, RemoteClient, RemoteClientConfig, RemoteProviderKind,
};
use idoris_upstream::{
    ChatChunk, ChatChunkStream, ChatMessage, ChatRequest, RemoteChat, UpstreamError,
};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct Key;

#[async_trait::async_trait]
impl CredentialSource for Key {
    async fn api_key(&self, _provider: &str) -> Result<String, UpstreamError> {
        Ok("test-key".into())
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        model: "test-model".into(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: "hi".into(),
        }],
    }
}

fn client(base_url: String, kind: RemoteProviderKind) -> RemoteClient {
    RemoteClient::new(
        RemoteClientConfig {
            kind,
            base_url,
            provider_label: "test".into(),
        },
        Arc::new(Key),
    )
}

fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(15)
}

async fn drain(mut stream: ChatChunkStream) -> (String, Vec<bool>, Vec<String>) {
    let mut text = String::new();
    let mut dones = Vec::new();
    let mut errors = Vec::new();
    while let Some(item) = std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
        match item {
            Ok(ChatChunk { delta, done }) => {
                text.push_str(&delta);
                dones.push(done);
            }
            Err(err) => errors.push(err.reason_code().to_owned()),
        }
    }
    (text, dones, errors)
}

fn openai_partial(eol: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"partial\"}},\"finish_reason\":null}}]}}{eol}{eol}"
    )
}

async fn mock_stream(kind: RemoteProviderKind, body: String) -> (RemoteClient, MockServer) {
    let server = MockServer::start().await;
    let endpoint = match kind {
        RemoteProviderKind::OpenAiCompatible => "/v1/chat/completions",
        RemoteProviderKind::AnthropicCompatible => "/v1/messages",
    };
    Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path(endpoint))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw(body, "text/event-stream"),
        )
        .mount(&server)
        .await;
    (client(format!("{}/v1/", server.uri()), kind), server)
}

async fn assert_truncated(kind: RemoteProviderKind, body: String) {
    assert_stream_error(kind, body, "network_error").await;
}

async fn assert_malformed(kind: RemoteProviderKind, body: String) {
    assert_stream_error(kind, body, "malformed_response").await;
}

async fn assert_stream_error(kind: RemoteProviderKind, body: String, expected_error: &str) {
    let (client, _server) = mock_stream(kind, body).await;
    let (text, dones, errors) =
        drain(client.chat_stream(request(), deadline()).await.unwrap()).await;
    assert_eq!(text, "partial");
    assert!(
        dones.iter().all(|done| !done),
        "truncated stream emitted done: {dones:?}"
    );
    assert_eq!(errors, [expected_error]);
}

async fn assert_bom_stream_error(body: String, expected_text: &str, expected_error: &str) {
    let (client, _server) = mock_stream(RemoteProviderKind::OpenAiCompatible, body).await;
    let (text, dones, errors) =
        drain(client.chat_stream(request(), deadline()).await.unwrap()).await;
    assert_eq!(text, expected_text);
    assert!(dones.iter().all(|done| !done), "unexpected done: {dones:?}");
    assert_eq!(errors, [expected_error]);
}

#[tokio::test]
async fn openai_done_without_empty_line_is_truncated() {
    for ending in ["", "\n", "\r", "\r\n"] {
        assert_truncated(
            RemoteProviderKind::OpenAiCompatible,
            format!("{}data: [DONE]{ending}", openai_partial("\n")),
        )
        .await;
    }
}

#[tokio::test]
async fn openai_done_after_empty_colon_data_field_is_truncated() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("{}data:{eol}data: [DONE]{eol}{eol}", openai_partial(eol));
        assert_malformed(RemoteProviderKind::OpenAiCompatible, body).await;
    }
}

#[tokio::test]
async fn openai_done_after_colonless_data_field_is_truncated() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("{}data{eol}data: [DONE]{eol}{eol}", openai_partial(eol));
        assert_malformed(RemoteProviderKind::OpenAiCompatible, body).await;
    }
}

#[tokio::test]
async fn openai_bom_does_not_turn_an_empty_data_event_into_done() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("\u{feff}data:{eol}data: [DONE]{eol}{eol}");
        assert_bom_stream_error(body, "", "malformed_response").await;
    }
}

#[tokio::test]
async fn openai_leading_bom_allows_a_complete_done_event() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("\u{feff}data: [DONE]{eol}{eol}");
        let (client, _server) = mock_stream(RemoteProviderKind::OpenAiCompatible, body).await;
        let (text, dones, errors) =
            drain(client.chat_stream(request(), deadline()).await.unwrap()).await;
        assert!(text.is_empty());
        assert_eq!(dones, [true]);
        assert!(errors.is_empty());
    }
}

#[tokio::test]
async fn openai_bom_after_stream_start_is_not_stripped() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("{}\u{feff}data: [DONE]{eol}{eol}", openai_partial(eol));
        assert_bom_stream_error(body, "partial", "network_error").await;
    }
}

#[tokio::test]
async fn openai_second_leading_bom_is_not_stripped() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("\u{feff}\u{feff}data: [DONE]{eol}{eol}");
        assert_bom_stream_error(body, "", "network_error").await;
    }
}

#[tokio::test]
async fn anthropic_message_stop_without_empty_line_is_truncated() {
    for ending in ["", "\n", "\r", "\r\n"] {
        let anthropic = format!(
            "event: content_block_delta\ndata: {{\"delta\":{{\"text\":\"partial\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}{ending}"
        );
        assert_truncated(RemoteProviderKind::AnthropicCompatible, anthropic).await;
    }
}

#[tokio::test]
async fn complete_terminal_blank_lines_remain_successful() {
    for eol in ["\n", "\r\n", "\r"] {
        let body = format!("{}data: [DONE]{eol}{eol}", openai_partial(eol));
        let (client, _server) = mock_stream(RemoteProviderKind::OpenAiCompatible, body).await;
        let (text, dones, errors) =
            drain(client.chat_stream(request(), deadline()).await.unwrap()).await;
        assert_eq!(text, "partial");
        assert_eq!(dones.iter().filter(|done| **done).count(), 1);
        assert!(dones.last().copied().unwrap_or(false));
        assert!(errors.is_empty());

        let body = format!(
            "event: content_block_delta{eol}data: {{\"delta\":{{\"text\":\"partial\"}}}}{eol}{eol}event: message_stop{eol}data: {{\"type\":\"message_stop\"}}{eol}{eol}"
        );
        let (client, _server) = mock_stream(RemoteProviderKind::AnthropicCompatible, body).await;
        let (text, dones, errors) =
            drain(client.chat_stream(request(), deadline()).await.unwrap()).await;
        assert_eq!(text, "partial");
        assert_eq!(dones.iter().filter(|done| **done).count(), 1);
        assert!(dones.last().copied().unwrap_or(false));
        assert!(errors.is_empty());
    }
}
