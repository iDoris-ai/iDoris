//! 计算服务 S6 儿童安全：**PR #600 第二轮复审（round2-r2 N1、round2-r3 N1–N3 与 PARTIAL）的端到端回归**。
//!
//! 每一条都走真实的 HTTP 处理函数、执行器和安全 gate，上游和检查服务是 `127.0.0.1` 上的脚本化假服务，
//! **不连接任何真实的推理或安全服务**：
//!
//! - **N1（round2-r2 与 round2-r3 同一问题）**：`rules_only_acknowledgement` 的到期日要在运行期间生效——进程不重启，时钟拨过
//!   到期日（跨 UTC 日期、长期运行），儿童请求就在调用任何上游之前被拒绝；
//! - **N2**：LLM 检查器看到的工具参数不能丢掉数字、布尔和 `null`。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_safety.rs"]
mod child_safety;
#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::sync::Arc;

use child_safety::{answer, benign_question, child_body, child_with, judge_allow};
use fake_upstream::{Behavior, FakeUpstream, usage};
use harness::{
    Caller, DAY_MS, Endpoint, NOW_MS, Options, RecordingAdmission, VERIFIED_MS, build, post,
};
use idoris_compute_contracts::ids::TimestampMs;
use idoris_router::cloud::ManualClock;
use serde_json::json;

fn unavailable(reply: &harness::Reply, stage: &str, cause: &str) {
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "safety_unavailable");
    assert_eq!(reply.json()["error"]["stage"], stage, "{}", reply.text());
    assert_eq!(reply.json()["error"]["cause"], cause, "{}", reply.text());
    assert_eq!(reply.header("x-idoris-reason"), Some("safety_unavailable"));
}

/// 只用规则检查器、带显式确认的 `safety:` 段。
fn rules_only_ack(expires_on: &str) -> String {
    format!(
        "safety:\n  mode: rules_only\n  rules_only_acknowledgement:\n    reason: \"内测外部测试尚无签了DPA的检查器模型\"\n    acknowledged_by: \"ops-ack\"\n    expires_on: \"{expires_on}\"\n"
    )
}

/// 一个带工具调用的上游回答。
fn tool_call_answer(name: &str, arguments: &str) -> Behavior {
    Behavior::json(
        200,
        json!({
            "id": "chatcmpl-tool", "object": "chat.completion", "created": 1_700_000_000u64,
            "model": "example-org/chat-70b",
            "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
                "role": "assistant", "content": null,
                "tool_calls": [{"id": "call_1", "type": "function",
                                "function": {"name": name, "arguments": arguments}}]}}],
            "usage": usage(12, 30),
        }),
    )
}

// ---------------------------------------------------------------------------
// N1：确认的到期日在运行期间生效
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_expired_rules_only_acknowledgement_stops_serving_children_while_the_process_keeps_running()
 {
    // 配置加载时（2026-11-12）有效：到期日 2026-12-12（今天 + 30 天，UTC，含）。进程不重启，只有时钟在走。
    let expires =
        idoris_router::cloud::safety::config::utc_calendar_date_plus(TimestampMs(NOW_MS), 30)
            .unwrap();
    assert_eq!(expires, "2026-12-12");
    let main =
        FakeUpstream::start(vec![answer("สี่"), answer("สี่"), answer("สี่"), answer("never")]).await;
    let clock = Arc::new(ManualClock::new(TimestampMs(NOW_MS)));
    let h = build(
        vec![Endpoint::child_edu_compliant("example-zdr", main.url())],
        Options {
            callers: vec![Caller::child_edu()],
            safety_yaml: rules_only_ack(&expires),
            clock: Some(clock.clone()),
            ..Options::default()
        },
    );
    let ask = || child_body(&benign_question());

    // 启动当天：照常服务
    assert_eq!(post(&h.app, &ask()).await.status, 200);
    // 到期日那一天的任何时刻（含最后一毫秒）：照常服务（到期日含）
    clock.set(TimestampMs(VERIFIED_MS + 30 * DAY_MS + 3_600_000));
    assert_eq!(post(&h.app, &ask()).await.status, 200);
    clock.set(TimestampMs(VERIFIED_MS + 31 * DAY_MS - 1));
    assert_eq!(post(&h.app, &ask()).await.status, 200);
    assert_eq!(main.request_count(), 3);

    // 跨过 UTC 午夜：到期。没有 LLM 检查器，儿童请求在调用任何上游之前被拒绝
    clock.set(TimestampMs(VERIFIED_MS + 31 * DAY_MS));
    let reply = post(&h.app, &ask()).await;
    unavailable(&reply, "input", "no_compliant_checker");
    assert_eq!(
        main.request_count(),
        3,
        "the child's text went nowhere after the acknowledgement expired"
    );
    assert_eq!(main.connection_count(), 3);
    // 长期运行：之后的任何时候都一样（检查端点 / 主上游的核验仍在有效期内，这里只有确认过期）
    clock.set(TimestampMs(VERIFIED_MS + 31 * DAY_MS + 45 * DAY_MS));
    let reply = post(&h.app, &ask()).await;
    unavailable(&reply, "input", "no_compliant_checker");
    assert_eq!(main.connection_count(), 3);
}

#[tokio::test]
async fn the_acknowledgement_expiring_between_the_input_and_the_output_check_withholds_the_answer()
{
    // 输入检查通过时仍然有效；上游调用期间（准入之后）跨过了到期日：回答不发给客户端
    let expires =
        idoris_router::cloud::safety::config::utc_calendar_date_plus(TimestampMs(NOW_MS), 30)
            .unwrap();
    let main = FakeUpstream::start(vec![answer("UPSTREAM-ANSWER-THAT-MUST-NOT-ARRIVE")]).await;
    let clock = Arc::new(ManualClock::new(TimestampMs(NOW_MS)));
    let admission = RecordingAdmission::new();
    *admission.on_admit.lock().unwrap() = Some(Box::new({
        let clock = clock.clone();
        move |_| clock.set(TimestampMs(VERIFIED_MS + 31 * DAY_MS))
    }));
    let h = build(
        vec![Endpoint::child_edu_compliant("example-zdr", main.url())],
        Options {
            callers: vec![Caller::child_edu()],
            safety_yaml: rules_only_ack(&expires),
            clock: Some(clock),
            admission: Some(admission),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &child_body(&benign_question())).await;
    unavailable(&reply, "output", "no_compliant_checker");
    assert_eq!(main.request_count(), 1, "the upstream did answer");
    assert!(
        !String::from_utf8_lossy(&reply.body).contains("MUST-NOT-ARRIVE"),
        "{}",
        reply.text()
    );
}

#[tokio::test]
async fn an_expired_acknowledgement_does_not_touch_tenants_that_do_not_handle_child_data() {
    let expires =
        idoris_router::cloud::safety::config::utc_calendar_date_plus(TimestampMs(NOW_MS), 30)
            .unwrap();
    let main = FakeUpstream::start(vec![answer("ok"), answer("ok")]).await;
    let clock = Arc::new(ManualClock::new(TimestampMs(NOW_MS)));
    let h = build(
        vec![Endpoint::adult("example-alpha", main.url())],
        Options {
            callers: vec![Caller::adult_default()],
            safety_yaml: rules_only_ack(&expires),
            clock: Some(clock.clone()),
            ..Options::default()
        },
    );
    assert_eq!(post(&h.app, &harness::simple_body(100)).await.status, 200);
    clock.set(TimestampMs(VERIFIED_MS + 31 * DAY_MS + 10 * DAY_MS));
    assert_eq!(
        post(&h.app, &harness::simple_body(100)).await.status,
        200,
        "only child-data requests depend on the acknowledgement"
    );
}

// ---------------------------------------------------------------------------
// N2：检查模型看到完整的工具参数
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_judge_sees_the_numbers_booleans_and_nulls_of_the_tool_call_arguments() {
    // 孩子提供的八位私人密码 `31415926` 被上游放进工具参数的数字里：旧实现只把解码后的键和字符串值交给检查模型，
    // 数字、布尔、null 全部丢掉，检查模型看不到被回显的密码
    let arguments = r#"{"answer":31415926,"ok":true,"gone":null,"nested":{"n":[1,2.5,false]}}"#;
    let c = child_with(
        vec![tool_call_answer("answer", arguments)],
        vec![judge_allow(), judge_allow()],
        2_000,
        |_| {},
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let requests = c.judge.requests();
    assert_eq!(requests.len(), 2, "one input check and one output check");
    let output_payload = requests[1].json()["messages"][1]["content"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        output_payload.contains("\"stage\":\"output\""),
        "{output_payload}"
    );
    for scalar in ["31415926", "true", "null", "2.5", "false"] {
        assert!(
            output_payload.contains(scalar),
            "the judge never saw {scalar}: {output_payload}"
        );
    }
}
