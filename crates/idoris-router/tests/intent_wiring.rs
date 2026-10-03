#![allow(clippy::unwrap_used, clippy::expect_used)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::LoadMode;
use idoris_router::{AppState, build_app};
use tower::ServiceExt;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn card(endpoint: &str) -> ComponentCard {
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    card.endpoint = endpoint.into();
    card.load_policy.as_mut().unwrap().mode = LoadMode::Resident;
    card
}

fn policy() -> idoris_contracts::RoutingPolicy {
    serde_yaml::from_str(
        r#"
routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [local], fail_closed: true }
  default: { tiers: [remote], fail_closed: true }
"#,
    )
    .unwrap()
}

fn request(intent: Option<&str>, prompt: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    if let Some(intent) = intent {
        builder = builder.header("X-iDoris-Intent", intent);
    }
    builder
        .body(Body::from(
            serde_json::json!({
                "model": "idoris/daily",
                "messages": [{"role": "user", "content": prompt}],
            })
            .to_string(),
        ))
        .unwrap()
}

#[tokio::test]
async fn detected_intent_changes_yaml_route_but_explicit_header_still_wins() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "intent-ok"}}]
        })))
        .mount(&upstream)
        .await;

    let app = build_app(AppState {
        cards: vec![card(&upstream.uri())],
        routing_policy: policy(),
        ..AppState::default()
    });

    let detected = app
        .clone()
        .oneshot(request(None, "fix this bug"))
        .await
        .unwrap();
    assert_eq!(detected.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    let explicit = app
        .oneshot(request(Some("chat"), "fix this bug"))
        .await
        .unwrap();
    assert_eq!(explicit.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}
