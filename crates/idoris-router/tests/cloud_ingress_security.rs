//! 计算服务 S5 的安全测试：冒用付款上下文、冒用租户、路由表、控制面、Admin v0、CORS。
//!
//! 每一条「被拒」都断言三件事：HTTP 答案；假上游没有收到任何请求；账本里**没有任何账户**被记账
//! （包括凭证自己绑定的那个组织账户——「不回落到凭证自身的账户」）。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use h::*;
use idoris_compute_contracts::account::AccountStatus;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::VirtualKeyScope;
use serde_json::json;

/// 没有任何账户被记账、没有请求出网、没有留下请求记录。
fn assert_nothing_happened(w: &World) {
    assert_eq!(
        w.upstream.request_count(),
        0,
        "a refused request must never reach the upstream"
    );
    assert_eq!(
        w.request_count(),
        0,
        "a refused request must not leave a request record"
    );
    for account in [
        "eduai", "school-a", "school-b", "s1", "s2", "s3", "sh-a", "sh-b", "agent24", "a1", "a2",
        "rival", "r1", "stray", "st1",
    ] {
        assert_eq!(w.spent(account), 0, "{account} must not have been charged");
    }
}

// ---------------------------------------------------------------------------
// 冒用付款上下文
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_service_credential_cannot_name_a_member_outside_its_scope() {
    let w = World::new(WorldOptions::plenty()).await;
    let school_a = w.cred("svc_school_a");
    let eduai = w.cred("svc_eduai");
    let mut bodies = Vec::new();
    for (cred, member) in [
        // 同一个租户里、范围外的学校的孩子
        (school_a, "kid-3"),
        // 另一个租户的成员
        (school_a, "agent-1"),
        (eduai, "rival-1"),
        (eduai, "stray-1"),
        // 不存在的主体
        (eduai, "nobody-at-all"),
    ] {
        let reply = w.send(&Req::chat(cred).member(member)).await;
        assert_eq!(reply.status, 403, "{member}: {}", reply.text());
        assert_eq!(reply.code(), "payment_context_invalid", "{member}");
        assert_eq!(reply.reason(), Some("payment_context_invalid"));
        bodies.push(reply.json());
    }
    // 不存在的主体和范围外的主体是同一个答案：不能拿来探测一个 id 是否存在
    assert!(
        bodies.windows(2).all(|pair| pair[0] == pair[1]),
        "{bodies:#?}"
    );
    assert_nothing_happened(&w);
}

#[tokio::test]
async fn a_service_credential_without_a_member_id_is_refused_and_never_falls_back_to_its_own_account()
 {
    let w = World::new(WorldOptions::plenty()).await;
    for name in ["svc_eduai", "svc_school_a", "svc_agent24"] {
        let reply = w.send(&Req::chat(w.cred(name))).await;
        assert_eq!(reply.status, 403, "{name}: {}", reply.text());
        assert_eq!(reply.code(), "payment_context_invalid");
        assert!(
            reply.json()["error"]["remediation"]
                .as_str()
                .unwrap()
                .contains("X-iDoris-Member"),
            "the refusal tells the backend what to send"
        );
        // 模型列表同样要成员 id：每个数据面请求都有一个付款上下文
        let models = w.send(&Req::models(w.cred(name))).await;
        assert_eq!(models.status, 403, "{name}");
    }
    // 凭证绑定的组织账户（eduai / school-a / agent24）都有充值，如果回落到它们，这里会被记账
    assert_nothing_happened(&w);
}

#[tokio::test]
async fn member_and_node_credentials_cannot_name_someone_else() {
    let w = World::new(WorldOptions::plenty()).await;
    // 成员凭证断言别人
    let reply = w.send(&Req::chat(w.cred("mem_kid1")).member("kid-2")).await;
    assert_eq!(
        (reply.status.as_u16(), reply.code()),
        (403, "payment_context_invalid".to_string())
    );
    // 节点凭证不允许带成员 id（云端只认组织付款账户）
    let reply = w
        .send(&Req::chat(w.cred("node_agent24")).member("agent-1"))
        .await;
    assert_eq!(
        (reply.status.as_u16(), reply.code()),
        (403, "payment_context_invalid".to_string())
    );
    assert_nothing_happened(&w);

    // 对照：带上自己的主体就行；节点凭证不带就行，付款人是绑定的组织
    assert_eq!(
        w.send(&Req::chat(w.cred("mem_kid1")).member("kid-1"))
            .await
            .status,
        200
    );
    assert_eq!(w.send(&Req::chat(w.cred("node_agent24"))).await.status, 200);
    let rows = w.request_rows();
    assert_eq!(rows[0].0, "s1");
    assert_eq!(rows[1].0, "agent24");
}

#[tokio::test]
async fn the_member_header_must_be_exactly_one_valid_id() {
    let w = World::new(WorldOptions::plenty()).await;
    let svc = w.cred("svc_eduai");
    for request in [
        // 两个头：不是「取第一个」
        Req::chat(svc).member("kid-1").member("kid-2"),
        Req::chat(svc).header("x-idoris-member", ""),
        Req::chat(svc).header("x-idoris-member", "kid 1"),
        Req::chat(svc).header("x-idoris-member", "kid-1@example.com"),
        Req::chat(svc).header("x-idoris-member", "../kid-1"),
        Req::chat(svc).header("x-idoris-member", &"k".repeat(129)),
        // 成员凭证带了格式不对的头：格式先于一切被拒
        Req::chat(w.cred("mem_kid1")).header("x-idoris-member", "kid 1"),
    ] {
        let reply = w.send(&request).await;
        assert_eq!(reply.status, 400, "{}", reply.text());
        assert_eq!(reply.code(), "member_id_invalid");
    }
    assert_nothing_happened(&w);
}

#[tokio::test]
async fn a_tenant_without_a_declaration_is_refused_not_guessed() {
    let w = World::new(WorldOptions::plenty()).await;
    let reply = w
        .send(&Req::chat(w.cred("svc_stray")).member("stray-1"))
        .await;
    assert_eq!(reply.status, 403, "{}", reply.text());
    assert_eq!(reply.code(), "tenant_not_provisioned");
    assert_nothing_happened(&w);
}

#[tokio::test]
async fn a_local_data_key_has_no_payment_context_and_is_refused() {
    let w = World::new(WorldOptions::plenty()).await;
    // `idoris key issue` 签发的本地数据密钥：在虚拟 key 库里，但不是账户库里的凭证
    let minted = MintedVirtualKey::mint();
    w.keys
        .lock()
        .unwrap()
        .insert_active(
            &minted.key_id,
            minted.hash,
            &VirtualKeyScope {
                owner: "local".into(),
                tenant_id: Some("eduai".into()),
                allowed_privacy: vec![idoris_contracts::common::PrivacyClass::LocalOnly],
                allowed_roles: vec!["fast".into()],
                budget_ref: None,
                expires_at_ms: None,
                admin_scopes: Vec::new(),
            },
        )
        .unwrap();
    let reply = w
        .send(&Req::chat(&Cred {
            id: minted.key_id.clone(),
            secret: minted.secret.expose_secret().to_string(),
        }))
        .await;
    assert_eq!(reply.status, 401, "{}", reply.text());
    assert_nothing_happened(&w);
}

#[tokio::test]
async fn frozen_accounts_and_revoked_memberships_stop_the_next_request() {
    let w = World::new(WorldOptions::plenty()).await;
    let svc = w.cred("svc_eduai");
    assert_eq!(w.send(&Req::chat(svc).member("kid-1")).await.status, 200);

    // 冻结学校 A（付款账户 s1 的祖先，凭证锚点 eduai 之下）：凭证本身没事，这个成员的付款上下文建立不了
    w.accounts
        .set_account_status(&aid("school-a"), AccountStatus::Frozen)
        .unwrap();
    let frozen = w.send(&Req::chat(svc).member("kid-1")).await;
    assert_eq!(
        (frozen.status.as_u16(), frozen.code()),
        (403, "account_inactive".to_string())
    );
    // 另一所学校的孩子不受影响
    assert_eq!(w.send(&Req::chat(svc).member("kid-3")).await.status, 200);
    // 成员凭证在冻结的子树里：不透明的 401
    assert_eq!(w.send(&Req::chat(w.cred("mem_kid1"))).await.status, 401);

    // 撤销成员关系
    w.accounts
        .revoke_membership(
            &idoris_compute_contracts::ids::MembershipId::new("m-kid-3").unwrap(),
            ts(),
        )
        .unwrap();
    let revoked = w.send(&Req::chat(svc).member("kid-3")).await;
    assert_eq!(
        (revoked.status.as_u16(), revoked.code()),
        (403, "membership_revoked".to_string())
    );
    assert_eq!(w.upstream.request_count(), 2);
}

// ---------------------------------------------------------------------------
// 冒用租户：X-iDoris-Tenant 不读、不信
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tenant_header_is_ignored_and_the_tenant_always_comes_from_the_credential() {
    let w = World::new(WorldOptions::plenty()).await;
    let mut streaming = simple_body(100);
    streaming["stream"] = json!(true);

    // eduai 的成员凭证（child_edu：一律非流式）带着 X-iDoris-Tenant: agent24（adult：可以流式）想换租户
    let reply = w
        .send(
            &Req::chat(w.cred("mem_kid1"))
                .header("x-idoris-tenant", "agent24")
                .body(streaming.clone()),
        )
        .await;
    assert_eq!(reply.status, 400, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "streaming_not_allowed");
    assert_eq!(w.upstream.request_count(), 0);

    // 反过来：adult 租户带着 X-iDoris-Tenant: eduai，不会被降成 child_edu（也不会被升成别的）
    let reply = w
        .send(
            &Req::chat(w.cred("svc_agent24"))
                .member("agent-1")
                .header("x-idoris-tenant", "eduai")
                .body(streaming),
        )
        .await;
    assert_ne!(
        reply.json()["error"]["type"],
        "streaming_not_allowed",
        "{}",
        reply.text()
    );

    // 带着什么值、带几个、值是不是合法，都不影响结果：没有 400 invalid_tenant_scope、没有 403 tenant_scope_forbidden
    for headers in [
        vec![("x-idoris-tenant", "not-a-tenant")],
        vec![("x-idoris-tenant", "")],
        vec![("x-idoris-tenant", "a"), ("x-idoris-tenant", "b")],
        vec![("x-idoris-tenant", "rival")],
    ] {
        let mut request = Req::chat(w.cred("mem_kid1"));
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let reply = w.send(&request).await;
        assert_eq!(reply.status, 200, "{}", reply.text());
    }
    // 记到的永远是凭证推导出来的账户和租户属性
    for (payer, _, classes, billing) in w.request_rows().iter().skip(1) {
        assert_eq!(payer, "s1");
        assert_eq!(
            (classes.as_str(), billing.as_str()),
            ("child_edu", "subsidized")
        );
    }
    assert_eq!(w.spent("rival"), 0);
    assert_eq!(w.spent("r1"), 0);
}

// ---------------------------------------------------------------------------
// 路由表：云形态只有文档里的几条
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_the_documented_routes_exist_and_unknown_ones_are_404_before_any_authentication() {
    let w = World::new(WorldOptions::plenty()).await;
    let kid1 = w.cred("mem_kid1");
    for uri in [
        "/v1/chat/completions",
        "/v1/models",
        "/v1/embeddings",
        "/v1/messages",
        "/v1/rerank",
        "/v1/feedback",
        "/v1/inspect",
        "/capabilities",
        "/idoris/tenants/eduai/usage",
        "/idoris/tenants/eduai/budget",
        "/admin/api/v1/status",
        "/admin/api/v1/models",
        "/api/v1/embeddings",
        "/api/v1/completions",
        "/api/v2/models",
        "/api/v1",
        "/api",
        "/",
        "/api/v1/models/idoris",
    ] {
        // 带有效凭证
        let with_key = w.send(&Req::models(kid1).uri(uri)).await;
        assert_eq!(with_key.status, 404, "{uri}: {}", with_key.text());
        assert_eq!(with_key.json()["error"]["type"], "not_found", "{uri}");
        // 不带凭证：同样是 404，不是 401（不存在的路由不认证、不碰库、不泄露它存在不存在）
        let without = w.send(&Req::models(kid1).uri(uri).bearer_raw(None)).await;
        assert_eq!(without.status, 404, "{uri}");
    }
    // 方法不对：在已有的路径上也是 404（和既有的路由器一致）
    let wrong_method = w
        .send(&Req::models(kid1).uri("/api/v1/chat/completions"))
        .await;
    assert_eq!(wrong_method.status, 404, "{}", wrong_method.text());
    let wrong_method = w
        .send(&Req::chat(kid1).uri("/api/v1/models").method("POST"))
        .await;
    assert_eq!(wrong_method.status, 404, "{}", wrong_method.text());
    assert_nothing_happened(&w);

    // /health 还在（反向代理和编排器要用），不要凭证
    let health = w
        .send(&Req::models(kid1).uri("/health").bearer_raw(None))
        .await;
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["status"], "ok");
}

#[tokio::test]
async fn the_cloud_app_has_no_admin_surface_and_the_local_pipeline_is_not_reachable_through_it() {
    let w = World::new(WorldOptions::plenty()).await;
    let admin = w.cred("svc_eduai_admin");
    for uri in ["/admin/api/v1/status", "/admin/api/v1/backends", "/admin/"] {
        let reply = w.send(&Req::models(admin).uri(uri)).await;
        assert_eq!(reply.status, 404, "{uri}");
    }
    // 本地流水线的聊天入口：云形态下不存在（它读 X-iDoris-Tenant，是本地租户模式的契约）
    let reply = w
        .send(
            &Req::chat(w.cred("mem_kid1"))
                .uri("/v1/chat/completions")
                .header("x-idoris-tenant", "eduai"),
        )
        .await;
    assert_eq!(reply.status, 404);
    assert_nothing_happened(&w);
}

// ---------------------------------------------------------------------------
// 控制面：只有配置里写了 public: true 才挂在公网监听上
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_control_plane_is_mounted_on_the_public_listener_only_when_configured() {
    let off = World::new(WorldOptions {
        ingress: IngressYaml::default().control(false),
        ..WorldOptions::default()
    })
    .await;
    let admin = off.cred("svc_eduai_admin");
    for uri in [
        "/control/v1/whoami",
        "/control/v1/accounts/eduai",
        "/control/v1/audit",
    ] {
        let reply = off.send(&Req::models(admin).uri(uri)).await;
        assert_eq!(
            reply.status, 404,
            "{uri} must not exist when the control plane is not public"
        );
    }
    // 运维仍然可以用本机命令（同一个存储层函数）：账户库里的东西不受影响
    assert!(off.accounts.get_account(&aid("eduai")).unwrap().is_some());

    let on = World::new(WorldOptions::default()).await;
    let reply = on
        .send(&Req::models(on.cred("svc_eduai_admin")).uri("/control/v1/whoami"))
        .await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    // 控制面自己认证：没有凭证是它自己的 401，不是云入口的
    let anonymous = on
        .send(
            &Req::models(on.cred("svc_eduai_admin"))
                .uri("/control/v1/whoami")
                .bearer_raw(None),
        )
        .await;
    assert_eq!(anonymous.status, 401);
}

#[tokio::test]
async fn the_control_plane_never_carries_cors_headers_even_when_browsers_are_allowed_on_the_data_plane()
 {
    let w = World::new(WorldOptions {
        ingress: IngressYaml::default().cors(&["https://app.example.com"]),
        ..WorldOptions::default()
    })
    .await;
    let reply = w
        .send(
            &Req::models(w.cred("svc_eduai_admin"))
                .uri("/control/v1/whoami")
                .header("origin", "https://app.example.com"),
        )
        .await;
    assert_eq!(reply.status, 200);
    assert!(reply.header("access-control-allow-origin").is_none());
}

// ---------------------------------------------------------------------------
// CORS
// ---------------------------------------------------------------------------

#[tokio::test]
async fn by_default_no_cross_origin_access_is_granted_at_all() {
    let w = World::new(WorldOptions::plenty()).await;
    let kid1 = w.cred("mem_kid1");
    // 带 Origin 的正常请求：没有任何 CORS 头
    let reply = w
        .send(&Req::models(kid1).header("origin", "https://app.example.com"))
        .await;
    assert_eq!(reply.status, 200);
    assert!(reply.header("access-control-allow-origin").is_none());
    assert!(reply.header("access-control-expose-headers").is_none());
    assert!(reply.header("vary").is_none());
    // 预检：和任何未知的 OPTIONS 一样是 404
    let preflight = w
        .send(
            &Req::models(kid1)
                .method("OPTIONS")
                .bearer_raw(None)
                .header("origin", "https://app.example.com")
                .header("access-control-request-method", "POST"),
        )
        .await;
    assert_eq!(preflight.status, 404);
    assert!(preflight.header("access-control-allow-origin").is_none());
}

#[tokio::test]
async fn a_listed_origin_gets_cors_headers_and_the_s3_exposed_header_list_everywhere() {
    let w = World::new(WorldOptions {
        script: ok_script(5),
        ingress: IngressYaml::default().cors(&["https://app.example.com"]),
        ..WorldOptions::default()
    })
    .await;
    let kid1 = w.cred("mem_kid1");
    let exposed_of = |reply: &Reply| -> Vec<String> {
        reply
            .header("access-control-expose-headers")
            .unwrap_or_default()
            .split(',')
            .map(|s| s.trim().to_ascii_lowercase())
            .collect()
    };
    let assert_browser_can_read = |reply: &Reply| {
        let exposed = exposed_of(reply);
        // S3 的列表只有一份，一个不少；入口自己的两个也在
        for name in idoris_router::cloud::EXPOSED_HEADERS
            .iter()
            .chain(["Retry-After", "X-iDoris-Limit-Scope"].iter())
        {
            assert!(
                exposed.contains(&name.to_ascii_lowercase()),
                "{name} not in {exposed:?}"
            );
        }
        assert_eq!(
            reply.header("access-control-allow-origin"),
            Some("https://app.example.com")
        );
        assert_eq!(reply.header("vary"), Some("Origin"));
        assert!(reply.header("access-control-allow-credentials").is_none());
    };

    // 成功的响应
    let ok = w
        .send(&Req::chat(kid1).header("origin", "https://app.example.com"))
        .await;
    assert_eq!(ok.status, 200);
    assert_browser_can_read(&ok);
    // 被拒的响应也要带：浏览器里的 JS 读不到 401 / 403 / 429 的头，就没法给孩子看对的文案
    let unauthorized = w
        .send(
            &Req::chat(kid1)
                .bearer_raw(Some(&unknown_secret(1)))
                .header("origin", "https://app.example.com"),
        )
        .await;
    assert_eq!(unauthorized.status, 401);
    assert_browser_can_read(&unauthorized);
    let not_found = w
        .send(
            &Req::chat(kid1)
                .uri("/nope")
                .header("origin", "https://app.example.com"),
        )
        .await;
    assert_eq!(not_found.status, 404);
    assert_browser_can_read(&not_found);

    // 预检
    let preflight = w
        .send(
            &Req::chat(kid1)
                .method("OPTIONS")
                .bearer_raw(None)
                .header("origin", "https://app.example.com")
                .header("access-control-request-method", "POST")
                .header(
                    "access-control-request-headers",
                    "authorization, content-type",
                ),
        )
        .await;
    assert_eq!(preflight.status, 204);
    assert_eq!(
        preflight.header("access-control-allow-origin"),
        Some("https://app.example.com")
    );
    assert_eq!(
        preflight.header("access-control-allow-headers"),
        Some("authorization, content-type")
    );
    assert_eq!(preflight.header("access-control-max-age"), Some("300"));

    // 不在名单里的来源：不给任何 CORS 头，预检直接 403
    for origin in [
        "https://evil.example.net",
        "https://app.example.com.evil.net",
        "null",
    ] {
        let reply = w.send(&Req::chat(kid1).header("origin", origin)).await;
        assert_eq!(
            reply.status, 200,
            "the request itself is fine; the browser blocks the response"
        );
        // 没有 Allow-Origin，浏览器就会拦住这个响应（处理函数自己写的 Expose-Headers 单独存在不起任何作用）
        assert!(
            reply.header("access-control-allow-origin").is_none(),
            "{origin}"
        );
        let preflight = w
            .send(
                &Req::chat(kid1)
                    .method("OPTIONS")
                    .bearer_raw(None)
                    .header("origin", origin)
                    .header("access-control-request-method", "POST"),
            )
            .await;
        assert_eq!(preflight.status, 403, "{origin}");
        assert_eq!(preflight.code(), "origin_not_allowed");
        assert!(preflight.header("access-control-allow-origin").is_none());
    }
}

// ---------------------------------------------------------------------------
// 一致性检查
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ingress_refuses_a_cloud_upstream_that_still_has_a_static_caller_book() {
    let upstream = FakeUpstream::start(vec![]).await;
    let endpoints = vec![Endpoint::child_edu_compliant(
        "example-alpha",
        upstream.url(),
    )];
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("upstream.key");
    std::fs::write(&key_file, "sk-test\n").unwrap();
    std::fs::write(
        dir.path().join("permits.yaml"),
        h::base::permit_table_yaml(&endpoints),
    )
    .unwrap();
    // 夹具默认有一个静态调用方：S4 的测试替身
    let yaml = h::base::config_yaml(
        &endpoints,
        key_file.to_str().unwrap(),
        &CloudOptions::default(),
    );
    let parts = idoris_router::cloud::config::parse(&yaml, dir.path(), ts()).unwrap();
    let db = dir.path().join("db.sqlite3");
    let accounts = std::sync::Arc::new(idoris_tenancy::accounts::AccountStore::open(&db).unwrap());
    let keys = std::sync::Arc::new(std::sync::Mutex::new(
        idoris_tenancy::virtual_key::store::VirtualKeyStore::open(&db).unwrap(),
    ));
    let attempts = std::sync::Arc::new(idoris_tenancy::attempts::AttemptStore::open(&db).unwrap());
    let clock = std::sync::Arc::new(idoris_router::cloud::ManualClock::new(ts()));
    let cloud = idoris_router::cloud::CloudUpstream::new(
        parts,
        attempts,
        clock.clone(),
        std::sync::Arc::new(idoris_router::cloud::LedgerAdmission::new(accounts.clone())),
    );
    let config =
        idoris_router::cloud::ingress::IngressConfig::parse(&IngressYaml::default().render())
            .unwrap();
    let error = idoris_router::cloud::CloudIngress::new(
        config,
        accounts,
        idoris_router::auth::VirtualKeyAuthenticator::new(keys),
        clock,
        &cloud,
    )
    .unwrap_err();
    assert!(error.contains("callers:"), "{error}");
}
