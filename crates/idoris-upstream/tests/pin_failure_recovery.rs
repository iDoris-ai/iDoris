//! End-to-end regression for a deterministic oMLX admin-key rejection.
//! The inference key can still read model status, so an accepted load that
//! is loaded but unpinned is a completed postcondition failure. Successful
//! unload must release both the supervisor reservation and durable fence.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use idoris_backend::{Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Default)]
struct Engine(Arc<Mutex<HashSet<String>>>);

impl Engine {
    fn set(&self, id: &str, loaded: bool) {
        let mut models = self.0.lock().expect("engine state mutex");
        if loaded {
            models.insert(id.to_owned());
        } else {
            models.remove(id);
        }
    }

    fn snapshot(&self) -> Vec<String> {
        self.0
            .lock()
            .expect("engine state mutex")
            .iter()
            .cloned()
            .collect()
    }

    fn memory_gib(&self) -> u64 {
        self.0
            .lock()
            .expect("engine state mutex")
            .iter()
            .map(|id| if id == "qwen3-8b" { 20 } else { 2 })
            .sum()
    }
}

struct EngineStatus(Engine);

impl Respond for EngineStatus {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let models = self.0.snapshot();
        let used = self.0.memory_gib();
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": models,
            "model_memory_used": used * GIB,
            "model_memory_max": 0
        }))
    }
}

struct ModelChange {
    engine: Engine,
    id: &'static str,
    loaded: bool,
    status: u16,
}

impl Respond for ModelChange {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        if (200..300).contains(&self.status) {
            self.engine.set(self.id, self.loaded);
        }
        ResponseTemplate::new(self.status)
    }
}

struct OmlxModelStatus(Engine);

impl Respond for OmlxModelStatus {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let loaded = self.0.snapshot();
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "models": [
                {"id": "qwen3-8b", "loaded": loaded.iter().any(|id| id == "qwen3-8b"), "pinned": false, "estimated_size": 20 * GIB},
                {"id": "other-model", "loaded": loaded.iter().any(|id| id == "other-model"), "pinned": false, "estimated_size": 2 * GIB}
            ]
        }))
    }
}

fn resident() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::Resident,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    }
}

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

fn adapter(server: &MockServer, fence: std::path::PathBuf) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        // This simulates an inference sub-key: load/status work while admin
        // login deterministically rejects the same key.
        api_key: Some("inference-sub-key".into()),
        call_timeout: Duration::from_millis(500),
    })
    .expect("adapter")
    .with_load_fence_path(fence)
}

async fn spawn_after_owner_release(
    server: &MockServer,
    fence: std::path::PathBuf,
    config: SupervisorConfig,
) -> idoris_backend::SupervisorHandle {
    let mut last_error = String::new();
    for _ in 0..100 {
        let handle = Supervisor::spawn(Arc::new(adapter(server, fence.clone())), config.clone())
            .expect("supervisor spawn");
        match handle.status().await {
            Ok(_) => return handle,
            Err(error) => {
                last_error = error.to_string();
                drop(handle);
                if !last_error.contains("claiming engine ownership") {
                    panic!(
                        "supervisor startup failed for a reason other than ownership: {last_error}"
                    );
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    }
    panic!("supervisor did not acquire engine ownership: {last_error}");
}

#[tokio::test]
async fn rejected_admin_pin_is_released_and_supervisor_remains_usable() {
    let server = MockServer::start().await;
    let fence = idoris_backend::mock::temporary_load_fence_path();

    let engine = Engine::default();
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(EngineStatus(engine.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: true,
            status: 200,
        })
        .up_to_n_times(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(OmlxModelStatus(engine.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: false,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/other-model/load"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "other-model",
            loaded: true,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [
                {"id": "qwen3-8b", "object": "model"},
                {"id": "other-model", "object": "model"}
            ]
        })))
        .mount(&server)
        .await;

    let config = SupervisorConfig {
        probe_interval: Duration::from_millis(1),
        probe_max_attempts: 2,
        release_confirm_interval: Duration::from_millis(1),
        release_confirm_max_attempts: 2,
        ..SupervisorConfig::default()
    };
    let first = spawn_after_owner_release(&server, fence.clone(), config.clone()).await;
    let error = first
        .load("qwen3-8b", 20.0, resident())
        .await
        .expect_err("sub-key cannot pin through admin API");
    assert_eq!(error.reason_code(), "load_postcondition_failed", "{error}");
    assert_eq!(first.status().await.expect("status").used_gb, 0.0);

    // A successful cleanup clears the fence and budget, so this actor can
    // accept a different model immediately.
    first
        .load("other-model", 2.0, on_demand())
        .await
        .expect("same supervisor can load another model");
    assert_eq!(first.status().await.expect("status").used_gb, 2.0);
    drop(first);

    // Reconciliation observes the other model actually held by the engine.
    // The prior qwen failure left no fence, so ordinary adapter requests work.
    let restarted = spawn_after_owner_release(&server, fence, config).await;
    assert_eq!(
        restarted.status().await.expect("reconciled status").used_gb,
        2.0
    );
    restarted
        .load("qwen3-8b", 20.0, on_demand())
        .await
        .expect("load succeeds after restart; prior fence was cleared");
    assert_eq!(
        restarted.status().await.expect("loaded status").used_gb,
        22.0
    );
    let requests = server.received_requests().await.expect("requests");
    assert!(requests.iter().any(|request| {
        request.method == wiremock::http::Method::POST && request.url.path() == "/admin/api/login"
    }));
    server.verify().await;
}

#[tokio::test]
async fn failed_cleanup_keeps_budget_then_explicit_unload_recovers() {
    let server = MockServer::start().await;
    let fence = idoris_backend::mock::temporary_load_fence_path();
    let engine = Engine::default();
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(EngineStatus(engine.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(OmlxModelStatus(engine.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: true,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: false,
            status: 500,
        })
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: false,
            status: 200,
        })
        .with_priority(10)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/other-model/load"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "other-model",
            loaded: true,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/other-model/unload"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "other-model",
            loaded: false,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": [{"id": "qwen3-8b", "object": "model"}]
        })))
        .mount(&server)
        .await;

    let config = SupervisorConfig {
        release_confirm_interval: Duration::from_millis(1),
        release_confirm_max_attempts: 1,
        ..SupervisorConfig::default()
    };
    let handle = spawn_after_owner_release(&server, fence.clone(), config.clone()).await;
    let error = handle
        .load("qwen3-8b", 20.0, resident())
        .await
        .expect_err("pin postcondition fails");
    assert_eq!(error.reason_code(), "load_postcondition_failed", "{error}");
    assert_eq!(
        handle.status().await.expect("retained status").used_gb,
        20.0
    );

    handle
        .load("other-model", 2.0, on_demand())
        .await
        .expect("remaining four GiB can load another model");
    assert_eq!(handle.status().await.expect("both models").used_gb, 22.0);
    handle
        .unload("other-model")
        .await
        .expect("release other model");
    assert_eq!(handle.status().await.expect("qwen retained").used_gb, 20.0);
    drop(handle);

    let restarted = spawn_after_owner_release(&server, fence, config).await;
    assert_eq!(
        restarted.status().await.expect("reconciled status").used_gb,
        20.0
    );
    restarted
        .unload("qwen3-8b")
        .await
        .expect("release reconciled qwen");
    assert_eq!(
        restarted.status().await.expect("released status").used_gb,
        0.0
    );
    assert!(
        restarted.list().await.is_ok(),
        "fence was cleared after confirmed unload"
    );
    server.verify().await;
}

#[tokio::test]
async fn pin_transport_timeout_keeps_load_unconfirmed_and_durable_fence() {
    let server = MockServer::start().await;
    let fence = idoris_backend::mock::temporary_load_fence_path();
    let engine = Engine::default();
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(EngineStatus(engine.clone()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/load"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: true,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "omlx_admin_session=test; Path=/")
                .set_body_json(serde_json::json!({"success": true})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/admin/api/models/qwen3-8b/settings"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_millis(250)))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/qwen3-8b/unload"))
        .respond_with(ModelChange {
            engine: engine.clone(),
            id: "qwen3-8b",
            loaded: false,
            status: 200,
        })
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(OmlxModelStatus(engine.clone()))
        .expect(0)
        .mount(&server)
        .await;

    let make_adapter = || {
        OmlxAdapter::new(OmlxAdapterConfig {
            base_url: server.uri(),
            api_key: Some("main-key-for-timeout-test".into()),
            call_timeout: Duration::from_millis(50),
        })
        .expect("adapter")
        .with_load_fence_path(fence.clone())
    };
    let config = SupervisorConfig {
        release_confirm_interval: Duration::from_millis(1),
        release_confirm_max_attempts: 1,
        ..SupervisorConfig::default()
    };
    let first =
        Supervisor::spawn(Arc::new(make_adapter()), config.clone()).expect("first supervisor");
    let error = first
        .load("qwen3-8b", 20.0, resident())
        .await
        .expect_err("slow pin response leaves the operation unconfirmed");
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    assert_eq!(first.status().await.expect("retained status").used_gb, 20.0);
    drop(first);

    let mut restart_error = None;
    for _ in 0..100 {
        let restarted = Supervisor::spawn(Arc::new(make_adapter()), config.clone())
            .expect("restarted supervisor spawn");
        match restarted.status().await {
            Ok(_) => panic!("durable fence should reject restart status"),
            Err(error) if error.to_string().contains("claiming engine ownership") => {
                drop(restarted);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            Err(error) => {
                restart_error = Some(error);
                drop(restarted);
                break;
            }
        }
    }
    let restart_error = restart_error.expect("restart must observe durable fence");
    assert_eq!(restart_error.reason_code(), "internal", "{restart_error}");
    assert!(restart_error.to_string().contains("durable load fence"));
    server.verify().await;
}
