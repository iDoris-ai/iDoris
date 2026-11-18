//! S3 准入账本：预留的结算、释放、过期，overage 的归属（担保人 / 平台损失账户 / 冻结凭证），预留过期后的迟到结算，
//! 以及账本自己的守恒。
//!
//! 数字约定见 `support/ledger_fixture.rs`：`10` 个输入 token 加 `max_tokens = M` 的预留是 `10 + 2M` 微泰铢，
//! 所以 `max_tokens = 100` 的预留是 210。预留 TTL 默认 30 分钟。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use fixture::*;
use idoris_compute_contracts::admission::AdmissionDecision;
use idoris_compute_contracts::credential::CredentialStatus;
use idoris_compute_contracts::ids::IdempotencyKey;
use idoris_compute_contracts::reason::ReasonCode;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::{
    AlertKind, CreditLineSetting, HoldState, LedgerError, PLATFORM_LOSS_ACCOUNT, SettleResult,
};

fn key(s: &str) -> IdempotencyKey {
    IdempotencyKey::new(s).unwrap()
}

fn admit(store: &AccountStore, req: &Req) -> AdmissionDecision {
    store.admit_attempt(&req.input()).unwrap()
}

fn ttl(store: &AccountStore) -> i64 {
    store.ledger_config().unwrap().reservation_ttl_ms
}

/// `emp` 有 `emp_funds`，它的组织 `paid` 有 100 万；`emp` 没有授信。
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

fn spent_total(db: &TempDb, account: &str) -> i64 {
    scalar(
        db,
        &format!(
            "SELECT COALESCE(SUM(spent_micros), 0) FROM ledger_entries WHERE account_id = '{account}'"
        ),
    )
}

fn held_total(db: &TempDb, account: &str) -> i64 {
    scalar(
        db,
        &format!(
            "SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries WHERE account_id = '{account}'"
        ),
    )
}

// ------------------------------------------------------------------------------------------
// 结算与释放
// ------------------------------------------------------------------------------------------

#[test]
fn settling_below_the_reservation_converts_it_to_spend_and_returns_the_rest() {
    let db = TempDb::new("settle-below");
    let store = paid_chain(&db, 1_000);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210
    admit(&store, &req);
    assert_eq!(balance(&store, "emp", T0), 1_000 - 210);

    let outcome = store
        .settle_attempt(&req.attempt, &price(80), ts(T0 + 5_000))
        .unwrap();
    assert_eq!(outcome.result, SettleResult::Settled);
    assert!(!outcome.late);
    assert_eq!(outcome.charged, thb(80));
    assert_eq!(outcome.overage, thb(0), "没有超出预留");
    // 沿所有祖先：在途归零，已花 80，余额只少 80
    for account in ["emp", "paid", "platform"] {
        assert_eq!(held_total(&db, account), 0, "{account}");
        assert_eq!(spent_total(&db, account), 80, "{account}");
    }
    assert_eq!(balance(&store, "emp", T0 + 5_000), 1_000 - 80);
    assert_eq!(balance(&store, "paid", T0 + 5_000), 1_000_000 - 80);
    let hold = store.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(
        (hold.state, hold.charged),
        (HoldState::Settled, Some(thb(80)))
    );
    // 分录只追加：预留的那三条还在，结算是新增的三条
    let kinds: Vec<String> = store
        .entries_of_hold(&req.attempt, "model")
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "reserve", "reserve", "reserve", "settle", "settle", "settle"
        ]
    );
    assert_clean(&store);
}

#[test]
fn settling_zero_is_a_release_and_settling_is_idempotent_but_never_overwrites() {
    let db = TempDb::new("settle-idem");
    let store = paid_chain(&db, 1_000);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    store
        .settle_attempt(&req.attempt, &price(80), t0())
        .unwrap();
    // 同一个金额重放：什么都不写
    let entries_before = store.entries_of_hold(&req.attempt, "model").unwrap().len();
    let replay = store
        .settle_attempt(&req.attempt, &price(80), ts(T0 + 1))
        .unwrap();
    assert_eq!(replay.result, SettleResult::AlreadySettled);
    assert_eq!(
        store.entries_of_hold(&req.attempt, "model").unwrap().len(),
        entries_before
    );
    // 不同的金额：错误，不覆盖
    let err = store
        .settle_attempt(&req.attempt, &price(81), t0())
        .unwrap_err();
    assert!(
        matches!(
            err,
            LedgerError::SettleConflict {
                existing: 80,
                requested: 81,
                ..
            }
        ),
        "{err}"
    );
    let err = store
        .settle_attempt(&req.attempt, &price(0), t0())
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::SettleConflict { .. }),
        "已经收过钱不能再说「没有钱要收」: {err}"
    );
    assert_eq!(balance(&store, "emp", T0), 1_000 - 80);

    // 释放之后又来一笔要收的钱：错误
    let other = Req::new(&store, "r-2", 0, "cred-e", 100);
    admit(&store, &other);
    store
        .settle_attempt(&other.attempt, &price(0), t0())
        .unwrap();
    let err = store
        .settle_attempt(&other.attempt, &price(1), t0())
        .unwrap_err();
    assert!(
        matches!(err, LedgerError::ChargeAfterRelease { .. }),
        "{err}"
    );
    // 单位不一致、不存在的预留
    let usd = idoris_compute_contracts::pricing::Price::from_micros(
        idoris_compute_contracts::money::Unit::Usd,
        1,
    )
    .unwrap();
    assert!(matches!(
        store.settle_attempt(&req.attempt, &usd, t0()).unwrap_err(),
        LedgerError::Invalid(_)
    ));
    let ghost = Req::new(&store, "ghost", 0, "cred-e", 10);
    assert!(matches!(
        store
            .settle_attempt(&ghost.attempt, &price(1), t0())
            .unwrap_err(),
        LedgerError::HoldNotFound(_)
    ));
    assert_clean(&store);
}

#[test]
fn the_database_itself_refuses_to_release_a_hold_twice_or_rewrite_history() {
    // 就算 Rust 层有 bug：触发器不让同一个预留在同一个账户上释放得比预留的多，也不让改写、删除分录
    let db = TempDb::new("db-guards");
    let store = paid_chain(&db, 1_000);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.pragma_update(None, "foreign_keys", "ON").unwrap();
    let insert_release = |k: &str| {
        conn.execute(
            "INSERT INTO ledger_entries (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros,
                                         liability_micros, window_at_ms, at_ms, idempotency_key)
             VALUES ('emp', 'r-1#0', 'release', 'THB', -300, 0, 0, 0, 0, 0, ?1)",
            [k],
        )
    };
    let err = insert_release("forged-1").unwrap_err().to_string();
    assert!(err.contains("released more than it reserved"), "{err}");
    // 追加式：改、删、REPLACE 都不行
    for sql in [
        "UPDATE ledger_entries SET held_micros = 0",
        "DELETE FROM ledger_entries",
        "INSERT OR REPLACE INTO ledger_entries (entry_id, account_id, hold_id, kind, unit, held_micros, spent_micros,
            funds_micros, liability_micros, window_at_ms, at_ms, idempotency_key)
         VALUES (1, 'emp', 'r-1#0', 'reserve', 'THB', 0, 0, 0, 0, 0, 0, 'rewritten')",
        "INSERT OR REPLACE INTO ledger_entries (account_id, hold_id, kind, unit, held_micros, spent_micros,
            funds_micros, liability_micros, window_at_ms, at_ms, idempotency_key)
         VALUES ('emp', 'r-1#0', 'reserve', 'THB', 0, 0, 0, 0, 0, 0, 'r-1#0|reserve|emp')",
        "UPDATE ledger_holds SET reserved_micros = 1",
        "DELETE FROM ledger_holds",
        "UPDATE ledger_holds SET state = 'settled'",
    ] {
        assert!(conn.execute(sql, []).is_err(), "must be refused: {sql}");
    }
    // 种类和数列必须对得上：reserve 不能带已花
    let err = conn
        .execute(
            "INSERT INTO ledger_entries (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros,
                                         liability_micros, window_at_ms, at_ms, idempotency_key)
             VALUES ('emp', 'r-1#0', 'reserve', 'THB', 0, 5, 0, 0, 0, 0, 'forged-2')",
            [],
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("CHECK"), "{err}");
    // 树里不能建一个叫平台损失的账户
    let err = store
        .create_account(
            &account(
                "platform-loss",
                idoris_compute_contracts::account::AccountKind::Org,
                Some("platform"),
            ),
            t0(),
        )
        .unwrap_err()
        .to_string();
    assert!(err.contains("reserved"), "{err}");
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// overage：担保人承担、平台损失账户、冻结凭证
// ------------------------------------------------------------------------------------------

#[test]
fn overage_within_the_envelope_is_borne_by_the_guarantor_and_freezes_nothing() {
    let db = TempDb::new("overage-sponsor");
    let store = paid_chain(&db, 1_000);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210
    admit(&store, &req);
    // 实际 400，超出预留 190；链上还能吸收 min(1_000 - 210, 999_790) = 790
    let outcome = store
        .settle_attempt(&req.attempt, &price(400), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.overage, thb(190));
    assert_eq!(outcome.sponsor_covered, thb(190));
    assert_eq!(outcome.platform_loss, thb(0));
    assert!(!outcome.credential_frozen);
    // 担保人（emp 的父账户 paid）有一条「担保承担」分录，平台损失账户什么都没有
    let paid_entries = store.entries_of_account("paid").unwrap();
    let covered: Vec<_> = paid_entries
        .iter()
        .filter(|e| e.kind == "sponsor_covered")
        .collect();
    assert_eq!(covered.len(), 1);
    assert_eq!(covered[0].liability, 190);
    assert!(
        store
            .entries_of_account(PLATFORM_LOSS_ACCOUNT)
            .unwrap()
            .is_empty()
    );
    // overage 是事实，照常记账：已花是实际的 400
    assert_eq!(spent_total(&db, "emp"), 400);
    assert_eq!(balance(&store, "emp", T0 + 1_000), 600);
    assert!(
        store
            .alerts(Some(AlertKind::PlatformLoss))
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store
            .get_credential(&cid("cred-e"))
            .unwrap()
            .unwrap()
            .status(),
        CredentialStatus::Active
    );
    assert_clean(&store);
}

#[test]
fn overage_beyond_the_credit_goes_to_the_platform_loss_account_alerts_and_freezes_the_credential() {
    let db = TempDb::new("overage-loss");
    let store = paid_chain(&db, 300);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210，余额 300 -> 90
    // 同一张凭证的下一次请求，付款上下文在冻结之前就解析好了（请求正在路上）
    let next = Req::new(&store, "r-2", 0, "cred-e", 10);
    admit(&store, &req);
    // 实际 500，超出 290；链上只能吸收 90（余额 300 - 预留 210），没有授信 -> 担保人 90，平台损失 200
    let outcome = store
        .settle_attempt(&req.attempt, &price(500), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(outcome.overage, thb(290));
    assert_eq!(outcome.sponsor_covered, thb(90));
    assert_eq!(outcome.platform_loss, thb(200));
    assert!(outcome.credential_frozen);
    assert!(
        outcome
            .logs
            .iter()
            .any(|l| l.contains("platform_loss") && l.contains("credential_frozen=true"))
    );
    // 平台损失账户有一条 200 的分录
    assert_eq!(store.loss_total("platform_loss").unwrap(), 200);
    let loss = store.entries_of_account(PLATFORM_LOSS_ACCOUNT).unwrap();
    assert_eq!(loss.len(), 1);
    assert_eq!(
        (loss[0].kind.as_str(), loss[0].liability),
        ("platform_loss", 200)
    );
    // 可查询的告警（日志之外）：平台损失 + 凭证冻结，带金额、凭证、担保人
    let loss_alerts = store.alerts(Some(AlertKind::PlatformLoss)).unwrap();
    assert_eq!(loss_alerts.len(), 1);
    assert_eq!(loss_alerts[0].amount, thb(200));
    assert_eq!(loss_alerts[0].credential.as_deref(), Some("cred-e"));
    assert_eq!(loss_alerts[0].guarantor.as_deref(), Some("paid"));
    assert_eq!(
        store
            .alerts(Some(AlertKind::CredentialFrozen))
            .unwrap()
            .len(),
        1
    );
    // 凭证真的被冻结了：已经解析好付款上下文的请求在准入事务里被拦住（凭证被冻结的原因码），新请求连上下文都解析不出来
    assert_eq!(
        store
            .get_credential(&cid("cred-e"))
            .unwrap()
            .unwrap()
            .status(),
        CredentialStatus::Frozen
    );
    let decision = admit(&store, &next);
    assert!(
        matches!(&decision, AdmissionDecision::Reject { reason } if reason.code() == ReasonCode::CredentialFrozen),
        "{decision:?}"
    );
    let err = store
        .payment_context(&cid("cred-e"), None, ts(T0 + 2_000))
        .unwrap_err();
    assert!(err.to_string().contains("frozen"), "{err}");
    // 告警记录同样是只追加的
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    for sql in [
        "UPDATE ledger_alerts SET kind = 'grace_used'",
        "DELETE FROM ledger_alerts",
    ] {
        assert!(conn.execute(sql, []).is_err(), "{sql}");
    }
    // 全额照常记账（500 都是已花），账户已经穿过自己的下限
    assert_eq!(balance(&store, "emp", T0 + 1_000), 300 - 500);
    assert_clean(&store);
}

#[test]
fn the_credit_line_extends_what_the_guarantor_absorbs_before_anything_is_a_loss() {
    let db = TempDb::new("overage-credit");
    let store = paid_chain(&db, 300);
    store
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(100),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    // 链上能吸收 min(300 - 210 + 100, ...) = 190；超出 290 -> 担保人 190，平台损失 100
    let outcome = store
        .settle_attempt(&req.attempt, &price(500), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (190, 100)
    );
    assert!(outcome.credential_frozen);
    // 授信刚好够：不是损失
    let db2 = TempDb::new("overage-credit-exact");
    let store2 = paid_chain(&db2, 300);
    store2
        .set_credit_line(
            &aid("emp"),
            &CreditLineSetting {
                limit: thb(200),
                valid_until: None,
            },
            t0(),
        )
        .unwrap();
    let req = Req::new(&store2, "r-1", 0, "cred-e", 100);
    admit(&store2, &req);
    let outcome = store2
        .settle_attempt(&req.attempt, &price(500), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (290, 0)
    );
    assert!(!outcome.credential_frozen);
    assert_eq!(
        store2
            .get_credential(&cid("cred-e"))
            .unwrap()
            .unwrap()
            .status(),
        CredentialStatus::Active
    );
}

#[test]
fn rate_and_in_flight_limits_do_not_turn_an_overage_into_a_loss() {
    // 速率闸门和在途上限是平台的节流，不是授信：这一分钟请求再多，已经发生的 overage 也不能因此算成平台损失
    use idoris_tenancy::accounts::ledger::AccountLimits;
    let db = TempDb::new("overage-throttle");
    let store = paid_chain(&db, 1_000_000);
    store
        .set_account_limits(
            &aid("emp"),
            &AccountLimits {
                minute_cap: Some(thb(250)),
                inflight_cap: Some(thb(250)),
                ..AccountLimits::default()
            },
            t0(),
        )
        .unwrap();
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    let outcome = store
        .settle_attempt(&req.attempt, &price(400), ts(T0 + 1_000))
        .unwrap();
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (190, 0)
    );
    assert!(!outcome.credential_frozen);
}

// ------------------------------------------------------------------------------------------
// 预留过期之后迟到的结算
// ------------------------------------------------------------------------------------------

#[test]
fn a_late_settlement_after_expiry_is_treated_as_overage_of_the_whole_amount() {
    let db = TempDb::new("late-fits");
    let store = paid_chain(&db, 1_000);
    let ttl = ttl(&store);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    // 预留过期、有人扫过：额度放回去
    assert_eq!(store.sweep_expired_holds(ts(T0 + ttl)).unwrap(), 1);
    assert_eq!(
        store.hold(&req.attempt, "model").unwrap().unwrap().state,
        HoldState::Expired
    );
    assert_eq!(balance(&store, "emp", T0 + ttl), 1_000);
    // 迟到的结算：整笔 100 都是 overage（预留早已不占额度），链上够吸收 -> 担保人承担，不是损失
    let outcome = store
        .settle_attempt(&req.attempt, &price(100), ts(T0 + ttl + 60_000))
        .unwrap();
    assert!(outcome.late);
    assert_eq!(outcome.overage, thb(100));
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (100, 0)
    );
    assert!(!outcome.credential_frozen);
    let hold = store.hold(&req.attempt, "model").unwrap().unwrap();
    assert_eq!(
        (hold.state, hold.late, hold.charged),
        (HoldState::Settled, true, Some(thb(100)))
    );
    assert_eq!(
        store.alerts(Some(AlertKind::LateSettlement)).unwrap().len(),
        1
    );
    // 分录：reserve、expire、late_settle（已花 100，没有重复冲在途），最后是担保人承担的那一条责任分录
    let kinds: Vec<String> = store
        .entries_of_hold(&req.attempt, "model")
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            "reserve",
            "reserve",
            "reserve",
            "expire",
            "expire",
            "expire",
            "late_settle",
            "late_settle",
            "late_settle",
            "sponsor_covered"
        ]
    );
    assert_eq!(held_total(&db, "emp"), 0);
    assert_eq!(spent_total(&db, "emp"), 100);
    assert_clean(&store);
}

#[test]
fn a_late_settlement_that_no_longer_fits_is_a_platform_loss_and_freezes_the_credential() {
    let db = TempDb::new("late-loss");
    let store = paid_chain(&db, 300);
    let ttl = ttl(&store);
    let first = Req::new(&store, "r-1", 0, "cred-e", 100); // 预留 210，之后过期
    admit(&store, &first);
    // 过期之后别的请求占走了放回去的额度：余额 300 - 210 = 90
    let second = Req::new(&store, "r-2", 0, "cred-e", 100).at(T0 + ttl);
    assert!(matches!(
        admit(&store, &second),
        AdmissionDecision::Allow { .. }
    ));
    assert_eq!(balance(&store, "emp", T0 + ttl), 90);
    // 第一个请求迟到的结算 200：链上只能吸收 90 -> 担保人 90，平台损失 110，冻结
    let outcome = store
        .settle_attempt(&first.attempt, &price(200), ts(T0 + ttl + 1_000))
        .unwrap();
    assert!(outcome.late);
    assert_eq!(
        (outcome.sponsor_covered.micros, outcome.platform_loss.micros),
        (90, 110)
    );
    assert!(outcome.credential_frozen);
    assert_eq!(store.loss_total("platform_loss").unwrap(), 110);
    let detail: Vec<String> = store
        .alerts(Some(AlertKind::PlatformLoss))
        .unwrap()
        .into_iter()
        .map(|a| a.detail)
        .collect();
    assert_eq!(detail, ["late_charge_beyond_credit"]);
    assert_clean(&store);
}

#[test]
fn lateness_is_decided_by_time_not_by_whether_anyone_swept() {
    // 同一笔结算，不管有没有人扫过，归类必须一样：到了 TTL 的预留收到结算，先补一次过期再按迟到处理
    let db = TempDb::new("late-determinism");
    let store = paid_chain(&db, 1_000);
    let ttl = ttl(&store);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    // 没有任何准入或扫描在 TTL 之后发生；hold 在库里仍是 open
    assert_eq!(
        store.hold(&req.attempt, "model").unwrap().unwrap().state,
        HoldState::Open
    );
    let outcome = store
        .settle_attempt(&req.attempt, &price(50), ts(T0 + ttl))
        .unwrap();
    assert!(outcome.late, "now >= expires_at 就是迟到（右端开）");
    let kinds: Vec<String> = store
        .entries_of_hold(&req.attempt, "model")
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(
        kinds.contains(&"expire".to_string()) && kinds.contains(&"late_settle".to_string()),
        "{kinds:?}"
    );
    // 对照：TTL 前一毫秒结算不是迟到
    let other = Req::new(&store, "r-2", 0, "cred-e", 100).at(T0 + 5_000);
    admit(&store, &other);
    let outcome = store
        .settle_attempt(&other.attempt, &price(50), ts(T0 + 5_000 + ttl - 1))
        .unwrap();
    assert!(!outcome.late);
    assert_clean(&store);
}

#[test]
fn a_late_release_changes_nothing_but_the_state() {
    let db = TempDb::new("late-release");
    let store = paid_chain(&db, 1_000);
    let ttl = ttl(&store);
    let req = Req::new(&store, "r-1", 0, "cred-e", 100);
    admit(&store, &req);
    store.sweep_expired_holds(ts(T0 + ttl)).unwrap();
    let entries_before = store.entries_of_hold(&req.attempt, "model").unwrap().len();
    let outcome = store
        .release_attempt(&req.attempt, ts(T0 + ttl + 1))
        .unwrap()
        .unwrap();
    assert_eq!(outcome.result, SettleResult::Released);
    assert_eq!(
        store.entries_of_hold(&req.attempt, "model").unwrap().len(),
        entries_before
    );
    assert_eq!(
        store.hold(&req.attempt, "model").unwrap().unwrap().state,
        HoldState::Released
    );
    // 释放之后迟到的结算要收钱：错误（已经宣告没有钱要收）
    assert!(matches!(
        store
            .settle_attempt(&req.attempt, &price(5), ts(T0 + ttl + 2))
            .unwrap_err(),
        LedgerError::ChargeAfterRelease { .. }
    ));
    assert_clean(&store);
}

// ------------------------------------------------------------------------------------------
// 守恒：一连串随机的准入 / 结算 / 释放 / 过期之后，分录和预留状态严格对得上
// ------------------------------------------------------------------------------------------

#[test]
fn a_long_pseudo_random_sequence_keeps_every_account_conserved() {
    // 确定性的伪随机（固定种子），所以失败可以复现
    let db = TempDb::new("conservation");
    let store = world(&db);
    allocate_chain(
        &store,
        "a",
        &["pool", "school", "teacher", "kid-a"],
        1_000_000,
    );
    allocate(&store, "b", "teacher", "kid-b", 1_000_000);
    allocate(&store, "c", "school", "kid-c", 1_000_000);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(10_000_000), t0())
        .unwrap();
    store
        .fund_account(&aid("emp"), &thb(50_000), &key("f-emp"), t0())
        .unwrap();
    store
        .fund_account(&aid("paid"), &thb(500_000), &key("f-paid"), t0())
        .unwrap();

    let mut state: u64 = 0x5eed_1234_abcd_ef01;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let credentials = ["cred-a", "cred-b", "cred-c", "cred-e"];
    let mut open: Vec<Req> = Vec::new();
    let mut now = T0;
    for step in 0..400 {
        now += (next() % 40_000) as i64;
        match next() % 5 {
            // 准入
            0..=2 => {
                let credential = credentials[(next() % 4) as usize];
                let max_tokens = 10 + next() % 200;
                // 凭证可能因为 overage 被冻结了：解析不出付款上下文就不发这一次
                if let Some(request) = Req::try_new(
                    &store,
                    &format!("seq-{step}"),
                    0,
                    credential,
                    max_tokens,
                    now,
                ) {
                    let decision = store.admit_attempt(&request.input()).unwrap();
                    if decision.is_admitted() {
                        open.push(request);
                    }
                }
            }
            // 结算（金额可能小于、等于、大于预留）
            3 if !open.is_empty() => {
                let request = open.swap_remove((next() % open.len() as u64) as usize);
                let hold = store.hold(&request.attempt, "model").unwrap().unwrap();
                let charge = match next() % 4 {
                    0 => 0,
                    1 => hold.reserved.micros,
                    2 => hold.reserved.micros / 2,
                    _ => hold.reserved.micros + (next() % 300) as i64,
                };
                // 预留还没被别的路径收尾；迟到、冻结都由账本自己处理
                let outcome = store.settle_attempt(&request.attempt, &price(charge), ts(now));
                assert!(outcome.is_ok(), "step {step}: {outcome:?}");
            }
            // 释放
            4 if !open.is_empty() => {
                let request = open.swap_remove((next() % open.len() as u64) as usize);
                store
                    .release_attempt(&request.attempt, ts(now))
                    .unwrap()
                    .unwrap();
            }
            _ => {}
        }
        if step % 25 == 0 {
            store.sweep_expired_holds(ts(now)).unwrap();
            assert_clean(&store);
        }
    }
    store
        .sweep_expired_holds(ts(now + 10 * ttl(&store)))
        .unwrap();
    assert_clean(&store);
    // 全局：所有账户的在途加起来 = 仍然 open 的预留额 × 链长
    let open_reserved: i64 = scalar(
        &db,
        "SELECT COALESCE(SUM(reserved_micros), 0) FROM ledger_holds WHERE state = 'open'",
    );
    assert_eq!(open_reserved, 0, "全部已收尾或过期");
    assert_eq!(
        scalar(
            &db,
            "SELECT COALESCE(SUM(held_micros), 0) FROM ledger_entries"
        ),
        0
    );
}
