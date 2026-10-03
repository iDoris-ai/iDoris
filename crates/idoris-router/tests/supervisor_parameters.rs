#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_contracts::{ComponentCard, load_policy::LoadMode};
use idoris_router::{AppState, build_app, dispatch::BoundSupervisor};
use serde_json::{Value, json};
use tower::ServiceExt;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_json, method, path},
};

fn card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = endpoint.into();
    card.provider.id = "Qwen3-0.6B-4bit".into();
    card
}

fn request(body: Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

#[tokio::test]
async fn unsupported_options_are_400_before_any_upstream_call() {
    let upstream = MockServer::start().await;
    let card = card(&upstream.uri());
    let app = build_app(AppState {
        supervisor: Some(BoundSupervisor::spawn_omlx(&card).unwrap()),
        cards: vec![card],
        ..AppState::default()
    });
    for (key, value) in [
        ("max_tokens", json!(40)),
        ("max_completion_tokens", json!(40)),
        ("temperature", json!(0.2)),
        ("top_p", json!(0.9)),
        ("stop", json!(["END"])),
        ("seed", json!(7)),
        ("presence_penalty", json!(0.5)),
        ("frequency_penalty", json!(0.2)),
        ("logit_bias", json!({"42": 1})),
        ("n", json!(1)),
        ("logprobs", json!(false)),
        ("top_logprobs", json!(2)),
        ("response_format", json!({"type":"json_object"})),
        ("tools", json!([])),
        ("tool_choice", json!("none")),
        ("parallel_tool_calls", json!(false)),
        ("functions", json!([])),
        ("function_call", json!("auto")),
        ("stream_options", json!({})),
        ("user", json!("caller")),
        ("metadata", json!({})),
        ("store", json!(false)),
        ("service_tier", json!("auto")),
        ("reasoning_effort", json!("low")),
        ("modalities", json!(["text"])),
        ("audio", json!({})),
        ("prediction", json!({})),
        ("future_option", Value::Null),
        ("max_tokens", Value::Null),
    ] {
        let mut body = json!({"model":"idoris/daily", "messages":[{"role":"user","content":"hi"}]});
        body[key] = value;
        let response = app.clone().oneshot(request(body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{key}");
        assert_eq!(response.headers()["x-idoris-served-locality"], "loopback");
        assert!(response.headers().contains_key("x-idoris-record-id"));
        let body: Value =
            serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes())
                .unwrap();
        assert_eq!(body["error"]["type"], "unsupported_field");
        assert_eq!(body["error"]["reason_code"], "unsupported_parameter");
        assert!(body["error"]["remediation"].as_str().unwrap().contains(key));
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn message_fields_and_multimodal_content_are_not_dropped() {
    let upstream = MockServer::start().await;
    let app = build_app(AppState {
        cards: vec![card(&upstream.uri())],
        ..AppState::default()
    });
    for messages in [
        json!(null),
        json!([{"role":"user","content":[{"type":"text","text":"hi"}]}]),
        json!([{"role":"assistant","content":"", "tool_calls":[]}]),
        json!([{"role":"user","content":"hi","name":"jason"}]),
        json!([{"role":"user"}]),
    ] {
        let response = app
            .clone()
            .oneshot(request(json!({"messages": messages})))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn resident_parameters_reach_the_upstream_unchanged() {
    let upstream = MockServer::start().await;
    let body = json!({"model":"Qwen3-0.6B-4bit", "max_tokens":40, "temperature":0.2, "stream":false, "messages":[{"role":"user","content":"hi"}]});
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_json(&body))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"model":"Qwen3-0.6B-4bit"})))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut card = card(&upstream.uri());
    card.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
    let app = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    });
    assert_eq!(
        app.oneshot(request(body)).await.unwrap().status(),
        StatusCode::OK
    );
}
