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
) {
    let (dir, ledger) = super::tests::configured_budget_ledger(limit_minor);
    let ledger = Arc::new(ledger);
    let card = super::tests::paid_component_card("paid-1");
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: "paid-1".to_string(),
        memory_gb: 1.0,
    }]));
    let supervisor = Supervisor::spawn(adapter, SupervisorConfig::default()).unwrap();
    let app = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(&card, supervisor)).into(),
        budget_ledger: Some(ledger.clone()),
        ..AppState::default()
    });
    (dir, ledger, app)
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
    // Each request reserves the router's own pre-execution estimate ("hi"
    // is the joined prompt); leave room for ten such reservations.
    let reservation = budget::estimate_cost_minor(
        &super::tests::paid_component_card("paid-1").provider.cost,
        "hi",
    )
    .unwrap();
    assert!(reservation > 0);
    let limit = reservation * 10;
    let (_dir, ledger, app) = paid_app(limit);
    let tasks: Vec<_> = (0..100)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move { app.oneshot(post_chat(BODY, &[])).await.unwrap() })
        })
        .collect();
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

    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(ok + payment_required, 100);
    assert!(ok >= 1, "the budget must admit some requests");
    assert!(
        payment_required >= 1,
        "the budget must actually be contended"
    );
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
