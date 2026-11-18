//! S1: the real virtual-key id and the real model reach the budget ledger through every request path
//! that reserves budget, replacing the old `(default, <provider>, <provider>)` placeholder.
//!
//! Each test makes a request through the real router (`build_app`) with a real authenticator and a
//! real SQLite ledger, then reads the `reservations` / `budget_periods` rows back with a separate
//! connection. The paths: the local Supervisor path, the direct HTTP proxy path (role alias, named
//! model, unusable model name) and the protocol endpoints (`/v1/embeddings`). The no-key path and the
//! historical `default` rows are covered too, so "personal form unchanged" is a tested claim.
#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_backend::{MockAdapter, ModelInfo, Supervisor, SupervisorConfig};
use idoris_contracts::ComponentCard;
use idoris_contracts::common::{Capability, PrivacyClass};
use idoris_policy::ROLES;
use idoris_tenancy::budget::{BudgetLedger, BudgetScope, Price, SpendGate};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::tests::{
    configured_budget_ledger, paid_component_card, post_chat, resident_component_card,
};
use super::*;

const DAILY_BODY: &str = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

/// A key allowed to use every role, so it can also send a non-role model name.
fn key_scope() -> VirtualKeyScope {
    VirtualKeyScope {
        owner: "agent24-instance".into(),
        tenant_id: None,
        allowed_privacy: vec![PrivacyClass::LocalOnly],
        allowed_roles: ROLES.iter().map(|role| role.as_str().to_string()).collect(),
        budget_ref: None,
        expires_at_ms: None,
        admin_scopes: Vec::new(),
    }
}

fn authenticator(keys: &[&MintedVirtualKey]) -> auth::VirtualKeyAuthenticator {
    let store = Arc::new(Mutex::new(
        VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    for key in keys {
        store
            .lock()
            .unwrap()
            .insert_active(&key.key_id, key.hash, &key_scope())
            .unwrap();
    }
    auth::VirtualKeyAuthenticator::new(store)
}

fn bearer(key: &MintedVirtualKey) -> String {
    format!("Bearer {}", key.secret.expose_secret())
}

#[derive(Debug, PartialEq, Eq)]
struct Row {
    key_id: String,
    provider_id: String,
    model_id: String,
    status: String,
}

fn rows(dir: &tempfile::TempDir) -> Vec<Row> {
    let conn = Connection::open(dir.path().join("b.sqlite3")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT key_id, provider_id, model_id, status FROM reservations \
             ORDER BY created_at_ms, rowid",
        )
        .unwrap();
    stmt.query_map([], |r| {
        Ok(Row {
            key_id: r.get(0)?,
            provider_id: r.get(1)?,
            model_id: r.get(2)?,
            status: r.get(3)?,
        })
    })
    .unwrap()
    .collect::<Result<_, _>>()
    .unwrap()
}

fn settled_scopes(dir: &tempfile::TempDir) -> Vec<(String, String, String, i64)> {
    let conn = Connection::open(dir.path().join("b.sqlite3")).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT key_id, provider_id, model_id, spent_minor FROM budget_periods \
             ORDER BY key_id, provider_id, model_id",
        )
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

/// Maps the `daily` role to `model` so the backend model differs from the card (provider) id.
fn with_daily_model(mut card: ComponentCard, model: &str) -> ComponentCard {
    card.extensions = Some(
        serde_json::from_value(json!({
            "_degradation": "Removing these bindings disables aliases.",
            "idoris_role_models": {"daily": model}
        }))
        .unwrap(),
    );
    card
}

fn local_state(
    card: &ComponentCard,
    model: &str,
    ledger: Arc<BudgetLedger>,
    authenticator: Option<auth::VirtualKeyAuthenticator>,
) -> (AppState, Arc<MockAdapter>) {
    let adapter = Arc::new(MockAdapter::new(vec![ModelInfo {
        id: model.to_string(),
        memory_gb: 1.0,
    }]));
    let as_adapter: Arc<dyn idoris_backend::RuntimeAdapter> = adapter.clone();
    let supervisor = Supervisor::spawn(as_adapter, SupervisorConfig::default()).unwrap();
    let state = AppState {
        cards: vec![card.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(card, supervisor)).into(),
        budget_ledger: Some(ledger),
        virtual_key_authenticator: authenticator,
        ..AppState::default()
    };
    (state, adapter)
}

async fn upstream_with_usage() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": "chatcmpl-upstream",
            "choices": [{"message": {"role": "assistant", "content": "ok"}}],
            "usage": {"prompt_tokens": 7, "completion_tokens": 11, "total_tokens": 18}
        })))
        .mount(&server)
        .await;
    server
}

fn proxy_card(endpoint: &str, role_model: &str) -> ComponentCard {
    let mut card = resident_component_card("paid", endpoint);
    card.provider.cost = paid_component_card("paid").provider.cost;
    with_daily_model(card, role_model)
}

// ---------------------------------------------------------------- local Supervisor path

#[tokio::test]
async fn local_paid_request_is_recorded_under_the_authenticated_key_and_the_backend_model() {
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let key = MintedVirtualKey::mint();
    let card = with_daily_model(paid_component_card("paid-1"), "qwen3-4b-instruct");
    let (state, _adapter) = local_state(
        &card,
        "qwen3-4b-instruct",
        Arc::new(ledger),
        Some(authenticator(&[&key])),
    );
    let response = build_app(state)
        .oneshot(post_chat(DAILY_BODY, &[("Authorization", &bearer(&key))]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key(HEADER_COST_MINOR));

    assert_eq!(
        rows(&dir),
        [Row {
            key_id: key.key_id.clone(),
            provider_id: "paid-1".into(),
            model_id: "qwen3-4b-instruct".into(),
            status: "settled".into(),
        }]
    );
    // The settlement is booked under the same real scope, and nothing under the old placeholder.
    let spent = settled_scopes(&dir);
    assert_eq!(spent.len(), 1, "{spent:?}");
    assert_eq!(
        (
            spent[0].0.as_str(),
            spent[0].1.as_str(),
            spent[0].2.as_str()
        ),
        (key.key_id.as_str(), "paid-1", "qwen3-4b-instruct")
    );
    assert!(spent[0].3 > 0);
}

#[tokio::test]
async fn two_keys_are_recorded_separately_but_share_the_one_tenant_total() {
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let ledger = Arc::new(ledger);
    let (a, b) = (MintedVirtualKey::mint(), MintedVirtualKey::mint());
    let card = with_daily_model(paid_component_card("paid-1"), "qwen3-4b-instruct");
    let (state, _adapter) = local_state(
        &card,
        "qwen3-4b-instruct",
        ledger.clone(),
        Some(authenticator(&[&a, &b])),
    );
    let app = build_app(state);
    for key in [&a, &b, &a] {
        let response = app
            .clone()
            .oneshot(post_chat(DAILY_BODY, &[("Authorization", &bearer(key))]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }
    let recorded: Vec<String> = rows(&dir).into_iter().map(|row| row.key_id).collect();
    assert_eq!(
        recorded,
        [a.key_id.clone(), b.key_id.clone(), a.key_id.clone()]
    );
    // Per-key spend rows add up to the single tenant total.
    let per_key: i64 = settled_scopes(&dir).iter().map(|row| row.3).sum();
    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert_eq!(view.spent_minor, per_key);
    assert!(settled_scopes(&dir).len() == 2 && per_key > 0);
}

#[tokio::test]
async fn a_local_request_without_a_virtual_key_records_the_documented_no_key_constant() {
    // Personal form, no authenticator wired: the key is the named constant, not "" and not a new sentinel.
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let card = with_daily_model(paid_component_card("paid-1"), "qwen3-4b-instruct");
    let (state, _adapter) = local_state(&card, "qwen3-4b-instruct", Arc::new(ledger), None);
    let response = build_app(state)
        .oneshot(post_chat(DAILY_BODY, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].key_id, budget::NO_VIRTUAL_KEY_ID);
    assert_eq!(recorded[0].key_id, "default");
    // ... while the model is still the real backend model, not the provider id.
    assert_eq!(recorded[0].model_id, "qwen3-4b-instruct");
    assert_eq!(recorded[0].provider_id, "paid-1");
}

#[tokio::test]
async fn dev_no_key_requests_record_the_no_key_constant_even_when_an_authenticator_exists() {
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let key = MintedVirtualKey::mint();
    let mut card = with_daily_model(paid_component_card("paid-1"), "qwen3-4b-instruct");
    // IDORIS_DEV_NO_KEY only admits free loopback candidates.
    card.provider.cost = idoris_contracts::provider::Cost {
        input_per_m: 0.0,
        output_per_m: 0.0,
    };
    let (mut state, _adapter) = local_state(
        &card,
        "qwen3-4b-instruct",
        Arc::new(ledger),
        Some(authenticator(&[&key])),
    );
    state.dev_no_key_enabled = true;
    let response = build_app(state)
        .oneshot(post_chat(DAILY_BODY, &[]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].key_id, budget::NO_VIRTUAL_KEY_ID);
}

// ---------------------------------------------------------------- direct HTTP proxy path

#[tokio::test]
async fn proxy_request_via_a_role_alias_records_the_real_key_and_the_forwarded_model() {
    let server = upstream_with_usage().await;
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let key = MintedVirtualKey::mint();
    let app = build_app(AppState {
        cards: vec![proxy_card(&server.uri(), "vendor/upstream-model-v2")],
        budget_ledger: Some(Arc::new(ledger)),
        virtual_key_authenticator: Some(authenticator(&[&key])),
        ..AppState::default()
    });
    let response = app
        .oneshot(post_chat(DAILY_BODY, &[("Authorization", &bearer(&key))]))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().contains_key(HEADER_COST_MINOR));
    assert_eq!(
        rows(&dir),
        [Row {
            key_id: key.key_id.clone(),
            provider_id: "paid".into(),
            // the role alias was resolved to the model actually forwarded upstream
            model_id: "vendor/upstream-model-v2".into(),
            status: "settled".into(),
        }]
    );
    assert_eq!(
        settled_scopes(&dir)[0].0,
        key.key_id,
        "the proxy settlement inherits the real key from the reservation"
    );
}

#[tokio::test]
async fn proxy_request_naming_a_model_records_that_model() {
    let server = upstream_with_usage().await;
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let key = MintedVirtualKey::mint();
    let app = build_app(AppState {
        cards: vec![proxy_card(&server.uri(), "unused")],
        budget_ledger: Some(Arc::new(ledger)),
        virtual_key_authenticator: Some(authenticator(&[&key])),
        ..AppState::default()
    });
    let response = app
        .oneshot(post_chat(
            r#"{"model":"mlx-community/Qwen3-4B-4bit","messages":[{"role":"user","content":"hi"}]}"#,
            &[("Authorization", &bearer(&key))],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].model_id, "mlx-community/Qwen3-4B-4bit");
    assert_eq!(recorded[0].provider_id, "paid");
    assert_eq!(recorded[0].key_id, key.key_id);
}

#[tokio::test]
async fn proxy_request_without_a_model_or_with_an_unusable_one_records_the_provider_as_model() {
    let server = upstream_with_usage().await;
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let key = MintedVirtualKey::mint();
    let app = build_app(AppState {
        cards: vec![proxy_card(&server.uri(), "unused")],
        budget_ledger: Some(Arc::new(ledger)),
        virtual_key_authenticator: Some(authenticator(&[&key])),
        ..AppState::default()
    });
    let long_name = "m".repeat(129);
    for (index, body) in [
        // no model named at all
        r#"{"messages":[{"role":"user","content":"hi"}]}"#.to_string(),
        // not a valid model id: must not be copied into the ledger
        r#"{"model":"has space & @symbols","messages":[{"role":"user","content":"hi"}]}"#
            .to_string(),
        format!(r#"{{"model":"{long_name}","messages":[{{"role":"user","content":"hi"}}]}}"#),
    ]
    .iter()
    .enumerate()
    {
        let response = app
            .clone()
            .oneshot(post_chat(body, &[("Authorization", &bearer(&key))]))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "case {index}");
    }
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 3);
    for row in &recorded {
        assert_eq!(row.model_id, "paid", "{row:?}");
        assert_eq!(row.key_id, key.key_id);
    }
}

#[tokio::test]
async fn proxy_request_without_a_virtual_key_records_the_no_key_constant() {
    let server = upstream_with_usage().await;
    let (dir, ledger) = configured_budget_ledger(1_000_000);
    let app = build_app(AppState {
        cards: vec![proxy_card(&server.uri(), "unused")],
        budget_ledger: Some(Arc::new(ledger)),
        ..AppState::default()
    });
    let response = app
        .oneshot(post_chat(
            r#"{"model":"vendor/some-model","messages":[{"role":"user","content":"hi"}]}"#,
            &[],
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 1);
    assert_eq!(recorded[0].key_id, "default");
    assert_eq!(recorded[0].model_id, "vendor/some-model");
}

// ---------------------------------------------------------------- protocol endpoints

#[tokio::test]
async fn embeddings_are_recorded_under_the_authenticated_key_and_requested_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "object": "list",
            "data": [{"object": "embedding", "index": 0, "embedding": [0.1, 0.2]}]
        })))
        .mount(&server)
        .await;
    let (dir, ledger) = configured_budget_ledger(0);
    let key = MintedVirtualKey::mint();
    let mut card = resident_component_card("embed-local", &server.uri());
    card.provider.capabilities = vec![Capability::Embedding];
    let app = build_app(AppState {
        cards: vec![card],
        budget_ledger: Some(Arc::new(ledger)),
        virtual_key_authenticator: Some(authenticator(&[&key])),
        ..AppState::default()
    });
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .method("POST")
                .uri("/v1/embeddings")
                .header("content-type", "application/json")
                .header("authorization", bearer(&key))
                .body(Body::from(r#"{"model":"embed-model","input":"hello"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 1, "{recorded:?}");
    assert_eq!(recorded[0].key_id, key.key_id);
    assert_eq!(recorded[0].model_id, "embed-model");
    assert_eq!(recorded[0].provider_id, "embed-local");
}

#[tokio::test]
async fn rerank_and_messages_are_recorded_under_the_authenticated_key_and_requested_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results": []})))
        .mount(&server)
        .await;
    for (uri, capability, model, payload) in [
        (
            "/v1/rerank",
            Capability::Rerank,
            "rerank-model",
            r#"{"model":"rerank-model","query":"q","documents":["a","b"]}"#,
        ),
        (
            "/v1/messages",
            Capability::Chat,
            "claude-local",
            r#"{"model":"claude-local","max_tokens":16,"messages":[{"role":"user","content":[{"type":"text","text":"hello"}]}]}"#,
        ),
    ] {
        let (dir, ledger) = configured_budget_ledger(0);
        let key = MintedVirtualKey::mint();
        let mut card = resident_component_card("proto-local", &server.uri());
        card.provider.capabilities = vec![capability];
        let app = build_app(AppState {
            cards: vec![card],
            budget_ledger: Some(Arc::new(ledger)),
            virtual_key_authenticator: Some(authenticator(&[&key])),
            ..AppState::default()
        });
        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .header("authorization", bearer(&key))
                    .body(Body::from(payload))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let recorded = rows(&dir);
        assert_eq!(recorded.len(), 1, "{uri}: {recorded:?}");
        assert_eq!(recorded[0].key_id, key.key_id, "{uri}");
        assert_eq!(recorded[0].model_id, model, "{uri}");
        assert_eq!(recorded[0].provider_id, "proto-local", "{uri}");
    }
}

// ---------------------------------------------------------------- history and concurrency

#[tokio::test]
async fn historical_default_rows_still_count_toward_the_tenant_limit_for_real_key_requests() {
    // Rows written by the pre-S1 code: (default, provider, provider). After the switch the same tenant
    // total must still include them; "personal form unchanged" means exactly this.
    let (dir, ledger) = configured_budget_ledger(0);
    let ledger = Arc::new(ledger);
    let cost = paid_component_card("paid-1").provider.cost;
    let estimate = budget::estimate_cost_minor(&cost, "hi").unwrap();
    let historical_spend = 5_000;
    ledger
        .configure_tenant(
            budget::PERSONAL_TENANT_ID,
            historical_spend + estimate - 1,
            "UTC",
            SpendGate::PaidOnly,
        )
        .unwrap();
    let legacy_scope = BudgetScope::new(budget::PERSONAL_TENANT_ID, "default", "paid-1", "paid-1");
    let id = ledger
        .reserve(&legacy_scope, Price::Known(historical_spend))
        .unwrap();
    ledger
        .settle(budget::PERSONAL_TENANT_ID, &id, historical_spend)
        .unwrap();

    let key = MintedVirtualKey::mint();
    let card = with_daily_model(paid_component_card("paid-1"), "qwen3-4b-instruct");
    let (state, _adapter) = local_state(
        &card,
        "qwen3-4b-instruct",
        ledger.clone(),
        Some(authenticator(&[&key])),
    );
    let app = build_app(state);

    // One minor unit short: the historical 5000 under `default` is what makes the difference.
    let denied = app
        .clone()
        .oneshot(post_chat(DAILY_BODY, &[("Authorization", &bearer(&key))]))
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::PAYMENT_REQUIRED);
    let bytes = denied.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "budget_exceeded");
    assert_eq!(json["error"]["spent_minor"], historical_spend);
    // Rejected before any row was written for the real key.
    assert!(
        rows(&dir).iter().all(|row| row.key_id == "default"),
        "{:?}",
        rows(&dir)
    );

    // Exactly enough: the request goes through and is recorded under the real key.
    ledger
        .configure_tenant(
            budget::PERSONAL_TENANT_ID,
            historical_spend + estimate,
            "UTC",
            SpendGate::PaidOnly,
        )
        .unwrap();
    let allowed = app
        .oneshot(post_chat(DAILY_BODY, &[("Authorization", &bearer(&key))]))
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    let recorded = rows(&dir);
    assert_eq!(recorded.len(), 2);
    assert_eq!(recorded[0].key_id, "default");
    assert_eq!(recorded[1].key_id, key.key_id);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_requests_under_distinct_real_keys_never_overrun_the_one_tenant_budget() {
    // Before S1 every request shared one `(default, provider, provider)` sub-scope. Now each request's
    // scope differs, so a ledger that summed per key instead of per tenant would hand every key its own
    // allowance. Hold the admitted requests in the backend so the in-flight window is observable.
    const KEYS: usize = 6;
    const REQUESTS: usize = 60;
    let reservation =
        budget::estimate_cost_minor(&paid_component_card("paid-1").provider.cost, "hi").unwrap();
    let limit = reservation * 10;
    let (dir, ledger) = configured_budget_ledger(limit);
    let ledger = Arc::new(ledger);
    let keys: Vec<MintedVirtualKey> = (0..KEYS).map(|_| MintedVirtualKey::mint()).collect();
    let card = paid_component_card("paid-1");
    let (state, adapter) = local_state(
        &card,
        "paid-1",
        ledger.clone(),
        Some(authenticator(&keys.iter().collect::<Vec<_>>())),
    );
    adapter.set_chat_delay("paid-1", std::time::Duration::from_millis(2_500));
    let app = build_app(state);
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

    let tasks: Vec<_> = (0..REQUESTS)
        .map(|index| {
            let app = app.clone();
            let auth = bearer(&keys[index % KEYS]);
            tokio::spawn(async move {
                app.oneshot(post_chat(body, &[("Authorization", &auth)]))
                    .await
                    .unwrap()
            })
        })
        .collect();

    // While the admitted requests are held, reserved + spent never exceeds the limit and exactly
    // limit / reservation requests are admitted, regardless of which key sent them.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
        assert!(
            view.reserved_minor + view.spent_minor <= limit,
            "in flight: reserved {} + spent {} > limit {limit}",
            view.reserved_minor,
            view.spent_minor
        );
        if view.reserved_minor / reservation == limit / reservation {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "admission stalled");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (mut ok, mut payment_required) = (0, 0);
    for task in tasks {
        match task.await.unwrap().status() {
            StatusCode::OK => ok += 1,
            StatusCode::PAYMENT_REQUIRED => payment_required += 1,
            other => panic!("unexpected status {other}"),
        }
    }
    let admitted = usize::try_from(limit / reservation).unwrap();
    assert_eq!((ok, payment_required), (admitted, REQUESTS - admitted));
    let view = ledger.tenant_readview(budget::PERSONAL_TENANT_ID).unwrap();
    assert!(view.spent_minor <= limit);
    assert_eq!(view.reserved_minor, 0);
    // Several different keys really took part, and every settled amount is booked under a real key.
    let recorded = rows(&dir);
    let distinct: std::collections::BTreeSet<_> = recorded
        .iter()
        .filter(|row| row.status == "settled")
        .map(|row| row.key_id.clone())
        .collect();
    assert!(distinct.len() >= 2, "{distinct:?}");
    assert!(recorded.iter().all(|row| row.key_id != "default"));
    let per_scope: i64 = settled_scopes(&dir).iter().map(|row| row.3).sum();
    assert_eq!(per_scope, view.spent_minor);
}
