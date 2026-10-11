#![allow(clippy::unwrap_used)]

use idoris_contracts::common::PrivacyClass;
use idoris_contracts::provider::Locality;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

fn opts() -> ForwardOpts<'static> {
    ForwardOpts {
        request_id: None,
        tenant_id: None,
        record_id: "record-path",
        provider_id: "provider-path",
        served_locality: Locality::Loopback,
        privacy: PrivacyClass::LocalOnly,
        require_openai_usage: false,
    }
}

#[tokio::test]
async fn generic_buffered_paths_preserve_non_chat_payloads() {
    let server = MockServer::start().await;
    for path in ["/v1/embeddings", "/v1/rerank"] {
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(path))
            .and(wiremock::matchers::body_json(serde_json::json!({
                "input": "hello"
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
            .expect(1)
            .mount(&server)
            .await;
    }
    let proxy = ChatProxy::new(reqwest::Client::new());
    for path in [
        BufferedUpstreamPath {
            path: "/v1/embeddings",
            force_non_stream: false,
        },
        BufferedUpstreamPath {
            path: "/v1/rerank",
            force_non_stream: false,
        },
    ] {
        let outcome = proxy
            .forward_buffered_path(
                &server.uri(),
                path,
                &serde_json::json!({"input":"hello"}),
                &opts(),
            )
            .await;
        assert_eq!(outcome.status, 200);
    }
    server.verify().await;
}

#[tokio::test]
async fn chat_path_still_forces_stream_false() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/chat/completions"))
        .and(wiremock::matchers::body_json(serde_json::json!({
            "messages": [],
            "stream": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok":true})))
        .expect(1)
        .mount(&server)
        .await;
    let proxy = ChatProxy::new(reqwest::Client::new());
    let outcome = proxy
        .forward_buffered(
            &server.uri(),
            &serde_json::json!({"messages": [], "stream": true}),
            &opts(),
        )
        .await;
    assert_eq!(outcome.status, 200);
    server.verify().await;
}
