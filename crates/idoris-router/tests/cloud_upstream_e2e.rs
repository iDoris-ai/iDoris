//! 计算服务 S4 端到端：成功、各种上游故障、失败切换、发布边界、流式 usage、取消。
//!
//! 每个场景断言**三件事**：
//!
//! 1. 客户端看到什么（状态码、响应体、稳定原因码的响应头、流里的事件）；
//! 2. 尝试记录表里落了什么（每次尝试的状态、供应商请求号、失败标注、usage 判定）；
//! 3. 结算结果（用户付多少、平台付多少、哪些挂账待核销）。
//!
//! 全部对本地假上游（`127.0.0.1`）进行，**不连接任何真实的推理供应商**。
//!
//! 数字约定（见 `support/cloud_harness.rs`）：`simple_body(100)` 的输入估算是 11 token，
//! 对 THB 用户的价是每百万 token 输入 10 / 输出 40，上游成本是 USD 0.26 / 0.38。
//!
//! | 量 | 计算 | 微单位 |
//! |---|---|---|
//! | 预留价（max_tokens=100） | ceil(11×10 + 100×40) | 4110 微泰铢 |
//! | 预留成本 | ceil((11×0.26 + 100×0.38) × 1e6 / 1e6 的微美元) = ceil(40.86) | 41 微美元 |
//! | 实际价（12 入 30 出） | 12×10 + 30×40 | 1320 微泰铢 |
//! | 实际成本（12 入 30 出） | ceil(3.12 + 11.4) = ceil(14.52) | 15 微美元 |
//! | 实际价（12 入 2 出） | 12×10 + 2×40 | 200 微泰铢 |
//! | 实际成本（12 入 2 出） | ceil(3.12 + 0.76) = ceil(3.88) | 4 微美元 |
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::time::Duration;

use fake_upstream::{
    Behavior, FakeUpstream, SseStep, completion, content_event, done_event, usage, usage_event,
};
use harness::{Endpoint, Harness, Options, build, post, simple_body, sse_data};
use idoris_compute_contracts::admission::{AdmissionDecision, Admitted, Reservation};
use idoris_compute_contracts::attempt::{AttemptState, AttemptStateKind, SettleBasis};
use idoris_compute_contracts::ids::{EndpointKey, PriceVersion, RequestId};
use idoris_compute_contracts::money::Unit;
use idoris_compute_contracts::pricing::{Cost, Price, RateCard, Usage};
use idoris_compute_contracts::request::{AdmittedCandidate, RequestError};
use idoris_tenancy::attempts::{AttemptRow, RequestOutcome};
use serde_json::{Value, json};

const RESERVED_PRICE: i64 = 4110;
const RESERVED_COST: i64 = 41;

struct Setup {
    harness: Harness,
    alpha: FakeUpstream,
    beta: FakeUpstream,
}

async fn two_upstreams(alpha: Vec<Behavior>, beta: Vec<Behavior>) -> Setup {
    two_upstreams_with(alpha, beta, Options::default()).await
}

async fn two_upstreams_with(alpha: Vec<Behavior>, beta: Vec<Behavior>, options: Options) -> Setup {
    let alpha = FakeUpstream::start(alpha).await;
    let beta = FakeUpstream::start(beta).await;
    let harness = build(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        options,
    );
    Setup {
        harness,
        alpha,
        beta,
    }
}

fn rows(h: &Harness, id: &RequestId) -> Vec<AttemptRow> {
    h.store.attempt_rows(id).unwrap()
}

/// (尝试序号, 类别, 金额, 单位) 的分录摘要，按写入顺序。
fn postings(h: &Harness, id: &RequestId) -> Vec<(u8, String, i64, String)> {
    h.store
        .postings(id)
        .unwrap()
        .into_iter()
        .map(|p| {
            (
                p.attempt.index().0,
                p.kind,
                p.money.micros,
                p.money.unit.to_string(),
            )
        })
        .collect()
}

fn p(attempt: u8, kind: &str, micros: i64, unit: &str) -> (u8, String, i64, String) {
    (attempt, kind.to_string(), micros, unit.to_string())
}

fn ok_completion(id: &str, usage_value: Option<Value>) -> Behavior {
    Behavior::json(200, completion(id, "hello from upstream", usage_value))
}

fn state_kinds(h: &Harness, id: &RequestId) -> Vec<AttemptStateKind> {
    rows(h, id).iter().map(AttemptRow::state_kind).collect()
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

// ---------------------------------------------------------------------------
// 成功
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_non_stream_request_passes_tools_and_structured_output_and_bills_actual_usage() {
    let tool_call_response = json!({
        "id": "chatcmpl-ok",
        "object": "chat.completion",
        "choices": [{"index": 0, "finish_reason": "tool_calls", "message": {
            "role": "assistant", "content": null,
            "tool_calls": [{"id": "call_1", "type": "function",
                "function": {"name": "get_weather", "arguments": "{\"city\":\"Bangkok\"}"}}]}}],
        "usage": usage(12, 30),
    });
    let alpha = FakeUpstream::start(vec![Behavior::json_with_header(
        200,
        tool_call_response.clone(),
        "x-request-id",
        "req-from-header",
    )])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options::default(),
    );
    let tools = json!([{"type": "function", "function": {"name": "get_weather",
        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}]);
    let response_format =
        json!({"type": "json_schema", "json_schema": {"name": "w", "schema": {"type": "object"}}});
    let mut body = simple_body(100);
    body["tools"] = tools.clone();
    body["tool_choice"] = json!("auto");
    body["response_format"] = response_format.clone();
    body["user"] = json!("student-4711");
    body["metadata"] = json!({"school": "A"});
    // 输入估算覆盖 messages / tools / tool_choice / response_format（保守估算），预留价 = 估算 × 10 + 100 × 40
    let estimate = idoris_tenancy::budget::estimate_tokens(
        &format!(
            "{}{}{}{}",
            body["messages"], body["tools"], body["tool_choice"], body["response_format"]
        ),
        "cloud",
    ) as i64;
    let reserved_price = estimate * 10 + 4000;
    assert!(
        reserved_price > RESERVED_PRICE,
        "tools and a schema raise the reservation"
    );

    let reply = post(&h.app, &body).await;

    // ---- 客户端看到什么：上游的工具调用响应原样回传，没有降级头 ----
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json(), tool_call_response);
    assert_eq!(reply.header("content-type"), Some("application/json"));
    assert!(reply.header("x-idoris-reason").is_none());
    assert!(reply.header("x-idoris-degraded").is_none());

    // ---- 上游收到什么：白名单字段、强制的 model / max_tokens，没有能识别用户的字段 ----
    let requests = alpha.requests();
    assert_eq!(requests.len(), 1);
    let sent = requests[0].json();
    assert_eq!(requests[0].path, "/v1/chat/completions");
    assert_eq!(
        requests[0].header("authorization"),
        Some(format!("Bearer {}", harness::KEY).as_str())
    );
    assert_eq!(sent["model"], "example-org/chat-70b");
    assert_eq!(sent["max_tokens"], 100);
    assert_eq!(sent["tools"], tools);
    assert_eq!(sent["tool_choice"], "auto");
    assert_eq!(sent["response_format"], response_format);
    assert_eq!(sent["stream"], false);
    assert!(sent.get("user").is_none() && sent.get("metadata").is_none());

    // ---- 尝试记录 ----
    let id = reply.request_id();
    let attempts = rows(&h, &id);
    assert_eq!(attempts.len(), 1);
    let attempt = &attempts[0];
    assert_eq!(
        attempt.state,
        AttemptState::Settled {
            basis: SettleBasis::ActualUsage(Usage::new(12, 30, 0).unwrap())
        }
    );
    assert!(attempt.pays_user);
    assert_eq!(attempt.provider, "example-alpha");
    assert_eq!(attempt.price_version, "pv-example-alpha");
    assert_eq!(attempt.effective_max_tokens, 100);
    // 正文里的 id 优先于响应头里的
    assert_eq!(attempt.supplier_request_id.as_deref(), Some("chatcmpl-ok"));
    assert_eq!(attempt.supplier_id_anomaly, None);
    assert_eq!(attempt.usage_claim.as_ref().unwrap()["verdict"], "valid");
    let events: Vec<AttemptStateKind> = h
        .store
        .events(&attempt.attempt)
        .unwrap()
        .iter()
        .map(|e| e.to)
        .collect();
    assert_eq!(
        events,
        vec![
            AttemptStateKind::Sending,
            AttemptStateKind::MaybeExecuted,
            AttemptStateKind::ExecutedUsageKnown,
            AttemptStateKind::Settled
        ],
        "write-ahead: sending and maybe_executed are recorded before the response exists"
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Answered));
    assert_eq!(request.published_attempt, Some(0));
    assert_eq!(request.reason_code, None);
    assert_eq!(request.charged_micros, Some(1320));
    assert_eq!(request.charged_basis.as_deref(), Some("actual_usage"));
    assert_eq!(request.reserved_micros, Some(reserved_price));
    assert_eq!(request.overage_micros, Some(0));
    assert_eq!(request.billing, "paid");
    assert_eq!(request.payer_account, "acct-default");

    // ---- 结算：用户付实际价，平台成本按实际用量，用户侧预留释放差额 ----
    assert_eq!(
        postings(&h, &id),
        vec![
            p(0, "user_price", 1320, "THB"),
            p(0, "platform_cost", 15, "USD"),
            p(0, "release", reserved_price - 1320, "THB"),
        ]
    );
}

#[tokio::test]
async fn tool_results_sent_back_by_the_client_reach_the_upstream_verbatim() {
    let alpha = FakeUpstream::start(vec![ok_completion("c2", Some(usage(40, 9)))]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options::default(),
    );
    let messages = json!([
        {"role": "user", "content": "weather in Bangkok?"},
        {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Bangkok\"}"}}]},
        {"role": "tool", "tool_call_id": "call_1", "content": "31C and humid"}
    ]);
    let reply = post(
        &h.app,
        &json!({"model": "idoris/agent", "messages": messages, "max_tokens": 100}),
    )
    .await;
    assert_eq!(reply.status, 200);
    assert_eq!(alpha.requests()[0].json()["messages"], messages);
}

#[tokio::test]
async fn a_slow_but_in_time_upstream_is_not_a_failure() {
    let alpha = FakeUpstream::start(vec![Behavior::slow_json(
        200,
        completion("slow", "x", Some(usage(12, 30))),
        Duration::from_millis(300),
    )])
    .await;
    let beta = FakeUpstream::start(vec![]).await;
    let h = build(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    assert!(
        reply.header("x-idoris-reason").is_none(),
        "no failover happened"
    );
    assert_eq!(beta.request_count(), 0);
    assert_eq!(rows(&h, &reply.request_id()).len(), 1);
}

#[tokio::test]
async fn a_stream_hides_the_usage_block_unless_the_client_asked_and_still_bills_it() {
    for client_wants_usage in [false, true] {
        let alpha = FakeUpstream::start(vec![Behavior::sse(vec![
            SseStep::Send(content_event("chatcmpl-s", "Hel")),
            SseStep::Send(content_event("chatcmpl-s", "lo")),
            SseStep::Send(usage_event("chatcmpl-s", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ])])
        .await;
        let h = build(
            vec![Endpoint::adult("example-alpha", alpha.url())],
            Options::default(),
        );
        let mut body = simple_body(100);
        body["stream"] = json!(true);
        if client_wants_usage {
            body["stream_options"] = json!({"include_usage": true});
        }
        let reply = post(&h.app, &body).await;

        assert_eq!(reply.status, 200);
        assert_eq!(
            reply.header("content-type"),
            Some("text/event-stream; charset=utf-8")
        );
        let events = sse_data(&reply.text());
        let has_usage_block = events.iter().any(|e| e.contains("\"usage\""));
        assert_eq!(has_usage_block, client_wants_usage, "{events:?}");
        assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
        assert!(
            events.iter().any(|e| e.contains("\"Hel\""))
                && events.iter().any(|e| e.contains("\"lo\""))
        );

        // 上游收到的是流式请求，且要求了流内 usage（不管客户端要没要）
        let sent = alpha.requests()[0].json();
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["stream_options"], json!({"include_usage": true}));

        let id = reply.request_id();
        assert_eq!(
            rows(&h, &id)[0].state,
            AttemptState::Settled {
                basis: SettleBasis::ActualUsage(Usage::new(12, 2, 0).unwrap())
            }
        );
        assert_eq!(
            postings(&h, &id),
            vec![
                p(0, "user_price", 200, "THB"),
                p(0, "platform_cost", 4, "USD"),
                p(0, "release", RESERVED_PRICE - 200, "THB"),
            ]
        );
        assert_eq!(
            rows(&h, &id)[0].supplier_request_id.as_deref(),
            Some("chatcmpl-s")
        );
    }
}

#[tokio::test]
async fn a_stream_that_never_reports_usage_is_billed_at_the_reservation_and_held_for_reconciliation()
 {
    let alpha = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("chatcmpl-nu", "Hel")),
        SseStep::Send(content_event("chatcmpl-nu", "lo")),
        SseStep::Send(done_event()),
        SseStep::Finish,
    ])])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options::default(),
    );
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&h.app, &body).await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        sse_data(&reply.text()).last().map(String::as_str),
        Some("[DONE]")
    );

    let id = reply.request_id();
    let attempt = &rows(&h, &id)[0];
    assert_eq!(
        attempt.state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        }
    );
    assert_eq!(attempt.usage_claim.as_ref().unwrap()["verdict"], "absent");
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(request.charged_basis.as_deref(), Some("reserved_amount"));
    // 用户付预留价；上游成本按预留额挂账待核销（hold，不是 platform_cost）；没有可释放的预留
    assert_eq!(
        postings(&h, &id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ]
    );
}

/// 给错的 usage：负数、小数、缓存大于输入、总数对不上、输出超过发给上游的 max_tokens、不是对象。
/// 一律当作「usage 未知」：用户付预留价（不付上游声称的那个数），原因写进记录。
#[tokio::test]
async fn wrong_usage_is_never_taken_at_face_value() {
    let cases: Vec<(Value, &str)> = vec![
        (
            json!({"prompt_tokens": -5, "completion_tokens": 3}),
            "invalid:prompt_tokens_not_a_non_negative_integer",
        ),
        (
            json!({"prompt_tokens": 5.5, "completion_tokens": 3}),
            "invalid:prompt_tokens_not_a_non_negative_integer",
        ),
        (
            json!({"prompt_tokens": 5, "completion_tokens": 3, "prompt_tokens_details": {"cached_tokens": 99}}),
            "invalid:cached_exceeds_prompt",
        ),
        (
            json!({"prompt_tokens": 5, "completion_tokens": 3, "total_tokens": 1_000_000}),
            "invalid:total_tokens_mismatch",
        ),
        (
            // 输出远超发给上游的 max_tokens=100：上游不遵守 max_tokens，它的计数不可信
            json!({"prompt_tokens": 5, "completion_tokens": 900_000_000, "total_tokens": 900_000_005}),
            "invalid:completion_exceeds_max_tokens",
        ),
        (json!("lots"), "invalid:not_an_object"),
    ];
    for stream in [false, true] {
        for (wrong, verdict) in &cases {
            let behavior = if stream {
                Behavior::sse(vec![
                    SseStep::Send(content_event("w", "x")),
                    SseStep::Send(usage_event("w", wrong.clone())),
                    SseStep::Send(done_event()),
                    SseStep::Finish,
                ])
            } else {
                Behavior::json(200, completion("w", "x", Some(wrong.clone())))
            };
            let alpha = FakeUpstream::start(vec![behavior]).await;
            let h = build(
                vec![Endpoint::adult("example-alpha", alpha.url())],
                Options::default(),
            );
            let mut body = simple_body(100);
            body["stream"] = json!(stream);
            let reply = post(&h.app, &body).await;
            assert_eq!(reply.status, 200, "{wrong} stream={stream}");
            let id = reply.request_id();
            let attempt = &rows(&h, &id)[0];
            assert_eq!(
                attempt.state,
                AttemptState::Settled {
                    basis: SettleBasis::ReservedAmount
                },
                "{wrong} stream={stream}"
            );
            assert_eq!(
                attempt.usage_claim.as_ref().unwrap()["verdict"].as_str(),
                Some(*verdict),
                "{wrong} stream={stream}"
            );
            let request = h.store.request_row(&id).unwrap().unwrap();
            assert_eq!(
                request.charged_micros,
                Some(RESERVED_PRICE),
                "the user pays the reservation, not the bogus claim: {wrong} stream={stream}"
            );
            assert_eq!(request.overage_micros, Some(0));
        }
    }
}

/// 上游在每个 chunk 里重复同一个请求号是常态（幂等）；请求号中途变了：保留第一个并标注。
#[tokio::test]
async fn a_repeated_supplier_id_is_idempotent_and_a_changed_one_is_flagged() {
    let alpha = FakeUpstream::start(vec![
        Behavior::sse(vec![
            SseStep::Send(content_event("same-id", "a")),
            SseStep::Send(content_event("same-id", "b")),
            SseStep::Send(usage_event("same-id", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ]),
        Behavior::sse(vec![
            SseStep::Send(content_event("first-id", "a")),
            SseStep::Send(content_event("second-id", "b")),
            SseStep::Send(usage_event("second-id", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ]),
    ])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options::default(),
    );
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let first = post(&h.app, &body).await;
    let row = &rows(&h, &first.request_id())[0];
    assert_eq!(row.supplier_request_id.as_deref(), Some("same-id"));
    assert_eq!(row.supplier_id_anomaly, None);

    let second = post(&h.app, &body).await;
    let row = &rows(&h, &second.request_id())[0];
    assert_eq!(
        row.supplier_request_id.as_deref(),
        Some("first-id"),
        "the first id is kept"
    );
    assert_eq!(
        row.supplier_id_anomaly.as_deref(),
        Some("id_changed_midstream")
    );
    // 请求号对不上不影响结算
    assert_eq!(
        h.store
            .request_row(&second.request_id())
            .unwrap()
            .unwrap()
            .charged_micros,
        Some(200)
    );
}

/// 上游对不同的请求返回同一个请求号（缓存回放、实现有 bug）：两条记录都保留，后一条标注，不拒绝服务。
#[tokio::test]
async fn the_same_supplier_id_on_two_requests_keeps_both_rows_and_flags_the_second() {
    let alpha = FakeUpstream::start(vec![
        ok_completion("dup-1", Some(usage(12, 30))),
        ok_completion("dup-1", Some(usage(12, 30))),
    ])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options::default(),
    );
    let first = post(&h.app, &simple_body(100)).await;
    let second = post(&h.app, &simple_body(100)).await;
    assert_eq!((first.status.as_u16(), second.status.as_u16()), (200, 200));
    let a = &rows(&h, &first.request_id())[0];
    let b = &rows(&h, &second.request_id())[0];
    assert_eq!(a.supplier_id_anomaly, None);
    assert_eq!(
        b.supplier_id_anomaly.as_deref(),
        Some(format!("duplicate_of:{}", a.attempt).as_str())
    );
    // 对账：按供应商请求号反查得到两条，说明这个号对不上单一的一次尝试
    let found = h
        .store
        .attempts_by_supplier_request_id("example-alpha", "dup-1")
        .unwrap();
    assert_eq!(found.len(), 2);
    // 两个用户都按各自的实际用量付费
    for reply in [&first, &second] {
        assert_eq!(
            h.store
                .request_row(&reply.request_id())
                .unwrap()
                .unwrap()
                .charged_micros,
            Some(1320)
        );
    }
}

// ---------------------------------------------------------------------------
// 首字节前的失败切换
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_5xx_before_any_byte_switches_to_the_next_candidate_and_the_platform_absorbs_the_first() {
    let s = two_upstreams(
        vec![Behavior::json(
            503,
            json!({"error": {"message": "overloaded"}}),
        )],
        vec![ok_completion("from-beta", Some(usage(12, 30)))],
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    let h = &s.harness;

    // ---- 客户端：拿到第二个候选的回答，带稳定的失败切换原因码 ----
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["id"], "from-beta");
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    assert_eq!(reply.header("x-idoris-degraded"), Some("true"));
    assert_eq!((s.alpha.request_count(), s.beta.request_count()), (1, 1));

    // ---- 尝试记录 ----
    let id = reply.request_id();
    let attempts = rows(h, &id);
    assert_eq!(attempts.len(), 2);
    // 5xx 不能证明没执行：第一次尝试保持 maybe_executed，不释放、不结算
    assert_eq!(attempts[0].state, AttemptState::MaybeExecuted);
    assert!(!attempts[0].pays_user);
    assert_eq!(attempts[0].failure_kind.as_deref(), Some("http_status"));
    assert_eq!(attempts[0].failure_http_status, Some(503));
    assert_eq!(attempts[0].provider, "example-alpha");
    assert_eq!(attempts[1].provider, "example-beta");
    assert!(attempts[1].pays_user);
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.published_attempt, Some(1));
    assert_eq!(request.reason_code.as_deref(), Some("upstream_failover"));

    // ---- 结算：用户只付第二次；第一次的成本按预留额挂账（hold），它占用户额度的价预留整笔释放 ----
    assert_eq!(
        postings(h, &id),
        vec![
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB"),
            p(1, "user_price", 1320, "THB"),
            p(1, "platform_cost", 15, "USD"),
            p(1, "release", RESERVED_PRICE - 1320, "THB"),
        ]
    );
    assert_eq!(request.charged_micros, Some(1320), "the user pays once");
}

#[tokio::test]
async fn a_4xx_refusal_proves_non_execution_so_the_first_attempt_costs_nothing() {
    let s = two_upstreams(
        vec![Behavior::json(
            429,
            json!({"error": {"message": "rate limited"}}),
        )],
        vec![ok_completion("from-beta", Some(usage(12, 30)))],
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    let h = &s.harness;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    let id = reply.request_id();
    let attempts = rows(h, &id);
    assert_eq!(
        attempts[0].state,
        AttemptState::NotSent,
        "a 4xx is evidence of non-execution"
    );
    assert_eq!(attempts[0].failure_http_status, Some(429));
    // 平台没有成本：没有 hold，也没有 platform_cost，只释放它占的价预留
    assert_eq!(
        postings(h, &id),
        vec![
            p(0, "release", RESERVED_PRICE, "THB"),
            p(1, "user_price", 1320, "THB"),
            p(1, "platform_cost", 15, "USD"),
            p(1, "release", RESERVED_PRICE - 1320, "THB"),
        ]
    );
}

#[tokio::test]
async fn a_request_shaped_4xx_is_not_retried_elsewhere() {
    let s = two_upstreams(
        vec![Behavior::json(
            400,
            json!({"error": {"message": "context too long"}}),
        )],
        vec![ok_completion("never", None)],
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    let h = &s.harness;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.json()["error"]["type"], "upstream_error");
    assert_eq!(
        reply.json()["error"]["reason_code"],
        "upstream_rejected_request"
    );
    assert_eq!(
        s.beta.request_count(),
        0,
        "data minimization: no second upstream sees the input"
    );
    let id = reply.request_id();
    assert_eq!(state_kinds(h, &id), vec![AttemptStateKind::NotSent]);
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.reason_code.as_deref(), Some("upstream_rejected"));
    assert_eq!(request.charged_micros, None);
    assert_eq!(
        postings(h, &id),
        vec![p(0, "release", RESERVED_PRICE, "THB")]
    );
}

#[tokio::test]
async fn a_silent_upstream_times_out_and_the_request_switches_within_its_budget() {
    let s = two_upstreams(
        vec![Behavior::Hang],
        vec![ok_completion("from-beta", Some(usage(12, 30)))],
    )
    .await;
    let started = std::time::Instant::now();
    let reply = post(&s.harness.app, &simple_body(100)).await;
    let h = &s.harness;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    assert!(started.elapsed() < Duration::from_secs(5));
    let id = reply.request_id();
    let attempts = rows(h, &id);
    assert_eq!(
        attempts[0].state,
        AttemptState::MaybeExecuted,
        "a timeout proves nothing"
    );
    assert_eq!(
        attempts[0].failure_kind.as_deref(),
        Some("response_head_timeout")
    );
    assert_eq!(
        postings(h, &id)[..2],
        [
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB")
        ]
    );
    // 上游看到客户端（我们）断开了：超时的那次被取消
    assert!(s.alpha.client_closed(0, Duration::from_secs(3)).await);
}

#[tokio::test]
async fn a_connection_that_closes_before_any_response_byte_switches_too() {
    let s = two_upstreams(
        vec![Behavior::CloseBeforeResponse],
        vec![ok_completion("from-beta", Some(usage(12, 30)))],
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    let id = reply.request_id();
    let attempts = rows(&s.harness, &id);
    assert_eq!(attempts[0].state, AttemptState::MaybeExecuted);
    assert_eq!(attempts[0].failure_kind.as_deref(), Some("send_failed"));
    assert_eq!((s.alpha.request_count(), s.beta.request_count()), (1, 1));
    // 断开前可能已经执行：成本按预留额挂账（hold），不是释放；用户侧的价预留释放
    assert_eq!(
        postings(&s.harness, &id)[..2],
        [
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB")
        ]
    );
}

/// 连接被拒：没有字节到达上游，但 reqwest 的 `send` 把「连接」和「写字节」合在一个 future 里，没法在两者之间落库，
/// 所以先落了 `WireWriteStarted`（保守）。这次尝试因此保持 `maybe_executed`（成本挂账，等对账查无此单后释放），
/// 失败标注是 `connect_failed`，对账的人一眼能看出它是「没连上」。这是有意的保守，见 `idoris_upstream::cloud::client`。
#[tokio::test]
async fn a_refused_connection_is_recorded_conservatively_as_maybe_executed_with_a_connect_annotation()
 {
    let beta = FakeUpstream::start(vec![ok_completion("from-beta", Some(usage(12, 30)))]).await;
    let refused = FakeUpstream::refused_url().await;
    let h = build(
        vec![
            Endpoint::adult("example-alpha", refused),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    let id = reply.request_id();
    let attempts = rows(&h, &id);
    assert_eq!(attempts[0].state, AttemptState::MaybeExecuted);
    assert_eq!(attempts[0].failure_kind.as_deref(), Some("connect_failed"));
    assert_eq!(beta.request_count(), 1);
    // 保守：没连上也先按「可能执行」挂账（hold），等对账查无此单后才释放
    assert_eq!(
        postings(&h, &id)[..2],
        [
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB")
        ]
    );
}

#[tokio::test]
async fn when_every_candidate_fails_the_client_gets_a_stable_error_and_pays_nothing() {
    let s = two_upstreams(
        vec![Behavior::json(503, json!({"error": {}}))],
        vec![Behavior::json(502, json!({"error": {}}))],
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    let h = &s.harness;
    assert_eq!(reply.status, 502);
    assert_eq!(reply.json()["error"]["type"], "upstream_error");
    assert_eq!(reply.json()["error"]["reason_code"], "http_status");
    let id = reply.request_id();
    assert_eq!(
        state_kinds(h, &id),
        vec![
            AttemptStateKind::MaybeExecuted,
            AttemptStateKind::MaybeExecuted
        ]
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.reason_code.as_deref(), Some("upstream_unavailable"));
    assert_eq!(request.published_attempt, None);
    assert_eq!(
        request.charged_micros, None,
        "no attempt answered: the user pays 0"
    );
    // 两次尝试的成本都挂账待核销，用户侧预留全部释放
    assert_eq!(
        postings(h, &id),
        vec![
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB"),
            p(1, "hold", RESERVED_COST, "USD"),
            p(1, "release", RESERVED_PRICE, "THB"),
        ]
    );
}

#[tokio::test]
async fn every_candidate_is_re_admitted_before_the_switch_and_a_denied_one_is_never_called() {
    // 切换前按新候选重新准入：两次准入调用，对应两个候选，序号递增
    let admission = harness::RecordingAdmission::new();
    let s = two_upstreams_with(
        vec![Behavior::json(503, json!({"error": {}}))],
        vec![ok_completion("from-beta", Some(usage(12, 30)))],
        Options {
            admission: Some(admission.clone()),
            ..Options::default()
        },
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    assert_eq!(
        admission.calls(),
        vec![
            ("example-alpha".to_string(), 0),
            ("example-beta".to_string(), 1)
        ]
    );

    // 第二个候选被准入拒绝：不会调用它，请求以第一次失败的原因结束
    let admission = harness::RecordingAdmission::rejecting(&["example-beta"]);
    let s = two_upstreams_with(
        vec![Behavior::json(503, json!({"error": {}}))],
        vec![ok_completion("never", None)],
        Options {
            admission: Some(admission.clone()),
            ..Options::default()
        },
    )
    .await;
    let reply = post(&s.harness.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502);
    assert_eq!(s.beta.request_count(), 0);
    assert_eq!(admission.providers(), vec!["example-alpha", "example-beta"]);
    assert_eq!(rows(&s.harness, &reply.request_id()).len(), 1);
}

#[tokio::test]
async fn an_admission_denial_of_the_only_candidate_is_a_402_without_any_upstream_call() {
    let admission = harness::RecordingAdmission::rejecting(&["example-alpha"]);
    let alpha = FakeUpstream::start(vec![ok_completion("never", None)]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url())],
        Options {
            admission: Some(admission),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 402);
    assert_eq!(reply.json()["error"]["type"], "budget_exhausted");
    assert_eq!(reply.header("x-idoris-reason"), Some("budget_exhausted"));
    assert_eq!(alpha.connection_count(), 0);
    let request = h.store.request_row(&reply.request_id()).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Rejected));
    assert_eq!(request.reason_code.as_deref(), Some("budget_exhausted"));
    assert!(rows(&h, &reply.request_id()).is_empty());
}

// ---------------------------------------------------------------------------
// 流式：首字节之前切换；首字节之后禁止切换
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stream_that_fails_before_its_first_event_switches_and_the_client_sees_only_the_second() {
    let s = two_upstreams(
        // 200 的流，但第一个事件就是错误事件
        vec![Behavior::sse(vec![
            SseStep::Send("data: {\"error\":{\"message\":\"overloaded\"}}\n\n".to_string()),
            SseStep::Finish,
        ])],
        vec![Behavior::sse(vec![
            SseStep::Send(content_event("from-beta", "Hel")),
            SseStep::Send(content_event("from-beta", "lo")),
            SseStep::Send(usage_event("from-beta", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ])],
    )
    .await;
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&s.harness.app, &body).await;
    let h = &s.harness;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    let text = reply.text();
    assert!(
        !text.contains("overloaded"),
        "nothing of the failed attempt reached the client"
    );
    assert!(text.contains("from-beta"));
    let id = reply.request_id();
    let attempts = rows(h, &id);
    // 2xx 之后出的问题：确认执行了、usage 未知 -> 按预留额结算为平台成本（hold）
    assert_eq!(
        attempts[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        }
    );
    assert!(!attempts[0].pays_user);
    assert_eq!(
        attempts[0].failure_kind.as_deref(),
        Some("stream_error_event")
    );
    assert_eq!(
        postings(h, &id),
        vec![
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB"),
            p(1, "user_price", 200, "THB"),
            p(1, "platform_cost", 4, "USD"),
            p(1, "release", RESERVED_PRICE - 200, "THB"),
        ]
    );
}

#[tokio::test]
async fn a_stream_that_closes_right_after_the_headers_switches_before_any_byte_reaches_the_client()
{
    let s = two_upstreams(
        // 200 + 事件流头，然后什么事件都没有就断开
        vec![Behavior::sse(vec![SseStep::Close])],
        vec![Behavior::sse(vec![
            SseStep::Send(content_event("from-beta", "ok")),
            SseStep::Send(usage_event("from-beta", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ])],
    )
    .await;
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&s.harness.app, &body).await;
    let h = &s.harness;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    assert!(reply.text().contains("from-beta"));
    let id = reply.request_id();
    let attempts = rows(h, &id);
    assert_eq!(
        attempts[0].failure_kind.as_deref(),
        Some("stream_truncated")
    );
    assert!(!attempts[0].pays_user);
    assert_eq!(attempts[1].provider, "example-beta");
    assert_eq!(
        h.store.request_row(&id).unwrap().unwrap().charged_micros,
        Some(200),
        "the user pays only the answering attempt"
    );
}

/// **发布边界**：首字节已经交给客户端之后上游断开。不允许透明重试或切换：
/// 没有第二次上游调用、没有第二次尝试；流以错误事件结束；按预留额结算（usage 未知）；
/// 持久化的请求状态让契约继续拒绝切换（`PublishBoundary`），数据库也拒绝追加尝试。
#[tokio::test]
async fn after_the_first_byte_an_upstream_failure_never_switches_and_the_stream_ends_with_an_error()
{
    let s = two_upstreams(
        vec![Behavior::sse(vec![
            SseStep::Send(content_event("chatcmpl-cut", "Hel")),
            SseStep::Send(content_event("chatcmpl-cut", "lo")),
            SseStep::Close, // 没有终结块：截断
        ])],
        vec![ok_completion("must-never-be-called", Some(usage(12, 30)))],
    )
    .await;
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&s.harness.app, &body).await;
    let h = &s.harness;

    // ---- 客户端：已经收到的内容 + 一个带稳定代码的错误事件，没有 [DONE] ----
    assert_eq!(
        reply.status, 200,
        "the status was already committed with the first byte"
    );
    let events = sse_data(&reply.text());
    assert!(events.iter().any(|e| e.contains("\"Hel\"")));
    assert!(events.iter().any(|e| e.contains("\"lo\"")));
    let last: Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["error"]["type"], "upstream_error");
    assert_eq!(last["error"]["reason_code"], "stream_truncated");
    assert!(
        !events.iter().any(|e| e == "[DONE]"),
        "[DONE] means normal completion"
    );

    // ---- 没有第二次上游调用 ----
    assert_eq!(s.alpha.request_count(), 1);
    assert_eq!(s.beta.request_count(), 0);
    assert_eq!(s.beta.connection_count(), 0);

    // ---- 尝试记录：一次尝试；已发布；usage 未知，按预留额结算，成本挂账 ----
    let id = reply.request_id();
    let attempts = rows(h, &id);
    assert_eq!(attempts.len(), 1, "no second attempt exists");
    assert_eq!(
        attempts[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        }
    );
    assert!(attempts[0].pays_user);
    assert_eq!(
        attempts[0].failure_kind.as_deref(),
        Some("stream_truncated")
    );
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Answered));
    assert_eq!(request.published_attempt, Some(0));
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(request.charged_basis.as_deref(), Some("reserved_amount"));
    assert_eq!(
        postings(h, &id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ]
    );

    // ---- 边界在持久化的状态里：重建出来的请求，契约继续拒绝切换 ----
    let mut loaded = h.store.load_request(&id).unwrap().unwrap();
    assert!(loaded.published().is_some());
    let next = AdmittedCandidate {
        decision: AdmissionDecision::Allow {
            admitted: Admitted {
                endpoint: EndpointKey::new("example-beta", "example-org/chat-70b", "openai-compat")
                    .unwrap(),
                payer: loaded.payment().payer().clone(),
                effective_max_tokens: 100,
                reservation: Reservation {
                    price: Price::from_micros(Unit::Thb, 1).unwrap(),
                    cost: Cost::from_micros(Unit::Usd, 1).unwrap(),
                },
            },
        },
        price_version: PriceVersion::new("pv").unwrap(),
        cost_rates: RateCard::new(
            Cost::from_micros(Unit::Usd, 1).unwrap(),
            Cost::from_micros(Unit::Usd, 1).unwrap(),
            None,
        )
        .unwrap(),
        price_rates: RateCard::new(
            Price::from_micros(Unit::Thb, 1).unwrap(),
            Price::from_micros(Unit::Thb, 1).unwrap(),
            None,
        )
        .unwrap(),
    };
    assert_eq!(
        loaded.begin_next_attempt(next).unwrap_err(),
        RequestError::PublishBoundary
    );
    // 数据库自己也拒绝：绕过契约直接插一行新的尝试
    let conn = rusqlite::Connection::open(&h.db_path).unwrap();
    let inserted = conn.execute(
        "INSERT INTO compute_attempts
           (attempt_id, request_id, attempt_index, provider, model, endpoint, price_version,
            effective_max_tokens, reserved_price_micros, reserved_price_unit, reserved_cost_micros,
            reserved_cost_unit, snapshot_json, state, state_json, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, 1, 'p', 'm', 'e', 'pv', 1, 0, 'THB', 0, 'USD', '{}', 'admitted', '{\"state\":\"admitted\"}', 0, 0)",
        rusqlite::params![format!("{id}#1"), id.as_str()],
    );
    // 这个请求既已发布又已关闭，两个触发器都会拒绝（哪个先触发不保证）；
    // 只针对发布边界的触发器见 idoris-tenancy 的 `the_publish_boundary_is_enforced_by_the_database_too`。
    let message = inserted.unwrap_err().to_string();
    assert!(
        message.contains("publish boundary") || message.contains("request is closed"),
        "the database refuses a new attempt: {message}"
    );
}

#[tokio::test]
async fn a_stream_that_goes_quiet_after_the_first_byte_ends_with_a_timeout_error_and_no_retry() {
    let s = two_upstreams(
        vec![Behavior::sse(vec![
            SseStep::Send(content_event("c", "Hel")),
            SseStep::Sleep(Duration::from_secs(10)),
        ])],
        vec![ok_completion("never", None)],
    )
    .await;
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&s.harness.app, &body).await;
    assert_eq!(reply.status, 200);
    let events = sse_data(&reply.text());
    let last: Value = serde_json::from_str(events.last().unwrap()).unwrap();
    assert_eq!(last["error"]["reason_code"], "stream_idle_timeout");
    assert_eq!(s.beta.request_count(), 0);
    let id = reply.request_id();
    assert_eq!(rows(&s.harness, &id).len(), 1);
    assert_eq!(
        s.harness
            .store
            .request_row(&id)
            .unwrap()
            .unwrap()
            .charged_basis
            .as_deref(),
        Some("reserved_amount")
    );
}

#[tokio::test]
async fn usage_that_arrived_before_the_connection_died_is_the_final_accounting() {
    // 内容、最终 usage 都到了，只是没有 [DONE] 就结束了：完整；补一个 [DONE]；按实际用量付费
    let s = two_upstreams(
        vec![Behavior::sse(vec![
            SseStep::Send(content_event("c", "Hel")),
            SseStep::Send(usage_event("c", usage(12, 2))),
            SseStep::Finish,
        ])],
        vec![],
    )
    .await;
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&s.harness.app, &body).await;
    let events = sse_data(&reply.text());
    assert_eq!(events.last().map(String::as_str), Some("[DONE]"));
    let request = s
        .harness
        .store
        .request_row(&reply.request_id())
        .unwrap()
        .unwrap();
    assert_eq!(request.charged_micros, Some(200));
    assert_eq!(request.charged_basis.as_deref(), Some("actual_usage"));
}

// ---------------------------------------------------------------------------
// 取消
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_client_that_disconnects_mid_stream_cancels_the_upstream_and_is_billed_the_reservation() {
    let s = two_upstreams(
        vec![Behavior::sse(vec![
            SseStep::Send(content_event("c", "Hel")),
            SseStep::Hang,
        ])],
        vec![ok_completion("never", None)],
    )
    .await;
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let response = harness::send(&s.harness.app, harness::chat_request(&body)).await;
    assert_eq!(response.status(), 200);
    let request_id = RequestId::new(
        response
            .headers()
            .get("x-idoris-record-id")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    // 读到第一个内容块，然后客户端断开（丢弃响应体）
    let mut stream = response.into_body();
    let frame = http_body_util::BodyExt::frame(&mut stream)
        .await
        .unwrap()
        .unwrap();
    assert!(String::from_utf8_lossy(frame.data_ref().unwrap()).contains("\"Hel\""));
    drop(stream);

    // 上游看到连接被关（取消），请求被收尾；用户付预留价（usage 未知，契约的规则），成本挂账待核销
    assert!(s.alpha.client_closed(0, Duration::from_secs(3)).await);
    wait_closed(&s.harness, &request_id).await;
    let h = &s.harness;
    let attempts = rows(h, &request_id);
    assert_eq!(attempts.len(), 1);
    assert_eq!(
        attempts[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ReservedAmount
        }
    );
    assert_eq!(attempts[0].failure_kind.as_deref(), Some("client_gone"));
    let request = h.store.request_row(&request_id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Answered));
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(
        s.beta.request_count(),
        0,
        "a disconnect is not a reason to call another upstream"
    );
    assert_eq!(
        postings(h, &request_id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ]
    );
}

/// 上游的「取消」能力是 false（断开连接后可能还在生成、还在计费）：我们关掉了连接，但是否停计费不确定，
/// 所以不下结论——尝试保持 `maybe_executed`（成本按预留额挂账、不释放），用户仍按预留价付费（契约的规则），
/// 对账扫描能直接找到它。
#[tokio::test]
async fn a_disconnect_from_an_upstream_that_may_keep_billing_stays_maybe_executed() {
    let alpha = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("c", "Hel")),
        SseStep::Hang,
    ])])
    .await;
    let h = build(
        vec![Endpoint::adult("example-alpha", alpha.url()).no_cancellation()],
        Options::default(),
    );
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let response = harness::send(&h.app, harness::chat_request(&body)).await;
    let request_id = RequestId::new(
        response
            .headers()
            .get("x-idoris-record-id")
            .unwrap()
            .to_str()
            .unwrap(),
    )
    .unwrap();
    let mut stream = response.into_body();
    http_body_util::BodyExt::frame(&mut stream)
        .await
        .unwrap()
        .unwrap();
    drop(stream);

    assert!(alpha.client_closed(0, Duration::from_secs(3)).await);
    wait_closed(&h, &request_id).await;
    let attempts = rows(&h, &request_id);
    assert_eq!(
        attempts[0].state,
        AttemptState::MaybeExecuted,
        "whether billing stopped is not known"
    );
    assert!(attempts[0].pays_user);
    assert_eq!(attempts[0].failure_kind.as_deref(), Some("client_gone"));
    let request = h.store.request_row(&request_id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Answered));
    assert_eq!(request.charged_micros, Some(RESERVED_PRICE));
    assert_eq!(request.charged_basis.as_deref(), Some("reserved_amount"));
    assert_eq!(
        postings(&h, &request_id),
        vec![
            p(0, "user_price", RESERVED_PRICE, "THB"),
            p(0, "hold", RESERVED_COST, "USD"),
        ]
    );
    // 对账能直接找到它
    let held = h
        .store
        .attempts_in_states(&[AttemptStateKind::MaybeExecuted], 10)
        .unwrap();
    assert_eq!(held.len(), 1);
}

/// 客户端在任何一个字节之前断开（这里是丢弃整个请求 future）：向上游取消；上游是否停计费不确定，
/// 尝试保持 `maybe_executed`（成本保留待核销，**不释放**）；没有发布，用户付 0。
#[tokio::test]
async fn a_client_that_disconnects_before_any_byte_leaves_maybe_executed_held_and_charges_nothing()
{
    let s = two_upstreams(vec![Behavior::Hang], vec![ok_completion("never", None)]).await;
    let request = harness::chat_request(&simple_body(100));
    let app = s.harness.app.clone();
    let task = tokio::spawn(async move { harness::send(&app, request).await });
    // 等到请求确实到了上游再让客户端离开（不睡固定时长：机器负载高时固定时长会让测试不稳）
    assert!(s.alpha.wait_for_requests(1, Duration::from_secs(10)).await);
    task.abort();
    assert!(
        task.await.unwrap_err().is_cancelled(),
        "the request was still waiting on the upstream when the client left"
    );

    assert!(
        s.alpha.client_closed(0, Duration::from_secs(3)).await,
        "the upstream call was cancelled"
    );
    // 收尾发生在 future 被丢弃的那一刻（Drop）
    let held = s
        .harness
        .store
        .attempts_in_states(&[AttemptStateKind::MaybeExecuted], 10)
        .unwrap();
    assert_eq!(held.len(), 1);
    let id = held[0].request.clone();
    wait_closed(&s.harness, &id).await;
    let request = s.harness.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.charged_micros, None);
    assert_eq!(
        postings(&s.harness, &id),
        vec![
            p(0, "hold", RESERVED_COST, "USD"),
            p(0, "release", RESERVED_PRICE, "THB"),
        ],
        "maybe_executed keeps its cost on the books; only the user-side reservation is released"
    );
    assert_eq!(s.beta.request_count(), 0);
}
