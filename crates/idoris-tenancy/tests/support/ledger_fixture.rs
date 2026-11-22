//! S3 准入账本测试共用的夹具：账户树、主体、成员关系、凭证、价目、一次请求的输入。
//!
//! 账户树（全部 THB，除了补贴池所在的一枝，所有账户一开始**什么额度、资金都没有**——失败即关闭，每个测试自己给钱）：
//!
//! ```text
//! platform
//!  ├─ pool (补贴池)
//!  │   └─ school (组织)
//!  │        ├─ teacher (担保人)        主体 p-t，成员关系 m-t
//!  │        │    ├─ kid-a (成员)       主体 p-a，成员关系 m-a
//!  │        │    └─ kid-b (成员)       主体 p-b，成员关系 m-b
//!  │        └─ kid-c (成员)            主体 p-c，成员关系 m-c
//!  └─ paid (组织)
//!       └─ emp (成员)                  主体 p-e，成员关系 m-e
//! ```
//!
//! 价目（微泰铢）：每个输入 token 1，每个输出 token 2，所以 `10` 个输入加 `max_tokens = 100` 的预留是 `10 + 200 = 210`。
//! 时间：`T0` 是 2026-11-01T12:00:00Z（泰国是 19:00，两个时区都在 11 月）。
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, Allocation, Membership, MembershipRole, MembershipStatus,
};
use idoris_compute_contracts::credential::{
    Credential, CredentialBinding, CredentialStatus, PaymentContext,
};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, AttemptId, AttemptIndex, BillingMonth, CredentialId, EndpointKey,
    MembershipId, PrincipalId, RequestId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_compute_contracts::pricing::{Cost, Price, RateCard};
use idoris_compute_contracts::reserve::{ReservePolicy, ReserveRequest};
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::AdmitInput;

/// 2026-11-01T12:00:00Z
pub const T0: i64 = 1_793_534_400_000;
pub const MINUTE: i64 = 60_000;
pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 86_400_000;

pub fn ts(ms: i64) -> TimestampMs {
    TimestampMs(ms)
}
pub fn t0() -> TimestampMs {
    TimestampMs(T0)
}

pub struct TempDb(PathBuf);

impl TempDb {
    pub fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-ledger-{tag}-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )))
    }
    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.0.clone().into_os_string();
            p.push(suffix);
            let _ = std::fs::remove_file(p);
        }
    }
}

pub fn aid(s: &str) -> AccountId {
    AccountId::new(s).unwrap()
}
pub fn pid(s: &str) -> PrincipalId {
    PrincipalId::new(s).unwrap()
}
pub fn mid(s: &str) -> MembershipId {
    MembershipId::new(s).unwrap()
}
pub fn cid(s: &str) -> CredentialId {
    CredentialId::new(s).unwrap()
}
pub fn thb(micros: i64) -> Money {
    Money::new(Unit::Thb, micros)
}
pub fn price(micros: i64) -> Price {
    Price::from_micros(Unit::Thb, micros).unwrap()
}
pub fn month(y: u16, m: u8) -> BillingMonth {
    BillingMonth::new(y, m).unwrap()
}

pub fn account(id: &str, kind: AccountKind, parent: Option<&str>) -> Account {
    Account {
        id: aid(id),
        kind,
        parent: parent.map(aid),
        unit: Unit::Thb,
        status: AccountStatus::Active,
    }
}

/// 建好整棵树、主体、成员关系、凭证。`kid-*` 的凭证叫 `cred-a` / `cred-b` / `cred-c`，老师 `cred-t`，职员 `cred-e`。
pub fn world(db: &TempDb) -> AccountStore {
    let store = AccountStore::open(db.path()).unwrap();
    for (id, kind, parent) in [
        ("platform", AccountKind::Platform, None),
        ("pool", AccountKind::SubsidyPool, Some("platform")),
        ("school", AccountKind::Org, Some("pool")),
        ("teacher", AccountKind::Sponsor, Some("school")),
        ("kid-a", AccountKind::Member, Some("teacher")),
        ("kid-b", AccountKind::Member, Some("teacher")),
        ("kid-c", AccountKind::Member, Some("school")),
        ("paid", AccountKind::Org, Some("platform")),
        ("emp", AccountKind::Member, Some("paid")),
    ] {
        store
            .create_account(&account(id, kind, parent), t0())
            .unwrap();
    }
    for (principal, membership, org, role, acct) in [
        ("p-t", "m-t", "school", MembershipRole::Sponsor, "teacher"),
        ("p-a", "m-a", "school", MembershipRole::Member, "kid-a"),
        ("p-b", "m-b", "school", MembershipRole::Member, "kid-b"),
        ("p-c", "m-c", "school", MembershipRole::Member, "kid-c"),
        ("p-e", "m-e", "paid", MembershipRole::Member, "emp"),
    ] {
        store.ensure_principal(&pid(principal), t0()).unwrap();
        store
            .create_membership(
                &Membership {
                    id: mid(membership),
                    principal: pid(principal),
                    org: aid(org),
                    role,
                    account: Some(aid(acct)),
                    status: MembershipStatus::Active,
                },
                t0(),
            )
            .unwrap();
        store
            .create_credential(
                &Credential::new(
                    cid(&format!("cred-{}", &principal[2..])),
                    CredentialBinding::Member {
                        membership: mid(membership),
                    },
                    CredentialStatus::Active,
                    None,
                    Vec::new(),
                )
                .unwrap(),
                t0(),
            )
            .unwrap();
    }
    store
}

/// 一个花费类额度（预付，窗口 `[from, until)`）。S1 的 `validate_allocation` 要求授权者本人有流入它的额度，
/// 所以测试要自己从平台往下一级一级铺。
pub fn grant(micros: i64, purpose: Purpose, from: i64, until: i64) -> Grant {
    Grant::new(thb(micros), purpose, ts(from), ts(until)).unwrap()
}

/// 铺一条额度（预付）。
pub fn allocate(store: &AccountStore, id: &str, parent: &str, child: &str, micros: i64) {
    store
        .create_allocation(
            &Allocation {
                id: AllocationId::new(id).unwrap(),
                parent: aid(parent),
                child: aid(child),
                ceiling: grant(micros, Purpose::Prepaid, T0 - DAY, T0 + 60 * DAY),
                idempotency_key: None,
            },
            t0(),
        )
        .unwrap();
}

/// 撤销一笔额度（S2 的「关授信」）。撤销记在 S2 的侧表 `allocation_revocations` 里（只追加，`allocations` 不动）。
/// 这张表在 S2 合入之前不存在，所以这里模拟它：只建账本读到的那一列（账本看到这张表就把有撤销行的额度排除）。
/// S2 合入之后表是它的迁移建的、还有「谁撤销的」两列，这里按表里有没有这两列决定怎么插，所以夹具两种情形都能用。
///
/// 撤销时刻固定记成 `T0`；要指定撤销时刻（比如「花完之后才撤销」）用 [`revoke_allocation_at`]。
pub fn revoke_allocation(db: &TempDb, allocation_id: &str) {
    revoke_allocation_at(db, allocation_id, T0);
}

/// 同 [`revoke_allocation`]，但撤销时刻由调用方指定（`allocation_revocations.revoked_at_ms`）。
pub fn revoke_allocation_at(db: &TempDb, allocation_id: &str, revoked_at_ms: i64) {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS allocation_revocations (
             allocation_id TEXT NOT NULL PRIMARY KEY,
             revoked_at_ms INTEGER NOT NULL
         )",
    )
    .unwrap();
    let with_actor: bool = conn
        .query_row(
            "SELECT COUNT(*) > 0 FROM pragma_table_info('allocation_revocations') WHERE name = 'revoked_by_kind'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let sql = if with_actor {
        "INSERT INTO allocation_revocations (allocation_id, revoked_at_ms, revoked_by_kind, revoked_by_id)
         VALUES (?1, ?2, 'operator', 'ledger-test')"
    } else {
        "INSERT INTO allocation_revocations (allocation_id, revoked_at_ms) VALUES (?1, ?2)"
    };
    conn.execute(sql, rusqlite::params![allocation_id, revoked_at_ms])
        .unwrap();
}

/// 沿一条链从平台往下铺同样大小的额度：`chain` 是 `[平台的直接子账户, ..., 叶子]`。
pub fn allocate_chain(store: &AccountStore, name: &str, chain: &[&str], micros: i64) {
    let mut parent = "platform";
    for (index, child) in chain.iter().enumerate() {
        allocate(store, &format!("{name}-{index}"), parent, child, micros);
        parent = child;
    }
}

/// 凭证 `credential`（成员凭证，主体不用再断言）此刻的付款上下文。
pub fn payment(store: &AccountStore, credential: &str) -> PaymentContext {
    store
        .payment_context(&cid(credential), None, t0())
        .unwrap_or_else(|e| panic!("payment context for {credential}: {e}"))
}

pub fn price_card() -> RateCard<Price> {
    RateCard::new(price(1_000_000), price(2_000_000), None).unwrap()
}

pub fn cost_card() -> RateCard<Cost> {
    RateCard::new(
        Cost::from_micros(Unit::Usd, 300_000).unwrap(),
        Cost::from_micros(Unit::Usd, 500_000).unwrap(),
        None,
    )
    .unwrap()
}

pub fn policy() -> ReservePolicy {
    ReservePolicy::new(1_000, 10).unwrap()
}

/// 一次准入请求的输入（自己拥有所有被借用的东西）。
#[derive(Clone)]
pub struct Req {
    pub attempt: AttemptId,
    pub payment: PaymentContext,
    pub endpoint: EndpointKey,
    pub sizing: ReserveRequest,
    pub policy: ReservePolicy,
    pub price: RateCard<Price>,
    pub cost: RateCard<Cost>,
    pub allow_grace: bool,
    pub now: TimestampMs,
}

impl Req {
    /// `request#index`，`credential` 的付款账户，`max_tokens`，输入估算 10 个 token（预留 = `10 + 2 × max_tokens`）。
    pub fn new(
        store: &AccountStore,
        request: &str,
        index: u8,
        credential: &str,
        max_tokens: u64,
    ) -> Self {
        Self::with_payment(request, index, payment(store, credential), max_tokens)
    }

    /// 同 [`Req::new`]，但凭证此刻解析不出付款上下文（被冻结、撤销……）时返回 `None`。
    pub fn try_new(
        store: &AccountStore,
        request: &str,
        index: u8,
        credential: &str,
        max_tokens: u64,
        now: i64,
    ) -> Option<Self> {
        let payment = store
            .payment_context(&cid(credential), None, ts(now))
            .ok()?;
        Some(Self::with_payment(request, index, payment, max_tokens).at(now))
    }

    pub fn with_payment(
        request: &str,
        index: u8,
        payment: PaymentContext,
        max_tokens: u64,
    ) -> Self {
        Self {
            attempt: AttemptId::new(RequestId::new(request).unwrap(), AttemptIndex(index)),
            payment,
            endpoint: EndpointKey::new("example-provider", "example-org/model", "default").unwrap(),
            sizing: ReserveRequest {
                requested_max_tokens: Some(max_tokens),
                input_tokens_estimate: 10,
            },
            policy: policy(),
            price: price_card(),
            cost: cost_card(),
            allow_grace: false,
            now: t0(),
        }
    }

    pub fn at(mut self, now: i64) -> Self {
        self.now = ts(now);
        self
    }

    pub fn grace(mut self) -> Self {
        self.allow_grace = true;
        self
    }

    pub fn input(&self) -> AdmitInput<'_> {
        AdmitInput {
            attempt: &self.attempt,
            payment: &self.payment,
            endpoint: &self.endpoint,
            sizing: self.sizing,
            policy: &self.policy,
            price_rates: &self.price,
            cost_rates: &self.cost,
            allow_grace: self.allow_grace,
            now: self.now,
        }
    }
}

/// 预留额：`10 + 2 × max_tokens`。
pub fn reserve_of(max_tokens: i64) -> i64 {
    10 + 2 * max_tokens
}

/// 对账：账本自己守恒，并且（有尝试记录表时）和尝试记录对得上。
pub fn assert_clean(store: &AccountStore) {
    let report = store.reconcile_with_attempts().unwrap();
    assert!(
        report.is_clean(),
        "ledger reconciliation found {:#?}",
        report.mismatches
    );
}

/// 原生 SQL 数行（不经过被测的 API）。
pub fn scalar(db: &TempDb, sql: &str) -> i64 {
    rusqlite::Connection::open(db.path())
        .unwrap()
        .query_row(sql, [], |row| row.get(0))
        .unwrap()
}

/// 解冻一张凭证（平台损失会冻结凭证；账户库允许解冻，撤销才不可逆）。随机历史里损失之后接着走，要把凭证解冻。
pub fn unfreeze_credential(db: &TempDb, credential: &str) {
    let changed = rusqlite::Connection::open(db.path())
        .unwrap()
        .execute(
            "UPDATE credentials SET status = 'active' WHERE credential_id = ?1 AND status = 'frozen'",
            [credential],
        )
        .unwrap();
    assert_eq!(changed, 1, "the credential {credential} was frozen");
}

/// 把一个预留的分录改回 **v0.5.0 写下的样子**：没迟到的结算里超出预留的部分和预留之内的部分一起记在一条 `settle` 里
/// （已花是整笔，归属时刻是预留创建的时刻），不另记 `late_settle`。
///
/// v0.5.0 的库里没有 overage 的结算（夹具里所有结算都等于预留），所以测试用现在的代码写一笔，再用这个函数按旧写法改回去：
/// 把 `late_settle` 的已花并回同一账户上的 `settle`，删掉 `late_settle`。要拆掉只追加的触发器（这个库只给这个测试用，
/// 之后账本的代码照常读它）。迟到的结算（预留过期之后才结算）旧版本本来就写 `late_settle`，没有 `settle` 的预留不动。
pub fn rewrite_overage_as_v0_5_0(db: &TempDb, hold_id: &str) {
    rewrite_overage_partially(db, hold_id, 0);
}

/// 把一个预留的分录改成**超出的部分只搬了一半的中间形态**（Codex 三复审 Low）：每个账户上，`late_settle` 里只留 `keep_in_late`，
/// 其余的并回同一账户上的 `settle`。`keep_in_late = 0` 就是 v0.5.0 的旧写法（`late_settle` 整条删掉）；`keep_in_late` 等于原来的
/// `late_settle` 就是现在的写法（什么都不改）。例：预留 100、实际 100 + 100 超出：`settle` 已花 100 + `late_settle` 100 →
/// `keep_in_late = 40`：`settle` 已花 160 + `late_settle` 40。
///
/// 要拆掉只追加的触发器（这个库只给这个测试用）。没有 `late_settle` 的预留（比如 main 上本来就是旧写法）不动。
pub fn rewrite_overage_partially(db: &TempDb, hold_id: &str, keep_in_late: i64) {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute_batch(
        "DROP TRIGGER IF EXISTS ledger_entries_append_only_update;
         DROP TRIGGER IF EXISTS ledger_entries_append_only_delete;",
    )
    .unwrap();
    let late: Vec<(String, i64)> = {
        let mut stmt = conn
            .prepare(
                "SELECT account_id, spent_micros FROM ledger_entries
                  WHERE hold_id = ?1 AND kind = 'late_settle'
                    AND EXISTS (SELECT 1 FROM ledger_entries s
                                 WHERE s.hold_id = ?1 AND s.account_id = ledger_entries.account_id AND s.kind = 'settle')",
            )
            .unwrap();
        stmt.query_map([hold_id], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    for (account, spent) in late {
        assert!(
            spent >= keep_in_late,
            "{hold_id} on {account}: only {spent} is booked as a settlement-time spend, cannot keep {keep_in_late}"
        );
        conn.execute(
            "UPDATE ledger_entries SET spent_micros = spent_micros + ?3
              WHERE hold_id = ?1 AND account_id = ?2 AND kind = 'settle'",
            rusqlite::params![hold_id, account, spent - keep_in_late],
        )
        .unwrap();
        if keep_in_late == 0 {
            conn.execute(
                "DELETE FROM ledger_entries WHERE hold_id = ?1 AND account_id = ?2 AND kind = 'late_settle'",
                rusqlite::params![hold_id, account],
            )
            .unwrap();
        } else {
            conn.execute(
                "UPDATE ledger_entries SET spent_micros = ?3
                  WHERE hold_id = ?1 AND account_id = ?2 AND kind = 'late_settle'",
                rusqlite::params![hold_id, account, keep_in_late],
            )
            .unwrap();
        }
    }
}

/// 让一个预留在 `account` 上的净额变成 `-(release_more - reserved)`（库被改坏了的样子）：先拆掉「在途净额必须落在 [0, 预留额]」的触发器，
/// 再多插一条 `release`，把预留 `reserved` 之外又放掉 `release_more - reserved`。账户、预留、币种、归属时刻抄这个预留的 `reserve` 那条。
pub fn make_the_net_negative(db: &TempDb, account: &str, hold_id: &str, release_more: i64) {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute_batch("DROP TRIGGER ledger_entries_held_bounds")
        .unwrap();
    let inserted = conn
        .execute(
            "INSERT INTO ledger_entries
                 (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
                  window_at_ms, at_ms, idempotency_key)
             SELECT account_id, hold_id, 'release', unit, ?3, 0, 0, 0, window_at_ms, at_ms, 'tampered-negative-' || hold_id
               FROM ledger_entries WHERE hold_id = ?2 AND account_id = ?1 AND kind = 'reserve'",
            rusqlite::params![account, hold_id, -release_more],
        )
        .unwrap();
    assert_eq!(
        inserted, 1,
        "the reservation has one reserve entry on {account}"
    );
}

/// 给一个预留在 `account` 上插一条负的 `late_settle`（结算那一刻才发生的花费的净额变成负数）：CHECK 要求 `spent >= 0`，所以这里把检查关掉。
pub fn make_the_settlement_time_spend_negative(
    db: &TempDb,
    account: &str,
    hold_id: &str,
    spent: i64,
) {
    let conn = rusqlite::Connection::open(db.path()).unwrap();
    conn.execute_batch("PRAGMA ignore_check_constraints = ON")
        .unwrap();
    let inserted = conn
        .execute(
            "INSERT INTO ledger_entries
                 (account_id, hold_id, kind, unit, held_micros, spent_micros, funds_micros, liability_micros,
                  window_at_ms, at_ms, idempotency_key)
             SELECT account_id, hold_id, 'late_settle', unit, 0, ?3, 0, 0, window_at_ms + 5, at_ms + 5, 'tampered-late-' || hold_id
               FROM ledger_entries WHERE hold_id = ?2 AND account_id = ?1 AND kind = 'reserve'",
            rusqlite::params![account, hold_id, spent],
        )
        .unwrap();
    assert_eq!(
        inserted, 1,
        "the reservation has one reserve entry on {account}"
    );
}
