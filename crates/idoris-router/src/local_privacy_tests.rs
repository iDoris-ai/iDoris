//! HTTP regression coverage for local-only privacy validation.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use http_body_util::BodyExt;
use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::Locality;
use idoris_policy::{AdmissionStatus, Card, Role, validate_registration};
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::tests::{post_chat, resident_component_card, sample_component_card};
use super::{AppState, HEADER_SERVED_LOCALITY, build_app, dispatch};

#[tokio::test]
async fn local_only_loopback_requires_trusted_privacy_on_both_paths() {
    for resident in [false, true] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"marker":"trusted-local-ok"})),
            )
            .expect(u64::from(resident))
            .mount(&upstream)
            .await;

        let mut card = if resident {
            resident_component_card("local-any", &upstream.uri())
        } else {
            let mut card = sample_component_card("local-any");
            card.endpoint = upstream.uri();
            card.load_policy = Some(LoadPolicy {
                mode: LoadMode::OnDemand,
                keepalive: Keepalive::IdleTtl { idle_ttl_s: 300 },
                admission: Admission::Coexist,
            });
            card
        };
        card.provider.tier = Tier::Local;
        card.provider.locality = Locality::Loopback;
        card.allowed_egress = vec![idoris_contracts::component_card::Egress::Loopback];
        let registration = |component: idoris_contracts::ComponentCard| Card {
            component,
            roles: vec![Role::Daily],
            experiment: false,
            min_ram_gb: 0.0,
            estimated_cost_minor: Some(0),
            admission_status: AdmissionStatus::Ready,
        };
        card.provider.privacy_class = PrivacyClass::Any;
        card.privacy_class = PrivacyClass::Any;
        assert_eq!(validate_registration(&[registration(card.clone())]), Ok(()));

        let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
            idoris_backend::ModelInfo {
                id: "local-any".into(),
                memory_gb: 1.0,
            },
        ]));
        let supervisor =
            idoris_backend::Supervisor::spawn(adapter.clone(), Default::default()).unwrap();
        let bound = (!resident).then(|| dispatch::BoundSupervisor::new(&card, supervisor));
        let rejected = build_app(AppState {
            cards: vec![card.clone()],
            supervisor: bound.clone(),
            ..AppState::default()
        })
        .oneshot(post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"private"}]}"#,
            &[("x-idoris-privacy", "local_only")],
        ))
        .await
        .unwrap();
        assert_eq!(
            rejected.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let body = rejected.into_body().collect().await.unwrap().to_bytes();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(error["error"]["type"], "local_only_unavailable");
        assert_eq!(adapter.load_call_count("local-any"), 0);
        assert!(upstream.received_requests().await.unwrap().is_empty());

        card.provider.privacy_class = PrivacyClass::LocalOnly;
        card.privacy_class = PrivacyClass::LocalOnly;
        assert_eq!(validate_registration(&[registration(card.clone())]), Ok(()));
        let response = build_app(AppState {
            cards: vec![card],
            supervisor: bound,
            ..AppState::default()
        })
        .oneshot(post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"private"}]}"#,
            &[("x-idoris-privacy", "local_only")],
        ))
        .await
        .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get(HEADER_SERVED_LOCALITY).unwrap(),
            "loopback"
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if resident {
            assert_eq!(json["marker"], "trusted-local-ok");
            assert_eq!(adapter.load_call_count("local-any"), 0);
        } else {
            assert_eq!(
                json["choices"][0]["message"]["content"],
                "mock reply to: private"
            );
            assert_eq!(adapter.load_call_count("local-any"), 1);
        }
        upstream.verify().await;
    }
}
