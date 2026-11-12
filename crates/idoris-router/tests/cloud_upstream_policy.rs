//! 计算服务 S4 端到端：端点许可表的硬约束、认证与调用方解析、请求校验、默认关闭、密钥与正文不落盘。
//!
//! 这一组回答「儿童数据会不会被送到不合规的上游」：`child_edu` 请求在今天的条件下（S8 的核验结论：没有任何端点能
//! 合规承接 child_edu）被拒绝，返回稳定原因码，**没有任何上游调用发生**——用假服务的连接计数证明，
//! 不是看日志。全部对本地假上游进行，**不连接任何真实的推理供应商**。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::Request;
use fake_upstream::{
    Behavior, FakeUpstream, SseStep, completion, content_event, done_event, usage, usage_event,
};
use harness::{
    Caller, DAY_MS, Endpoint, KEY, NOW_MS, Options, RecordingAdmission, VERIFIED_MS, build, p,
    post, postings, request_count, rows, simple_body,
};
use idoris_compute_contracts::attempt::AttemptStateKind;
use idoris_contracts::DeployMode;
use idoris_contracts::common::PrivacyClass;
use idoris_router::auth::VirtualKeyAuthenticator;
use idoris_router::cloud::ManualClock;
use idoris_tenancy::attempts::RequestOutcome;
use idoris_tenancy::virtual_key::MintedVirtualKey;
use idoris_tenancy::virtual_key::store::{VirtualKeyScope, VirtualKeyStore};
use serde_json::{Value, json};

fn ok(id: &str) -> Behavior {
    Behavior::json(200, completion(id, "ok", Some(usage(12, 30))))
}

/// 今天的现实（S8）：没有任何端点同时满足「ZDR + 已签 DPA + 允许 child_edu」。
/// 成人端点留存有例外、DPA 未签；调研过但没核实出结论的端点按最坏值写（允许的类别为空）。
fn todays_reality(a: &FakeUpstream, b: &FakeUpstream, c: &FakeUpstream) -> Vec<Endpoint> {
    let mut unverified = Endpoint::adult("example-gamma", c.url());
    unverified.classes = vec![];
    unverified.retention = "retained";
    unverified.regions = vec!["unknown"];
    unverified.stream_usage = false;
    unverified.tool_calling = false;
    unverified.structured_output = false;
    let mut official = Endpoint::adult("example-beta", b.url());
    official.retention = "retained";
    official.classes = vec!["adult"];
    vec![
        Endpoint::adult("example-alpha", a.url()),
        official,
        unverified,
    ]
}

// ---------------------------------------------------------------------------
// child_edu
// ---------------------------------------------------------------------------

#[tokio::test]
async fn child_edu_in_the_cloud_is_rejected_today_with_zero_upstream_calls() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let b = FakeUpstream::start(vec![ok("never")]).await;
    let c = FakeUpstream::start(vec![ok("never")]).await;
    let admission = RecordingAdmission::new();
    let h = build(
        todays_reality(&a, &b, &c),
        Options {
            callers: vec![Caller::child_edu()],
            admission: Some(admission.clone()),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;

    // ---- 客户端：稳定原因码 ----
    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["type"], "no_compliant_endpoint");
    assert_eq!(
        reply.json()["error"]["reason_code"],
        "no_compliant_endpoint"
    );

    // ---- 没有任何上游调用：不是「请求被拒绝了」，是连 TCP 连接都没有建立过 ----
    for server in [&a, &b, &c] {
        assert_eq!(server.connection_count(), 0);
        assert_eq!(server.request_count(), 0);
    }
    assert!(
        admission.calls().is_empty(),
        "no candidate survived the hard constraints, so none was even admitted"
    );

    // ---- 留一行审计痕迹：被拒绝，没有尝试，没有分录 ----
    let id = reply.request_id();
    let request = h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(request.outcome, Some(RequestOutcome::Rejected));
    assert_eq!(
        request.reason_code.as_deref(),
        Some("no_compliant_endpoint")
    );
    assert_eq!(request.data_classes, "child_edu");
    assert!(rows(&h, &id).is_empty());
    assert!(postings(&h, &id).is_empty());
}

#[tokio::test]
async fn child_edu_streaming_is_refused_before_any_upstream_or_admission() {
    // P0：edu 一律非流式（设计稿 §6）。即使有合规端点，流式请求也在任何调用之前被拒绝。
    let z = FakeUpstream::start(vec![ok("never")]).await;
    let admission = RecordingAdmission::new();
    let h = build(
        vec![Endpoint::child_edu_compliant("example-zdr", z.url())],
        Options {
            callers: vec![Caller::child_edu()],
            admission: Some(admission.clone()),
            ..Options::default()
        },
    );
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&h.app, &body).await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.json()["error"]["type"], "streaming_not_allowed");
    assert_eq!(z.connection_count(), 0);
    assert!(admission.calls().is_empty());
}

#[tokio::test]
async fn child_edu_only_ever_reaches_a_zdr_signed_dpa_endpoint_even_when_listed_last() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let b = FakeUpstream::start(vec![ok("never")]).await;
    let z = FakeUpstream::start(vec![ok("from-zdr")]).await;
    let admission = RecordingAdmission::new();
    let h = build(
        vec![
            Endpoint::adult("example-alpha", a.url()),
            {
                // 有例外的留存、已签 DPA：只能承接成人数据（S0 的许可表也不允许它自己声明 child_edu）
                let mut e = Endpoint::adult("example-beta", b.url());
                e.classes = vec!["adult"];
                e.retention = "with_exceptions";
                e.dpa = "signed";
                e
            },
            Endpoint::child_edu_compliant("example-zdr", z.url()),
        ],
        Options {
            callers: vec![Caller::child_edu()],
            admission: Some(admission.clone()),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["id"], "from-zdr");
    assert_eq!((a.connection_count(), b.connection_count()), (0, 0));
    assert_eq!(z.request_count(), 1);
    assert_eq!(admission.providers(), vec!["example-zdr"]);
    let attempts = rows(&h, &reply.request_id());
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].provider, "example-zdr");
}

#[tokio::test]
async fn a_failed_compliant_endpoint_never_falls_back_to_a_non_compliant_one() {
    // 兜底同样要过硬约束：唯一合规的端点失败了，不会去找一个不合规的端点「凑合」
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let z = FakeUpstream::start(vec![Behavior::json(503, json!({"error": {}}))]).await;
    let h = build(
        vec![
            Endpoint::child_edu_compliant("example-zdr", z.url()),
            Endpoint::adult("example-alpha", a.url()),
        ],
        Options {
            callers: vec![Caller::child_edu()],
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502);
    assert_eq!(z.request_count(), 1);
    assert_eq!(
        a.connection_count(),
        0,
        "the fallback candidate is filtered by the same hard constraints"
    );
    assert_eq!(rows(&h, &reply.request_id()).len(), 1);
}

#[tokio::test]
async fn the_tenants_regions_must_contain_every_region_the_endpoint_may_process_in() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let b = FakeUpstream::start(vec![ok("from-sg")]).await;
    let mut offshore = Endpoint::adult("example-alpha", a.url());
    offshore.regions = vec!["sg-1", "eu-1"]; // 推理可能落在 eu-1：每一个区域都必须被允许
    let h = build(
        vec![offshore, Endpoint::adult("example-beta", b.url())],
        Options {
            callers: vec![Caller {
                allowed_regions: Some(vec!["sg-1"]),
                ..Caller::adult_default()
            }],
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.json()["id"], "from-sg");
    assert_eq!(a.connection_count(), 0);
}

#[tokio::test]
async fn a_credential_without_a_cloud_payment_context_is_refused_before_any_upstream() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options {
            callers: vec![],
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 403);
    assert_eq!(reply.json()["error"]["type"], "caller_not_configured");
    assert_eq!(a.connection_count(), 0);
    assert_eq!(request_count(&h), 0);
}

// ---------------------------------------------------------------------------
// subscription、核验过期、流式 usage、能力
// ---------------------------------------------------------------------------

#[tokio::test]
async fn subscription_is_never_selectable_in_the_cloud_form() {
    let cli = FakeUpstream::start(vec![ok("must-never-be-called")]).await;
    let a = FakeUpstream::start(vec![ok("from-alpha")]).await;
    let admission = RecordingAdmission::new();
    let h = build(
        vec![
            // 订阅路径排第一、而且配置了地址：只要有一点点可能被选中，它就会先收到请求
            Endpoint::subscription("example-cli", Some(cli.url())),
            Endpoint::adult("example-alpha", a.url()),
        ],
        Options {
            admission: Some(admission.clone()),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    assert_eq!(reply.json()["id"], "from-alpha");
    assert_eq!(cli.connection_count(), 0);
    assert_eq!(admission.providers(), vec!["example-alpha"]);
    assert_eq!(rows(&h, &reply.request_id())[0].provider, "example-alpha");

    // 只有订阅路径：没有合规候选
    let cli = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::subscription("example-cli", Some(cli.url()))],
        Options::default(),
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["type"], "no_compliant_endpoint");
    assert_eq!(cli.connection_count(), 0);
}

#[tokio::test]
async fn an_endpoint_whose_verification_expired_is_not_selected() {
    // 核验 2026-11-12，最大年龄 90 天：到 verified_at + 90 天（含）就过期
    let expiry = VERIFIED_MS + 90 * DAY_MS;
    for (now, served) in [
        (expiry - 1, true),
        (expiry, false),
        (expiry + DAY_MS, false),
        (VERIFIED_MS - 1, false), // 核验时间在未来也不认
    ] {
        let a = FakeUpstream::start(vec![ok("from-alpha")]).await;
        let clock = Arc::new(ManualClock::new(
            idoris_compute_contracts::ids::TimestampMs(now),
        ));
        let h = build(
            vec![Endpoint::adult("example-alpha", a.url())],
            Options {
                clock: Some(clock),
                ..Options::default()
            },
        );
        let reply = post(&h.app, &simple_body(100)).await;
        if served {
            assert_eq!(reply.status, 200, "now = expiry - 1ms");
        } else {
            assert_eq!(reply.status, 503, "now = {now}");
            assert_eq!(reply.json()["error"]["type"], "no_compliant_endpoint");
            assert_eq!(
                a.connection_count(),
                0,
                "an expired endpoint receives nothing"
            );
        }
    }
}

#[tokio::test]
async fn verification_is_re_checked_between_attempts_so_a_switch_never_lands_on_an_expired_endpoint()
 {
    // beta 核验得早：到 2026-11-13T00:00:00Z 过期。准入第一个候选时把时钟拨过去，
    // 第一个候选失败后重新选择候选，beta 已经过期，不能被切换到
    let a = FakeUpstream::start(vec![Behavior::json(503, json!({"error": {}}))]).await;
    let b = FakeUpstream::start(vec![ok("never")]).await;
    let clock = Arc::new(ManualClock::new(
        idoris_compute_contracts::ids::TimestampMs(NOW_MS),
    ));
    let admission = RecordingAdmission::new();
    *admission.on_admit.lock().unwrap() = Some(Box::new({
        let clock = clock.clone();
        move |provider| {
            if provider == "example-alpha" {
                clock.set(idoris_compute_contracts::ids::TimestampMs(
                    VERIFIED_MS + DAY_MS,
                ));
            }
        }
    }));
    let h = build(
        vec![
            Endpoint::adult("example-alpha", a.url()),
            Endpoint::adult("example-beta", b.url()).verified("2026-08-15"),
        ],
        Options {
            clock: Some(clock),
            admission: Some(admission.clone()),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502);
    assert_eq!(b.connection_count(), 0);
    assert_eq!(
        admission.providers(),
        vec!["example-alpha"],
        "the expired candidate was never even admitted"
    );
    assert_eq!(rows(&h, &reply.request_id()).len(), 1);
}

#[tokio::test]
async fn a_paid_or_subsidized_stream_is_only_routed_to_an_endpoint_with_stream_usage() {
    for billing in ["paid", "subsidized"] {
        let n = FakeUpstream::start(vec![ok("never-streamed-here")]).await;
        let a = FakeUpstream::start(vec![Behavior::sse(vec![
            SseStep::Send(content_event("s", "x")),
            SseStep::Send(usage_event("s", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ])])
        .await;
        let admission = RecordingAdmission::new();
        let h = build(
            vec![
                Endpoint::adult("example-nostream", n.url()).no_stream_usage(),
                Endpoint::adult("example-alpha", a.url()),
            ],
            Options {
                callers: vec![Caller {
                    billing,
                    ..Caller::adult_default()
                }],
                admission: Some(admission.clone()),
                ..Options::default()
            },
        );
        let mut body = simple_body(100);
        body["stream"] = json!(true);
        let reply = post(&h.app, &body).await;
        assert_eq!(reply.status, 200, "{billing}");
        assert_eq!(
            n.connection_count(),
            0,
            "{billing}: the endpoint without stream usage is never called"
        );
        assert_eq!(admission.providers(), vec!["example-alpha"], "{billing}");
        assert_eq!(rows(&h, &reply.request_id())[0].provider, "example-alpha");
    }

    // 非流式不需要流式 usage：第一个候选就是它
    let n = FakeUpstream::start(vec![ok("from-nostream")]).await;
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![
            Endpoint::adult("example-nostream", n.url()).no_stream_usage(),
            Endpoint::adult("example-alpha", a.url()),
        ],
        Options::default(),
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.json()["id"], "from-nostream");

    // 只有一个不支持流式 usage 的端点：付费流式请求没有合规候选
    let n = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::adult("example-nostream", n.url()).no_stream_usage()],
        Options::default(),
    );
    let mut body = simple_body(100);
    body["stream"] = json!(true);
    let reply = post(&h.app, &body).await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["type"], "no_compliant_endpoint");
    assert_eq!(n.connection_count(), 0);

    // 不计费的流式请求不受这条约束（它不需要拿 usage 来结算）
    let n = FakeUpstream::start(vec![Behavior::sse(vec![
        SseStep::Send(content_event("s", "x")),
        SseStep::Send(done_event()),
        SseStep::Finish,
    ])])
    .await;
    let h = build(
        vec![Endpoint::adult("example-nostream", n.url()).no_stream_usage()],
        Options {
            callers: vec![Caller {
                billing: "unbilled",
                ..Caller::adult_default()
            }],
            ..Options::default()
        },
    );
    let reply = post(&h.app, &body).await;
    assert_eq!(reply.status, 200);
}

#[tokio::test]
async fn tools_and_structured_output_need_an_endpoint_that_supports_them() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let b = FakeUpstream::start(vec![ok("from-capable"), ok("from-capable")]).await;
    let mut weak = Endpoint::adult("example-alpha", a.url());
    weak.tool_calling = false;
    weak.structured_output = false;
    let h = build(
        vec![weak, Endpoint::adult("example-beta", b.url())],
        Options::default(),
    );
    let mut body = simple_body(100);
    body["tools"] = json!([{"type": "function", "function": {"name": "f"}}]);
    let reply = post(&h.app, &body).await;
    assert_eq!(reply.json()["id"], "from-capable");
    assert_eq!(a.connection_count(), 0);
    let mut body = simple_body(100);
    body["response_format"] = json!({"type": "json_object"});
    let reply = post(&h.app, &body).await;
    assert_eq!(reply.json()["id"], "from-capable");
    // 不需要这些能力的请求，第一个候选照常服务
    let a2 = FakeUpstream::start(vec![ok("from-weak")]).await;
    let mut weak = Endpoint::adult("example-alpha", a2.url());
    weak.tool_calling = false;
    let h = build(vec![weak], Options::default());
    assert_eq!(
        post(&h.app, &simple_body(100)).await.json()["id"],
        "from-weak"
    );
}

// ---------------------------------------------------------------------------
// 本地失败：没有字节离开本机
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_missing_upstream_credential_is_a_local_failure_nothing_was_sent_and_nothing_is_held() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let b = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![
            Endpoint::adult("example-alpha", a.url()),
            Endpoint::adult("example-beta", b.url()),
        ],
        Options::default(),
    );
    std::fs::remove_file(h.dir.path().join("upstream.key")).unwrap();
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502);
    assert_eq!(
        reply.json()["error"]["reason_code"],
        "credential_unavailable"
    );
    assert_eq!((a.connection_count(), b.connection_count()), (0, 0));
    let id = reply.request_id();
    // prepare 失败发生在写入 WireWriteStarted 之前：安全，状态是 not_sent，没有成本挂账
    let states: Vec<AttemptStateKind> = rows(&h, &id).iter().map(|r| r.state_kind()).collect();
    assert_eq!(
        states,
        vec![AttemptStateKind::NotSent, AttemptStateKind::NotSent]
    );
    assert_eq!(
        postings(&h, &id),
        vec![p(0, "release", 4110, "THB"), p(1, "release", 4110, "THB")]
    );
}

// ---------------------------------------------------------------------------
// 认证与调用方解析（沿用既有的虚拟 key 中间件）
// ---------------------------------------------------------------------------

fn key_scope(tenant: Option<&str>) -> VirtualKeyScope {
    VirtualKeyScope {
        owner: "test".into(),
        tenant_id: tenant.map(str::to_owned),
        allowed_privacy: vec![PrivacyClass::LocalOnly, PrivacyClass::Any],
        allowed_roles: [
            "fast", "daily", "deep", "vision", "embed", "rerank", "decide", "auto",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect(),
        budget_ref: None,
        expires_at_ms: None,
        admin_scopes: Vec::new(),
    }
}

#[tokio::test]
async fn the_virtual_key_decides_who_pays_and_unknown_tenants_get_nothing() {
    let a = FakeUpstream::start(vec![ok("one"), ok("two")]).await;
    let dir = tempfile::tempdir().unwrap();
    let store = VirtualKeyStore::open(dir.path().join("keys.sqlite3")).unwrap();
    let agent24 = MintedVirtualKey::mint();
    store
        .insert_active(&agent24.key_id, agent24.hash, &key_scope(Some("agent24")))
        .unwrap();
    let stranger = MintedVirtualKey::mint();
    store
        .insert_active(
            &stranger.key_id,
            stranger.hash,
            &key_scope(Some("stranger")),
        )
        .unwrap();
    let store = Arc::new(Mutex::new(store));
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options {
            callers: vec![
                Caller::adult_default(),
                Caller {
                    tenant_id: Some("agent24"),
                    payer: "acct-agent24",
                    unit: "USD",
                    ..Caller::adult_default()
                },
            ],
            state: Some(Box::new(move |state| idoris_router::AppState {
                deploy_mode: DeployMode::Tenant,
                virtual_key_authenticator: Some(VirtualKeyAuthenticator::new(store)),
                ..state
            })),
            ..Options::default()
        },
    );
    let with = |secret: &str, tenant: &str| -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {secret}"))
            .header("x-idoris-tenant", tenant)
            .body(Body::from(simple_body(100).to_string()))
            .unwrap()
    };
    // 没有 key：沿用既有的 401，不碰任何上游
    let anonymous =
        harness::collect(harness::send(&h.app, harness::chat_request(&simple_body(100))).await)
            .await;
    assert_eq!(
        anonymous.status, 400,
        "tenant mode asks for the tenant header first"
    );
    let no_key = harness::collect(
        harness::send(
            &h.app,
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("x-idoris-tenant", "agent24")
                .body(Body::from(simple_body(100).to_string()))
                .unwrap(),
        )
        .await,
    )
    .await;
    assert_eq!(no_key.status, 401);
    assert_eq!(a.connection_count(), 0);

    // agent24 的 key：付款账户是 acct-agent24，单位 USD（不是默认调用方的 THB）
    let reply = harness::collect(
        harness::send(&h.app, with(agent24.secret.expose_secret(), "agent24")).await,
    )
    .await;
    assert_eq!(reply.status, 200);
    let request = h.store.request_row(&reply.request_id()).unwrap().unwrap();
    assert_eq!(request.payer_account, "acct-agent24");
    assert_eq!(request.price_unit, "USD");
    assert_eq!(request.credential, "cred-acct-agent24");
    // USD 的价：0.3 / 0.5 每百万 token；12 入 30 出 -> ceil(3.6 + 15) = 19 微美元
    assert_eq!(request.charged_micros, Some(19));

    // 虚拟 key 上有 tenant_id、但调用方簿里没有这个租户：不回落到默认调用方
    let reply = harness::collect(
        harness::send(&h.app, with(stranger.secret.expose_secret(), "stranger")).await,
    )
    .await;
    assert_eq!(reply.status, 403);
    assert_eq!(reply.json()["error"]["type"], "caller_not_configured");
    assert_eq!(
        a.request_count(),
        1,
        "only the agent24 request reached the upstream"
    );
}

// ---------------------------------------------------------------------------
// 请求校验
// ---------------------------------------------------------------------------

#[tokio::test]
async fn malformed_requests_are_refused_before_any_upstream_or_ledger_work() {
    let a = FakeUpstream::start(vec![ok("never")]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let cases: Vec<(Value, u16, &str)> = vec![
        (
            json!({"messages": [{"role": "user", "content": "x"}]}),
            400,
            "unsupported_field",
        ),
        (
            json!({"model": "idoris/unknown", "messages": [{"role": "user", "content": "x"}]}),
            400,
            "unsupported_field",
        ),
        (json!({"model": "idoris/agent"}), 400, "invalid_request"),
        (
            json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "x"}], "n": 2}),
            400,
            "unsupported_completion_count",
        ),
        (
            json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "x"}], "max_tokens": 0}),
            400,
            "invalid_completion_cap",
        ),
        (
            json!({"model": "idoris/agent", "messages": [{"role": "user", "content": "x"}], "stream": "yes"}),
            400,
            "invalid_request",
        ),
    ];
    for (body, status, error_type) in cases {
        let reply = post(&h.app, &body).await;
        assert_eq!(reply.status, status, "{body}");
        assert_eq!(reply.json()["error"]["type"], error_type, "{body}");
    }
    // 不是 JSON、不是对象
    for raw in ["not json", "[]"] {
        let reply = harness::collect(
            harness::send(
                &h.app,
                Request::builder()
                    .method("POST")
                    .uri("/v1/chat/completions")
                    .body(Body::from(raw))
                    .unwrap(),
            )
            .await,
        )
        .await;
        assert_eq!(reply.status, 400, "{raw}");
    }
    assert_eq!(a.connection_count(), 0);
    assert_eq!(
        request_count(&h),
        0,
        "validation failures leave no ledger rows"
    );
}

// ---------------------------------------------------------------------------
// 默认关闭
// ---------------------------------------------------------------------------

#[tokio::test]
async fn with_the_switch_off_the_existing_local_pipeline_is_what_serves_the_route() {
    assert!(
        idoris_router::AppState::default().cloud.is_none(),
        "default off"
    );
    let body = r#"{"model":"idoris/daily","messages":[{"role":"user","content":"hi"}]}"#;
    let request = || {
        Request::builder()
            .method("POST")
            .uri("/v1/chat/completions")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    // 开关关闭：既有的本地流水线（没有任何组件时是 local_only_unavailable）
    let off = idoris_router::build_app(idoris_router::AppState::default());
    let reply = harness::collect(harness::send(&off, request()).await).await;
    assert_eq!(reply.status, 503);
    assert_eq!(reply.json()["error"]["type"], "local_only_unavailable");

    // 开关打开：同一个请求由云处理函数回答（这个模型不在云路由里）
    let a = FakeUpstream::start(vec![]).await;
    let h = build(
        vec![Endpoint::adult("example-alpha", a.url())],
        Options::default(),
    );
    let reply = harness::collect(harness::send(&h.app, request()).await).await;
    assert_eq!(reply.status, 400);
    assert_eq!(reply.json()["error"]["reason_code"], "unsupported_model");
    // 其它路由不受影响
    let health = harness::collect(
        harness::send(
            &h.app,
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await,
    )
    .await;
    assert_eq!(health.status, 200);
}

#[test]
fn the_switch_needs_the_cloud_form_and_is_off_when_unset() {
    use idoris_router::cloud::CloudUpstream;
    // 未设：关闭，什么都不变
    assert!(
        CloudUpstream::from_env_path(None, DeployMode::Tenant, || unreachable!())
            .unwrap()
            .is_none()
    );
    assert!(
        CloudUpstream::from_env_path(None, DeployMode::Personal, || unreachable!())
            .unwrap()
            .is_none()
    );
    // 设了但是个人形态：启动失败，个人 iDoris 永远不会把请求送到云上游
    let err = CloudUpstream::from_env_path(
        Some(std::path::PathBuf::from("/nonexistent/cloud.yaml")),
        DeployMode::Personal,
        || unreachable!("the form check comes before any file or database access"),
    )
    .unwrap_err();
    assert!(err.contains("个人形态不会把请求送到云上游"), "{err}");
}

// ---------------------------------------------------------------------------
// 密钥与正文
// ---------------------------------------------------------------------------

#[tokio::test]
async fn neither_the_prompt_nor_the_completion_nor_the_key_ever_reach_the_database_or_an_error() {
    const PROMPT: &str = "PROMPT-SENTINEL-7d1e5c";
    const REPLY: &str = "COMPLETION-SENTINEL-42ab91";
    const TOOL_ARGS: &str = "TOOLARGS-SENTINEL-9c0d";
    let a = FakeUpstream::start(vec![
        // 成功（非流式）
        Behavior::json(200, completion("db-1", REPLY, Some(usage(12, 30)))),
        // 5xx，错误正文里回显密钥和提示词（恶意或有 bug 的上游）
        Behavior::json(
            500,
            json!({"error": {"message": format!("echo {KEY} {PROMPT}")}}),
        ),
        // 流：成功
        Behavior::sse(vec![
            SseStep::Send(content_event("db-3", REPLY)),
            SseStep::Send(usage_event("db-3", usage(12, 2))),
            SseStep::Send(done_event()),
            SseStep::Finish,
        ]),
        // 流：中途被截断，内容里有工具参数
        Behavior::sse(vec![
            SseStep::Send(content_event("db-4", TOOL_ARGS)),
            SseStep::Close,
        ]),
        // 401，错误正文回显密钥
        Behavior::json(401, json!({"error": {"message": format!("bad key {KEY}")}})),
    ])
    .await;
    let b = FakeUpstream::start(vec![
        Behavior::json(200, completion("db-2", REPLY, Some(usage(12, 30)))),
        Behavior::json(401, json!({"error": {"message": format!("bad key {KEY}")}})),
    ])
    .await;
    let h = build(
        vec![
            Endpoint::adult("example-alpha", a.url()),
            Endpoint::adult("example-beta", b.url()),
        ],
        Options::default(),
    );
    let mut visible = Vec::new();
    for stream in [false, false, true, true, false] {
        let mut body = json!({
            "model": "idoris/agent",
            "messages": [{"role": "user", "content": PROMPT}],
            "max_tokens": 100,
            "stream": stream,
        });
        body["tools"] =
            json!([{"type": "function", "function": {"name": "f", "description": TOOL_ARGS}}]);
        let reply = post(&h.app, &body).await;
        visible.push(format!("{:?}{}", reply.headers, reply.text()));
    }

    // 客户端看到的错误里没有密钥、没有上游的错误正文
    for text in &visible {
        assert!(!text.contains(KEY), "{text}");
        assert!(!text.contains("echo "), "{text}");
        assert!(!text.contains("bad key"), "{text}");
    }

    // 整个数据库文件（含 WAL）里没有正文、没有工具参数、没有密钥
    let mut scanned = 0;
    for entry in std::fs::read_dir(h.dir.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        if !name.starts_with("idoris.sqlite3") {
            continue;
        }
        let bytes = std::fs::read(&path).unwrap();
        scanned += bytes.len();
        for sentinel in [PROMPT, REPLY, TOOL_ARGS, KEY, "echo ", "bad key"] {
            assert!(
                !bytes
                    .windows(sentinel.len())
                    .any(|w| w == sentinel.as_bytes()),
                "{sentinel} found in {name}"
            );
        }
    }
    assert!(scanned > 0, "the database was actually scanned");
    // 失败标注只有稳定的代码
    let conn = rusqlite::Connection::open(&h.db_path).unwrap();
    let kinds: Vec<String> = conn
        .prepare(
            "SELECT DISTINCT failure_kind FROM compute_attempts WHERE failure_kind IS NOT NULL",
        )
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert!(!kinds.is_empty());
    for kind in kinds {
        assert!(
            kind.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
            "failure kinds are fixed codes, got {kind:?}"
        );
    }
}
