//! 计算服务 S5 端到端：设计稿 §13 的前两条验收，加上「普通 OpenAI 客户端只配地址和密钥就能用」。
//!
//! 整条纵向路径，不是只看「返回了文本」：HTTP 请求 → 云入口（认证、成员 id、付款上下文、租户、限流）→ S4 处理函数 →
//! S3 准入账本 → 假上游 → 结算，断言**三件事**：客户端看到什么；假上游收到了没有；账本和尝试记录里记到了哪个账户。
//! 全部对本地假上游（`127.0.0.1`）进行，不连接任何真实的推理供应商。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use h::*;
use serde_json::json;

// ---------------------------------------------------------------------------
// 普通 OpenAI 客户端：只要地址加密钥
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_plain_openai_client_needs_only_the_address_and_a_member_key() {
    let w = World::new(WorldOptions::plenty()).await;
    let kid1 = w.cred("mem_kid1");

    // GET /api/v1/models：OpenAI 的列表形状，列的是云路由簿里的模型名
    let models = w.send(&Req::models(kid1)).await;
    assert_eq!(models.status, 200, "{}", models.text());
    let body = models.json();
    assert_eq!(body["object"], "list");
    assert_eq!(body["data"][0]["id"], "idoris/agent");
    assert_eq!(body["data"][0]["object"], "model");
    assert!(models.header("x-idoris-record-id").is_some());

    // POST /api/v1/chat/completions：没有 X-iDoris-Tenant、没有 X-iDoris-Member，什么头都没有
    let chat = w.send(&Req::chat(kid1)).await;
    assert_eq!(chat.status, 200, "{}", chat.text());
    assert_eq!(chat.json()["choices"][0]["message"]["content"], "hello");
    assert_eq!(chat.header("x-idoris-tier"), Some("normal"));
    assert_eq!(w.upstream.request_count(), 1);

    // 付款人是成员凭证自己的成员账户 s1；租户 eduai 的数据类别（child_edu）和计费性质（补贴）是从租户推导的
    assert_eq!(w.spent("s1"), ACTUAL_MICROS);
    assert_eq!(w.spent("s2"), 0, "nobody else paid");
    assert_eq!(
        w.request_rows(),
        vec![(
            "s1".to_string(),
            kid1.id.clone(),
            "child_edu".to_string(),
            "subsidized".to_string()
        )]
    );
}

#[tokio::test]
async fn an_agent24_member_is_an_adult_tenant_paying_for_itself() {
    let w = World::new(WorldOptions::plenty()).await;
    let reply = w.send(&Req::chat(w.cred("mem_agent1"))).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(
        w.request_rows(),
        vec![(
            "a1".to_string(),
            w.cred("mem_agent1").id.clone(),
            "adult".to_string(),
            "paid".to_string()
        )]
    );
    // adult 租户可以流式（edu 租户不行，见 security 测试）
    let mut stream_body = simple_body(100);
    stream_body["stream"] = json!(true);
    let streamed = w
        .send(&Req::chat(w.cred("mem_agent1")).body(stream_body))
        .await;
    // 假上游脚本里是普通 JSON 回复，不是 SSE：这里只看云入口和处理函数放行了它（没有被入口挡住，
    // 也没有被「edu 一律非流式」挡住），不看流式内容
    assert_ne!(streamed.status, 401);
    assert_ne!(streamed.status, 403);
    assert_ne!(streamed.status, 429);
    assert_ne!(streamed.json()["error"]["type"], "streaming_not_allowed");
}

// ---------------------------------------------------------------------------
// §13 第 1 条：开户 -> 发放额度 -> 调用 -> 撤销 -> 撤销后调用被拒
// ---------------------------------------------------------------------------

#[tokio::test]
async fn open_account_grant_allowance_call_revoke_then_the_call_is_refused() {
    let w = World::new(WorldOptions::plenty()).await;
    let admin = w.cred("svc_eduai_admin").clone();
    let control = |uri: &str, body: serde_json::Value| Req::chat(&admin).uri(uri).body(body);

    // 1. 开户：eduai 的后端（带 admin_scopes 的 Service 凭证）经公网控制面给学校 B 开一个新成员的账户、登记主体、建成员关系
    for (uri, body) in [
        (
            "/control/v1/accounts",
            json!({"account_id": "s-new", "kind": "member", "parent": "school-b", "unit": "THB"}),
        ),
        (
            "/control/v1/principals",
            json!({"principal_id": "kid-new", "org": "school-b"}),
        ),
        (
            "/control/v1/memberships",
            json!({"membership_id": "m-kid-new", "principal_id": "kid-new", "org": "school-b",
                   "role": "member", "account": "s-new"}),
        ),
    ] {
        let reply = w.send(&control(uri, body)).await;
        assert_eq!(reply.status, 201, "{uri}: {}", reply.text());
    }

    // 2. 发放额度：学校 B 给这个新成员 2 泰铢的授权上限（授权上限，不是转移余额）
    let grant = w
        .send(&control(
            "/control/v1/allocations",
            json!({"allocation_id": "g-new", "parent": "school-b", "child": "s-new",
                   "amount": {"unit": "THB", "micros": 2_000_000}, "purpose": "prepaid",
                   "valid_from_ms": NOW_MS - DAY_MS, "valid_until_ms": NOW_MS + 30 * DAY_MS}),
        ))
        .await;
    assert_eq!(grant.status, 201, "{}", grant.text());

    // 3. 签发这个成员自己的凭证（响应里的明文密钥只出现一次）
    let issued = w
        .send(&control(
            "/control/v1/credentials",
            json!({"binding": {"kind": "member", "membership": "m-kid-new"},
                   "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]}),
        ))
        .await;
    assert_eq!(issued.status, 201, "{}", issued.text());
    let kid_new = Cred {
        id: issued.json()["data"]["credential"]["id"]
            .as_str()
            .unwrap()
            .to_string(),
        secret: issued.json()["data"]["secret"]
            .as_str()
            .unwrap()
            .to_string(),
    };

    // 4. 调用：用这张凭证，普通 OpenAI 客户端的样子（只有地址和密钥）。钱记在新开的成员账户上。
    let call = w.send(&Req::chat(&kid_new)).await;
    assert_eq!(call.status, 200, "{}", call.text());
    assert_eq!(w.upstream.request_count(), 1);
    let rows = w.request_rows();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "s-new", "the freshly opened account paid");
    assert_eq!(rows[0].1, kid_new.id);
    assert_eq!(rows[0].2, "child_edu");
    // 额度用掉了实际价（授权上限从 2_000_000 减去 1320）
    let view = w.accounts.account_view(&aid("s-new"), ts()).unwrap();
    assert_eq!(
        view.ceiling_remaining.unwrap().micros,
        2_000_000 - ACTUAL_MICROS
    );

    // 5. 撤销：凭证被撤销
    let revoke = w
        .send(&control(
            &format!("/control/v1/credentials/{}/status", kid_new.id),
            json!({"status": "revoked"}),
        ))
        .await;
    assert_eq!(revoke.status, 200, "{}", revoke.text());

    // 6. 撤销之后：下一次调用就被拒，而且没有出网、没有任何新的账
    let refused = w.send(&Req::chat(&kid_new)).await;
    assert_eq!(refused.status, 401, "{}", refused.text());
    assert_eq!(
        w.upstream.request_count(),
        1,
        "a revoked credential never reaches the upstream"
    );
    assert_eq!(w.request_rows().len(), 1);
    assert_eq!(
        w.accounts
            .account_view(&aid("s-new"), ts())
            .unwrap()
            .ceiling_remaining
            .unwrap()
            .micros,
        2_000_000 - ACTUAL_MICROS
    );
    // 被拒的原因不透明：和一个根本不存在的密钥是同一个答案
    let unknown = w
        .send(&Req::chat(&kid_new).bearer_raw(Some(&unknown_secret(7))))
        .await;
    assert_eq!(refused.json(), unknown.json());
}

#[tokio::test]
async fn consent_withdrawal_stops_the_very_next_request_for_service_and_member_credentials_alike() {
    let w = World::new(WorldOptions::plenty()).await;
    let svc = w.cred("svc_eduai");
    let kid1 = w.cred("mem_kid1");
    assert_eq!(w.send(&Req::chat(svc).member("kid-1")).await.status, 200);
    assert_eq!(w.send(&Req::chat(kid1)).await.status, 200);
    assert_eq!(w.upstream.request_count(), 2);

    // eduai 的后端撤销这个孩子的处理资格（经控制面）
    let admin = w.cred("svc_eduai_admin");
    let revoked = w
        .send(
            &Req::chat(admin)
                .uri("/control/v1/principals/kid-1/revoke")
                .body(json!({"org": "school-a"})),
        )
        .await;
    assert_eq!(revoked.status, 200, "{}", revoked.text());

    // Service 凭证（后端）：403，稳定的原因码，后端据此给孩子看对应的文案
    let by_service = w.send(&Req::chat(svc).member("kid-1")).await;
    assert_eq!(by_service.status, 403, "{}", by_service.text());
    assert_eq!(by_service.code(), "principal_revoked");
    assert_eq!(by_service.reason(), Some("principal_revoked"));
    // 成员凭证：凭证本身随主体一起失效，是不透明的 401（数据面闸门）
    assert_eq!(w.send(&Req::chat(kid1)).await.status, 401);
    // 没有再出网，没有新的账；别的孩子不受影响
    assert_eq!(w.upstream.request_count(), 2);
    assert_eq!(w.send(&Req::chat(svc).member("kid-2")).await.status, 200);
    assert_eq!(w.spent("s2"), ACTUAL_MICROS);
}

// ---------------------------------------------------------------------------
// §13 第 2 条：同一个主体属于两个组织，每次请求的付款上下文都唯一且正确
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_principal_in_two_orgs_is_charged_to_the_right_account_on_every_request() {
    let w = World::new(WorldOptions::plenty()).await;
    let a = w.cred("svc_school_a");
    let b = w.cred("svc_school_b");

    // 同一个主体 shared：学校 A 的账户 sh-a、学校 B 的账户 sh-b。两张不同的 Service 凭证，交替调用多次。
    let mut expected_a = 0;
    let mut expected_b = 0;
    for round in 0..4 {
        let via_a = w.send(&Req::chat(a).member("shared")).await;
        assert_eq!(via_a.status, 200, "round {round}: {}", via_a.text());
        expected_a += ACTUAL_MICROS;
        assert_eq!(w.spent("sh-a"), expected_a, "round {round}");
        assert_eq!(w.spent("sh-b"), expected_b, "round {round}");

        let via_b = w.send(&Req::chat(b).member("shared")).await;
        assert_eq!(via_b.status, 200, "round {round}: {}", via_b.text());
        expected_b += ACTUAL_MICROS;
        assert_eq!(w.spent("sh-a"), expected_a, "round {round}");
        assert_eq!(w.spent("sh-b"), expected_b, "round {round}");
    }
    // 尝试记录里每个请求的付款账户和凭证对得上，没有一条记到别处
    let rows = w.request_rows();
    assert_eq!(rows.len(), 8);
    for (index, (payer, credential, classes, billing)) in rows.iter().enumerate() {
        let (want_payer, want_credential) = if index % 2 == 0 {
            ("sh-a", &a.id)
        } else {
            ("sh-b", &b.id)
        };
        assert_eq!(payer, want_payer, "request {index}");
        assert_eq!(credential, want_credential, "request {index}");
        assert_eq!(
            (classes.as_str(), billing.as_str()),
            ("child_edu", "subsidized")
        );
    }
    // 两所学校共同的上级 eduai 是同一个租户；别的孩子的账户一分没动
    for untouched in ["s1", "s2", "s3"] {
        assert_eq!(w.spent(untouched), 0, "{untouched}");
    }
}

#[tokio::test]
async fn a_service_credential_that_covers_both_memberships_is_ambiguous_and_refused_not_guessed() {
    let w = World::new(WorldOptions::plenty()).await;
    // svc_eduai 的范围同时包含学校 A 和学校 B：主体 shared 在范围内有两个有效的成员账户
    let reply = w
        .send(&Req::chat(w.cred("svc_eduai")).member("shared"))
        .await;
    assert_eq!(reply.status, 403, "{}", reply.text());
    assert_eq!(reply.code(), "payment_context_invalid");
    assert_eq!(w.upstream.request_count(), 0);
    assert_eq!(w.request_count(), 0);
    assert_eq!((w.spent("sh-a"), w.spent("sh-b")), (0, 0));
}

// ---------------------------------------------------------------------------
// 账本守恒：每一次成功的调用，两张账（账本和尝试记录）对得上
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_ledger_and_the_attempt_records_agree_after_a_mixed_run() {
    let w = World::new(WorldOptions::plenty()).await;
    for (cred, member) in [
        ("svc_eduai", Some("kid-1")),
        ("svc_eduai", Some("kid-2")),
        ("svc_school_b", Some("kid-3")),
        ("svc_agent24", Some("agent-1")),
        ("mem_agent2", None),
        ("node_agent24", None),
    ] {
        let mut request = Req::chat(w.cred(cred));
        if let Some(member) = member {
            request = request.member(member);
        }
        assert_eq!(w.send(&request).await.status, 200, "{cred}");
    }
    // 几个被拒的请求
    assert_eq!(w.send(&Req::chat(w.cred("svc_eduai"))).await.status, 403);
    assert_eq!(
        w.send(&Req::chat(w.cred("svc_eduai")).member("agent-1"))
            .await
            .status,
        403
    );
    let report = w.accounts.reconcile_with_attempts().unwrap();
    assert!(report.is_clean(), "{:#?}", report.mismatches);
    assert_eq!(w.request_count(), 6);
    assert_eq!(w.upstream.request_count(), 6);
}
