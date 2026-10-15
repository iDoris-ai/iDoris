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

fn request(model: Option<&str>, headers: &[(&str, &str)], prompt: &str) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let mut body = serde_json::json!({
        "messages": [{"role": "user", "content": prompt}],
    });
    if let Some(model) = model {
        body["model"] = serde_json::json!(model);
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

fn remote_card(endpoint: &str) -> ComponentCard {
    let mut card = card(endpoint);
    card.provider.id = "remote".into();
    card.provider.tier = idoris_contracts::common::Tier::Remote;
    card.provider.locality = idoris_contracts::provider::Locality::Remote;
    card.provider.privacy_class = idoris_contracts::common::PrivacyClass::Any;
    card.privacy_class = idoris_contracts::common::PrivacyClass::Any;
    card.allowed_egress = vec![idoris_contracts::component_card::Egress::Internet];
    card.fail_closed = false;
    card.fallback_policy = idoris_contracts::common::FallbackPolicy::NextInChain;
    card
}

fn app(cards: Vec<ComponentCard>, routing_policy: idoris_contracts::RoutingPolicy) -> axum::Router {
    build_app(AppState {
        cards,
        routing_policy,
        ..AppState::default()
    })
}

async fn error_type(response: axum::response::Response) -> String {
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    json["error"]["type"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn only_auto_without_an_intent_header_changes_the_yaml_route() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "intent-ok"}}]
        })))
        .mount(&upstream)
        .await;
    let app = app(vec![card(&upstream.uri())], policy());

    let detected = app
        .clone()
        .oneshot(request(Some("idoris/auto"), &[], "fix this bug"))
        .await
        .unwrap();
    assert_eq!(detected.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    let explicit = app
        .clone()
        .oneshot(request(
            Some("idoris/auto"),
            &[("X-iDoris-Intent", "chat")],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(explicit.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    let missed = app
        .clone()
        .oneshot(request(Some("idoris/auto"), &[], "tell me a joke"))
        .await
        .unwrap();
    assert_eq!(missed.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    for model in [
        Some("idoris/daily"),
        Some("idoris/fast"),
        Some("idoris/deep"),
        Some("idoris/vision"),
        Some("idoris/embed"),
        Some("idoris/rerank"),
        Some("idoris/decide"),
        Some("org/opaque-model"),
        None,
    ] {
        let response = app
            .clone()
            .oneshot(request(model, &[], "fix this bug"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    }

    let declared = app
        .clone()
        .oneshot(request(
            Some("idoris/vision"),
            &[("X-iDoris-Intent", "coding")],
            "tell me a joke",
        ))
        .await
        .unwrap();
    assert_eq!(declared.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);

    let blank_intent = app
        .clone()
        .oneshot(request(
            Some("idoris/auto"),
            &[("X-iDoris-Intent", " ")],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(blank_intent.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_type(blank_intent).await, "invalid_header");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);

    let bad_complexity = app
        .oneshot(request(
            Some("idoris/auto"),
            &[("X-iDoris-Complexity", "ultra")],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(bad_complexity.status(), StatusCode::BAD_REQUEST);
    assert_eq!(error_type(bad_complexity).await, "invalid_header");
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
}

#[tokio::test]
async fn declared_complexity_is_not_replaced_by_auto_intent_detection() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "intent-ok"}}]
        })))
        .mount(&upstream)
        .await;
    let routing_policy = serde_yaml::from_str(
        r#"
routing_policy:
  version: 1
  rules:
    - if: { intent: coding, complexity: simple }
      then: { tiers: [local], fail_closed: true }
    - if: { complexity: complex }
      then: { tiers: [remote], fail_closed: true }
  default: { tiers: [remote], fail_closed: true }
"#,
    )
    .unwrap();
    let app = app(vec![card(&upstream.uri())], routing_policy);

    let detected_simple = app
        .clone()
        .oneshot(request(
            Some("idoris/auto"),
            &[("x-idoris-privacy", "any")],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(detected_simple.status(), StatusCode::OK);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    let declared_complex = app
        .clone()
        .oneshot(request(
            Some("idoris/auto"),
            &[
                ("x-idoris-privacy", "any"),
                ("x-idoris-complexity", "complex"),
            ],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(declared_complex.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);

    let chat_complex = app
        .oneshot(request(
            Some("idoris/daily"),
            &[
                ("x-idoris-privacy", "any"),
                ("x-idoris-complexity", "complex"),
            ],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(chat_complex.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn local_only_drops_remote_candidates_in_every_call_mode() {
    let local = MockServer::start().await;
    let remote = MockServer::start().await;
    for server in [&local, &remote] {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "choices": [{"message": {"content": "served"}}]
            })))
            .mount(server)
            .await;
    }
    let routing_policy = serde_yaml::from_str(
        r#"
routing_policy:
  version: 1
  rules:
    - if: { intent: coding }
      then: { tiers: [remote], fail_closed: true }
  default: { tiers: [local], fail_closed: true }
"#,
    )
    .unwrap();
    let app = app(
        vec![card(&local.uri()), remote_card(&remote.uri())],
        routing_policy,
    );

    let chat = app
        .clone()
        .oneshot(request(Some("idoris/daily"), &[], "fix this bug"))
        .await
        .unwrap();
    assert_eq!(chat.status(), StatusCode::OK);
    assert_eq!(local.received_requests().await.unwrap().len(), 1);
    assert_eq!(remote.received_requests().await.unwrap().len(), 0);

    let auto = app
        .clone()
        .oneshot(request(Some("idoris/auto"), &[], "fix this bug"))
        .await
        .unwrap();
    assert_eq!(auto.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_type(auto).await, "local_only_unavailable");
    assert_eq!(local.received_requests().await.unwrap().len(), 1);
    assert_eq!(remote.received_requests().await.unwrap().len(), 0);

    let declared = app
        .clone()
        .oneshot(request(
            Some("org/opaque-model"),
            &[("X-iDoris-Intent", "coding")],
            "tell me a joke",
        ))
        .await
        .unwrap();
    assert_eq!(declared.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error_type(declared).await, "local_only_unavailable");
    assert_eq!(remote.received_requests().await.unwrap().len(), 0);

    let remote_allowed = app
        .oneshot(request(
            Some("idoris/auto"),
            &[("x-idoris-privacy", "any")],
            "fix this bug",
        ))
        .await
        .unwrap();
    assert_eq!(remote_allowed.status(), StatusCode::OK);
    assert_eq!(remote.received_requests().await.unwrap().len(), 1);
    assert_eq!(local.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn default_policy_chat_mode_still_selects_the_local_candidate() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"message": {"content": "default-ok"}}]
        })))
        .mount(&upstream)
        .await;
    let routing_policy: idoris_contracts::RoutingPolicy =
        serde_yaml::from_str(include_str!("../../../config/routing-policy.yaml")).unwrap();
    let app = app(vec![card(&upstream.uri())], routing_policy);
    for model in [Some("idoris/daily"), Some("org/opaque-model"), None] {
        for prompt in ["hi", "fix this bug"] {
            let response = app
                .clone()
                .oneshot(request(model, &[], prompt))
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 6);
}
