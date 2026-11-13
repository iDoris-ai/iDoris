//! 控制面（S2，HTTP 开通）发出去的**授信**和**免费额度**必须真的进准入账本（PR #600 Codex 评审 #6 / #7 / #8）：
//!
//! * #6 授信的权威数据源只有一个：`allocations`（purpose = `credit_line`）。控制面开通、有效期、撤销、准入读取，
//!   以及存储层的 `set_credit_line` / `disable_credit_line`，读写的是同一份记录；
//! * #7 免费额度走控制面发放时同样受补贴池该账期总上限约束，并且登记补贴池的承诺（`ledger_subsidy_grants`）；
//! * #8 免费额度的账期（`month`）和有效窗口不能脱钩：窗口必须恰好是这个月（账本时区）的窗口，
//!   否则同一个主体换个 `month` 标签就能在同一段时间里拿到两份额度。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::account::AccountKind;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::control::{ControlReason, WriteResult};
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::{AllocationId, BillingMonth, IdempotencyKey};
use idoris_tenancy::accounts::ledger::{CreditLineSetting, LedgerConfig};
use idoris_tenancy::accounts::{
    AccountStore, AllocationRecord, Failure, GrantAllocation, Meta, RevokeAllocation, Who, Written,
};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn operator() -> Who<'static> {
    Who::Operator("ledger-test")
}

fn grant_allocation(
    store: &AccountStore,
    request: &GrantAllocation,
    now: i64,
) -> Result<Written<AllocationRecord>, Failure> {
    store.control_grant_allocation(
        &operator(),
        &Meta {
            now: ts(now),
            request_id: "req-grant",
        },
        request,
    )
}

fn revoke_allocation_via_control(store: &AccountStore, id: &str, now: i64) {
    store
        .control_revoke_allocation(
            &operator(),
            &Meta {
                now: ts(now),
                request_id: "req-revoke",
            },
            &RevokeAllocation {
                id: AllocationId::new(id).unwrap(),
            },
        )
        .unwrap();
}

fn credit_request(id: &str, micros: i64, from: i64, until: i64) -> GrantAllocation {
    GrantAllocation {
        id: AllocationId::new(id).unwrap(),
        parent: aid("paid"),
        child: aid("emp"),
        amount: thb(micros),
        purpose: Purpose::CreditLine,
        valid_from: ts(from),
        valid_until: ts(until),
        month: None,
    }
}

/// `emp` 有 100 的余额、组织 `paid` 有 100 万；`paid` 从平台拿到了足够下发授信的额度（S0 要求授权方自己有授权）。
fn credit_world(db: &TempDb) -> AccountStore {
    let store = world(db);
    store
        .fund_account(&aid("emp"), &thb(100), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    allocate(&store, "e-0", "platform", "paid", 9_000_000);
    store
}

fn credit_limit(store: &AccountStore, now: i64) -> i64 {
    store
        .account_view(&aid("emp"), ts(now))
        .unwrap()
        .credit_limit
        .micros
}

// ------------------------------------------------------------------------------------------
// #6 授信：控制面开通的授信进准入计算，有效期、撤销一致
// ------------------------------------------------------------------------------------------

#[test]
fn a_credit_line_opened_through_the_control_plane_is_what_admission_reads() {
    // 旧代码：`purpose = credit_line` 只写进 allocations，准入读的是另一张表，授信永远是 0
    let db = TempDb::new("credit-control-plane");
    let store = credit_world(&db);
    assert_eq!(credit_limit(&store, T0), 0);
    let opened = grant_allocation(&store, &credit_request("cl-1", 200, T0, T0 + DAY), T0).unwrap();
    assert_eq!(opened.result, WriteResult::Created);
    assert_eq!(credit_limit(&store, T0 + 1), 200);
    // 全额预留 210 > 余额 100，旧代码只能降级；有 200 的授信就是 100 + 200 >= 210，全额放行
    let req = Req::new(&store, "r-1", 0, "cred-e", 100).at(T0 + 1);
    let decision = store.admit_attempt(&req.input()).unwrap();
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    assert_eq!(
        store.account_view(&aid("emp"), ts(T0 + 1)).unwrap().balance,
        Some(thb(100 - 210)),
        "靠授信透支"
    );
    assert_clean(&store);
}

#[test]
fn revoking_the_credit_allocation_closes_the_credit_line_at_once() {
    let db = TempDb::new("credit-revoke");
    let store = credit_world(&db);
    grant_allocation(&store, &credit_request("cl-1", 200, T0, T0 + DAY), T0).unwrap();
    assert_eq!(credit_limit(&store, T0 + 1), 200);
    revoke_allocation_via_control(&store, "cl-1", T0 + 2);
    assert_eq!(
        credit_limit(&store, T0 + 3),
        0,
        "撤销之后准入读到的授信是 0"
    );
    // 关掉的授信不能靠原样重发复活
    let revived = grant_allocation(&store, &credit_request("cl-1", 200, T0, T0 + DAY), T0 + 4);
    assert_eq!(
        revived.unwrap_err().reason,
        ControlReason::AllocationRevoked
    );
    assert_eq!(credit_limit(&store, T0 + 5), 0);
}

#[test]
fn a_credit_line_follows_its_validity_window() {
    let db = TempDb::new("credit-window");
    let store = credit_world(&db);
    grant_allocation(
        &store,
        &credit_request("cl-1", 200, T0 + HOUR, T0 + 2 * HOUR),
        T0,
    )
    .unwrap();
    assert_eq!(credit_limit(&store, T0), 0, "还没到 valid_from");
    assert_eq!(
        credit_limit(&store, T0 + HOUR),
        200,
        "valid_from 这一毫秒生效"
    );
    assert_eq!(credit_limit(&store, T0 + 2 * HOUR - 1), 200);
    assert_eq!(
        credit_limit(&store, T0 + 2 * HOUR),
        0,
        "valid_until 这一毫秒失效（右端开）"
    );
}

#[test]
fn the_storage_api_and_the_control_plane_share_one_source_of_truth() {
    let db = TempDb::new("credit-one-source");
    let store = credit_world(&db);
    grant_allocation(&store, &credit_request("cl-1", 200, T0, T0 + DAY), T0).unwrap();
    // 存储层的「设置授信」是替换：之前开的（控制面开的也一样）被撤销，现在生效的只有这一条
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(50),
                valid_until: Some(ts(T0 + HOUR)),
            },
            ts(T0 + 10),
        )
        .unwrap();
    assert_eq!(credit_limit(&store, T0 + 20), 50);
    let line = store.credit_line(&aid("emp")).unwrap().unwrap();
    assert!(line.enabled);
    assert_eq!(line.limit, thb(50));
    assert_eq!(line.valid_until, Some(ts(T0 + HOUR)));
    assert_eq!(line.guarantor, aid("paid"));
    // 被替换掉的那一条在控制面这边也是已撤销的（读额度的接口看得到），不是两份各自为政
    let record = store
        .allocation_record(&AllocationId::new("cl-1").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(record.revoked_at_ms, Some(T0 + 10));
    // 关授信 = 撤销所有还生效的授信额度
    store.disable_credit_line(&aid("emp"), ts(T0 + 30)).unwrap();
    assert_eq!(credit_limit(&store, T0 + 31), 0);
    let line = store.credit_line(&aid("emp")).unwrap().unwrap();
    assert!(!line.enabled && line.limit == thb(0));
    // 反过来：控制面撤销存储层开的授信，准入同样读到 0
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(70),
                valid_until: None,
            },
            ts(T0 + 40),
        )
        .unwrap();
    assert_eq!(credit_limit(&store, T0 + 41), 70);
    let id: String = scalar_text(
        &db,
        "SELECT allocation_id FROM allocations WHERE purpose = 'credit_line' ORDER BY rowid DESC LIMIT 1",
    );
    revoke_allocation_via_control(&store, &id, ts(T0 + 50).0);
    assert_eq!(credit_limit(&store, T0 + 51), 0);
}

/// 原生 SQL 读一个文本值。
fn scalar_text(db: &TempDb, sql: &str) -> String {
    rusqlite::Connection::open(db.path())
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

// ------------------------------------------------------------------------------------------
// #7 / #8 免费额度：补贴池上限、月份与有效窗口
// ------------------------------------------------------------------------------------------

/// 2026-11 / 2026-12 在 UTC 账期里的窗口（账本默认时区是 UTC）。
const NOV_START: i64 = T0 - 12 * HOUR;
const DEC_START: i64 = NOV_START + 30 * DAY;
const JAN_START: i64 = DEC_START + 31 * DAY;

fn free_request(
    id: &str,
    child: &str,
    month: BillingMonth,
    micros: i64,
    from: i64,
    until: i64,
) -> GrantAllocation {
    let parent = match child {
        "kid-a" | "kid-b" => "teacher",
        "kid-c" => "school",
        other => panic!("no parent for {other}"),
    };
    GrantAllocation {
        id: AllocationId::new(id).unwrap(),
        parent: aid(parent),
        child: aid(child),
        amount: thb(micros),
        purpose: Purpose::FreeTier,
        valid_from: ts(from),
        valid_until: ts(until),
        month: Some(month),
    }
}

fn november(id: &str, child: &str, micros: i64) -> GrantAllocation {
    free_request(id, child, month(2026, 11), micros, NOV_START, DEC_START)
}

/// 补贴池有 `cap` 的 11 月额度；授权方（老师、学校）从池往下都有足够的授权，窗口覆盖 11、12 月。
fn subsidy_world(db: &TempDb, cap: i64) -> AccountStore {
    let store = world(db);
    let mut parent = "platform";
    for (index, child) in ["pool", "school", "teacher"].into_iter().enumerate() {
        store
            .create_allocation(
                &idoris_compute_contracts::account::Allocation {
                    id: AllocationId::new(format!("a-{index}")).unwrap(),
                    parent: aid(parent),
                    child: aid(child),
                    ceiling: grant(9_000_000, Purpose::Prepaid, T0 - DAY, T0 + 90 * DAY),
                    idempotency_key: None,
                },
                t0(),
            )
            .unwrap();
        parent = child;
    }
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(cap), t0())
        .unwrap();
    store
}

fn granted_rows(db: &TempDb) -> i64 {
    scalar(db, "SELECT count(*) FROM ledger_subsidy_grants")
}

#[test]
fn free_tier_granted_through_the_control_plane_is_bounded_by_the_pool_cap_and_registered() {
    // 池当月上限 100、授权方的额度足够：向两个主体各发 60。旧代码两个都成功，承诺账是 0，
    // 实际花到 100 才拒绝，已经发出去的额度兑现不了
    let db = TempDb::new("subsidy-control-cap");
    let store = subsidy_world(&db, 100);
    let first = grant_allocation(&store, &november("f-a", "kid-a", 60), T0).unwrap();
    assert_eq!(first.result, WriteResult::Created);
    let second = grant_allocation(&store, &november("f-b", "kid-b", 60), T0);
    assert_eq!(
        second.unwrap_err().reason,
        ControlReason::AllocationExceedsGrantor,
        "池的总上限只剩 40，装不下第二个 60"
    );
    // 被拒的那一笔什么都没留下：没有额度行，也没有承诺行
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
    assert_eq!(granted_rows(&db), 1);
    let report = store.pool_report(&aid("pool"), month(2026, 11)).unwrap();
    assert_eq!(
        (report.cap, report.granted, report.consumed),
        (thb(100), thb(60), thb(0))
    );
    // 剩下的 40 还发得出去
    assert_eq!(
        grant_allocation(&store, &november("f-b2", "kid-b", 40), T0)
            .unwrap()
            .result,
        WriteResult::Created
    );
    assert_eq!(
        store
            .pool_report(&aid("pool"), month(2026, 11))
            .unwrap()
            .granted,
        thb(100)
    );
}

#[test]
fn a_pool_without_a_cap_for_the_month_grants_nothing_through_the_control_plane() {
    // 没设上限的账期上限是 0（失败即关闭），控制面发放同样如此
    let db = TempDb::new("subsidy-control-no-cap");
    let store = subsidy_world(&db, 100);
    let december = free_request("f-dec", "kid-a", month(2026, 12), 1, DEC_START, JAN_START);
    assert_eq!(
        grant_allocation(&store, &december, T0).unwrap_err().reason,
        ControlReason::AllocationExceedsGrantor
    );
    assert_eq!(granted_rows(&db), 0);
}

#[test]
fn replaying_a_control_plane_free_tier_grant_neither_duplicates_the_allocation_nor_the_commitment()
{
    let db = TempDb::new("subsidy-control-replay");
    let store = subsidy_world(&db, 100);
    grant_allocation(&store, &november("f-a", "kid-a", 60), T0).unwrap();
    // 同一个 (成员, 月份)、同样内容、换一个 id：返回已有的那一笔，承诺只占一次（60，不是 120）
    let again = grant_allocation(&store, &november("f-a2", "kid-a", 60), T0 + 1).unwrap();
    assert_eq!(again.result, WriteResult::Replayed);
    assert_eq!(again.value.allocation.id.as_str(), "f-a");
    assert_eq!(granted_rows(&db), 1);
    assert_eq!(
        store
            .pool_report(&aid("pool"), month(2026, 11))
            .unwrap()
            .granted,
        thb(60)
    );
    // 同一个 (成员, 月份)、不同金额：幂等键冲突，承诺不变
    let conflict = grant_allocation(&store, &november("f-a3", "kid-a", 70), T0 + 2);
    assert_eq!(
        conflict.unwrap_err().reason,
        ControlReason::IdempotencyConflict
    );
    assert_eq!(granted_rows(&db), 1);
}

#[test]
fn the_free_tier_window_must_be_exactly_the_month_in_the_billing_time_zone() {
    let db = TempDb::new("subsidy-control-window");
    let store = subsidy_world(&db, 1_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 12), &thb(1_000), t0())
        .unwrap();
    // 旧代码：month 只用来生成幂等键，窗口不核对。同一个主体、同一个 11 月窗口，month 分别写 2026-11 和 2026-12，
    // 换一个 allocation id，两笔都被接受，当前可用额度相加
    let nov_a = november("f-nov", "kid-a", 100);
    grant_allocation(&store, &nov_a, T0).unwrap();
    let mislabelled = free_request(
        "f-dec-mislabelled",
        "kid-a",
        month(2026, 12),
        100,
        NOV_START,
        DEC_START,
    );
    assert_eq!(
        grant_allocation(&store, &mislabelled, T0)
            .unwrap_err()
            .reason,
        ControlReason::InvalidAllocation,
        "月份写 12 月、窗口却是 11 月"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1,
        "没有第二笔"
    );
    // 窗口差一毫秒也不行；只覆盖月份的一部分也不行
    for (label, from, until) in [
        ("late start", NOV_START + 1, DEC_START),
        ("early end", NOV_START, DEC_START - 1),
        ("a week of the month", NOV_START, NOV_START + 7 * DAY),
        ("a window spanning two months", NOV_START, JAN_START),
    ] {
        let request = free_request("f-bad-window", "kid-b", month(2026, 11), 10, from, until);
        assert_eq!(
            grant_allocation(&store, &request, T0).unwrap_err().reason,
            ControlReason::InvalidAllocation,
            "{label}"
        );
    }
    // 对的月份配对的窗口，12 月那一笔是新的一笔，在 12 月才生效
    let december = free_request("f-dec", "kid-a", month(2026, 12), 100, DEC_START, JAN_START);
    assert_eq!(
        grant_allocation(&store, &december, T0).unwrap().result,
        WriteResult::Created
    );
    let ceiling_at = |now: i64| {
        store
            .account_view(&aid("kid-a"), ts(now))
            .unwrap()
            .ceiling_remaining
            .unwrap()
            .micros
    };
    assert_eq!(ceiling_at(T0), 100, "11 月里只有 11 月那一笔，不是两笔相加");
    assert_eq!(ceiling_at(DEC_START + 1), 100, "12 月里只有 12 月那一笔");
    // 账本时区改成泰国：窗口按泰国的月份算（UTC 的 11 月窗口不再对）
    let bangkok = TempDb::new("subsidy-control-window-bkk");
    let store = subsidy_world(&bangkok, 1_000);
    store
        .set_ledger_config(&LedgerConfig {
            billing_timezone: "Asia/Bangkok".into(),
            reservation_ttl_ms: 1_800_000,
        })
        .unwrap();
    assert_eq!(
        grant_allocation(&store, &nov_a, T0).unwrap_err().reason,
        ControlReason::InvalidAllocation,
        "UTC 的 11 月窗口在泰国账期里差了 7 小时"
    );
    let bkk_start = NOV_START - 7 * HOUR;
    let bkk_end = DEC_START - 7 * HOUR;
    let request = free_request("f-bkk", "kid-a", month(2026, 11), 100, bkk_start, bkk_end);
    assert_eq!(
        grant_allocation(&store, &request, T0).unwrap().result,
        WriteResult::Created
    );
}

#[test]
fn the_free_tier_a_member_gets_through_the_control_plane_is_spendable_and_counted_on_the_pool() {
    // 端到端：控制面发放 -> 准入花掉 -> 补贴池报表上 granted 和 consumed 都对得上
    let db = TempDb::new("subsidy-control-spend");
    let store = subsidy_world(&db, 1_000);
    grant_allocation(&store, &november("f-a", "kid-a", 500), T0).unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-a", 100).at(T0 + 1);
    let decision = store.admit_attempt(&req.input()).unwrap();
    assert!(decision.is_admitted(), "{decision:?}");
    store
        .settle_attempt(&req.attempt, &price(80), ts(T0 + 2))
        .unwrap();
    let report = store.pool_report(&aid("pool"), month(2026, 11)).unwrap();
    assert_eq!(
        (report.cap, report.granted, report.consumed),
        (thb(1_000), thb(500), thb(80))
    );
    assert_eq!(
        store.get_account(&aid("kid-a")).unwrap().unwrap().kind,
        AccountKind::Member
    );
    assert_clean(&store);
}
