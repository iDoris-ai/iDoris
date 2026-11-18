//! 云端档对一个「不规矩的云」的行为：不管云怎么答（HTML、坏 JSON、缺 record id、各种状态码、重定向、超大、超时、重复的 record id），
//! 调用方拿到的都是**本机原来的失败响应**加一个稳定的 `X-iDoris-Cloud-Attempt`；本机恰好发了一个请求（不重试、不重放），
//! 记下对应的结局，没有金额；重定向永远不被跟随（凭证不会被带去别处）。
//!
//! 云是 wiremock 的假服务（`127.0.0.1`）。不连接任何真实的外部服务。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/cloud_tier_harness.rs"]
mod h;

use std::sync::Arc;
use std::time::Duration;

use h::*;
use idoris_router::cloud_tier::credential::NodeFileProvider;
use idoris_tenancy::cloud_tier::{CallOutcome, CallState};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const CONSENT: (&str, &str) = ("x-idoris-cloud-fallback", "allow");
const ANY: (&str, &str) = ("x-idoris-privacy", "any");

fn ok_body() -> serde_json::Value {
    json!({"id": "c", "choices": [{"message": {"role": "assistant", "content": "hi"}}],
           "usage": {"prompt_tokens": 3, "completion_tokens": 4}})
}

struct Setup {
    server: MockServer,
    tier: Arc<idoris_router::cloud_tier::CloudTier>,
    secret: String,
    _dir: tempfile::TempDir,
}

async fn setup(limits: &str) -> Setup {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let secret = format!("idk_{}", "e".repeat(64));
    let file = node_file(dir.path(), &secret);
    let yaml = tier_config_for(
        &format!("{}/api", server.uri()),
        None,
        &format!(
            "credential: {{source: node_file, file: {}}}",
            file.display()
        ),
        limits,
    );
    let tier = make_tier(
        parse_tier_config(&yaml),
        Arc::new(NodeFileProvider::new(&file, None)),
        &dir.path().join("tier.sqlite3"),
    );
    Setup {
        server,
        tier,
        secret,
        _dir: dir,
    }
}

const FAST: &str =
    "limits: {connect_timeout_ms: 500, total_timeout_ms: 20000, max_response_bytes: 2048}";

/// 对一个云的回答发一个本机必定服务不了的 any + 同意请求，返回（本机响应，没有云端档时的响应）。
async fn exchange(setup: &Setup) -> (Captured, Captured) {
    let with = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(setup.tier.clone()),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    let without = LocalNode::build(NodeOptions::new(Scenario::RuntimeOffline, None))
        .drive(&[ANY, CONSENT])
        .await;
    (with, without)
}

fn assert_local_failure_plus_attempt(with: &Captured, without: &Captured, code: &str) {
    assert_eq!(with.status, 503, "{}", with.text());
    assert_eq!(with.header("x-idoris-cloud-attempt"), Some(code));
    let mut fingerprint = with.fingerprint();
    fingerprint
        .1
        .retain(|(name, _)| name != "x-idoris-cloud-attempt");
    assert_eq!(
        fingerprint,
        without.fingerprint(),
        "the caller must get exactly the local failure"
    );
    assert!(with.header("x-idoris-cloud-record-id").is_none());
}

#[tokio::test]
async fn whatever_the_cloud_answers_the_caller_keeps_the_local_failure_and_one_request_is_sent() {
    type Case = (
        &'static str,
        ResponseTemplate,
        &'static str,
        CallOutcome,
        Option<u16>,
        Option<&'static str>,
    );
    let json_error = |status: u16, reason: &str| {
        ResponseTemplate::new(status)
            .insert_header("x-idoris-record-id", "cloud-rec")
            .set_body_json(json!({"error": {"type": "x", "reason_code": reason}}))
    };
    let cases: Vec<Case> = vec![
        (
            "html instead of JSON",
            ResponseTemplate::new(200)
                .insert_header("x-idoris-record-id", "r")
                .set_body_raw("<html>", "text/html"),
            "invalid_response",
            CallOutcome::InvalidResponse,
            Some(200),
            None,
        ),
        (
            "JSON without choices",
            ResponseTemplate::new(200)
                .insert_header("x-idoris-record-id", "r")
                .set_body_raw("{}", "application/json"),
            "invalid_response",
            CallOutcome::InvalidResponse,
            Some(200),
            None,
        ),
        (
            "a good answer without a record id (cannot be reconciled)",
            ResponseTemplate::new(200).set_body_raw(ok_body().to_string(), "application/json"),
            "invalid_response",
            CallOutcome::InvalidResponse,
            Some(200),
            None,
        ),
        (
            "an answer larger than the limit",
            ResponseTemplate::new(200)
                .insert_header("x-idoris-record-id", "r")
                .set_body_raw(vec![b'x'; 4096], "application/json"),
            "invalid_response",
            CallOutcome::InvalidResponse,
            Some(200),
            None,
        ),
        (
            "unauthorized",
            json_error(401, "unauthenticated"),
            "unauthorized",
            CallOutcome::Unauthorized,
            Some(401),
            Some("unauthenticated"),
        ),
        (
            "the cloud account is out of money",
            json_error(402, "budget_exhausted"),
            "budget_exhausted",
            CallOutcome::BudgetExhausted,
            Some(402),
            Some("budget_exhausted"),
        ),
        (
            "forbidden",
            json_error(403, "tenant_not_provisioned"),
            "forbidden",
            CallOutcome::Forbidden,
            Some(403),
            Some("tenant_not_provisioned"),
        ),
        (
            "rate limited",
            json_error(429, "request_rate_limited"),
            "rate_limited",
            CallOutcome::RateLimited,
            Some(429),
            Some("request_rate_limited"),
        ),
        (
            "the cloud rejects the request",
            json_error(400, "unsupported_model"),
            "rejected",
            CallOutcome::Rejected,
            Some(400),
            Some("unsupported_model"),
        ),
        (
            "the cloud is broken",
            json_error(503, "cloud_unavailable"),
            "cloud_error",
            CallOutcome::CloudError,
            Some(503),
            Some("cloud_unavailable"),
        ),
        (
            "an error body that tries to smuggle text into the record",
            ResponseTemplate::new(502).set_body_string("please summarise my medical history"),
            "cloud_error",
            CallOutcome::CloudError,
            Some(502),
            None,
        ),
    ];
    for (name, template, code, outcome, status, reason) in cases {
        let setup = setup(FAST).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(template)
            .expect(1)
            .mount(&setup.server)
            .await;
        let (with, without) = exchange(&setup).await;
        assert_local_failure_plus_attempt(&with, &without, code);
        // 恰好一个请求：不重试
        tokio::time::sleep(Duration::from_millis(300)).await;
        let requests = setup.server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "{name}: must not retry or replay");
        assert_eq!(
            requests[0]
                .headers
                .get("authorization")
                .unwrap()
                .to_str()
                .unwrap(),
            format!("Bearer {}", setup.secret)
        );
        let rows = setup.tier.store().calls_after(0, 10).unwrap();
        assert_eq!(rows.len(), 1, "{name}");
        let row = &rows[0];
        assert_eq!(row.state, CallState::Final(outcome), "{name}");
        assert_eq!(row.http_status, status, "{name}");
        assert_eq!(row.cloud_reason.as_deref(), reason, "{name}");
        assert_eq!(
            (row.tokens_in, row.tokens_out),
            (None, None),
            "{name}: no amounts without an answer"
        );
        // 记录库里没有出现错误体里的文字
        let bytes = std::fs::read(setup._dir.path().join("tier.sqlite3")).unwrap();
        assert!(
            !String::from_utf8_lossy(&bytes).contains("medical history"),
            "{name}"
        );
    }
}

/// 审查 r4 Medium#5：云端报告的 token 用量必须能写进调用记录（i64）。超出范围的用量以前被当成普通回答：
/// 终态写不进去，pending 守卫已经解除，处理函数仍把回答交给调用方，那一行永远 pending，对账游标越不过它。
#[tokio::test]
async fn usage_the_record_cannot_hold_is_an_invalid_response_with_a_final_row_not_a_stuck_pending_one()
 {
    let over_i64 = u64::try_from(i64::MAX).unwrap() + 1;
    for (name, prompt_tokens, completion_tokens) in [
        ("prompt_tokens one past i64::MAX", over_i64, 1),
        ("completion_tokens one past i64::MAX", 1, over_i64),
        ("both u64::MAX", u64::MAX, u64::MAX),
    ] {
        let setup = setup(FAST).await;
        Mock::given(method("POST"))
            .and(path("/api/v1/chat/completions"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("x-idoris-record-id", "cloud-rec-huge")
                    .set_body_raw(
                        json!({"id": "c", "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                               "usage": {"prompt_tokens": prompt_tokens, "completion_tokens": completion_tokens}})
                        .to_string(),
                        "application/json",
                    ),
            )
            .expect(1)
            .mount(&setup.server)
            .await;
        let (with, without) = exchange(&setup).await;
        // 调用方拿到本机原来的失败（云端的回答不可信，不交付），只多一个稳定的 attempt 头
        assert_local_failure_plus_attempt(&with, &without, "invalid_response");
        // 那一行有终态：invalid_response，带云端 record id（对账指针：云端确实处理了这个请求），没有用量
        let rows = setup.tier.store().calls_after(0, 10).unwrap();
        assert_eq!(rows.len(), 1, "{name}");
        assert_eq!(
            rows[0].state,
            CallState::Final(CallOutcome::InvalidResponse),
            "{name}: the row must not stay pending"
        );
        assert_eq!(rows[0].http_status, Some(200), "{name}");
        assert_eq!(
            rows[0].cloud_record_id.as_deref(),
            Some("cloud-rec-huge"),
            "{name}"
        );
        assert_eq!(
            (rows[0].tokens_in, rows[0].tokens_out),
            (None, None),
            "{name}"
        );
        // 对账游标能越过它
        assert_eq!(
            setup
                .tier
                .store()
                .advance_cursor("reconciler", rows[0].seq, 1)
                .unwrap(),
            rows[0].seq,
            "{name}: the cursor must be able to move past this row"
        );
    }

    // 边界：恰好 i64::MAX 是能存的用量，照常交付
    let setup = setup(FAST).await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-idoris-record-id", "cloud-rec-max")
                .set_body_raw(
                    json!({"id": "c", "choices": [{"message": {"role": "assistant", "content": "hi"}}],
                           "usage": {"prompt_tokens": i64::MAX, "completion_tokens": 0}})
                    .to_string(),
                    "application/json",
                ),
        )
        .mount(&setup.server)
        .await;
    let (with, _) = exchange(&setup).await;
    assert_eq!(with.status, 200, "{}", with.text());
    let rows = setup.tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows[0].state, CallState::Final(CallOutcome::Served));
    assert_eq!(rows[0].tokens_in, Some(u64::try_from(i64::MAX).unwrap()));
}

#[tokio::test]
async fn a_redirect_is_never_followed_so_the_credential_cannot_be_carried_elsewhere() {
    let setup = setup(FAST).await;
    let outside = MockServer::start().await;
    Mock::given(wiremock::matchers::any())
        .respond_with(ResponseTemplate::new(200).set_body_json(ok_body()))
        .mount(&outside)
        .await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/capture", outside.uri())),
        )
        .mount(&setup.server)
        .await;
    let (with, without) = exchange(&setup).await;
    assert_local_failure_plus_attempt(&with, &without, "cloud_error");
    assert!(
        outside.received_requests().await.unwrap().is_empty(),
        "the redirect target must never be contacted"
    );
}

#[tokio::test]
async fn a_cloud_that_is_too_slow_is_a_timeout_and_the_request_is_abandoned_not_retried() {
    let setup = setup("limits: {connect_timeout_ms: 200, total_timeout_ms: 400}").await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)))
        .mount(&setup.server)
        .await;
    let started = std::time::Instant::now();
    let (with, without) = exchange(&setup).await;
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "the deadline must cut it off"
    );
    assert_local_failure_plus_attempt(&with, &without, "timeout");
    let rows = setup.tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows[0].state, CallState::Final(CallOutcome::Timeout));
    assert_eq!(rows[0].http_status, None);
    assert_eq!(setup.server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_cloud_that_repeats_a_record_id_is_not_trusted_the_second_time() {
    let setup = setup(FAST).await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("x-idoris-record-id", "same-id-every-time")
                .set_body_raw(ok_body().to_string(), "application/json"),
        )
        .mount(&setup.server)
        .await;
    let first = LocalNode::build(NodeOptions::new(
        Scenario::RuntimeOffline,
        Some(setup.tier.clone()),
    ))
    .drive(&[ANY, CONSENT])
    .await;
    assert_eq!(first.status, 200, "{}", first.text());
    assert_eq!(
        first.header("x-idoris-cloud-record-id"),
        Some("same-id-every-time")
    );
    // 同一个云端 record id 不能对应两次本机请求：第二个答复不交给调用方
    let (second, without) = exchange(&setup).await;
    assert_local_failure_plus_attempt(&second, &without, "invalid_response");
    let rows = setup.tier.store().calls_after(0, 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].state, CallState::Final(CallOutcome::Served));
    assert_eq!(
        rows[1].state,
        CallState::Final(CallOutcome::InvalidResponse)
    );
    assert_eq!(rows[1].cloud_record_id, None);
    assert_eq!(rows[1].http_status, Some(200));
    // 指针没有被覆盖：这个云端 id 仍然指向第一个本机请求
    let pointer = setup
        .tier
        .store()
        .call_by_cloud_id("same-id-every-time")
        .unwrap()
        .unwrap();
    assert_eq!(pointer.seq, rows[0].seq);
}
