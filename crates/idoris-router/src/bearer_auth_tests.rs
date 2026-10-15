#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use http_body_util::BodyExt;
use idoris_contracts::ComponentAuth;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use crate::tests::{post_chat, resident_component_card};
use crate::upstream_auth::{self, EnvLookup};
use crate::{AppState, build_app};

#[tokio::test]
async fn caller_authorization_does_not_become_the_upstream_credential() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(serde_json::json!({"marker":"upstream"})),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let mut card = resident_component_card("local-any", &upstream.uri());
    card.auth = Some(ComponentAuth::Bearer {
        env: "IDORIS_CARD_BEARER_FOR_TEST".to_string(),
    });
    let bearers =
        upstream_auth::prepare_upstream_auth(&[card.clone()], &mut BTreeMap::new(), &|name| {
            assert_eq!(name, "IDORIS_CARD_BEARER_FOR_TEST");
            EnvLookup::Value("configured-upstream-token".to_string())
        })
        .unwrap();
    let mut state = AppState {
        cards: vec![card],
        ..AppState::default()
    };
    upstream_auth::install_upstream_bearers(&mut state, bearers);
    let response = build_app(state)
        .oneshot(post_chat(
            r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
            &[("authorization", "Bearer caller-token")],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(json["marker"], "upstream");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        "Bearer configured-upstream-token"
    );
}
