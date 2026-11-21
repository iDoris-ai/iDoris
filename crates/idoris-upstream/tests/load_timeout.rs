//! IDORIS-CORE S8（接口规范 §2.11.2「实现前提」）：oMLX 的 `POST /v1/models/{id}/load` 有自己的超时，与 `call_timeout`
//! 分开。冷加载大模型会超过 10 秒的管理调用超时；不分开，它会被当作「结果未知」并留下 `load.pending` 围栏。
//! 只有 `with_load_timeout` 打开之后才生效（生产的工厂打开它）；`status`、`unload`、`probe_ready`、`list` 与钉住（pin）调用永远用
//! `call_timeout`。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use idoris_backend::{RuntimeAdapter, Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::omlx::{
    DEFAULT_LOAD_TIMEOUT, LOAD_TIMEOUT_ENV, MAX_LOAD_TIMEOUT_SECS, MIN_LOAD_TIMEOUT_SECS,
    load_timeout_from_env,
};
use idoris_upstream::{OmlxAdapter, OmlxAdapterConfig};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CALL: Duration = Duration::from_millis(250);

fn on_demand() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::OnDemand,
        keepalive: Keepalive::IdleTtl { idle_ttl_s: 60 },
        admission: Admission::Coexist,
    }
}

fn adapter(server: &MockServer) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        api_key: None,
        call_timeout: CALL,
    })
    .unwrap()
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
}

/// 假 oMLX：`load` 延迟 `load_delay`；`/v1/models/status` 延迟 `status_delay`，回答既当目录、固定状态，也当就绪探测。
async fn engine(load_delay: Duration, status_delay: Duration) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/m/load"))
        .respond_with(ResponseTemplate::new(200).set_delay(load_delay))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/models/m/unload"))
        .respond_with(ResponseTemplate::new(200).set_delay(status_delay))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(status_delay)
                .set_body_json(serde_json::json!({
                    "models": [{"id": "m", "loaded": true, "pinned": false, "estimated_size": 1 << 30}]
                })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(status_delay)
                .set_body_json(serde_json::json!({
                    "loaded_models": [], "model_memory_used": 0, "model_memory_max": 0
                })),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn a_load_slower_than_call_timeout_succeeds_once_the_adapter_has_its_own_load_timeout() {
    let server = engine(Duration::from_millis(700), Duration::ZERO).await;
    let adapter = adapter(&server).with_load_timeout(Duration::from_secs(5));
    let started = Instant::now();
    adapter.load("m", Some(&on_demand())).await.unwrap();
    assert!(
        started.elapsed() >= Duration::from_millis(650),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn without_one_a_load_keeps_call_timeout_and_is_an_unknown_outcome() {
    let server = engine(Duration::from_millis(1_500), Duration::ZERO).await;
    let error = adapter(&server)
        .load("m", Some(&on_demand()))
        .await
        .unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
}

#[tokio::test]
async fn a_load_that_outlasts_its_own_load_timeout_is_still_an_unknown_outcome() {
    let server = engine(Duration::from_millis(3_000), Duration::ZERO).await;
    let adapter = adapter(&server).with_load_timeout(Duration::from_millis(400));
    let started = Instant::now();
    let error = adapter.load("m", Some(&on_demand())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    assert!(
        started.elapsed() < Duration::from_millis(2_500),
        "the load timeout fired, not the engine's answer: {:?}",
        started.elapsed()
    );
    assert!(error.to_string().contains("timed out"), "{error}");
}

#[tokio::test]
async fn the_load_timeout_does_not_loosen_any_other_call() {
    // Every management call answers after 2 s, longer than call_timeout (250 ms) and shorter than the
    // 5 s load timeout: all of them must still time out at call_timeout.
    let server = engine(Duration::ZERO, Duration::from_millis(2_000)).await;
    let adapter = adapter(&server).with_load_timeout(Duration::from_secs(5));
    for (name, error) in [
        ("list", adapter.list().await.unwrap_err()),
        ("status", adapter.status().await.unwrap_err()),
        ("unload", adapter.unload("m").await.unwrap_err()),
    ] {
        assert!(error.to_string().contains("timed out"), "{name}: {error}");
    }
    // `probe_ready` swallows the failure into "not ready yet" (its contract) but must also give up at call_timeout.
    let started = Instant::now();
    assert!(!adapter.probe_ready("m").await.unwrap());
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "{:?}",
        started.elapsed()
    );
    // The pin/state check that follows a load is a management call too: a slow status after a fast load fails the load.
    let error = adapter.load("m", Some(&on_demand())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
}

#[tokio::test]
async fn the_adapter_declares_its_load_timeout_to_the_supervisor_only_when_it_has_one() {
    let server = MockServer::start().await;
    let plain = adapter(&server);
    assert_eq!(RuntimeAdapter::load_timeout(&plain), None);
    let declared = adapter(&server).with_load_timeout(Duration::from_secs(42));
    assert_eq!(
        RuntimeAdapter::load_timeout(&declared),
        Some(Duration::from_secs(42))
    );
}

/// 端到端：真的 `OmlxAdapter` 在 Supervisor 之后。适配器声明了加载预算，Supervisor 的外层兜底（预算加余量）不会在 30 秒
/// 的通用超时处截断它；这里把通用超时缩到 250 毫秒来证明。
#[tokio::test]
async fn the_supervisor_honours_the_adapters_load_budget_in_front_of_a_real_adapter() {
    let server = engine(Duration::from_millis(700), Duration::ZERO).await;
    let adapter = adapter(&server).with_load_timeout(Duration::from_secs(5));
    let handle = Supervisor::spawn(
        Arc::new(adapter),
        SupervisorConfig {
            adapter_call_timeout: CALL,
            ..SupervisorConfig::default()
        },
    )
    .unwrap();
    handle.load("m", 1.0, on_demand()).await.unwrap();
    assert!(handle.is_ready("m").await.unwrap());
}

// ---------------------------------------------------------------------------------------------
// `IDORIS_LOAD_TIMEOUT_S`
// ---------------------------------------------------------------------------------------------

fn parse(raw: &str) -> Result<Duration, String> {
    load_timeout_from_env(Some(OsStr::new(raw))).map_err(|error| error.to_string())
}

#[test]
fn the_default_load_timeout_is_180_seconds_and_an_unset_or_blank_variable_means_the_default() {
    assert_eq!(DEFAULT_LOAD_TIMEOUT, Duration::from_secs(180));
    assert_eq!(load_timeout_from_env(None).unwrap(), DEFAULT_LOAD_TIMEOUT);
    for blank in ["", " ", "\t", "  \n"] {
        assert_eq!(parse(blank), Ok(DEFAULT_LOAD_TIMEOUT), "{blank:?}");
    }
}

#[test]
fn a_whole_number_of_seconds_in_range_is_taken_as_given() {
    assert_eq!(MIN_LOAD_TIMEOUT_SECS, 1);
    assert_eq!(MAX_LOAD_TIMEOUT_SECS, 3600);
    for (raw, secs) in [
        ("1", 1),
        ("7", 7),
        (" 42 ", 42),
        ("180", 180),
        ("3599", 3599),
        ("3600", 3600),
        ("007", 7),
    ] {
        assert_eq!(parse(raw), Ok(Duration::from_secs(secs)), "{raw:?}");
    }
}

#[test]
fn anything_else_is_an_error_naming_the_variable_never_a_silent_default() {
    for raw in [
        "0",
        "3601",
        "99999999999999999999",
        "-1",
        "+5",
        "1.5",
        "1e3",
        "abc",
        "10s",
        "5 0",
        "１２",
    ] {
        let error = parse(raw).unwrap_err();
        assert!(error.contains(LOAD_TIMEOUT_ENV), "{raw:?}: {error}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let not_utf8 = OsStr::from_bytes(&[0x31, 0xff]);
        let error = load_timeout_from_env(Some(not_utf8))
            .unwrap_err()
            .to_string();
        assert!(error.contains(LOAD_TIMEOUT_ENV), "{error}");
    }
}

/// 生产工厂给适配器带上加载超时（环境里没设置覆盖值时是 180 秒）；非 omlx 的 provider 仍然没有适配器。
#[test]
fn the_production_factory_gives_the_omlx_adapter_a_load_timeout() {
    let card: idoris_contracts::ComponentCard = serde_json::from_value(serde_json::json!({
        "provider": {"id": "omlx", "family": "local", "tier": "local",
            "capabilities": ["chat"], "privacy_class": "local_only",
            "cost": {"input_per_m": 0, "output_per_m": 0}, "locality": "loopback"},
        "form": "http_service", "endpoint": "http://127.0.0.1:1", "version_pin": "omlx@0.6.4",
        "privacy_class": "local_only", "allowed_egress": ["loopback"],
        "fallback_policy": "fail_closed", "fail_closed": true
    }))
    .unwrap();
    let adapter = idoris_upstream::factory::create_adapter(&card).unwrap();
    let expected = load_timeout_from_env(std::env::var_os(LOAD_TIMEOUT_ENV).as_deref()).unwrap();
    assert_eq!(adapter.load_timeout(), Some(expected));
}
