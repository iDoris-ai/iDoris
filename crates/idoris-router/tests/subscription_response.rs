#![allow(clippy::unwrap_used)]

use http_body_util::BodyExt;
use idoris_backend::ChatResponse;
use idoris_router::subscription::response;
use idoris_router::subscription::source::SubscriptionSourceError;
use idoris_upstream::subscription::error::{
    SubscriptionDiagnostics, SubscriptionErrorCode, SubscriptionRelayError,
};

async fn json(response: axum::response::Response) -> serde_json::Value {
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn success_is_buffered_openai_json_with_remote_and_record_id() {
    let chat = ChatResponse {
        model: "claude-subscription".into(),
        content: "答😀😀".into(),
    };
    let response = response::success(&chat, "你😀😀😀", "record-success", true);
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    assert_eq!(response.headers()["x-idoris-served-locality"], "remote");
    assert_eq!(response.headers()["x-idoris-record-id"], "record-success");
    assert!(
        response
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("application/json")
    );
    let body = json(response).await;
    assert_eq!(body["model"], "claude-subscription");
    assert_eq!(body["choices"][0]["message"]["content"], "答😀😀");
    assert_eq!(body["usage"]["prompt_tokens"], 2);
    assert_eq!(body["usage"]["completion_tokens"], 2);
    assert_eq!(body["usage"]["total_tokens"], 4);
}

#[tokio::test]
async fn relay_failure_is_502_remote_and_never_leaks_stderr_content() {
    let sentinel = "SECRET_STDERR_SENTINEL";
    let diagnostics = SubscriptionDiagnostics::from_stderr(Some(23), sentinel.as_bytes());
    let error =
        SubscriptionRelayError::with_diagnostics(SubscriptionErrorCode::CliFailed, diagnostics);
    let response = response::relay_failure(&error, "record-error");
    assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
    assert_eq!(response.headers()["x-idoris-served-locality"], "remote");
    assert_eq!(response.headers()["x-idoris-record-id"], "record-error");
    let text = serde_json::to_string(&json(response).await).unwrap();
    assert!(text.contains("subscription_relay_failed"));
    assert!(text.contains("RELAY_CLI_FAILED"));
    assert!(!text.contains(sentinel));
}

#[tokio::test]
async fn source_rejection_is_403_with_record_id_but_not_served_locality() {
    let response =
        response::source_rejection(SubscriptionSourceError::NonLoopback, "record-source");
    assert_eq!(response.status(), axum::http::StatusCode::FORBIDDEN);
    assert_eq!(response.headers()["x-idoris-record-id"], "record-source");
    assert!(response.headers().get("x-idoris-served-locality").is_none());
    let body = json(response).await;
    assert_eq!(body["error"]["type"], "subscription_source_not_loopback");
    assert_eq!(
        body["error"]["reason_code"],
        "SUBSCRIPTION_SOURCE_FORBIDDEN"
    );
}
