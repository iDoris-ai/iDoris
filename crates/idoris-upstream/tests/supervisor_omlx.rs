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
    .expect("adapter must build")
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path());
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
    assert_eq!(run(ResponseTemplate::new(503), 200).await, 20.0);
}

#[tokio::test]
async fn external_pin_drift_after_load_triggers_a_real_unload() {
    let pinned = ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "models": [{"id": "qwen3-8b", "loaded": true, "pinned": true}]
    }));
    assert_eq!(run(pinned, 200).await, 20.0);
}

/// If the unload itself fails, the memory may still be held: the ledger
/// must keep counting it instead of reporting it free.
#[tokio::test]
async fn a_failed_release_keeps_the_memory_on_the_ledger() {
    assert_eq!(run(ResponseTemplate::new(503), 500).await, 20.0);
}

/// Startup reconciliation reserves memory already held by another client.
/// A managed load must add its estimate on top, then release only that
/// estimate while preserving the pre-existing residency.
#[tokio::test]
async fn load_and_unload_preserve_other_resident_models() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let server = MockServer::start().await;
    let status = |models: &[&str], used: u64| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": models,
            "model_memory_used": used,
            "model_memory_max": 0
        }))
    };

    // Supervisor startup sees another model occupying 8 GiB. The next
    // sample confirms the new model at 9 GiB; release polling then sees 8.
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(status(&["already-resident"], 8 * GIB))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(status(&["already-resident", "qwen3-8b"], 9 * GIB))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(status(&["already-resident"], 8 * GIB))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": true, "pinned": false}]
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;

    let adapter = OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        api_key: None,
        call_timeout: Duration::from_millis(500),
    })
    .expect("adapter")
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path());
    let handle = Supervisor::spawn(Arc::new(adapter), SupervisorConfig::default()).expect("spawn");
    let baseline = handle.status().await.expect("baseline status");
    assert_eq!(baseline.used_gb, 8.0);

    handle
        .load("qwen3-8b", 1.0, on_demand())
        .await
        .expect("load alongside existing model");
    let loaded = handle.status().await.expect("loaded status");
    assert_eq!(loaded.used_gb, 9.0);
    assert!(loaded.loaded.contains(&"qwen3-8b".to_string()));

    handle
        .unload("qwen3-8b")
        .await
        .expect("unload managed model");
    let unloaded = handle.status().await.expect("unloaded status");
    assert_eq!(unloaded.used_gb, 8.0);
    assert!(!unloaded.loaded.contains(&"qwen3-8b".to_string()));
    server.verify().await;
}

/// A fresh Supervisor using the same engine fence path must refuse another
/// load after the first accepted load failed readiness, even though cleanup
/// saw empty engine status.
#[tokio::test]
async fn probe_timeout_fence_survives_recreating_the_adapter() {
    let server = MockServer::start().await;
    let fence_path = idoris_backend::mock::temporary_load_fence_path();
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ResponseTemplate::new(200))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": true, "pinned": false}]
        })))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [{"id": "qwen3-8b", "loaded": false, "pinned": false}]
        })))
        .mount(&server)
        .await;

    let make_adapter = || {
        OmlxAdapter::new(OmlxAdapterConfig {
            base_url: server.uri(),
            api_key: None,
            call_timeout: Duration::from_millis(500),
        })
        .expect("adapter")
        .with_load_fence_path(fence_path.clone())
    };
    let config = SupervisorConfig {
        probe_interval: Duration::from_millis(1),
        probe_max_attempts: 1,
        ..SupervisorConfig::default()
    };
    let first =
        Supervisor::spawn(Arc::new(make_adapter()), config.clone()).expect("first supervisor");
    let first_error = first
        .load("qwen3-8b", 1.0, on_demand())
        .await
        .expect_err("the accepted load must fail readiness");
    assert_eq!(
        first_error.reason_code(),
        "probe_timed_out",
        "{first_error}"
    );
    drop(first);

    let second = Supervisor::spawn(Arc::new(make_adapter()), config).expect("recreated supervisor");
    let second_error = second
        .load("qwen3-8b", 1.0, on_demand())
        .await
        .expect_err("the durable fence must refuse another load");
    assert_eq!(second_error.reason_code(), "internal", "{second_error}");
    assert!(second_error.to_string().contains("durable load fence"));
    server.verify().await;
}

/// K09/K11: the adapter deadline fires before the Supervisor's outer
/// deadline. A received load may still allocate after any unload ack and
/// apparently empty status response, so the ledger must retain its estimate.
#[tokio::test]
async fn k09_load_timeout_retains_occupancy_after_unload_and_empty_status() {
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
        (200, status_body(&[], 0), 20.0),
    ];
    for (unload_status, release_status, expected_gb) in scenarios {
        let server = MockServer::start().await;
        // Only the startup sample is empty; later polls use release_status below.
        Mock::given(method("GET"))
            .and(path("/api/status"))
            .respond_with(status_body(&[], 0))
            .with_priority(1)
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
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
        .expect("adapter")
        .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path());
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
