//! IDORIS-CORE S8（接口规范 §2.11.2「实现前提」）：oMLX 的一次加载有自己的超时，与 `call_timeout` 分开。冷加载大模型会超过
//! 10 秒的管理调用超时；不分开，它会被当作「结果未知」并留下 `load.pending` 围栏。
//! 只有 `with_load_timeout` 打开之后才生效（生产的工厂打开它）。这个预算管**整个加载**：`POST …/load`，以及它之后的钉住（pin）与
//! 状态核对（#634 预审 W-3），全部要在预算里完成，否则报「结果未知」（`load_unconfirmed`）；`status`、`unload`、`probe_ready`、
//! `list` 永远用 `call_timeout`。
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::ffi::OsStr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use idoris_backend::{RuntimeAdapter, Supervisor, SupervisorConfig};
use idoris_contracts::LoadPolicy;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use idoris_upstream::omlx::{LOAD_TIMEOUT_ENV, load_timeout_from_env};
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
// 加载预算管整个加载（#634 预审 W-3）：POST 之后的钉住与状态核对也在同一个预算里
// ---------------------------------------------------------------------------------------------

fn resident() -> LoadPolicy {
    LoadPolicy {
        mode: LoadMode::Resident,
        keepalive: Keepalive::Pinned { pinned: true },
        admission: Admission::Coexist,
    }
}

/// 像生产那样：通用的管理调用超时是 10 秒，比下面用例里的加载预算长得多；带 key，resident 的钉住要登录管理会话。确认下限设成 0：
/// 这一组用例看的是预算本身（后续步骤只拿到 POST 剩下的部分），下限另有用例（见下面「确认下限」一节）。
fn adapter_with_call_timeout(server: &MockServer, call_timeout: Duration) -> OmlxAdapter {
    OmlxAdapter::new(OmlxAdapterConfig {
        base_url: server.uri(),
        api_key: Some("test-key".to_string()),
        call_timeout,
    })
    .unwrap()
    .with_load_fence_path(idoris_backend::mock::temporary_load_fence_path())
    .with_confirmation_floor(Duration::ZERO)
}

/// 假 oMLX，resident 路径的全套：`load`、管理会话登录、`PUT .../settings`（钉住）、核对用的 `GET /v1/models/status`（已钉住）。
async fn resident_engine(
    load_delay: Duration,
    put_delay: Duration,
    status_delay: Duration,
) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/models/m/load"))
        .respond_with(ResponseTemplate::new(200).set_delay(load_delay))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/admin/api/login"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("set-cookie", "omlx_admin_session=test-session; HttpOnly")
                .set_body_json(serde_json::json!({"success": true})),
        )
        .mount(&server)
        .await;
    Mock::given(method("PUT"))
        .and(path("/admin/api/models/m/settings"))
        .respond_with(ResponseTemplate::new(200).set_delay(put_delay))
        .mount(&server)
        .await;
    // Supervisor 启动时采样的那一份（`GET /api/status`）。
    Mock::given(method("GET"))
        .and(path("/api/status"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "loaded_models": [], "model_memory_used": 0, "model_memory_max": 0
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1/models/status"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(status_delay)
                .set_body_json(serde_json::json!({
                    "models": [{"id": "m", "loaded": true, "pinned": true, "estimated_size": 1 << 30}]
                })),
        )
        .mount(&server)
        .await;
    server
}

#[tokio::test]
async fn the_state_check_after_a_load_is_inside_the_load_budget() {
    // POST 马上答；核对状态的那次 GET 要 3 秒：在单次调用的 10 秒之内，却比整个 1 秒的加载预算长。预算管整个加载，所以在 1 秒
    // 处就放弃，报「结果未知」；不然要等到 3 秒才答，而且会成功。
    let server = engine(Duration::from_millis(200), Duration::from_secs(3)).await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(1));
    let started = Instant::now();
    let error = adapter.load("m", Some(&on_demand())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    assert!(error.to_string().contains("load budget"), "{error}");
    assert!(
        started.elapsed() < Duration::from_millis(2_300),
        "the budget cut the check at about 1 s: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_pin_steps_of_a_resident_load_are_inside_the_load_budget() {
    // 同上，resident 路径：登录与 PUT（钉住）也属于这次加载。PUT 要 3 秒，预算 1 秒。
    let server = resident_engine(
        Duration::from_millis(200),
        Duration::from_secs(3),
        Duration::ZERO,
    )
    .await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(1));
    let started = Instant::now();
    let error = adapter.load("m", Some(&resident())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    assert!(
        started.elapsed() < Duration::from_millis(2_300),
        "{:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn the_follow_up_gets_only_what_the_post_left_of_the_budget() {
    // 预算 3 秒，POST 用掉 2 秒，核对要 3 秒：只剩 1 秒，所以总共约 3 秒就放弃；若核对又拿到一整个预算，要到约 5 秒。
    let server = engine(Duration::from_secs(2), Duration::from_secs(3)).await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(3));
    let started = Instant::now();
    let error = adapter.load("m", Some(&on_demand())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(2_800) && waited < Duration::from_millis(4_300),
        "the whole load, POST included, stayed inside the 3 s budget: {waited:?}"
    );
}

#[tokio::test]
async fn a_load_that_confirms_inside_the_budget_still_succeeds() {
    // 对照：POST 与核对加起来比单次调用的超时长，却在预算之内，加载成功（预算不是把后续步骤变严）。
    let server = engine(Duration::from_millis(500), Duration::from_millis(400)).await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(5));
    adapter.load("m", Some(&on_demand())).await.unwrap();
    let server = resident_engine(
        Duration::from_millis(300),
        Duration::from_millis(300),
        Duration::from_millis(300),
    )
    .await;
    adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(5))
        .load("m", Some(&resident()))
        .await
        .unwrap();
}

#[tokio::test]
async fn without_a_declared_budget_each_follow_up_call_keeps_its_own_call_timeout() {
    // 没有 `with_load_timeout` 的适配器（v0.5.0 的行为）：POST 用 `call_timeout`，之后的每次调用各用各的 `call_timeout`，没有「整个加载」
    // 的总预算。POST 1.5 秒加核对 1 秒，共 2.5 秒，比 2 秒的 `call_timeout` 长，却每次都在它之内：成功。
    let server = engine(Duration::from_millis(1_500), Duration::from_secs(1)).await;
    adapter_with_call_timeout(&server, Duration::from_secs(2))
        .load("m", Some(&on_demand()))
        .await
        .unwrap();
}

/// 端到端：真的 `OmlxAdapter` 在 Supervisor 之后。后续步骤卡住时，报的是适配器自己的「结果未知」（`load_unconfirmed`，围栏保留），
/// 不是 Supervisor 外层兜底的 `adapter_timed_out`。
#[tokio::test]
async fn a_stalled_confirmation_is_reported_as_load_unconfirmed_not_as_the_supervisors_timeout() {
    let server = resident_engine(
        Duration::from_millis(200),
        Duration::from_secs(5),
        Duration::ZERO,
    )
    .await;
    let fence = idoris_backend::mock::temporary_load_fence_path();
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_fence_path(fence.clone())
        .with_load_timeout(Duration::from_secs(1));
    let handle = Supervisor::spawn(Arc::new(adapter), SupervisorConfig::default()).unwrap();
    let error = handle.load("m", 1.0, resident()).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    assert!(fence.exists(), "an unknown outcome keeps the durable fence");
}

// ---------------------------------------------------------------------------------------------
// 确认下限（本地预审 L-5）：引擎在预算快用完时才接受了加载，确认仍然有一个下限的时间
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_load_accepted_at_the_end_of_the_budget_still_gets_the_confirmation_floor() {
    // 预算 3 秒，POST 用 1.5 秒，只剩 1.5 秒；核对要 2.5 秒：比剩下的长，比 4 秒的下限短。有下限，它成功（总共约 4 秒，超出预算
    // 约 1 秒）；没有下限，1.5 秒处就是「结果未知」，围栏保留，可模型其实已经加载好了。
    let server = engine(Duration::from_millis(1_500), Duration::from_millis(2_500)).await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(3))
        .with_confirmation_floor(Duration::from_secs(4));
    let started = Instant::now();
    adapter.load("m", Some(&on_demand())).await.unwrap();
    let took = started.elapsed();
    assert!(
        took > Duration::from_secs(3),
        "the confirmation ran past the budget, on the floor: {took:?}"
    );
}

#[tokio::test]
async fn the_floor_is_the_most_the_confirmation_may_run_past_the_budget() {
    // 对照：下限不是白送的时间。预算 1 秒，POST 用 0.2 秒，下限 1 秒，核对要 4 秒（单次调用的 10 秒之内）：拿到的是
    // max(剩下的 0.8 秒, 下限 1 秒) = 1 秒，所以约 1.2 秒就放弃，不是等到 4 秒。
    let server = engine(Duration::from_millis(200), Duration::from_secs(4)).await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(10))
        .with_load_timeout(Duration::from_secs(1))
        .with_confirmation_floor(Duration::from_secs(1));
    let started = Instant::now();
    let error = adapter.load("m", Some(&on_demand())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(1_100) && waited < Duration::from_millis(2_800),
        "cut at about budget + 0.2 s, not at the 4 s the check needs: {waited:?}"
    );
}

#[tokio::test]
async fn the_floor_never_exceeds_the_one_the_supervisors_backstop_accounts_for() {
    // 下限最多是 `LOAD_CONFIRMATION_FLOOR`（Supervisor 的兜底按它算）：要一个 10 分钟的下限，得到的还是它。核对要 7 秒：
    // 比预算剩下的长，也比 5 秒的下限长，所以 5 秒处放弃，不是 10 分钟。
    let server = engine(Duration::from_millis(200), Duration::from_secs(7)).await;
    let adapter = adapter_with_call_timeout(&server, Duration::from_secs(30))
        .with_load_timeout(Duration::from_secs(1))
        .with_confirmation_floor(Duration::from_secs(600));
    let started = Instant::now();
    let error = adapter.load("m", Some(&on_demand())).await.unwrap_err();
    assert_eq!(error.reason_code(), "load_unconfirmed", "{error}");
    let waited = started.elapsed();
    assert!(
        waited >= Duration::from_millis(4_800) && waited < Duration::from_millis(6_800),
        "cut at about 5 s (the floor, errata eight (6)), not at 10 minutes or at the 7 s the check needs: {waited:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// `IDORIS_LOAD_TIMEOUT_S`
// ---------------------------------------------------------------------------------------------

fn parse(raw: &str) -> Result<Duration, String> {
    load_timeout_from_env(Some(OsStr::new(raw))).map_err(|error| error.to_string())
}

#[test]
fn an_unset_or_blank_variable_means_the_documented_default_of_180_seconds() {
    // 规范 §2.11.2 与勘误八（6）对外写的缺省是 180 秒：断言的是解析函数交出来的值，对的是规范里的数，不是实现里常量自己。
    let documented = Duration::from_secs(180);
    assert_eq!(load_timeout_from_env(None).unwrap(), documented);
    for blank in ["", " ", "\t", "  \n"] {
        assert_eq!(parse(blank), Ok(documented), "{blank:?}");
    }
}

#[test]
fn a_whole_number_of_seconds_in_range_is_taken_as_given() {
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
