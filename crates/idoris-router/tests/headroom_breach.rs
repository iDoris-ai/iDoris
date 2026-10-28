//! Allocated budgets keep 1GB of headroom; only a chat request carrying
//! `X-iDoris-Headroom-Breach: confirmed` may spend it, one request at a time.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use idoris_backend::{MockAdapter, ModelInfo};
use idoris_contracts::ComponentCard;
use idoris_router::{AppState, build_app, runtime::RuntimeRegistry};
use serde_json::{Value, json};
use tower::ServiceExt;

const BUDGET: u64 = 5_000_000_000;

fn app(budget: Option<u64>) -> (Router, Arc<MockAdapter>) {
    let card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    let adapter = Arc::new(MockAdapter::new(
        [("big", 4.5), ("small", 1.0), ("huge", 6.0)]
            .map(|(id, memory_gb)| ModelInfo {
                id: id.into(),
                memory_gb,
            })
            .to_vec(),
    ));
    let runtimes =
        RuntimeRegistry::spawn_with_factory_and_budget(std::slice::from_ref(&card), budget, {
            let adapter = adapter.clone();
            move |_| Ok(adapter.clone())
        })
        .unwrap();
    let app = build_app(AppState {
        cards: vec![card],
        runtimes,
        ..AppState::default()
    });
    (app, adapter)
}

async fn post(
    app: &Router,
    path: &str,
    model: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = json!({"model": model, "messages": [{"role": "user", "content": "hi"}]});
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn chat(app: &Router, model: &str, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    post(app, "/v1/chat/completions", model, headers).await
}

const CONFIRM: (&str, &str) = ("X-iDoris-Headroom-Breach", "confirmed");

#[tokio::test]
async fn unconfirmed_load_into_the_headroom_asks_for_confirmation_and_loads_nothing() {
    let (app, adapter) = app(Some(BUDGET));
    let (status, body) = chat(&app, "big", &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["type"], "headroom_confirmation_required");
    assert_eq!(
        body["error"]["reason_code"],
        "headroom_confirmation_required"
    );
    let remediation = body["error"]["remediation"].as_str().unwrap();
    assert!(remediation.contains("X-iDoris-Headroom-Breach: confirmed"));
    assert!(!remediation.contains("runtime:") && !remediation.contains("omlx"));
    assert_eq!(adapter.load_call_count("big"), 0);
}

#[tokio::test]
async fn confirmed_header_spends_the_headroom_for_that_request_only() {
    let (app, adapter) = app(Some(BUDGET));
    let (status, _) = chat(&app, "big", &[("x-idoris-headroom-breach", "CONFIRMED")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(adapter.load_call_count("big"), 1);
    // Nothing is remembered: after small evicts big, reloading big asks again.
    assert_eq!(chat(&app, "small", &[]).await.0, StatusCode::OK);
    let (status, body) = chat(&app, "big", &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["type"], "headroom_confirmation_required");
    assert_eq!(adapter.load_call_count("big"), 1);
}

#[tokio::test]
async fn header_on_another_endpoint_grants_nothing() {
    let (app, adapter) = app(Some(BUDGET));
    let _ = post(&app, "/v1/messages", "big", &[CONFIRM]).await;
    let _ = post(&app, "/v1/embeddings", "big", &[CONFIRM]).await;
    assert_eq!(adapter.load_call_count("big"), 0);
    let (status, body) = chat(&app, "big", &[]).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["type"], "headroom_confirmation_required");
}

#[tokio::test]
async fn any_value_but_confirmed_counts_as_absent() {
    let (app, adapter) = app(Some(BUDGET));
    for value in ["true", "1", "confirm", "confirmed2", "yes", ""] {
        let (status, body) = chat(&app, "big", &[("x-idoris-headroom-breach", value)]).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{value:?}");
        assert_eq!(body["error"]["type"], "headroom_confirmation_required");
    }
    // A repeated header is ambiguous, so it is not a confirmation either.
    let request = Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .header("x-idoris-headroom-breach", "confirmed")
        .header("x-idoris-headroom-breach", "confirmed")
        .body(Body::from(
            json!({"model": "big", "messages": [{"role": "user", "content": "hi"}]}).to_string(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(adapter.load_call_count("big"), 0);
}

#[tokio::test]
async fn fits_under_the_reduced_cap_needs_no_confirmation() {
    let (app, adapter) = app(Some(BUDGET));
    assert_eq!(chat(&app, "small", &[]).await.0, StatusCode::OK);
    assert_eq!(adapter.load_call_count("small"), 1);
}

#[tokio::test]
async fn over_the_full_allocation_keeps_the_existing_refusal() {
    let (app, adapter) = app(Some(BUDGET));
    for headers in [&[][..], &[CONFIRM][..]] {
        let (status, body) = chat(&app, "huge", headers).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["error"]["type"], "local_only_unavailable");
        assert_ne!(
            body["error"]["reason_code"],
            "headroom_confirmation_required"
        );
    }
    assert_eq!(adapter.load_call_count("huge"), 0);
}

#[tokio::test]
async fn without_an_allocated_budget_nothing_changes() {
    let (app, adapter) = app(None);
    assert_eq!(chat(&app, "big", &[]).await.0, StatusCode::OK);
    assert_eq!(adapter.load_call_count("big"), 1);
}

const JOIN_REASON: &str = "different headroom confirmation";

async fn staggered(
    app: &Router,
    model: &str,
    first: &[(&str, &str)],
    second: &[(&str, &str)],
) -> ((StatusCode, Value), (StatusCode, Value)) {
    let late = async {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        chat(app, model, second).await
    };
    tokio::join!(chat(app, model, first), late)
}

#[tokio::test]
async fn unconfirmed_request_cannot_join_an_in_flight_confirmed_load() {
    let (app, adapter) = app(Some(BUDGET));
    adapter.set_load_delay("big", std::time::Duration::from_millis(400));
    let (confirmed, plain) = staggered(&app, "big", &[CONFIRM], &[]).await;
    assert_eq!(confirmed.0, StatusCode::OK);
    assert_ne!(plain.0, StatusCode::OK);
    assert_eq!(plain.1["error"]["reason_code"], "supervisor_busy");
    assert!(
        plain.1["error"]["remediation"]
            .as_str()
            .unwrap()
            .contains(JOIN_REASON),
        "{plain:?}"
    );
    assert_eq!(adapter.load_call_count("big"), 1);
}

#[tokio::test]
async fn confirmed_request_cannot_join_an_in_flight_unconfirmed_load() {
    let (app, adapter) = app(Some(BUDGET));
    adapter.set_load_delay("small", std::time::Duration::from_millis(400));
    let (plain, confirmed) = staggered(&app, "small", &[], &[CONFIRM]).await;
    assert_eq!(plain.0, StatusCode::OK);
    assert_ne!(confirmed.0, StatusCode::OK);
    assert!(
        confirmed.1["error"]["remediation"]
            .as_str()
            .unwrap()
            .contains(JOIN_REASON),
        "{confirmed:?}"
    );
    assert_eq!(adapter.load_call_count("small"), 1);
}
