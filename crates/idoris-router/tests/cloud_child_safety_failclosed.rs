//! 计算服务 S6 儿童安全：**失败即关闭**，以及**输出检查在发布之前**。
//!
//! 设计稿 §7：「安全服务超时或出错，一律拒绝放行」；§13：「edu 在安全服务超时时拒绝放行」。
//! 安全服务是一个本地假的 OpenAI 兼容服务（放行、拒绝、超时、5xx、返回垃圾、半截 JSON、慢响应、连接被拒……）：
//! 每一种故障，回答都不会发给客户端，返回稳定原因码，并且**假上游的回答内容没有出现在响应里**。
//! 全部对本地假服务进行，**不连接任何真实的推理或安全服务**。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_safety.rs"]
mod child_safety;
#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use child_safety::{
    ANSWER_SENTINEL, CHILD_SENTINEL, Child, answer, benign_question, child_body, child_rules_only,
    child_with, child_with_judge, contains, harmful_question, judge_allow, judge_block, judge_says,
    judge_yaml,
};
use fake_upstream::{Behavior, FakeUpstream};
use harness::{Caller, Endpoint, Options, build, post};
use idoris_compute_contracts::ids::TimestampMs;
use idoris_compute_contracts::pricing::Cost;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::safety::SafetyStage;
use idoris_router::cloud::safety::{
    ExtraCostOutcome, ExtraCostRefusal, ExtraCostRequest, ExtraCostTicket,
};
use idoris_router::cloud::{ExtraCostMeter, ManualClock, SimpleExtraCost};
use idoris_tenancy::attempts::RequestOutcome;
use serde_json::json;

fn unavailable(reply: &harness::Reply, stage: &str, cause: &str) {
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "safety_unavailable");
    assert_eq!(reply.json()["error"]["reason_code"], "safety_unavailable");
    assert_eq!(reply.json()["error"]["stage"], stage, "{}", reply.text());
    assert_eq!(reply.json()["error"]["cause"], cause, "{}", reply.text());
    assert_eq!(reply.header("x-idoris-reason"), Some("safety_unavailable"));
}

/// 没有任何内容泄露给客户端：回答的哨兵、孩子的哨兵都不在响应里。
fn assert_no_content(reply: &harness::Reply) {
    assert!(!contains(&reply.body, ANSWER_SENTINEL), "the answer leaked");
    assert!(
        !contains(&reply.body, CHILD_SENTINEL),
        "the child's text leaked"
    );
    assert!(
        reply.json().get("choices").is_none(),
        "no completion body of any kind: {}",
        reply.text()
    );
}

/// 这次请求没有向用户收钱：没有发布，没有 user_price 分录。
fn assert_nothing_published_nothing_charged(c: &Child, reply: &harness::Reply) {
    let id = reply.request_id();
    let row = c.h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(row.published_attempt, None, "nothing was published");
    assert_eq!(row.charged_micros, None, "the user pays 0");
    let postings = c.h.store.postings(&id).unwrap();
    assert!(
        postings.iter().all(|p| p.kind != "user_price"),
        "no user_price posting: {postings:?}"
    );
}

// ---------------------------------------------------------------------------
// 安全服务的各种故障：输入阶段
// ---------------------------------------------------------------------------

/// (名字, 假服务的行为, 预期的 cause)。
fn failure_modes() -> Vec<(&'static str, Behavior, &'static str)> {
    vec![
        ("hangs", Behavior::Hang, "timeout"),
        (
            "slower than the budget",
            Behavior::slow_json(
                200,
                json!({"choices": [{"message": {"content": "{\"verdict\":\"allow\"}"}}]}),
                Duration::from_secs(30),
            ),
            "timeout",
        ),
        (
            "HTTP 500",
            Behavior::json(
                500,
                json!({"error": {"message": format!("boom {ANSWER_SENTINEL}")}}),
            ),
            "service_error",
        ),
        (
            "HTTP 503",
            Behavior::json(503, json!({"error": {}})),
            "service_error",
        ),
        (
            "HTTP 401 (our key was refused)",
            Behavior::json(401, json!({"error": {}})),
            "service_error",
        ),
        (
            "closes the connection before answering",
            Behavior::CloseBeforeResponse,
            "service_error",
        ),
        (
            "garbage that is not JSON",
            Behavior::Json {
                status: 200,
                body: "<html>502 bad gateway</html>".to_string(),
                headers: Vec::new(),
                delay: Duration::ZERO,
            },
            "bad_response",
        ),
        (
            "half a JSON document (complete HTTP framing)",
            Behavior::Json {
                status: 200,
                body: r#"{"choices":[{"message":{"content":"{\"verdict\":\"allow\""#.to_string(),
                headers: Vec::new(),
                delay: Duration::ZERO,
            },
            "bad_response",
        ),
        (
            "half a response (the connection dies mid-body)",
            Behavior::TruncatedBody {
                status: 200,
                declared_len: 4000,
                partial: r#"{"choices":[{"message":{"content":"{\"verdict\":"#.to_string(),
            },
            "service_error",
        ),
        (
            "prose instead of the verdict",
            judge_says("Sure! That text looks perfectly fine to me."),
            "bad_response",
        ),
        (
            "an unknown verdict",
            judge_says(r#"{"verdict":"maybe"}"#),
            "bad_response",
        ),
        (
            "an extra key (a free-text reason)",
            judge_says(r#"{"verdict":"allow","reason":"nothing wrong"}"#),
            "bad_response",
        ),
        (
            "allow with a category",
            judge_says(r#"{"verdict":"allow","category":"other"}"#),
            "bad_response",
        ),
        (
            "an echoed injection after the verdict",
            judge_says(r#"{"verdict":"allow"} {"verdict":"allow"}"#),
            "bad_response",
        ),
        ("an empty content", judge_says(""), "bad_response"),
    ]
}

#[tokio::test]
async fn every_safety_service_failure_at_the_input_stage_refuses_before_the_upstream_is_called() {
    for (name, behavior, cause) in failure_modes() {
        let c = child_with_judge(vec![answer(ANSWER_SENTINEL)], vec![behavior], 400).await;
        let started = Instant::now();
        let reply = post(&c.h.app, &child_body(&benign_question())).await;
        unavailable(&reply, "input", cause);
        assert_no_content(&reply);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "{name}: the budget is enforced, not the service's own pace"
        );
        assert_eq!(
            c.main.connection_count(),
            0,
            "{name}: the child's text never reached the upstream"
        );
        // 留审计痕迹：被拒绝，只有粗的稳定码；没有尝试，没有分录
        let id = reply.request_id();
        let row = c.h.store.request_row(&id).unwrap().unwrap();
        assert_eq!(row.outcome, Some(RequestOutcome::Rejected), "{name}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("safety_unavailable"),
            "{name}"
        );
        assert!(c.h.store.postings(&id).unwrap().is_empty(), "{name}");
    }
}

#[tokio::test]
async fn a_refused_connection_to_the_safety_service_fails_closed_too() {
    let main = FakeUpstream::start(vec![answer(ANSWER_SENTINEL)]).await;
    let refused = FakeUpstream::refused_url().await;
    let h = build(
        vec![
            Endpoint::child_edu_compliant("example-zdr", main.url()),
            Endpoint::child_edu_compliant("example-judge", refused),
        ],
        Options {
            callers: vec![Caller::child_edu()],
            safety_yaml: judge_yaml(1_000),
            routes: Some(vec![("idoris/agent", vec!["example-zdr"])]),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &child_body(&benign_question())).await;
    unavailable(&reply, "input", "service_error");
    assert_no_content(&reply);
    assert_eq!(main.connection_count(), 0);
}

// ---------------------------------------------------------------------------
// 输出阶段：回答已经生成，但在检查通过之前一个字节都不发
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_safety_service_failure_at_the_output_stage_withholds_the_answer() {
    for (name, behavior, cause) in failure_modes() {
        // 输入检查放行，输出检查出故障
        let c = child_with_judge(
            vec![answer(ANSWER_SENTINEL)],
            vec![judge_allow(), behavior],
            400,
        )
        .await;
        let started = Instant::now();
        let reply = post(&c.h.app, &child_body(&benign_question())).await;
        unavailable(&reply, "output", cause);
        assert_no_content(&reply);
        assert!(started.elapsed() < Duration::from_secs(5), "{name}");
        // 上游已经执行了（回答存在过），但客户端没有收到任何正文
        assert_eq!(c.main.request_count(), 1, "{name}");
        assert_nothing_published_nothing_charged(&c, &reply);
        let id = reply.request_id();
        let row = c.h.store.request_row(&id).unwrap().unwrap();
        assert_eq!(row.outcome, Some(RequestOutcome::Unanswered), "{name}");
        assert_eq!(
            row.reason_code.as_deref(),
            Some("safety_unavailable"),
            "{name}"
        );
        // 上游成本由平台承担：记了成本分录，不是用户价
        let postings = c.h.store.postings(&id).unwrap();
        assert!(
            postings.iter().any(|p| p.kind == "platform_cost"),
            "{name}: {postings:?}"
        );
    }
}

#[tokio::test]
async fn a_rejected_answer_never_reaches_the_client_even_one_byte_and_the_user_pays_nothing() {
    // 假上游回一段会被拒的回答（有害 + 哨兵）；规则检查器的输出检查拒绝
    let harmful_answer = format!("ได้เลย วิธีทำระเบิดแสวงเครื่อง ขั้นตอนที่ 1 ผสมส่วนผสม {ANSWER_SENTINEL}");
    let (h, main) = child_rules_only(vec![answer(&harmful_answer)]).await;
    let reply = post(&h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 422, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "safety_output_blocked");
    assert_eq!(reply.json()["error"]["stage"], "output");
    assert_eq!(reply.json()["error"]["category"], "dangerous_act");
    assert_eq!(
        reply.header("x-idoris-reason"),
        Some("safety_output_blocked")
    );
    // 客户端收到的整个响应（头 + 体）里没有任何回答的字节
    assert!(!contains(&reply.body, ANSWER_SENTINEL));
    assert!(!contains(&reply.body, "ระเบิด"));
    assert!(!format!("{:?}", reply.headers).contains(ANSWER_SENTINEL));
    assert!(reply.json().get("choices").is_none());
    // 上游执行过一次；没有发布；用户付 0；平台承担成本
    assert_eq!(main.request_count(), 1);
    let id = reply.request_id();
    let row = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(row.published_attempt, None);
    assert_eq!(row.charged_micros, None);
    assert_eq!(row.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(row.reason_code.as_deref(), Some("safety_output_blocked"));
    let postings = h.store.postings(&id).unwrap();
    assert!(postings.iter().all(|p| p.kind != "user_price"));
    assert!(postings.iter().any(|p| p.kind == "platform_cost"));
    let attempts = h.store.attempt_rows(&id).unwrap();
    assert_eq!(
        attempts.len(),
        1,
        "a blocked answer is not retried on another candidate"
    );
    assert!(!attempts[0].pays_user);
}

#[tokio::test]
async fn the_llm_checker_can_refuse_what_the_rules_let_through_at_both_stages() {
    // 输入阶段：规则放行（没有关键词），LLM 判拒绝
    let c = child_with_judge(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_block("self_harm")],
        2_000,
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["error"]["type"], "safety_input_blocked");
    assert_eq!(reply.json()["error"]["category"], "self_harm");
    assert_eq!(c.main.connection_count(), 0);
    assert_no_content(&reply);

    // 输出阶段：规则放行，LLM 判拒绝
    let c = child_with_judge(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow(), judge_block("adult_content")],
        2_000,
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["error"]["type"], "safety_output_blocked");
    assert_eq!(reply.json()["error"]["category"], "adult_content");
    assert_no_content(&reply);
    assert_nothing_published_nothing_charged(&c, &reply);
    // 判分结果里的未知类别按 other
    let c = child_with_judge(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_says(
            r#"{"verdict":"block","category":"a_new_category"}"#,
        )],
        2_000,
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.json()["error"]["category"], "other");
}

#[tokio::test]
async fn when_the_rules_refuse_the_paid_llm_checker_is_never_called() {
    let c = child_with_judge(vec![answer(ANSWER_SENTINEL)], vec![judge_allow()], 2_000).await;
    let reply = post(&c.h.app, &child_body(&harmful_question())).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["error"]["type"], "safety_input_blocked");
    assert_eq!(
        c.judge.connection_count(),
        0,
        "rules first: no money spent, no text sent"
    );
    assert_eq!(c.main.connection_count(), 0);
}

#[tokio::test]
async fn both_checks_pass_then_the_answer_is_served_and_the_user_pays_once() {
    let c = child_with_judge(vec![answer("สี่")], vec![judge_allow(), judge_allow()], 2_000).await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.json()["choices"][0]["message"]["content"], "สี่");
    let id = reply.request_id();
    let row = c.h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(row.outcome, Some(RequestOutcome::Answered));
    assert!(row.charged_micros.is_some());
    assert_eq!(
        c.judge.request_count(),
        2,
        "one input check and one output check"
    );

    // 发给检查服务的请求：白名单字段、固定的采样参数；没有任何能关联到孩子的字段；密钥不在请求体里
    let requests = c.judge.requests();
    for request in &requests {
        let body = request.json();
        let mut keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec!["max_tokens", "messages", "model", "stream", "temperature"]
        );
        assert_eq!(body["max_tokens"], 32);
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["stream"], false);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][1]["role"], "user");
        // 待检查的文本在 JSON 字符串里，不是拼进提示的字面文本
        let user_message = body["messages"][1]["content"].as_str().unwrap();
        let payload: serde_json::Value = serde_json::from_str(user_message).unwrap();
        assert!(payload["stage"] == "input" || payload["stage"] == "output");
        assert!(!String::from_utf8_lossy(&request.body).contains(harness::KEY));
    }
    let input_payload: serde_json::Value = serde_json::from_str(
        requests[0].json()["messages"][1]["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(input_payload["stage"], "input");
    assert!(
        input_payload["messages"][0]["text"]
            .as_str()
            .unwrap()
            .contains(CHILD_SENTINEL)
    );
    let output_payload: serde_json::Value = serde_json::from_str(
        requests[1].json()["messages"][1]["content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(output_payload["stage"], "output");
    assert_eq!(output_payload["reply"][0]["text"], "สี่");
}

// ---------------------------------------------------------------------------
// 端点许可表：检查端点和主上游一样受约束
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_safety_endpoint_whose_verification_expires_while_running_stops_being_usable() {
    // 检查端点核验得早：加载时有效，之后时钟拨过它的过期时间；主上游核验得晚，仍然有效
    let main = FakeUpstream::start(vec![answer("สี่"), answer("never")]).await;
    let judge = FakeUpstream::start(vec![
        judge_allow(),
        judge_allow(),
        judge_allow(),
        judge_allow(),
    ])
    .await;
    let clock = Arc::new(ManualClock::new(TimestampMs(harness::NOW_MS)));
    let h = build(
        vec![
            Endpoint::child_edu_compliant("example-zdr", main.url()),
            // 2026-09-01 + 90 天 = 2026-11-30 过期
            Endpoint::child_edu_compliant("example-judge", judge.url()).verified("2026-09-01"),
        ],
        Options {
            callers: vec![Caller::child_edu()],
            safety_yaml: judge_yaml(1_000),
            routes: Some(vec![("idoris/agent", vec!["example-zdr"])]),
            clock: Some(clock.clone()),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(judge.request_count(), 2);
    // 拨到 2026-12-05：检查端点的核验过期，主上游（2026-11-12 + 90 天）仍有效
    clock.set(TimestampMs(harness::NOW_MS + 23 * harness::DAY_MS));
    let reply = post(&h.app, &child_body(&benign_question())).await;
    unavailable(&reply, "input", "no_compliant_checker");
    assert_eq!(
        judge.request_count(),
        2,
        "an expired safety endpoint receives nothing more"
    );
    assert_eq!(main.request_count(), 1, "and the child's text goes nowhere");
}

fn load(endpoints: &[Endpoint], callers: &[Caller], safety_yaml: &str) -> Result<(), String> {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("upstream.key");
    std::fs::write(&key_file, "sk-test\n").unwrap();
    std::fs::write(
        dir.path().join("permits.yaml"),
        harness::permit_table_yaml(endpoints),
    )
    .unwrap();
    let options = Options {
        callers: callers.to_vec(),
        safety_yaml: safety_yaml.to_string(),
        routes: Some(vec![("idoris/agent", vec!["example-zdr"])]),
        ..Options::default()
    };
    let yaml = harness::config_yaml(endpoints, key_file.to_str().unwrap(), &options);
    idoris_router::cloud::config::parse(&yaml, dir.path(), TimestampMs(harness::NOW_MS)).map(|_| ())
}

#[test]
fn a_safety_endpoint_the_permit_table_does_not_allow_for_children_fails_the_startup() {
    let url = "http://127.0.0.1:9/v1".to_string();
    let main = Endpoint::child_edu_compliant("example-zdr", url.clone());
    let child = [Caller::child_edu()];
    let judge = |tweak: fn(&mut Endpoint)| {
        let mut endpoint = Endpoint::child_edu_compliant("example-judge", url.clone());
        tweak(&mut endpoint);
        endpoint
    };
    // 合规：能加载
    load(&[main.clone(), judge(|_| {})], &child, &judge_yaml(1_000)).unwrap();
    // 每一条硬约束单独不满足，都启动失败，错误里说明被排除的原因（枚举，没有内容）
    for (what, endpoint, needle) in [
        (
            "does not allow child_edu",
            judge(|e| e.classes = vec!["adult", "public"]),
            "DataClassNotAllowed",
        ),
        (
            "keeps data (not zdr)",
            judge(|e| {
                // 许可表自己不允许「声明 child_edu 却不是 ZDR」，所以同时去掉 child_edu：照样不合规
                e.retention = "with_exceptions";
                e.classes = vec!["adult"];
            }),
            "RetentionTooWeak",
        ),
        (
            "no signed DPA",
            judge(|e| {
                e.dpa = "unsigned";
                e.classes = vec!["adult"];
            }),
            "DpaRequired",
        ),
        (
            "verification expired",
            judge(|e| e.verified_at = "2026-01-01"),
            "VerificationExpired",
        ),
        (
            "a subscription (CLI) path",
            {
                let mut e = judge(|_| {});
                e.access = "subscription";
                e
            },
            "SubscriptionNotAllowedInCloud",
        ),
    ] {
        let error = load(&[main.clone(), endpoint], &child, &judge_yaml(1_000)).unwrap_err();
        assert!(
            error.contains("不满足儿童数据的端点许可表硬约束") && error.contains(needle),
            "{what}: {error}"
        );
    }
    // 区域：儿童调用方限定了区域，检查端点的处理区域不在其中
    let regional = [Caller {
        allowed_regions: Some(vec!["sg-1"]),
        ..Caller::child_edu()
    }];
    let mut offshore = Endpoint::child_edu_compliant("example-judge", url.clone());
    offshore.regions = vec!["sg-1", "eu-1"];
    let mut regional_main = Endpoint::child_edu_compliant("example-zdr", url.clone());
    regional_main.regions = vec!["sg-1"];
    let error = load(&[regional_main, offshore], &regional, &judge_yaml(1_000)).unwrap_err();
    assert!(error.contains("RegionNotAllowed"), "{error}");
}

#[test]
fn the_safety_section_rules() {
    let url = "http://127.0.0.1:9/v1".to_string();
    let main = Endpoint::child_edu_compliant("example-zdr", url.clone());
    let judge = Endpoint::child_edu_compliant("example-judge", url);
    let child = [Caller::child_edu()];
    let both = [main.clone(), judge];
    // 有儿童调用方却没有 safety 段：加载失败
    let error = load(std::slice::from_ref(&main), &child, "").unwrap_err();
    assert!(error.contains("必须配置 safety"), "{error}");
    // mode 必填：没有默认值（只用规则检查器必须是显式的选择）
    let error = load(
        std::slice::from_ref(&main),
        &child,
        "safety:\n  timeout_ms: 1000\n",
    )
    .unwrap_err();
    assert!(error.contains("mode"), "{error}");
    // rules_only 可以，不能同时写 llm_judge
    load(
        std::slice::from_ref(&main),
        &child,
        "safety:\n  mode: rules_only\n",
    )
    .unwrap();
    let error = load(
        &both,
        &child,
        &judge_yaml(1_000).replace("rules_and_llm", "rules_only"),
    )
    .unwrap_err();
    assert!(error.contains("不能同时配置 llm_judge"), "{error}");
    // rules_and_llm 必须有 llm_judge
    let error = load(
        std::slice::from_ref(&main),
        &child,
        "safety:\n  mode: rules_and_llm\n",
    )
    .unwrap_err();
    assert!(error.contains("必须配置 llm_judge"), "{error}");
    // LLM 检查端点必须在 endpoints 里配置
    let error = load(std::slice::from_ref(&main), &child, &judge_yaml(1_000)).unwrap_err();
    assert!(error.contains("不在端点许可表里"), "{error}");
    // 上限
    for yaml in [
        "safety:\n  mode: rules_only\n  timeout_ms: 0\n",
        "safety:\n  mode: rules_only\n  timeout_ms: 600000\n",
        "safety:\n  mode: rules_only\n  max_input_bytes: 0\n",
        "safety:\n  mode: rules_only\n  surprise: true\n",
    ] {
        assert!(
            load(std::slice::from_ref(&main), &child, yaml).is_err(),
            "{yaml}"
        );
    }
    // 没有儿童调用方时，safety 段可以没有；有也行（成人租户不受影响）
    load(std::slice::from_ref(&main), &[Caller::adult_default()], "").unwrap();
    load(
        &[main],
        &[Caller::adult_default()],
        "safety:\n  mode: rules_only\n",
    )
    .unwrap();
}

// ---------------------------------------------------------------------------
// 成本：事前占额度，事后结算
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_cost_of_the_safety_checks_is_reserved_before_and_settled_after_with_the_real_usage() {
    let meter = Arc::new(SimpleExtraCost::new());
    let c = child_with(
        vec![answer("สี่")],
        vec![judge_allow(), judge_allow()],
        2_000,
        |options| options.extra_cost = Some(meter.clone() as Arc<dyn ExtraCostMeter>),
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200);
    let entries = meter.entries();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].stage, SafetyStage::Input);
    assert_eq!(entries[1].stage, SafetyStage::Output);
    for entry in &entries {
        // 事前的预留是上界（比实际用量大）
        assert!(entry.reserved.micros() > 13, "{entry:?}");
        match &entry.outcome {
            ExtraCostOutcome::Spent(items) => {
                assert_eq!(items.len(), 1);
                // 假服务报告 40 个输入 + 6 个输出 token；价目 0.26 / 0.38 美元每百万 token：ceil(10.4 + 2.28) = 13 微美元
                assert_eq!(
                    items[0].cost,
                    Cost::from_micros(idoris_compute_contracts::money::Unit::Usd, 13).unwrap()
                );
                assert!(!items[0].estimated);
                assert_eq!(items[0].usage.unwrap().prompt_tokens(), 40);
            }
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(
        meter.open_tickets(),
        0,
        "every reservation was settled exactly once"
    );

    // 超时：请求可能已经到了检查服务，花了多少不知道：按上界估算记账（estimated），不是记 0，也不是丢掉。
    // （检查器自己发现的超时带着估算的花费；gate 外层的超时或 future 被丢弃则是 Unknown，见 safety 模块的单元测试。）
    let meter = Arc::new(SimpleExtraCost::new());
    let c = child_with(
        vec![answer("never")],
        vec![Behavior::Hang],
        300,
        |options| options.extra_cost = Some(meter.clone() as Arc<dyn ExtraCostMeter>),
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    unavailable(&reply, "input", "timeout");
    let entries = meter.entries();
    assert_eq!(entries.len(), 1);
    match &entries[0].outcome {
        ExtraCostOutcome::Spent(items) => {
            assert_eq!(items.len(), 1);
            assert!(items[0].estimated && items[0].usage.is_none(), "{items:?}");
            assert!(
                items[0].cost.micros() > 13,
                "charged at the upper bound, not at zero"
            );
        }
        ExtraCostOutcome::Unknown => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(meter.open_tickets(), 0);

    // 规则拒绝：LLM 没有被调用，花费是零笔，但预留照样被结算（不悬空）
    let meter = Arc::new(SimpleExtraCost::new());
    let c = child_with(
        vec![answer("never")],
        vec![judge_allow()],
        2_000,
        |options| options.extra_cost = Some(meter.clone() as Arc<dyn ExtraCostMeter>),
    )
    .await;
    let reply = post(&c.h.app, &child_body(&harmful_question())).await;
    assert_eq!(reply.status, 422);
    assert_eq!(
        meter.entries()[0].outcome,
        ExtraCostOutcome::Spent(Vec::new())
    );
    assert_eq!(meter.open_tickets(), 0);
}

/// 拒绝额度的计量器：付款账户链放不下安全检查的成本。
#[derive(Debug)]
struct Refusing {
    reservations: Mutex<usize>,
}

impl ExtraCostMeter for Refusing {
    fn reserve(
        &self,
        _request: &ExtraCostRequest<'_>,
    ) -> Result<ExtraCostTicket, ExtraCostRefusal> {
        *self.reservations.lock().unwrap() += 1;
        Err(ExtraCostRefusal::Denied(ReasonCode::BudgetExhausted))
    }

    fn settle(&self, _ticket: ExtraCostTicket, _outcome: ExtraCostOutcome) {
        panic!("nothing was reserved, so nothing may be settled");
    }
}

#[tokio::test]
async fn when_the_safety_check_cannot_be_afforded_nothing_is_checked_and_nothing_is_sent() {
    let meter = Arc::new(Refusing {
        reservations: Mutex::new(0),
    });
    let c = child_with(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow()],
        2_000,
        |options| options.extra_cost = Some(meter.clone() as Arc<dyn ExtraCostMeter>),
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    // 准入类的拒绝（和主调用的准入拒绝同一套：402 budget_exhausted）
    assert_eq!(reply.status, 402, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "budget_exhausted");
    assert_eq!(c.judge.connection_count(), 0);
    assert_eq!(c.main.connection_count(), 0);
    assert_no_content(&reply);
    assert_eq!(*meter.reservations.lock().unwrap(), 1);
}
