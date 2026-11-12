//! 计算服务 S5：三级上限和公网防滥用。
//!
//! 三级上限（平台 / 租户 / 成员）每一级单独触发；共享出口（同一个 IP、多张凭证）不会被整体封禁；
//! 不受信任来源的 `X-Forwarded-For` 不影响限流归属；请求体上限、慢请求超时、并发名额跟着响应体走。
//! 时间用可拨的时钟（令牌桶的回填），慢请求用真实的短超时。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use std::time::Duration;

use h::*;
use idoris_router::cloud::ingress::LimitKeys;
use serde_json::json;

const HUGE: &str = "{requests_per_minute: 6000000, burst: 1000000, max_concurrent: 100000}";

fn limits(platform: &str, tenant: &str, member: &str) -> WorldOptions {
    WorldOptions {
        script: ok_script(300),
        ingress: IngressYaml::default().limits(platform, tenant, member),
        ..WorldOptions::default()
    }
}

fn keys(tenant: &str, member: &str) -> LimitKeys {
    LimitKeys {
        tenant: tenant.to_string(),
        member: member.to_string(),
    }
}

/// 被限流的答案：429、稳定的原因码、哪一级拦的、至少 1 秒的 Retry-After。
fn assert_limited(reply: &Reply, code: &str, scope: &str) -> u64 {
    assert_eq!(reply.status, 429, "{}", reply.text());
    assert_eq!(reply.code(), code);
    assert_eq!(reply.reason(), Some(code));
    assert_eq!(reply.header("x-idoris-limit-scope"), Some(scope));
    let retry: u64 = reply.header("retry-after").unwrap().parse().unwrap();
    assert!(retry >= 1);
    retry
}

// ---------------------------------------------------------------------------
// 成员级
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_member_rate_limit_triggers_alone_names_its_scope_and_recovers_with_the_clock() {
    // 成员：每分钟 20 个（每 3 秒 1 个），突发 2
    let w = World::new(limits(
        HUGE,
        HUGE,
        "{requests_per_minute: 20, burst: 2, max_concurrent: 100}",
    ))
    .await;
    let kid1 = w.cred("mem_kid1");
    assert_eq!(w.send(&Req::chat(kid1)).await.status, 200);
    assert_eq!(w.send(&Req::chat(kid1)).await.status, 200);

    let limited = w.send(&Req::chat(kid1)).await;
    let retry = assert_limited(&limited, "request_rate_limited", "member");
    assert_eq!(retry, 3);
    // 被限的请求没有出网、没有留下请求记录
    assert_eq!(w.upstream.request_count(), 2);
    assert_eq!(w.request_count(), 2);

    // 同一个租户里的另一个成员完全不受影响
    assert_eq!(w.send(&Req::chat(w.cred("mem_kid2"))).await.status, 200);
    // 同一个成员用另一张凭证（Service 凭证 + 成员 id）仍然是同一个成员桶：它是按（租户，主体）算的，不是按凭证
    let via_service = w
        .send(&Req::chat(w.cred("svc_eduai")).member("kid-1"))
        .await;
    assert_limited(&via_service, "request_rate_limited", "member");

    // 拨时钟：Retry-After 说几秒，几秒之后就过
    w.clock.advance_ms(i64::try_from(retry).unwrap() * 1000 - 1);
    assert_eq!(
        w.send(&Req::chat(kid1)).await.status,
        429,
        "one millisecond early"
    );
    w.clock.advance_ms(1);
    assert_eq!(w.send(&Req::chat(kid1)).await.status, 200);
}

#[tokio::test]
async fn the_member_concurrency_limit_triggers_alone_and_the_slot_comes_back() {
    let slow = Behavior::slow_json(
        200,
        completion("slow", "hello", Some(usage(12, 30))),
        Duration::from_millis(700),
    );
    let w = World::new(WorldOptions {
        script: vec![slow, ok_behavior(), ok_behavior(), ok_behavior()],
        ingress: IngressYaml::default().limits(
            HUGE,
            HUGE,
            "{requests_per_minute: 6000000, burst: 1000000, max_concurrent: 1}",
        ),
        ..WorldOptions::default()
    })
    .await;
    let kid1 = w.cred("mem_kid1").clone();
    let first = w.spawn(Req::chat(&kid1));
    assert!(
        w.upstream
            .wait_for_requests(1, Duration::from_secs(3))
            .await
    );

    // 第一个还在上游路上：三级各占一个名额
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("eduai", "p:kid-1")),
        (1, 1, 1)
    );
    // 同一个成员的第二个并发请求被拒；别的成员不受影响
    let second = w.send(&Req::chat(&kid1)).await;
    assert_limited(&second, "concurrency_limited", "member");
    assert_eq!(w.send(&Req::chat(w.cred("mem_kid2"))).await.status, 200);

    assert_eq!(first.await.unwrap().status, 200);
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("eduai", "p:kid-1")),
        (0, 0, 0)
    );
    // 名额回来了：并发拒绝没有消耗速率令牌，也没有留下请求记录
    assert_eq!(w.send(&Req::chat(&kid1)).await.status, 200);
    assert_eq!(w.request_count(), 3);
}

// ---------------------------------------------------------------------------
// 租户级
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tenant_limit_triggers_alone_when_every_member_is_within_its_own_limit() {
    // 租户：每分钟 60 个，突发 3；成员宽松
    let w = World::new(limits(
        HUGE,
        "{requests_per_minute: 60, burst: 3, max_concurrent: 100000}",
        HUGE,
    ))
    .await;
    let svc = w.cred("svc_eduai");
    for member in ["kid-1", "kid-2", "kid-3"] {
        assert_eq!(
            w.send(&Req::chat(svc).member(member)).await.status,
            200,
            "{member}"
        );
    }
    // 每个成员自己的桶都是满的，但租户的 3 个令牌用完了
    let limited = w.send(&Req::chat(svc).member("kid-1")).await;
    let retry = assert_limited(&limited, "request_rate_limited", "tenant");
    assert_eq!(retry, 1);
    // 另一个租户（agent24）有自己的租户桶
    assert_eq!(w.send(&Req::chat(w.cred("mem_agent1"))).await.status, 200);
    assert_eq!(w.upstream.request_count(), 4);
    // 一秒之后租户桶回填一个
    w.clock.advance_ms(1_000);
    assert_eq!(w.send(&Req::chat(svc).member("kid-2")).await.status, 200);
}

#[tokio::test]
async fn the_tenant_concurrency_limit_counts_every_member_together() {
    let slow = |id: &str| {
        Behavior::slow_json(
            200,
            completion(id, "hello", Some(usage(12, 30))),
            Duration::from_millis(700),
        )
    };
    let w = World::new(WorldOptions {
        script: vec![slow("a"), slow("b"), ok_behavior(), ok_behavior()],
        ingress: IngressYaml::default().limits(
            HUGE,
            "{requests_per_minute: 6000000, burst: 1000000, max_concurrent: 2}",
            HUGE,
        ),
        ..WorldOptions::default()
    })
    .await;
    let svc = w.cred("svc_eduai").clone();
    let a = w.spawn(Req::chat(&svc).member("kid-1"));
    let b = w.spawn(Req::chat(&svc).member("kid-2"));
    assert!(
        w.upstream
            .wait_for_requests(2, Duration::from_secs(3))
            .await
    );
    // 第三个成员：自己没有任何限制，租户并发满了
    let third = w.send(&Req::chat(&svc).member("kid-3")).await;
    assert_limited(&third, "concurrency_limited", "tenant");
    assert_eq!(a.await.unwrap().status, 200);
    assert_eq!(b.await.unwrap().status, 200);
    assert_eq!(w.send(&Req::chat(&svc).member("kid-3")).await.status, 200);
}

// ---------------------------------------------------------------------------
// 平台级
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_platform_limit_triggers_alone_across_tenants() {
    let w = World::new(limits(
        "{requests_per_minute: 60, burst: 2, max_concurrent: 100000}",
        HUGE,
        HUGE,
    ))
    .await;
    assert_eq!(w.send(&Req::chat(w.cred("mem_kid1"))).await.status, 200);
    assert_eq!(w.send(&Req::chat(w.cred("mem_agent1"))).await.status, 200);
    // 第三个请求来自第三个成员、第二个租户：自己的成员桶、租户桶都是满的，平台用完了
    let limited = w.send(&Req::chat(w.cred("mem_agent2"))).await;
    assert_limited(&limited, "request_rate_limited", "platform");
    assert_eq!(w.upstream.request_count(), 2);
    w.clock.advance_ms(1_000);
    assert_eq!(w.send(&Req::chat(w.cred("mem_agent2"))).await.status, 200);
}

#[tokio::test]
async fn the_platform_concurrency_limit_caps_the_whole_node() {
    let slow = Behavior::slow_json(
        200,
        completion("slow", "hello", Some(usage(12, 30))),
        Duration::from_millis(700),
    );
    let w = World::new(WorldOptions {
        script: vec![slow, ok_behavior(), ok_behavior()],
        ingress: IngressYaml::default().limits(
            "{requests_per_minute: 6000000, burst: 1000000, max_concurrent: 1}",
            HUGE,
            HUGE,
        ),
        ..WorldOptions::default()
    })
    .await;
    let first = w.spawn(Req::chat(w.cred("mem_kid1")));
    assert!(
        w.upstream
            .wait_for_requests(1, Duration::from_secs(3))
            .await
    );
    let limited = w.send(&Req::chat(w.cred("mem_agent1"))).await;
    assert_limited(&limited, "concurrency_limited", "platform");
    assert_eq!(first.await.unwrap().status, 200);
    assert_eq!(w.send(&Req::chat(w.cred("mem_agent1"))).await.status, 200);
}

// ---------------------------------------------------------------------------
// 外层的限额不被内层的拒绝偷偷耗光
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_member_that_keeps_hitting_its_own_limit_never_drains_the_tenant_or_the_platform() {
    // 成员突发 1；租户突发 4；平台突发 4
    let w = World::new(limits(
        "{requests_per_minute: 60, burst: 4, max_concurrent: 100000}",
        "{requests_per_minute: 60, burst: 4, max_concurrent: 100000}",
        "{requests_per_minute: 60, burst: 1, max_concurrent: 100000}",
    ))
    .await;
    let svc = w.cred("svc_eduai");
    assert_eq!(w.send(&Req::chat(svc).member("kid-1")).await.status, 200);
    // kid-1 再刷 60 次：每次都在成员级被拒，外层一个令牌都不该被动
    for _ in 0..60 {
        let reply = w.send(&Req::chat(svc).member("kid-1")).await;
        assert_eq!(reply.header("x-idoris-limit-scope"), Some("member"));
    }
    // 租户和平台都还剩 3 个：别的成员可以再用 3 次
    for member in ["kid-2", "kid-3"] {
        assert_eq!(
            w.send(&Req::chat(svc).member(member)).await.status,
            200,
            "{member}"
        );
    }
    assert_eq!(
        w.send(&Req::chat(w.cred("mem_kid2"))).await.status,
        429,
        "kid-2 hit its own burst of 1"
    );
    assert_eq!(w.send(&Req::chat(w.cred("mem_agent1"))).await.status, 200);
    // 此时租户 eduai 用了 3 个（kid-1、kid-2、kid-3），平台用了 4 个：下一个请求被平台拦
    let limited = w.send(&Req::chat(w.cred("mem_agent2"))).await;
    assert_limited(&limited, "request_rate_limited", "platform");
}

// ---------------------------------------------------------------------------
// 共享出口：同一个 IP、多张凭证
// ---------------------------------------------------------------------------

#[tokio::test]
async fn many_credentials_behind_one_shared_ip_are_never_blocked_as_a_group() {
    // 来源软配额小到一个 IP 只能发 1 个「没有凭证」的请求：如果有任何按 IP 的东西碰到带凭证的请求，这里立刻露馅
    let w = World::new(WorldOptions {
        script: ok_script(300),
        ingress: IngressYaml::default()
            .trusted(&["10.0.0.0/8"])
            .abuse(
                "  anonymous: {requests_per_minute: 1, burst: 1}\n  per_source_anonymous: {requests_per_minute: 1, burst: 1}\n  auth_failures: {max_failures: 1, window_secs: 3600}\n",
            ),
        ..WorldOptions::default()
    })
    .await;
    // 一所学校的共享出口：经受信代理转发，客户端地址都是 198.51.100.7
    let school_ip = "198.51.100.7";
    let identities: Vec<Req> = vec![
        Req::chat(w.cred("mem_kid1")),
        Req::chat(w.cred("mem_kid2")),
        Req::chat(w.cred("svc_eduai")).member("kid-1"),
        Req::chat(w.cred("svc_eduai")).member("kid-2"),
        Req::chat(w.cred("svc_school_a")).member("shared"),
        Req::chat(w.cred("svc_school_b")).member("kid-3"),
        Req::chat(w.cred("mem_agent1")),
        Req::chat(w.cred("svc_agent24")).member("agent-2"),
        Req::chat(w.cred("node_agent24")),
    ];
    for round in 0..4 {
        for (index, identity) in identities.iter().enumerate() {
            let request = identity
                .clone()
                .peer("10.0.0.2")
                .header("x-forwarded-for", school_ip);
            let reply = w.send(&request).await;
            assert_eq!(
                reply.status,
                200,
                "round {round}, identity {index}: {}",
                reply.text()
            );
        }
    }
    assert_eq!(w.upstream.request_count(), 36);
}

#[tokio::test]
async fn a_flood_of_bad_credentials_from_a_shared_ip_never_blocks_the_valid_ones_from_that_ip() {
    let w = World::new(WorldOptions {
        script: ok_script(50),
        ingress: IngressYaml::default().trusted(&["10.0.0.0/8"]).abuse(
            "  anonymous: {requests_per_minute: 60, burst: 100}\n  per_source_anonymous: {requests_per_minute: 6, burst: 3}\n  auth_failures: {max_failures: 100, window_secs: 60}\n",
        ),
        ..WorldOptions::default()
    })
    .await;
    let via_school = |request: Req| {
        request
            .peer("10.0.0.2")
            .header("x-forwarded-for", "198.51.100.7")
    };
    let valid = Req::chat(w.cred("mem_kid1"));

    // 一个学生把密钥输错 / 一堆垃圾请求：前 3 个是 401，之后这个来源的软配额用完，变成 429
    let mut statuses = Vec::new();
    for seed in 0..12 {
        let bad = via_school(Req::chat(w.cred("mem_kid1")).bearer_raw(Some(&unknown_secret(seed))));
        statuses.push(w.send(&bad).await.status.as_u16());
    }
    assert_eq!(&statuses[..3], &[401, 401, 401]);
    assert!(
        statuses[3..].iter().all(|status| *status == 429),
        "{statuses:?}"
    );
    let throttled = w
        .send(&via_school(Req::chat(w.cred("mem_kid1")).bearer_raw(None)))
        .await;
    assert_eq!(throttled.status, 429);
    assert_eq!(throttled.code(), "anonymous_rate_limited");
    assert!(throttled.header("retry-after").is_some());

    // 同一个出口后面带着有效凭证的请求：一个都不受影响，不管这个 IP 现在多「脏」
    for round in 0..5 {
        let reply = w.send(&via_school(valid.clone())).await;
        assert_eq!(reply.status, 200, "round {round}: {}", reply.text());
    }
    // 软配额是速率不是封禁：过一会儿，这个 IP 的垃圾请求又能得到 401（而不是永远 429）
    w.clock.advance_ms(10_000);
    let after = w
        .send(&via_school(
            Req::chat(w.cred("mem_kid1")).bearer_raw(Some(&unknown_secret(99))),
        ))
        .await;
    assert_eq!(after.status, 401);
}

#[tokio::test]
async fn auth_failures_are_counted_per_credential_prefix_not_per_ip() {
    let w = World::new(WorldOptions {
        script: ok_script(20),
        ingress: IngressYaml::default().abuse(
            "  anonymous: {requests_per_minute: 6000000, burst: 1000000}\n  per_source_anonymous: {requests_per_minute: 6000000, burst: 1000000}\n  auth_failures: {max_failures: 3, window_secs: 60}\n",
        ),
        ..WorldOptions::default()
    })
    .await;
    let same_ip = "198.51.100.7";
    let from = |request: Req| request.peer(same_ip);
    // 一把写错的密钥（比如已经被撤销的）：连续 3 次 401
    let stale = unknown_secret(0x00ab_cdef);
    for _ in 0..3 {
        let reply = w
            .send(&from(
                Req::chat(w.cred("mem_kid1")).bearer_raw(Some(&stale)),
            ))
            .await;
        assert_eq!(reply.status, 401);
    }
    // 第 4 次：这个前缀被限流，在内存里直接拒绝，带 Retry-After（窗口剩余）
    let throttled = w
        .send(&from(
            Req::chat(w.cred("mem_kid1")).bearer_raw(Some(&stale)),
        ))
        .await;
    assert_eq!(throttled.status, 429, "{}", throttled.text());
    assert_eq!(throttled.code(), "auth_throttled");
    assert_eq!(throttled.header("retry-after"), Some("60"));
    // 换一个 IP，同一把密钥照样被限流：键是凭证前缀，不是来源
    let elsewhere = w
        .send(
            &Req::chat(w.cred("mem_kid1"))
                .bearer_raw(Some(&stale))
                .peer("192.0.2.9"),
        )
        .await;
    assert_eq!(elsewhere.code(), "auth_throttled");
    // 同一个 IP 上另一把写错的密钥（前缀不同）：只是 401，不受那把密钥的影响
    let other_bad = w
        .send(&from(
            Req::chat(w.cred("mem_kid1")).bearer_raw(Some(&unknown_secret(0x0012_3456))),
        ))
        .await;
    assert_eq!(other_bad.status, 401);
    // 同一个 IP 上有效的密钥：照常（共享出口后面的其他学生不受这个学生输错密钥的影响）
    for cred in ["mem_kid1", "mem_kid2", "mem_agent1"] {
        let reply = w.send(&from(Req::chat(w.cred(cred)))).await;
        assert_eq!(reply.status, 200, "{cred}: {}", reply.text());
    }
    // 窗口过去之后，那把密钥又得到 401（继续是 401：它确实是错的）
    w.clock.advance_ms(60_000);
    let later = w
        .send(&from(
            Req::chat(w.cred("mem_kid1")).bearer_raw(Some(&stale)),
        ))
        .await;
    assert_eq!(later.status, 401);
}

#[tokio::test]
async fn a_revoked_credential_in_a_retry_loop_is_throttled_without_touching_the_database_again() {
    let w = World::new(WorldOptions {
        script: ok_script(5),
        ingress: IngressYaml::default().abuse(
            "  anonymous: {requests_per_minute: 6000000, burst: 1000000}\n  per_source_anonymous: {requests_per_minute: 6000000, burst: 1000000}\n  auth_failures: {max_failures: 3, window_secs: 60}\n",
        ),
        ..WorldOptions::default()
    })
    .await;
    let kid2 = w.cred("mem_kid2");
    assert_eq!(w.send(&Req::chat(kid2)).await.status, 200);
    w.accounts
        .set_credential_status(
            &idoris_compute_contracts::ids::CredentialId::new(&kid2.id).unwrap(),
            idoris_compute_contracts::credential::CredentialStatus::Revoked,
            ts(),
        )
        .unwrap();
    for _ in 0..3 {
        assert_eq!(w.send(&Req::chat(kid2)).await.status, 401);
    }
    let throttled = w.send(&Req::chat(kid2)).await;
    assert_eq!(
        (throttled.status.as_u16(), throttled.code()),
        (429, "auth_throttled".to_string())
    );
    assert_eq!(w.upstream.request_count(), 1);
}

// ---------------------------------------------------------------------------
// 来源：只信任配置里列出的反向代理
// ---------------------------------------------------------------------------

fn anonymous_world(
    trusted: &[&str],
    per_source_burst: u32,
) -> impl std::future::Future<Output = World> {
    let abuse = format!(
        "  anonymous: {{requests_per_minute: 6000000, burst: 1000000}}\n  per_source_anonymous: {{requests_per_minute: 60, burst: {per_source_burst}}}\n"
    );
    let mut ingress = IngressYaml::default().abuse(&abuse);
    if !trusted.is_empty() {
        ingress = ingress.trusted(trusted);
    }
    World::new(WorldOptions {
        script: ok_script(5),
        ingress,
        ..WorldOptions::default()
    })
}

#[tokio::test]
async fn an_untrusted_peer_cannot_dodge_its_quota_by_rotating_forged_forwarded_for_values() {
    let w = anonymous_world(&["10.0.0.0/8"], 3).await;
    let kid1 = w.cred("mem_kid1");
    let mut statuses = Vec::new();
    for round in 0..10 {
        // 直接连上来的客户端（不在受信代理名单里）每次伪造一个不同的客户端地址
        let request = Req::chat(kid1)
            .bearer_raw(None)
            .peer("203.0.113.50")
            .header("x-forwarded-for", &format!("198.51.100.{}", round + 1));
        statuses.push(w.send(&request).await.status.as_u16());
    }
    assert_eq!(&statuses[..3], &[401, 401, 401]);
    assert!(statuses[3..].iter().all(|s| *s == 429), "{statuses:?}");
    // 名单是空的：对谁都一样
    let w = anonymous_world(&[], 3).await;
    let mut statuses = Vec::new();
    for round in 0..8 {
        let request = Req::chat(w.cred("mem_kid1"))
            .bearer_raw(None)
            .peer("10.0.0.2")
            .header("x-forwarded-for", &format!("198.51.100.{}", round + 1));
        statuses.push(w.send(&request).await.status.as_u16());
    }
    assert!(statuses[3..].iter().all(|s| *s == 429), "{statuses:?}");
}

#[tokio::test]
async fn a_trusted_proxy_attributes_each_forwarded_client_its_own_quota() {
    let w = anonymous_world(&["10.0.0.0/8"], 2).await;
    let kid1 = w.cred("mem_kid1");
    let via_proxy = |client: &str| {
        Req::chat(kid1)
            .bearer_raw(None)
            .peer("10.0.0.2")
            .header("x-forwarded-for", &format!("1.1.1.1, {client}"))
    };
    // 三个不同的客户端，各自两个请求都是 401（没有凭证）；客户端自己在链最左边伪造的 1.1.1.1 没有任何作用
    for client in ["198.51.100.1", "198.51.100.2", "198.51.100.3"] {
        for _ in 0..2 {
            assert_eq!(w.send(&via_proxy(client)).await.status, 401, "{client}");
        }
    }
    // 同一个客户端的第三个请求：它自己的配额用完了
    assert_eq!(w.send(&via_proxy("198.51.100.1")).await.status, 429);
    assert_eq!(w.send(&via_proxy("198.51.100.4")).await.status, 401);
}

// ---------------------------------------------------------------------------
// 请求体上限、慢请求
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_oversized_body_is_refused_whether_declared_or_streamed_and_costs_nothing() {
    let w = World::new(WorldOptions {
        script: ok_script(5),
        ingress: IngressYaml::default().abuse("  body_max_bytes: 2048\n"),
        ..WorldOptions::default()
    })
    .await;
    let kid1 = w.cred("mem_kid1");
    // 声明了 Content-Length：一个字节都不用读就拒绝
    let declared = w
        .send(
            &Req::chat(kid1)
                .raw_body(vec![b'x'; 10])
                .header("content-length", "5000"),
        )
        .await;
    assert_eq!(declared.status, 413, "{}", declared.text());
    assert_eq!(declared.code(), "request_body_too_large");
    // 没有 Content-Length（分块上传）：边读边数，超限就拒绝
    let streamed = w.send(&Req::chat(kid1).raw_body(vec![b'x'; 5000])).await;
    assert_eq!(streamed.status, 413);
    assert_eq!(streamed.code(), "request_body_too_large");
    // 刚好在上限内：照常处理
    let padding = "y".repeat(1500);
    let fits = w
        .send(&Req::chat(kid1).body(json!({
            "model": "idoris/agent",
            "messages": [{"role": "user", "content": padding}],
            "max_tokens": 100,
        })))
        .await;
    assert_eq!(fits.status, 200, "{}", fits.text());
    assert_eq!(w.upstream.request_count(), 1);
    // 被拒的请求不占任何名额：三级的并发都是 0
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("eduai", "p:kid-1")),
        (0, 0, 0)
    );
}

#[tokio::test]
async fn a_slow_body_times_out_and_releases_everything() {
    let w = World::new(WorldOptions {
        script: ok_script(5),
        ingress: IngressYaml::default().abuse("  body_read_timeout_ms: 300\n"),
        ..WorldOptions::default()
    })
    .await;
    let started = std::time::Instant::now();
    let reply = w.send(&Req::chat(w.cred("mem_kid1")).stalled()).await;
    assert_eq!(reply.status, 408, "{}", reply.text());
    assert_eq!(reply.code(), "request_timeout");
    assert!(started.elapsed() < Duration::from_secs(5));
    assert!(started.elapsed() >= Duration::from_millis(250));
    assert_eq!(w.upstream.request_count(), 0);
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("eduai", "p:kid-1")),
        (0, 0, 0)
    );
    // 慢请求没有拖垮这个成员：下一个正常请求照常
    assert_eq!(w.send(&Req::chat(w.cred("mem_kid1"))).await.status, 200);
}

#[tokio::test]
async fn an_unauthenticated_request_is_refused_before_its_body_is_ever_read() {
    // 没有凭证的慢请求：立刻 401，不会为它等 300 毫秒、更不会为它缓冲任何东西
    let w = World::new(WorldOptions {
        script: ok_script(1),
        ingress: IngressYaml::default().abuse("  body_read_timeout_ms: 5000\n"),
        ..WorldOptions::default()
    })
    .await;
    let started = std::time::Instant::now();
    let reply = w
        .send(&Req::chat(w.cred("mem_kid1")).bearer_raw(None).stalled())
        .await;
    assert_eq!(reply.status, 401);
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

// ---------------------------------------------------------------------------
// 并发名额跟着响应体走（流式）
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_streaming_response_keeps_its_concurrency_slot_until_the_stream_is_done() {
    use h::fake_upstream::{done_event, usage_event};
    let stream = Behavior::sse(vec![
        SseStep::Send(content_event("st", "hi")),
        SseStep::Sleep(Duration::from_millis(300)),
        SseStep::Send(usage_event("st", usage(12, 30))),
        SseStep::Send(done_event()),
        SseStep::Finish,
    ]);
    let w = World::new(WorldOptions {
        script: vec![stream, ok_behavior(), ok_behavior()],
        ingress: IngressYaml::default().limits(
            HUGE,
            HUGE,
            "{requests_per_minute: 6000000, burst: 1000000, max_concurrent: 1}",
        ),
        ..WorldOptions::default()
    })
    .await;
    // agent24 是 adult 租户，可以流式
    let agent1 = w.cred("mem_agent1");
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    body["stream_options"] = json!({"include_usage": true});
    let response = {
        use tower::ServiceExt;
        w.app
            .clone()
            .oneshot(Req::chat(agent1).body(body).build())
            .await
            .unwrap()
    };
    assert_eq!(response.status(), 200);
    assert!(
        response.headers()["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/event-stream")
    );
    // 响应头已经发出，流还没有读完：名额还被占着
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("agent24", "p:agent-1")),
        (1, 1, 1)
    );
    let second = w.send(&Req::chat(agent1)).await;
    assert_limited(&second, "concurrency_limited", "member");
    // 把流读完：名额回来
    let finished = collect(response).await;
    assert!(finished.text().contains("[DONE]"), "{}", finished.text());
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("agent24", "p:agent-1")),
        (0, 0, 0)
    );
    assert_eq!(w.send(&Req::chat(agent1)).await.status, 200);
}

#[tokio::test]
async fn dropping_a_response_before_its_stream_ends_releases_the_slot_too() {
    use h::fake_upstream::{done_event, usage_event};
    let stream = Behavior::sse(vec![
        SseStep::Send(content_event("st", "hi")),
        SseStep::Sleep(Duration::from_millis(500)),
        SseStep::Send(usage_event("st", usage(12, 30))),
        SseStep::Send(done_event()),
        SseStep::Finish,
    ]);
    let w = World::new(WorldOptions {
        script: vec![stream, ok_behavior()],
        ingress: IngressYaml::default().limits(
            HUGE,
            HUGE,
            "{requests_per_minute: 6000000, burst: 1000000, max_concurrent: 1}",
        ),
        ..WorldOptions::default()
    })
    .await;
    let agent1 = w.cred("mem_agent1");
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let response = {
        use tower::ServiceExt;
        w.app
            .clone()
            .oneshot(Req::chat(agent1).body(body).build())
            .await
            .unwrap()
    };
    assert_eq!(response.status(), 200);
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("agent24", "p:agent-1")),
        (1, 1, 1)
    );
    // 客户端断开（丢掉响应）：名额立刻回来
    drop(response);
    assert_eq!(
        w.ingress.limiter().in_flight(&keys("agent24", "p:agent-1")),
        (0, 0, 0)
    );
}
