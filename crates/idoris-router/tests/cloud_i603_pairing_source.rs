//! 计算服务 issue #603 的 N3：配对兑换的失败限速按来源分桶（云形态：控制面挂在公网监听上，边缘是 `control_edge`）。
//!
//! 限速原来是全局的（一分钟 20 次、鉴权前）：一个来源每分钟约 20 个垃圾码，所有租户的合法配对就都拿 429。现在每个来源有自己的
//! 份额，另有一个远高于单来源上限的全局总闸。来源的取法和云入口的来源软配额是同一套：只信任受信代理的 `X-Forwarded-For`，
//! 没有受信代理时只用 TCP 对端地址。整条路径是真的：HTTP -> 云入口最外层 -> `control_edge` -> 控制面 -> 账户库。
//! 每个场景在修复之前的代码上都会失败。
//!
//! | 场景 | 断言 |
//! |---|---|
//! | 受信代理后面的两个真实客户端 | 各有各的失败份额：一个用满，另一个的合法码照常兑换 |
//! | 直连客户端伪造 `X-Forwarded-For` | 不受信：换头不换份额 |
//! | 没有受信代理 | 来源只是 TCP 对端，头永远被忽略 |
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ingress_harness.rs"]
mod h;

use h::*;
use serde_json::json;

/// 用 eduai 的管理凭证（经公网控制面）给 kid-1 的成员关系创建一个配对码，返回明文码。
async fn pairing_code(w: &World) -> String {
    let created = w
        .send(
            &Req::chat(w.cred("svc_eduai_admin"))
                .uri("/control/v1/pairing/codes")
                .body(json!({"membership": "m-kid-1", "allowed_privacy": ["local_only"], "allowed_roles": ["fast"]})),
        )
        .await;
    assert_eq!(created.status, 201, "{}", created.text());
    created.json()["data"]["code"].as_str().unwrap().to_string()
}

/// 兑换请求：没有凭证（码本身就是凭据），从 TCP 对端 `peer` 发来，可以带 `X-Forwarded-For`。
fn redeem_req(w: &World, peer: &str, forwarded: Option<&str>, code: &str) -> Req {
    let mut request = Req::chat(w.cred("svc_eduai"))
        .bearer_raw(None)
        .uri("/control/v1/pairing/redeem")
        .body(json!({"code": code, "device_label": "laptop"}))
        .peer(peer);
    if let Some(forwarded) = forwarded {
        request = request.header("x-forwarded-for", forwarded);
    }
    request
}

fn wrong_code(n: u32) -> String {
    let digits = format!("{n:012}");
    format!("{}-{}-{}", &digits[0..4], &digits[4..8], &digits[8..12])
}

#[tokio::test]
async fn behind_a_trusted_proxy_each_forwarded_client_has_its_own_pairing_failure_budget() {
    let w =
        World::new(WorldOptions::plenty().ingress(IngressYaml::default().trusted(&["10.0.0.0/8"])))
            .await;
    let code = pairing_code(&w).await;
    // 反向代理（10.0.0.2，受信）后面的两个真实客户端。吵闹的那个把自己的份额用完
    let (noisy, honest) = ("198.51.100.7", "198.51.100.8");
    for n in 0..20 {
        let reply = w
            .send(&redeem_req(&w, "10.0.0.2", Some(noisy), &wrong_code(n)))
            .await;
        assert_eq!(
            (reply.status.as_u16(), reply.code().as_str()),
            (400, "pairing_code_invalid"),
            "attempt {n}"
        );
    }
    let blocked = w
        .send(&redeem_req(&w, "10.0.0.2", Some(noisy), &wrong_code(99)))
        .await;
    assert_eq!(
        (blocked.status.as_u16(), blocked.code().as_str()),
        (429, "pairing_rate_limited")
    );
    assert!(blocked.header("retry-after").is_some());
    // 同一个反向代理后面的另一个客户端：它的合法码照常兑换（桶的键是转发来的客户端地址，不是代理的地址）
    let reply = w
        .send(&redeem_req(&w, "10.0.0.2", Some(honest), &code))
        .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
    assert!(reply.json()["data"]["secret"].as_str().is_some());
}

#[tokio::test]
async fn a_direct_client_cannot_buy_a_fresh_pairing_budget_by_forging_forwarded_for() {
    // 10.0.0.0/8 是受信代理；203.0.113.50 不是：它自己写的 X-Forwarded-For 一律不信，来源就是它自己的对端地址
    let w =
        World::new(WorldOptions::plenty().ingress(IngressYaml::default().trusted(&["10.0.0.0/8"])))
            .await;
    let code = pairing_code(&w).await;
    for round in 0..30_u32 {
        let forged = format!("198.51.100.{}", round + 1);
        let reply = w
            .send(&redeem_req(
                &w,
                "203.0.113.50",
                Some(&forged),
                &wrong_code(round),
            ))
            .await;
        let expected = if round < 20 { 400 } else { 429 };
        assert_eq!(reply.status.as_u16(), expected, "round {round}");
    }
    // 另一个真实的直连客户端不受影响
    let reply = w.send(&redeem_req(&w, "203.0.113.51", None, &code)).await;
    assert_eq!(reply.status, 201, "{}", reply.text());
}

#[tokio::test]
async fn without_a_trusted_proxy_the_tcp_peer_is_the_pairing_source_whatever_the_header_says() {
    // 受信代理名单是空的：对任何对端都一样，`X-Forwarded-For` 永远被忽略
    let w = World::new(WorldOptions::plenty()).await;
    let code = pairing_code(&w).await;
    for round in 0..22_u32 {
        let forged = format!("198.51.100.{}", round + 1);
        let reply = w
            .send(&redeem_req(
                &w,
                "10.0.0.2",
                Some(&forged),
                &wrong_code(round),
            ))
            .await;
        let expected = if round < 20 { 400 } else { 429 };
        assert_eq!(reply.status.as_u16(), expected, "round {round}");
    }
    let reply = w
        .send(&redeem_req(&w, "10.0.0.3", Some("10.0.0.2"), &code))
        .await;
    assert_eq!(reply.status, 201, "{}", reply.text());
}
