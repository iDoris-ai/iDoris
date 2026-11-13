//! 计算服务 S3 端到端：云执行器接上真实的准入账本（`LedgerAdmission` + 尝试记录的账本钩子），
//! 体验分级的每一级都走完整条路：HTTP 请求 → 准入（账本，沿付款账户所有祖先的五项检查）→ 假上游 → 结算，
//! 断言**三件事**：
//!
//! 1. 客户端看到什么：状态码、稳定的原因码、`X-iDoris-Tier`、`X-iDoris-Max-Tokens`、浏览器可读的 `Access-Control-Expose-Headers`；
//! 2. 假上游收到什么：`max_tokens` 是不是压低后的值，被拒绝的请求有没有出网；
//! 3. 账本和尝试记录里落了什么：两张账对得上（`reconcile_with_attempts`），预留释放了，宽限记下了。
//!
//! 数字约定（见 `support/cloud_harness.rs`）：`simple_body(100)` 输入估算 11 token，对 THB 用户的价是每百万 token 输入 10 /
//! 输出 40，所以全额预留 `max_tokens = 100` 是 **4110** 微泰铢；12 入 30 出的实际价是 **1320**。最低有用的预留
//! （`max_tokens = 10`）是 510。全部对本地假上游（`127.0.0.1`）进行，不连接任何真实的推理供应商。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "../../idoris-upstream/tests/support/fake_upstream.rs"]
mod fake_upstream;
#[path = "support/cloud_harness.rs"]
mod harness;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use fake_upstream::{Behavior, FakeUpstream, completion, usage};
use harness::{
    Endpoint, Harness, NOW_MS, Options, collect, config_yaml, permit_table_yaml, post, send,
    simple_body,
};
use idoris_compute_contracts::account::{Account, AccountKind, AccountStatus};
use idoris_compute_contracts::credential::{Credential, CredentialBinding, CredentialStatus};
use idoris_compute_contracts::ids::{
    AccountId, AttemptId, AttemptIndex, CredentialId, IdempotencyKey, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::request::{Attempt, SettlementPlan};
use idoris_router::cloud::config::{self};
use idoris_router::cloud::{
    CloudUpstream, EXPOSED_HEADERS, LedgerAdmission, ManualClock, SimpleAdmission,
};
use idoris_router::{AppState, build_app};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AccountLimits, AlertKind, HoldState, LedgerConfig};
use idoris_tenancy::attempts::{AttemptLedgerHook, AttemptStore};
use serde_json::json;

/// 全额预留（`max_tokens = 100`）。
const FULL: i64 = 4110;
/// 12 入 30 出的实际价。
const ACTUAL: i64 = 1320;
const PAYER: &str = "acct-default";

fn aid(s: &str) -> AccountId {
    AccountId::new(s).unwrap()
}
fn ts() -> TimestampMs {
    TimestampMs(NOW_MS)
}
fn thb(micros: i64) -> Money {
    Money::new(Unit::Thb, micros)
}

// ---------------------------------------------------------------------------
// 装配：真实的账本、尝试记录的账本钩子、云执行器
// ---------------------------------------------------------------------------

/// 付款账户在树里的位置。
#[derive(Clone, Copy)]
enum Layout {
    /// platform -> payer
    Direct,
    /// platform -> pool -> payer（补贴池的账期上限也生效）
    UnderPool,
}

struct Ledgered {
    h: Harness,
    accounts: Arc<AccountStore>,
}

/// 一个可以让指定尝试的创建失败的账本钩子：包住真正的钩子（测「候选没被采用要释放」）。
struct FlakyHook {
    inner: Arc<dyn AttemptLedgerHook>,
    /// 创建哪些序号的尝试时失败。
    fail_attempt_creation_at: Mutex<Vec<u8>>,
    armed: AtomicBool,
}

impl AttemptLedgerHook for FlakyHook {
    fn attempt_created(
        &self,
        tx: &rusqlite::Transaction<'_>,
        attempt: &Attempt,
        at: TimestampMs,
    ) -> Result<(), String> {
        if self.armed.load(Ordering::SeqCst)
            && self
                .fail_attempt_creation_at
                .lock()
                .unwrap()
                .contains(&attempt.index().0)
        {
            return Err("injected failure while creating the attempt".to_string());
        }
        self.inner.attempt_created(tx, attempt, at)
    }

    fn request_finalized(
        &self,
        tx: &rusqlite::Transaction<'_>,
        plan: &SettlementPlan,
        at: TimestampMs,
    ) -> Result<Vec<String>, String> {
        self.inner.request_finalized(tx, plan, at)
    }
}

struct Wiring {
    layout: Layout,
    funds: i64,
    /// 对 `provider` 的 THB 价目改成 `(输入, 输出)` 每百万 token（让某个候选更便宜）。
    cheaper: Option<(&'static str, &'static str, &'static str)>,
    flaky: Option<Arc<FlakyHook>>,
    ttl_ms: Option<i64>,
}

impl Wiring {
    fn new(funds: i64) -> Self {
        Self {
            layout: Layout::Direct,
            funds,
            cheaper: None,
            flaky: None,
            ttl_ms: None,
        }
    }
}

fn build_with_ledger(endpoints: Vec<Endpoint>, options: Options, wiring: Wiring) -> Ledgered {
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("upstream.key");
    std::fs::write(&key_file, format!("{}\n", harness::KEY)).unwrap();
    std::fs::write(
        dir.path().join("permits.yaml"),
        permit_table_yaml(&endpoints),
    )
    .unwrap();
    let mut yaml = config_yaml(&endpoints, key_file.to_str().unwrap(), &options);
    if let Some((provider, input, output)) = wiring.cheaper {
        let at = yaml.find(&format!("provider: {provider}\n")).unwrap();
        let (head, tail) = yaml.split_at(at);
        let tail = tail.replacen(
            "{unit: THB, input_per_mtok: \"10\", output_per_mtok: \"40\"}",
            &format!("{{unit: THB, input_per_mtok: \"{input}\", output_per_mtok: \"{output}\"}}"),
            1,
        );
        yaml = format!("{head}{tail}");
    }
    let clock = options
        .clock
        .clone()
        .unwrap_or_else(|| Arc::new(ManualClock::new(TimestampMs(NOW_MS))));
    let parts = config::parse(&yaml, dir.path(), TimestampMs(NOW_MS))
        .unwrap_or_else(|e| panic!("fixture config must load: {e}\n{yaml}"));
    let db_path = dir.path().join("idoris.sqlite3");

    // 账户存储（含账本）与尝试记录存储共用同一个库文件，和生产装配一样
    let accounts = Arc::new(AccountStore::open(&db_path).unwrap());
    if let Some(ttl_ms) = wiring.ttl_ms {
        accounts
            .set_ledger_config(&LedgerConfig {
                billing_timezone: "UTC".into(),
                reservation_ttl_ms: ttl_ms,
            })
            .unwrap();
    }
    let store = Arc::new(AttemptStore::open(&db_path).unwrap());
    let real_hook = accounts.attempt_ledger_hook();
    match &wiring.flaky {
        Some(flaky) => store.install_ledger_hook(flaky.clone()).unwrap(),
        None => store.install_ledger_hook(real_hook).unwrap(),
    }

    // 账户树和凭证：每个调用方一个组织账户 + 一张节点凭证（`cred-<payer>`，和夹具的静态调用方簿一致）
    let account = |id: &str, kind: AccountKind, parent: Option<&str>| Account {
        id: aid(id),
        kind,
        parent: parent.map(aid),
        unit: Unit::Thb,
        status: AccountStatus::Active,
    };
    accounts
        .create_account(&account("platform", AccountKind::Platform, None), ts())
        .unwrap();
    let parent = match wiring.layout {
        Layout::Direct => "platform",
        Layout::UnderPool => {
            accounts
                .create_account(
                    &account("pool", AccountKind::SubsidyPool, Some("platform")),
                    ts(),
                )
                .unwrap();
            "pool"
        }
    };
    for caller in &options.callers {
        accounts
            .create_account(&account(caller.payer, AccountKind::Org, Some(parent)), ts())
            .unwrap();
        accounts
            .create_credential(
                &Credential::new(
                    CredentialId::new(format!("cred-{}", caller.payer)).unwrap(),
                    CredentialBinding::Node {
                        account: aid(caller.payer),
                    },
                    CredentialStatus::Active,
                    None,
                    Vec::new(),
                )
                .unwrap(),
                ts(),
            )
            .unwrap();
        if wiring.funds > 0 {
            accounts
                .fund_account(
                    &aid(caller.payer),
                    &thb(wiring.funds),
                    &IdempotencyKey::new(format!("fund-{}", caller.payer)).unwrap(),
                    ts(),
                )
                .unwrap();
        }
    }

    let cloud = Arc::new(CloudUpstream::new(
        parts,
        store.clone(),
        clock.clone(),
        Arc::new(LedgerAdmission::new(accounts.clone())),
    ));
    let state = AppState {
        cloud: Some(cloud.clone()),
        ..AppState::default()
    };
    Ledgered {
        h: Harness {
            app: build_app(state),
            cloud,
            store,
            clock,
            db_path,
            dir,
        },
        accounts,
    }
}

async fn one_upstream(script: Vec<Behavior>, wiring: Wiring) -> (Ledgered, FakeUpstream) {
    let upstream = FakeUpstream::start(script).await;
    let ledgered = build_with_ledger(
        vec![Endpoint::adult("example-alpha", upstream.url())],
        Options::default(),
        wiring,
    );
    (ledgered, upstream)
}

fn ok(id: &str) -> Behavior {
    Behavior::json(200, completion(id, "hello", Some(usage(12, 30))))
}

fn attempt(request: &RequestId, index: u8) -> AttemptId {
    AttemptId::new(request.clone(), AttemptIndex(index))
}

fn balance(l: &Ledgered) -> i64 {
    l.accounts
        .account_view(&aid(PAYER), ts())
        .unwrap()
        .balance
        .unwrap()
        .micros
}

fn assert_books_agree(l: &Ledgered) {
    let report = l.accounts.reconcile_with_attempts().unwrap();
    assert!(
        report.is_clean(),
        "ledger and attempt records disagree: {:#?}",
        report.mismatches
    );
}

fn expose_header_names(reply: &harness::Reply) -> Vec<String> {
    reply
        .header("access-control-expose-headers")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .collect()
}

/// 浏览器要读的头都在 `Access-Control-Expose-Headers` 里（设计稿 §5，评审 L2）。
fn assert_browser_can_read(reply: &harness::Reply) {
    let exposed = expose_header_names(reply);
    for name in EXPOSED_HEADERS {
        assert!(
            exposed.contains(&name.to_ascii_lowercase()),
            "{name} is not exposed: {exposed:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 体验分级：每一级都有端到端用例
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tier_1_normal_admits_the_full_reservation_and_settles_the_actual_price() {
    let (l, upstream) = one_upstream(vec![ok("n-1")], Wiring::new(100_000)).await;
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    // 第 1 级：没有原因码，级别头是 normal，没有降级头、没有压低 max_tokens
    assert_eq!(reply.header("x-idoris-reason"), None);
    assert_eq!(reply.header("x-idoris-degraded"), None);
    assert_eq!(reply.header("x-idoris-tier"), Some("normal"));
    assert_eq!(reply.header("x-idoris-max-tokens"), None);
    assert_browser_can_read(&reply);
    // 上游收到的是请求的 max_tokens（没压低）
    assert_eq!(upstream.requests()[0].json()["max_tokens"], 100);
    // 账本：预留结算成实际价，在途归零；和尝试记录的 user_price / release 对得上
    let id = reply.request_id();
    let hold = l.accounts.hold(&attempt(&id, 0), "model").unwrap().unwrap();
    assert_eq!(
        (hold.state, hold.charged, hold.reserved),
        (HoldState::Settled, Some(thb(ACTUAL)), thb(FULL))
    );
    assert_eq!(balance(&l), 100_000 - ACTUAL);
    let platform = l.accounts.account_view(&aid("platform"), ts()).unwrap();
    assert!(platform.balance.is_none());
    assert_books_agree(&l);
}

#[tokio::test]
async fn tier_2_degraded_lowers_max_tokens_and_the_upstream_sees_the_lowered_value() {
    // 余额 2_000 < 4110：全额放不下，压低到放得下的最大值：(2000 - 110) / 40 = 47 个输出 token，预留 1990
    let (l, upstream) = one_upstream(vec![ok("d-1")], Wiring::new(2_000)).await;
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("x-idoris-reason"), Some("budget_degraded"));
    assert_eq!(reply.header("x-idoris-degraded"), Some("true"));
    assert_eq!(reply.header("x-idoris-tier"), Some("degraded"));
    assert_eq!(reply.header("x-idoris-max-tokens"), Some("47"));
    assert_browser_can_read(&reply);
    // 关键：发给上游的 max_tokens 就是压低后的值，上游不能输出超过预留的内容
    assert_eq!(upstream.requests()[0].json()["max_tokens"], 47);
    let id = reply.request_id();
    let hold = l.accounts.hold(&attempt(&id, 0), "model").unwrap().unwrap();
    assert_eq!(hold.reserved, thb(1_990));
    assert_eq!(hold.charged, Some(thb(ACTUAL)));
    assert_eq!(balance(&l), 2_000 - ACTUAL);
    assert_books_agree(&l);
}

#[tokio::test]
async fn a_cheaper_candidate_is_tried_before_grace_and_before_a_402() {
    // alpha 贵（10/40），beta 便宜（1/2）。余额 300：alpha 压到最低有用的预留也要 510 放不下；
    // 换到 beta：全额预留只要 211，放得下，正常放行——不降级、不宽限、不 402
    let alpha = FakeUpstream::start(vec![ok("c-alpha")]).await;
    let beta = FakeUpstream::start(vec![ok("c-beta")]).await;
    let wiring = Wiring {
        cheaper: Some(("example-beta", "1", "2")),
        ..Wiring::new(300)
    };
    let l = build_with_ledger(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
        wiring,
    );
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(alpha.request_count(), 0, "放不下的候选没有出网");
    assert_eq!(beta.request_count(), 1);
    assert_eq!(
        reply.header("x-idoris-reason"),
        None,
        "换了更便宜的候选，全额放行，没有降级也没有宽限"
    );
    assert_eq!(reply.header("x-idoris-tier"), Some("normal"));
    assert!(l.accounts.grace_uses(&aid(PAYER)).unwrap().is_empty());
    let id = reply.request_id();
    let hold = l.accounts.hold(&attempt(&id, 0), "model").unwrap().unwrap();
    assert_eq!(hold.reserved, thb(211), "beta 的全额预留：11 × 1 + 100 × 2");
    assert_books_agree(&l);
}

#[tokio::test]
async fn tier_3_grace_is_the_last_resort_and_the_second_try_in_the_same_month_is_a_402() {
    // 余额 300 < 最低有用的预留 510：压低放不下，只有一个候选可换：宽限一轮
    let (l, upstream) = one_upstream(vec![ok("g-1")], Wiring::new(300)).await;
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    assert_eq!(reply.header("x-idoris-reason"), Some("grace_used"));
    assert_eq!(reply.header("x-idoris-tier"), Some("grace"));
    assert_eq!(reply.header("x-idoris-degraded"), None, "宽限不是降级");
    assert_browser_can_read(&reply);
    // 宽限放行的是完整的一次预留（max_tokens 没压低）
    assert_eq!(upstream.requests()[0].json()["max_tokens"], 100);
    // 账本：记了一次宽限；担保人（付款账户的父账户）有一条可查询的告警；超出下限的部分由平台承担
    let uses = l.accounts.grace_uses(&aid(PAYER)).unwrap();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].period.to_string(), "2026-11");
    let alerts = l.accounts.alerts(Some(AlertKind::GraceUsed)).unwrap();
    assert_eq!(
        (alerts.len(), alerts[0].guarantor.as_deref()),
        (1, Some("platform"))
    );
    // 实际价 1320，账户自己的 300 先付，其余 1020 平台承担（不是 overage，不冻结凭证）
    assert_eq!(
        l.accounts.loss_total("grace_absorbed").unwrap(),
        ACTUAL - 300
    );
    assert_eq!(l.accounts.loss_total("platform_loss").unwrap(), 0);
    assert_books_agree(&l);

    // 同一个月第二次：钱不够，宽限用过了 -> 402，原因码与级别都在响应头里，请求没有出网
    let second = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(second.status, 402, "{}", second.text());
    assert_eq!(second.header("x-idoris-reason"), Some("budget_exhausted"));
    assert_eq!(second.header("x-idoris-tier"), Some("exhausted"));
    assert_browser_can_read(&second);
    assert_eq!(second.json()["error"]["type"], "budget_exhausted");
    assert_eq!(upstream.request_count(), 1);
    assert_books_agree(&l);
}

#[tokio::test]
async fn tier_4_payment_required_is_a_402_with_stable_headers_and_nothing_leaves_the_host() {
    let (l, upstream) = one_upstream(vec![ok("x-1")], Wiring::new(0)).await;
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 402, "{}", reply.text());
    assert_eq!(reply.header("x-idoris-reason"), Some("budget_exhausted"));
    assert_eq!(reply.header("x-idoris-tier"), Some("exhausted"));
    assert_eq!(reply.header("x-idoris-degraded"), None);
    assert_browser_can_read(&reply);
    assert_eq!(upstream.request_count(), 0);
    // 没有占任何额度：库里没有预留；留下一行已关闭的拒绝请求（审计）
    let conn = rusqlite::Connection::open(&l.h.db_path).unwrap();
    let holds: i64 = conn
        .query_row("SELECT COUNT(*) FROM ledger_holds", [], |r| r.get(0))
        .unwrap();
    assert_eq!(holds, 0);
    let rejected: i64 = conn
        .query_row("SELECT COUNT(*) FROM compute_requests WHERE outcome = 'rejected' AND reason_code = 'budget_exhausted'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(rejected, 1);
    assert_books_agree(&l);
}

// ---------------------------------------------------------------------------
// 平台保护：速率、在途、补贴池、凭证冻结——不在四级阶梯上，没有级别头，宽限和降级都绕不过
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_rate_gate_is_a_429_that_neither_degrading_nor_grace_can_bypass() {
    let (l, upstream) = one_upstream(vec![ok("r-1"), ok("r-2")], Wiring::new(1_000_000)).await;
    l.accounts
        .set_account_limits(
            &aid(PAYER),
            &AccountLimits {
                minute_cap: Some(thb(5_000)),
                ..AccountLimits::default()
            },
            ts(),
        )
        .unwrap();
    assert_eq!(post(&l.h.app, &simple_body(100)).await.status, 200); // 实际 1320 计入窗口
    // 4110（预留）+ 1320（已用）> 5000：速率闸门不过，其余都充足
    let limited = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(limited.status, 429, "{}", limited.text());
    assert_eq!(limited.header("x-idoris-reason"), Some("rate_limited"));
    assert_eq!(limited.header("x-idoris-tier"), None, "不在四级阶梯上");
    assert_browser_can_read(&limited);
    assert_eq!(upstream.request_count(), 1);
    assert!(l.accounts.grace_uses(&aid(PAYER)).unwrap().is_empty());
    assert_books_agree(&l);
}

#[tokio::test]
async fn the_in_flight_cap_and_the_subsidy_pool_cap_are_429s_with_their_own_codes() {
    // 在途上限
    let (l, _upstream) = one_upstream(vec![ok("f-1")], Wiring::new(1_000_000)).await;
    l.accounts
        .set_account_limits(
            &aid(PAYER),
            &AccountLimits {
                inflight_cap: Some(thb(100)),
                ..AccountLimits::default()
            },
            ts(),
        )
        .unwrap();
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(
        (reply.status.as_u16(), reply.header("x-idoris-reason")),
        (429, Some("in_flight_limited"))
    );
    // 补贴池总上限：付款账户挂在补贴池下，池这个月的上限 5_000（付款账户自己的钱充足）
    let upstream = FakeUpstream::start(vec![ok("p-1")]).await;
    let l = build_with_ledger(
        vec![Endpoint::adult("example-alpha", upstream.url())],
        Options::default(),
        Wiring {
            layout: Layout::UnderPool,
            ..Wiring::new(1_000_000)
        },
    );
    l.accounts
        .set_pool_cap(
            &aid("pool"),
            idoris_compute_contracts::ids::BillingMonth::new(2026, 11).unwrap(),
            &thb(5_000),
            ts(),
        )
        .unwrap();
    let first = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(first.status, 200, "{}", first.text());
    // 池这个月已经花掉 1320，剩 3680 < 4110：池的总上限不过。它是平台的保护，不是「钱不够」：
    // 不会靠压低 max_tokens 放行（那会让池被越用越薄），也不会宽限——直接 429
    let second = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(
        (second.status.as_u16(), second.header("x-idoris-reason")),
        (429, Some("subsidy_pool_exhausted"))
    );
    assert_eq!(second.header("x-idoris-tier"), None);
    assert_eq!(upstream.request_count(), 1);
    assert_books_agree(&l);
}

#[tokio::test]
async fn a_credential_frozen_after_the_platform_took_a_loss_is_refused_with_its_own_code() {
    // 凭证冻结（overage 超出授信时系统自动冻结）：之后用它请求被拒，原因码是 credential_frozen
    let (l, upstream) = one_upstream(vec![ok("z-1")], Wiring::new(1_000_000)).await;
    l.accounts
        .set_credential_status(
            &CredentialId::new(format!("cred-{PAYER}")).unwrap(),
            CredentialStatus::Frozen,
            ts(),
        )
        .unwrap();
    let reply = post(&l.h.app, &simple_body(100)).await;
    // 凭证在账本里已经冻结：付款上下文是静态调用方簿解析的，准入事务里再核对一次，拦在这里
    assert_eq!(reply.status, 403, "{}", reply.text());
    assert_eq!(reply.header("x-idoris-reason"), Some("credential_frozen"));
    assert_eq!(upstream.request_count(), 0);
}

// ---------------------------------------------------------------------------
// 候选切换与「候选没被采用要释放」
// ---------------------------------------------------------------------------

#[tokio::test]
async fn failover_releases_the_failed_candidate_so_a_balance_for_one_request_still_works() {
    // 余额刚好够一次全额预留。第一个候选 503，切换到第二个：第二个候选的准入必须看得到第一个释放出来的额度
    let alpha = FakeUpstream::start(vec![Behavior::json(503, json!({"error": "down"}))]).await;
    let beta = FakeUpstream::start(vec![ok("fo-beta")]).await;
    let l = build_with_ledger(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
        Wiring::new(FULL),
    );
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200, "{}", reply.text());
    // 首字节前切换：upstream_failover（降级级别），没有压低 max_tokens（释放腾出了完整额度）
    assert_eq!(reply.header("x-idoris-reason"), Some("upstream_failover"));
    assert_eq!(reply.header("x-idoris-tier"), Some("degraded"));
    assert_eq!(reply.header("x-idoris-max-tokens"), None);
    assert_eq!(beta.requests()[0].json()["max_tokens"], 100);
    let id = reply.request_id();
    let first = l.accounts.hold(&attempt(&id, 0), "model").unwrap().unwrap();
    let second = l.accounts.hold(&attempt(&id, 1), "model").unwrap().unwrap();
    assert_eq!(
        (first.state, first.charged),
        (HoldState::Released, Some(thb(0)))
    );
    assert_eq!(
        (second.state, second.charged),
        (HoldState::Settled, Some(thb(ACTUAL)))
    );
    assert_eq!(balance(&l), FULL - ACTUAL);
    assert_books_agree(&l);
}

#[tokio::test]
async fn grace_only_rescues_a_request_that_would_otherwise_be_a_402_not_a_failover_that_cannot_afford_the_next_candidate()
 {
    // alpha 便宜（1/2，全额预留 211），beta 贵（10/40，最低有用的预留 510）。余额 300：alpha 放得下，被放行后 503；
    // 切换时 alpha 的预留释放，但 beta 压到最低也要 510 放不下。这个请求已经被正常放行过一次，所以不再去动宽限：
    // 按上游失败报 502（S4 的「被拒的候选不会被调用、请求以第一次失败的原因结束」），本月的宽限原封不动
    let alpha = FakeUpstream::start(vec![Behavior::json(503, json!({"error": "down"}))]).await;
    let beta = FakeUpstream::start(vec![ok("never")]).await;
    let l = build_with_ledger(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
        Wiring {
            cheaper: Some(("example-alpha", "1", "2")),
            ..Wiring::new(300)
        },
    );
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502, "{}", reply.text());
    assert_eq!(alpha.request_count(), 1);
    assert_eq!(beta.request_count(), 0, "放不下的候选没有出网");
    assert!(
        l.accounts.grace_uses(&aid(PAYER)).unwrap().is_empty(),
        "宽限没有被消耗"
    );
    assert_eq!(l.accounts.loss_total("grace_absorbed").unwrap(), 0);
    let id = reply.request_id();
    let first = l.accounts.hold(&attempt(&id, 0), "model").unwrap().unwrap();
    assert_eq!(
        (first.state, first.charged),
        (HoldState::Released, Some(thb(0)))
    );
    assert!(
        l.accounts
            .hold(&attempt(&id, 1), "model")
            .unwrap()
            .is_none(),
        "beta 没有占任何预留"
    );
    assert_eq!(balance(&l), 300, "用户付 0");
    assert_books_agree(&l);
}

#[tokio::test]
async fn a_candidate_that_is_admitted_but_never_becomes_an_attempt_is_released() {
    // 准入放行了（预留占住了），但创建尝试失败（这里注入：账本钩子拒绝）：执行器必须把预留放回去
    let upstream = FakeUpstream::start(vec![ok("rel-1")]).await;
    let flaky = Arc::new(FlakyHook {
        inner: Arc::new(idoris_tenancy::accounts::ledger::LedgerAttemptHook),
        fail_attempt_creation_at: Mutex::new(vec![0]),
        armed: AtomicBool::new(true),
    });
    let l = build_with_ledger(
        vec![Endpoint::adult("example-alpha", upstream.url())],
        Options::default(),
        Wiring {
            flaky: Some(flaky.clone()),
            ..Wiring::new(100_000)
        },
    );
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 503, "失败即关闭：{}", reply.text());
    assert_eq!(upstream.request_count(), 0, "尝试没创建就不出网");
    // 预留没有悬着：状态是 released，额度完整地回来了（不用等 TTL）
    let rows: i64 = rusqlite::Connection::open(&l.h.db_path)
        .unwrap()
        .query_row(
            "SELECT COUNT(*) FROM ledger_holds WHERE state = 'released'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(balance(&l), 100_000);
    // 对照：钩子恢复之后同样的请求正常通过
    flaky.armed.store(false, Ordering::SeqCst);
    assert_eq!(post(&l.h.app, &simple_body(100)).await.status, 200);
    assert_books_agree(&l);
}

#[tokio::test]
async fn a_switch_whose_attempt_cannot_be_created_releases_the_new_reservation_too() {
    // 第一个候选 503；第二个候选准入放行了，但创建第二次尝试失败：第二个预留也要放回去（第一个在切换时已经释放）
    let alpha = FakeUpstream::start(vec![Behavior::json(503, json!({"error": "down"}))]).await;
    let beta = FakeUpstream::start(vec![ok("sw-beta")]).await;
    let flaky = Arc::new(FlakyHook {
        inner: Arc::new(idoris_tenancy::accounts::ledger::LedgerAttemptHook),
        fail_attempt_creation_at: Mutex::new(vec![1]),
        armed: AtomicBool::new(true),
    });
    let l = build_with_ledger(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
        Wiring {
            flaky: Some(flaky),
            ..Wiring::new(100_000)
        },
    );
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 503, "{}", reply.text());
    assert_eq!(beta.request_count(), 0);
    let conn = rusqlite::Connection::open(&l.h.db_path).unwrap();
    let open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ledger_holds WHERE state = 'open'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open, 0, "没有悬着的预留");
    assert_eq!(balance(&l), 100_000);
    assert_books_agree(&l);
}

#[tokio::test]
async fn a_request_that_fails_everywhere_charges_nothing_and_releases_everything() {
    let alpha = FakeUpstream::start(vec![Behavior::json(500, json!({"error": "boom"}))]).await;
    let beta = FakeUpstream::start(vec![Behavior::json(500, json!({"error": "boom"}))]).await;
    let l = build_with_ledger(
        vec![
            Endpoint::adult("example-alpha", alpha.url()),
            Endpoint::adult("example-beta", beta.url()),
        ],
        Options::default(),
        Wiring::new(100_000),
    );
    let reply = post(&l.h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 502, "{}", reply.text());
    assert_eq!(balance(&l), 100_000, "用户付 0");
    let conn = rusqlite::Connection::open(&l.h.db_path).unwrap();
    let open: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM ledger_holds WHERE state = 'open'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(open, 0);
    assert_books_agree(&l);
}

// ---------------------------------------------------------------------------
// 并发：同时在途的请求不会把额度用超
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_requests_never_exceed_the_rate_gate_while_all_reservations_are_in_flight() {
    // 上游慢，十个请求同时到达，所有预留同时在途。速率上限 = 3 个全额预留 -> 恰好 3 个放行，7 个 429。
    // 上游报的用量（11 入 100 出）让实际价恰好等于预留（110 + 4000 = 4110）：已结算的和还在途的对速率窗口的
    // 贡献一样，所以哪怕有请求到得晚（别的请求已经结算了），结果也与调度无关。
    let delay = Duration::from_millis(500);
    let script: Vec<Behavior> = (0..10)
        .map(|i| {
            Behavior::slow_json(
                200,
                completion(&format!("cc-{i}"), "hi", Some(usage(11, 100))),
                delay,
            )
        })
        .collect();
    let (l, upstream) = one_upstream(script, Wiring::new(10_000_000)).await;
    l.accounts
        .set_account_limits(
            &aid(PAYER),
            &AccountLimits {
                minute_cap: Some(thb(3 * FULL)),
                ..AccountLimits::default()
            },
            ts(),
        )
        .unwrap();
    let app = l.h.app.clone();
    let handles: Vec<_> = (0..10)
        .map(|_| {
            let app = app.clone();
            tokio::spawn(async move {
                collect(send(&app, harness::chat_request(&simple_body(100))).await).await
            })
        })
        .collect();
    let mut replies = Vec::new();
    for h in handles {
        replies.push(h.await.unwrap());
    }
    let ok_count = replies.iter().filter(|r| r.status == 200).count();
    let limited = replies
        .iter()
        .filter(|r| r.status == 429 && r.header("x-idoris-reason") == Some("rate_limited"))
        .count();
    assert_eq!(
        (ok_count, limited),
        (3, 7),
        "{:?}",
        replies.iter().map(|r| r.status).collect::<Vec<_>>()
    );
    assert_eq!(upstream.request_count(), 3);
    assert_eq!(balance(&l), 10_000_000 - 3 * FULL);
    assert_books_agree(&l);
}

// ---------------------------------------------------------------------------
// 装配
// ---------------------------------------------------------------------------

#[tokio::test]
async fn from_file_wires_the_ledger_and_refuses_a_reservation_ttl_shorter_than_a_request() {
    let upstream = FakeUpstream::start(vec![ok("ff-1")]).await;
    let dir = tempfile::tempdir().unwrap();
    let key_file = dir.path().join("upstream.key");
    std::fs::write(&key_file, format!("{}\n", harness::KEY)).unwrap();
    let endpoints = vec![Endpoint::adult("example-alpha", upstream.url())];
    std::fs::write(
        dir.path().join("permits.yaml"),
        permit_table_yaml(&endpoints),
    )
    .unwrap();
    let yaml = config_yaml(&endpoints, key_file.to_str().unwrap(), &Options::default());
    let config_path = dir.path().join("cloud.yaml");
    std::fs::write(&config_path, yaml).unwrap();
    let db_path = dir.path().join("idoris.sqlite3");

    // 把预留 TTL 设得比一次请求可能持续的最长时间（总期限 + 流的总时长）还短：启动失败，不是悄悄放行
    let accounts = AccountStore::open(&db_path).unwrap();
    accounts
        .set_ledger_config(&LedgerConfig {
            billing_timezone: "UTC".into(),
            reservation_ttl_ms: 1_000,
        })
        .unwrap();
    let err = CloudUpstream::from_file(&config_path, &db_path).unwrap_err();
    assert!(err.contains("TTL"), "{err}");
    // 默认 TTL（30 分钟）满足
    accounts
        .set_ledger_config(&LedgerConfig {
            billing_timezone: "UTC".into(),
            reservation_ttl_ms: 1_800_000,
        })
        .unwrap();
    let cloud = Arc::new(CloudUpstream::from_file(&config_path, &db_path).unwrap());
    // 生产装配用的是账本：账户库里没有 acct-default，所以付款被拒（SimpleAdmission 会一律放行）
    let app = build_app(AppState {
        cloud: Some(cloud),
        ..AppState::default()
    });
    let reply = post(&app, &simple_body(100)).await;
    assert_eq!(reply.status, 403, "{}", reply.text());
    assert_eq!(
        reply.header("x-idoris-reason"),
        Some("payment_context_invalid")
    );
    assert_eq!(upstream.request_count(), 0);
}

#[tokio::test]
async fn the_s4_stand_in_admission_still_admits_everything_and_holds_nothing() {
    // 对照：S4 留下的测试替身没变（一律放行、不占额度），S4 的端到端测试靠它隔离准入
    let upstream = FakeUpstream::start(vec![ok("s4-1")]).await;
    let h = harness::build(
        vec![Endpoint::adult("example-alpha", upstream.url())],
        Options {
            admission: Some(Arc::new(SimpleAdmission)),
            ..Options::default()
        },
    );
    let reply = post(&h.app, &simple_body(100)).await;
    assert_eq!(reply.status, 200);
    // 没有钩子、没有账本：尝试记录照常工作，不占任何额度
    assert_eq!(h.store.attempt_rows(&reply.request_id()).unwrap().len(), 1);
}
