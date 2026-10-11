#![allow(clippy::unwrap_used)]

use http_body_util::BodyExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

#[tokio::test]
async fn generalized_stream_transport_preserves_messages_path_and_payload() {
    let server = MockServer::start().await;
    let payload = json!({
        "model": "claude-local",
        "max_tokens": 16,
        "messages": [{"role":"user","content":"hello"}],
        "stream": true
    });
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/messages"))
        .and(wiremock::matchers::body_json(payload.clone()))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_raw("event: message_stop\ndata: {}\n\n", "text/event-stream"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let proxy = proxy::ChatProxy::new(reqwest::Client::new());
    let outcome = proxy
        .forward_stream_path(
            &server.uri(),
            proxy::StreamingUpstreamPath::MESSAGES,
            &payload,
        )
        .await;
    let proxy::StreamOutcome::Stream {
        status, response, ..
    } = outcome
    else {
        panic!("expected streaming response");
    };
    assert_eq!(status, 200);
    let bytes = response.collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), b"event: message_stop\ndata: {}\n\n");
    server.verify().await;
}
