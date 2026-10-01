//! prdaemon #48 round 2, M1, end to end: a real `OmlxAdapter` behind a real
//! `Supervisor`, with oMLX simulated by `wiremock`. `POST .../load`
//! succeeds (the engine now holds the model), then the follow-up
//! `GET /v1/models/status` fails or reports external pin drift. The
//! Supervisor must send a real `POST .../unload` — not settle on `Stopped`
//! and forget the memory. `MockAdapter` can't express this two-step shape,
//! which is why this lives here rather than in `idoris-backend`.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use idoris_backend::{Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

async fn run(verify: ResponseTemplate, unload_status: u16) -> f64 {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(verify)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ResponseTemplate::new(unload_status))
        .expect(1)
        .mount(&server)
        .await;
    // K11/H3: cleanup needs engine release evidence beyond the POST acknowledgement.
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 0
        })))
        .mount(&server)
        .await;

    let adapter = OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        api_key: None,
        call_timeout: Duration::from_millis(500),
    })
    .expect("adapter must build");
    let handle = Supervisor::spawn(Arc::new(adapter), SupervisorConfig::default()).expect("spawn");
    let err = handle
        .load("qwen3-8b", 20.0, on_demand())
        .await
        .expect_err("load must fail when the post-load check fails");
    assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
    let used_gb = handle.status().await.expect("status").used_gb;
    server.verify().await; // exactly one real unload was sent
    used_gb
}

#[tokio::test]
async fn status_503_after_load_triggers_a_real_unload() {
    assert_eq!(run(ResponseTemplate::new(503), 200).await, 0.0);
}

#[tokio::test]
async fn external_pin_drift_after_load_triggers_a_real_unload() {
    let pinned = ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "models": [{"id": "qwen3-8b", "loaded": true, "pinned": true}]
    }));
    assert_eq!(run(pinned, 200).await, 0.0);
}

/// If the unload itself fails, the memory may still be held: the ledger
/// must keep counting it instead of reporting it free.
#[tokio::test]
async fn a_failed_release_keeps_the_memory_on_the_ledger() {
    assert_eq!(run(ResponseTemplate::new(503), 500).await, 20.0);
}

/// K09/H1: the adapter deadline fires before the Supervisor's outer
/// deadline. A received load may still allocate memory after that point.
#[tokio::test]
async fn k09_load_timeout_triggers_release_and_preserves_unreleased_memory() {
    let status_body = |loaded_models: &[&str], used: u64| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": loaded_models, "model_memory_used": used,
            "model_memory_max": 0
        }))
    };
    let scenarios = [
        (200, ResponseTemplate::new(503), 20.0),
        (200, status_body(&["qwen3-8b"], 0), 20.0),
        (200, status_body(&[], 20 * 1024 * 1024 * 1024), 20.0),
        (500, status_body(&[], 0), 20.0),
        (200, status_body(&[], 0), 0.0),
    ];
    for (unload_status, release_status, expected_gb) in scenarios {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/models/qwen3-8b/load"))
            .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(1)))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/models/qwen3-8b/unload"))
            .respond_with(ResponseTemplate::new(unload_status))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/status"))
            .respond_with(release_status)
            .mount(&server)
            .await;
        let adapter = OmlxAdapter::new(OmlxAdapterConfig {
            base_url: server.uri(),
            api_key: None,
            call_timeout: Duration::from_millis(100),
        })
        .expect("adapter");
        let handle =
            Supervisor::spawn(Arc::new(adapter), SupervisorConfig::default()).expect("spawn");
        let err = handle
            .load("qwen3-8b", 20.0, on_demand())
            .await
            .expect_err("timeout");
        assert_eq!(err.reason_code(), "load_unconfirmed", "{err}");
        assert_eq!(handle.status().await.expect("status").used_gb, expected_gb);
        server.verify().await;
    }
}
