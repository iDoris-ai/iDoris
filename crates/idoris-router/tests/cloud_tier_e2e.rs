//! 云端档（计算服务 S7）端到端：本机 iDoris（进程内 `AppState` + 假 oMLX）→ 真实的 HTTP → S5 的云形态应用（真实的账户、
//! 账本、云入口，挂在 `127.0.0.1` 的 TCP 上）→ 假上游。断言三件事：调用方看到什么、云端收到什么、本机和云端各自记了什么。
//!
//! 覆盖：个人形态（真实配对出来的设备凭证）和组织形态（Node 凭证文件）都能经云端档拿到回答；对账指针；云端不可达 / 恢复 /
//! 不重放；离线时本机 QUOTA 账本纹丝不动；云端拒绝（401、429）时调用方拿到本机原来的失败；没配对、凭证有问题时不出网。
//! **不连接任何真实的外部服务，不启动任何推理进程。**
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/cloud_tier_harness.rs"]
mod h;

use std::sync::Arc;
use std::time::Duration;

use h::ingress::{ACTUAL_MICROS, IngressYaml, aid};
use h::*;
use idoris_contracts::ComponentCard;
use idoris_router::cloud_tier::{CloudTier, DeviceStore};
use idoris_tenancy::cloud_tier::{CallOutcome, CallState};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Respond, ResponseTemplate};

const CONSENT: (&str, &str) = ("x-idoris-cloud-fallback", "allow");
const ANY: (&str, &str) = ("x-idoris-privacy", "any");

struct Personal {
    cloud: TcpCloud,
    tier: Arc<CloudTier>,
    state_dir: std::path::PathBuf,
    dir: tempfile::TempDir,
}

async fn personal_with(cloud: TcpCloud) -> Personal {
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier");
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    pair_device(&cloud, &state_dir, &code).await.unwrap();
    let tier = make_tier(
        parse_tier_config(&tier_config_yaml(&cloud, Some(&state_dir), "")),
        paired_provider(&cloud, &state_dir),
        &dir.path().join("tier.sqlite3"),
    );
    cloud.log.clear();
    Personal {
        cloud,
        tier,
        state_dir,
        dir,
    }
}

async fn personal() -> Personal {
    personal_with(TcpCloud::start().await).await
}

// ================================================================================================
// 个人形态：真实配对 -> 云端档 -> 云
// ================================================================================================

#[tokio::test]
async fn a_paired_personal_node_gets_its_answer_from_the_cloud_and_records_only_a_pointer() {
    let p = personal().await;
    let mut options = NodeOptions::new(Scenario::RuntimeOffline, Some(p.tier.clone()));
    options.with_event_log = true;
    let node = LocalNode::build(options);
    let local_secret_free_prompt = "hello cloud tier";

    let response = node.drive(&[ANY, CONSENT]).await;
    assert_eq!(response.status, 200, "{}", response.text());
    // 云端的响应体原样
    assert_eq!(response.json()["choices"][0]["message"]["content"], "hello");
    assert_eq!(response.json()["usage"]["prompt_tokens"], 12);
    assert_eq!(response.header("content-type"), Some("application/json"));
    // 如实回传落点和降级（G-6）
    assert_eq!(response.header("x-idoris-served-locality"), Some("remote"));
    assert_eq!(response.header("x-idoris-degraded"), Some("true"));
    assert!(
        response
            .header("x-idoris-reason")
            .unwrap()
            .ends_with("CloudTierFallback")
    );
    // 本机不记云端成本：没有 cost 头
    assert!(response.header("x-idoris-cost-minor").is_none());
    // X-iDoris-Record-Id 仍然是本机的；云端的 id 在新增的可选头里
    let local_id = response.header("x-idoris-record-id").unwrap().to_string();
    let cloud_id = response
        .header("x-idoris-cloud-record-id")
        .unwrap()
        .to_string();
    assert_ne!(local_id, cloud_id);
    assert_eq!(
        response.header("x-idoris-cloud-service-tier"),
        Some("normal")
    );
    assert!(response.header("x-idoris-cloud-attempt").is_none());

    // 云端：一个请求、付款人是这台设备绑定的成员账户 a1、凭证是配对出来的那张、没有成员头
    assert_eq!(p.cloud.log.chats().len(), 1);
    assert!(!p.cloud.log.chats()[0].has_member_header);
    let device = DeviceStore::new(&p.state_dir).read().unwrap().unwrap();
    let rows = p.cloud.world.request_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "a1");
    assert_eq!(rows[0].1, device.credential_id);
    assert_eq!(rows[0].2, "adult");
    assert_eq!(rows[0].3, "paid");
    assert_eq!(p.cloud.world.spent("a1"), ACTUAL_MICROS);
    // 云端自己的 request id 就是它回的 record id（对账指针对得上云端的账）
    assert!(p.cloud.world.request_rows().len() == 1);

    // 本机：一行对账指针，没有金额，没有正文
    let row = p.tier.store().call_by_cloud_id(&cloud_id).unwrap().unwrap();
    assert_eq!(row.local_request_id, local_id);
    assert_eq!(row.state, CallState::Final(CallOutcome::Served));
    assert_eq!(row.http_status, Some(200));
    assert_eq!((row.tokens_in, row.tokens_out), (Some(12), Some(30)));
    assert_eq!(row.attempt_no, 1);
    assert_eq!(p.tier.store().call_count().unwrap(), 1);

    // 事件日志：本机的决策、出网和完成都有记录（provider 是云端档，落点 remote），没有正文
    let events = node
        .event_log
        .as_ref()
        .unwrap()
        .events_for_record(Some("personal"), &local_id)
        .unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.event.event_type.as_str()).collect();
    for needed in [
        "request.received",
        "decided",
        "dispatched",
        "completed",
        "audit.finalized",
    ] {
        assert!(kinds.contains(&needed), "{needed} missing in {kinds:?}");
    }
    let dispatched = events
        .iter()
        .find(|e| e.event.event_type.as_str() == "dispatched")
        .unwrap();
    assert_eq!(dispatched.event.metadata["provider_id"], "idoris-cloud");
    assert_eq!(dispatched.event.metadata["served_locality"], "remote");
    let completed = events
        .iter()
        .find(|e| e.event.event_type.as_str() == "completed")
        .unwrap();
    assert_eq!(completed.event.metadata["status"], "success");
    assert_eq!(completed.event.metadata["provider_id"], "idoris-cloud");

    // 磁盘上没有正文：调用记录库、事件库的全部文件里都找不到 prompt
    for dir in [p.dir.path(), node.dir.path()] {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                assert!(
                    !String::from_utf8_lossy(&bytes).contains(local_secret_free_prompt),
                    "prompt text found on disk in {}",
                    path.display()
                );
            }
        }
    }
}

#[tokio::test]
async fn every_cloud_served_request_gets_its_own_record_and_the_cursor_walks_forward() {
    let p = personal().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        let node = LocalNode::build(NodeOptions::new(
            Scenario::RuntimeOffline,
            Some(p.tier.clone()),
        ));
        let response = node.drive(&[ANY, CONSENT]).await;
        assert_eq!(response.status, 200, "{}", response.text());
        ids.push((
            response.header("x-idoris-record-id").unwrap().to_string(),
            response
                .header("x-idoris-cloud-record-id")
                .unwrap()
                .to_string(),
        ));
    }
    let store = p.tier.store();
    let rows = store.calls_after(0, 10).unwrap();
    assert_eq!(rows.len(), 3);
    for (row, (local, cloud)) in rows.iter().zip(&ids) {
        assert_eq!(&row.local_request_id, local);
        assert_eq!(row.cloud_record_id.as_ref(), Some(cloud));
    }
    assert!(rows.windows(2).all(|pair| pair[0].seq < pair[1].seq));
    // 对账游标：只增不减、幂等
    assert_eq!(
        store.advance_cursor("reconciler", rows[1].seq, 1).unwrap(),
        rows[1].seq
    );
    assert_eq!(
        store.advance_cursor("reconciler", rows[1].seq, 2).unwrap(),
        rows[1].seq
    );
    assert_eq!(
        store.advance_cursor("reconciler", rows[0].seq, 3).unwrap(),
        rows[1].seq
    );
    assert_eq!(
        store
            .calls_after(store.cursor("reconciler").unwrap(), 10)
            .unwrap()
            .len(),
        1
    );
    // 云端那边也恰好是三笔账，云端的 request id 和本机记的对得上（对账指针指向真实存在的云端请求）
    assert_eq!(p.cloud.world.request_count(), 3);
    assert_eq!(p.cloud.world.spent("a1"), 3 * ACTUAL_MICROS);
}

// ================================================================================================
// 组织形态：Node 凭证
// ================================================================================================

/// 组织的本机节点：租户模式（`X-iDoris-Tenant` + 绑定了租户的虚拟 key）、本机 QUOTA 账本（租户 acme）、没有在线的本机运行时。
struct OrgNode {
    app: axum::Router,
    ledger: Arc<idoris_tenancy::budget::BudgetLedger>,
    bearer: String,
    _dir: tempfile::TempDir,
}

fn org_node_with(tier: Option<Arc<CloudTier>>) -> OrgNode {
    use idoris_tenancy::virtual_key::MintedVirtualKey;
    use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
    let dir = tempfile::tempdir().unwrap();
    let ledger = Arc::new(
        idoris_tenancy::budget::BudgetLedger::open(dir.path().join("quota.sqlite3")).unwrap(),
    );
    ledger
        .configure_tenant(
            "acme",
            1_000_000,
            "UTC",
            idoris_tenancy::budget::SpendGate::PaidOnly,
        )
        .unwrap();
    let mut card: ComponentCard =
        serde_yaml::from_str(include_str!("../../../config/components/omlx.yaml")).unwrap();
    // 本机模型有价（QUOTA 单位）：本机的请求会预留、结算 QUOTA；云端档不读写它
    card.provider.cost = idoris_contracts::provider::Cost {
        input_per_m: 1_000.0,
        output_per_m: 2_000.0,
    };
    // 绑定了租户 acme 的数据 key，隐私上限含 any
    let key = MintedVirtualKey::mint();
    let keys = Arc::new(std::sync::Mutex::new(
        VirtualKeyStore::new(rusqlite::Connection::open_in_memory().unwrap()).unwrap(),
    ));
    keys.lock()
        .unwrap()
        .insert_active(
            &key.key_id,
            key.hash,
            &VirtualKeyScope {
                owner: "acme-member".into(),
                tenant_id: Some("acme".into()),
                allowed_privacy: vec![
                    idoris_contracts::common::PrivacyClass::LocalOnly,
                    idoris_contracts::common::PrivacyClass::Any,
                ],
                allowed_roles: [
                    "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
                ]
                .map(String::from)
                .to_vec(),
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    let state = idoris_router::AppState {
        deploy_mode: idoris_contracts::DeployMode::Tenant,
        cards: vec![card],
        routing_policy: policy(POLICY_ALLOWING_REMOTE),
        budget_ledger: Some(ledger.clone()),
        virtual_key_authenticator: Some(idoris_router::auth::VirtualKeyAuthenticator::new(keys)),
        cloud_tier: tier,
        ..idoris_router::AppState::default()
    };
    OrgNode {
        app: idoris_router::build_app(state),
        ledger,
        bearer: format!("Bearer {}", key.secret.expose_secret()),
        _dir: dir,
    }
}

fn org_node(tier: Arc<CloudTier>) -> OrgNode {
    org_node_with(Some(tier))
}

/// 没有云端档的组织节点（对照）。
fn org_node_plain() -> OrgNode {
    org_node_with(None)
}

impl OrgNode {
    async fn chat(&self, extra: &[(&str, &str)]) -> Captured {
        let mut headers = vec![
            ("authorization", self.bearer.as_str()),
            ("x-idoris-tenant", "acme"),
        ];
        headers.extend_from_slice(extra);
        send_chat(&self.app, chat_body("small"), &headers).await
    }
}

fn quota(ledger: &idoris_tenancy::budget::BudgetLedger) -> (i64, i64, i64) {
    let view = ledger.tenant_readview("acme").unwrap();
    (view.limit_minor, view.spent_minor, view.reserved_minor)
}

#[tokio::test]
async fn an_organization_node_uses_its_node_credential_and_never_touches_its_own_quota_for_cloud_calls()
 {
    let cloud = TcpCloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let node_secret = cloud.world.cred("node_agent24").secret.clone();
    let node_id = cloud.world.cred("node_agent24").id.clone();
    let file = node_file(dir.path(), &node_secret);
    let tier = make_tier(
        parse_tier_config(&tier_config_yaml(
            &cloud,
            None,
            &format!(
                "credential: {{source: node_file, file: {}}}",
                file.display()
            ),
        )),
        Arc::new(idoris_router::cloud_tier::credential::NodeFileProvider::new(&file, None)),
        &dir.path().join("tier.sqlite3"),
    );
    let node = org_node(tier.clone());
    assert_eq!(quota(&node.ledger), (1_000_000, 0, 0));

    let response = node.chat(&[ANY, CONSENT]).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(response.header("x-idoris-served-locality"), Some("remote"));

    // 云端只认组织付款账户：付款人是 agent24 组织自己，凭证是 Node 凭证，没有成员头
    let rows = cloud.world.request_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "agent24");
    assert_eq!(rows[0].1, node_id);
    assert_eq!(cloud.world.spent("agent24"), ACTUAL_MICROS);
    assert!(!cloud.log.chats()[0].has_member_header);
    // 本机 QUOTA：没有读，没有写——没有结算，没有残留的预留
    assert_eq!(quota(&node.ledger), (1_000_000, 0, 0));
    assert!(response.header("x-idoris-cost-minor").is_none());
    // 对账指针在本机，用量是 token 当量
    let row = tier
        .store()
        .call_by_cloud_id(response.header("x-idoris-cloud-record-id").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CallState::Final(CallOutcome::Served));
    assert_eq!((row.tokens_in, row.tokens_out), (Some(12), Some(30)));
}

// ================================================================================================
// 离线：不记账、不加额度、不补发、不重放
// ================================================================================================

#[tokio::test]
async fn while_the_cloud_is_unreachable_the_node_consumes_only_its_own_quota_and_nothing_is_replayed_afterwards()
 {
    let mut cloud = TcpCloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let node_secret = cloud.world.cred("node_agent24").secret.clone();
    let file = node_file(dir.path(), &node_secret);
    let tier = make_tier(
        parse_tier_config(&tier_config_yaml(
            &cloud,
            None,
            &format!(
                "credential: {{source: node_file, file: {}}}",
                file.display()
            ),
        )),
        Arc::new(idoris_router::cloud_tier::credential::NodeFileProvider::new(&file, None)),
        &dir.path().join("tier.sqlite3"),
    );
    let node = org_node(tier.clone());
    let headers = [ANY, CONSENT];

    cloud.stop().await;

    // 云端不可达：三个请求，每个都拿到本机原来的失败（只多一个 X-iDoris-Cloud-Attempt 头），QUOTA 纹丝不动
    let mut failures = Vec::new();
    for _ in 0..3 {
        let response = node.chat(&headers).await;
        assert_eq!(response.status, 503, "{}", response.text());
        assert_eq!(response.json()["error"]["type"], "local_only_unavailable");
        assert_eq!(
            response.header("x-idoris-cloud-attempt"),
            Some("unreachable")
        );
        failures.push(response);
        assert_eq!(
            quota(&node.ledger),
            (1_000_000, 0, 0),
            "offline must not touch the local QUOTA"
        );
    }
    // 调用方拿到的和「没有云端档」时一样（扣掉新增的那个可选头）
    let plain = org_node_plain();
    let plain_response = plain.chat(&headers).await;
    for response in &failures {
        let mut with = response.fingerprint();
        with.1.retain(|(name, _)| name != "x-idoris-cloud-attempt");
        assert_eq!(with, plain_response.fingerprint());
    }
    // 记录：三行 unreachable，没有云端 id，没有任何金额 / 用量
    let rows = tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows.len(), 3);
    for row in &rows {
        assert_eq!(row.state, CallState::Final(CallOutcome::Unreachable));
        assert_eq!(row.cloud_record_id, None);
        assert_eq!(
            (row.tokens_in, row.tokens_out, row.http_status),
            (None, None, None)
        );
    }
    // 云端那头的账：一分没动
    assert_eq!(cloud.world.spent("agent24"), 0);

    // 云端恢复：离线期间的请求**不会**被补发或重放
    cloud.restart().await;
    cloud.log.clear();
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert_eq!(
        cloud.log.count(),
        0,
        "requests from the offline period were replayed"
    );
    assert_eq!(cloud.world.request_count(), 0);

    // 新的请求照常走云端，恰好一个
    let served = node.chat(&headers).await;
    assert_eq!(served.status, 200, "{}", served.text());
    assert_eq!(cloud.log.chats().len(), 1);
    assert_eq!(cloud.world.request_count(), 1);
    let rows = tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[3].state, CallState::Final(CallOutcome::Served));
    assert_eq!(quota(&node.ledger), (1_000_000, 0, 0));
}

// ================================================================================================
// 云端拒绝：调用方拿到本机原来的失败，只多一个可选头
// ================================================================================================

#[tokio::test]
async fn a_revoked_device_credential_is_refused_by_the_cloud_and_the_caller_keeps_the_local_failure()
 {
    let p = personal().await;
    let device = DeviceStore::new(&p.state_dir).read().unwrap().unwrap();
    p.cloud.operator_revoke_credential(&device.credential_id);

    let with = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(p.tier.clone()),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    let without = LocalNode::build(NodeOptions::new(Scenario::RuntimeOffline, None))
        .drive(&[ANY, CONSENT])
        .await;
    assert_eq!(with.status, 503);
    assert_eq!(with.header("x-idoris-cloud-attempt"), Some("unauthorized"));
    let mut fingerprint = with.fingerprint();
    fingerprint
        .1
        .retain(|(name, _)| name != "x-idoris-cloud-attempt");
    assert_eq!(fingerprint, without.fingerprint());
    // 云端收到了请求（401），本机记下这次出网、云端的状态码，没有金额
    assert_eq!(p.cloud.log.chats().len(), 1);
    let rows = p.tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].state, CallState::Final(CallOutcome::Unauthorized));
    assert_eq!(rows[0].http_status, Some(401));
    assert_eq!((rows[0].tokens_in, rows[0].tokens_out), (None, None));
    assert_eq!(
        p.cloud.world.request_count(),
        0,
        "a refused credential creates no cloud request"
    );
    // 凭证文件还在（是不是撤销了由用户 `idoris cloud unpair` / 重新配对决定，不在请求路径上自动删）
    assert!(DeviceStore::new(&p.state_dir).read().unwrap().is_some());
}

#[tokio::test]
async fn a_cloud_rate_limit_is_reported_with_its_stable_reason_and_changes_nothing_else() {
    let ingress = IngressYaml::default().limits(
        "{requests_per_minute: 600000, burst: 100000, max_concurrent: 10000}",
        "{requests_per_minute: 600000, burst: 100000, max_concurrent: 10000}",
        "{requests_per_minute: 1, burst: 1, max_concurrent: 10}",
    );
    let cloud = TcpCloud::start_with(WorldOptions::plenty().ingress(ingress)).await;
    let p = personal_with(cloud).await;
    let node = |tier: &Arc<CloudTier>| {
        LocalNode::build(NodeOptions::new(
            Scenario::RuntimeOffline,
            Some(tier.clone()),
        ))
    };
    let first = node(&p.tier).drive(&[ANY, CONSENT]).await;
    assert_eq!(first.status, 200, "{}", first.text());
    let second = node(&p.tier).drive(&[ANY, CONSENT]).await;
    assert_eq!(second.status, 503);
    assert_eq!(
        second.header("x-idoris-cloud-attempt"),
        Some("rate_limited")
    );
    let rows = p.tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows[1].state, CallState::Final(CallOutcome::RateLimited));
    assert_eq!(rows[1].http_status, Some(429));
    assert_eq!(
        rows[1].cloud_reason.as_deref(),
        Some("request_rate_limited")
    );
}

// ================================================================================================
// 没配对、凭证有问题：不出网，如实告诉调用方「本该回落却没能回落」
// ================================================================================================

#[tokio::test]
async fn an_unpaired_node_does_not_send_anything_and_says_why() {
    let cloud = TcpCloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier"); // 没有设备凭证文件
    let tier = make_tier(
        parse_tier_config(&tier_config_yaml(&cloud, Some(&state_dir), "")),
        paired_provider(&cloud, &state_dir),
        &dir.path().join("tier.sqlite3"),
    );
    let with = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(tier.clone()),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    let without = LocalNode::build(NodeOptions::new(Scenario::RuntimeOffline, None))
        .drive(&[ANY, CONSENT])
        .await;
    assert_eq!(with.header("x-idoris-cloud-attempt"), Some("not_paired"));
    let mut fingerprint = with.fingerprint();
    fingerprint
        .1
        .retain(|(name, _)| name != "x-idoris-cloud-attempt");
    assert_eq!(fingerprint, without.fingerprint());
    assert_eq!(cloud.log.count(), 0);
    assert_eq!(
        tier.store().call_count().unwrap(),
        0,
        "nothing was attempted, so nothing is recorded"
    );
}

#[tokio::test]
async fn a_credential_that_is_not_safe_to_use_is_never_sent() {
    use std::os::unix::fs::PermissionsExt;
    let p = personal().await;
    let device_file = DeviceStore::new(&p.state_dir).path();
    let attempt = |tier: &Arc<CloudTier>| {
        let tier = tier.clone();
        async move {
            LocalNode::build(NodeOptions::new(Scenario::RuntimeOffline, Some(tier)))
                .drive(&[ANY, CONSENT])
                .await
        }
    };
    // 其他用户也能读的凭证文件
    std::fs::set_permissions(&device_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    let response = attempt(&p.tier).await;
    assert_eq!(
        response.header("x-idoris-cloud-attempt"),
        Some("credential_unavailable")
    );
    std::fs::set_permissions(&device_file, std::fs::Permissions::from_mode(0o600)).unwrap();
    // 配置里的云端换了地址：凭证只发往签发它的那个源
    let other = make_tier(
        parse_tier_config(&format!(
            "schema_version: 1\nbase_url: http://localhost:{}/api\nmodel: idoris/agent\nstate_dir: {}\n",
            p.cloud.addr.port(),
            p.state_dir.display()
        )),
        Arc::new(
            idoris_router::cloud_tier::credential::PairedDeviceProvider::new(
                DeviceStore::new(&p.state_dir),
                format!("http://localhost:{}", p.cloud.addr.port()),
                None,
            ),
        ),
        &p.dir.path().join("other.sqlite3"),
    );
    let response = attempt(&other).await;
    assert_eq!(
        response.header("x-idoris-cloud-attempt"),
        Some("credential_unavailable")
    );
    assert_eq!(
        p.cloud.log.count(),
        0,
        "an unsafe credential must never be sent anywhere"
    );
    // 文件恢复正常之后就能用
    let response = attempt(&p.tier).await;
    assert_eq!(response.status, 200, "{}", response.text());
    assert_eq!(p.cloud.log.chats().len(), 1);
}

#[tokio::test]
async fn unpairing_takes_effect_on_the_very_next_request_without_a_restart() {
    let p = personal().await;
    let first = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(p.tier.clone()),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    assert_eq!(first.status, 200);
    // 设备凭证文件被删（`idoris cloud unpair` 做的）：正在运行的节点下一个请求就不再用云端档
    DeviceStore::new(&p.state_dir).remove().unwrap();
    let second = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(p.tier.clone()),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    assert_eq!(second.status, 503);
    assert_eq!(second.header("x-idoris-cloud-attempt"), Some("not_paired"));
    assert_eq!(
        p.cloud.log.chats().len(),
        1,
        "no request after the credential was removed"
    );
}

// ================================================================================================
// 云端档的容量上限
// ================================================================================================

#[tokio::test]
async fn the_cloud_tier_limits_how_many_calls_are_in_flight_and_does_not_queue() {
    let cloud = TcpCloud::start_with(WorldOptions::with_script(
        (0..10)
            .map(|_| {
                h::ingress::Behavior::slow_json(
                    200,
                    h::ingress::completion("ok", "slow", Some(h::ingress::usage(12, 30))),
                    Duration::from_millis(400),
                )
            })
            .collect(),
    ))
    .await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier");
    let admin = cloud.issue_agent24_admin();
    let code = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    pair_device(&cloud, &state_dir, &code).await.unwrap();
    let yaml = tier_config_yaml(&cloud, Some(&state_dir), "").replace(
        "limits: {connect_timeout_ms: 1000, total_timeout_ms: 20000}",
        "limits: {connect_timeout_ms: 1000, total_timeout_ms: 20000, max_concurrent: 1}",
    );
    let tier = make_tier(
        parse_tier_config(&yaml),
        paired_provider(&cloud, &state_dir),
        &dir.path().join("tier.sqlite3"),
    );
    let node = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(tier.clone()),
    ));
    let app = node.app.clone();
    let slow =
        tokio::spawn(async move { send_chat(&app, chat_body("small"), &[ANY, CONSENT]).await });
    // 等第一个真的占住名额
    for _ in 0..100 {
        if cloud.log.chats().len() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let second = send_chat(&node.app, chat_body("small"), &[ANY, CONSENT]).await;
    assert_eq!(second.status, 503);
    assert_eq!(second.header("x-idoris-cloud-attempt"), Some("saturated"));
    assert_eq!(slow.await.unwrap().status, 200);
    assert_eq!(
        cloud.log.chats().len(),
        1,
        "the saturated call must not have been queued or sent"
    );
    let _ = aid("a1");
    let _ = json!({});
}

// ================================================================================================
// 配对：同一台机器上的两个配对不能互相覆盖（审查 r4 Medium#4）
// ================================================================================================

/// 两个配对同时开始（单线程运行时：A 在等兑换响应的时候 B 开始）。以前「尚未配对」的检查和最终的 `rename` 之间没有任何锁，
/// 而 `rename` 会覆盖已有文件：两个进程各兑换一个账号的配对码、都先读到文件不存在，A 写入之后 B 覆盖它，两个都报告成功——
/// A 的云凭证仍然有效、但本机已经丢了它（孤儿凭证），之后的请求用的是 B 的付款上下文。
#[tokio::test]
async fn two_pairings_racing_on_one_machine_cannot_both_redeem_and_the_loser_does_not_burn_its_code()
 {
    let cloud = TcpCloud::start().await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier");
    let admin = cloud.issue_agent24_admin();
    let code_a = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    let code_b = cloud.mint_pairing_code(&admin, "m-agent-1", 600).await;
    cloud.log.clear();

    let (a, b) = tokio::join!(
        pair_device(&cloud, &state_dir, &code_a),
        pair_device(&cloud, &state_dir, &code_b)
    );

    let redeems = cloud
        .log
        .all()
        .iter()
        .filter(|seen| seen.path.ends_with("/pairing/redeem"))
        .count();
    assert_eq!(
        redeems, 1,
        "the second pairing redeemed its one-time code although this machine was being paired: {a:?} / {b:?}"
    );
    let winners = [&a, &b].iter().filter(|outcome| outcome.is_ok()).count();
    assert_eq!(winners, 1, "exactly one pairing may succeed: {a:?} / {b:?}");
    let loser = [&a, &b]
        .into_iter()
        .find_map(|outcome| outcome.as_ref().err())
        .unwrap();
    assert!(
        loser.contains("已经配对") || loser.contains("正在运行"),
        "{loser}"
    );
    // 文件里是赢家的凭证，而且它在云端有效（没有被覆盖、没有被撤销）
    let device = DeviceStore::new(&state_dir).read().unwrap().unwrap();
    let reply = cloud
        .world
        .send(&Req::chat(&Cred {
            id: String::new(),
            secret: device.secret.expose_secret().to_string(),
        }))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
}

fn control_plane_secret(seed: char) -> String {
    format!("idk_{}", seed.to_string().repeat(64))
}

/// 每次兑换发一张不同的凭证（seed 依次是 a、b、c……），兑换响应慢 `delay`（给「兑换期间别的进程动了文件」留出窗口）。
struct SlowRedeem {
    delay: Duration,
    calls: std::sync::atomic::AtomicUsize,
}

impl Respond for SlowRedeem {
    fn respond(&self, _request: &wiremock::Request) -> ResponseTemplate {
        let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let seed = char::from(b'a' + u8::try_from(n).unwrap());
        ResponseTemplate::new(201)
            .set_delay(self.delay)
            .set_body_json(json!({
                "result": "created", "reason_code": "ok", "request_id": "r",
                "data": {
                    "credential": {"id": format!("vk_new_{n}"), "binding": {"kind": "member", "membership": "m-new"}, "status": "active"},
                    "secret": control_plane_secret(seed), "membership": "m-new", "pairing_id": format!("pc_new_{n}")
                }
            }))
    }
}

async fn run_pair_against(
    server: &MockServer,
    state_dir: &std::path::Path,
    code: &str,
) -> Result<u8, String> {
    let config = parse_tier_config(&tier_config_for(
        &format!("{}/api", server.uri()),
        Some(state_dir),
        "",
        "",
    ));
    let mut stdin = std::io::Cursor::new(format!("{code}\n").into_bytes());
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    idoris_router::cloud_tier::cli::run_pair(
        idoris_router::cloud_tier::cli::CliContext {
            config: &config,
            db_path: &state_dir.join("idoris.sqlite3"),
            http: idoris_upstream::http_client().unwrap(),
            now_ms: 1_700_000_000_000,
            probe: &NoLocalKeys,
        },
        None,
        &mut stdin,
        &mut stdout,
        &mut stderr,
    )
    .await
}

/// 就算有进程不遵守锁（旧版本、手工放文件），安装也不覆盖已有的凭证：兑换成功之后发现装不上，就用手里这张新凭证把云端那张撤销，
/// 免得它成了没人持有的孤儿；已有的那张原样不动。
#[tokio::test]
async fn an_install_never_overwrites_a_credential_that_appeared_meanwhile_and_the_new_one_is_revoked()
 {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/control/v1/pairing/redeem"))
        .respond_with(SlowRedeem {
            delay: Duration::from_millis(700),
            calls: std::sync::atomic::AtomicUsize::new(0),
        })
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/control/v1/pairing/unpair"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"data": {"credential": {"status": "revoked"}}})),
        )
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let state_dir = dir.path().join("cloud-tier");
    let origin = parse_tier_config(&tier_config_for(
        &format!("{}/api", server.uri()),
        Some(&state_dir),
        "",
        "",
    ))
    .origin();

    let state = state_dir.clone();
    let outcome = {
        let racer = async {
            // 等兑换请求到达云端（此时配对流程正在等慢吞吞的兑换响应），然后另一个「进程」把一张凭证放进了状态目录
            for _ in 0..200 {
                let seen = server.received_requests().await.unwrap();
                if seen
                    .iter()
                    .any(|r| r.url.path().ends_with("/pairing/redeem"))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let file = state.join("device.json");
            std::fs::write(
                &file,
                json!({
                    "version": 1, "origin": origin, "credential_id": "vk_interloper",
                    "membership": "m-other", "pairing_id": "pc_interloper",
                    "paired_at_ms": 1, "secret": control_plane_secret('f'),
                })
                .to_string(),
            )
            .unwrap();
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        };
        let (outcome, ()) = tokio::join!(
            run_pair_against(&server, &state_dir, "ABCD-EFGH-JKMN"),
            racer
        );
        outcome
    };

    let message = outcome.expect_err(
        "the pairing reported success although another credential appeared during the redeem",
    );
    assert!(message.contains("没有覆盖"), "{message}");
    // 已有的那张原样不动
    assert_eq!(
        DeviceStore::new(&state_dir)
            .read()
            .unwrap()
            .unwrap()
            .secret
            .expose_secret(),
        control_plane_secret('f')
    );
    // 新兑换的那张被撤销了：恰好一次 unpair，带的是新凭证（seed a），不是已有的那张
    let unpairs: Vec<_> = server
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|r| r.url.path().ends_with("/pairing/unpair"))
        .collect();
    assert_eq!(
        unpairs.len(),
        1,
        "the freshly redeemed credential must be revoked"
    );
    assert_eq!(
        unpairs[0]
            .headers
            .get("authorization")
            .unwrap()
            .to_str()
            .unwrap(),
        format!("Bearer {}", control_plane_secret('a'))
    );
    // 没有留下临时文件
    let names: Vec<String> = std::fs::read_dir(&state_dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name != ".lock")
        .collect();
    assert_eq!(names, ["device.json"], "{names:?}");
}
