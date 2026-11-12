//! 计算服务 S4 端到端：失败即关闭、取消、尝试上限，以及给 S6 留的「完整回答在发布之前」接缝。
//!
//! 这些场景证明「先记状态再写字节」「库出问题时不放行、不出网」「客户端断开时取消并收尾」「用户只付发布了的那一次」。
//! 全部对本地假上游进行，**不连接任何真实的推理供应商**。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::collections::BTreeSet;
use std::time::Duration;

use fake_upstream::{Behavior, FakeUpstream, completion, usage};
use harness::{
    Endpoint, Harness, Options, build, p, post, postings, request_count, rows, simple_body,
};
use idoris_compute_contracts::attempt::{AttemptState, AttemptStateKind, SettleBasis};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{EndpointKey, RequestId};
use idoris_compute_contracts::permit::{
    Billing, CapabilityNeeds, DataClass, Retention, TenantDataPolicy,
};
use idoris_compute_contracts::pricing::Usage;
use idoris_router::cloud::{CloudCall, CloudError, CloudOutcome};
use idoris_tenancy::attempts::RequestOutcome;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn ok(id: &str) -> Behavior {
    Behavior::json(200, completion(id, "ok", Some(usage(12, 30))))
}

/// 直接驱动执行器（绕过 HTTP）。
fn direct_call(
    h: &Harness,
    request: &str,
    providers: &[&str],
    cancel: CancellationToken,
) -> CloudCall {
    let _ = h;
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
        billing: Billing::Paid,
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
        cancel,
    }
}

fn break_table(h: &Harness, table: &str) {
    let conn = rusqlite::Connection::open(&h.db_path).unwrap();
    conn.execute_batch(&format!("DROP TABLE {table}")).unwrap();
}

// ---------------------------------------------------------------------------
// 先记状态，再写字节；库出问题就不放行
// ---------------------------------------------------------------------------

#[tokio::test]
async fn if_the_write_ahead_record_cannot_be_written_nothing_is_sent() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    // 请求行和第一次尝试能写，但转移日志（WireWriteStarted 要写在这里）写不进去
    break_table(&h, "compute_attempt_events");
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["type"], "cloud_unavailable");
    assert_eq!(
        a.connection_count(),
        0,
        "the state could not be recorded, so no byte left the host"
    );
    // 内部细节不外泄
    assert!(!reply.text().contains("sqlite") && !reply.text().contains("no such table"));
}

#[tokio::test]
async fn if_the_request_cannot_even_be_recorded_it_fails_closed_before_any_upstream_work() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    {
        let conn = rusqlite::Connection::open(&h.db_path).unwrap();
        conn.execute_batch("ALTER TABLE compute_requests RENAME TO compute_requests_gone")
            .unwrap();
    }
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 503);
    assert_eq!(a.connection_count(), 0);
}

/// 发布已经落库、答案已经交给客户端，之后结算写不进去：不能把已经产生的回答吞掉（上游的钱已经花了），
/// 但库里的状态保持诚实——请求还开着，尝试停在「已执行、usage 已知」，等对账补上；不会出现「已结算」的假象。
#[tokio::test]
async fn a_settlement_failure_after_publish_still_delivers_the_answer_and_leaves_the_books_honest()
{
    let a = FakeUpstream::start(vec![ok("c1")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    break_table(&h, "compute_postings");
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["id"], "c1");
    let id = reply.request_id();
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(
        request.published_attempt,
        Some(0),
        "the publish was recorded before the bytes left"
    );
    assert_eq!(
        request.outcome, None,
        "not closed: settlement did not happen"
    );
    let attempt = &rows(&h, &id)[0];
    assert!(attempt.pays_user);
    assert_eq!(
        attempt.state,
        AttemptState::ExecutedUsageKnown {
            usage: Usage::new(12, 30, 0).unwrap()
        },
        "not settled: a failed settlement transaction rolls back completely"
    );
}

// ---------------------------------------------------------------------------
// 取消
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_request_cancels_the_upstream_call_and_is_settled_as_cancelled() {
    let a = FakeUpstream::start(vec![Behavior::Hang]).await;
    let b = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![
            Endpoint::adult("example-alpha", a.url()),
            Endpoint::adult("example-beta", b.url()),
        ],
        Options::default(),
    );
    let token = CancellationToken::new();
    let call = direct_call(
        &h,
        "req-cancel-1",
        &["example-alpha", "example-beta"],
        token.clone(),
    );
    // 请求确实到了上游之后再取消（不睡固定时长：机器负载高时固定时长会让测试不稳）
    let canceller = async {
        assert!(a.wait_for_requests(1, Duration::from_secs(10)).await);
        token.cancel();
    };
    let (result, ()) = tokio::join!(h.cloud.executor().execute(call), canceller);
    assert!(matches!(result, Err(CloudError::Cancelled)), "{result:?}");
    assert!(
        a.client_closed(0, Duration::from_secs(3)).await,
        "the upstream call was cancelled"
    );
    assert_eq!(
        b.connection_count(),
        0,
        "a cancelled request does not fall over to another candidate"
    );

    let id = RequestId::new("req-cancel-1").unwrap();
    // 上游是否停计费不确定：保持 maybe_executed，成本保留待核销，不释放
    assert_eq!(rows(&h, &id)[0].state, AttemptState::MaybeExecuted);
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.reason_code.as_deref(), Some("client_cancelled"));
    assert_eq!(
        postings(&h, &id),
        vec![p(0, "hold", 41, "USD"), p(0, "release", 4110, "THB")]
    );
}

#[tokio::test]
async fn an_already_cancelled_request_never_touches_the_upstream() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let token = CancellationToken::new();
    token.cancel();
    let result = h
        .cloud
        .executor()
        .execute(direct_call(&h, "req-cancel-2", &["example-alpha"], token))
        .await;
    assert!(matches!(result, Err(CloudError::Cancelled)));
    assert_eq!(a.connection_count(), 0);
}

// ---------------------------------------------------------------------------
// 尝试上限
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_request_never_makes_more_upstream_calls_than_max_attempts() {
    let servers = [
        FakeUpstream::start(vec![Behavior::json(503, json!({"error": {}}))]).await,
        FakeUpstream::start(vec![Behavior::json(503, json!({"error": {}}))]).await,
        FakeUpstream::start(vec![Behavior::json(503, json!({"error": {}}))]).await,
        FakeUpstream::start(vec![ok("the-fourth-must-never-be-called")]).await,
    ];
    let h = build(
        ["example-a", "example-b", "example-c", "example-d"]
            .iter()
            .zip(&servers)
            .map(|(name, server)| Endpoint::adult(name, server.url()))
            .collect(),
        Options::default(), // max_attempts: 3
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502);
    assert_eq!(
        servers
            .iter()
            .map(FakeUpstream::request_count)
            .collect::<Vec<_>>(),
        vec![1, 1, 1, 0]
    );
    assert_eq!(rows(&h, &reply.request_id()).len(), 3);
}

// ---------------------------------------------------------------------------
// 给 S6 留的接缝：完整回答在发布之前
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_withheld_answer_charges_the_user_nothing_and_the_platform_absorbs_the_executed_cost() {
    let a = FakeUpstream::start(vec![ok("answer-the-safety-check-rejects")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let outcome = h
        .cloud
        .executor()
        .execute(direct_call(
            &h,
            "req-withheld",
            &["example-alpha"],
            CancellationToken::new(),
        ))
        .await
        .unwrap();
    let CloudOutcome::Complete(pending) = outcome else {
        panic!("a non-stream call returns a pending answer");
    };
    // 回答在手里，但还没有任何字节交给客户端
    assert!(pending.body().len() > 10);
    let id = RequestId::new("req-withheld").unwrap();
    assert_eq!(
        h.store.request_row(&id).unwrap().unwrap().published_attempt,
        None
    );
    let settled = pending.withhold("output_rejected").unwrap();
    assert_eq!(settled.outcome, RequestOutcome::Unanswered);
    assert_eq!(settled.charged_micros, None);

    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.reason_code.as_deref(), Some("output_rejected"));
    assert_eq!(request.charged_micros, None);
    // 上游确实执行了：实际成本记为平台成本（不是 hold，用量已知），用户侧预留整笔释放
    assert_eq!(
        postings(&h, &id),
        vec![
            p(0, "platform_cost", 15, "USD"),
            p(0, "release", 4110, "THB")
        ]
    );
    assert_eq!(
        rows(&h, &id)[0].state,
        AttemptState::Settled {
            basis: SettleBasis::ActualUsage(Usage::new(12, 30, 0).unwrap())
        }
    );
    assert!(!rows(&h, &id)[0].pays_user);
}

#[tokio::test]
async fn dropping_a_pending_answer_without_deciding_settles_it_as_unpublished() {
    let a = FakeUpstream::start(vec![ok("c")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let outcome = h
        .cloud
        .executor()
        .execute(direct_call(
            &h,
            "req-dropped",
            &["example-alpha"],
            CancellationToken::new(),
        ))
        .await
        .unwrap();
    drop(outcome);
    let id = RequestId::new("req-dropped").unwrap();
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Unanswered));
    assert_eq!(request.charged_micros, None);
    assert_eq!(
        rows(&h, &id)
            .iter()
            .map(|r| r.state_kind())
            .collect::<Vec<_>>(),
        vec![AttemptStateKind::Settled]
    );
}

#[tokio::test]
async fn publishing_is_the_only_way_a_user_gets_charged_and_it_happens_once() {
    let a = FakeUpstream::start(vec![ok("c")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let CloudOutcome::Complete(pending) = h
        .cloud
        .executor()
        .execute(direct_call(
            &h,
            "req-publish",
            &["example-alpha"],
            CancellationToken::new(),
        ))
        .await
        .unwrap()
    else {
        panic!("pending answer expected");
    };
    let answer = pending.publish().unwrap();
    let body: Value = serde_json::from_slice(&answer.body).unwrap();
    assert_eq!(body["id"], "c");
    assert_eq!(answer.settled.unwrap().charged_micros, Some(1320));
    let id = RequestId::new("req-publish").unwrap();
    assert_eq!(
        postings(&h, &id)
            .iter()
            .filter(|(_, kind, _, _)| kind == "user_price")
            .count(),
        1,
        "the user is charged exactly once"
    );
    assert_eq!(request_count(&h), 1);
}
