//! 计算服务 S6：edu 租户关闭代理响应缓存（设计稿 §7：「edu 租户**关闭响应缓存**」；代码勘察 §6：旧代理缓存存完整 body，
//! 原来没有任何开关）。
//!
//! 旧的 `ChatProxy` 把成功的缓冲响应（完整 body）放进进程内缓存和飞行表，窗口 60 秒，用来做幂等重放。
//! 关闭之后，这些租户的请求不读缓存、不写缓存、不登记重放：同一个 `X-iDoris-Request-Id` 发两次，上游就收到两次。
//! 其它租户的缓存行为一字不变（对照组）。全部对本地假上游进行，**不连接任何真实的推理供应商**。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;

use fake_upstream::{Behavior, FakeUpstream, completion, usage};
use idoris_contracts::common::PrivacyClass;
use idoris_contracts::provider::Locality;
use idoris_router::proxy::{ChatProxy, ForwardOpts};
use serde_json::json;

/// 对 `tenant` 发两次同一个 request id 的请求。返回 (上游收到几次, 第二次是不是缓存命中)。
async fn twice(tenant: Option<&str>, uncached: &[&str]) -> (usize, bool) {
    let upstream = FakeUpstream::start(vec![
        Behavior::json(200, completion("one", "REPLY-ONE", Some(usage(12, 30)))),
        Behavior::json(200, completion("two", "REPLY-TWO", Some(usage(12, 30)))),
    ])
    .await;
    let proxy = ChatProxy::new(idoris_upstream::http_client().unwrap());
    proxy.disable_cache_for_tenants(uncached.iter().map(|scope| (*scope).to_string()));
    let endpoint = format!("http://{}", upstream.addr());
    let body = json!({"model": "m", "messages": [{"role": "user", "content": "hi"}]});
    let mut last = None;
    for record in ["rec-1", "rec-2"] {
        let opts = ForwardOpts {
            request_id: Some("rid-same"),
            tenant_id: tenant,
            record_id: record,
            provider_id: "example-provider",
            served_locality: Locality::Loopback,
            privacy: PrivacyClass::Any,
            require_openai_usage: false,
        };
        let outcome = proxy.forward_buffered(&endpoint, &body, &opts).await;
        assert_eq!(outcome.status, 200, "{record}");
        last = Some(outcome);
    }
    (upstream.request_count(), last.unwrap().cached)
}

#[tokio::test]
async fn the_cache_still_replays_for_tenants_it_was_not_disabled_for() {
    // 对照组：没有关闭任何租户时，第二次是缓存命中，上游只收到一次
    assert_eq!(twice(Some("adult"), &[]).await, (1, true));
    // 关闭的是别的租户：这个租户照旧
    assert_eq!(twice(Some("adult"), &["edu"]).await, (1, true));
    assert_eq!(twice(None, &["edu"]).await, (1, true));
}

#[tokio::test]
async fn a_disabled_tenant_is_never_cached_and_never_replayed() {
    // 同一个 request id 发两次：上游收到两次，第二次不是缓存命中（响应正文没有在代理里留存）
    assert_eq!(twice(Some("edu"), &["edu"]).await, (2, false));
    // 没有租户的默认调用方用 `\0personal` 这个范围
    assert_eq!(twice(None, &["\u{0}personal"]).await, (2, false));
    // 只增不减：再关闭一次别的租户不会重新打开
    assert_eq!(twice(Some("edu"), &["edu", "other"]).await, (2, false));
}
