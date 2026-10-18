//! M5 acceptance: "100 concurrent requests cannot overrun the budget",
//! end to end through the router (the ledger's own race tests live in
//! `idoris-tenancy/tests/budget_concurrency.rs`).
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::StatusCode;
use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
use tower::ServiceExt;

use super::tests::post_chat;
use super::*;

const BODY: &str = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

fn paid_app(
    limit_minor: i64,
) -> (
    tempfile::TempDir,
    Arc<idoris_tenancy::budget::BudgetLedger>,
    Router,
    Arc<MockAdapter>,
) {
    let (dir, ledger) = super::tests::configured_budget_ledger(limit_minor);
    let ledger = Arc::new(ledger);
    let card = super::tests::paid_component_card("paid-1");
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: "paid-1".to_string(),
        memory_gb: 1.0,
    }]));
    // Keep a handle with the same inner state so the test can set a delay.
    let as_adapter: Arc<dyn idoris_backend::RuntimeAdapter> = adapter.clone();
    let supervisor = Supervisor::spawn(as_adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(&card, supervisor)).into(),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    (dir, ledger, app, adapter)
}

fn cost_of(response: &Response) -> i64 {
    response
        .headers()
        .get(HEADER_COST_MINOR)
        .unwrap()
        .to_str()
        .unwrap()
        .parse()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn one_hundred_concurrent_paid_requests_never_overrun_the_budget() {
    // Each request reserves the router's own pre-execution estimate ("hi" is the
    // joined prompt); leave room for exactly ten such reservations.
    let reservation = budget::estimate_cost_minor(
        &super::tests::paid_component_card("paid-1").provider.cost,
        "hi",
    )
    .unwrap();
    assert!(reservation > 0);
    let limit = reservation * 10;
    let (_dir, ledger, app, adapter) = paid_app(limit);

    // Hold every admitted request inside the backend so the window in which
    // reservations are *held* (not yet settled) is observable. Without this the
    // assertions only run after everything settled, and a ledger that admits
    // twice the budget still looks fine - which is exactly the review's finding.
    adapter.set_chat_delay("paid-1", std::time::Duration::from_millis(2_500));

    let tasks: Vec<_> = (0..100)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move { app.oneshot(post_chat(BODY, &[])).await.unwrap() })
        })
        .collect();

    // While the admitted requests are held: the ledger must never let
    // `reserved + spent` past the limit, and it must admit exactly
    // `limit / reservation` of them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let held = loop {
        let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        let held = view.reserved_minor / reservation;
        assert!(
            view.reserved_minor + view.spent_minor <= limit,
            "in flight: reserved {} + spent {} > limit {limit}",
            view.reserved_minor,
            view.spent_minor
        );
        if held == limit / reservation {
            assert_eq!(
                view.spent_minor, 0,
                "nothing may settle while every admitted request is still held"
            );
            break held;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "only {held} of {} reservations were taken while 100 requests were in flight",
            limit / reservation
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    };
    assert_eq!(
        held,
        limit / reservation,
        "the budget must admit exactly limit / reservation requests"
    );

    let mut ok = 0;
    let mut payment_required = 0;
    let mut charged = 0;
    for task in tasks {
        let response = task.await.unwrap();
        match response.status() {
            StatusCode::OK => {
                ok += 1;
                charged += cost_of(&response);
            }
            StatusCode::PAYMENT_REQUIRED => payment_required += 1,
            other => panic!("unexpected status {other}"),
        }
    }

    assert_eq!(ok + payment_required, 100);
    assert_eq!(
        ok,
        limit / reservation,
        "exactly the ten reservations the budget affords may be admitted"
    );
    assert_eq!(
        payment_required, 90,
        "the other ninety must be refused with 402"
    );

    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert!(
        view.spent_minor <= limit,
        "spent {} > limit {limit}",
        view.spent_minor
    );
    assert_eq!(
        view.spent_minor, charged,
        "ledger and X-iDoris-Cost-Minor disagree"
    );
    assert_eq!(view.reserved_minor, 0, "no reservation may leak");
}
