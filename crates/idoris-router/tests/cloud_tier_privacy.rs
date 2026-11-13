//! 云端档（计算服务 S7）的隐私硬闸：**`local_only` 永不出网**，以及「有同意、隐私 `any`、策略允许、本机服务不了」之外的一切都不出网。
//!
//! 证据是三件事一起看，不只看「返回了什么」：
//! 1. 调用方看到的响应（状态码、错误体、全部响应头）和**没有云端档时逐字节相同**；
//! 2. 云端收到的 HTTP 请求数是 0（计数中间件在认证之前，连 401 的也数）；
//! 3. 本机的调用记录里没有任何行。
//!
//! 云是 S5 的云形态应用（真实的账户、账本、云入口，挂在 `127.0.0.1` 的 TCP 上），本机是进程内的 `AppState` + 假 oMLX
//! 运行时（`MockAdapter`）。**不连接任何真实的外部服务，不启动任何推理进程。**
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/cloud_tier_harness.rs"]
mod h;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use h::*;
use idoris_router::cloud::Clock;
use idoris_router::cloud_tier::{CloudTier, CredentialProvider};
use idoris_tenancy::cloud_tier::{CallOutcome, CallState};
use serde_json::json;

const CONSENT: (&str, &str) = ("x-idoris-cloud-fallback", "allow");
const ANY: (&str, &str) = ("x-idoris-privacy", "any");
const LOCAL_ONLY: (&str, &str) = ("x-idoris-privacy", "local_only");

/// 一个已经配对、凭证有效的云端档，连着一个正常工作的云。
struct Rig {
    cloud: TcpCloud,
    tier: Arc<CloudTier>,
    _dir: tempfile::TempDir,
}

async fn rig() -> Rig {
    rig_hooked(
        |provider| provider,
        Arc::new(idoris_router::cloud::SystemClock),
    )
    .await
}

/// 同 [`rig`]，但凭证来源和时钟由调用方包一层：测试在「通过闸门之后、出网之前」的某一步做手脚。
async fn rig_hooked(
    wrap_provider: impl FnOnce(Arc<dyn CredentialProvider>) -> Arc<dyn CredentialProvider>,
    clock: Arc<dyn Clock>,
) -> Rig {
    let cloud = TcpCloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier");
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    pair_device(&cloud, &state_dir, &code).await.unwrap();
    let tier = make_tier_with_clock(
        parse_tier_config(&tier_config_yaml(&cloud, Some(&state_dir), "")),
        wrap_provider(paired_provider(&cloud, &state_dir)),
        clock,
        &dir.path().join("tier.sqlite3"),
    );
    // 配对本身是对云控制面的请求；清掉，之后计数里只剩被测的请求
    cloud.log.clear();
    Rig {
        cloud,
        tier,
        _dir: dir,
    }
}

impl Rig {
    fn node(&self, scenario: Scenario) -> LocalNode {
        LocalNode::build(NodeOptions::new(scenario, Some(self.tier.clone())))
    }

    fn calls(&self) -> u64 {
        self.tier.store().call_count().unwrap()
    }
}

fn baseline(scenario: Scenario) -> LocalNode {
    LocalNode::build(NodeOptions::new(scenario, None))
}

fn describe(scenario: Scenario, headers: &[(&str, &str)]) -> String {
    format!("{scenario:?} with {headers:?}")
}

// ================================================================================================
// local_only 永不出网（最重要）
// ================================================================================================

#[tokio::test]
async fn local_only_never_reaches_the_cloud_whatever_goes_wrong_locally() {
    let rig = rig().await;
    // 隐私声明：显式 local_only，没声明（保守默认 = local_only）。同意头、降级声明、余量确认一应俱全地带着——
    // 全部加起来也不能让 local_only 出网。
    let variants: [&[(&str, &str)]; 4] = [
        &[LOCAL_ONLY],
        &[],
        &[LOCAL_ONLY, CONSENT, ("x-idoris-fallback", "next_in_chain")],
        &[CONSENT],
    ];
    for scenario in Scenario::FAILING {
        for headers in variants {
            let without = baseline(scenario).drive(headers).await;
            let with = rig.node(scenario).drive(headers).await;
            assert_ne!(
                without.status,
                200,
                "{}: the scenario must actually fail locally ({})",
                describe(scenario, headers),
                without.text()
            );
            assert_eq!(
                with.fingerprint(),
                without.fingerprint(),
                "{}: the response must be byte-identical to the one without a cloud tier\nwith:    {}\nwithout: {}",
                describe(scenario, headers),
                with.text(),
                without.text()
            );
            assert!(
                with.header("x-idoris-cloud-attempt").is_none()
                    && with.header("x-idoris-cloud-record-id").is_none(),
                "{}: not even an additive header may appear when nothing left the machine",
                describe(scenario, headers)
            );
        }
    }
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "the cloud received a request from a local_only call"
    );
    assert_eq!(
        rig.calls(),
        0,
        "nothing may be recorded as an egress attempt"
    );
}

#[tokio::test]
async fn the_local_failures_are_the_ones_the_scenarios_claim_to_be() {
    // 防止场景悄悄变成别的失败：每个场景在 local_only 下的错误体就是它声称的那一种。
    let expected: [(Scenario, u16, &str, &str); 10] = [
        (
            Scenario::RuntimeOffline,
            503,
            "local_only_unavailable",
            "supervisor_unavailable",
        ),
        (Scenario::RuntimeError, 503, "local_only_unavailable", ""),
        (
            Scenario::MemoryTooSmall,
            503,
            "local_only_unavailable",
            "eviction_impossible",
        ),
        (Scenario::Oom, 503, "local_only_unavailable", "oom"),
        (
            Scenario::HeadroomNeeded,
            503,
            "headroom_confirmation_required",
            "headroom_confirmation_required",
        ),
        (
            Scenario::HeadroomConfirmedStillTooBig,
            503,
            "local_only_unavailable",
            "eviction_impossible",
        ),
        (
            Scenario::EvictionFailed,
            503,
            "local_only_unavailable",
            "eviction_failed",
        ),
        (
            Scenario::Overloaded,
            503,
            "local_only_unavailable",
            "supervisor_busy",
        ),
        (
            Scenario::BudgetExhausted,
            402,
            "budget_exceeded",
            "budget_exceeded",
        ),
        (Scenario::RoleUnbound, 503, "local_only_unavailable", ""),
    ];
    for (scenario, status, error_type, reason_code) in expected {
        let response = baseline(scenario).drive(&[LOCAL_ONLY]).await;
        assert_eq!(response.status, status, "{scenario:?}: {}", response.text());
        let body = response.json();
        assert_eq!(
            body["error"]["type"],
            error_type,
            "{scenario:?}: {}",
            response.text()
        );
        if !reason_code.is_empty() {
            assert_eq!(body["error"]["reason_code"], reason_code, "{scenario:?}");
        }
    }
}

#[tokio::test]
async fn confirming_the_headroom_is_not_permission_to_leave_the_machine() {
    let rig = rig().await;
    // 带了余量确认、用满全部额度仍然放不下；调用方声明 any，但没有同意出网
    let node = rig.node(Scenario::HeadroomConfirmedStillTooBig);
    let response = node.drive(&[ANY]).await;
    let baseline_response = baseline(Scenario::HeadroomConfirmedStillTooBig)
        .drive(&[ANY])
        .await;
    assert_eq!(response.fingerprint(), baseline_response.fingerprint());
    assert_eq!(response.status, 503);
    assert_eq!(response.json()["error"]["type"], "local_only_unavailable");
    assert_ne!(
        response.json()["error"]["reason_code"],
        "headroom_confirmation_required"
    );
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "the headroom confirmation was treated as egress consent"
    );
    // 同一个请求加上真正的同意，才会去云端
    let consented = rig
        .node(Scenario::HeadroomConfirmedStillTooBig)
        .drive(&[ANY, CONSENT])
        .await;
    assert_eq!(consented.status, 200, "{}", consented.text());
    assert_eq!(rig.cloud.log.chats().len(), 1);
}

#[tokio::test]
async fn needing_the_headroom_asks_the_user_and_never_goes_to_the_cloud_even_with_consent() {
    let rig = rig().await;
    let node = rig.node(Scenario::HeadroomNeeded);
    let response = node.drive(&[ANY, CONSENT]).await;
    assert_eq!(response.status, 503);
    assert_eq!(
        response.json()["error"]["type"],
        "headroom_confirmation_required"
    );
    assert_eq!(
        response.json()["error"]["reason_code"],
        "headroom_confirmation_required"
    );
    assert_eq!(
        response.fingerprint(),
        baseline(Scenario::HeadroomNeeded)
            .drive(&[ANY, CONSENT])
            .await
            .fingerprint()
    );
    assert_eq!(rig.cloud.log.count(), 0);
    // 同意出网也不会替用户动用保留的内存余量：本机什么都没加载
    assert_eq!(node.adapter.as_ref().unwrap().load_call_count("big"), 0);
}

// ================================================================================================
// 没有同意就不出网；长得像的头不是同意
// ================================================================================================

#[tokio::test]
async fn any_without_the_explicit_consent_never_reaches_the_cloud() {
    let rig = rig().await;
    let not_consent: [&[(&str, &str)]; 7] = [
        &[ANY],
        &[ANY, ("x-idoris-fallback", "next_in_chain")],
        &[ANY, ("x-idoris-cloud-fallback", "allowed")],
        &[ANY, ("x-idoris-cloud-fallback", "true")],
        &[ANY, ("x-idoris-cloud-fallback", "")],
        &[
            ANY,
            ("x-idoris-cloud-fallback", "allow"),
            ("x-idoris-cloud-fallback", "allow"),
        ],
        &[ANY, ("x-idoris-cloud-tier", "allow")],
    ];
    for scenario in Scenario::FAILING {
        for headers in not_consent {
            let without = baseline(scenario).drive(headers).await;
            let with = rig.node(scenario).drive(headers).await;
            assert_eq!(
                with.fingerprint(),
                without.fingerprint(),
                "{}",
                describe(scenario, headers)
            );
        }
    }
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "a request without the explicit consent reached the cloud"
    );
    assert_eq!(rig.calls(), 0);
}

// ================================================================================================
// 有同意：本机服务不了的才去云端；要问用户 / 要退避 / 预算 / 瞬时失败的不去
// ================================================================================================

#[tokio::test]
async fn with_consent_the_cloud_answers_exactly_the_failures_the_local_node_cannot_serve() {
    let rig = rig().await;
    for scenario in Scenario::FAILING {
        let before = rig.cloud.log.chats().len();
        let response = rig.node(scenario).drive(&[ANY, CONSENT]).await;
        let after = rig.cloud.log.chats().len();
        if scenario.goes_to_the_cloud() {
            assert_eq!(response.status, 200, "{scenario:?}: {}", response.text());
            assert_eq!(after - before, 1, "{scenario:?}");
            assert_eq!(
                response.header("x-idoris-served-locality"),
                Some("remote"),
                "{scenario:?}"
            );
            assert_eq!(
                response.header("x-idoris-degraded"),
                Some("true"),
                "{scenario:?}"
            );
            assert!(
                response
                    .header("x-idoris-reason")
                    .unwrap()
                    .contains("CloudTierFallback")
            );
            assert_eq!(response.json()["choices"][0]["message"]["content"], "hello");
        } else {
            let without = baseline(scenario).drive(&[ANY, CONSENT]).await;
            assert_eq!(
                response.fingerprint(),
                without.fingerprint(),
                "{scenario:?}: must stay exactly the local answer"
            );
            assert_eq!(after, before, "{scenario:?}: must not reach the cloud");
        }
    }
}

#[tokio::test]
async fn a_healthy_local_node_never_calls_the_cloud_even_with_every_permission() {
    let rig = rig().await;
    let response = rig.node(Scenario::Healthy).drive(&[ANY, CONSENT]).await;
    assert_eq!(response.status, 200);
    assert_ne!(response.header("x-idoris-served-locality"), Some("remote"));
    assert!(response.header("x-idoris-cloud-record-id").is_none());
    assert_eq!(rig.cloud.log.count(), 0);
    assert_eq!(rig.calls(), 0);
    assert_eq!(
        response.fingerprint().0,
        baseline(Scenario::Healthy)
            .drive(&[ANY, CONSENT])
            .await
            .fingerprint()
            .0
    );
}

#[tokio::test]
async fn the_cloud_only_ever_sees_what_the_caller_consented_to_send_and_nothing_local() {
    let rig = rig().await;
    let body = json!({
        "model": "idoris/daily",
        "user": "caller-1",
        "metadata": {"session": "s-1"},
        "messages": [{"role": "user", "name": "alice", "content": "hello cloud tier"}],
        "max_tokens": 64,
    });
    let node = rig.node(Scenario::RoleUnbound);
    let response = send_chat(&node.app, body, &[ANY, CONSENT]).await;
    assert_eq!(response.status, 200, "{}", response.text());
    let chats = rig.cloud.log.chats();
    assert_eq!(chats.len(), 1);
    let seen = &chats[0];
    let sent = seen.body.as_ref().unwrap();
    // 本机的角色别名不出网，换成配置的云端模型名
    assert_eq!(sent["model"], "idoris/agent");
    assert_eq!(sent["max_tokens"], 64);
    // 身份字段被清洗（和发往任何非 loopback 提供方的请求体同一个清洗）
    assert!(sent.get("user").is_none() && sent.get("metadata").is_none());
    assert!(sent["messages"][0].get("name").is_none());
    assert_eq!(sent["messages"][0]["content"], "hello cloud tier");
    // 请求头：只有凭证，没有调用方的任何头，没有成员头
    assert!(seen.has_authorization);
    assert!(!seen.has_member_header);
    for leaked in [
        "x-idoris-privacy",
        "x-idoris-cloud-fallback",
        "x-idoris-tenant",
        "x-idoris-request-id",
        "x-idoris-session",
    ] {
        assert!(
            !seen.header_names.iter().any(|name| name == leaked),
            "{leaked} was forwarded"
        );
    }
}

// ================================================================================================
// 内容检查、远程熔断、流式、路由策略
// ================================================================================================

#[tokio::test]
async fn content_the_gate_pins_local_never_leaves_even_when_everything_else_allows_it() {
    let rig = rig().await;
    let body = json!({
        "model": "small",
        "messages": [{"role": "user", "content": "证件 11010519491231002X，电话 13812345678"}],
    });
    let node = rig.node(Scenario::RuntimeOffline);
    let with = send_chat(&node.app, body.clone(), &[ANY, CONSENT]).await;
    let without = send_chat(
        &baseline(Scenario::RuntimeOffline).app,
        body,
        &[ANY, CONSENT],
    )
    .await;
    assert_eq!(with.status, 503);
    assert_eq!(with.fingerprint(), without.fingerprint());
    assert_eq!(rig.cloud.log.count(), 0, "PII left the machine");
}

#[tokio::test]
async fn the_remote_kill_switch_pins_everything_local() {
    let rig = rig().await;
    let mut options = NodeOptions::new(Scenario::RuntimeOffline, Some(rig.tier.clone()));
    options.kill_switch = true;
    let with = LocalNode::build(options).drive(&[ANY, CONSENT]).await;
    let mut baseline_options = NodeOptions::new(Scenario::RuntimeOffline, None);
    baseline_options.kill_switch = true;
    let without = LocalNode::build(baseline_options)
        .drive(&[ANY, CONSENT])
        .await;
    assert_eq!(with.status, 503);
    assert_eq!(with.fingerprint(), without.fingerprint());
    assert_eq!(rig.cloud.log.count(), 0);
}

// ------------------------------------------------------------------------------------------------
// 远程熔断以「出网那一刻」的值为准，不是请求进来时的快照（审查 r4 High#1）。
// 三个窗口各有一个测试，每个测试只对应一道检查：
//   本机处理期间打开 -> 闸门（第 1 层）读到新值，连凭证都不读；
//   闸门之后、出网之前打开（读凭证 / 占名额的时候）-> 出网前的第一次复核，不落 pending 行、不写 dispatched 事件；
//   落 pending 行的时候打开 -> `forward::send` 里最后一道检查，行写成 cancelled。
// 三种情况调用方拿到的都和没有云端档时逐字节相同（没有出网，连 X-iDoris-Cloud-Attempt 都没有），云端收到 0 个请求。
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_kill_switch_turned_on_while_the_local_attempt_runs_stops_the_fallback_at_the_gate() {
    let switch = Arc::new(AtomicBool::new(false));
    let provider: Arc<std::sync::Mutex<Option<Arc<ProbeProvider>>>> = Default::default();
    let rig = rig_hooked(
        {
            let provider = provider.clone();
            move |inner| {
                let counting = ProbeProvider::counting(inner);
                *provider.lock().unwrap() = Some(counting.clone());
                counting
            }
        },
        Arc::new(idoris_router::cloud::SystemClock),
    )
    .await;
    let provider = provider.lock().unwrap().clone().unwrap();

    // 请求以 any + 同意进来，那一刻熔断是关的（快照 = 关）；本机加载 `small` 的 400 ms 里熔断被打开；本机随后失败。
    let mut options = NodeOptions::new(Scenario::RuntimeError, Some(rig.tier.clone()));
    options.shared_kill_switch = Some(switch.clone());
    options.local_load_delay = Some(Duration::from_millis(400));
    let with = LocalNode::build(options)
        .drive_and_trip_the_kill_switch_while_the_local_load_runs(&[ANY, CONSENT])
        .await;
    let mut baseline_options = NodeOptions::new(Scenario::RuntimeError, None);
    baseline_options.local_load_delay = Some(Duration::from_millis(400));
    let without = LocalNode::build(baseline_options)
        .drive_and_trip_the_kill_switch_while_the_local_load_runs(&[ANY, CONSENT])
        .await;

    assert_ne!(
        without.status,
        200,
        "the scenario must fail locally: {}",
        without.text()
    );
    assert_eq!(
        with.fingerprint(),
        without.fingerprint(),
        "the caller must get exactly what it gets without a cloud tier\nwith:    {}\nwithout: {}",
        with.text(),
        without.text()
    );
    assert!(with.header("x-idoris-cloud-attempt").is_none());
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "the cloud received a request although the kill switch was on before the send"
    );
    assert_eq!(
        provider.loads(),
        0,
        "the gate must refuse on the live switch, before the credential is even read"
    );
    assert_eq!(rig.tier.store().call_count().unwrap(), 0);
}

#[tokio::test]
async fn a_kill_switch_turned_on_after_the_gate_but_before_the_send_leaves_no_trace_and_no_request()
{
    let switch = Arc::new(AtomicBool::new(false));
    let provider: Arc<std::sync::Mutex<Option<Arc<ProbeProvider>>>> = Default::default();
    let rig = rig_hooked(
        {
            let provider = provider.clone();
            let switch = switch.clone();
            move |inner| {
                // `load()` 是闸门通过之后的第一步：在这里把熔断打开
                let tripping = ProbeProvider::tripping(inner, switch);
                *provider.lock().unwrap() = Some(tripping.clone());
                tripping
            }
        },
        Arc::new(idoris_router::cloud::SystemClock),
    )
    .await;
    let provider = provider.lock().unwrap().clone().unwrap();

    let mut options = NodeOptions::new(Scenario::RuntimeOffline, Some(rig.tier.clone()));
    options.shared_kill_switch = Some(switch.clone());
    options.with_event_log = true;
    let node = LocalNode::build(options);
    let with = node.drive(&[ANY, CONSENT]).await;
    let without = baseline(Scenario::RuntimeOffline)
        .drive(&[ANY, CONSENT])
        .await;

    assert_eq!(with.status, 503, "{}", with.text());
    assert_eq!(
        with.fingerprint(),
        without.fingerprint(),
        "nothing left the machine, so not even the additive attempt header may appear"
    );
    assert_eq!(provider.loads(), 1, "the request must have got this far");
    assert!(
        switch.load(std::sync::atomic::Ordering::SeqCst),
        "the hook must have tripped the switch"
    );
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "the cloud received a request although the kill switch was on before the send"
    );
    // 没有任何副作用：没有 pending 行，没有 dispatched 事件（没有一个有头无尾的出网尝试）
    assert_eq!(
        rig.tier.store().call_count().unwrap(),
        0,
        "a call that never left must not leave a row"
    );
    let local_id = with.header("x-idoris-record-id").unwrap().to_string();
    let events = node
        .event_log
        .as_ref()
        .unwrap()
        .events_for_record(Some("personal"), &local_id)
        .unwrap();
    assert!(
        events
            .iter()
            .all(|event| event.event.event_type.as_str() != "dispatched"),
        "a dispatched event was written for a call that never left"
    );
}

#[tokio::test]
async fn a_kill_switch_turned_on_while_the_pending_row_is_written_still_stops_the_send() {
    let switch = Arc::new(AtomicBool::new(false));
    // 落 pending 行（`record_begin`）是出网之前最后几步之一，它第一次读时钟：在这里把熔断打开
    let clock = TrippingClock::new(switch.clone());
    let rig = rig_hooked(|provider| provider, clock.clone()).await;

    let mut options = NodeOptions::new(Scenario::RuntimeOffline, Some(rig.tier.clone()));
    options.shared_kill_switch = Some(switch.clone());
    options.with_event_log = true;
    let node = LocalNode::build(options);
    let with = node.drive(&[ANY, CONSENT]).await;
    let without = baseline(Scenario::RuntimeOffline)
        .drive(&[ANY, CONSENT])
        .await;

    assert_eq!(with.status, 503, "{}", with.text());
    assert_eq!(
        with.fingerprint(),
        without.fingerprint(),
        "nothing left the machine, so not even the additive attempt header may appear"
    );
    assert!(clock.calls() >= 1, "the request must have got this far");
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "the cloud received a request although the kill switch was on before the send"
    );
    // 行已经落下了（先写后出网）：它必须有终态，不能一直 pending；没有出网、没有答复、没有云端 id、没有金额
    let rows = rig.tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, CallState::Final(CallOutcome::Cancelled));
    assert_eq!(
        (
            rows[0].cloud_record_id.clone(),
            rows[0].http_status,
            rows[0].tokens_in,
            rows[0].tokens_out
        ),
        (None, None, None, None)
    );
    // 事件流不留一个有头无尾的出网尝试：dispatched 写了，就补一个失败的 completed
    let events = node
        .event_log
        .as_ref()
        .unwrap()
        .events_for_record(Some("personal"), &rows[0].local_request_id)
        .unwrap();
    let completed = events
        .iter()
        .find(|event| event.event.event_type.as_str() == "completed")
        .expect("a dispatched event needs its completed event");
    assert_eq!(completed.event.metadata["status"], "failure");
}

#[tokio::test]
async fn streaming_requests_are_never_sent_to_the_cloud() {
    let rig = rig().await;
    let body = json!({"model": "small", "stream": true,
                      "messages": [{"role": "user", "content": "hi"}]});
    let with = send_chat(
        &rig.node(Scenario::RuntimeOffline).app,
        body.clone(),
        &[ANY, CONSENT],
    )
    .await;
    let without = send_chat(
        &baseline(Scenario::RuntimeOffline).app,
        body,
        &[ANY, CONSENT],
    )
    .await;
    assert_eq!(with.fingerprint(), without.fingerprint());
    assert_eq!(rig.cloud.log.count(), 0);

    // 回落挂钩有三个（无候选、选路被拒、后端失败），前两个在 Supervisor 的 stream 校验之前：
    // 每个本会回落的场景都带上 stream=true 再跑一遍，必须和没有云端档时逐字节相同、云端收到 0 个请求。
    for scenario in Scenario::FAILING {
        if !scenario.goes_to_the_cloud() {
            continue;
        }
        let with = rig.node(scenario).drive_with(&[ANY, CONSENT], true).await;
        let without = baseline(scenario).drive_with(&[ANY, CONSENT], true).await;
        assert_eq!(
            with.fingerprint(),
            without.fingerprint(),
            "{scenario:?}: a streaming request must stay exactly the local answer"
        );
        assert_eq!(
            rig.cloud.log.count(),
            0,
            "{scenario:?}: streamed to the cloud"
        );
    }
}

#[tokio::test]
async fn the_routing_policy_still_decides_whether_remote_is_allowed() {
    let rig = rig().await;
    // 仓库自带的策略：只有 complexity: complex 才允许 remote。简单请求即使有同意也不出网。
    let mut with_options = NodeOptions::new(Scenario::RuntimeOffline, Some(rig.tier.clone()));
    with_options.policy = POLICY_SHIPPED;
    let with = LocalNode::build(with_options).drive(&[ANY, CONSENT]).await;
    let mut base_options = NodeOptions::new(Scenario::RuntimeOffline, None);
    base_options.policy = POLICY_SHIPPED;
    let without = LocalNode::build(base_options).drive(&[ANY, CONSENT]).await;
    assert_eq!(with.status, 503);
    assert_eq!(with.fingerprint(), without.fingerprint());
    assert_eq!(
        rig.cloud.log.count(),
        0,
        "the shipped policy keeps simple requests local"
    );
    // complex：策略允许 remote，云端档才被问到
    let mut complex_options = NodeOptions::new(Scenario::RuntimeOffline, Some(rig.tier.clone()));
    complex_options.policy = POLICY_SHIPPED;
    let served = LocalNode::build(complex_options)
        .drive(&[ANY, CONSENT, ("x-idoris-complexity", "complex")])
        .await;
    assert_eq!(served.status, 200, "{}", served.text());
    assert_eq!(rig.cloud.log.chats().len(), 1);
}

#[tokio::test]
async fn a_policy_action_that_says_fail_closed_is_honoured_even_with_remote_in_its_tiers() {
    let rig = rig().await;
    let mut with_options = NodeOptions::new(Scenario::RuntimeOffline, Some(rig.tier.clone()));
    with_options.policy = POLICY_REMOTE_BUT_FAIL_CLOSED;
    let with = LocalNode::build(with_options).drive(&[ANY, CONSENT]).await;
    let mut base_options = NodeOptions::new(Scenario::RuntimeOffline, None);
    base_options.policy = POLICY_REMOTE_BUT_FAIL_CLOSED;
    let without = LocalNode::build(base_options).drive(&[ANY, CONSENT]).await;
    assert_eq!(with.fingerprint(), without.fingerprint());
    assert_eq!(rig.cloud.log.count(), 0);
}

#[tokio::test]
async fn a_role_the_cloud_has_no_model_for_is_not_guessed() {
    let cloud = TcpCloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier");
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    pair_device(&cloud, &state_dir, &code).await.unwrap();
    cloud.log.clear();
    // 只映射了 deep：请求 idoris/daily 没有云端模型名 = 没有云端档
    let yaml = format!(
        "schema_version: 1\nbase_url: {}\nrole_models: {{deep: idoris/agent}}\nstate_dir: {}\n",
        cloud.base_url(),
        state_dir.display()
    );
    let tier = make_tier(
        parse_tier_config(&yaml),
        paired_provider(&cloud, &state_dir),
        &dir.path().join("tier.sqlite3"),
    );
    let with = LocalNode::build(NodeOptions::new(Scenario::RoleUnbound, Some(tier)))
        .drive(&[ANY, CONSENT])
        .await;
    let without = baseline(Scenario::RoleUnbound).drive(&[ANY, CONSENT]).await;
    assert_eq!(with.fingerprint(), without.fingerprint());
    assert_eq!(cloud.log.count(), 0);
    // 没有声明角色的请求（具体模型名）也没有默认模型名：同样不回落
    let with = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(make_tier(
            parse_tier_config(&yaml),
            paired_provider(&cloud, &state_dir),
            &dir.path().join("tier2.sqlite3"),
        )),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    assert_eq!(
        with.fingerprint(),
        baseline(Scenario::RuntimeOffline)
            .drive(&[ANY, CONSENT])
            .await
            .fingerprint()
    );
    assert_eq!(cloud.log.count(), 0);
}

// ================================================================================================
// 其它推理端点没有钩子
// ================================================================================================

#[tokio::test]
async fn the_other_inference_endpoints_never_use_the_cloud_tier() {
    let rig = rig().await;
    for (path, body) in [
        (
            "/v1/embeddings",
            json!({"model": "small", "input": "hello cloud tier"}),
        ),
        (
            "/v1/messages",
            json!({"model": "small", "max_tokens": 16,
                                "messages": [{"role": "user", "content": "hello cloud tier"}]}),
        ),
        (
            "/v1/rerank",
            json!({"model": "small", "query": "q", "documents": ["a", "b"]}),
        ),
    ] {
        let node = rig.node(Scenario::RuntimeOffline);
        let with = send(&node.app, "POST", path, Some(body.clone()), &[ANY, CONSENT]).await;
        let without = send(
            &baseline(Scenario::RuntimeOffline).app,
            "POST",
            path,
            Some(body),
            &[ANY, CONSENT],
        )
        .await;
        assert_eq!(with.fingerprint(), without.fingerprint(), "{path}");
    }
    assert_eq!(rig.cloud.log.count(), 0);
}
