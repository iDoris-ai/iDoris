//! 计算服务 S6 × S3：**安全检查的成本走真实的准入账本**。设计稿 §7：「安全检查成本计入准入和结算，它的数据处理方同样要符合
//! 端点许可表」。
//!
//! 装配和生产一样：`LedgerAdmission`、尝试记录的账本钩子、`LedgerExtraCost`，账户存储与尝试记录库共用同一个 SQLite 文件；
//! 安全服务和主上游都是 `127.0.0.1` 上的脚本化假服务，**不连接任何真实的推理或安全服务**。
//!
//! 数字约定（见 `support/cloud_harness.rs`）：对 THB 用户的价是每百万 token 输入 10 / 输出 40。假检查服务每次回
//! 40 入 6 出，实际价 `40 x 10 + 6 x 40 = 640` 微泰铢；假主上游回 12 入 30 出，实际价 `12 x 10 + 30 x 40 = 1320`。
//!
//! 断言三件事：
//!
//! 1. 客户端看到什么（稳定的码、浏览器可读的头）；2. 假上游收到什么（被拒的请求一个字节都没出网）；
//! 3. 账本里落了什么：每个阶段预留一次、结算或释放，**没有留着不关的预留**，两张账（账本和尝试记录）对得上。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_safety.rs"]
mod child_safety;
#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::collections::BTreeSet;
use std::sync::Arc;

use child_safety::{
    ANSWER_SENTINEL, CHILD_SENTINEL, Child, answer, benign_question, child_body, child_with,
    contains, harmful_question, judge_allow, judge_block, judge_says,
};
use fake_upstream::Behavior;
use harness::{
    Caller, Endpoint, LedgerFunds, NOW_MS, Options, config_yaml, permit_table_yaml, post,
    seed_ledger,
};
use idoris_compute_contracts::credential::PaymentContext;
use idoris_compute_contracts::ids::{
    AccountId, AttemptId, AttemptIndex, EndpointKey, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::pricing::{Cost, Price, Usage};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_compute_contracts::safety::SafetyStage;
use idoris_router::cloud::config::{self};
use idoris_router::cloud::safety::{
    ExtraCostEstimate, ExtraCostOutcome, ExtraCostRefusal, ExtraCostRequest, LedgerExtraCost,
    SpendItem,
};
use idoris_router::cloud::{ExtraCostMeter, ManualClock};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AccountLimits, HoldRow, HoldState};
use idoris_tenancy::attempts::RequestOutcome;
use serde_json::json;

const FUNDS: i64 = 100_000_000;
const PAYER: &str = "acct-default";
/// 假检查服务每次返回的用量（40 入 6 出）对应的实际价。
const CHECK_PRICE: i64 = 640;
/// 假主上游的回答（12 入 30 出）对应的实际价。
const MODEL_PRICE: i64 = 1320;
const INPUT: &str = "safety_input";
const OUTPUT: &str = "safety_output";

async fn ledgered(main: Vec<Behavior>, judge: Vec<Behavior>, funds: i64) -> Child {
    ledgered_with(main, judge, 2_000, funds, |_| {}).await
}

async fn ledgered_with(
    main: Vec<Behavior>,
    judge: Vec<Behavior>,
    timeout_ms: u64,
    funds: i64,
    tweak: impl FnOnce(&mut Options),
) -> Child {
    child_with(main, judge, timeout_ms, |options| {
        options.ledger = Some(LedgerFunds { micros: funds });
        tweak(options);
    })
    .await
}

fn accounts(c: &Child) -> &AccountStore {
    c.accounts.as_ref().expect("the ledger fixture is on")
}

fn attempt0(id: &RequestId) -> AttemptId {
    AttemptId::new(id.clone(), AttemptIndex(0))
}

fn hold(c: &Child, id: &RequestId, component: &str) -> Option<HoldRow> {
    accounts(c).hold(&attempt0(id), component).unwrap()
}

fn state_of(c: &Child, id: &RequestId, component: &str) -> Option<HoldState> {
    hold(c, id, component).map(|row| row.state)
}

/// 付款账户一共花掉的钱（微泰铢）：账本分录 `spent` 列之和。
fn spent_total(c: &Child) -> i64 {
    accounts(c)
        .entries_of_account(PAYER)
        .unwrap()
        .iter()
        .map(|entry| entry.spent)
        .sum()
}

/// 没有留着不关的预留；账本和尝试记录对得上。
fn assert_ledger_is_clean(c: &Child) {
    let report = accounts(c).reconcile_with_attempts().unwrap();
    assert!(report.is_clean(), "{:?}", report.mismatches);
}

fn assert_no_content(reply: &harness::Reply) {
    assert!(!contains(&reply.body, ANSWER_SENTINEL), "the answer leaked");
    assert!(
        !contains(&reply.body, CHILD_SENTINEL),
        "the child's text leaked"
    );
    assert!(reply.json().get("choices").is_none(), "{}", reply.text());
}

// ---------------------------------------------------------------------------
// 正常路径：预留、结算、对账
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_checks_are_reserved_in_the_ledger_and_settled_with_the_real_usage() {
    let c = ledgered(vec![answer("สี่")], vec![judge_allow(), judge_allow()], FUNDS).await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let id = reply.request_id();

    for component in [INPUT, OUTPUT] {
        let row = hold(&c, &id, component).unwrap_or_else(|| panic!("{component} was reserved"));
        assert_eq!(row.state, HoldState::Settled, "{component}");
        // 结算的是实际价，不是预留额；预留额是上界
        assert_eq!(
            row.charged.as_ref().map(|m| m.micros),
            Some(CHECK_PRICE),
            "{component}"
        );
        assert!(
            row.reserved.micros > CHECK_PRICE,
            "{component}: the reservation is an upper bound, got {:?}",
            row.reserved
        );
        assert_eq!(row.reserved.unit, Unit::Thb);
    }
    // 回答本身的预留照旧（S3 的准入）：用户为回答付 1320
    let model = hold(&c, &id, "model").expect("the answer's own reservation");
    assert_eq!(model.state, HoldState::Settled);
    assert_eq!(model.charged.as_ref().map(|m| m.micros), Some(MODEL_PRICE));
    // 付款账户一共花了：回答 + 两次检查
    assert_eq!(spent_total(&c), MODEL_PRICE + 2 * CHECK_PRICE);
    assert_ledger_is_clean(&c);
    // 检查服务收到了两次请求（输入、输出）
    assert_eq!(c.judge.request_count(), 2);
}

#[tokio::test]
async fn a_blocked_request_is_charged_for_the_check_that_ran_and_the_upstream_gets_nothing() {
    // 检查（LLM）给出了结论：拦截。检查是一项用了的服务，按实际用量收；上游一个字节都没收到
    let c = ledgered(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_block("self_harm")],
        FUNDS,
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 422, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "safety_input_blocked");
    assert_no_content(&reply);
    assert_eq!(c.main.connection_count(), 0);
    let id = reply.request_id();
    let row = hold(&c, &id, INPUT).expect("the input check was reserved");
    assert_eq!(row.state, HoldState::Settled);
    assert_eq!(row.charged.as_ref().map(|m| m.micros), Some(CHECK_PRICE));
    // 没有输出检查，也没有回答的预留（没有尝试）
    assert!(hold(&c, &id, OUTPUT).is_none());
    assert!(hold(&c, &id, "model").is_none());
    assert_eq!(spent_total(&c), CHECK_PRICE);
    assert_ledger_is_clean(&c);
}

#[tokio::test]
async fn a_rule_block_that_never_called_the_llm_costs_nothing() {
    // 规则先行：规则就拦下了，LLM 没有被调用——预留释放，不收钱
    let c = ledgered(vec![answer(ANSWER_SENTINEL)], vec![judge_allow()], FUNDS).await;
    let reply = post(&c.h.app, &child_body(&harmful_question())).await;
    assert_eq!(reply.status, 422);
    assert_eq!(reply.json()["error"]["type"], "safety_input_blocked");
    assert_eq!(
        c.judge.connection_count(),
        0,
        "rules first: the LLM is never called"
    );
    assert_eq!(c.main.connection_count(), 0);
    let id = reply.request_id();
    assert_eq!(state_of(&c, &id, INPUT), Some(HoldState::Released));
    assert_eq!(spent_total(&c), 0);
    assert_ledger_is_clean(&c);
}

// ---------------------------------------------------------------------------
// 故障：不收钱，不留着不关的预留
// ---------------------------------------------------------------------------

fn failure_modes() -> Vec<(&'static str, Behavior)> {
    vec![
        ("hangs", Behavior::Hang),
        (
            "HTTP 500",
            Behavior::json(500, json!({"error": {"message": "boom"}})),
        ),
        (
            "prose instead of the verdict",
            judge_says("Sure, that looks fine!"),
        ),
    ]
}

#[tokio::test]
async fn a_check_that_fails_costs_the_user_nothing_and_leaves_no_open_reservation() {
    for (name, failure) in failure_modes() {
        // 输入阶段故障：不调用上游；预留释放，不收钱
        let c = ledgered_with(
            vec![answer(ANSWER_SENTINEL)],
            vec![failure.clone()],
            400,
            FUNDS,
            |_| {},
        )
        .await;
        let reply = post(&c.h.app, &child_body(&benign_question())).await;
        assert_eq!(reply.status, 503, "{name}: {}", reply.text());
        assert_eq!(
            reply.json()["error"]["type"],
            "safety_unavailable",
            "{name}"
        );
        assert_eq!(reply.json()["error"]["stage"], "input", "{name}");
        assert_no_content(&reply);
        assert_eq!(c.main.connection_count(), 0, "{name}");
        let id = reply.request_id();
        assert_eq!(
            state_of(&c, &id, INPUT),
            Some(HoldState::Released),
            "{name}"
        );
        assert_eq!(
            spent_total(&c),
            0,
            "{name}: a failing safety service never bills the child"
        );
        assert_ledger_is_clean(&c);

        // 输出阶段故障：输入检查给出了结论（收 640），上游答过了但回答没有发出去（用户付 0），
        // 输出检查的预留释放
        let c = ledgered_with(
            vec![answer(ANSWER_SENTINEL)],
            vec![judge_allow(), failure],
            400,
            FUNDS,
            |_| {},
        )
        .await;
        let reply = post(&c.h.app, &child_body(&benign_question())).await;
        assert_eq!(reply.status, 503, "{name}: {}", reply.text());
        assert_eq!(reply.json()["error"]["stage"], "output", "{name}");
        assert_no_content(&reply);
        let id = reply.request_id();
        assert_eq!(state_of(&c, &id, INPUT), Some(HoldState::Settled), "{name}");
        assert_eq!(
            state_of(&c, &id, OUTPUT),
            Some(HoldState::Released),
            "{name}"
        );
        assert_eq!(
            state_of(&c, &id, "model"),
            Some(HoldState::Released),
            "{name}: the withheld answer is not charged"
        );
        assert_eq!(spent_total(&c), CHECK_PRICE, "{name}");
        let row = c.h.store.request_row(&id).unwrap().unwrap();
        assert_eq!(row.outcome, Some(RequestOutcome::Unanswered), "{name}");
        assert_ledger_is_clean(&c);
    }
}

// ---------------------------------------------------------------------------
// 账本拒绝：失败即关闭
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_check_that_cannot_be_afforded_is_refused_with_the_admission_code_and_nothing_is_sent() {
    // 账户没有充值：可用额度是 0，连检查的预留都放不下
    let c = ledgered(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow(), judge_allow()],
        0,
    )
    .await;
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 402, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "budget_exhausted");
    assert_eq!(reply.header("x-idoris-reason"), Some("budget_exhausted"));
    assert_eq!(reply.header("x-idoris-tier"), Some("exhausted"));
    assert_no_content(&reply);
    // 不检查、不出网：检查服务和主上游都没有收到任何东西
    assert_eq!(c.judge.connection_count(), 0);
    assert_eq!(c.main.connection_count(), 0);
    // 账本里什么都没占
    let id = reply.request_id();
    for component in [INPUT, OUTPUT, "model"] {
        assert!(hold(&c, &id, component).is_none(), "{component}");
    }
    assert_eq!(spent_total(&c), 0);
}

#[tokio::test]
async fn a_refusal_of_the_output_check_withholds_the_answer_and_the_user_pays_nothing_for_it() {
    // 第一遍：额度充足，量出输出检查要预留多少、回答的预留是多少
    let probe = ledgered(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow(), judge_allow()],
        FUNDS,
    )
    .await;
    let reply = post(&probe.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    let id = reply.request_id();
    let output_reserved = hold(&probe, &id, OUTPUT).unwrap().reserved.micros;
    let model_reserved = hold(&probe, &id, "model").unwrap().reserved.micros;

    // 第二遍：在途总额上限正好放不下「回答的预留还开着时再占输出检查的额度」——输入检查（先占先结算）和回答都放得下
    let cap = model_reserved + output_reserved - 1;
    let c = ledgered(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow(), judge_allow()],
        FUNDS,
    )
    .await;
    accounts(&c)
        .set_account_limits(
            &AccountId::new(PAYER).unwrap(),
            &AccountLimits {
                inflight_cap: Some(Money::new(Unit::Thb, cap)),
                ..AccountLimits::default()
            },
            TimestampMs(NOW_MS),
        )
        .unwrap();
    let reply = post(&c.h.app, &child_body(&benign_question())).await;
    assert_eq!(reply.status, 429, "{}", reply.text());
    assert_eq!(reply.json()["error"]["type"], "in_flight_limited");
    assert_eq!(reply.header("x-idoris-reason"), Some("in_flight_limited"));
    // 浏览器读得到这个码：和 S3 的体验分级头同一套 Access-Control-Expose-Headers
    assert!(
        reply
            .header("access-control-expose-headers")
            .is_some_and(|names| names.to_ascii_lowercase().contains("x-idoris-reason")),
        "{:?}",
        reply.headers
    );
    assert_no_content(&reply);
    // 上游答过了（回答存在过），但客户端一个字节都没收到；回答不收钱，输出检查没有占住额度
    assert_eq!(c.main.request_count(), 1);
    let id = reply.request_id();
    assert_eq!(state_of(&c, &id, INPUT), Some(HoldState::Settled));
    assert!(
        hold(&c, &id, OUTPUT).is_none(),
        "refused: nothing was reserved"
    );
    assert_eq!(state_of(&c, &id, "model"), Some(HoldState::Released));
    assert_eq!(spent_total(&c), CHECK_PRICE);
    let row = c.h.store.request_row(&id).unwrap().unwrap();
    assert_eq!(row.published_attempt, None);
    assert_ledger_is_clean(&c);
}

#[tokio::test]
async fn reserving_the_same_stage_twice_fails_closed_without_reusing_the_reservation() {
    let c = ledgered(
        vec![answer(ANSWER_SENTINEL)],
        vec![judge_allow(), judge_allow()],
        FUNDS,
    )
    .await;
    // 同一个请求号、同一个阶段，账本里已经有一笔预留（比如重复投递）
    let request_id = RequestId::new("req-hold-exists").unwrap();
    let payment: PaymentContext = serde_json::from_value(
        json!({"credential": format!("cred-{PAYER}"), "payer": PAYER, "unit": "THB"}),
    )
    .unwrap();
    let preexisting = Price::from_micros(Unit::Thb, 5_000).unwrap();
    accounts(&c)
        .reserve_request_component(
            &request_id,
            INPUT,
            &payment,
            &preexisting,
            TimestampMs(NOW_MS),
        )
        .unwrap();

    let call = idoris_router::cloud::CloudCall {
        request_id: request_id.clone(),
        payment,
        tenant: idoris_compute_contracts::permit::TenantDataPolicy::new(
            BTreeSet::from([idoris_compute_contracts::permit::DataClass::ChildEdu]),
            None,
            idoris_compute_contracts::permit::Retention::Zdr,
            true,
        )
        .unwrap(),
        billing: idoris_compute_contracts::permit::Billing::Paid,
        privacy: Default::default(),
        client_body: child_body(&benign_question()).as_object().unwrap().clone(),
        stream: false,
        client_wants_usage_chunk: false,
        requested_max_tokens: Some(100),
        input_tokens_estimate: 20,
        candidates: vec![
            EndpointKey::new("example-zdr", "example-org/chat-70b", "openai-compat").unwrap(),
        ],
        needs: idoris_compute_contracts::permit::CapabilityNeeds::default(),
        cancel: tokio_util::sync::CancellationToken::new(),
    };
    let result = c.h.cloud.executor().execute(call).await;
    match result {
        Err(idoris_router::cloud::CloudError::SafetyUnavailable { stage, cause }) => {
            assert_eq!(stage, SafetyStage::Input);
            assert_eq!(cause.as_str(), "internal");
        }
        other => panic!("expected a fail-closed refusal, got {other:?}"),
    }
    // 没有检查、没有出网
    assert_eq!(c.judge.connection_count(), 0);
    assert_eq!(c.main.connection_count(), 0);
    // 已有的那笔预留原样还在（既没有被复用，也没有被我们结算或释放掉）
    let row = hold(&c, &request_id, INPUT).unwrap();
    assert_eq!(row.state, HoldState::Open);
    assert_eq!(row.reserved.micros, 5_000);
}

// ---------------------------------------------------------------------------
// 计价：用检查端点的价目表折算成付款单位
// ---------------------------------------------------------------------------

/// 单独装一个账本和一个 `LedgerExtraCost`（不经过执行器），直接验证预留额和结算额的算法。
struct MeterFixture {
    _dir: tempfile::TempDir,
    accounts: Arc<AccountStore>,
    meter: LedgerExtraCost,
    payment: PaymentContext,
    judge: EndpointKey,
}

fn meter_fixture(price_for_judge: bool, funds: i64) -> Option<MeterFixture> {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("upstream.key");
    std::fs::write(&key_file, format!("{}\n", harness::KEY)).unwrap();
    let endpoints = vec![
        Endpoint::child_edu_compliant("example-zdr", "http://127.0.0.1:1".to_string()),
        Endpoint::child_edu_compliant("example-judge", "http://127.0.0.1:2".to_string()),
    ];
    std::fs::write(
        dir.path().join("permits.yaml"),
        permit_table_yaml(&endpoints),
    )
    .unwrap();
    let options = Options {
        callers: vec![Caller::child_edu()],
        safety_yaml: child_safety::judge_yaml(1_000),
        routes: Some(vec![("idoris/agent", vec!["example-zdr"])]),
        ..Options::default()
    };
    let mut yaml = config_yaml(&endpoints, key_file.to_str().unwrap(), &options);
    if !price_for_judge {
        // 去掉检查端点（第二个端点）的 THB 价目
        let line = "      - {unit: THB, input_per_mtok: \"10\", output_per_mtok: \"40\"}\n";
        let at = yaml.rfind(line).expect("the judge's THB price line");
        yaml.replace_range(at..at + line.len(), "");
        // 配置层在启动时就拒绝这种配置：检查的成本无法折算成付款单位
        let error = match config::parse(&yaml, dir.path(), TimestampMs(NOW_MS)) {
            Ok(_) => panic!("a check endpoint without a price for the payer's unit must not load"),
            Err(error) => error,
        };
        assert!(
            error.contains("example-judge") && error.contains("THB") && error.contains("价目"),
            "{error}"
        );
        return None;
    }
    let parts = config::parse(&yaml, dir.path(), TimestampMs(NOW_MS)).unwrap();
    let db = dir.path().join("idoris.sqlite3");
    let accounts = Arc::new(AccountStore::open(&db).unwrap());
    seed_ledger(
        &accounts,
        &[Caller::child_edu()],
        LedgerFunds { micros: funds },
    );
    let meter = LedgerExtraCost::new(
        accounts.clone(),
        Arc::new(parts.endpoints),
        Arc::new(ManualClock::new(TimestampMs(NOW_MS))),
    );
    let payment: PaymentContext = serde_json::from_value(
        json!({"credential": format!("cred-{PAYER}"), "payer": PAYER, "unit": "THB"}),
    )
    .unwrap();
    Some(MeterFixture {
        _dir: dir,
        accounts,
        meter,
        payment,
        judge: EndpointKey::new("example-judge", "example-org/chat-70b", "openai-compat").unwrap(),
    })
}

fn estimate(judge: &EndpointKey) -> ExtraCostEstimate {
    ExtraCostEstimate {
        endpoint: judge.clone(),
        // 上游成本是供应商币种（USD）：账本不能拿它当价用
        max_cost: Cost::from_micros(Unit::Usd, 7).unwrap(),
        max_input_tokens: 1_000,
        max_output_tokens: 100,
    }
}

#[test]
fn the_reservation_is_priced_with_the_check_endpoints_price_card_in_the_payers_unit() {
    let f = meter_fixture(true, FUNDS).unwrap();
    let request_id = RequestId::new("req-pricing").unwrap();
    let estimate = estimate(&f.judge);
    let ticket = f
        .meter
        .reserve(&ExtraCostRequest {
            request_id: &request_id,
            payment: &f.payment,
            stage: SafetyStage::Input,
            estimate: &estimate,
            now: TimestampMs(NOW_MS),
        })
        .unwrap();
    // 1000 x 10 + 100 x 40 = 14_000 微泰铢（10 / 40 泰铢每百万 token）：不是 7（USD 成本）
    let reserved = ticket.reserved_price.clone().unwrap();
    assert_eq!(
        (reserved.unit().clone(), reserved.micros()),
        (Unit::Thb, 14_000)
    );
    let row = f
        .accounts
        .hold(&attempt0(&request_id), INPUT)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HoldState::Open);
    assert_eq!(row.reserved.micros, 14_000);
    assert_eq!(row.reserved.unit, Unit::Thb);

    // 结算：实际用量 100 入 10 出 = 100 x 10 + 10 x 40 = 1400
    let spend = SpendItem {
        endpoint: f.judge.clone(),
        cost: Cost::from_micros(Unit::Usd, 1).unwrap(),
        usage: Some(Usage::new(100, 10, 0).unwrap()),
        estimated: false,
    };
    f.meter.settle(ticket, ExtraCostOutcome::Spent(vec![spend]));
    let row = f
        .accounts
        .hold(&attempt0(&request_id), INPUT)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HoldState::Settled);
    assert_eq!(row.charged.as_ref().map(|m| m.micros), Some(1_400));
}

#[test]
fn a_dropped_or_unknown_check_is_released_and_unknown_usage_is_charged_at_the_reservation() {
    let f = meter_fixture(true, FUNDS).unwrap();
    let estimate = estimate(&f.judge);
    let reserve = |name: &str, stage: SafetyStage| {
        let request_id = RequestId::new(name).unwrap();
        let ticket = f
            .meter
            .reserve(&ExtraCostRequest {
                request_id: &request_id,
                payment: &f.payment,
                stage,
                estimate: &estimate,
                now: TimestampMs(NOW_MS),
            })
            .unwrap();
        (request_id, ticket)
    };
    // Unknown（超时、客户端断开、future 被丢弃）：释放，平台承担
    let (id, ticket) = reserve("req-unknown", SafetyStage::Input);
    f.meter.settle(ticket, ExtraCostOutcome::Unknown);
    assert_eq!(
        f.accounts
            .hold(&attempt0(&id), INPUT)
            .unwrap()
            .unwrap()
            .state,
        HoldState::Released
    );
    // NotRun：释放
    let (id, ticket) = reserve("req-notrun", SafetyStage::Output);
    f.meter.settle(ticket, ExtraCostOutcome::NotRun);
    assert_eq!(
        f.accounts
            .hold(&attempt0(&id), OUTPUT)
            .unwrap()
            .unwrap()
            .state,
        HoldState::Released
    );
    // 给出了结论但用量未知：按预留额收（和 S4 对「用量未知」的规则一样）
    let (id, ticket) = reserve("req-unknown-usage", SafetyStage::Input);
    let spend = SpendItem {
        endpoint: f.judge.clone(),
        cost: Cost::from_micros(Unit::Usd, 7).unwrap(),
        usage: None,
        estimated: true,
    };
    f.meter.settle(ticket, ExtraCostOutcome::Spent(vec![spend]));
    let row = f.accounts.hold(&attempt0(&id), INPUT).unwrap().unwrap();
    assert_eq!(row.state, HoldState::Settled);
    assert_eq!(row.charged.as_ref().map(|m| m.micros), Some(14_000));
    // 实际用量超过预留（检查模型的 tokenizer 和字节估算对不上）：封顶在预留额，不触发账本的 overage
    let (id, ticket) = reserve("req-over", SafetyStage::Input);
    let spend = SpendItem {
        endpoint: f.judge.clone(),
        cost: Cost::from_micros(Unit::Usd, 7).unwrap(),
        usage: Some(Usage::new(10_000_000, 1_000_000, 0).unwrap()),
        estimated: false,
    };
    f.meter.settle(ticket, ExtraCostOutcome::Spent(vec![spend]));
    let row = f.accounts.hold(&attempt0(&id), INPUT).unwrap().unwrap();
    assert_eq!(row.charged.as_ref().map(|m| m.micros), Some(14_000));
    assert!(
        f.accounts.alerts(None).unwrap().is_empty(),
        "no overage, no alert"
    );
}

#[test]
fn the_ledger_refusing_the_reservation_is_an_admission_refusal_and_a_double_reservation_is_not_reused()
 {
    // 没有充值：账本拒绝，原因码原样交给调用方（失败即关闭：不检查、不出网）
    let f = meter_fixture(true, 0).unwrap();
    let request_id = RequestId::new("req-broke").unwrap();
    let estimate = estimate(&f.judge);
    let request = ExtraCostRequest {
        request_id: &request_id,
        payment: &f.payment,
        stage: SafetyStage::Input,
        estimate: &estimate,
        now: TimestampMs(NOW_MS),
    };
    assert!(matches!(
        f.meter.reserve(&request),
        Err(ExtraCostRefusal::Denied(ReasonCode::BudgetExhausted))
    ));
    assert!(
        f.accounts
            .hold(&attempt0(&request_id), INPUT)
            .unwrap()
            .is_none()
    );

    // 有钱：第一次预留成功，同一个 (请求, 阶段) 第二次预留失败即关闭，已有的预留不动
    let f = meter_fixture(true, FUNDS).unwrap();
    let request = ExtraCostRequest {
        request_id: &request_id,
        payment: &f.payment,
        stage: SafetyStage::Input,
        estimate: &estimate,
        now: TimestampMs(NOW_MS),
    };
    let first = f.meter.reserve(&request).unwrap();
    assert!(matches!(
        f.meter.reserve(&request),
        Err(ExtraCostRefusal::Unavailable)
    ));
    let row = f
        .accounts
        .hold(&attempt0(&request_id), INPUT)
        .unwrap()
        .unwrap();
    assert_eq!(row.state, HoldState::Open);
    assert_eq!(row.reserved.micros, 14_000);
    f.meter.settle(first, ExtraCostOutcome::NotRun);
    // 另一个阶段是另一笔预留，不受影响
    let output = ExtraCostRequest {
        stage: SafetyStage::Output,
        ..request.clone()
    };
    let ticket = f.meter.reserve(&output).unwrap();
    f.meter.settle(ticket, ExtraCostOutcome::NotRun);
}

#[test]
fn the_check_endpoint_must_be_priced_in_every_child_callers_unit() {
    // 配置里检查端点没有付款单位（THB）的价目：启动就失败，不是运行时才发现没法计价
    assert!(meter_fixture(false, FUNDS).is_none());
}
