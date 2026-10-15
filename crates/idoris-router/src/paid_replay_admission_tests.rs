#![allow(clippy::unwrap_used)]
use super::*;
use axum::http::StatusCode;
use std::time::Duration;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn paid_waiters_and_cache_hits_do_not_require_another_full_quote() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(json!({
                    "choices":[{"message":{"role":"assistant","content":"answer"}}],
                    "usage":{"prompt_tokens":1,"completion_tokens":1100,"total_tokens":1101}
                })),
        )
        .expect(1)
        .mount(&upstream)
        .await;
    let (_dir, ledger) = super::tests::configured_budget_ledger(4000);
    let ledger = Arc::new(ledger);
    let mut card = super::tests::resident_component_card("paid", &upstream.uri());
    card.provider.cost = super::tests::paid_component_card("paid").provider.cost;
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    let request = || {
        super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"same"}]}"#,
            &[(HEADER_REQUEST_ID, "paid-headroom")],
        )
    };
    let leader = tokio::spawn(app.clone().oneshot(request()));
    tokio::time::timeout(Duration::from_secs(2), async {
        while upstream.received_requests().await.unwrap().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let waiter = tokio::spawn(app.clone().oneshot(request()));
    let first = leader.await.unwrap().unwrap();
    assert_eq!(first.status(), StatusCode::OK);
    let replay = waiter.await.unwrap().unwrap();
    assert_eq!(
        replay.status(),
        StatusCode::OK,
        "waiting for the same POST must not reserve twice"
    );
    assert!(replay.headers().contains_key(HEADER_CACHED));
    let cached = app.oneshot(request()).await.unwrap();
    assert_eq!(
        cached.status(),
        StatusCode::OK,
        "already-paid bytes require zero-cost admission"
    );
    assert!(cached.headers().contains_key(HEADER_CACHED));
    assert!(!cached.headers().contains_key(HEADER_COST_MINOR));
    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(view.spent_minor, 2201);
    assert_eq!(view.reserved_minor, 0);
    upstream.verify().await;
}
