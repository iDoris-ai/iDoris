#![allow(clippy::unwrap_used)]

use http_body_util::BodyExt;
use serde_json::Value;
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
            "configured-messages",
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

#[tokio::test]
async fn configured_chat_and_messages_streams_use_selected_provider_not_endpoint_fallback() {
    use tower::ServiceExt;

    for path in ["/v1/chat/completions", "/v1/messages"] {
        let failed = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("failed"))
            .expect(2)
            .mount(&failed)
            .await;
        let usable = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("data: [DONE]\n\n", "text/event-stream"),
            )
            .expect(1)
            .mount(&usable)
            .await;
        let proxy = Arc::new(proxy::ChatProxy::new(reqwest::Client::new()));
        let options = proxy::ForwardOpts {
            request_id: None,
            tenant_id: None,
            record_id: "configured-test",
            provider_id: "configured-provider",
            served_locality: idoris_contracts::provider::Locality::Loopback,
            privacy: PrivacyClass::Any,
            require_openai_usage: false,
        };
        for _ in 0..2 {
            assert_eq!(
                proxy
                    .forward_buffered(&failed.uri(), &json!({}), &options)
                    .await
                    .status,
                500
            );
        }
        let app = build_app(AppState {
            cards: vec![super::tests::resident_component_card(
                "configured-provider",
                &usable.uri(),
            )],
            proxy: proxy.clone(),
            ..AppState::default()
        });
        let request = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"local","max_tokens":16,"stream":true,"messages":[]}"#,
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{path}");
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            serde_json::from_slice::<Value>(&bytes).unwrap()["error"]["type"],
            "provider_circuit_open"
        );
        assert!(usable.received_requests().await.unwrap().is_empty());
        // Same origin can still be used under the public endpoint-key fallback.
        let proxy::StreamOutcome::Stream { response, .. } =
            proxy.forward_stream(&usable.uri(), &json!({})).await
        else {
            panic!("healthy origin was blocked by a different provider key");
        };
        response.collect().await.unwrap();
        failed.verify().await;
        usable.verify().await;
    }
}

#[tokio::test]
async fn missing_terminal_marker_after_success_never_opens_configured_provider() {
    use tower::ServiceExt;

    for path in ["/v1/chat/completions", "/v1/messages"] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw("data: {\"delta\":\"unfinished\"}\n\n", "text/event-stream"),
            )
            .expect(3)
            .mount(&server)
            .await;
        let app = build_app(AppState {
            cards: vec![super::tests::resident_component_card(
                "configured-provider",
                &server.uri(),
            )],
            ..AppState::default()
        });
        for _ in 0..3 {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"model":"local","max_tokens":16,"stream":true,"messages":[]}"#,
                ))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert!(response.into_body().collect().await.is_err());
        }
        server.verify().await;
    }
}

#[tokio::test]
async fn buffered_embeddings_fail_over_after_two_provider_faults() {
    use idoris_contracts::common::Capability;
    use tower::ServiceExt;

    let failed = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error":"failed"})))
        .expect(2)
        .mount(&failed)
        .await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .and(wiremock::matchers::path("/v1/embeddings"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"data":[{"embedding":[0.1]}]})),
        )
        .expect(1)
        .mount(&healthy)
        .await;

    let mut failing_card =
        super::tests::resident_component_card("protocol-history-failing", &failed.uri());
    failing_card.provider.capabilities = vec![Capability::Embedding];
    let mut healthy_card =
        super::tests::resident_component_card("protocol-history-healthy", &healthy.uri());
    healthy_card.provider.capabilities = vec![Capability::Embedding];
    let app = build_app(AppState {
        cards: vec![failing_card, healthy_card],
        ..AppState::default()
    });
    let request = || {
        axum::http::Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"model":"embedding-history","input":"hello"}"#,
            ))
            .unwrap()
    };

    for attempt in 1..=2 {
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "attempt {attempt}"
        );
    }
    let response = app.oneshot(request()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(failed.received_requests().await.unwrap().len(), 2);
    let recovered = healthy.received_requests().await.unwrap();
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].url.path(), "/v1/embeddings");
    let forwarded: Value = serde_json::from_slice(&recovered[0].body).unwrap();
    assert_eq!(forwarded["model"], "embedding-history");
    failed.verify().await;
    healthy.verify().await;
}

#[tokio::test]
async fn buffered_protocol_endpoints_return_exact_open_reason_without_third_send() {
    use idoris_contracts::common::Capability;
    use tower::ServiceExt;

    for (path, card_id, capability, payload) in [
        (
            "/v1/embeddings",
            "protocol-open-embedding",
            Capability::Embedding,
            json!({"model":"embedding-open","input":"hello"}),
        ),
        (
            "/v1/rerank",
            "protocol-open-rerank",
            Capability::Rerank,
            json!({"model":"rerank-open","query":"hello","documents":["world"]}),
        ),
        (
            "/v1/messages",
            "protocol-open-messages",
            Capability::Chat,
            json!({"model":"messages-open","max_tokens":8,"stream":false,"messages":[]}),
        ),
    ] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(path))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error":"failed"})))
            .expect(2)
            .mount(&upstream)
            .await;
        let mut card = super::tests::resident_component_card(card_id, &upstream.uri());
        card.provider.capabilities = vec![capability];
        let app = build_app(AppState {
            cards: vec![card],
            ..AppState::default()
        });
        let send = || {
            axum::http::Request::builder()
                .method("POST")
                .uri(path)
                .header("content-type", "application/json")
                .body(Body::from(payload.to_string()))
                .unwrap()
        };

        for attempt in 1..=2 {
            let response = app.clone().oneshot(send()).await.unwrap();
            assert_eq!(
                response.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{path} attempt {attempt}"
            );
        }
        let response = app.oneshot(send()).await.unwrap();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "{path} open refusal"
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let error: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(error["error"]["type"], "provider_circuit_open", "{path}");
        let requests = upstream.received_requests().await.unwrap();
        assert_eq!(requests.len(), 2, "{path} must not send after opening");
        assert!(requests.iter().all(|request| request.url.path() == path));
        upstream.verify().await;
    }
}
