#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_tenancy::event_log::{EventLogStore, EventType};
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
