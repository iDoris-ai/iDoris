//! M5-14: the provider breaker on the resident proxy path, seen from a
//! chat client — an open provider receives nothing and no dispatch is
//! recorded for the refused request.
#![allow(clippy::unwrap_used)]

use axum::http::StatusCode;
use http_body_util::BodyExt;
use idoris_tenancy::event_log::{EventLogStore, EventType};
use rusqlite::Connection;
use tower::ServiceExt;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use super::tests::{post_chat, resident_component_card};
use super::*;

#[tokio::test]
async fn failing_local_upstream_is_not_contacted_once_its_breaker_opens() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .mount(&server)
        .await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![resident_component_card("flaky-local", &server.uri())],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

    let mut last_record = String::new();
    let mut statuses = Vec::new();
    let mut refusals = Vec::new();
    for _ in 0..3 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        statuses.push(response.status());
        last_record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let body_bytes = response.into_body().collect().await.unwrap().to_bytes();
        refusals.push(serde_json::from_slice::<serde_json::Value>(&body_bytes).ok());
    }
    assert_eq!(
        statuses,
        [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::SERVICE_UNAVAILABLE
        ]
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
    // The all-open case must stay the retryable `provider_circuit_open` from the
    // proxy send gate. Marking every open provider `Blocked` during selection
    // turned it into `no_eligible_candidate`, which clients read as "no such
    // model" rather than "cooling down, retry" - a contract change.
    let Some(refusal) = refusals[2].as_ref() else {
        panic!("refusal body is not JSON");
    };
    assert_eq!(
        refusal["error"]["type"], "provider_circuit_open",
        "all-open chat must answer the retryable breaker reason, got {refusal}"
    );
    let refused = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &last_record)
        .unwrap();
    assert!(
        refused
            .iter()
            .all(|e| e.event.event_type != EventType::Dispatched),
        "a breaker refusal must not be recorded as a dispatch"
    );
}

#[tokio::test]
async fn selection_moves_to_a_healthy_candidate_once_a_breaker_opens() {
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500).set_body_json(json!({"error": "boom"})))
        .mount(&failing)
        .await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"marker": "healthy"})))
        .mount(&healthy)
        .await;
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![
            resident_component_card("a-failing", &failing.uri()),
            resident_component_card("b-healthy", &healthy.uri()),
        ],
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;

    let mut statuses = Vec::new();
    let mut last_record = String::new();
    for _ in 0..3 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        statuses.push(response.status());
        last_record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
    }
    assert_eq!(
        statuses,
        [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::OK
        ]
    );
    assert_eq!(failing.received_requests().await.unwrap().len(), 2);
    assert_eq!(healthy.received_requests().await.unwrap().len(), 1);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &last_record)
        .unwrap();
    let decided = events
        .iter()
        .find(|e| e.event.event_type == EventType::Decided)
        .unwrap();
    assert_eq!(decided.event.metadata["provider_id"], "b-healthy");
}

#[tokio::test]
async fn protocol_selection_also_moves_to_a_healthy_candidate() {
    let failing = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "busy"})))
        .mount(&failing)
        .await;
    let healthy = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"object": "list", "data": []})),
        )
        .mount(&healthy)
        .await;
    let embedding = |id: &str, uri: &str| {
        let mut card = resident_component_card(id, uri);
        card.provider.capabilities = vec![idoris_contracts::common::Capability::Embedding];
        card
    };
    let app = build_app(AppState {
        cards: vec![
            embedding("a-failing", &failing.uri()),
            embedding("b-healthy", &healthy.uri()),
        ],
        ..AppState::default()
    });
    let mut statuses = Vec::new();
    for _ in 0..3 {
        let request = axum::http::Request::builder()
            .method("POST")
            .uri("/v1/embeddings")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"model":"embed","input":["hello"]}"#))
            .unwrap();
        statuses.push(app.clone().oneshot(request).await.unwrap().status());
    }
    assert_eq!(
        statuses,
        [
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::OK
        ]
    );
    assert_eq!(failing.received_requests().await.unwrap().len(), 2);
    assert_eq!(healthy.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn supervisor_model_that_keeps_failing_is_locked_out_for_another() {
    let cards = vec![
        super::tests::sample_component_card("a-flaky"),
        super::tests::sample_component_card("b-steady"),
    ];
    let mut adapters = Vec::new();
    let mut bound = Vec::new();
    for card in &cards {
        let id = card.provider.id.clone();
        let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
            idoris_backend::ModelInfo {
                id: id.clone(),
                memory_gb: 1.0,
            },
        ]));
        if id == "a-flaky" {
            adapter.set_load_script(&id, vec![idoris_backend::mock::LoadOutcome::Fail; 3]);
        }
        let supervisor = idoris_backend::Supervisor::spawn(
            adapter.clone(),
            idoris_backend::SupervisorConfig::default(),
        )
        .unwrap();
        bound.push(dispatch::BoundSupervisor::new(card, supervisor));
        adapters.push(adapter);
    }
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards,
        runtimes: runtime::RuntimeRegistry::from_supervisors(bound),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    let mut ok = Vec::new();
    let mut last_record = String::new();
    for _ in 0..3 {
        let response = app.clone().oneshot(post_chat(body, &[])).await.unwrap();
        ok.push(response.status() == StatusCode::OK);
        last_record = response
            .headers()
            .get(HEADER_RECORD_ID)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
    }
    assert_eq!(ok, [false, false, true]);
    assert_eq!(
        adapters[0].load_call_count("a-flaky"),
        2,
        "locked after two model faults"
    );
    assert_eq!(adapters[1].load_call_count("b-steady"), 1);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &last_record)
        .unwrap();
    let decided = events
        .iter()
        .find(|e| e.event.event_type == EventType::Decided)
        .unwrap();
    assert_eq!(decided.event.metadata["provider_id"], "b-steady");
}

#[tokio::test]
async fn the_fallback_re_decide_agrees_with_the_decided_rejection() {
    // With the only model locked, selection is rejected and the legacy
    // fallback re-decides; it must reach the same verdict the decided event
    // recorded instead of re-picking the locked model.
    let card = super::tests::sample_component_card("only-flaky");
    let adapter = Arc::new(idoris_backend::MockAdapter::new(vec![
        idoris_backend::ModelInfo {
            id: "only-flaky".to_string(),
            memory_gb: 1.0,
        },
    ]));
    adapter.set_load_script(
        "only-flaky",
        vec![idoris_backend::mock::LoadOutcome::Fail; 3],
    );
    let supervisor = idoris_backend::Supervisor::spawn(
        adapter.clone(),
        idoris_backend::SupervisorConfig::default(),
    )
    .unwrap();
    let event_log = Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap());
    let app = build_app(AppState {
        cards: vec![card.clone()],
        runtimes: Some(dispatch::BoundSupervisor::new(&card, supervisor)).into(),
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    let mut last = None;
    for _ in 0..3 {
        last = Some(app.clone().oneshot(post_chat(body, &[])).await.unwrap());
    }
    let response = last.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let record_id = response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();
    let bytes = http_body_util::BodyExt::collect(response.into_body())
        .await
        .unwrap()
        .to_bytes();
    let error: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let decided = events
        .iter()
        .find(|e| e.event.event_type == EventType::Decided)
        .unwrap();
    assert_eq!(decided.event.metadata["status"], "rejected");
    assert_eq!(error["error"]["type"], decided.event.metadata["reason"]);
    assert_eq!(adapter.load_call_count("only-flaky"), 2);
}

/// Same acceptance as the chat case, on the three protocol endpoints: when the
/// single eligible card's breaker is open, the third request must stay the
/// retryable `provider_circuit_open` and must not be sent.
#[tokio::test]
async fn single_card_all_open_keeps_the_retryable_reason_on_protocol_endpoints() {
    for (uri, capability, payload) in [
        (
            "/v1/embeddings",
            idoris_contracts::common::Capability::Embedding,
            r#"{"model":"embed","input":["hello"]}"#,
        ),
        (
            "/v1/rerank",
            idoris_contracts::common::Capability::Rerank,
            r#"{"model":"rerank","query":"q","documents":["a","b"]}"#,
        ),
        (
            "/v1/messages",
            idoris_contracts::common::Capability::Chat,
            r#"{"model":"idoris/daily","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#,
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "busy"})))
            .mount(&server)
            .await;
        let mut card = resident_component_card("only-card", &server.uri());
        card.provider.capabilities = vec![capability];
        let app = build_app(AppState {
            cards: vec![card],
            ..AppState::default()
        });

        let mut statuses = Vec::new();
        let mut bodies = Vec::new();
        for _ in 0..3 {
            let request = axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(payload))
                .unwrap();
            let response = app.clone().oneshot(request).await.unwrap();
            statuses.push(response.status());
            let bytes = response.into_body().collect().await.unwrap().to_bytes();
            bodies.push(serde_json::from_slice::<serde_json::Value>(&bytes).ok());
        }

        assert_eq!(
            statuses[2],
            StatusCode::SERVICE_UNAVAILABLE,
            "{uri} third request must be 503"
        );
        let Some(refusal) = bodies[2].as_ref() else {
            panic!("{uri} refusal body is not JSON");
        };
        assert_eq!(
            refusal["error"]["type"], "provider_circuit_open",
            "{uri} all-open must answer the retryable breaker reason, got {refusal}"
        );
        assert_eq!(
            server.received_requests().await.unwrap().len(),
            2,
            "{uri} the refused request must not be sent"
        );
    }
}

/// B4: in a mixed tier the fallback must look at the card it would actually
/// choose, not at every policy card. A resident chat card whose breaker is open,
/// next to an embedding-only lifecycle card, must still answer the retryable
/// `provider_circuit_open` — the lifecycle card is not selectable for chat.
#[tokio::test]
async fn a_mixed_tier_keeps_the_retryable_reason_for_chat() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503).set_body_json(json!({"error": "busy"})))
        .mount(&server)
        .await;
    let mut chat_card = resident_component_card("chat-proxy", &server.uri());
    chat_card.provider.capabilities = vec![idoris_contracts::common::Capability::Chat];
    // Same tier, but only an embedding lifecycle card: never selectable for chat.
    let mut embedding_card = super::tests::sample_component_card("embed-lifecycle");
    embedding_card.provider.capabilities = vec![idoris_contracts::common::Capability::Embedding];
    embedding_card.load_policy = None;

    let app = build_app(AppState {
        cards: vec![chat_card, embedding_card],
        ..AppState::default()
    });

    let mut statuses = Vec::new();
    let mut bodies = Vec::new();
    for _ in 0..3 {
        let response = app
            .clone()
            .oneshot(post_chat(
                r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#,
                &[],
            ))
            .await
            .unwrap();
        statuses.push(response.status());
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        bodies.push(serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    }
    assert_eq!(statuses[2], StatusCode::SERVICE_UNAVAILABLE);
    let Some(refusal) = bodies[2].as_ref() else {
        panic!("refusal body is not JSON");
    };
    assert_eq!(
        refusal["error"]["type"], "provider_circuit_open",
        "a mixed tier must keep the retryable reason, got {refusal}"
    );
    assert_eq!(server.received_requests().await.unwrap().len(), 2);
}
