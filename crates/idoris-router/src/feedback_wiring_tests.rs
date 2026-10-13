#![allow(clippy::unwrap_used)]

use std::sync::{Arc, Mutex};

use axum::http::{HeaderValue, Request, StatusCode, header::AUTHORIZATION};
use http_body_util::BodyExt;
use idoris_contracts::DeployMode;
use idoris_contracts::common::PrivacyClass;
use idoris_tenancy::event_log::{EventLogStore, EventType, NewEvent};
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use rusqlite::Connection;
use tower::ServiceExt;

use super::*;

fn memory_event_log() -> Arc<EventLogStore> {
    Arc::new(EventLogStore::new(Connection::open_in_memory().unwrap()).unwrap())
}

async fn create_record(app: &Router) -> String {
    let response = app
        .clone()
        .oneshot(super::tests::post_chat(
            r#"{"messages":[{"role":"user","content":"feedback target"}]}"#,
            &[("x-idoris-trace-id", "feedback-trace")],
        ))
        .await
        .unwrap();
    response
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string()
}

fn feedback_request(body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/feedback")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn feedback_request_with_tenant(body: serde_json::Value, tenant: &str) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/feedback")
        .header("content-type", "application/json")
        .header("x-idoris-tenant", tenant)
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

fn with_key(mut request: Request<Body>, key: &MintedVirtualKey) -> Request<Body> {
    request.headers_mut().insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {}", key.secret.expose_secret())).unwrap(),
    );
    request
}

fn tenant_auth(keys: &[(&MintedVirtualKey, Option<&str>)]) -> auth::VirtualKeyAuthenticator {
    let store = Arc::new(Mutex::new(
        VirtualKeyStore::new(Connection::open_in_memory().unwrap()).unwrap(),
    ));
    for (key, tenant) in keys {
        store
            .lock()
            .unwrap()
            .insert_active(
                &key.key_id,
                key.hash,
                &VirtualKeyScope {
                    owner: "feedback-test".into(),
                    tenant_id: tenant.map(str::to_string),
                    allowed_privacy: vec![PrivacyClass::LocalOnly],
                    allowed_roles: vec!["fast".into()],
                    budget_ref: None,
                    expires_at_ms: None,
                    admin_scopes: Vec::new(),
                },
            )
            .unwrap();
    }
    auth::VirtualKeyAuthenticator::new(store)
}

#[tokio::test]
async fn metadata_feedback_appends_to_existing_record() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let record_id = create_record(&app).await;
    let response = app
        .oneshot(feedback_request(json!({
            "record_id": record_id,
            "rating": "up",
            "labels": ["useful", "accepted"],
            "outcome": "kept",
            "rubric": [
                {"id":"correct","pass":true},
                {"id":"style","pass":false}
            ]
        })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let feedback = events
        .iter()
        .filter(|event| event.event.event_type == EventType::FeedbackReceived)
        .collect::<Vec<_>>();
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].event.metadata.get("rating"), Some(&json!("up")));
    assert_eq!(
        feedback[0].event.metadata.get("outcome"),
        Some(&json!("kept"))
    );
    assert_eq!(
        feedback[0].event.metadata.get("labels"),
        Some(&json!(["useful", "accepted"]))
    );
    assert_eq!(
        feedback[0].event.metadata.get("rubric"),
        Some(&json!([
            {"id":"correct","pass":true},
            {"id":"style","pass":false}
        ]))
    );
    assert_eq!(
        feedback[0].event.trace_id.as_deref(),
        Some("feedback-trace")
    );
}

#[tokio::test]
async fn explicit_null_rubric_keeps_existing_rating_feedback_valid() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log.clone()),
        ..AppState::default()
    });
    let record_id = create_record(&app).await;
    let response = app
        .oneshot(feedback_request(json!({
            "record_id": record_id,
            "rating": "up",
            "rubric": null
        })))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let events = event_log
        .events_for_record(Some(budget::PERSONAL_TENANT_ID), &record_id)
        .unwrap();
    let feedback = events
        .iter()
        .filter(|event| event.event.event_type == EventType::FeedbackReceived)
        .collect::<Vec<_>>();
    assert_eq!(feedback.len(), 1);
    assert_eq!(feedback[0].event.metadata.get("rating"), Some(&json!("up")));
    assert!(!feedback[0].event.metadata.contains_key("rubric"));
}

#[tokio::test]
async fn feedback_rejects_unknown_record_and_content_fields() {
    let event_log = memory_event_log();
    let app = build_app(AppState {
        event_log: Some(event_log),
        ..AppState::default()
    });
    let missing = app
        .clone()
        .oneshot(feedback_request(json!({
            "record_id": "missing-record",
            "rating": "down"
        })))
        .await
        .unwrap();
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    let record_id = create_record(&app).await;
    for body in [json!({"record_id": record_id, "corrected_output": "secret"})] {
        let response = app.clone().oneshot(feedback_request(body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
    }
}

#[tokio::test]
async fn feedback_control_char_metadata_is_400_before_event_log() {
    let app = build_app(AppState::default());
    for body in [
        json!({"record_id":"record","outcome":"bad\nvalue"}),
        json!({"record_id":"record","labels":["ok","bad\tvalue"]}),
    ] {
        let response = app.clone().oneshot(feedback_request(body)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "invalid_feedback");
    }
}

#[tokio::test]
async fn tenant_feedback_rejects_invalid_tenant_identifier_before_event_log() {
    let key = MintedVirtualKey::mint();
    let app = build_app(AppState {
        deploy_mode: DeployMode::Tenant,
        virtual_key_authenticator: Some(tenant_auth(&[(&key, Some("victim"))])),
        ..AppState::default()
    });
    let tenant = "t".repeat(129);
    let response = app
        .oneshot(with_key(
            feedback_request_with_tenant(json!({"record_id":"record","rating":"up"}), &tenant),
            &key,
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(json["error"]["type"], "invalid_tenant_scope");
}

#[tokio::test]
async fn tenant_key_cannot_write_feedback_or_request_event_to_another_tenant() {
    let event_log = memory_event_log();
    let record_id = "victim-record";
    event_log
        .append(
            Some("victim"),
            &NewEvent {
                event_id: uuid::Uuid::new_v4().to_string(),
                tenant_id: "victim".into(),
                record_id: record_id.into(),
                event_type: EventType::RequestReceived,
                ts_utc_ms: 1_800_000_000_000,
                request_id: None,
                session_id: None,
                trace_id: None,
                parent_id: None,
                origin_record_id: None,
                metadata: Default::default(),
            },
        )
        .unwrap();
    let attacker = MintedVirtualKey::mint();
    let unbound = MintedVirtualKey::mint();
    let victim = MintedVirtualKey::mint();
    let app = build_app(AppState {
        deploy_mode: DeployMode::Tenant,
        event_log: Some(event_log.clone()),
        virtual_key_authenticator: Some(tenant_auth(&[
            (&attacker, Some("attacker")),
            (&unbound, None),
            (&victim, Some("victim")),
        ])),
        ..AppState::default()
    });
    let body = json!({"record_id":record_id,"rating":"up"});
    for key in [&attacker, &unbound] {
        let response = app
            .clone()
            .oneshot(with_key(
                feedback_request_with_tenant(body.clone(), "victim"),
                key,
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let no_auth = app
        .clone()
        .oneshot(feedback_request_with_tenant(body.clone(), "victim"))
        .await
        .unwrap();
    assert_eq!(no_auth.status(), StatusCode::UNAUTHORIZED);
    let cross_tenant_chat = app
        .clone()
        .oneshot(with_key(
            super::tests::post_chat("{}", &[("x-idoris-tenant", "victim")]),
            &attacker,
        ))
        .await
        .unwrap();
    assert_eq!(cross_tenant_chat.status(), StatusCode::FORBIDDEN);
    assert_eq!(
        event_log
            .events_for_record(Some("victim"), record_id)
            .unwrap()
            .len(),
        1
    );
    let authorized = app
        .clone()
        .oneshot(with_key(
            feedback_request_with_tenant(body, "victim"),
            &victim,
        ))
        .await
        .unwrap();
    assert_eq!(authorized.status(), StatusCode::NO_CONTENT);
    let events = event_log
        .events_for_record(Some("victim"), record_id)
        .unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[1].event.event_type, EventType::FeedbackReceived);

    let authorized_chat = app
        .oneshot(with_key(
            super::tests::post_chat("{}", &[("x-idoris-tenant", "victim")]),
            &victim,
        ))
        .await
        .unwrap();
    let chat_record_id = authorized_chat
        .headers()
        .get(HEADER_RECORD_ID)
        .unwrap()
        .to_str()
        .unwrap();
    assert!(
        !event_log
            .events_for_record(Some("victim"), chat_record_id)
            .unwrap()
            .is_empty()
    );
}
