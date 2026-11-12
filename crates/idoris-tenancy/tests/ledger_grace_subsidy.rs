//! S3 准入账本：宽限（每成员每周期一次、跨周期重置、金额上限、不依赖 overage、超出部分由平台承担）、
//! 免费额度发放（幂等、补贴池总上限、跨月）、附加成本（安全检查）的预留与结算、账本配置与时区。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::account::AccountStatus;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::grant::Purpose;
use idoris_compute_contracts::ids::IdempotencyKey;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{
    AccountLimits, AlertKind, ComponentDecision, CreditLineSetting, FreeTierIssue, FreeTierOutcome,
    FundOutcome, HoldState, LedgerConfig, LedgerError, SettleResult,
};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn denied(decision: &AdmissionDecision) -> Option<ReasonCode> {
    match decision {
        AdmissionDecision::Reject { reason } => Some(reason.code()),
        _ => None,
    }
}

/// `emp` 有 `emp_funds`（没有授信），组织 `paid` 有 100 万。
fn paid_chain(db: &TempDb, emp_funds: i64) -> AccountStore {
    let store = world(db);
    store
        .fund_account(&aid("emp"), &thb(emp_funds), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    store
}

fn balance(store: &AccountStore, account: &str, now: i64) -> i64 {
    store
        .account_view(&aid(account), ts(now))
        .unwrap()
        .balance
        .unwrap()
        .micros
}

/// 先不允许宽限试一遍（压低 max_tokens），不行再允许宽限：和云执行器的两遍一样。
fn admit_with_ladder(
    store: &AccountStore,
    request: &str,
    index: u8,
    credential: &str,
    max_tokens: u64,
    now: i64,
) -> AdmissionDecision {
    let first = Req::new(store, request, index, credential, max_tokens).at(now);
    let decision = store.admit_attempt(&first.input()).unwrap();
    if decision.is_admitted() || denied(&decision) != Some(ReasonCode::BudgetExhausted) {
        return decision;
    }
    let second = Req::new(store, request, index, credential, max_tokens)
        .at(now)
        .grace();
    store.admit_attempt(&second.input()).unwrap()
}

// ------------------------------------------------------------------------------------------
// 宽限
// ------------------------------------------------------------------------------------------

#[test]
fn grace_is_the_last_resort_a_full_reservation_beyond_the_funds_for_one_round() {
    let db = TempDb::new("grace-basic");
    let store = paid_chain(&db, 20);
    // 余额 20：最低有用的预留要 30，压低放不下；不允许宽限就是 402
    let plain = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert_eq!(
        denied(&store.admit_attempt(&plain.input()).unwrap()),
        Some(ReasonCode::BudgetExhausted)
    );
    // 允许宽限：放行完整的一次预留（210），原因码 grace_used
    let graced = Req::new(&store, "r-1", 0, "cred-e", 100).grace();
    let decision = store.admit_attempt(&graced.input()).unwrap();
    let AdmissionDecision::Grace { admitted } = &decision else {
        panic!("expected grace, got {decision:?}");
    };
    assert_eq!(decision.reason_code(), Some(ReasonCode::GraceUsed));
    assert_eq!(admitted.reservation.price.micros(), 210);
    assert_eq!(admitted.effective_max_tokens, 100);
    // 占着完整的预留：账户现在是 20 - 210
    assert_eq!(balance(&store, "emp", T0), -190);
    // 记录：每个账户每个账期一次；告警通知担保人（emp 的父账户 paid）
    let uses = store.grace_uses(&aid("emp")).unwrap();
    assert_eq!(uses.len(), 1);
    assert_eq!(uses[0].period.to_string(), "2026-11");
    assert_eq!(
        (uses[0].reserve.micros, uses[0].shortfall.micros),
        (210, 190)
    );
    assert!(uses[0].voided_at.is_none());
    let alerts = store.alerts(Some(AlertKind::GraceUsed)).unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(
        (alerts[0].account.as_str(), alerts[0].guarantor.as_deref()),
        ("emp", Some("paid"))
    );
    assert_eq!(alerts[0].amount, thb(190));
    let hold = store.hold(&graced.attempt, "model").unwrap().unwrap();
    assert_eq!(hold.grace_floor, Some(thb(20)), "放行时可用额度 + 授信");
    assert_clean(&store);
}

#[test]
fn only_one_grace_per_member_per_period_and_it_resets_in_the_next_period() {
    let db = TempDb::new("grace-once");
    let store = paid_chain(&db, 20);
    let first = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert!(matches!(first, AdmissionDecision::Grace { .. }));
    // 把这一轮结算掉：实际 100，账户 20 - 100 = -80（透支了）
    store
        .settle_attempt(&attempt("r-1", 0), &price(100), ts(T0 + 1_000))
        .unwrap();
    // 同一周期再来：账户已经透支，也用过宽限了 -> 402
    let again = admit_with_ladder(&store, "r-2", 0, "cred-e", 100, T0 + 2_000);
    assert_eq!(
        denied(&again),
        Some(ReasonCode::BudgetExhausted),
        "{again:?}"
    );
    // 账户补一点钱，让它回到「没有透支」（-80 + 100 = 20）：本周期宽限已经用过，仍然 402
    store
        .fund_account(&aid("emp"), &thb(100), &key("f-emp-2"), ts(T0 + 3_000))
        .unwrap();
    assert_eq!(balance(&store, "emp", T0 + 3_000), 20);
    let still = admit_with_ladder(&store, "r-3", 0, "cred-e", 100, T0 + 3_000);
    assert_eq!(
        denied(&still),
        Some(ReasonCode::BudgetExhausted),
        "同一个周期最多一次"
    );
    // 下一个周期（12 月）重置：账户还是 20，宽限又可以用了
    let next_month = T0 + 31 * DAY;
    assert_eq!(balance(&store, "emp", next_month), 20);
    let renewed = admit_with_ladder(&store, "r-4", 0, "cred-e", 100, next_month);
    assert!(
        matches!(renewed, AdmissionDecision::Grace { .. }),
        "{renewed:?}"
    );
    let periods: Vec<String> = store
        .grace_uses(&aid("emp"))
        .unwrap()
        .iter()
        .map(|u| u.period.to_string())
        .collect();
    assert_eq!(periods, ["2026-11", "2026-12"]);
}

fn attempt(request: &str, index: u8) -> idoris_compute_contracts::ids::AttemptId {
    idoris_compute_contracts::ids::AttemptId::new(
        idoris_compute_contracts::ids::RequestId::new(request).unwrap(),
        idoris_compute_contracts::ids::AttemptIndex(index),
    )
}

#[test]
fn the_period_is_the_calendar_month_in_the_ledger_time_zone() {
    // 泰国首发：账期按 Asia/Bangkok。2026-10-31T18:00Z 在泰国已经是 11 月 1 日
    // 2026-10-31T16:59Z 在泰国是 10 月 31 日 23:59；一个多小时之后（18:00Z）已经是泰国的 11 月 1 日 01:00。
    let before_midnight = T0 - 19 * HOUR - MINUTE; // 2026-10-31T16:59:00Z
    let after_midnight = T0 - 18 * HOUR; // 2026-10-31T18:00:00Z
    let periods_used = |zone: &str| -> (Vec<String>, bool) {
        let db = TempDb::new("grace-zone");
        let store = paid_chain(&db, 20);
        store
            .set_ledger_config(&LedgerConfig {
                billing_timezone: zone.into(),
                reservation_ttl_ms: 1_800_000,
            })
            .unwrap();
        let first = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, before_midnight);
        assert!(
            matches!(first, AdmissionDecision::Grace { .. }),
            "{first:?}"
        );
        store
            .settle_attempt(&attempt("r-1", 0), &price(100), ts(before_midnight + 1_000))
            .unwrap();
        store
            .fund_account(
                &aid("emp"),
                &thb(100),
                &key("f-emp-2"),
                ts(before_midnight + 2_000),
            )
            .unwrap();
        assert_eq!(balance(&store, "emp", after_midnight), 20);
        let second = admit_with_ladder(&store, "r-2", 0, "cred-e", 100, after_midnight);
        let periods = store
            .grace_uses(&aid("emp"))
            .unwrap()
            .iter()
            .map(|u| u.period.to_string())
            .collect();
        (periods, matches!(second, AdmissionDecision::Grace { .. }))
    };
    // 泰国：10 月和 11 月是两个周期，第二次宽限可以用
    assert_eq!(
        periods_used("Asia/Bangkok"),
        (vec!["2026-10".to_string(), "2026-11".to_string()], true)
    );
    // 对照 UTC：两次都在 10 月，同一个周期，第二次被拒
    assert_eq!(periods_used("UTC"), (vec!["2026-10".to_string()], false));
}

#[test]
fn a_grace_round_that_charged_nothing_is_voided_so_a_failed_candidate_does_not_burn_it() {
    let db = TempDb::new("grace-void");
    let store = paid_chain(&db, 20);
    let first = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert!(matches!(first, AdmissionDecision::Grace { .. }));
    // 这个候选失败了，没有向用户收费：释放，宽限作废
    store
        .release_attempt(&attempt("r-1", 0), ts(T0 + 1_000))
        .unwrap()
        .unwrap();
    let uses = store.grace_uses(&aid("emp")).unwrap();
    assert_eq!(uses.len(), 1);
    assert!(uses[0].voided_at.is_some());
    // 切换到下一个候选：宽限还在，可以再用一次
    let second = admit_with_ladder(&store, "r-1", 1, "cred-e", 100, T0 + 2_000);
    assert!(
        matches!(second, AdmissionDecision::Grace { .. }),
        "{second:?}"
    );
    // 数据库层：本周期没作废的宽限至多一行
    let active = scalar(
        &db,
        "SELECT COUNT(*) FROM ledger_grace_uses WHERE account_id = 'emp' AND voided_at_ms IS NULL",
    );
    assert_eq!(active, 1);
    assert_clean(&store);
}

#[test]
fn the_database_allows_at_most_one_active_grace_per_account_per_period() {
    let db = TempDb::new("grace-unique");
    let store = paid_chain(&db, 20);
    admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    // 绕过 Rust 层：先塞一个同账户同账期的（伪造的）开放预留，再给它塞一行没作废的宽限。
    // 预留是新的 hold_id，别的触发器（hold_id 唯一、宽限必须属于同账户同单位同账期同下限的开放预留）都放行，
    // 所以挡住第二行的只可能是「每账户每账期至多一行没作废的宽限」这条唯一索引。
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute(
        "INSERT INTO ledger_holds (hold_id, attempt_id, component, request_id, attempt_index, payer_account,
                                   credential_id, unit, reserved_micros, period, created_at_ms, expires_at_ms,
                                   grace_floor_micros, state)
         SELECT 'r-9#0', 'r-9#0', 'model', 'r-9', 0, payer_account, credential_id, unit, reserved_micros, period,
                created_at_ms, expires_at_ms, grace_floor_micros, 'open'
           FROM ledger_holds WHERE hold_id = 'r-1#0'",
        [],
    )
    .unwrap();
    let err = conn
        .execute(
            "INSERT INTO ledger_grace_uses (account_id, period, hold_id, unit, reserve_micros, shortfall_micros,
                                            floor_micros, used_at_ms)
             VALUES ('emp', '2026-11', 'r-9#0', 'THB', 210, 190, 20, 0)",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("UNIQUE") && err.contains("ledger_grace_uses"),
        "{err}"
    );
    // 作废了第一行之后，同一个账期可以再有一行：唯一的是「没作废的」
    conn.execute(
        "UPDATE ledger_grace_uses SET voided_at_ms = 1 WHERE hold_id = 'r-1#0'",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO ledger_grace_uses (account_id, period, hold_id, unit, reserve_micros, shortfall_micros,
                                        floor_micros, used_at_ms)
         VALUES ('emp', '2026-11', 'r-9#0', 'THB', 210, 190, 20, 0)",
        [],
    )
    .unwrap();
}

#[test]
fn a_grace_round_costs_the_platform_at_most_one_reservation_and_only_beyond_the_credit_floor() {
    let db = TempDb::new("grace-absorbed");
    let store = paid_chain(&db, 20);
    let first = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0); // 预留 210，下限 20
    assert!(matches!(first, AdmissionDecision::Grace { .. }));
    // 实际 100：账户自己的 20 先付，超出下限的 80 由平台承担（不是担保人，不冻结凭证）
    let outcome = store
        .settle_attempt(&attempt("r-1", 0), &price(100), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.grace_absorbed, thb(80));
    assert_eq!(outcome.overage, thb(0));
    assert_eq!(outcome.platform_loss, thb(0), "宽限不是 overage，不算损失");
    assert!(!outcome.credential_frozen);
    assert_eq!(store.loss_total("grace_absorbed").unwrap(), 80);
    assert_eq!(store.loss_total("platform_loss").unwrap(), 0);
    assert_eq!(
        store
            .get_credential(&cid("cred-e"))
            .unwrap()
            .unwrap()
            .status(),
        idoris_compute_contracts::credential::CredentialStatus::Active
    );
    // 上限：实际用满预留（210）也只是 210 - 20 = 190 = 缺口，不会更多
    let db2 = TempDb::new("grace-absorbed-cap");
    let store2 = paid_chain(&db2, 20);
    admit_with_ladder(&store2, "r-1", 0, "cred-e", 100, T0);
    let outcome = store2
        .settle_attempt(&attempt("r-1", 0), &price(210), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.grace_absorbed, thb(190));
    // 用得比账户自己的额度还少：平台一分不出
    let db3 = TempDb::new("grace-absorbed-none");
    let store3 = paid_chain(&db3, 20);
    admit_with_ladder(&store3, "r-1", 0, "cred-e", 100, T0);
    let outcome = store3
        .settle_attempt(&attempt("r-1", 0), &price(15), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.grace_absorbed, thb(0));
    assert_eq!(store3.loss_total("grace_absorbed").unwrap(), 0);
    assert_clean(&store3);
}

#[test]
fn an_overage_on_top_of_a_grace_round_is_still_a_loss_and_freezes_the_credential() {
    let db = TempDb::new("grace-overage");
    let store = paid_chain(&db, 20);
    admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    // 实际 300，超出预留 210 的 90 是 overage：账户已经在下限之下，没有余量，全部记平台损失并冻结
    let outcome = store
        .settle_attempt(&attempt("r-1", 0), &price(300), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.overage, thb(90));
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (0, 90)
    );
    assert_eq!(outcome.grace_absorbed, thb(190));
    assert!(outcome.credential_frozen);
}

#[test]
fn grace_eligibility_does_not_depend_on_earlier_overage() {
    // 宽限资格只由「本周期次数、可用额度、预留」决定，不读 settle 的 overage（评审 B4）：
    // 先发生一次 overage（担保人承担，没有损失），账户余额回到 20，宽限照样可以用。
    let db = TempDb::new("grace-vs-overage");
    let store = paid_chain(&db, 400);
    let first = Req::new(&store, "r-0", 0, "cred-e", 100);
    store.admit_attempt(&first.input()).unwrap();
    let outcome = store
        .settle_attempt(&first.attempt, &price(400), ts(T0 + 1_000))
        .unwrap();
    // 预留 210，实际 400：overage 190，链上还能吸收（emp 剩 190，paid 很多），全部由担保人承担，平台没有损失
    assert_eq!(outcome.overage, thb(190));
    assert_eq!(outcome.sponsor_covered, thb(190));
    assert_eq!(outcome.platform_loss, thb(0));
    assert!(!outcome.credential_frozen);
    assert!(
        store
            .alerts(Some(AlertKind::PlatformLoss))
            .unwrap()
            .is_empty()
    );
    // overage 是事实，照常记账：已花是实际的 400，余额 0
    assert_eq!(balance(&store, "emp", T0 + 1_000), 0);
    store
        .fund_account(&aid("emp"), &thb(20), &key("f-emp-2"), ts(T0 + 2_000))
        .unwrap();
    assert_eq!(balance(&store, "emp", T0 + 2_000), 20);
    let decision = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0 + 3_000);
    assert!(
        matches!(decision, AdmissionDecision::Grace { .. }),
        "{decision:?}"
    );
}

#[test]
fn a_revoked_allowance_does_not_earn_a_grace_round() {
    // emp 只有一笔 20 的额度（放不下最低有用的预留 30）：没撤销时宽限一轮；管理员把这笔额度撤销之后，
    // 账户就是「被明确关掉」，不该再有一次平台买单的宽限
    let build = |tag: &str| {
        let db = TempDb::new(tag);
        let store = world(&db);
        allocate(&store, "e-0", "platform", "paid", 9_000_000);
        allocate(&store, "e-1", "paid", "emp", 20);
        (db, store)
    };
    let (_db, store) = build("grace-allowance-live");
    let decision = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert!(
        matches!(decision, AdmissionDecision::Grace { .. }),
        "{decision:?}"
    );

    let (db, store) = build("grace-allowance-revoked");
    revoke_allocation(&db, "e-1");
    let decision = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
}

#[test]
fn grace_cannot_rescue_what_is_not_the_payers_own_money() {
    // 缺钱的是组织（祖先），不是付款账户自己：那是别人的钱，宽限帮不了
    let db = TempDb::new("grace-ancestor");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(20), &key("f-paid"), t0())
        .unwrap();
    let decision = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
}

#[test]
fn grace_cannot_get_around_the_platform_protections() {
    // 钱不够，同时速率也超：宽限不能绕过速率闸门
    let db = TempDb::new("grace-vs-rate");
    let store = paid_chain(&db, 20);
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                minute_cap: Some(thb(100)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let decision = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert!(denied(&decision).is_some(), "{decision:?}");
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
}

#[test]
fn grace_is_never_given_to_an_account_that_is_already_overdrawn() {
    // S0 的 check_grace：宽限是「最后一单」，不是给已经欠着钱的账户续命（缺口会超过一次预留）
    let db = TempDb::new("grace-overdrawn");
    let store = paid_chain(&db, 100);
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(500),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    // 先靠授信透支：预留 210，余额 100 -> -110（在授信之内，放行）
    let first = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(matches!(
        store.admit_attempt(&first.input()).unwrap(),
        AdmissionDecision::Allow { .. }
    ));
    store
        .settle_attempt(&first.attempt, &price(210), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(balance(&store, "emp", T0 + 1_000), -110);
    // 把授信关掉：账户在透支，钱不够，宽限也不给
    store
        .disable_credit_line(&aid("emp"), ts(T0 + 2_000))
        .unwrap();
    let decision = admit_with_ladder(&store, "r-2", 0, "cred-e", 100, T0 + 3_000);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
}

#[test]
fn an_account_that_was_never_provisioned_gets_no_grace_but_one_whose_allowance_ran_out_does() {
    // 宽限是「额度用完之后的最后一单」。一个什么都没配置过的账户（可用额是失败即关闭填出来的 0）没有「用完」，
    // 给它宽限就是让每个新账户每月白拿一次完整的预留（平台买单）。
    let db = TempDb::new("grace-unprovisioned");
    let store = world(&db);
    let decision = admit_with_ladder(&store, "r-1", 0, "cred-e", 100, T0);
    assert_eq!(
        denied(&decision),
        Some(ReasonCode::BudgetExhausted),
        "{decision:?}"
    );
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
    // 对照：有过额度、现在过期了（比如上个月的免费额度，新一个月的还没发）：算「用完」，可以宽限一轮
    let db2 = TempDb::new("grace-expired-allowance");
    let store2 = subsidy_world(&db2, 1_000_000);
    issue(&store2, "p-a", "kid-a", 2026, 11, 1_000, T0).unwrap();
    store2
        .set_pool_cap(
            &aid("pool"),
            month(2026, 12),
            &thb(1_000_000),
            ts(T0 + 31 * DAY),
        )
        .unwrap();
    let december = T0 + 31 * DAY;
    let decision = admit_with_ladder(&store2, "r-1", 0, "cred-a", 100, december);
    assert!(
        matches!(decision, AdmissionDecision::Grace { .. }),
        "{decision:?}"
    );
}

#[test]
fn allow_grace_false_never_returns_grace() {
    let db = TempDb::new("grace-flag");
    let store = paid_chain(&db, 20);
    let plain = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(!matches!(
        store.admit_attempt(&plain.input()).unwrap(),
        AdmissionDecision::Grace { .. }
    ));
    assert!(store.grace_uses(&aid("emp")).unwrap().is_empty());
}

// ------------------------------------------------------------------------------------------
// 免费额度发放
// ------------------------------------------------------------------------------------------

fn issue(
    store: &AccountStore,
    principal: &str,
    account: &str,
    y: u16,
    m: u8,
    micros: i64,
    now: i64,
) -> Result<FreeTierOutcome, LedgerError> {
    store.issue_free_tier(&FreeTierIssue {
        principal: &pid(principal),
        member_account: &aid(account),
        month: month(y, m),
        amount: thb(micros),
        now: ts(now),
    })
}

/// 补贴池 11 月上限 `cap`；学校、老师这些中间账户的额度充足（免费额度只是成员自己的那一段）。
fn subsidy_world(db: &TempDb, cap: i64) -> AccountStore {
    let store = world(db);
    allocate_chain(&store, "s", &["pool", "school", "teacher"], 100_000_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(cap), t0())
        .unwrap();
    store
}

#[test]
fn a_free_tier_grant_is_a_month_window_allowance_and_spending_it_shows_up_on_the_pool() {
    let db = TempDb::new("free-basic");
    let store = subsidy_world(&db, 1_000_000);
    let outcome = issue(&store, "p-a", "kid-a", 2026, 11, 1_000, T0).unwrap();
    let FreeTierOutcome::Granted(allocation) = &outcome else {
        panic!("expected a new grant, got {outcome:?}");
    };
    assert_eq!(allocation.ceiling.purpose(), Purpose::FreeTier);
    assert_eq!(allocation.ceiling.amount(), &thb(1_000));
    assert_eq!(
        allocation.parent,
        aid("teacher"),
        "额度来自成员的父账户，补贴池是祖先"
    );
    assert_eq!(
        allocation.idempotency_key.as_ref().unwrap().as_str(),
        "subsidy:p-a:2026-11"
    );
    // 有效期是账本时区（默认 UTC）的 11 月：[11-01 00:00, 12-01 00:00)
    let (from, until) = (
        allocation.ceiling.valid_from().millis(),
        allocation.ceiling.valid_until().millis(),
    );
    assert_eq!(from, 1_793_491_200_000, "2026-11-01T00:00:00Z");
    assert_eq!(until, 1_796_083_200_000, "2026-12-01T00:00:00Z");
    // 成员能花它：投影 = 额度 1_000
    assert_eq!(
        store
            .account_view(&aid("kid-a"), t0())
            .unwrap()
            .ceiling_remaining,
        Some(thb(1_000))
    );
    let request = Req::new(&store, "r-1", 0, "cred-a", 100);
    assert!(matches!(
        store.admit_attempt(&request.input()).unwrap(),
        AdmissionDecision::Allow { .. }
    ));
    store
        .settle_attempt(&request.attempt, &price(150), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(
        store
            .account_view(&aid("kid-a"), t0())
            .unwrap()
            .ceiling_remaining,
        Some(thb(850))
    );
    // 「补贴池记一笔支出，成员账户记一笔消耗」：每月花了多少一眼可见
    let report = store.pool_report(&aid("pool"), month(2026, 11)).unwrap();
    assert_eq!(report.cap, thb(1_000_000));
    assert_eq!(report.granted, thb(1_000));
    assert_eq!(report.consumed, thb(150));
    let member_spent: i64 = scalar(
        &db,
        "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = 'kid-a'",
    );
    assert_eq!(member_spent, 150);
    assert_clean(&store);
}

#[test]
fn issuing_twice_for_the_same_member_and_month_is_idempotent() {
    let db = TempDb::new("free-idem");
    let store = subsidy_world(&db, 1_000_000);
    let first = issue(&store, "p-a", "kid-a", 2026, 11, 1_000, T0).unwrap();
    let again = issue(&store, "p-a", "kid-a", 2026, 11, 1_000, T0 + 5_000).unwrap();
    let FreeTierOutcome::Replayed(existing) = &again else {
        panic!("expected a replay, got {again:?}");
    };
    assert_eq!(existing.id, first.allocation().id, "拿到的是同一条额度");
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_subsidy_grants"), 1);
    // 池只占一份
    assert_eq!(
        store
            .pool_report(&aid("pool"), month(2026, 11))
            .unwrap()
            .granted,
        thb(1_000)
    );
    // 同一个键不同的金额：冲突，不悄悄改额度
    let err = issue(&store, "p-a", "kid-a", 2026, 11, 2_000, T0 + 6_000).unwrap_err();
    assert!(matches!(err, LedgerError::GrantConflict(_)), "{err}");
}

#[test]
fn the_pool_cap_bounds_what_can_be_granted_in_a_period() {
    let db = TempDb::new("free-pool-cap");
    let store = subsidy_world(&db, 1_000);
    issue(&store, "p-a", "kid-a", 2026, 11, 600, T0).unwrap();
    // 600 + 600 > 1_000：超出池的总上限，拒绝，什么都没写
    let err = issue(&store, "p-b", "kid-b", 2026, 11, 600, T0).unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::PoolCapExceeded {
                requested: 600,
                remaining: 400,
                ..
            }
        ),
        "{err}"
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
    // 刚好用完（边界）
    issue(&store, "p-b", "kid-b", 2026, 11, 400, T0).unwrap();
    let err = issue(&store, "p-c", "kid-c", 2026, 11, 1, T0).unwrap_err();
    assert!(
        matches!(err, LedgerError::PoolCapExceeded { remaining: 0, .. }),
        "{err}"
    );
    // 一个账期没设上限就是 0
    let err = issue(&store, "p-c", "kid-c", 2026, 12, 1, T0).unwrap_err();
    assert!(
        matches!(err, LedgerError::PoolCapExceeded { remaining: 0, .. }),
        "{err}"
    );
}

#[test]
fn a_new_month_is_a_new_grant_with_its_own_cap_and_the_old_allowance_expires() {
    let db = TempDb::new("free-cross-month");
    let store = subsidy_world(&db, 1_000);
    issue(&store, "p-a", "kid-a", 2026, 11, 1_000, T0).unwrap();
    let dec_1 = T0 + 31 * DAY; // 2026-12-02
    // 12 月：上限另设；同一个主体 12 月是另一个幂等键（另一份额度）
    store
        .set_pool_cap(&aid("pool"), month(2026, 12), &thb(500), ts(dec_1))
        .unwrap();
    let december = issue(&store, "p-a", "kid-a", 2026, 12, 500, dec_1).unwrap();
    assert!(matches!(december, FreeTierOutcome::Granted(_)));
    assert_eq!(
        december
            .allocation()
            .idempotency_key
            .as_ref()
            .unwrap()
            .as_str(),
        "subsidy:p-a:2026-12"
    );
    // 11 月的额度到 12 月就过期了：12 月的可用额只有 12 月那份，11 月没花完的不累积
    let view = store.account_view(&aid("kid-a"), ts(dec_1)).unwrap();
    assert_eq!(view.ceiling_remaining, Some(thb(500)));
    // 过去的月份发不出去
    let err = issue(&store, "p-b", "kid-b", 2026, 11, 1, dec_1).unwrap_err();
    assert!(matches!(err, LedgerError::Invalid(_)), "{err}");
}

#[test]
fn free_tier_grants_are_validated() {
    let db = TempDb::new("free-validation");
    let store = subsidy_world(&db, 1_000_000);
    // 金额必须为正、单位与账户一致
    assert!(matches!(
        issue(&store, "p-a", "kid-a", 2026, 11, 0, T0).unwrap_err(),
        LedgerError::Invalid(_)
    ));
    assert!(issue(&store, "p-a", "kid-a", 2026, 11, -5, T0).is_err());
    let usd =
        idoris_compute_contracts::money::Money::new(idoris_compute_contracts::money::Unit::Usd, 5);
    let err = store
        .issue_free_tier(&FreeTierIssue {
            principal: &pid("p-a"),
            member_account: &aid("kid-a"),
            month: month(2026, 11),
            amount: usd,
            now: t0(),
        })
        .unwrap_err();
    assert!(matches!(err, LedgerError::Invalid(_)), "{err}");
    // 账户必须属于这个主体
    assert!(matches!(
        issue(&store, "p-b", "kid-a", 2026, 11, 5, T0).unwrap_err(),
        LedgerError::Invalid(_)
    ));
    // 不是成员账户（老师是担保人账户）
    assert!(matches!(
        issue(&store, "p-t", "teacher", 2026, 11, 5, T0).unwrap_err(),
        LedgerError::Invalid(_)
    ));
    // 上面没有补贴池（emp 在 paid 之下）
    assert!(matches!(
        issue(&store, "p-e", "emp", 2026, 11, 5, T0).unwrap_err(),
        LedgerError::NoSubsidyPool(_)
    ));
    // 被撤销的成员关系、主体、被冻结的账户都发不出
    store.revoke_membership(&mid("m-b"), t0()).unwrap();
    assert!(matches!(
        issue(&store, "p-b", "kid-b", 2026, 11, 5, T0).unwrap_err(),
        LedgerError::NotActive { .. }
    ));
    store.revoke_principal(&pid("p-c"), t0()).unwrap();
    assert!(matches!(
        issue(&store, "p-c", "kid-c", 2026, 11, 5, T0).unwrap_err(),
        LedgerError::NotActive { .. }
    ));
    store
        .set_account_status(&aid("school"), AccountStatus::Frozen)
        .unwrap();
    assert!(matches!(
        issue(&store, "p-a", "kid-a", 2026, 11, 5, T0).unwrap_err(),
        LedgerError::NotActive { .. }
    ));
    // 全部失败：库里什么都没有
    assert_eq!(
        scalar(
            &db,
            "SELECT COUNT(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        0
    );
    assert_eq!(scalar(&db, "SELECT COUNT(*) FROM ledger_subsidy_grants"), 0);
}

#[test]
fn the_free_tier_window_follows_the_ledger_time_zone() {
    // 泰国的 11 月从 2026-10-31T17:00:00Z 开始
    let db = TempDb::new("free-zone");
    let store = subsidy_world(&db, 1_000_000);
    store
        .set_ledger_config(&LedgerConfig {
            billing_timezone: "Asia/Bangkok".into(),
            reservation_ttl_ms: 1_800_000,
        })
        .unwrap();
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(1_000_000), t0())
        .unwrap();
    let granted = issue(&store, "p-a", "kid-a", 2026, 11, 1_000, T0).unwrap();
    assert_eq!(
        granted.allocation().ceiling.valid_from().millis(),
        1_793_491_200_000 - 7 * HOUR
    );
    // 窗口起点前一毫秒还不能花，起点那一刻可以
    let start = granted.allocation().ceiling.valid_from().millis();
    assert_eq!(
        store
            .account_view(&aid("kid-a"), ts(start - 1))
            .unwrap()
            .ceiling_remaining,
        Some(thb(0)),
        "曾经有过额度，现在不生效 = 0"
    );
    assert_eq!(
        store
            .account_view(&aid("kid-a"), ts(start))
            .unwrap()
            .ceiling_remaining,
        Some(thb(1_000))
    );
}

#[test]
fn funding_is_idempotent_by_key_and_conflicts_are_errors() {
    let db = TempDb::new("fund");
    let store = world(&db);
    assert_eq!(
        store
            .fund_account(&aid("emp"), &thb(100), &key("k-1"), t0())
            .unwrap(),
        FundOutcome::Funded
    );
    assert_eq!(
        store
            .fund_account(&aid("emp"), &thb(100), &key("k-1"), ts(T0 + 1))
            .unwrap(),
        FundOutcome::Replayed
    );
    assert_eq!(balance(&store, "emp", T0), 100, "重放没有再记一次");
    assert!(matches!(
        store
            .fund_account(&aid("emp"), &thb(101), &key("k-1"), t0())
            .unwrap_err(),
        LedgerError::GrantConflict(_)
    ));
    assert!(matches!(
        store
            .fund_account(&aid("paid"), &thb(100), &key("k-1"), t0())
            .unwrap_err(),
        LedgerError::GrantConflict(_)
    ));
    // 补贴池和平台根不能充值；金额必须为正
    for bad in ["pool", "platform"] {
        assert!(matches!(
            store
                .fund_account(&aid(bad), &thb(1), &key("k-bad"), t0())
                .unwrap_err(),
            LedgerError::Invalid(_)
        ));
    }
    assert!(
        store
            .fund_account(&aid("emp"), &thb(0), &key("k-zero"), t0())
            .is_err()
    );
}

#[test]
fn ledger_configuration_is_validated() {
    let db = TempDb::new("config");
    let store = world(&db);
    let default = store.ledger_config().unwrap();
    assert_eq!(
        (
            default.billing_timezone.as_str(),
            default.reservation_ttl_ms
        ),
        ("UTC", 1_800_000)
    );
    assert!(
        store
            .set_ledger_config(&LedgerConfig {
                billing_timezone: "Not/AZone".into(),
                reservation_ttl_ms: 1
            })
            .is_err()
    );
    assert!(
        store
            .set_ledger_config(&LedgerConfig {
                billing_timezone: "UTC".into(),
                reservation_ttl_ms: 0
            })
            .is_err()
    );
    assert_eq!(
        store.ledger_config().unwrap(),
        default,
        "拒绝的配置没有生效"
    );
}

// ------------------------------------------------------------------------------------------
// 附加成本（给 S6：安全检查的成本计入准入和结算）
// ------------------------------------------------------------------------------------------

#[test]
fn an_extra_cost_is_reserved_along_every_ancestor_and_settled_like_the_main_reservation() {
    let db = TempDb::new("component");
    let store = paid_chain(&db, 1_000);
    let request = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210
    store.admit_attempt(&request.input()).unwrap();
    assert_eq!(balance(&store, "emp", T0), 790);
    // 安全检查的预估成本 40：沿所有祖先占住
    let decision = store
        .reserve_component(&request.attempt, "safety_input", &price(40), t0())
        .unwrap();
    assert_eq!(
        decision,
        ComponentDecision::Reserved {
            hold_id: "r-1#0~safety_input".into()
        }
    );
    assert_eq!(balance(&store, "emp", T0), 750);
    assert_eq!(balance(&store, "paid", T0), 1_000_000 - 250);
    // 按实际成本结算 25：已花 25，剩下的释放
    let outcome = store
        .settle_component(&request.attempt, "safety_input", &price(25), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.result, SettleResult::Settled);
    assert_eq!(balance(&store, "emp", T0 + 1_000), 1_000 - 210 - 25);
    // 幂等
    assert_eq!(
        store
            .settle_component(&request.attempt, "safety_input", &price(25), ts(T0 + 1_001))
            .unwrap()
            .result,
        SettleResult::AlreadySettled
    );
    // 主要预留不受影响，照常结算
    store
        .settle_attempt(&request.attempt, &price(100), ts(T0 + 2_000))
        .unwrap();
    assert_eq!(balance(&store, "emp", T0 + 2_000), 1_000 - 100 - 25);
    assert_clean(&store);
}

#[test]
fn an_extra_cost_is_checked_like_any_admission_and_a_rejection_holds_nothing() {
    let db = TempDb::new("component-reject");
    let store = paid_chain(&db, 230);
    let request = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210，剩 20
    store.admit_attempt(&request.input()).unwrap();
    // 安全检查要 40 > 20：拒绝，原因码和准入一样
    let decision = store
        .reserve_component(&request.attempt, "safety_input", &price(40), t0())
        .unwrap();
    assert_eq!(
        decision,
        ComponentDecision::Rejected(ReasonCode::BudgetExhausted)
    );
    assert!(
        store
            .hold(&request.attempt, "safety_input")
            .unwrap()
            .is_none()
    );
    assert_eq!(balance(&store, "emp", T0), 20);
    // 速率闸门同样适用
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                minute_cap: Some(thb(215)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let decision = store
        .reserve_component(&request.attempt, "safety_output", &price(10), t0())
        .unwrap();
    assert_eq!(
        decision,
        ComponentDecision::Rejected(ReasonCode::RateLimited)
    );
    // 输入不合法：component 名、没有主要预留、重复预留、单位不对
    assert!(
        store
            .reserve_component(&request.attempt, "model", &price(1), t0())
            .is_err()
    );
    assert!(
        store
            .reserve_component(&request.attempt, "Bad Name", &price(1), t0())
            .is_err()
    );
    let ghost = Req::new(&store, "ghost", 0, "cred-e", 10);
    assert!(matches!(
        store
            .reserve_component(&ghost.attempt, "safety_input", &price(1), t0())
            .unwrap_err(),
        LedgerError::HoldNotFound(_)
    ));
    store
        .reserve_component(&request.attempt, "safety_input", &price(5), t0())
        .unwrap();
    assert!(matches!(
        store
            .reserve_component(&request.attempt, "safety_input", &price(5), t0())
            .unwrap_err(),
        LedgerError::HoldExists(_)
    ));
    let state = store
        .hold(&request.attempt, "safety_input")
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(state, HoldState::Open);
    // 释放
    store
        .release_component(&request.attempt, "safety_input", ts(T0 + 1))
        .unwrap();
    assert_eq!(balance(&store, "emp", T0 + 1), 20);
    assert_clean(&store);
}
