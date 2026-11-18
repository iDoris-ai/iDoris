//! S3 准入账本：**分摊守恒**（PR #600 Codex 评审 #3 / #4 / #5，PR-Daemon S1 F1 / F2）。
//!
//! 一次结算的实际消费 `charged` 必须恰好分给三方，不多不少：
//!
//! ```text
//! charged = 用户承担 (user_borne) + 赞助方承担 (sponsor_covered) + 平台承担 (platform_loss + grace_absorbed)
//! ```
//!
//! 这条等式在每个场景里都用**账本分录**（不经过被测的结算返回值）再核对一遍：
//!
//! * 记到付款账户链上的已花 = `charged - grace_absorbed`（宽限里平台承担的那部分不进用户和担保人的账）；
//! * 担保人承担 + 平台损失 = overage（预留之外的部分；迟到的结算是整笔扣掉宽限承担之后的部分）；
//! * 结果不依赖扫描顺序、结算顺序（`ledger_conservation_*`）。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::credential::CredentialStatus;
use idoris_compute_contracts::ids::{AttemptId, AttemptIndex, IdempotencyKey, RequestId};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{AlertKind, CreditLineSetting, HoldState, SettleOutcome};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn attempt(request: &str, index: u8) -> AttemptId {
    AttemptId::new(RequestId::new(request).unwrap(), AttemptIndex(index))
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

fn ttl(store: &AccountStore) -> i64 {
    store.ledger_config().unwrap().reservation_ttl_ms
}

/// 先不允许宽限试一遍，不行再允许宽限：和云执行器的两遍一样。
fn admit_with_ladder(
    store: &AccountStore,
    request: &str,
    credential: &str,
    max_tokens: u64,
    now: i64,
) -> AdmissionDecision {
    let first = Req::new(store, request, 0, credential, max_tokens).at(now);
    let decision = store.admit_attempt(&first.input()).unwrap();
    let exhausted = matches!(&decision, AdmissionDecision::Reject { reason } if reason.code() == ReasonCode::BudgetExhausted);
    if decision.is_admitted() || !exhausted {
        return decision;
    }
    let second = Req::new(store, request, 0, credential, max_tokens)
        .at(now)
        .grace();
    store.admit_attempt(&second.input()).unwrap()
}

/// 一个预留在**分录**里的分摊（原生 SQL，不经过被测的结算返回值）。
#[derive(Debug, PartialEq, Eq)]
struct Split {
    /// 记在付款账户上的已花。
    spent_on_payer: i64,
    sponsor: i64,
    loss: i64,
    absorbed: i64,
}

fn split_of(db: &TempDb, hold: &str, payer: &str) -> Split {
    let liability = |kind: &str| {
        scalar(
            db,
            &format!(
                "SELECT COALESCE(SUM(liability_micros), 0) FROM ledger_entries
                  WHERE hold_id = '{hold}' AND kind = '{kind}'"
            ),
        )
    };
    Split {
        spent_on_payer: scalar(
            db,
            &format!(
                "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries
                  WHERE hold_id = '{hold}' AND account_id = '{payer}'"
            ),
        ),
        sponsor: liability("sponsor_covered"),
        loss: liability("platform_loss"),
        absorbed: liability("grace_absorbed"),
    }
}

/// 守恒：分录里的分摊加起来恰好是实际消费，并且和结算返回值一致。
fn assert_conserved(db: &TempDb, hold: &str, payer: &str, charged: i64, outcome: &SettleOutcome) {
    let split = split_of(db, hold, payer);
    assert_eq!(
        split.spent_on_payer + split.absorbed,
        charged,
        "付款账户链上的已花 + 平台承担的宽限 = 实际消费: {split:?}"
    );
    assert_eq!(
        (split.sponsor, split.loss, split.absorbed),
        (
            outcome.sponsor_covered.micros,
            outcome.platform_loss.micros,
            outcome.grace_absorbed.micros
        ),
        "分录和结算返回值一致"
    );
    assert_eq!(
        outcome.user_borne.micros
            + outcome.sponsor_covered.micros
            + outcome.platform_loss.micros
            + outcome.grace_absorbed.micros,
        charged,
        "实际消费 = 用户承担 + 赞助方 + 平台承担: {outcome:?}"
    );
    assert!(outcome.user_borne.micros >= 0, "{outcome:?}");
}

// ------------------------------------------------------------------------------------------
// #3 宽限中平台承担的部分不能扣用户余额
// ------------------------------------------------------------------------------------------

#[test]
fn a_grace_round_the_platform_absorbs_never_reaches_the_users_balance() {
    // 余额 20、宽限预留 210、实际消费 100：自己的 20 付完，其余 80 平台承担。用户余额是 0，不是 -80
    let db = TempDb::new("conserve-grace-balance");
    let store = paid_chain(&db, 20);
    let decision = admit_with_ladder(&store, "r-1", "cred-e", 100, T0);
    assert!(
        matches!(decision, AdmissionDecision::Grace { .. }),
        "{decision:?}"
    );
    let outcome = store
        .settle_attempt(&attempt("r-1", 0), &price(100), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.grace_absorbed, thb(80));
    assert_eq!(outcome.platform_loss, thb(0));
    assert_eq!(outcome.user_borne, thb(20));
    assert_eq!(
        balance(&store, "emp", T0 + 1_000),
        0,
        "平台承担的 80 不能让用户余额变负"
    );
    // 担保人（组织）也只出了用户自己的 20，平台承担的部分不进它的账
    assert_eq!(balance(&store, "paid", T0 + 1_000), 1_000_000 - 20);
    assert_conserved(&db, "r-1#0", "emp", 100, &outcome);
    assert_clean(&store);
    // 之后的充值全部是用户自己的钱：不会先去填一笔已经归给平台的费用
    store
        .fund_account(&aid("emp"), &thb(50), &key("f-emp-2"), ts(T0 + 2_000))
        .unwrap();
    assert_eq!(balance(&store, "emp", T0 + 2_000), 50);
}

#[test]
fn a_grace_round_inside_the_credit_floor_is_still_the_users_own_debt() {
    // 余额 5、授信 20：账户自己的 5 加授信 20 是下限 25，还放不下最低有用的预留 30，所以走宽限（预留 210）。
    // 实际消费 100：下限之内的 25 是用户自己的（5 的余额加 20 的有担保的负值），超出下限的 75 才由平台承担
    let db = TempDb::new("conserve-grace-credit");
    let store = paid_chain(&db, 5);
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(20),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    let decision = admit_with_ladder(&store, "r-1", "cred-e", 100, T0);
    assert!(
        matches!(decision, AdmissionDecision::Grace { .. }),
        "{decision:?}"
    );
    let outcome = store
        .settle_attempt(&attempt("r-1", 0), &price(100), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.grace_absorbed, thb(75));
    assert_eq!(outcome.user_borne, thb(25));
    assert_eq!(
        balance(&store, "emp", T0 + 1_000),
        5 - 25,
        "用户只背自己那一份：-20 正好是授信下限"
    );
    assert_conserved(&db, "r-1#0", "emp", 100, &outcome);
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// #4 迟到的宽限结算不重复计算平台责任
// ------------------------------------------------------------------------------------------

#[test]
fn a_late_settlement_of_a_grace_hold_counts_the_platform_share_once_and_freezes_nothing() {
    // 余额 20、宽限预留 210，TTL 之后才结算 100。旧代码：担保人 20 + 平台损失 80 + 宽限承担 80 = 180（同一笔缺口算了两遍），
    // 还因为「平台损失」冻结了凭证。正确：宽限承担 80（平台批准过的），剩下 20 是用户自己的钱，没有损失
    let db = TempDb::new("conserve-late-grace");
    let store = paid_chain(&db, 20);
    let decision = admit_with_ladder(&store, "r-1", "cred-e", 100, T0);
    assert!(matches!(decision, AdmissionDecision::Grace { .. }));
    let late_at = T0 + ttl(&store) + 1_000;
    let outcome = store
        .settle_attempt(&attempt("r-1", 0), &price(100), ts(late_at))
        .unwrap();
    assert!(outcome.late);
    assert_eq!(outcome.grace_absorbed, thb(80), "{outcome:?}");
    assert_eq!(outcome.platform_loss, thb(0), "{outcome:?}");
    assert_eq!(outcome.sponsor_covered, thb(20), "{outcome:?}");
    assert_eq!(
        outcome.overage,
        thb(20),
        "扣掉已获批的宽限承担额之后才是迟到的 overage"
    );
    assert!(!outcome.credential_frozen, "没有平台损失，不冻结凭证");
    assert_eq!(
        store
            .get_credential(&cid("cred-e"))
            .unwrap()
            .unwrap()
            .status(),
        CredentialStatus::Active
    );
    assert!(
        store
            .alerts(Some(AlertKind::PlatformLoss))
            .unwrap()
            .is_empty()
    );
    // 责任总额恰好是实际消费，不是它的 1.8 倍
    let split = split_of(&db, "r-1#0", "emp");
    assert_eq!(split.sponsor + split.loss + split.absorbed, 100);
    assert_conserved(&db, "r-1#0", "emp", 100, &outcome);
    assert_eq!(balance(&store, "emp", late_at), 0);
    assert_clean(&store);
}

#[test]
fn a_late_settlement_of_a_grace_hold_that_overshoots_the_reservation_is_split_without_overlap() {
    // 迟到、宽限、而且实际消费超出预留：宽限承担 min(A, R) - 下限；剩下的（含超出预留的部分）才按迟到 overage 分
    let db = TempDb::new("conserve-late-grace-over");
    let store = paid_chain(&db, 20);
    admit_with_ladder(&store, "r-1", "cred-e", 100, T0); // 预留 210，下限 20
    let late_at = T0 + ttl(&store) + 1_000;
    let outcome = store
        .settle_attempt(&attempt("r-1", 0), &price(300), ts(late_at))
        .unwrap();
    // 宽限承担 210 - 20 = 190；其余 110：账户自己有 20 余量（担保人承担 20），再多的 90 是平台损失
    assert_eq!(outcome.grace_absorbed, thb(190), "{outcome:?}");
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (20, 90),
        "{outcome:?}"
    );
    assert!(outcome.credential_frozen);
    assert_conserved(&db, "r-1#0", "emp", 300, &outcome);
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// #5 结果不依赖扫描顺序
// ------------------------------------------------------------------------------------------

/// 余额 200，A 和 B 各预留 100（`max_tokens = 45`），两个都超过了 TTL 但没有人扫过。
fn two_expired_holds(tag: &str) -> (TempDb, AccountStore, i64) {
    let db = TempDb::new(tag);
    let store = paid_chain(&db, 200);
    for request in ["r-a", "r-b"] {
        let req = Req::new(&store, request, 0, "cred-e", 45);
        assert!(matches!(
            store.admit_attempt(&req.input()).unwrap(),
            AdmissionDecision::Allow { .. }
        ));
    }
    assert_eq!(balance(&store, "emp", T0), 0);
    let late_at = T0 + ttl(&store) + 1_000;
    (db, store, late_at)
}

#[test]
fn other_expired_holds_do_not_turn_a_late_settlement_into_a_false_platform_loss() {
    let (db, store, late_at) = two_expired_holds("conserve-expired-others");
    // 没人扫过 B：旧代码只过期 A 自己，B 仍占着 100，于是余量只有 100，150 里的 50 被记成平台损失并冻结凭证
    let outcome = store
        .settle_attempt(&attempt("r-a", 0), &price(150), ts(late_at))
        .unwrap();
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (150, 0),
        "{outcome:?}"
    );
    assert!(!outcome.credential_frozen);
    assert_conserved(&db, "r-a#0", "emp", 150, &outcome);
    // B 也被同一个事务过期了（额度放回去），不需要谁再扫一遍
    assert_eq!(
        store
            .hold(&attempt("r-b", 0), "model")
            .unwrap()
            .unwrap()
            .state,
        HoldState::Expired
    );
    assert_clean(&store);
}

#[test]
fn a_late_settlement_gives_the_same_answer_whether_or_not_someone_swept_first_and_in_either_order()
{
    // 四种做法：A 先结算 / 先扫再结算 A / B 先结算 / 先扫再结算 B。同一笔 150 的结算，结果必须一样
    let settle = |first: &str, sweep_before: bool| {
        let (db, store, late_at) = two_expired_holds("conserve-order");
        if sweep_before {
            store.sweep_expired_holds(ts(late_at)).unwrap();
        }
        let outcome = store
            .settle_attempt(&attempt(first, 0), &price(150), ts(late_at))
            .unwrap();
        assert_conserved(&db, &format!("{first}#0"), "emp", 150, &outcome);
        assert_clean(&store);
        (
            outcome.sponsor_covered.micros,
            outcome.platform_loss.micros,
            outcome.credential_frozen,
        )
    };
    let reference = settle("r-a", true);
    assert_eq!(reference, (150, 0, false));
    for (first, sweep) in [("r-a", false), ("r-b", false), ("r-b", true)] {
        assert_eq!(settle(first, sweep), reference, "{first} sweep={sweep}");
    }
}

#[test]
fn settling_both_expired_holds_loses_the_same_total_in_either_order() {
    // 余额 200，A、B 各预留 100 都过期，实际 150 和 100（共 250，超出余额 50）：不管谁先，总损失都是 50
    let run = |order: [(&str, i64); 2]| {
        let (db, store, late_at) = two_expired_holds("conserve-both");
        let mut loss = 0;
        let mut sponsor = 0;
        for (request, charged) in order {
            let outcome = store
                .settle_attempt(&attempt(request, 0), &price(charged), ts(late_at))
                .unwrap();
            assert_conserved(&db, &format!("{request}#0"), "emp", charged, &outcome);
            loss += outcome.platform_loss.micros;
            sponsor += outcome.sponsor_covered.micros;
        }
        assert_clean(&store);
        (sponsor, loss)
    };
    let one = run([("r-a", 150), ("r-b", 100)]);
    let other = run([("r-b", 100), ("r-a", 150)]);
    assert_eq!(one, (200, 50));
    assert_eq!(one, other, "总的担保人承担和平台损失不依赖结算顺序");
}

// ------------------------------------------------------------------------------------------
// 对账：同一块缺口被算两遍、责任记错了归属，都在对账里现形（PR-Daemon S1 F2：旧对账从不看责任分录）
// ------------------------------------------------------------------------------------------

#[test]
fn reconcile_flags_a_double_counted_or_misattributed_liability() {
    let forge = |db: &TempDb, hold: &str, kind: &str, account: &str, micros: i64, key: &str| {
        let conn = rusqlite::Connection::open(db.path()).unwrap();
        conn.execute(
            "INSERT INTO ledger_entries (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros,
                                         liability_micros, window_at_ms, at_ms, idempotency_key)
             VALUES (?1, ?2, ?3, 'THB', 0, 0, 0, ?4, 0, 0, ?5)",
            rusqlite::params![account, hold, kind, micros, key],
        )
        .unwrap();
    };
    let mismatches = |store: &AccountStore| -> Vec<String> {
        store
            .reconcile_with_attempts()
            .unwrap()
            .mismatches
            .into_iter()
            .map(|m| format!("{}: {}", m.subject, m.detail))
            .collect()
    };

    // 干净的宽限结算：对账通过
    let db = TempDb::new("conserve-reconcile-grace");
    let store = paid_chain(&db, 20);
    admit_with_ladder(&store, "r-1", "cred-e", 100, T0);
    store
        .settle_attempt(&attempt("r-1", 0), &price(100), ts(T0 + 1_000))
        .unwrap();
    assert!(mismatches(&store).is_empty());
    // 旧代码的错账：同一块缺口再记一次平台损失，责任合计 80 + 80 = 160 > 实际消费 100
    forge(
        &db,
        "r-1#0",
        "platform_loss",
        "platform-loss",
        80,
        "forged-loss",
    );
    let found = mismatches(&store);
    assert!(
        found
            .iter()
            .any(|line| line.starts_with("r-1#0") && line.contains("exceed the charge")),
        "{found:?}"
    );
    assert!(
        found.iter().any(|line| line.contains("overage")),
        "担保人 + 平台损失必须等于 overage（这笔没有 overage）: {found:?}"
    );

    // 没有宽限的预留不能有宽限承担
    let db = TempDb::new("conserve-reconcile-plain");
    let store = paid_chain(&db, 1_000);
    let req = Req::new(&store, "r-2", 0, "cred-e", 100);
    store.admit_attempt(&req.input()).unwrap();
    store
        .settle_attempt(&req.attempt, &price(80), ts(T0 + 1_000))
        .unwrap();
    assert!(mismatches(&store).is_empty());
    forge(
        &db,
        "r-2#0",
        "grace_absorbed",
        "platform-loss",
        10,
        "forged-grace",
    );
    let found = mismatches(&store);
    assert!(
        found
            .iter()
            .any(|line| line.starts_with("r-2#0") && line.contains("the platform absorbed 10")),
        "{found:?}"
    );

    // 还没结算的预留（在途）不能有任何责任分录
    let db = TempDb::new("conserve-reconcile-open");
    let store = paid_chain(&db, 1_000);
    let req = Req::new(&store, "r-3", 0, "cred-e", 100);
    store.admit_attempt(&req.input()).unwrap();
    forge(
        &db,
        "r-3#0",
        "platform_loss",
        "platform-loss",
        5,
        "forged-open",
    );
    let found = mismatches(&store);
    assert!(
        found
            .iter()
            .any(|line| line.starts_with("r-3#0") && line.contains("only a settled hold may")),
        "{found:?}"
    );
}

// ------------------------------------------------------------------------------------------
// 守恒：把「余额、授信、预留、实际消费、是否迟到」排成一张表，每一格都核对等式
// ------------------------------------------------------------------------------------------

#[test]
fn the_charge_is_split_exactly_between_user_sponsor_and_platform_in_every_combination() {
    let mut cells = 0;
    for funds in [5_i64, 20, 150, 400] {
        for credit in [0_i64, 20, 60] {
            for charged in [0_i64, 15, 20, 100, 210, 300] {
                for late in [false, true] {
                    let tag = format!("conserve-table-{funds}-{credit}-{charged}-{late}");
                    let db = TempDb::new(&tag);
                    let store = paid_chain(&db, funds);
                    if credit > 0 {
                        store
                            .set_credit_line(
                                &aid("emp"),
                                &CreditLineSetting {
                                    limit: thb(credit),
                                    valid_until: None,
                                },
                                t0(),
                            )
                            .unwrap();
                    }
                    let decision = admit_with_ladder(&store, "r-1", "cred-e", 100, T0);
                    if !decision.is_admitted() {
                        continue;
                    }
                    let at = if late {
                        T0 + ttl(&store) + 1_000
                    } else {
                        T0 + 1_000
                    };
                    let outcome = store
                        .settle_attempt(&attempt("r-1", 0), &price(charged), ts(at))
                        .unwrap();
                    if charged > 0 {
                        assert_conserved(&db, "r-1#0", "emp", charged, &outcome);
                    }
                    // 账户永远不会因为平台承担的部分而低于「自己的钱 + 授信」以下太多：
                    // 用户余额 = 资金 - 用户和担保人承担的部分（overage 里平台损失那一份照旧记账，见交付说明）
                    let payer_spent = split_of(&db, "r-1#0", "emp").spent_on_payer;
                    assert_eq!(
                        balance(&store, "emp", at),
                        funds - payer_spent,
                        "funds={funds} credit={credit} charged={charged} late={late}"
                    );
                    assert!(
                        payer_spent <= charged,
                        "平台承担的部分不会让账户多扣: funds={funds} credit={credit} charged={charged} late={late}"
                    );
                    assert_clean(&store);
                    cells += 1;
                }
            }
        }
    }
    assert!(cells >= 60, "表里只跑了 {cells} 格");
}
