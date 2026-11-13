#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use idoris_contracts::DeployMode;
use idoris_contracts::common::{PrivacyClass, Tier};
use idoris_contracts::provider::Locality;
use idoris_policy::{effective_served_locality, privacy::is_local_capable};
use idoris_tenancy::cloud_tier::CallOutcome;
use serde_json::json;

use super::*;

fn secret(seed: char) -> String {
    format!("idk_{}", seed.to_string().repeat(64))
}

fn config_with(extra: &str) -> CloudTierConfig {
    CloudTierConfig::parse(
        &format!(
            "schema_version: 1\nbase_url: https://idoris.ai/api\nmodel: idoris/agent\n{extra}"
        ),
        Path::new("."),
    )
    .unwrap()
}

// ------------------------------------------------------------------------------------------------
// 合成的组件卡：它必须是「远程、可以承载 any、不是可信本地」的，并且过得了既有的校验
// ------------------------------------------------------------------------------------------------

#[test]
fn the_synthesized_card_is_a_remote_any_card_that_can_never_pass_the_local_only_gate() {
    let tier_config = config_with("role_models: {fast: idoris/agent, daily: idoris/agent}\n");
    let card = build_card(&tier_config).unwrap();
    assert_eq!(card.provider.id, PROVIDER_ID);
    assert_eq!(card.provider.tier, Tier::Remote);
    assert_eq!(card.provider.locality, Locality::Remote);
    assert_eq!(card.privacy_class, PrivacyClass::Any);
    assert_eq!(card.provider.privacy_class, PrivacyClass::Any);
    assert_eq!(card.allowed_egress, vec![Egress::Internet]);
    // 既有的隐私闸门：它不是可信本地，所以 local_only 的路径永远选不到它
    let wrapped = idoris_policy::Card {
        component: card.clone(),
        roles: Vec::new(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: Some(0),
        admission_status: idoris_policy::AdmissionStatus::Ready,
    };
    assert!(!is_local_capable(&wrapped));
    assert_eq!(effective_served_locality(&wrapped), Locality::Remote);
    // 只接手配置里列出的角色，值是占位符（真正的云端模型名在配置里，不在卡里）
    let models = crate::dispatch::role_models(&card).unwrap();
    assert_eq!(
        models.keys().map(String::as_str).collect::<Vec<_>>(),
        ["daily", "fast"]
    );
    assert!(models.values().all(|value| value == "cloud"));
    // 成本为 0：本机预算（QUOTA）不读写云端档
    assert_eq!(card.provider.cost.input_per_m, 0.0);
    assert_eq!(card.provider.cost.output_per_m, 0.0);
}

#[test]
fn a_cloud_route_name_that_looks_like_a_role_alias_does_not_break_the_card() {
    // 云端的路由名（`idoris/agent`）长得像本机的 `idoris/<role>` 别名；如果把它放进 idoris_role_models，
    // 既有的校验会把整张卡当成坏卡丢掉。真正的云端模型名只在配置里，所以这里必须能正常构造并选出来。
    let tier_config = config_with("role_models: {daily: idoris/daily-ish}\n");
    let card = build_card(&tier_config).unwrap();
    assert!(crate::dispatch::role_models(&card).is_ok());
    assert_eq!(
        tier_config.cloud_model_for(Some("daily")),
        Some("idoris/daily-ish")
    );
}

#[test]
fn a_catalog_role_is_mapped_by_role_and_everything_else_uses_the_default_model() {
    let tier_config = config_with("role_models: {deep: idoris/deep}\n");
    assert_eq!(
        tier_config.cloud_model_for(Some("deep")),
        Some("idoris/deep")
    );
    assert_eq!(tier_config.cloud_model_for(Some("daily")), None);
    assert_eq!(tier_config.cloud_model_for(None), Some("idoris/agent"));
}

// ------------------------------------------------------------------------------------------------
// 请求体：本机的模型名不出网，身份字段被清洗
// ------------------------------------------------------------------------------------------------

fn tier_for_body_tests() -> CloudTier {
    let config = config_with("");
    let http = idoris_upstream::cloud::build_http_client(config.connect_timeout).unwrap();
    CloudTier::new(CloudTierParts {
        config,
        store: Arc::new(CloudTierStore::open_in_memory().unwrap()),
        provider: Arc::new(credential::NodeEnvProvider::new(None, None)),
        http,
        clock: Arc::new(SystemClock),
    })
    .unwrap()
}

#[test]
fn the_request_body_sent_to_the_cloud_carries_the_cloud_model_and_no_identity_fields() {
    let tier = tier_for_body_tests();
    let request = json!({
        "model": "idoris/daily",
        "user": "u-1",
        "safety_identifier": "s",
        "prompt_cache_key": "k",
        "metadata": {"a": 1},
        "messages": [{"role": "user", "name": "alice", "content": "hi"}],
        "temperature": 0.2,
        "max_tokens": 32,
    });
    let body = tier.cloud_body(&request, "idoris/agent");
    assert_eq!(body["model"], "idoris/agent");
    for dropped in ["user", "safety_identifier", "prompt_cache_key", "metadata"] {
        assert!(body.get(dropped).is_none(), "{dropped}");
    }
    assert!(body["messages"][0].get("name").is_none());
    assert_eq!(body["messages"][0]["content"], "hi");
    assert_eq!(body["temperature"], 0.2);
    assert_eq!(body["max_tokens"], 32);
    // 原请求不被改动
    assert_eq!(request["model"], "idoris/daily");
    assert_eq!(request["user"], "u-1");
}

// ------------------------------------------------------------------------------------------------
// 稳定字符串
// ------------------------------------------------------------------------------------------------

#[test]
fn the_attempt_codes_are_pinned_and_cover_every_outcome_the_caller_can_see() {
    assert_eq!(
        ATTEMPT_CODES,
        [
            "not_paired",
            "credential_unavailable",
            "saturated",
            "audit_unavailable",
            "store_unavailable",
            "unreachable",
            "timeout",
            "cancelled",
            "unauthorized",
            "forbidden",
            "budget_exhausted",
            "rate_limited",
            "rejected",
            "cloud_error",
            "invalid_response",
        ]
    );
    // 记录的每个「没有回答」的结局都有一个对应的 X-iDoris-Cloud-Attempt 值（served 是成功，interrupted 只在崩溃恢复里出现）
    for outcome in CallOutcome::ALL {
        if matches!(outcome, CallOutcome::Served | CallOutcome::Interrupted) {
            continue;
        }
        assert!(ATTEMPT_CODES.contains(&outcome.as_str()), "{outcome:?}");
    }
    // 每个值都是 snake_case，能放进响应头
    for code in ATTEMPT_CODES {
        assert!(
            code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
            "{code}"
        );
        assert!(HeaderValue::from_str(code).is_ok());
    }
    for name in [
        HEADER_CLOUD_RECORD_ID,
        HEADER_CLOUD_ATTEMPT,
        HEADER_CLOUD_REASON,
        HEADER_CLOUD_SERVICE_TIER,
        HEADER_CLOUD_MAX_TOKENS,
    ] {
        assert!(
            HeaderName::from_bytes(name.to_ascii_lowercase().as_bytes()).is_ok(),
            "{name}"
        );
        assert!(name.starts_with("X-iDoris-Cloud-"));
    }
    assert_eq!(HEADER_CONSENT, "x-idoris-cloud-fallback");
    assert_eq!(CONSENT_VALUE, "allow");
    assert_eq!(PROVIDER_ID, "idoris-cloud");
}

#[test]
fn annotating_a_response_changes_nothing_but_one_header() {
    let original = (StatusCode::SERVICE_UNAVAILABLE, "body").into_response();
    let before = original.headers().clone();
    let annotated = annotate(original, "unreachable");
    assert_eq!(annotated.status(), StatusCode::SERVICE_UNAVAILABLE);
    let mut after = annotated.headers().clone();
    assert_eq!(
        after.remove("x-idoris-cloud-attempt").unwrap(),
        "unreachable"
    );
    assert_eq!(after, before);
}

#[test]
fn a_served_response_reports_where_it_came_from_and_never_a_local_cost() {
    let card = build_card(&config_with("")).unwrap();
    let selected = Selected {
        decision: idoris_policy::Decision {
            chosen_id: PROVIDER_ID.to_string(),
            reason_codes: vec![idoris_policy::ReasonCode::AdmissionReady],
            degradations: Vec::new(),
        },
        served_locality: Locality::Remote,
        card,
        load_policy: LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        },
        estimated_cost_minor: 0,
    };
    let answer = forward::CloudAnswer {
        body: axum::body::Bytes::from_static(br#"{"choices":[]}"#),
        status: 200,
        cloud_record_id: "cloud-rec-1".to_string(),
        reason: Some("budget_degraded".to_string()),
        service_tier: Some("degraded".to_string()),
        max_tokens: Some("512".to_string()),
        tokens_in: Some(1),
        tokens_out: Some(2),
    };
    let response = served_response(&selected, answer);
    assert_eq!(response.status(), StatusCode::OK);
    let h = response.headers();
    assert_eq!(h["content-type"], "application/json");
    assert_eq!(h["x-idoris-served-locality"], "remote");
    assert_eq!(h["x-idoris-degraded"], "true");
    assert_eq!(h["x-idoris-reason"], "AdmissionReady,CloudTierFallback");
    assert_eq!(h["x-idoris-cloud-record-id"], "cloud-rec-1");
    assert_eq!(h["x-idoris-cloud-reason"], "budget_degraded");
    assert_eq!(h["x-idoris-cloud-service-tier"], "degraded");
    assert_eq!(h["x-idoris-cloud-max-tokens"], "512");
    // 云端的头不以它们自己的名字出现（不和本机同名头混），本机不记云端成本，也没有本机用量
    assert!(h.get("x-idoris-tier").is_none() && h.get("x-idoris-max-tokens").is_none());
    assert!(h.get("x-idoris-cost-minor").is_none());
    assert!(
        h.get("x-idoris-record-id").is_none(),
        "the local record id is the middleware's, not ours"
    );
    assert!(
        response
            .extensions()
            .get::<crate::usage::UsageFact>()
            .is_none()
    );
}

// ------------------------------------------------------------------------------------------------
// 启动：形态 × 凭证来源
// ------------------------------------------------------------------------------------------------

struct Rig {
    dir: tempfile::TempDir,
}

impl Rig {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    /// 每次调用写一个新文件（一个测试里要同时有几份配置）。
    fn config(&self, credential: &str) -> PathBuf {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let path = self.dir.path().join(format!("cloud-tier-{n}.yaml"));
        std::fs::write(
            &path,
            format!(
                "schema_version: 1\nbase_url: http://127.0.0.1:9/api\nmodel: idoris/agent\n{credential}\n"
            ),
        )
        .unwrap();
        path
    }

    fn node_file(&self, mode: u32, contents: &str) -> PathBuf {
        let path = self.dir.path().join("node.key");
        std::fs::write(&path, contents).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    fn resolve(
        &self,
        startup: StartupForm,
        config: Option<PathBuf>,
        env: &[(&'static str, String)],
    ) -> Result<Option<Arc<CloudTier>>, String> {
        let db = self.dir.path().join("idoris.sqlite3");
        let env = env.to_vec();
        CloudTier::resolve_for_form(
            &startup,
            config,
            move || Ok(db),
            &move |name| {
                env.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, v)| v.clone())
            },
            None,
        )
    }
}

fn form(mode: DeployMode, raw: Option<&str>) -> StartupForm {
    StartupForm::from_env_value(mode, raw).unwrap()
}

#[test]
fn unset_means_nothing_at_all_for_every_form_and_touches_nothing() {
    let rig = Rig::new();
    for startup in [
        form(DeployMode::Personal, None),
        form(DeployMode::Personal, Some("personal")),
        form(DeployMode::Tenant, None),
        form(DeployMode::Tenant, Some("organization")),
        form(DeployMode::Tenant, Some("cloud")),
    ] {
        assert!(rig.resolve(startup, None, &[]).unwrap().is_none());
    }
    // 没有创建数据库文件，也没有设备凭证目录
    assert_eq!(std::fs::read_dir(rig.dir.path()).unwrap().count(), 0);
}

#[test]
fn the_personal_form_accepts_a_device_credential_and_does_not_need_to_be_paired_yet() {
    let rig = Rig::new();
    let config = rig.config("");
    for startup in [
        form(DeployMode::Personal, None),
        form(DeployMode::Personal, Some("personal")),
    ] {
        let tier = rig
            .resolve(startup, Some(config.clone()), &[])
            .unwrap()
            .unwrap();
        assert_eq!(tier.config().origin(), "http://127.0.0.1:9");
        // 还没配对：不是启动错误
        assert_eq!(
            tier.provider().load().unwrap_err(),
            CredentialError::NotPaired
        );
    }
    // 打开了调用记录库（启动时才建表）
    let conn = rusqlite::Connection::open(rig.dir.path().join("idoris.sqlite3")).unwrap();
    let tables: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'cloud_tier_calls'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 1);
}

#[test]
fn the_cloud_form_and_legacy_tenant_deployments_refuse_to_enable_a_cloud_tier() {
    let rig = Rig::new();
    let config = rig.config("");
    let cloud = rig
        .resolve(
            form(DeployMode::Tenant, Some("cloud")),
            Some(config.clone()),
            &[],
        )
        .unwrap_err();
    assert!(cloud.contains("cloud 形态"), "{cloud}");
    let legacy = rig
        .resolve(form(DeployMode::Tenant, None), Some(config.clone()), &[])
        .unwrap_err();
    assert!(legacy.contains("IDORIS_FORM=organization"), "{legacy}");
    // 拒绝发生在碰配置文件和数据库之前
    let missing = rig
        .resolve(
            form(DeployMode::Tenant, Some("cloud")),
            Some(PathBuf::from("/nonexistent/cloud-tier.yaml")),
            &[],
        )
        .unwrap_err();
    assert!(missing.contains("cloud 形态"), "{missing}");
    assert!(!rig.dir.path().join("idoris.sqlite3").exists());
}

#[test]
fn the_credential_kind_must_match_the_form() {
    let rig = Rig::new();
    let node = rig.node_file(0o600, &secret('a'));
    let node_config = rig.config(&format!(
        "credential: {{source: node_file, file: {}}}",
        node.display()
    ));
    let personal_config = rig.config("");
    let org = form(DeployMode::Tenant, Some("organization"));
    let personal = form(DeployMode::Personal, None);
    // 组织形态 + 个人设备配对
    let error = rig.resolve(org, Some(personal_config), &[]).unwrap_err();
    assert!(error.contains("Node 凭证"), "{error}");
    // 个人形态 + Node 凭证
    let error = rig
        .resolve(personal, Some(node_config.clone()), &[])
        .unwrap_err();
    assert!(error.contains("设备凭证"), "{error}");
    // 组织形态 + Node 凭证文件：好
    let tier = rig.resolve(org, Some(node_config), &[]).unwrap().unwrap();
    assert_eq!(
        tier.provider().load().unwrap().secret.expose_secret(),
        secret('a')
    );
}

#[test]
fn an_organization_node_credential_is_checked_at_start_up_not_at_the_first_fallback() {
    let rig = Rig::new();
    let org = form(DeployMode::Tenant, Some("organization"));
    for (mode, contents, expected) in [
        (0o644, secret('a'), "不可用"),
        (0o666, secret('a'), "不可用"),
        (0o600, "ab".repeat(32), "格式不对"), // Admin token 的形状
        (0o600, String::new(), "格式不对"),
        (0o600, format!("{}x", secret('a')), "格式不对"),
    ] {
        let file = rig.node_file(mode, &contents);
        let config = rig.config(&format!(
            "credential: {{source: node_file, file: {}}}",
            file.display()
        ));
        let error = rig.resolve(org, Some(config), &[]).unwrap_err();
        assert!(error.contains(expected), "{mode:o}/{contents:?}: {error}");
        assert!(
            !error.contains(&secret('a')),
            "the secret must not be echoed: {error}"
        );
    }
    // 文件不存在
    let config = rig.config("credential: {source: node_file, file: /nonexistent/node.key}");
    assert!(rig.resolve(org, Some(config), &[]).is_err());
    // 环境变量来源
    let config = rig.config("credential: {source: node_env, env: IDORIS_TEST_NODE}");
    assert!(
        rig.resolve(org, Some(config.clone()), &[]).is_err(),
        "unset env"
    );
    assert!(
        rig.resolve(
            org,
            Some(config.clone()),
            &[("IDORIS_TEST_NODE", "ab".repeat(32))]
        )
        .is_err(),
        "an Admin-token-shaped value"
    );
    let tier = rig
        .resolve(org, Some(config), &[("IDORIS_TEST_NODE", secret('b'))])
        .unwrap()
        .unwrap();
    assert_eq!(
        tier.provider().load().unwrap().secret.expose_secret(),
        secret('b')
    );
}

#[test]
fn a_local_key_in_the_node_credential_file_is_refused_at_start_up() {
    let rig = Rig::new();
    let file = rig.node_file(0o600, &secret('a'));
    let config = rig.config(&format!(
        "credential: {{source: node_file, file: {}}}",
        file.display()
    ));
    struct Local(String);
    impl LocalSecretProbe for Local {
        fn is_local_secret(&self, candidate: &str) -> bool {
            candidate == self.0
        }
    }
    let db = rig.dir.path().join("idoris.sqlite3");
    let result = CloudTier::resolve_for_form(
        &form(DeployMode::Tenant, Some("organization")),
        Some(config),
        move || Ok(db),
        &|_| None,
        Some(Arc::new(Local(secret('a')))),
    );
    let error = result.unwrap_err();
    assert!(error.contains("本地数据密钥"), "{error}");
}

#[test]
fn a_broken_configuration_stops_start_up_and_names_the_file() {
    let rig = Rig::new();
    let path = rig.dir.path().join("broken.yaml");
    std::fs::write(&path, "schema_version: 1\nsurprise: true\n").unwrap();
    let error = rig
        .resolve(form(DeployMode::Personal, None), Some(path.clone()), &[])
        .unwrap_err();
    assert!(
        error.contains("broken.yaml") && error.contains("云端档配置"),
        "{error}"
    );
    let missing = rig
        .resolve(
            form(DeployMode::Personal, None),
            Some(rig.dir.path().join("nope.yaml")),
            &[],
        )
        .unwrap_err();
    assert!(missing.contains("无法读取云端档配置"), "{missing}");
}

#[test]
fn crash_leftovers_are_marked_interrupted_when_the_tier_starts() {
    let rig = Rig::new();
    let db = rig.dir.path().join("idoris.sqlite3");
    {
        let store = CloudTierStore::open(&db).unwrap();
        store.begin_call("left-by-a-crash", 1, 5).unwrap();
    }
    let tier = rig
        .resolve(form(DeployMode::Personal, None), Some(rig.config("")), &[])
        .unwrap()
        .unwrap();
    let row = tier
        .store()
        .call_by_local_id("left-by-a-crash", 1)
        .unwrap()
        .unwrap();
    assert_eq!(
        row.state,
        idoris_tenancy::cloud_tier::CallState::Final(CallOutcome::Interrupted)
    );
}

// ------------------------------------------------------------------------------------------------
// pending 行的看守：调用方挂断（处理 future 被丢掉）时，这一行必须落成终态，不能一直 pending
// ------------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_pending_call_that_is_dropped_is_finished_as_cancelled_and_a_disarmed_one_is_left_to_its_owner()
 {
    use idoris_tenancy::cloud_tier::CallState;
    let tier = tier_for_body_tests();

    // 没有 disarm 就被丢掉（调用方在出网途中挂断）：写成 cancelled，没有云端 id、没有金额
    let pending = tier.record_begin("req-dropped").await.unwrap();
    let row = tier
        .store()
        .call_by_local_id("req-dropped", 1)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CallState::Pending);
    drop(pending);
    // 取消终态在后台线程写（析构不等数据库，见 `PendingCall`）：先等它写完，再读——栅栏是确定性的，不是睡一觉
    tier.wait_for_background_writes().await;
    let row = tier
        .store()
        .call_by_local_id("req-dropped", 1)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CallState::Final(CallOutcome::Cancelled));
    assert!(row.finished_at_ms.is_some());
    assert_eq!(
        (
            row.cloud_record_id,
            row.http_status,
            row.cloud_reason,
            row.tokens_in,
            row.tokens_out
        ),
        (None, None, None, None, None)
    );

    // disarm 之后 drop 什么都不做：终态由正常路径写（这里写成 served，带云端 id 和用量）
    let mut pending = tier.record_begin("req-finished").await.unwrap();
    let ticket = pending.disarm();
    drop(pending);
    tier.wait_for_background_writes().await;
    assert_eq!(
        tier.store()
            .call_by_local_id("req-finished", 1)
            .unwrap()
            .unwrap()
            .state,
        CallState::Pending,
        "dropping a disarmed guard must not touch the row"
    );
    tier.record_finish(
        ticket,
        OwnedFinish {
            outcome: CallOutcome::Served,
            finished_at_ms: 9,
            cloud_record_id: Some("cloud-1".to_string()),
            http_status: Some(200),
            cloud_reason: None,
            tokens_in: Some(3),
            tokens_out: Some(4),
        },
    )
    .await
    .unwrap();
    let row = tier
        .store()
        .call_by_local_id("req-finished", 1)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, CallState::Final(CallOutcome::Served));
    assert_eq!(row.cloud_record_id.as_deref(), Some("cloud-1"));

    // 已经有终态的行：看守再被丢掉也不覆盖它
    let mut pending = tier.record_begin("req-already-final").await.unwrap();
    let ticket = pending.disarm();
    tier.record_finish(
        ticket,
        OwnedFinish {
            outcome: CallOutcome::Timeout,
            finished_at_ms: 9,
            cloud_record_id: None,
            http_status: None,
            cloud_reason: None,
            tokens_in: None,
            tokens_out: None,
        },
    )
    .await
    .unwrap();
    pending.armed = true;
    drop(pending);
    // 没有这道栅栏，下面的断言会在后台写入落下之前就通过，什么都没测到
    tier.wait_for_background_writes().await;
    assert_eq!(
        tier.store()
            .call_by_local_id("req-already-final", 1)
            .unwrap()
            .unwrap()
            .state,
        CallState::Final(CallOutcome::Timeout)
    );
}

#[tokio::test]
async fn a_caller_that_hangs_up_while_the_pending_row_is_being_written_still_ends_up_with_a_final_row()
 {
    use idoris_tenancy::cloud_tier::CallState;
    let tier = tier_for_body_tests();
    // 零时限：`record_begin` 被 poll 一次（写库已经交给了阻塞线程）就被丢掉，接收端随之消失
    let attempt = tokio::time::timeout(
        std::time::Duration::ZERO,
        tier.record_begin("req-hangup-during-begin"),
    )
    .await;
    // 写库抢在丢弃之前完成时，看守已经在手上：它也一样被丢掉
    drop(attempt);
    // 不管谁抢在前：这一行最终必须是 cancelled，不能停在 pending
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let row = tier
            .store()
            .call_by_local_id("req-hangup-during-begin", 1)
            .unwrap();
        if let Some(row) = row
            && row.state != CallState::Pending
        {
            assert_eq!(row.state, CallState::Final(CallOutcome::Cancelled));
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the row stayed pending after the caller hung up"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// 用文件库（别的连接也能打开它）造一个云端档，测试要从另一个连接占住写锁。
fn tier_with_file_store(db: &Path) -> CloudTier {
    let config = config_with("");
    let http = idoris_upstream::cloud::build_http_client(config.connect_timeout).unwrap();
    CloudTier::new(CloudTierParts {
        config,
        store: Arc::new(CloudTierStore::open(db).unwrap()),
        provider: Arc::new(credential::NodeEnvProvider::new(None, None)),
        http,
        clock: Arc::new(SystemClock),
    })
    .unwrap()
}

/// 轮询到这一行有了终态（或超时）。
async fn wait_for_final_state(
    tier: &CloudTier,
    local_request_id: &str,
    within: std::time::Duration,
) -> Option<idoris_tenancy::cloud_tier::CallRecord> {
    use idoris_tenancy::cloud_tier::CallState;
    let deadline = std::time::Instant::now() + within;
    loop {
        if let Some(row) = tier.store().call_by_local_id(local_request_id, 1).unwrap()
            && row.state != CallState::Pending
        {
            return Some(row);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// 审查 r4 Medium#7：看守在异步任务里被丢掉（调用方挂断，hyper 丢掉整个处理 future）时，不能同步等数据库——
/// `finish_call` 要等连接的互斥锁和 `BEGIN IMMEDIATE`（busy timeout 5 秒）。别的写事务占着库时，同时挂断的几个云请求
/// 会把几个异步工作线程全卡住，同一个运行时上的其它请求和计时器也跑不动。取消终态交给阻塞线程池去写；写入仍然保证落下。
#[tokio::test]
async fn dropping_a_pending_call_never_waits_for_the_database_on_the_async_thread() {
    use idoris_tenancy::cloud_tier::CallState;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tier.sqlite3");
    let tier = tier_with_file_store(&db);
    let pending = tier.record_begin("req-held-up").await.unwrap();

    // 另一个连接占着写锁（别的写事务、对账方的长事务）；存储连接的 busy timeout 是 5 秒
    let holder = rusqlite::Connection::open(&db).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();

    // `#[tokio::test]` 是单线程运行时：这里卡住的就是整个运行时
    let started = std::time::Instant::now();
    drop(pending);
    let blocked_for = started.elapsed();
    assert!(
        blocked_for < std::time::Duration::from_millis(1500),
        "dropping the guard blocked the async thread for {blocked_for:?} while another connection held the write lock"
    );

    // 占着库的时候，行还没有终态；锁一放开，后台的写入就把它写成 cancelled
    assert_eq!(
        tier.store()
            .call_by_local_id("req-held-up", 1)
            .unwrap()
            .unwrap()
            .state,
        CallState::Pending
    );
    holder.execute_batch("COMMIT").unwrap();
    let row = wait_for_final_state(&tier, "req-held-up", std::time::Duration::from_secs(10))
        .await
        .expect("the cancelled row was never written after the lock was released");
    assert_eq!(row.state, CallState::Final(CallOutcome::Cancelled));
    assert_eq!(
        (
            row.cloud_record_id,
            row.http_status,
            row.tokens_in,
            row.tokens_out
        ),
        (None, None, None, None)
    );
}

/// 停机时：进程收到停机信号，还连着的调用方的处理 future 随运行时一起被丢掉，看守在这时析构。交给阻塞线程池的写入
/// 要在 `Runtime::shutdown_timeout`（二进制停机时用的就是它）返回之前落下，否则这一行只能等下次启动被标成 interrupted。
#[test]
fn a_cancel_write_that_is_in_flight_when_the_runtime_shuts_down_still_lands() {
    use idoris_tenancy::cloud_tier::CallState;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("tier.sqlite3");
    let tier = Arc::new(tier_with_file_store(&db));
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        for n in 0..8 {
            let pending = tier
                .record_begin(&format!("req-at-shutdown-{n}"))
                .await
                .unwrap();
            // 一个还没做完的处理 future 持有看守（调用方还连着）
            tokio::spawn(async move {
                let _held = pending;
                std::future::pending::<()>().await;
            });
        }
        tokio::task::yield_now().await;
    });
    // 和二进制一样的停机方式：丢掉所有任务（看守析构），再等阻塞任务
    runtime.shutdown_timeout(std::time::Duration::from_secs(10));
    for n in 0..8 {
        let row = tier
            .store()
            .call_by_local_id(&format!("req-at-shutdown-{n}"), 1)
            .unwrap()
            .unwrap();
        assert_eq!(
            row.state,
            CallState::Final(CallOutcome::Cancelled),
            "req-at-shutdown-{n} was left pending by the shutdown"
        );
    }
}

// ------------------------------------------------------------------------------------------------
// 本地密钥探针
// ------------------------------------------------------------------------------------------------

#[test]
fn the_local_key_probe_recognises_active_local_keys_and_fails_closed_when_it_cannot_tell() {
    use idoris_tenancy::virtual_key::MintedVirtualKey;
    use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
    let store = Arc::new(std::sync::Mutex::new(
        VirtualKeyStore::new(rusqlite::Connection::open_in_memory().unwrap()).unwrap(),
    ));
    let key = MintedVirtualKey::mint();
    store
        .lock()
        .unwrap()
        .insert_active(
            &key.key_id,
            key.hash,
            &VirtualKeyScope {
                owner: "t".into(),
                tenant_id: None,
                allowed_privacy: vec![PrivacyClass::LocalOnly],
                allowed_roles: vec!["fast".into()],
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    let probe = VirtualKeyProbe(store.clone());
    assert!(
        probe.is_local_secret(key.secret.expose_secret()),
        "a live local key"
    );
    assert!(
        !probe.is_local_secret(&secret('c')),
        "an unknown key is not a local key"
    );
    assert!(!probe.is_local_secret("not even key-shaped"));
    // 吊销之后它不再是「有效的本地数据密钥」
    store.lock().unwrap().revoke(&key.key_id, 1).unwrap();
    assert!(!probe.is_local_secret(key.secret.expose_secret()));
    // 库被毒化（判不了）：按「是」处理，宁可拒绝
    let poisoned = store.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poisoned.lock().unwrap();
        panic!("poison the key store");
    })
    .join();
    assert!(probe.is_local_secret(&secret('c')), "cannot tell -> refuse");
}

// ------------------------------------------------------------------------------------------------
// 密钥不进 Debug（审查 r4 Medium#3）：每一个持有云凭证明文的类型，`{:?}` / `{:#?}` 都不含密钥——包括经由
// `dyn CredentialProvider`、`Arc`、`Option`、`Result`、`Vec` 这些包装格式化的情形。
// ------------------------------------------------------------------------------------------------

#[test]
fn no_type_that_holds_a_secret_prints_it_in_debug() {
    use credential::{
        CredentialKind, DeviceRecord, LoadedCredential, NodeEnvProvider, NodeFileProvider,
        PairedDeviceProvider,
    };

    let raw = secret('e');
    // 整个密钥、以及它的任何一段足够长的十六进制（哪怕只漏出一部分也算漏）
    let needles = [raw.clone(), "e".repeat(16)];
    let assert_clean = |what: &str, rendered: String| {
        for needle in &needles {
            assert!(
                !rendered.contains(needle.as_str()),
                "{what} printed the secret in Debug output: {rendered}"
            );
        }
    };

    // 环境变量来源：值就存在结构体里（启动时读一次）。`NodeEnvProvider` 以前直接 derive(Debug)。
    let env = NodeEnvProvider::new(Some(raw.clone()), None);
    assert_clean("NodeEnvProvider {:?}", format!("{env:?}"));
    assert_clean("NodeEnvProvider {:#?}", format!("{env:#?}"));
    // 值不是合法密钥的形状（比如 Admin token 被误填）也是密钥：同样不能打印
    let admin_like = "ab".repeat(32);
    let misfilled = NodeEnvProvider::new(Some(admin_like.clone()), None);
    assert!(
        !format!("{misfilled:?}").contains(&admin_like),
        "a mis-filled env value must not be printed either"
    );
    // 经 `dyn CredentialProvider`（云端档里存的就是这个类型）、`Arc`、`Option`、`Vec` 格式化
    let dynamic: Arc<dyn CredentialProvider> =
        Arc::new(NodeEnvProvider::new(Some(raw.clone()), None));
    assert_clean("Arc<dyn CredentialProvider>", format!("{dynamic:?}"));
    assert_clean("&dyn CredentialProvider", format!("{:?}", &*dynamic));
    assert_clean(
        "Option<Arc<dyn ..>>",
        format!("{:?}", Some(dynamic.clone())),
    );
    assert_clean("Vec<Arc<dyn ..>>", format!("{:#?}", vec![dynamic.clone()]));

    // 云端档本身（持有凭证来源）
    let config = config_with("");
    let http = idoris_upstream::cloud::build_http_client(config.connect_timeout).unwrap();
    let tier = CloudTier::new(CloudTierParts {
        config,
        store: Arc::new(CloudTierStore::open_in_memory().unwrap()),
        provider: dynamic.clone(),
        http,
        clock: Arc::new(SystemClock),
    })
    .unwrap();
    assert_clean("CloudTier {:?}", format!("{tier:?}"));
    assert_clean("CloudTier {:#?}", format!("{tier:#?}"));
    assert_clean("Arc<CloudTier>", format!("{:?}", Arc::new(tier)));

    // 凭证、加载出来的凭证、设备记录、兑换结果
    let credential = CloudCredential::parse(&raw).unwrap();
    let loaded = LoadedCredential {
        secret: credential.clone(),
        kind: CredentialKind::Node,
    };
    assert_clean("CloudCredential", format!("{credential:?} {credential:#?}"));
    assert_clean("LoadedCredential", format!("{loaded:?} {loaded:#?}"));
    assert_clean(
        "Result<LoadedCredential, _>",
        format!("{:?}", Ok::<_, CredentialError>(loaded.clone())),
    );
    let record = DeviceRecord {
        origin: "https://idoris.ai".to_string(),
        credential_id: "vk_0123456789abcdef".to_string(),
        membership: "m-agent-1".to_string(),
        pairing_id: "pc_0123456789abcdef".to_string(),
        paired_at_ms: 1,
        secret: credential.clone(),
    };
    assert_clean("DeviceRecord", format!("{record:?} {record:#?}"));
    let redeemed = pairing::Redeemed {
        secret: credential.clone(),
        credential_id: "vk_0123456789abcdef".to_string(),
        membership: "m-agent-1".to_string(),
        pairing_id: "pc_0123456789abcdef".to_string(),
    };
    assert_clean("Redeemed", format!("{redeemed:?} {redeemed:#?}"));

    // 文件来源的 provider 只存路径，不存密钥：确认 Debug 里没有密钥
    let dir = tempfile::tempdir().unwrap();
    let node_file = NodeFileProvider::new(dir.path().join("node.key"), None);
    assert_clean("NodeFileProvider", format!("{node_file:?}"));
    let paired = PairedDeviceProvider::new(
        credential::DeviceStore::new(dir.path()),
        "https://idoris.ai",
        None,
    );
    assert_clean("PairedDeviceProvider", format!("{paired:?}"));
}
