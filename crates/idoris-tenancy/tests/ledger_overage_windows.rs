//! 超出预留的部分（overage）在**所有按归属时刻切的窗口**里是同一回事（Codex 三复审 High 1 / Low）。
//!
//! 没迟到的结算里超出预留的部分，担保人能吸收多少读的是**结算那一刻**生效的额度，所以账上它也属于结算的时刻：现在的结算把它另记一条
//! `late_settle`（归属时刻 = 结算时刻）。可 v0.5.0 / main 把它和预留之内的部分一起记在预留创建时刻的一条 `settle` 里，升级之后读旧分录、
//! 读只搬了一半的中间形态，都必须和新格式读出**同一本账**，不光是三本账的回放（`ledger_credit_books*.rs`），还有：
//!
//! * 额度窗口：没有授信的账户，能花的是此刻有效的额度减去窗口里已花的（`ceiling_remaining = 额度 - 窗口内的 Σ (held + spent)`）；
//! * 分钟 / 小时速率窗口（滑动窗口，`(now - 窗口, now]`）；
//! * 补贴池的账期消耗：准入的视图（`subsidy_pool_remaining`）和报表（`pool_report`）。
//!
//! 每个场景把**同一段历史**写成三种分录（新格式 / 中间形态 / 旧格式），期望数字全部手算，端到端断言：下一笔准入放不放行、窗口里的消耗。
//! 窗口里的消耗另有一份**独立于被测代码**的期望：`support/ledger_books.rs` 的 `attributed_spends`，从预留自己的记录（预留创建的时刻
//! 一笔、结算的时刻一笔）推出来，不看分录的 `window_at_ms`。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢，所以 `max_tokens = 45` 的预留
//! 正好是 100，`max_tokens = 20` 是 50，`max_tokens = 30` 是 70，`max_tokens = 10` 是最低有用的预留 30。预留的 TTL 是 30 分钟。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_books.rs"]
mod books;
#[path = "support/ledger_fixture.rs"]
mod fixture;

use books::spent_where;
use fixture::*;
use idoris_compute_contracts::account::Allocation;
use idoris_compute_contracts::admission::{AdmissionDecision, RateWindowKind};
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, IdempotencyKey};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::AccountLimits;

/// 旧额度在 `T` 到期，新额度在 `T` 生效。
const T: i64 = T0 + DAY;
/// 2026-12-01T00:00:00Z：补贴池的账期边界（账本时区默认 UTC）。
const DEC1: i64 = 1_796_083_200_000;

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn denied(decision: &AdmissionDecision) -> Option<ReasonCode> {
    match decision {
        AdmissionDecision::Reject { reason } => Some(reason.code()),
        _ => None,
    }
}

fn admit(store: &AccountStore, req: &Req) -> AdmissionDecision {
    store.admit_attempt(&req.input()).unwrap()
}

fn priced(decision: &AdmissionDecision) -> Option<i64> {
    decision.admitted().map(|a| a.reservation.price.micros())
}

/// 同一段历史的三种分录。预留 100、按 200 结算（超出 100）：
///
/// ```text
/// 新格式    reserve +100 | settle −100 / +100 | late_settle +100（结算时刻）
/// 中间形态  reserve +100 | settle −100 / +160 | late_settle +40（结算时刻）    超出的 100 里只搬了 40
/// 旧格式    reserve +100 | settle −100 / +200                                  超出的 100 还整笔留在 settle 里（v0.5.0 / main）
/// ```
#[derive(Clone, Copy, Debug)]
enum Encoding {
    New,
    Half,
    Old,
}

impl Encoding {
    fn name(self) -> &'static str {
        match self {
            Encoding::New => "new",
            Encoding::Half => "half",
            Encoding::Old => "old",
        }
    }

    /// 把现在的代码写下的（新格式）分录改成这种写法。
    fn rewrite(self, db: &TempDb, hold: &str) {
        match self {
            Encoding::New => {}
            Encoding::Half => rewrite_overage_partially(db, hold, 40),
            Encoding::Old => rewrite_overage_as_v0_5_0(db, hold),
        }
    }

    /// 付款账户上这个预留的分录：（种类，在途，已花，归属时刻）。
    fn rows(self, reserved_at: i64, settled_at: i64) -> Vec<(String, i64, i64, i64)> {
        let mut rows = vec![
            ("reserve".to_string(), 100, 0, reserved_at),
            (
                "settle".to_string(),
                -100,
                match self {
                    Encoding::New => 100,
                    Encoding::Half => 160,
                    Encoding::Old => 200,
                },
                reserved_at,
            ),
        ];
        match self {
            Encoding::New => rows.push(("late_settle".to_string(), 0, 100, settled_at)),
            Encoding::Half => rows.push(("late_settle".to_string(), 0, 40, settled_at)),
            Encoding::Old => {}
        }
        rows
    }
}

fn rows_of(
    store: &AccountStore,
    attempt: &idoris_compute_contracts::ids::AttemptId,
    account: &str,
) -> Vec<(String, i64, i64, i64)> {
    store
        .entries_of_hold(attempt, "model")
        .unwrap()
        .into_iter()
        .filter(|entry| entry.account == account)
        .map(|entry| {
            (
                entry.kind,
                entry.held,
                entry.spent,
                entry.window_at.millis(),
            )
        })
        .collect()
}

/// 一笔预付额度：`paid` 发给 `emp`，窗口 `[from, until)`，`T0` 创建。
fn prepaid(store: &AccountStore, id: &str, micros: i64, from: i64, until: i64) {
    store
        .create_allocation(
            &Allocation {
                id: AllocationId::new(id).unwrap(),
                parent: aid("paid"),
                child: aid("emp"),
                ceiling: grant(micros, Purpose::Prepaid, from, until),
                idempotency_key: None,
            },
            t0(),
        )
        .unwrap();
}

/// 为三种分录各生成一个测试，名字写清楚是哪种。
macro_rules! in_every_encoding {
    ($scenario:ident, $new:ident, $half:ident, $old:ident) => {
        #[test]
        fn $new() {
            $scenario(Encoding::New);
        }
        #[test]
        fn $half() {
            $scenario(Encoding::Half);
        }
        #[test]
        fn $old() {
            $scenario(Encoding::Old);
        }
    };
}

// ------------------------------------------------------------------------------------------
// 额度窗口：旧额度在 T 到期、新额度在 T 生效，T − 1ms 预留 100，T 按 200 结算
// ------------------------------------------------------------------------------------------

/// 从没有过授信、没有充值的账户 `emp`；上级 `paid` 的额度和余额都充裕。
///
/// 手算：
///
/// ```text
/// T − 1  预留 100：旧额度 [T − 1 小时, T) 容量 100，窗口里还没花过，放行 100
/// T      按 200 结算（超出 100）：新额度 [T, T + 1 小时) 容量 100 生效，窗口里还没花过，担保人能吸收 100：担保人承担 100，没有平台损失
/// 之后   新窗口里的消耗 = 超出的 100（结算时刻 T 的那一笔），新额度剩余 100 − 100 = 0：下一笔拒绝
///        旧窗口里的消耗 = 预留之内的 100（T − 1）
/// ```
///
/// 旧格式整笔 200 记在 T − 1：新窗口的消耗是 0，新额度剩余 100，下一笔 100 照样放行——同一块新额度被担保人吸收了一次，又被下一个请求花一次。
fn overage_in_the_allowance_window(encoding: Encoding) {
    let db = TempDb::new(&format!("overage-allowance-{}", encoding.name()));
    let store = world(&db);
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    allocate(&store, "e-0", "platform", "paid", 9_000_000);
    prepaid(&store, "emp-old", 100, T - HOUR, T);
    prepaid(&store, "emp-new", 100, T, T + HOUR);

    let first = Req::new(&store, "r-1", 0, "cred-e", 45).at(T - 1);
    assert_eq!(priced(&admit(&store, &first)), Some(100));
    let outcome = store
        .settle_attempt(&first.attempt, &price(200), ts(T))
        .unwrap();
    assert!(!outcome.late, "{outcome:?}");
    assert_eq!(outcome.overage, thb(100), "{outcome:?}");
    assert_eq!(outcome.user_borne, thb(100), "{outcome:?}");
    assert_eq!(outcome.sponsor_covered, thb(100), "{outcome:?}");
    assert_eq!(outcome.platform_loss, thb(0), "{outcome:?}");
    assert!(!outcome.credential_frozen, "{outcome:?}");
    drop(store);
    encoding.rewrite(&db, "r-1#0");

    let store = AccountStore::open(db.path()).unwrap();
    let name = encoding.name();
    assert_eq!(
        rows_of(&store, &first.attempt, "emp"),
        encoding.rows(T - 1, T),
        "{name}: the rows are written the way this encoding writes them"
    );
    // 独立的期望：两个窗口各花了多少（不看分录的 window_at_ms）
    assert_eq!(
        spent_where(db.path(), "emp", |at| at < T),
        100,
        "{name}: the old window holds the part inside the reservation"
    );
    assert_eq!(
        spent_where(db.path(), "emp", |at| at >= T),
        100,
        "{name}: the new window holds the overage"
    );
    // 被测：新窗口的额度剩余 100 − 100 = 0
    let view = store.account_view(&aid("emp"), ts(T + 1)).unwrap();
    assert_eq!(view.balance, None, "{name}");
    assert_eq!(
        view.ceiling_remaining,
        Some(thb(0)),
        "{name}: the new allowance 100 is spent by the overage 100"
    );
    // 端到端：下一笔（最低有用的预留 30）拒绝，宽限也不给没有钱的账户
    let next = Req::new(&store, "r-2", 0, "cred-e", 10).at(T + 1);
    let decision = admit(&store, &next);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "{name}: {decision:?}"
    );
    assert_clean(&store);
}

in_every_encoding!(
    overage_in_the_allowance_window,
    the_overage_counts_in_the_new_allowance_window_when_it_is_booked_as_it_is_now,
    the_overage_counts_in_the_new_allowance_window_when_half_of_it_is_still_inside_the_settle_entry,
    the_overage_counts_in_the_new_allowance_window_when_all_of_it_is_inside_the_settle_entry
);

// ------------------------------------------------------------------------------------------
// 速率窗口：分钟 / 小时，滑动窗口 (now − 窗口, now]
// ------------------------------------------------------------------------------------------

/// 充了值的账户 `emp`（余额 10 000，没有授信、没有额度），上限 150：
///
/// ```text
/// T0                  预留 100：窗口里 0 + 100 <= 150，放行
/// settle_at           按 200 结算（超出 100）：余额够，担保人承担 100，没有平台损失
/// read_at             窗口 (read_at − 窗口, read_at] 不含 T0，含 settle_at：
///                     新格式 / 中间形态 / 旧格式（读的时候分开）窗口里都是超出的 100；旧格式整笔 200 记在 T0，窗口里是 0
///                     下一笔 70：100 + 70 > 150 拒绝（RateLimited）；下一笔 50：100 + 50 <= 150 放行
/// ```
fn overage_in_a_rate_window(encoding: Encoding, window: RateWindowKind) {
    let (window_ms, settle_at, read_at) = match window {
        RateWindowKind::Minute => (MINUTE, T0 + 70_000, T0 + 71_000),
        RateWindowKind::Hour => (HOUR, T0 + 29 * MINUTE, T0 + 61 * MINUTE),
    };
    let name = format!("{}-{window:?}", encoding.name());
    let db = TempDb::new(&format!("overage-rate-{name}"));
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(10_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let limits = match window {
        RateWindowKind::Minute => AccountLimits {
            minute_cap: Some(thb(150)),
            ..AccountLimits::default()
        },
        RateWindowKind::Hour => AccountLimits {
            hour_cap: Some(thb(150)),
            ..AccountLimits::default()
        },
    };
    store
        .set_account_limits(&aid("emp"), &limits, t0())
        .unwrap();

    let first = Req::new(&store, "r-1", 0, "cred-e", 45);
    assert_eq!(priced(&admit(&store, &first)), Some(100), "{name}");
    let outcome = store
        .settle_attempt(&first.attempt, &price(200), ts(settle_at))
        .unwrap();
    assert!(!outcome.late, "{name}: {outcome:?}");
    assert_eq!(outcome.overage, thb(100), "{name}: {outcome:?}");
    assert_eq!(outcome.platform_loss, thb(0), "{name}: {outcome:?}");
    drop(store);
    encoding.rewrite(&db, "r-1#0");

    let store = AccountStore::open(db.path()).unwrap();
    assert_eq!(
        rows_of(&store, &first.attempt, "emp"),
        encoding.rows(T0, settle_at),
        "{name}: the rows are written the way this encoding writes them"
    );
    // 独立的期望：窗口里只有超出的 100（预留之内的 100 在 T0，已经滑出去了）
    assert_eq!(
        spent_where(db.path(), "emp", |at| at > read_at - window_ms),
        100,
        "{name}: only the overage is inside the window"
    );
    assert_eq!(spent_where(db.path(), "emp", |_| true), 200, "{name}");
    // 被测：视图里这个窗口的已用是 100
    let view = store.account_view(&aid("emp"), ts(read_at)).unwrap();
    let spent = view
        .rate_windows
        .iter()
        .find(|w| w.window == window)
        .map(|w| w.spent.clone());
    assert_eq!(spent, Some(thb(100)), "{name}: {view:?}");
    // 端到端：70 超出窗口，50 刚好放得下
    let too_much = Req::new(&store, "r-2", 0, "cred-e", 30).at(read_at);
    let decision = admit(&store, &too_much);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::RateLimited),
        "{name}: 100 + 70 > 150: {decision:?}"
    );
    let fits = Req::new(&store, "r-3", 0, "cred-e", 20).at(read_at);
    let decision = admit(&store, &fits);
    assert_eq!(
        priced(&decision),
        Some(50),
        "{name}: 100 + 50 <= 150: {decision:?}"
    );
    assert_clean(&store);
}

fn overage_in_the_minute_window(encoding: Encoding) {
    overage_in_a_rate_window(encoding, RateWindowKind::Minute);
}

fn overage_in_the_hour_window(encoding: Encoding) {
    overage_in_a_rate_window(encoding, RateWindowKind::Hour);
}

in_every_encoding!(
    overage_in_the_minute_window,
    the_overage_counts_in_the_minute_window_when_it_is_booked_as_it_is_now,
    the_overage_counts_in_the_minute_window_when_half_of_it_is_still_inside_the_settle_entry,
    the_overage_counts_in_the_minute_window_when_all_of_it_is_inside_the_settle_entry
);

in_every_encoding!(
    overage_in_the_hour_window,
    the_overage_counts_in_the_hour_window_when_it_is_booked_as_it_is_now,
    the_overage_counts_in_the_hour_window_when_half_of_it_is_still_inside_the_settle_entry,
    the_overage_counts_in_the_hour_window_when_all_of_it_is_inside_the_settle_entry
);

// ------------------------------------------------------------------------------------------
// 补贴池：账期边界（准入的视图和报表是同一个数）
// ------------------------------------------------------------------------------------------

/// 成员 `kid-a`（老师 → 学校 → 补贴池 → 平台），没有充值、没有授信：11 月和 12 月各一笔额度 1 000（窗口 `[T0, 12-01)`、`[12-01, 12-02)`），
/// 补贴池 11 月上限 1 000 000、12 月上限 150。
///
/// ```text
/// 12-01 − 1ms  预留 100（11 月）：池 11 月消耗 100
/// 12-01        按 200 结算（超出 100）：12 月的额度 1 000 生效，担保人吸收 100，没有平台损失
/// 之后         池 11 月的消耗 = 预留之内的 100，12 月的消耗 = 超出的 100（结算时刻在 12 月）
///              池 12 月剩余 150 − 100 = 50：下一笔 70 放不下，50 刚好
/// ```
///
/// 旧格式整笔 200 记在 11 月的最后一毫秒：11 月消耗 200、12 月 0，12 月剩余 150，下一笔 70 照样放行——报表和闸门都少算了 12 月的 100。
fn overage_in_the_subsidy_pool_period(encoding: Encoding) {
    let name = encoding.name();
    let db = TempDb::new(&format!("overage-pool-{name}"));
    let store = world(&db);
    allocate_chain(&store, "s", &["pool", "school", "teacher"], 100_000_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(1_000_000), t0())
        .unwrap();
    store
        .set_pool_cap(&aid("pool"), month(2026, 12), &thb(150), t0())
        .unwrap();
    for (id, from, until) in [("kid-nov", T0, DEC1), ("kid-dec", DEC1, DEC1 + DAY)] {
        store
            .create_allocation(
                &Allocation {
                    id: AllocationId::new(id).unwrap(),
                    parent: aid("teacher"),
                    child: aid("kid-a"),
                    ceiling: grant(1_000, Purpose::Prepaid, from, until),
                    idempotency_key: None,
                },
                t0(),
            )
            .unwrap();
    }

    let first = Req::new(&store, "r-1", 0, "cred-a", 45).at(DEC1 - 1);
    assert_eq!(priced(&admit(&store, &first)), Some(100), "{name}");
    let outcome = store
        .settle_attempt(&first.attempt, &price(200), ts(DEC1))
        .unwrap();
    assert!(!outcome.late, "{name}: {outcome:?}");
    assert_eq!(outcome.overage, thb(100), "{name}: {outcome:?}");
    assert_eq!(outcome.sponsor_covered, thb(100), "{name}: {outcome:?}");
    assert_eq!(outcome.platform_loss, thb(0), "{name}: {outcome:?}");
    drop(store);
    encoding.rewrite(&db, "r-1#0");

    let store = AccountStore::open(db.path()).unwrap();
    assert_eq!(
        rows_of(&store, &first.attempt, "pool"),
        encoding.rows(DEC1 - 1, DEC1),
        "{name}: the pool's rows are written the way this encoding writes them"
    );
    // 独立的期望：两个账期各花了多少
    assert_eq!(
        spent_where(db.path(), "pool", |at| at < DEC1),
        100,
        "{name}"
    );
    assert_eq!(
        spent_where(db.path(), "pool", |at| at >= DEC1),
        100,
        "{name}"
    );
    // 报表：每月花了多少一眼可见
    let november = store.pool_report(&aid("pool"), month(2026, 11)).unwrap();
    assert_eq!(
        november.consumed,
        thb(100),
        "{name}: November holds the part inside the reservation"
    );
    let december = store.pool_report(&aid("pool"), month(2026, 12)).unwrap();
    assert_eq!(december.cap, thb(150), "{name}");
    assert_eq!(
        december.consumed,
        thb(100),
        "{name}: December holds the overage"
    );
    // 闸门：和报表是同一个数
    let view = store.account_view(&aid("pool"), ts(DEC1 + 1)).unwrap();
    assert_eq!(
        view.subsidy_pool_remaining,
        Some(thb(50)),
        "{name}: 150 - 100"
    );
    // 端到端：70 放不下池 12 月剩下的 50，50 刚好
    let too_big = Req::new(&store, "r-2", 0, "cred-a", 30).at(DEC1 + 1);
    let decision = admit(&store, &too_big);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::SubsidyPoolExhausted),
        "{name}: 70 > 50: {decision:?}"
    );
    let fits = Req::new(&store, "r-3", 0, "cred-a", 20).at(DEC1 + 2);
    let decision = admit(&store, &fits);
    assert_eq!(priced(&decision), Some(50), "{name}: 50 fits: {decision:?}");
    assert_clean(&store);
}

in_every_encoding!(
    overage_in_the_subsidy_pool_period,
    the_overage_counts_in_the_pool_period_of_the_settlement_when_it_is_booked_as_it_is_now,
    the_overage_counts_in_the_pool_period_of_the_settlement_when_half_of_it_is_still_inside_the_settle_entry,
    the_overage_counts_in_the_pool_period_of_the_settlement_when_all_of_it_is_inside_the_settle_entry
);
