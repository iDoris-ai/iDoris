#![allow(clippy::unwrap_used)]

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use idoris_router::capabilities::{
    AdmissionStatus, CapabilitiesError, CapabilitiesProvider, CapabilityEntry,
};
use idoris_router::{AppState, build_app};
use tower::ServiceExt;

struct FakeProvider {
    calls: AtomicUsize,
    fail: bool,
}

impl CapabilitiesProvider for FakeProvider {
    fn snapshot(
        &self,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<CapabilityEntry>, CapabilitiesError>> + Send + '_>>
    {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err(CapabilitiesError::new("secret backend detail"));
            }
            Ok(vec![CapabilityEntry {
                id: "daily-9b".into(),
                capability: "reasoning".into(),
                resident: true,
                estimated_memory_gb: 6.25,
                ctx_limit: 4096,
                queue_depth: 1,
                admission_status: AdmissionStatus::Ready,
            }])
        })
    }
}

async fn get(state: AppState) -> axum::response::Response {
    build_app(state)
        .oneshot(
            Request::builder()
                .uri("/capabilities")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn injected_provider_returns_top_level_array_with_reference_shape_and_record_id() {
    let provider = Arc::new(FakeProvider {
        calls: AtomicUsize::new(0),
        fail: false,
    });
    let response = get(AppState {
        capabilities: Some(provider.clone()),
        ..AppState::default()
    })
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(response.headers().get("x-idoris-record-id").is_some());
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let entries = value.as_array().unwrap();
    assert_eq!(entries.len(), 1);
    let entry = entries[0].as_object().unwrap();
    assert_eq!(entry.len(), 7);
    for field in [
        "id",
        "capability",
        "resident",
        "estimated_memory_gb",
        "ctx_limit",
        "queue_depth",
        "admission_status",
    ] {
        assert!(entry.contains_key(field), "missing {field}");
    }
    assert_eq!(entry["admission_status"], "ready");
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_or_failing_provider_is_503_and_does_not_leak_provider_error() {
    for capabilities in [
        None,
        Some(Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            fail: true,
        }) as Arc<dyn CapabilitiesProvider>),
    ] {
        let response = get(AppState {
            capabilities,
            ..AppState::default()
        })
        .await;
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(response.headers().get("x-idoris-record-id").is_some());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8(body.to_vec()).unwrap();
        assert!(text.contains("capabilities_unavailable"));
        assert!(!text.contains("secret backend detail"));
    }
}
