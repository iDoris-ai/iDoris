//! S3 准入账本：五项检查（余额或授权额度、授信下限、速率闸门、在途总额上限、补贴池总上限）各自单独触发拒绝，
//! 沿祖先触发（叶子够、祖先不够），压低 `max_tokens`，凭证与账户链的核对，TTL 过期，候选切换释放。
//!
//! 每个用例只让**一项**不过（其余都给足），所以拒绝的原因就是被测的那一项。数字约定见 `support/ledger_fixture.rs`：
//! `10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::credential::CredentialStatus;
use idoris_compute_contracts::ids::{AttemptId, AttemptIndex, IdempotencyKey, RequestId};
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{
    AccountLimits, AlertKind, CreditLineSetting, HoldState, LedgerError, SettleResult,
};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn admit(store: &AccountStore, req: &Req) -> AdmissionDecision {
    store.admit_attempt(&req.input()).unwrap()
}

fn denied(decision: &AdmissionDecision) -> Option<ReasonCode> {
    match decision {
        AdmissionDecision::Reject { reason } => Some(reason.code()),
        _ => None,
    }
}

/// 给 `emp` 和它的组织各充足够的钱，让「钱」这一项通过；返回 store。
fn funded_paid_chain(db: &TempDb) -> AccountStore {
    let store = world(db);
    store
        .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    store
}

/// 给 `kid-a` 沿整条链（pool -> school -> teacher -> kid-a）各铺 `micros` 的额度。
fn allowance_chain_a(store: &AccountStore, micros: i64) {
    allocate_chain(store, "a", &["pool", "school", "teacher", "kid-a"], micros);
}

// ------------------------------------------------------------------------------------------
// 失败即关闭：什么都没配置的账户不是「不设限」
// ------------------------------------------------------------------------------------------

#[test]
fn an_account_with_nothing_configured_is_denied_not_unbounded() {
    let db = TempDb::new("default-deny");
    let store = world(&db);
    let view = store.account_view(&aid("kid-a"), t0()).unwrap();
    // S0 把 None 解释成「没配置、不设限」；对会花钱的账户这是最危险的默认值，所以账本填 Some(0)
    assert_eq!(view.ceiling_remaining, Some(thb(0)));
    assert_eq!(view.balance, None);
    let req = Req::new(&store, "r-1", 0, "cred-a", 100);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    // 平台根和补贴池本身没有自己的额度（它们是源头），其余账户都是 Some
    assert_eq!(
        store
            .account_view(&aid("platform"), t0())
            .unwrap()
            .ceiling_remaining,
        None
    );
    assert_eq!(
        store
            .account_view(&aid("school"), t0())
            .unwrap()
            .ceiling_remaining,
        Some(thb(0))
    );
    // 拒绝没有占任何东西
    assert!(store.hold(&req.attempt, "model").unwrap().is_none());
    assert_eq!(scalar(&db, "SELECT count(*) FROM ledger_entries"), 0);
}

#[test]
fn an_expired_allowance_is_zero_not_unbounded() {
    // 曾经有过额度、现在一个都不生效（上个月的免费额度过期了）：Some(0)，不是 None
    let db = TempDb::new("expired-allowance");
    let store = world(&db);
    allowance_chain_a(&store, 100_000);
    let req = Req::new(&store, "r-1", 0, "cred-a", 100).at(T0 + 61 * DAY);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    assert_eq!(
        store
            .account_view(&aid("kid-a"), ts(T0 + 61 * DAY))
            .unwrap()
            .ceiling_remaining,
        Some(thb(0))
    );
}

#[test]
fn an_allowance_is_active_from_its_start_up_to_but_not_including_its_end() {
    // `allocate_chain` 铺的窗口是 [T0 - 1 天, T0 + 60 天)：起点含，终点不含，各差 1 毫秒都要分得出来
    let db = TempDb::new("allowance-window-edges");
    let store = world(&db);
    allowance_chain_a(&store, 100_000);
    // 链经过补贴池：起点在 2026-10，终点附近在 2026-12，两个账期都要有池上限，否则先被池拦下
    for (y, m) in [(2026, 10), (2026, 12)] {
        store
            .set_pool_cap(&aid("pool"), month(y, m), &thb(10_000_000), t0())
            .unwrap();
    }
    let start = T0 - DAY;
    let end = T0 + 60 * DAY;
    for (request, now, admitted) in [
        ("r-before", start - 1, false),
        ("r-start", start, true),
        ("r-last", end - 1, true),
        ("r-end", end, false),
    ] {
        let req = Req::new(&store, request, 0, "cred-a", 100).at(now);
        let decision = admit(&store, &req);
        assert_eq!(decision.is_admitted(), admitted, "{request}: {decision:?}");
        if !admitted {
            assert_eq!(
                denied(&decision),
                Some(ReasonCode::BudgetExhausted),
                "{request}"
            );
        }
    }
}

// ------------------------------------------------------------------------------------------
// 放行：沿链每个账户各一条分录；可用额是分录的投影
// ------------------------------------------------------------------------------------------

#[test]
fn an_admitted_reservation_posts_one_entry_per_account_along_the_whole_chain() {
    let db = TempDb::new("allow-posts-chain");
    let store = funded_paid_chain(&db);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    let decision = admit(&store, &req);
    let admitted = decision.admitted().expect("admitted");
    assert!(matches!(decision, AdmissionDecision::Allow { .. }));
    assert_eq!(admitted.effective_max_tokens, 100);
    assert_eq!(admitted.reservation.price.micros(), reserve_of(100));
    // 上游成本的预留按同一个有效 max_tokens 算：10 × 0.3 + 100 × 0.5 = 53 微美元
    assert_eq!(admitted.reservation.cost.micros(), 53);

    let hold = store.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(hold.state, HoldState::Open);
    assert_eq!(hold.reserved, thb(reserve_of(100)));
    let entries = store.entries_of_hold(&req.attempt, "model").unwrap();
    let accounts: Vec<&str> = entries.iter().map(|e| e.account.as_str()).collect();
    assert_eq!(accounts, ["emp", "paid", "platform"], "沿所有祖先，包括根");
    assert!(
        entries
            .iter()
            .all(|e| e.kind == "reserve" && e.held == reserve_of(100) && e.spent == 0)
    );

    // 投影：余额 = 资金 - 在途
    let emp = store.account_view(&aid("emp"), t0()).unwrap();
    assert_eq!(emp.balance, Some(thb(1_000_000 - reserve_of(100))));
    let paid = store.account_view(&aid("paid"), t0()).unwrap();
    assert_eq!(paid.balance, Some(thb(1_000_000 - reserve_of(100))));
    assert_clean(&store);
}

#[test]
fn the_same_attempt_cannot_be_admitted_twice() {
    let db = TempDb::new("admit-twice");
    let store = funded_paid_chain(&db);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    let err = store.admit_attempt(&req.input()).unwrap_err();
    assert!(matches!(err, LedgerError::HoldExists(_)), "{err}");
    // 第二次什么都没多占
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(1_000_000 - reserve_of(100)))
    );
}

// ------------------------------------------------------------------------------------------
// 五项检查各自单独触发拒绝（每次只让一项不过）
// ------------------------------------------------------------------------------------------

/// 最低有用的预留：`max_tokens = 10`（`ReservePolicy` 的 `min_useful_max_tokens`）是 `10 + 20 = 30`。
/// 钱不够时账本**先**压低 `max_tokens`，所以「钱这一项单独触发拒绝」要用一个压无可压的请求（`max_tokens = 10`），
/// 否则缺的那一点钱只会让它降级，不会被拒绝（降级见 `when_funds_are_short_...`）。
const MIN_USEFUL: i64 = 30;

#[test]
fn check_1_funds_balance_alone_rejects_with_budget_exhausted() {
    let db = TempDb::new("check-funds-balance");
    let store = world(&db);
    // 余额：emp 只有 29，差 1；组织资金充足
    store
        .fund_account(&aid("emp"), &thb(MIN_USEFUL - 1), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 10);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    assert_eq!(
        scalar(&db, "SELECT count(*) FROM ledger_holds"),
        0,
        "拒绝不占额度"
    );
    // 刚好够就放行（边界），是完整的预留，不是降级
    store
        .fund_account(&aid("emp"), &thb(1), &key("f-emp-2"), t0())
        .unwrap();
    let req = Req::new(&store, "r-2", 0, "cred-e", 10);
    assert!(matches!(
        admit(&store, &req),
        AdmissionDecision::Allow { .. }
    ));
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(0))
    );
}

/// 只有 `kid-a` 自己的额度是 `kid_a` 微单位；它所有祖先的额度和补贴池 11 月的上限都充足。
fn store_with_kid_a_allowance(db: &TempDb, kid_a: i64) -> AccountStore {
    let store = world(db);
    allocate_chain(&store, "x", &["pool", "school", "teacher"], 1_000_000);
    allocate(&store, "kid-a-own", "teacher", "kid-a", kid_a);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(10_000_000), t0())
        .unwrap();
    store
}

#[test]
fn check_1_funds_authorized_ceiling_alone_rejects_with_budget_exhausted() {
    // 授权额度：kid-a 的额度 29，差 1；其余祖先额度充足
    let db = TempDb::new("check-funds-ceiling-29");
    let store = store_with_kid_a_allowance(&db, MIN_USEFUL - 1);
    let req = Req::new(&store, "r-1", 0, "cred-a", 10);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    assert_eq!(scalar(&db, "SELECT count(*) FROM ledger_holds"), 0);
    // 刚好够（边界）
    let db2 = TempDb::new("check-funds-ceiling-30");
    let store2 = store_with_kid_a_allowance(&db2, MIN_USEFUL);
    let req = Req::new(&store2, "r-1", 0, "cred-a", 10);
    assert!(matches!(
        admit(&store2, &req),
        AdmissionDecision::Allow { .. }
    ));
    assert_eq!(
        store2
            .account_view(&aid("kid-a"), t0())
            .unwrap()
            .ceiling_remaining,
        Some(thb(0))
    );
}

#[test]
fn check_2_credit_floor_alone_rejects_beyond_the_credit_and_allows_exactly_at_it() {
    let db = TempDb::new("check-credit");
    let store = world(&db);
    // emp 只有 10，最低有用的预留要 30，差 20；组织资金充足
    store
        .fund_account(&aid("emp"), &thb(10), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    // 授信 19：还差 1，拒绝（credit floor 这一项不过）
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(19),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 10);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    // 授信 20：刚好够，放行（有担保的负值），并留下告警（通知担保人）
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
    let req = Req::new(&store, "r-2", 0, "cred-e", 10);
    assert!(matches!(
        admit(&store, &req),
        AdmissionDecision::Allow { .. }
    ));
    let alerts = store.alerts(Some(AlertKind::CreditUsed)).unwrap();
    assert_eq!(alerts.len(), 1);
    assert_eq!(alerts[0].account, "emp");
    assert_eq!(
        alerts[0].guarantor.as_deref(),
        Some("paid"),
        "担保人就是父账户"
    );
    assert_eq!(alerts[0].amount, thb(20));
    // 账户余额现在是 -20：到了授信下限，再来一次就不行
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(-20))
    );
    let req = Req::new(&store, "r-3", 0, "cred-e", 10);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    assert_clean(&store);
}

#[test]
fn credit_lets_the_balance_go_negative_so_the_reservation_can_be_larger_than_the_funds() {
    // 授信不是只用来「救」最低预留：全额预留 210，余额 100，授信 110 → 全额放行，不降级
    let db = TempDb::new("credit-full");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(100), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(110),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    let decision = admit(&store, &Req::new(&store, "r-1", 0, "cred-e", 100));
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(-110))
    );
}

#[test]
fn a_disabled_or_expired_credit_line_is_zero_credit() {
    let db = TempDb::new("credit-switch");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(100), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    // 授信 1000 但一分钟后过期
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(1_000),
                valid_until: Some(ts(T0 + MINUTE)),
            },
            t0(),
        )
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 100).at(T0 + MINUTE - 1);
    assert!(
        matches!(admit(&store, &req), AdmissionDecision::Allow { .. }),
        "过期前一毫秒还有效"
    );
    let req = Req::new(&store, "r-2", 0, "cred-e", 10).at(T0 + MINUTE);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted),
        "到期那一毫秒已经失效（右端开）"
    );
    // 开关：关掉之后立即是 0
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(1_000_000),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    store.disable_credit_line(&aid("emp"), t0()).unwrap();
    let line = store.credit_line(&aid("emp")).unwrap().unwrap();
    assert!(!line.enabled && line.limit == thb(0));
    let req = Req::new(&store, "r-3", 0, "cred-e", 10).at(T0 + 1);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
}

#[test]
fn credit_lines_are_structurally_validated() {
    let db = TempDb::new("credit-validation");
    let store = world(&db);
    // 平台根没有担保人
    let err = store
        .set_credit_line(
            &aid("platform"),
            &CreditLineSetting {
                limit: thb(1),
                valid_until: None,
            },
            t0(),
        )
        .unwrap_err();
    assert!(matches!(err, LedgerError::Invalid(_)), "{err}");
    // 负数、别的单位、已经过期
    for bad in [
        CreditLineSetting {
            limit: thb(-1),
            valid_until: None,
        },
        CreditLineSetting {
            limit: idoris_compute_contracts::money::Money::new(
                idoris_compute_contracts::money::Unit::Usd,
                1,
            ),
            valid_until: None,
        },
        CreditLineSetting {
            limit: thb(1),
            valid_until: Some(t0()),
        },
    ] {
        assert!(
            store.set_credit_line(&aid("emp"), &bad, t0()).is_err(),
            "{bad:?}"
        );
    }
    // 不存在的账户
    assert!(
        store
            .set_credit_line(
                &aid("nobody"),
                &CreditLineSetting {
                    limit: thb(1),
                    valid_until: None
                },
                t0()
            )
            .is_err()
    );
    assert!(store.credit_line(&aid("emp")).unwrap().is_none());
}

#[test]
fn check_3_rate_gate_alone_rejects_per_minute_and_per_hour_with_a_sliding_window() {
    let db = TempDb::new("check-rate");
    let store = funded_paid_chain(&db);
    // 分钟上限 400：两次 210 = 420 超 20
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                minute_cap: Some(thb(400)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let r1 = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(matches!(
        admit(&store, &r1),
        AdmissionDecision::Allow { .. }
    ));
    let r2 = Req::new(&store, "r-2", 0, "cred-e", 100).at(T0 + 30_000);
    assert_eq!(denied(&admit(&store, &r2)), Some(ReasonCode::RateLimited));
    // 滑动窗口：第一次在 T0，窗口是 (now-60s, now]，T0+60s 时它刚好滑出去
    let r3 = Req::new(&store, "r-3", 0, "cred-e", 100).at(T0 + MINUTE - 1);
    assert_eq!(
        denied(&admit(&store, &r3)),
        Some(ReasonCode::RateLimited),
        "还在窗口里"
    );
    let r4 = Req::new(&store, "r-4", 0, "cred-e", 100).at(T0 + MINUTE);
    assert!(
        matches!(admit(&store, &r4), AdmissionDecision::Allow { .. }),
        "刚好滑出窗口"
    );

    // 小时上限同理（分钟不设限）。第一次请求结算掉（预留 TTL 只有 30 分钟），它按实际金额留在窗口里
    let db2 = TempDb::new("check-rate-hour");
    let store2 = funded_paid_chain(&db2);
    store2
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                hour_cap: Some(thb(400)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let h1 = Req::new(&store2, "h-1", 0, "cred-e", 100);
    assert!(matches!(
        admit(&store2, &h1),
        AdmissionDecision::Allow { .. }
    ));
    store2
        .settle_attempt(&h1.attempt, &price(reserve_of(100)), ts(T0 + 1_000))
        .unwrap();
    let h2 = Req::new(&store2, "h-2", 0, "cred-e", 100).at(T0 + 59 * MINUTE);
    assert_eq!(denied(&admit(&store2, &h2)), Some(ReasonCode::RateLimited));
    let h3 = Req::new(&store2, "h-3", 0, "cred-e", 100).at(T0 + HOUR);
    assert!(
        matches!(admit(&store2, &h3), AdmissionDecision::Allow { .. }),
        "刚好滑出小时窗口"
    );
}

#[test]
fn a_released_request_does_not_count_against_the_rate_gate() {
    // 速率闸门限制的是「准入的钱」：请求失败释放之后，它对窗口的净贡献是 0
    let db = TempDb::new("rate-release");
    let store = funded_paid_chain(&db);
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                minute_cap: Some(thb(270)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let r1 = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &r1);
    store
        .release_attempt(&r1.attempt, ts(T0 + 1_000))
        .unwrap()
        .unwrap();
    let r2 = Req::new(&store, "r-2", 0, "cred-e", 100).at(T0 + 2_000);
    assert!(
        matches!(admit(&store, &r2), AdmissionDecision::Allow { .. }),
        "释放掉的那次不占窗口"
    );
    // 结算按实际金额计入窗口（净贡献 = 实际金额，不是预留额）
    let charged = price(50);
    store
        .settle_attempt(&r2.attempt, &charged, ts(T0 + 3_000))
        .unwrap();
    let r3 = Req::new(&store, "r-3", 0, "cred-e", 100).at(T0 + 4_000);
    assert!(
        matches!(admit(&store, &r3), AdmissionDecision::Allow { .. }),
        "50 + 210 <= 270"
    );
    let r4 = Req::new(&store, "r-4", 0, "cred-e", 1).at(T0 + 5_000);
    assert_eq!(
        denied(&admit(&store, &r4)),
        Some(ReasonCode::RateLimited),
        "50 + 210 + 12 > 270"
    );
    assert_clean(&store);
}

#[test]
fn check_4_in_flight_cap_alone_rejects_and_frees_up_on_release() {
    let db = TempDb::new("check-inflight");
    let store = funded_paid_chain(&db);
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                inflight_cap: Some(thb(400)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let r1 = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(matches!(
        admit(&store, &r1),
        AdmissionDecision::Allow { .. }
    ));
    let r2 = Req::new(&store, "r-2", 0, "cred-e", 100);
    assert_eq!(
        denied(&admit(&store, &r2)),
        Some(ReasonCode::InFlightLimited)
    );
    // 在途是「还没结算」的：释放（或结算）之后额度回来
    store.release_attempt(&r1.attempt, t0()).unwrap();
    let r3 = Req::new(&store, "r-3", 0, "cred-e", 100);
    assert!(matches!(
        admit(&store, &r3),
        AdmissionDecision::Allow { .. }
    ));
    // 在途上限在组织这一层（祖先）同样生效
    let db2 = TempDb::new("check-inflight-ancestor");
    let store2 = funded_paid_chain(&db2);
    store2
        .set_account_limits(
            &aid("paid"),
            &AccountLimits {
                inflight_cap: Some(thb(250)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    assert!(matches!(
        admit(&store2, &Req::new(&store2, "a-1", 0, "cred-e", 100)),
        AdmissionDecision::Allow { .. }
    ));
    assert_eq!(
        denied(&admit(&store2, &Req::new(&store2, "a-2", 0, "cred-e", 100))),
        Some(ReasonCode::InFlightLimited)
    );
}

#[test]
fn check_5_subsidy_pool_cap_alone_rejects_and_a_month_without_a_cap_is_zero() {
    let db = TempDb::new("check-pool");
    let store = world(&db);
    // kid-c 一路额度充足；补贴池 11 月上限 400
    allocate_chain(&store, "c", &["pool", "school", "kid-c"], 1_000_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(400), t0())
        .unwrap();
    let r1 = Req::new(&store, "r-1", 0, "cred-c", 100);
    assert!(matches!(
        admit(&store, &r1),
        AdmissionDecision::Allow { .. }
    ));
    let r2 = Req::new(&store, "r-2", 0, "cred-c", 100);
    assert_eq!(
        denied(&admit(&store, &r2)),
        Some(ReasonCode::SubsidyPoolExhausted)
    );
    // 池上限是所有成员共用的：另一个成员（kid-a）也被池拒绝，尽管它自己的额度没动
    allowance_chain_a(&store, 1_000_000);
    let r3 = Req::new(&store, "r-3", 0, "cred-a", 100);
    assert_eq!(
        denied(&admit(&store, &r3)),
        Some(ReasonCode::SubsidyPoolExhausted)
    );
    // 12 月没设上限 = 0（失败即关闭）
    let r4 = Req::new(&store, "r-4", 0, "cred-c", 1).at(T0 + 40 * DAY);
    assert_eq!(
        denied(&admit(&store, &r4)),
        Some(ReasonCode::SubsidyPoolExhausted)
    );
    store
        .set_pool_cap(
            &aid("pool"),
            month(2026, 12),
            &thb(1_000),
            ts(T0 + 40 * DAY),
        )
        .unwrap();
    let r5 = Req::new(&store, "r-5", 0, "cred-c", 1).at(T0 + 40 * DAY);
    assert!(matches!(
        admit(&store, &r5),
        AdmissionDecision::Allow { .. }
    ));
    assert_clean(&store);
}

#[test]
fn a_pool_months_consumption_window_is_half_open_so_every_entry_belongs_to_exactly_one_month() {
    // 11 月 = [11-01 00:00, 12-01 00:00)（UTC）：12 月第一毫秒的预留算 12 月，不算 11 月；11 月最后一毫秒的算 11 月
    let db = TempDb::new("pool-month-edges");
    let store = world(&db);
    allocate_chain(&store, "c", &["pool", "school", "kid-c"], 10_000_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(500), t0())
        .unwrap();
    store
        .set_pool_cap(&aid("pool"), month(2026, 12), &thb(300), t0())
        .unwrap();
    let december = 1_796_083_200_000; // 2026-12-01T00:00:00Z
    // 11 月最后一毫秒：消耗 210 记在 11 月
    let a = Req::new(&store, "r-a", 0, "cred-c", 100).at(december - 1);
    assert!(admit(&store, &a).is_admitted());
    // 12 月第一毫秒：消耗 210 记在 12 月（12 月上限 300）
    let b = Req::new(&store, "r-b", 0, "cred-c", 100).at(december);
    assert!(admit(&store, &b).is_admitted());
    // 12 月再来一个：已经有 b 的 210，放不下。b 恰好在 12 月的起点，必须算进 12 月
    let c = Req::new(&store, "r-c", 0, "cred-c", 100).at(december + 1);
    assert_eq!(
        denied(&admit(&store, &c)),
        Some(ReasonCode::SubsidyPoolExhausted)
    );
    // 并发请求各自读时钟，读数可以比已有的分录早：回到 11 月最后一毫秒，11 月上限 500，已有 a 的 210，
    // 再放一个 210 刚好放得下；12 月起点的 b 不能算进 11 月
    let d = Req::new(&store, "r-d", 0, "cred-c", 100).at(december - 1);
    let decision = admit(&store, &d);
    assert!(decision.is_admitted(), "{decision:?}");
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 沿祖先：叶子够、祖先不够
// ------------------------------------------------------------------------------------------

#[test]
fn a_leaf_with_room_is_still_denied_when_any_single_ancestor_lacks_it() {
    use idoris_compute_contracts::credential::{Credential, CredentialBinding};
    // 链：kid-a -> teacher -> school -> pool -> platform。逐个让**一个**祖先不够，其余（包括叶子）充足。
    // 做法：让这个祖先的**另一条枝**（它的其它后代）把它的额度花到只剩 100，不碰 kid-a 走的那条路上的其它账户：
    //   teacher 不够 <- 同一个老师下的 kid-b 花；school 不够 <- 学校直属的 kid-c 花；
    //   pool 不够   <- 池下另一个组织 `other` 花。
    for short in ["teacher", "school", "pool"] {
        let db = TempDb::new(&format!("ancestor-{short}"));
        let store = world(&db);
        // 越往上额度越大（S1：下发的额度不能超过授权者本人的），只有 `short` 是 1_000_000，其余都 >= 它
        let big = 9_000_000;
        let pool_c = if short == "pool" { 1_000_000 } else { big };
        let school_c = (if short == "school" { 1_000_000 } else { big }).min(pool_c);
        let teacher_c = (if short == "teacher" { 1_000_000 } else { big }).min(school_c);
        allocate(&store, "plat-pool", "platform", "pool", pool_c);
        allocate(&store, "pool-school", "pool", "school", school_c);
        allocate(&store, "school-teacher", "school", "teacher", teacher_c);
        allocate(&store, "teacher-a", "teacher", "kid-a", 1_000_000);
        allocate(&store, "teacher-b", "teacher", "kid-b", 1_000_000);
        allocate(&store, "school-c", "school", "kid-c", 1_000_000);
        store
            .create_account(
                &account(
                    "other",
                    idoris_compute_contracts::account::AccountKind::Org,
                    Some("pool"),
                ),
                t0(),
            )
            .unwrap();
        allocate(&store, "pool-other", "pool", "other", 1_000_000);
        store
            .create_credential(
                &Credential::new(
                    cid("cred-other"),
                    CredentialBinding::Node {
                        account: aid("other"),
                    },
                    CredentialStatus::Active,
                    None,
                    Vec::new(),
                )
                .unwrap(),
                t0(),
            )
            .unwrap();
        store
            .set_pool_cap(&aid("pool"), month(2026, 11), &thb(100_000_000), t0())
            .unwrap();

        // 烧：额度 1_000_000，花到只剩 100
        let burner = match short {
            "teacher" => "cred-b",
            "school" => "cred-c",
            _ => "cred-other",
        };
        let burn = Req::new(&store, "burn", 0, burner, 100);
        assert!(admit(&store, &burn).is_admitted());
        store
            .settle_attempt(&burn.attempt, &price(1_000_000 - 100), t0())
            .unwrap();

        let remaining = |a: &str| {
            store
                .account_view(&aid(a), t0())
                .unwrap()
                .ceiling_remaining
                .unwrap()
        };
        assert_eq!(remaining(short), thb(100), "{short} 被烧到只剩 100");
        // 被烧的只有 short（和它之下的烧钱路径）；kid-a 自己和 kid-a 路上的其它祖先都充足
        assert_eq!(remaining("kid-a"), thb(1_000_000), "{short}: 叶子够");
        for other_ancestor in ["teacher", "school", "pool"] {
            if other_ancestor != short {
                assert!(
                    remaining(other_ancestor).micros >= 1_000_000,
                    "{short}: {other_ancestor} 够"
                );
            }
        }
        // 叶子够、祖先 short 不够（全额 210 > 100）：不是放行。压低 max_tokens 看的是整条链里最紧的那个上限：
        // 只剩 100 -> 10 + 2×45 = 100，所以降级到 45 个输出 token，而不是叶子自己的 100
        let degraded = Req::new(&store, "r-1", 0, "cred-a", 100);
        let decision = admit(&store, &degraded);
        let AdmissionDecision::Degrade { admitted, .. } = &decision else {
            panic!("{short}: expected a degraded admission, got {decision:?}");
        };
        assert_eq!(admitted.effective_max_tokens, 45, "{short}");
        assert_eq!(remaining(short), thb(0), "{short} 被这次预留用完");
        // 现在 short 一点不剩：叶子再够也被拒绝，并且什么都没占
        let req = Req::new(&store, "r-2", 0, "cred-a", 100).at(T0 + 1);
        assert_eq!(
            denied(&admit(&store, &req)),
            Some(ReasonCode::BudgetExhausted),
            "ancestor {short}"
        );
        assert!(
            store.hold(&req.attempt, "model").unwrap().is_none(),
            "{short}: 拒绝不占额度"
        );
        assert_eq!(
            remaining("kid-a"),
            thb(1_000_000 - 100),
            "叶子自己的额度只被降级的那次预留占了 100"
        );
        assert_clean(&store);
    }
}

#[test]
fn the_ancestor_chain_includes_pool_and_root_entries_so_rate_limits_work_on_any_ancestor() {
    // 速率闸门配在祖先（学校）上：它的两个成员共用同一个窗口
    let db = TempDb::new("rate-ancestor");
    let store = world(&db);
    allocate_chain(
        &store,
        "a",
        &["pool", "school", "teacher", "kid-a"],
        1_000_000,
    );
    allocate(&store, "b-leaf", "teacher", "kid-b", 1_000_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(10_000_000), t0())
        .unwrap();
    store
        .set_account_limits(
            &aid("school"),
            &AccountLimits {
                minute_cap: Some(thb(300)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    assert!(matches!(
        admit(&store, &Req::new(&store, "a-1", 0, "cred-a", 100)),
        AdmissionDecision::Allow { .. }
    ));
    assert_eq!(
        denied(&admit(&store, &Req::new(&store, "b-1", 0, "cred-b", 100))),
        Some(ReasonCode::RateLimited),
        "kid-b 自己没花过，但学校这一分钟的额度被 kid-a 占了"
    );
}

// ------------------------------------------------------------------------------------------
// 压低 max_tokens
// ------------------------------------------------------------------------------------------

#[test]
fn when_funds_are_short_max_tokens_is_lowered_to_the_largest_that_fits() {
    let db = TempDb::new("fit");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(150), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210 > 150
    let decision = admit(&store, &req);
    let AdmissionDecision::Degrade { admitted, reason } = &decision else {
        panic!("expected a degraded admission, got {decision:?}");
    };
    assert_eq!(reason.reason_code(), ReasonCode::BudgetDegraded);
    // 150 - 10（输入）= 140 → 70 个输出 token；这个值必须发给上游
    assert_eq!(admitted.effective_max_tokens, 70);
    assert_eq!(admitted.reservation.price.micros(), 150);
    // 成本预留按同一个有效 max_tokens：10 × 0.3 + 70 × 0.5 = 38
    assert_eq!(admitted.reservation.cost.micros(), 38);
    assert_eq!(
        store.hold(&req.attempt, "model").unwrap().unwrap().reserved,
        thb(150)
    );
    // 压低到刚好用完：账户现在是 0
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(0))
    );
    // 少 1 微单位：只放得下 69 个 token（10 + 138 = 148 <= 149，70 个要 150 > 149）
    let db2 = TempDb::new("fit-149");
    let store2 = world(&db2);
    store2
        .fund_account(&aid("emp"), &thb(149), &key("f-emp"), t0())
        .unwrap();
    store2
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let decision = admit(&store2, &Req::new(&store2, "r-1", 0, "cred-e", 100));
    assert_eq!(decision.admitted().unwrap().effective_max_tokens, 69);
    assert_eq!(decision.admitted().unwrap().reservation.price.micros(), 148);
}

#[test]
fn lowering_max_tokens_never_gets_around_the_platform_protections() {
    // 钱够压低，但速率闸门也超：不是「只有钱不够」，不降级，直接拒绝
    let db = TempDb::new("fit-vs-rate");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(150), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
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
    // 全额 210：钱和速率都不过。压低到钱放得下（150）速率仍然不过（上限 100）→ 拒绝，原因是 budget（S0 的顺序），不降级
    let decision = admit(&store, &Req::new(&store, "r-1", 0, "cred-e", 100));
    assert!(denied(&decision).is_some(), "{decision:?}");
    assert_eq!(scalar(&db, "SELECT count(*) FROM ledger_holds"), 0);
}

#[test]
fn when_even_the_minimum_useful_reservation_does_not_fit_the_candidate_is_denied() {
    // 压到最低有用值（10 个 token）也要 10 + 20 = 30：只有 29，放不下，也没有宽限 → budget_exhausted
    let db = TempDb::new("fit-none");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(29), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::BudgetExhausted)
    );
    // 刚好 30 能放下最低有用的预留
    store
        .fund_account(&aid("emp"), &thb(1), &key("f-emp-2"), t0())
        .unwrap();
    let decision = admit(&store, &Req::new(&store, "r-2", 0, "cred-e", 100));
    assert_eq!(decision.admitted().unwrap().effective_max_tokens, 10);
}

// ------------------------------------------------------------------------------------------
// 凭证与账户链的核对（在占预留的同一个事务里）
// ------------------------------------------------------------------------------------------

#[test]
fn a_credential_revoked_or_frozen_after_the_context_was_resolved_is_refused_at_admission() {
    for (status, code) in [
        (CredentialStatus::Revoked, ReasonCode::CredentialRevoked),
        (CredentialStatus::Frozen, ReasonCode::CredentialFrozen),
    ] {
        let db = TempDb::new("gate-credential");
        let store = funded_paid_chain(&db);
        // 付款上下文先解析好，之后凭证才被撤销 / 冻结：准入必须在占预留的事务里拦住
        let req = Req::new(&store, "r-1", 0, "cred-e", 100);
        store
            .set_credential_status(&cid("cred-e"), status, t0())
            .unwrap();
        assert_eq!(denied(&admit(&store, &req)), Some(code), "{status:?}");
        assert_eq!(scalar(&db, "SELECT count(*) FROM ledger_holds"), 0);
    }
}

#[test]
fn an_inactive_account_anywhere_in_the_chain_is_refused() {
    use idoris_compute_contracts::account::AccountStatus;
    for frozen in ["emp", "paid"] {
        let db = TempDb::new("gate-account");
        let store = funded_paid_chain(&db);
        let req = Req::new(&store, "r-1", 0, "cred-e", 100);
        store
            .set_account_status(&aid(frozen), AccountStatus::Frozen)
            .unwrap();
        assert_eq!(
            denied(&admit(&store, &req)),
            Some(ReasonCode::AccountInactive),
            "{frozen}"
        );
        store
            .set_account_status(&aid(frozen), AccountStatus::Active)
            .unwrap();
        assert!(
            matches!(admit(&store, &req), AdmissionDecision::Allow { .. }),
            "解冻后放行"
        );
    }
}

#[test]
fn a_revoked_principal_or_membership_is_refused() {
    let db = TempDb::new("gate-principal");
    let store = funded_paid_chain(&db);
    // 成员凭证的付款上下文带着主体和成员关系；撤销主体 → principal_revoked
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    store.revoke_principal(&pid("p-e"), t0()).unwrap();
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::PrincipalRevoked)
    );

    let db2 = TempDb::new("gate-membership");
    let store2 = funded_paid_chain(&db2);
    let req = Req::new(&store2, "r-1", 0, "cred-e", 100);
    store2.revoke_membership(&mid("m-e"), t0()).unwrap();
    assert_eq!(
        denied(&admit(&store2, &req)),
        Some(ReasonCode::MembershipRevoked)
    );
}

#[test]
fn an_expired_credential_is_refused() {
    use idoris_compute_contracts::credential::{Credential, CredentialBinding};
    let db = TempDb::new("gate-expiry");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(1_000_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    store
        .create_credential(
            &Credential::new(
                cid("cred-e-short"),
                CredentialBinding::Member {
                    membership: mid("m-e"),
                },
                CredentialStatus::Active,
                Some(ts(T0 + MINUTE)),
                Vec::new(),
            )
            .unwrap(),
            t0(),
        )
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e-short", 100).at(T0 + MINUTE - 1);
    assert!(matches!(
        admit(&store, &req),
        AdmissionDecision::Allow { .. }
    ));
    let req = Req::new(&store, "r-2", 0, "cred-e-short", 100).at(T0 + MINUTE);
    assert_eq!(
        denied(&admit(&store, &req)),
        Some(ReasonCode::CredentialExpired)
    );
}

// ------------------------------------------------------------------------------------------
// 预留的 TTL 与候选切换
// ------------------------------------------------------------------------------------------

#[test]
fn an_expired_reservation_frees_its_capacity_even_if_nobody_swept_it() {
    let db = TempDb::new("ttl");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(300), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let ttl = store.ledger_config().unwrap().reservation_ttl_ms;
    let r1 = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &r1);
    // TTL 之内：占着，第二个放不下（300 - 210 = 90 < 210）
    let r2 = Req::new(&store, "r-2", 0, "cred-e", 100).at(T0 + ttl - 1);
    let decision = admit(&store, &r2);
    assert_eq!(
        decision.admitted().map(|a| a.effective_max_tokens),
        Some(40),
        "只剩 90：压低到 (90-10)/2 = 40"
    );
    // 过了 TTL：第一个预留自动放回去（准入先扫过期），新的请求拿到完整额度
    let r3 = Req::new(&store, "r-3", 0, "cred-e", 100).at(T0 + ttl + 1);
    store.release_attempt(&r2.attempt, ts(T0 + ttl)).unwrap();
    let decision = admit(&store, &r3);
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    assert_eq!(
        store.hold(&r1.attempt, "model").unwrap().unwrap().state,
        HoldState::Expired
    );
    assert_clean(&store);
}

#[test]
fn switching_candidates_releases_the_failed_attempt_in_the_same_transaction() {
    // 余额刚好够一次：第一个候选失败后，第二个候选的准入必须看得到第一个释放出来的额度
    let db = TempDb::new("switch-release");
    let store = world(&db);
    store
        .fund_account(&aid("emp"), &thb(reserve_of(100)), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(1_000_000), &key("f-paid"), t0())
        .unwrap();
    let first = Req::new(&store, "r-1", 0, "cred-e", 100);
    assert!(matches!(
        admit(&store, &first),
        AdmissionDecision::Allow { .. }
    ));
    // 序号 0 还占着额度时，同一个请求的序号 1 也能放行（它先释放序号 0）
    let second = Req::new(&store, "r-1", 1, "cred-e", 100);
    let decision = admit(&store, &second);
    assert!(
        matches!(decision, AdmissionDecision::Allow { .. }),
        "{decision:?}"
    );
    assert_eq!(
        store.hold(&first.attempt, "model").unwrap().unwrap().state,
        HoldState::Released
    );
    assert_eq!(
        store.hold(&second.attempt, "model").unwrap().unwrap().state,
        HoldState::Open
    );
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(0)),
        "只占着第二次的预留，不是两次"
    );
    // 别的请求的预留不受影响
    let other = Req::new(&store, "r-9", 0, "cred-e", 100);
    assert_eq!(
        denied(&admit(&store, &other)),
        Some(ReasonCode::BudgetExhausted)
    );
    assert_clean(&store);
}

#[test]
fn releasing_is_idempotent_and_a_missing_hold_is_nothing_to_release() {
    let db = TempDb::new("release-idem");
    let store = funded_paid_chain(&db);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    let first = store.release_attempt(&req.attempt, t0()).unwrap().unwrap();
    assert_eq!(first.result, SettleResult::Released);
    let again = store.release_attempt(&req.attempt, t0()).unwrap().unwrap();
    assert_eq!(again.result, SettleResult::AlreadyReleased);
    let ghost = AttemptId::new(RequestId::new("ghost").unwrap(), AttemptIndex(0));
    assert!(store.release_attempt(&ghost, t0()).unwrap().is_none());
    // 释放之后额度回来，分录没有被改写（只追加）
    assert_eq!(
        store.account_view(&aid("emp"), t0()).unwrap().balance,
        Some(thb(1_000_000))
    );
    let kinds: Vec<String> = store
        .entries_of_hold(&req.attempt, "model")
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "reserve", "reserve", "reserve", "release", "release", "release"
        ]
    );
}
