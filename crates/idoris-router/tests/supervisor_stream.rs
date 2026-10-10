#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../src/supervisor_stream.rs"]
mod supervisor_stream;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use idoris_contracts::ComponentCard;
use idoris_router::dispatch::BoundSupervisor;
use idoris_router::{AppState, build_app};
use serde_json::{Map, Value, json};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = endpoint.to_string();
    card
}

fn request(body: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("X-iDoris-Privacy", "local_only")
        .body(Body::from(body.to_owned()))
        .unwrap()
}

#[test]
fn supervisor_stream_validator_accepts_only_false_or_absent() {
    for body in [json!({}), json!({"stream": false})] {
        let object = body.as_object().unwrap();
        assert_eq!(supervisor_stream::validate(object), Ok(()));
    }
    for body in [
        json!({"stream": true}),
        json!({"stream": null}),
        json!({"stream": "false"}),
        json!({"stream": 1}),
        json!({"stream": []}),
    ] {
        let object = body.as_object().unwrap();
        let message = supervisor_stream::validate(object).unwrap_err();
        assert!(message.contains("stream"));
    }
}

#[tokio::test]
async fn stream_true_is_400_before_any_omlx_request() {
    let upstream = MockServer::start().await;
    let supervisor = BoundSupervisor::spawn_omlx(&card(&upstream.uri())).unwrap();
    let app = build_app(AppState {
        cards: vec![card(&upstream.uri())],
        runtimes: Some(supervisor).into(),
        ..AppState::default()
    });

    let response = app
        .oneshot(request(
            r#"{"model":"idoris/daily","stream":true,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    let status = response.status();
    let locality = response.headers().get("X-iDoris-Served-Locality").cloned();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    assert_eq!(locality.unwrap(), "loopback");
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["type"], "unsupported_field");
    assert_eq!(body["error"]["reason_code"], "unsupported_stream");
    assert!(
        body["error"]["remediation"]
            .as_str()
            .unwrap()
            .contains("stream=false")
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn stream_false_uses_buffered_omlx_completion() {
    let upstream = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "loaded_models": [],
            "model_memory_used": 0,
            "model_memory_max": 16,
            "pressure": "ok"
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/omlx/load"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&upstream)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "models": [{
                "id": "omlx",
                "loaded": true,
                "pinned": false,
                "estimated_size": 1024_u64 * 1024 * 1024
            }]
        })))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"message": {"content": "buffered answer"}}]
        })))
        .mount(&upstream)
        .await;
    let supervisor = BoundSupervisor::spawn_omlx(&card(&upstream.uri())).unwrap();
    let app = build_app(AppState {
        cards: vec![card(&upstream.uri())],
        runtimes: Some(supervisor).into(),
        ..AppState::default()
    });

    let response = app
        .oneshot(request(
            r#"{"model":"idoris/daily","stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
        .await
        .unwrap();
    let status = response.status();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "buffered answer");
    let requests = upstream.received_requests().await.unwrap();
    assert!(
        requests
            .iter()
            .any(|request| request.url.path() == "/v1/chat/completions")
    );
}

#[tokio::test]
async fn malformed_stream_values_are_400_without_supervisor_work() {
    for stream in [Value::Null, json!("true"), json!(1), json!([])] {
        let upstream = MockServer::start().await;
        let mut body = Map::new();
        body.insert("model".to_string(), json!("idoris/daily"));
        body.insert("stream".to_string(), stream);
        body.insert("messages".to_string(), json!([]));
        let app = build_app(AppState {
            cards: vec![card(&upstream.uri())],
            // If validation runs after dispatch, the request fails later with
            // 503; the 400 proves malformed stream values stop at admission.
            ..AppState::default()
        });
        let response = app
            .oneshot(request(&Value::Object(body).to_string()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(upstream.received_requests().await.unwrap().is_empty());
    }
}
