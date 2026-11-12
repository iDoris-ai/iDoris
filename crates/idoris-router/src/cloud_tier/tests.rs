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
