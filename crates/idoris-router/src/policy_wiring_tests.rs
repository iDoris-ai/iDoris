//! Request-level regression coverage for applying YAML routing policy
//! before both the resident HTTP and local Supervisor execution paths.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use http_body_util::BodyExt;
use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::provider::Locality;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::tests::{
    configured_budget_ledger, paid_component_card, post_chat, resident_component_card,
    sample_component_card,
};
use super::{AppState, HEADER_SERVED_LOCALITY, build_app, dispatch};

fn policy(tiers: &str, fail_closed: bool) -> idoris_contracts::RoutingPolicy {
    serde_yaml::from_str(&format!(
        "routing_policy:\n  version: 1\n  rules: []\n  default: {{ tiers: [{tiers}], fail_closed: {fail_closed} }}\n"
    ))
    .unwrap()
}
fn remote(
    mut card: idoris_contracts::ComponentCard,
    endpoint: &str,
) -> idoris_contracts::ComponentCard {
    card.endpoint = endpoint.to_string();
    card.provider.tier = Tier::Remote;
    card.provider.locality = Locality::Remote;
    card.provider.privacy_class = PrivacyClass::Any;
    card.privacy_class = PrivacyClass::Any;
    card.allowed_egress = vec![idoris_contracts::component_card::Egress::Internet];
    card
}

#[tokio::test]
async fn yaml_policy_gates_proxy_and_supervisor_and_preserves_privacy() {
    for resident in [false, true] {
        for (tier, fail_closed) in [
            ("local", true),
            ("remote", true),
            ("lora", true),
            ("lora", false),
        ] {
            let local_upstream = MockServer::start().await;
            let remote_upstream = MockServer::start().await;
            for (server, selected) in [
                (&local_upstream, resident && tier == "local"),
                (&remote_upstream, resident && tier == "remote"),
            ] {
                Mock::given(method("POST"))
                    .respond_with(
                        ResponseTemplate::new(200)
                            .set_body_json(serde_json::json!({"marker":"policy-ok"})),
                    )
                    .expect(u64::from(selected))
                    .mount(server)
                    .await;
            }

            let local_card = if resident {
                resident_component_card("a-local", &local_upstream.uri())
            } else {
                sample_component_card("a-local")
            };
            let remote_card = remote(
                if resident {
                    resident_component_card("z-remote", &remote_upstream.uri())
                } else {
                    sample_component_card("z-remote")
                },
                &remote_upstream.uri(),
            );
            let adapter = Arc::new(idoris_backend::MockAdapter::new(
                ["a-local", "z-remote"]
                    .map(|id| idoris_backend::ModelInfo {
                        id: id.into(),
                        memory_gb: 1.0,
                    })
                    .to_vec(),
            ));
            let supervisor =
                idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
            let (cards, bound) = match tier {
                "local" => (
                    vec![local_card.clone(), remote_card],
                    Some(dispatch::BoundSupervisor::new(&local_card, supervisor)),
                ),
                "remote" => (
                    vec![local_card, remote_card.clone()],
                    Some(dispatch::BoundSupervisor::new(&remote_card, supervisor)),
                ),
                _ => (vec![local_card, remote_card], None),
            };
            let app = build_app(AppState {
                cards,
                runtimes: bound.into(),
                routing_policy: policy(tier, fail_closed),
                ..AppState::default()
            });
            let response = app.oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"policy-ok"}]}"#,
                &[("x-idoris-privacy", "any")],
            )).await.unwrap();
            if tier == "lora" {
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                );
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    json["error"]["type"],
                    if fail_closed {
                        "local_only_unavailable"
                    } else {
                        "no_candidate"
                    }
                );
                assert_eq!(adapter.load_call_count("a-local"), 0);
                assert_eq!(adapter.load_call_count("z-remote"), 0);
            } else {
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::OK,
                    "resident={resident}, tier={tier}"
                );
                assert_eq!(
                    response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
                    if tier == "remote" {
                        "remote"
                    } else {
                        "loopback"
                    }
                );
                let bytes = response.into_body().collect().await.unwrap().to_bytes();
                let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                if resident {
                    assert_eq!(json["marker"], "policy-ok");
                    assert_eq!(adapter.load_call_count("a-local"), 0);
                    assert_eq!(adapter.load_call_count("z-remote"), 0);
                } else {
                    assert!(
                        json["choices"][0]["message"]["content"]
                            .as_str()
                            .unwrap()
                            .contains("policy-ok")
                    );
                    assert_eq!(
                        adapter.load_call_count("a-local"),
                        u32::from(tier == "local")
                    );
                    assert_eq!(
                        adapter.load_call_count("z-remote"),
                        u32::from(tier == "remote")
                    );
                }
            }
            local_upstream.verify().await;
            remote_upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn local_only_request_cannot_use_policy_selected_remote_candidate() {
    for resident in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200))
            .expect(0)
            .mount(&upstream)
            .await;
        let mut card = remote(
            if resident {
                resident_component_card("remote", &upstream.uri())
            } else {
                sample_component_card("remote")
            },
            &upstream.uri(),
        );
        card.provider.cost = paid_component_card("remote").provider.cost;
        let (_dir, ledger) = configured_budget_ledger(1);
        let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
            idoris_backend::ModelInfo {
                id: "remote".into(),
                memory_gb: 1.0,
            },
        ]));
        let supervisor =
            idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
        let bound = dispatch::BoundSupervisor::new(&card, supervisor);
        let response = build_app(AppState {
            cards: vec![card],
            runtimes: Some(bound).into(),
            routing_policy: policy("remote", false),
            budget_ledger: Some(Arc::new(ledger)),
            ..AppState::default()
        })
        .oneshot(post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"private"}]}"#,
            &[("x-idoris-privacy", "local_only")],
        ))
        .await
        .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "local_only_unavailable");
        assert_eq!(adapter.load_call_count("remote"), 0);
        upstream.verify().await;
    }
}

#[tokio::test]
async fn concrete_model_is_not_rewritten_to_the_policy_selected_provider() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200))
        .expect(0)
        .mount(&upstream)
        .await;

    let local = sample_component_card("a-local");
    let remote = remote(sample_component_card("z-remote"), &upstream.uri());
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "z-remote".into(),
            memory_gb: 1.0,
        },
    ]));
    let supervisor =
        idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
    let response = build_app(AppState {
        cards: vec![local, remote.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(&remote, supervisor)).into(),
        routing_policy: policy("remote", false),
        ..AppState::default()
    })
    .oneshot(post_chat(
        r#"{"model":"a-local","messages":[{"role":"user","content":"policy model check"}]}"#,
        &[("x-idoris-privacy", "any")],
    ))
    .await
    .unwrap();

    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["reason_code"], "model_not_found");
    assert_eq!(adapter.load_call_count("z-remote"), 0);
    upstream.verify().await;
}

#[tokio::test]
async fn yaml_policy_precedes_budget_on_both_execution_paths() {
    for resident in [false, true] {
        for (tier, expected_status, expected_type) in [
            (
                "local",
                axum::http::StatusCode::PAYMENT_REQUIRED,
                "budget_exceeded",
            ),
            (
                "remote",
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                "no_candidate",
            ),
        ] {
            let upstream = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200))
                .expect(0)
                .mount(&upstream)
                .await;
            let (_dir, ledger) = configured_budget_ledger(1);
            let mut card = if resident {
                resident_component_card("paid", &upstream.uri())
            } else {
                sample_component_card("paid")
            };
            card.provider.cost = paid_component_card("paid").provider.cost;
            let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
                idoris_backend::ModelInfo {
                    id: "paid".into(),
                    memory_gb: 1.0,
                },
            ]));
            let supervisor =
                idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
            let bound = dispatch::BoundSupervisor::new(&card, supervisor);
            let response = build_app(AppState {
                cards: vec![card],
                runtimes: Some(bound).into(),
                routing_policy: policy(tier, false),
                budget_ledger: Some(Arc::new(ledger)),
                ..AppState::default()
            })
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"charge me"}]}"#,
                &[("x-idoris-privacy", "any")],
            ))
            .await
            .unwrap();
            assert_eq!(
                response.status(),
                expected_status,
                "resident={resident}, tier={tier}"
            );
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(
                json["error"]["type"], expected_type,
                "resident={resident}, tier={tier}"
            );
            assert_eq!(adapter.load_call_count("paid"), 0);
            upstream.verify().await;
        }
    }
}

#[tokio::test]
async fn yaml_selected_paid_candidate_reaches_supervisor_admission_after_budget() {
    let (_dir, ledger) = configured_budget_ledger(1_000_000);
    let card = paid_component_card("paid-local");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "paid-local".into(),
            memory_gb: 1.0,
        },
    ]));
    let supervisor =
        idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
    let response = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(&card, supervisor)).into(),
        routing_policy: policy("local", true),
        budget_ledger: Some(Arc::new(ledger)),
        ..AppState::default()
    })
    .oneshot(post_chat(
        r#"{"model":"idoris/daily","messages":[{"role":"user","content":"budget then admission"}]}"#,
        &[],
    ))
    .await
    .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(adapter.load_call_count("paid-local"), 1);
    assert!(response.headers().contains_key(super::HEADER_COST_MINOR));
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert!(
        json["choices"][0]["message"]["content"]
            .as_str()
            .unwrap()
            .contains("budget then admission")
    );
}
