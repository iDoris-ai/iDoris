//! 计算服务 PR #600 的 Codex 审查 r2 与 PR-Daemon 评审修复的端到端回归（云上游、执行器、流、结算）。
//!
//! 每个场景都是「在修复之前的代码上会失败」的复现：
//!
//! | 编号 | 场景 |
//! |---|---|
//! | R2 High#1 | 显式 `local_only` 不出网（执行器：任何入口构造的调用都一样），也不送安全检查模型 |
//! | R2 High#3 | `billing: unbilled` 没有实现：执行器失败即关闭，不悄悄按付费处理 |
//! | R2 Medium#3 | 任意 JSON 对象不是流式回答：发布之前失败，允许切换候选 |
//! | R2 Medium#4 | 首个网络块就含 `[DONE]`：流立刻完成，不等上游，不处理 `[DONE]` 之后的事件 |
//! | R2 Medium#6 | 非流式的总期限是「响应头 + 响应体」合起来算 |
//! | PD B4 | 流中途的 usage 块不是最终用量：客户端断开 / 流失败之后不能按它结算 |
//!
//! 数字约定同 `cloud_upstream_e2e.rs`：`simple_body(100)` 预留价 4110 微泰铢、预留成本 41 微美元；
//! 12 入 2 出的实际价 200 微泰铢。全部对本地假上游（`127.0.0.1`）进行，不连接任何真实的推理供应商。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use fake_upstream::{
    Behavior, FakeUpstream, SseStep, completion, content_event, done_event, usage, usage_event,
};
use harness::{Endpoint, Harness, Options, build, p, post, postings, rows, simple_body, sse_data};
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::attempt::{AttemptState, AttemptStateKind, SettleBasis};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{EndpointKey, RequestId};
use idoris_compute_contracts::permit::{
    Billing, CapabilityNeeds, DataClass, Retention, TenantDataPolicy,
};
use idoris_compute_contracts::pricing::Usage;
use idoris_router::cloud::{
    AdmissionError, AdmissionRequest, CallPrivacy, CandidateAdmission, CloudCall, CloudError,
    SimpleAdmission,
};
use idoris_tenancy::attempts::RequestOutcome;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

const RESERVED_PRICE: i64 = 4110;
const RESERVED_COST: i64 = 41;

fn streaming_body() -> Value {
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    body
}

/// 等到请求被关闭（流被丢弃时的收尾发生在客户端断开之后）。
async fn wait_closed(h: &Harness, id: &RequestId) {
    for _ in 0..200 {
        if h.store
            .request_row(id)
            .unwrap()
            .is_some_and(|r| r.outcome.is_some())
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("request {id} was never closed");
}

/// 发流式请求，读帧直到某一帧包含 `until`，然后客户端断开（丢弃响应体）。返回请求 id。
async fn read_until_then_disconnect(h: &Harness, body: &Value, until: &str) -> RequestId {
    let response = harness::send(&h.app, harness::chat_request(body)).await;
    assert_eq!(response.status(), 200);
    let id = RequestId::new(
        response
            .headers()
            .get("x-idoris-record-id")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let mut stream = response.into_body();
    let mut seen = String::new();
    while !seen.contains(until) {
        let frame = tokio::time::timeout(
            Duration::from_secs(5),
            http_body_util::BodyExt::frame(&mut stream),
        )
        .await
        .expect("the stream stalled before the expected frame")
        .expect("the stream ended before the expected frame")
        .unwrap();
        seen.push_str(&String::from_utf8_lossy(frame.data_ref().unwrap()));
    }
    drop(stream);
    id
}

fn direct_call(
    request: &str,
    providers: &[&str],
    privacy: CallPrivacy,
    billing: Billing,
) -> CloudCall {
    let payment: PaymentContext = serde_json::from_value(
        json!({"credential": "cred-direct", "payer": "acct-direct", "unit": "THB"}),
    )
    .unwrap();
    let body = simple_body(100);
    CloudCall {
        request_id: RequestId::new(request).unwrap(),
        payment,
        tenant: TenantDataPolicy::new(
            BTreeSet::from([DataClass::Adult]),
            None,
            Retention::WithExceptions,
            false,
        )
        .unwrap(),
        billing,
        privacy,
        client_body: body.as_object().unwrap().clone(),
        stream: false,
        client_wants_usage_chunk: false,
        requested_max_tokens: Some(100),
        input_tokens_estimate: 11,
        candidates: providers
            .iter()
            .map(|p| EndpointKey::new(*p, "example-org/chat-70b", "openai-compat").unwrap())
            .collect(),
        needs: CapabilityNeeds::default(),
        cancel: CancellationToken::new(),
    }
}

// ---------------------------------------------------------------------------
// R2 High#1：显式 local_only 永远不出网
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_local_only_call_is_refused_by_the_executor_before_any_upstream_work() {
    let a = FakeUpstream::start(vec![Behavior::json(200, completion("never", "x", None))]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let call = direct_call(
        "req-local-only",
        &["example-alpha"],
        CallPrivacy::LocalOnly,
        Billing::Paid,
    );
    let error = h.cloud.executor().execute(call).await.unwrap_err();
    assert!(matches!(error, CloudError::LocalOnlyRefused), "{error:?}");
    assert_eq!(error.close_reason(), "local_only_refused");
    // 一个字节都没出网：没有连接、没有准入、没有尝试
    assert_eq!(a.connection_count(), 0);
    let id = RequestId::new("req-local-only").unwrap();
    assert!(rows(&h, &id).is_empty(), "no attempt exists");
    // 留下一行已拒绝的审计记录（没有任何内容）
    let row = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(row.outcome, Some(RequestOutcome::Rejected));
    assert_eq!(row.reason_code.as_deref(), Some("local_only_refused"));

    // 对照：没声明、显式 any 照常服务
    for (privacy, request) in [
        (CallPrivacy::Undeclared, "req-undeclared"),
        (CallPrivacy::Any, "req-any"),
    ] {
        let a2 = FakeUpstream::start(vec![Behavior::json(
            200,
            completion("ok", "hello", Some(usage(12, 30))),
        )])
        .await;
        let h2 = build(
            vec![Endpoint::adult("example-alpha", a2.url())],
            Options::default(),
        );
        let call = direct_call(request, &["example-alpha"], privacy, Billing::Paid);
        h2.cloud.executor().execute(call).await.unwrap();
        assert_eq!(a2.request_count(), 1, "{privacy:?}");
    }
}

// ---------------------------------------------------------------------------
// R2 High#3：unbilled 没有实现，执行器失败即关闭
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_unbilled_call_is_refused_by_the_executor_instead_of_silently_charging_the_user() {
    let a = FakeUpstream::start(vec![Behavior::json(200, completion("never", "x", None))]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let call = direct_call(
        "req-unbilled",
        &["example-alpha"],
        CallPrivacy::Undeclared,
        Billing::Unbilled,
    );
    let error = h.cloud.executor().execute(call).await.unwrap_err();
    assert!(matches!(error, CloudError::Internal(_)), "{error:?}");
    assert_eq!(a.connection_count(), 0);
    let id = RequestId::new("req-unbilled").unwrap();
    assert!(rows(&h, &id).is_empty());
    assert_eq!(
        h.store.request_row(&id).unwrap().unwrap().outcome,
        Some(RequestOutcome::Rejected)
    );
}

// ---------------------------------------------------------------------------
// R2 Medium#3：任意 JSON 对象不是回答
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_json_object_that_is_not_a_completion_chunk_never_crosses_the_publish_boundary() {
    let alpha = FakeUpstream::start(vec![Behavior::sse(vec![
        // 上游先发一个空对象，随后报错
        SseStep::Send("data: {}\n\n".to_string()),
        SseStep::Send("data: {\"error\":{\"message\":\"boom\"}}\n\n".to_string()),
        SseStep::Finish,
    ])])
    .await;
    let beta = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("b", "from-beta")),
        SseStep::Send(usage_event("b", usage(12, 2))),
        SseStep::Send(done_event()),
        SseStep::Finish,
    ])])
    .await;
    let h = build(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
    );
    let reply = post(&h.app, &streaming_body()).await;
    assert_eq!(reply.status, 200);
    let events = sse_data(&reply.text());
    assert!(
        events.iter().any(|e| e.contains("from-beta")),
        "the second candidate answered: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e == "{}"),
        "the empty object was never forwarded: {events:?}"
    );
    assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    assert_eq!((alpha.request_count(), beta.request_count()), (1, 1));

    // 用户只付第二次（实际用量 12 入 2 出 = 200）；第一次没有发布，用户付 0
    let id = reply.request_id();
    let attempts = rows(&h, &id);
    assert_eq!(attempts.len(), 2);
    assert!(!attempts[0].pays_user && attempts[1].pays_user);
    assert_eq!(
        attempts[0].failure_kind.as_deref(),
        Some("stream_malformed")
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.published_attempt, Some(1));
    assert_eq!(request.charged_micros, Some(200));
}

// ---------------------------------------------------------------------------
// R2 Medium#4：首个网络块就含 [DONE]
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_done_in_the_first_network_chunk_completes_the_stream_at_once_and_later_events_are_ignored()
 {
    // 回答、usage、[DONE]、以及 [DONE] 之后的一个事件在同一个网络块里；上游随后不关闭连接
    let one_chunk = format!(
        "{}{}{}{}",
        content_event("c", "Hel"),
        usage_event("c", usage(12, 2)),
        done_event(),
        content_event("c", "AFTER-DONE"),
    );
    let alpha = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(one_chunk),
        SseStep::Hang,
    ])])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options::default(),
    );
    let started = Instant::now();
    // 默认的流静默超时是 600 ms：修复之前要等到它超时（并在 [DONE] 之后追加一个超时错误事件）才结束
    let reply = tokio::time::timeout(Duration::from_millis(350), post(&h.app, &streaming_body()))
        .await
        .expect(
            "the stream must finish as soon as it is complete, not wait for the upstream to close",
        );
    assert!(started.elapsed() < Duration::from_millis(500));
    assert_eq!(reply.status, 200);
    let events = sse_data(&reply.text());
    assert_eq!(
        events.last().map(String::as_str),
        Some("[DONE]"),
        "{events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("AFTER-DONE")),
        "events after [DONE] are not processed: {events:?}"
    );
    assert!(
        !events.iter().any(|e| e.contains("stream_idle_timeout")),
        "no timeout error after a complete stream: {events:?}"
    );
    // 完整结束：按实际用量付费
    let id = reply.request_id();
    wait_closed(&h, &id).await;
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.charged_micros, Some(200));
    assert_eq!(request.charged_basis.as_deref(), Some("actual_usage"));
}

// ---------------------------------------------------------------------------
// R2 Medium#6：总期限 = 响应头 + 响应体
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_non_stream_total_deadline_covers_the_response_head_and_the_body_together() {
    // 总期限 1000 ms；响应头第 600 ms 到（在 800 ms 的响应头期限之内），正文又过 600 ms 才到，合起来 1200 ms。
    // 修复之前：读正文的时限是发送之前按「剩余 1000 ms」算好的，从读正文这一刻重新开始计时，请求在第 1200 ms 成功。
    let slow = Behavior::SlowJson {
        status: 200,
        body: completion("slow", "too late", Some(usage(12, 30))).to_string(),
        head_delay: Duration::from_millis(600),
        body_delay: Duration::from_millis(600),
    };
    let a = FakeUpstream::start(vec![slow]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options {
            limits: "  total_deadline_ms: 1000\n  connect_timeout_ms: 500\n  response_head_timeout_ms: 800\n  body_timeout_ms: 2000\n  stream_idle_timeout_ms: 600\n  stream_total_timeout_ms: 10000\n  max_attempts: 1\n".to_string(),
            ..Options::default()
        },
    );
    let started = Instant::now();
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(
        reply.status,
        504,
        "the body missed the request's absolute deadline: {}",
        reply.text()
    );
    // 正文在总期限到的那一刻超时：下一轮循环先看到「总期限到了」，所以对客户端是 deadline_exceeded（也可能是 body_timeout）
    let code = reply.json()["error"]["reason_code"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(
        code == "deadline_exceeded" || code == "body_timeout",
        "{code}"
    );
    assert!(
        started.elapsed() < Duration::from_millis(1_800),
        "the request ended at its deadline, not 'head time + body time': {:?}",
        started.elapsed()
    );
    // 用户没有拿到回答：付 0；上游已经接受并（多半）执行了，尝试里记的失败是正文超时，成本挂账
    let id = reply.request_id();
    assert_eq!(
        rows(&h, &id)[0].failure_kind.as_deref(),
        Some("body_timeout"),
        "the body read was cut at the request's deadline"
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.charged_micros, None);
}

// ---------------------------------------------------------------------------
// PR-Daemon B4：流中途的 usage 不是最终用量
// ---------------------------------------------------------------------------

/// 上游：内容、一个 usage 块（`choices` 为空，12 入 2 出）、**还有更多内容**，然后挂起。
fn usage_then_more_content() -> Vec<SseStep> {
    vec![
        SseStep::Send(content_event("c", "Hel")),
        SseStep::Send(usage_event("c", usage(12, 2))),
        SseStep::Send(content_event("c", "lo-more")),
        SseStep::Hang,
    ]
}

#[tokio::test]
async fn a_mid_stream_usage_chunk_is_not_the_final_accounting_when_the_client_disconnects() {
    let a = FakeUpstream::start(vec![Behavior::sse(usage_then_more_content())]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    // 客户端读到「lo-more」（此时中途的 usage 块已经被处理过了）然后断开
    let id = read_until_then_disconnect(&h, &streaming_body(), "lo-more").await;
    assert!(a.client_closed(0, Duration::from_secs(3)).await);
    wait_closed(&h, &id).await;

    // 不是按中途的数字（12 入 2 出 = 200）结算：用户付预留价（用量未知的规则），成本按预留额挂账
    let attempts = rows(&h, &id);
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        },
        "a mid-stream usage must not make the usage 'known'"
    );
    assert_eq!(attempts[0].failure_kind.as_deref(), Some("client_gone"));
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(request.charged_basis.as_deref(), Some("reserved_amount"));
    assert_eq!(
        postings(&h, &id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ],
        "the reserved cost is held for reconciliation, not booked as a (low) actual cost"
    );
    // 上游声称的中途数字留给对账看，判定是「不可信」
    let claim = attempts[0].usage_claim.clone().unwrap();
    assert_eq!(claim["verdict"], "untrusted:not_final");
    assert_eq!(claim["raw"]["completion_tokens"], 2);
}

#[tokio::test]
async fn a_mid_stream_usage_chunk_does_not_hide_a_disconnect_from_an_endpoint_that_cannot_cancel() {
    // 许可表里这个端点「取消」能力是 false：断开之后上游可能还在生成、还在计费，不下结论，保持 maybe_executed。
    // 修复之前，中途的 usage 会盖过这个诚实的标注，写成「用量已知」，对账永远看不到它。
    let a = FakeUpstream::start(vec![Behavior::sse(usage_then_more_content())]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url()).no_cancellation()],
        Options::default(),
    );
    let id = read_until_then_disconnect(&h, &streaming_body(), "lo-more").await;
    assert!(a.client_closed(0, Duration::from_secs(3)).await);
    wait_closed(&h, &id).await;
    let attempts = rows(&h, &id);
    assert_eq!(
        attempts[0].state,
        AttemptState::MaybeExecuted,
        "whether the upstream kept generating is not known"
    );
    assert_eq!(
        h.store
            .attempts_in_states(&[AttemptStateKind::MaybeExecuted], 10)
            .unwrap()
            .len(),
        1,
        "reconciliation can find it"
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(
        postings(&h, &id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ]
    );
}

#[tokio::test]
async fn a_stream_that_fails_after_a_mid_stream_usage_chunk_is_billed_the_reservation() {
    let a = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("c", "Hel")),
        SseStep::Send(usage_event("c", usage(12, 2))),
        SseStep::Send(content_event("c", "lo")),
        SseStep::Close, // 没有终结块、没有 [DONE]：截断
    ])])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let reply = post(&h.app, &streaming_body()).await;
    assert_eq!(reply.status, 200);
    let events = sse_data(&reply.text());
    let last: Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["error"]["reason_code"], "stream_truncated");
    let id = reply.request_id();
    let attempts = rows(&h, &id);
    assert_eq!(
        attempts[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        }
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(
        postings(&h, &id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ]
    );
}

#[tokio::test]
async fn a_final_usage_chunk_is_trusted_even_when_the_client_leaves_before_done() {
    // 对照：usage 块是最后一块（choices 为空，之后没有内容）。客户端要了 usage 块、读到它就断开，没等 [DONE]：
    // 上游已经生成完了，这个 usage 就是整个响应的用量，按它结算（不是预留价）。
    let a = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("c", "Hel")),
        SseStep::Send(usage_event("c", usage(12, 2))),
        SseStep::Hang,
    ])])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let mut body = streaming_body();
    body["stream_options"] = json!({"include_usage": true});
    let id = read_until_then_disconnect(&h, &body, "prompt_tokens").await;
    assert!(a.client_closed(0, Duration::from_secs(3)).await);
    wait_closed(&h, &id).await;
    let attempts = rows(&h, &id);
    assert_eq!(
        attempts[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ActualUsage(Usage::new(12, 2, 0).unwrap())
        }
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.charged_micros, Some(200));
    assert_eq!(request.charged_basis.as_deref(), Some("actual_usage"));
}

// ---------------------------------------------------------------------------
// PR-Daemon S0 Low：已发送的尝试报零用量——口径问题（需要 owner 拍板），这里只钉住**今天**的行为
// ---------------------------------------------------------------------------

/// 上游对一个非空的提示回了一个「0 入 0 出」的 usage（结构合法，`total_tokens` 也对得上）。今天的规则：结构合法的 usage 就是
/// 实际用量，所以用户付 0、平台的成本也是 0，整笔预留释放，没有任何 `user_price` / `platform_cost` 分录。
///
/// **这是不是对的需要 owner 确认**：真实的分词器不会给一个非空提示算出 0 个输入 token，一个报 0 的上游多半是出错了
/// （或者在躲账单）。备选口径：(a) 按最小值收（至少 1 个输入 token）；(b) 非空请求报 0 用量按「用量未知」处理（预留价、
/// 挂账待核销）——会让用户为一个可能合法的零用量付预留价；(c) 维持现状（对用户最宽松，对账时才发现供应商账单对不上）。
/// 这条测试不替 owner 拍板，只让改口径的人必须同时改它。
#[tokio::test]
async fn a_sent_attempt_that_reports_zero_usage_is_charged_zero_today_an_owner_question() {
    let a = FakeUpstream::start(vec![Behavior::json(
        200,
        completion("zero", "hello", Some(usage(0, 0))),
    )])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    let id = reply.request_id();
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Answered));
    assert_eq!(
        request.charged_micros,
        Some(0),
        "zero usage is charged zero"
    );
    assert_eq!(request.charged_basis.as_deref(), Some("actual_usage"));
    assert_eq!(
        postings(&h, &id),
        vec![p(0, "release", RESERVED_PRICE, "THB")],
        "no user_price and no platform_cost: the whole reservation is released"
    );
}

// ---------------------------------------------------------------------------
// R2 Medium#2：同步的账本操作不饿死运行时
// ---------------------------------------------------------------------------

/// 准入要等 300 ms（模拟另一个连接持有写事务、SQLite 在 busy timeout 里等）。
struct SlowAdmission;

impl CandidateAdmission for SlowAdmission {
    fn admit(&self, request: &AdmissionRequest<'_>) -> Result<AdmissionDecision, AdmissionError> {
        std::thread::sleep(Duration::from_millis(300));
        SimpleAdmission.admit(request)
    }
}

/// 只有一个 worker 线程的多线程运行时：一个请求的准入在账本上阻塞 300 ms，旁边的心跳任务（代表健康检查、取消计时、
/// 别的请求）不能因此一次都排不上。修复之前准入直接在 worker 线程上阻塞，心跳在这 300 ms 里一次都跑不了。
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn a_slow_ledger_does_not_starve_the_runtime_while_a_request_is_admitted() {
    let a = FakeUpstream::start(vec![Behavior::json(
        200,
        completion("ok", "hello", Some(usage(12, 30))),
    )])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options {
            admission: Some(Arc::new(SlowAdmission)),
            ..Options::default()
        },
    );
    let beats = Arc::new(AtomicUsize::new(0));
    let heartbeat = {
        let beats = Arc::clone(&beats);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_millis(10)).await;
                beats.fetch_add(1, Ordering::SeqCst);
            }
        })
    };
    tokio::time::sleep(Duration::from_millis(60)).await;
    let before = beats.load(Ordering::SeqCst);
    // 请求跑在运行时的 worker 线程上（spawn），不是测试自己的线程上
    let app = h.app.clone();
    let reply = tokio::spawn(async move { post(&app, &simple_body(100)).await })
        .await
        .unwrap();
    let during = beats.load(Ordering::SeqCst) - before;
    heartbeat.abort();
    assert_eq!(reply.status, 200);
    assert!(
        during >= 10,
        "the other tasks kept running while the admission blocked: {during} beats"
    );
}
