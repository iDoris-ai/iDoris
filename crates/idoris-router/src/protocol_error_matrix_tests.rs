#![allow(clippy::unwrap_used)]

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_contracts::common::Capability;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::*;

const ENDPOINTS: [(&str, Capability); 3] = [
    ("/v1/embeddings", Capability::Embedding),
    ("/v1/rerank", Capability::Rerank),
    ("/v1/messages", Capability::Chat),
];

fn request(path: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

async fn json_body(response: Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn assert_unified_error(error: &serde_json::Value, expected_type: &str) {
    let error = &error["error"];
    assert_eq!(error["type"], expected_type);
    assert!(error.get("rule_id").is_some());
    assert!(error.get("reason_code").is_some());
    assert!(error.get("evidence").is_some());
    assert!(error.get("remediation").is_some());
    assert_eq!(error.as_object().unwrap().len(), 5);
}

#[tokio::test]
async fn protocol_endpoints_share_invalid_json_envelope() {
    let app = build_app(AppState::default());
    for (path, _) in ENDPOINTS {
        let response = app
            .clone()
            .oneshot(request(path, Body::from("{")))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        let json = json_body(response).await;
        assert_unified_error(&json, "invalid_json");
        assert_eq!(json["error"]["reason_code"], "invalid_json");
    }
}

#[tokio::test]
async fn protocol_endpoints_share_tenant_missing_envelope_before_egress() {
    let app = build_app(AppState {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        ..AppState::default()
    });
    for (path, _) in ENDPOINTS {
        let response = app
            .clone()
            .oneshot(request(
                path,
                Body::from(r#"{"model":"x","messages":[],"input":"x","documents":[]}"#),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
        let json = json_body(response).await;
        assert_unified_error(&json, "tenant_missing");
    }
}

#[tokio::test]
async fn protocol_endpoints_preserve_upstream_non_success_body() {
    for (path, capability) in ENDPOINTS {
        let server = MockServer::start().await;
        let upstream_body = format!("upstream-{path}");
        Mock::given(method("POST"))
            .and(wiremock::matchers::path(path))
            .respond_with(
                ResponseTemplate::new(418)
                    .insert_header("content-type", "text/plain")
                    .set_body_string(upstream_body.clone()),
            )
            .expect(1)
            .mount(&server)
            .await;
        let mut card = super::tests::resident_component_card("protocol-local", &server.uri());
        card.provider.capabilities = vec![capability];
        let app = build_app(AppState {
            cards: vec![card],
            ..AppState::default()
        });
        let payload = match path {
            "/v1/embeddings" => json!({"model":"x","input":"hello"}),
            "/v1/rerank" => json!({"model":"x","query":"q","documents":["d"]}),
            "/v1/messages" => json!({"model":"x","max_tokens":8,"messages":[]}),
            _ => unreachable!(),
        };
        let response = app
            .oneshot(request(
                path,
                Body::from(serde_json::to_vec(&payload).unwrap()),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::IM_A_TEAPOT, "{path}");
        assert_eq!(
            response.headers()[axum::http::header::CONTENT_TYPE],
            HeaderValue::from_static("text/plain")
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), upstream_body.as_bytes());
        server.verify().await;
    }
}
