//! Concrete dispatch identities; all models and backends here are synthetic.
#![allow(clippy::unwrap_used)]
use super::*;
use idoris_contracts::ComponentCard;

use super::tests::{post_chat, resident_component_card, sample_component_card};
use http_body_util::BodyExt;
use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn bind(card: &mut ComponentCard, bindings: serde_json::Value) {
    card.extensions = Some(
        serde_json::from_value(
            json!({"_degradation":"No aliases when removed", "idoris_role_models": bindings}),
        )
        .unwrap(),
    );
}

#[tokio::test]
async fn role_binding_lifecycle_uses_exact_configured_model_and_preserves_override() {
    let mut card = sample_component_card("omlx");
    bind(
        &mut card,
        json!({"daily":"chat-model", "auto":"chat-model"}),
    );
    let adapter = Arc::new(MockAdapter::new(vec![
        ModelInfo {
            id: "first-ocr".into(),
            memory_gb: 1.0,
        },
        ModelInfo {
            id: "chat-model".into(),
            memory_gb: 2.0,
        },
        ModelInfo {
            id: "override".into(),
            memory_gb: 1.0,
        },
    ]));
    let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        runtimes: dispatch::BoundSupervisor::new(&card, handle).into(),
        cards: vec![card],
        ..AppState::default()
    });
    for (requested, actual) in [
        ("idoris/daily", "chat-model"),
        ("idoris/auto", "chat-model"),
        ("override", "override"),
    ] {
        let response = app
            .clone()
            .oneshot(post_chat(
                &json!({"model":requested,"messages":[]}).to_string(),
                &[("x-idoris-session", "same-session")],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["model"], actual);
        assert_eq!(adapter.load_call_count(actual), 1);
    }
    assert_eq!(adapter.load_call_count("omlx"), 0);
    assert_eq!(adapter.load_call_count("first-ocr"), 0);
    for role in ["vision", "deep", "embed"] {
        let response = app
            .clone()
            .oneshot(post_chat(
                &json!({"model":format!("idoris/{role}"),"messages":[]}).to_string(),
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}

#[tokio::test]
async fn role_binding_resident_posts_concrete_model_for_buffered_stream_and_override() {
    let backend = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw("data: [DONE]\n\n", "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({"ok":true}))
            }
        })
        .mount(&backend)
        .await;
    let mut card = resident_component_card("resident-provider", &backend.uri());
    bind(&mut card, json!({"daily":"resident-chat"}));
    let app = build_app(AppState {
        cards: vec![card],
        ..AppState::default()
    });
    for (model, stream) in [
        ("idoris/daily", false),
        ("idoris/daily", true),
        ("explicit", false),
    ] {
        let response = app
            .clone()
            .oneshot(post_chat(
                &json!({"model":model,"messages":[],"stream":stream}).to_string(),
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        response.into_body().collect().await.unwrap();
    }
    let requests = backend.received_requests().await.unwrap();
    let models: Vec<_> = requests
        .iter()
        .map(|r| serde_json::from_slice::<serde_json::Value>(&r.body).unwrap()["model"].clone())
        .collect();
    assert_eq!(
        models,
        vec![
            json!("resident-chat"),
            json!("resident-chat"),
            json!("explicit")
        ]
    );
}

#[tokio::test]
async fn role_binding_negative_controls_never_post_or_load() {
    let backend = MockServer::start().await;
    let mut base = resident_component_card("provider", &backend.uri());
    for invalid in [
        json!(null),
        json!({}),
        json!({"daily":"idoris/daily"}),
        json!({"daily":3}),
        json!({"bogus":"chat"}),
    ] {
        bind(&mut base, invalid);
        let response = build_app(AppState {
            cards: vec![base.clone()],
            ..AppState::default()
        })
        .oneshot(post_chat(r#"{"model":"idoris/daily","messages":[]}"#, &[]))
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
    base.extensions = None;
    let response = build_app(AppState {
        cards: vec![base.clone()],
        ..AppState::default()
    })
    .oneshot(post_chat(r#"{"model":"idoris/auto","messages":[]}"#, &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    bind(&mut base, json!({"daily":"remote-chat"}));
    base.provider.locality = idoris_contracts::provider::Locality::Remote;
    base.provider.privacy_class = idoris_contracts::common::PrivacyClass::Any;
    base.privacy_class = idoris_contracts::common::PrivacyClass::Any;
    let response = build_app(AppState {
        cards: vec![base],
        ..AppState::default()
    })
    .oneshot(post_chat(r#"{"model":"idoris/daily","messages":[]}"#, &[]))
    .await
    .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(backend.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn role_binding_missing_model_and_capacity_fail_before_load() {
    for (target, memory) in [("missing", 1.0), ("chat", 1000.0)] {
        let mut card = sample_component_card("omlx");
        bind(&mut card, json!({"daily":target}));
        let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
            id: "chat".into(),
            memory_gb: memory,
        }]));
        let handle = Supervisor::spawn(adapter.clone(), SupervisorConfig::default()).unwrap();
        let response = build_app(AppState {
            runtimes: dispatch::BoundSupervisor::new(&card, handle).into(),
            cards: vec![card],
            ..AppState::default()
        })
        .oneshot(post_chat(r#"{"model":"idoris/daily","messages":[]}"#, &[]))
        .await
        .unwrap();
        assert!(response.status().is_server_error());
        assert_eq!(adapter.load_call_count("chat"), 0);
        assert_eq!(adapter.load_call_count("missing"), 0);
    }
}

#[test]
fn role_binding_loader_rejects_malformed_configuration() {
    let dir = tempfile::tempdir().unwrap();
    let mut card = sample_component_card("configured");
    bind(&mut card, json!({"daily":"  "}));
    std::fs::write(
        dir.path().join("card.yaml"),
        serde_yaml::to_string(&card).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        components::load_components(dir.path(), false),
        Err(components::LoadError::Invalid { .. })
    ));
}

#[test]
fn role_binding_only_configured_providers_enter_role_selection_even_with_fallback() {
    let mut unrelated = sample_component_card("aaa-unrelated");
    unrelated.extensions = None;
    let mut configured = sample_component_card("zzz-configured");
    bind(&mut configured, json!({"daily":"exact-chat"}));
    let mut headers = HeaderMap::new();
    headers.insert(
        "x-idoris-fallback",
        HeaderValue::from_static("next_in_chain"),
    );
    let profile = profile::parse_profile(
        &headers,
        Some("idoris/daily"),
        idoris_contracts::DeployMode::Personal,
    )
    .unwrap();
    let selected =
        dispatch::select(&[unrelated.clone(), configured.clone()], &profile, "").unwrap();
    assert_eq!(selected.card.provider.id, "zzz-configured");
    let profile = profile::parse_profile(
        &headers,
        Some("idoris/vision"),
        idoris_contracts::DeployMode::Personal,
    )
    .unwrap();
    assert!(dispatch::select(&[unrelated, configured], &profile, "").is_err());
}

#[test]
fn role_binding_subscription_configuration_is_rejected_and_http_controls_remain_valid() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("card.yaml");
    let mut subscription: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/subscription.yaml")).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("x-idoris-privacy", HeaderValue::from_static("any"));
    // Even an empty mapping is unsupported; concrete requests must not silently
    // ignore a configured target in favor of the relay's fixed identity.
    for mapping in [json!({}), json!({"daily":"arbitrary", "auto":"arbitrary"})] {
        subscription
            .extensions
            .as_mut()
            .unwrap()
            .insert("idoris_role_models".into(), mapping);
        std::fs::write(&path, serde_yaml::to_string(&subscription).unwrap()).unwrap();
        match components::load_components(dir.path(), false) {
            Err(components::LoadError::Invalid { message, .. }) => {
                assert!(message.contains("unsupported on subscription"), "{message}");
            }
            other => panic!("expected unsupported binding rejection, got {other:?}"),
        }
        for model in ["idoris/daily", "idoris/auto", "explicit-model"] {
            let profile = profile::parse_profile(
                &headers,
                Some(model),
                idoris_contracts::DeployMode::Personal,
            )
            .unwrap();
            assert!(dispatch::select(std::slice::from_ref(&subscription), &profile, "").is_err());
        }
    }
    subscription
        .extensions
        .as_mut()
        .unwrap()
        .remove("idoris_role_models");
    std::fs::write(&path, serde_yaml::to_string(&subscription).unwrap()).unwrap();
    assert_eq!(
        components::load_components(dir.path(), false).unwrap(),
        [subscription.clone()]
    );
    let concrete = profile::parse_profile(
        &headers,
        Some("explicit-model"),
        idoris_contracts::DeployMode::Personal,
    )
    .unwrap();
    assert!(dispatch::select(&[subscription], &concrete, "").is_ok());
    let lifecycle: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    for mut http in [
        lifecycle,
        resident_component_card("resident", "http://127.0.0.1:1234"),
    ] {
        bind(
            &mut http,
            json!({"daily":"installed-chat", "auto":"installed-chat"}),
        );
        std::fs::write(&path, serde_yaml::to_string(&http).unwrap()).unwrap();
        assert_eq!(
            components::load_components(dir.path(), false).unwrap(),
            [http.clone()]
        );
        for model in ["idoris/daily", "idoris/auto", "explicit-model"] {
            let profile = profile::parse_profile(
                &headers,
                Some(model),
                idoris_contracts::DeployMode::Personal,
            )
            .unwrap();
            assert_eq!(
                dispatch::select(std::slice::from_ref(&http), &profile, "")
                    .unwrap()
                    .card,
                http
            );
        }
    }
}
