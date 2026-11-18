//! 账户存储的并发：多个线程（各自一条连接，所以靠的是 SQLite 的锁，不是进程内的互斥）同时写同一片数据，
//! 校验和写入之间没有缝，结果与串行执行等价。这是「并发把额度用超 / 重复发放」类错账的直接防线。
//!
//! 手法与 `budget_concurrency.rs` 相同：线程在屏障上同时放行，开各自的连接，固定数量的线程争同一个名额，
//! 断言「恰好 1 个成功、其余是明确的拒绝」，不是「大概只有一个」。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use idoris_compute_contracts::account::{
    Account, AccountKind, AccountStatus, Allocation, AllocationError, DirectoryError, Membership,
    MembershipRole, MembershipStatus, TreeError, subsidy_idempotency_key,
};
use idoris_compute_contracts::grant::{Grant, Purpose};
use idoris_compute_contracts::ids::{
    AccountId, AllocationId, BillingMonth, MembershipId, PrincipalId, TimestampMs,
};
use idoris_compute_contracts::money::{Money, Unit};
use idoris_tenancy::accounts::{AccountStore, AccountStoreError, AllocationOutcome};

const THREADS: usize = 8;
const T0: i64 = 1_793_534_400_000;

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-account-race-{tag}-{}-{}.sqlite3",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        )))
    }
    fn path(&self) -> &Path {
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

fn aid(s: &str) -> AccountId {
    AccountId::new(s).unwrap()
}
fn pid(s: &str) -> PrincipalId {
    PrincipalId::new(s).unwrap()
}
fn now() -> TimestampMs {
    TimestampMs(T0)
}

fn account(id: &str, kind: AccountKind, parent: Option<&str>) -> Account {
    Account {
        id: aid(id),
        kind,
        parent: parent.map(aid),
        unit: Unit::Thb,
        status: AccountStatus::Active,
    }
}

fn grant(micros: i64, purpose: Purpose) -> Grant {
    Grant::new(
        Money::new(Unit::Thb, micros),
        purpose,
        TimestampMs(T0),
        TimestampMs(T0 + 30 * 86_400_000),
    )
    .unwrap()
}

/// 平台 -> 补贴池 -> 两个成员账户，同一个主体 `p-sub` 在两个成员账户上各有一个成员关系；池有额度。
fn base(db: &TempDb) {
    let s = AccountStore::open(db.path()).unwrap();
    for (id, kind, parent) in [
        ("plat", AccountKind::Platform, None),
        ("pool", AccountKind::SubsidyPool, Some("plat")),
        ("kid-1", AccountKind::Member, Some("pool")),
        ("kid-2", AccountKind::Member, Some("pool")),
        ("org", AccountKind::Org, Some("plat")),
    ] {
        s.create_account(&account(id, kind, parent), now()).unwrap();
    }
    s.ensure_principal(&pid("p-sub"), now()).unwrap();
    for (m, acct) in [("m-1", "kid-1"), ("m-2", "kid-2")] {
        s.create_membership(
            &Membership {
                id: MembershipId::new(m).unwrap(),
                principal: pid("p-sub"),
                org: aid("plat"),
                role: MembershipRole::Member,
                account: Some(aid(acct)),
                status: MembershipStatus::Active,
            },
            now(),
        )
        .unwrap();
    }
    s.create_allocation(
        &Allocation {
            id: AllocationId::new("pool-top").unwrap(),
            parent: aid("plat"),
            child: aid("pool"),
            ceiling: grant(9_000_000_000, Purpose::Prepaid),
            idempotency_key: None,
        },
        now(),
    )
    .unwrap();
}

/// 让 `THREADS` 个线程同时放行，各开各的连接，各跑一次 `f(线程序号, 连接)`。
fn race<T: Send + 'static>(
    db: &TempDb,
    f: impl Fn(usize, &AccountStore) -> T + Send + Sync + 'static,
) -> Vec<T> {
    let barrier = Arc::new(Barrier::new(THREADS));
    let f = Arc::new(f);
    let handles: Vec<_> = (0..THREADS)
        .map(|index| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            let f = Arc::clone(&f);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                f(index, &store)
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

fn subsidy_key() -> idoris_compute_contracts::ids::IdempotencyKey {
    subsidy_idempotency_key(&pid("p-sub"), BillingMonth::new(2026, 11).unwrap()).unwrap()
}

fn row_count(db: &TempDb, sql: &str) -> i64 {
    rusqlite::Connection::open(db.path())
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

#[test]
fn racing_identical_subsidy_grants_create_one_allocation_and_replay_the_rest() {
    let db = TempDb::new("identical");
    base(&db);
    let outcomes = race(&db, |index, store| {
        store
            .create_allocation(
                &Allocation {
                    // 每个重试的调用方各自生成一个新 id：幂等键才是这次发放的身份
                    id: AllocationId::new(format!("grant-{index}")).unwrap(),
                    parent: aid("pool"),
                    child: aid("kid-1"),
                    ceiling: grant(5_000_000, Purpose::FreeTier),
                    idempotency_key: Some(subsidy_key()),
                },
                now(),
            )
            .unwrap()
    });
    let created = outcomes
        .iter()
        .filter(|o| **o == AllocationOutcome::Created)
        .count();
    let replayed = outcomes
        .iter()
        .filter(|o| matches!(o, AllocationOutcome::Replayed(_)))
        .count();
    assert_eq!((created, replayed), (1, THREADS - 1), "{outcomes:?}");
    assert_eq!(
        row_count(
            &db,
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
    // 每个重放拿到的都是同一条已存在的额度
    let winners: Vec<_> = outcomes
        .iter()
        .filter_map(|o| match o {
            AllocationOutcome::Replayed(existing) => Some(existing.id.clone()),
            AllocationOutcome::Created => None,
        })
        .collect();
    assert!(
        winners.windows(2).all(|pair| pair[0] == pair[1]),
        "{winners:?}"
    );
}

#[test]
fn racing_subsidy_grants_with_different_amounts_let_exactly_one_win() {
    let db = TempDb::new("conflicting");
    base(&db);
    let results = race(&db, |index, store| {
        store.create_allocation(
            &Allocation {
                id: AllocationId::new(format!("grant-{index}")).unwrap(),
                parent: aid("pool"),
                child: aid("kid-1"),
                // 每个线程要的金额都不一样
                ceiling: grant(1_000_000 + i64::try_from(index).unwrap(), Purpose::FreeTier),
                idempotency_key: Some(subsidy_key()),
            },
            now(),
        )
    });
    let created = results
        .iter()
        .filter(|r| matches!(r, Ok(AllocationOutcome::Created)))
        .count();
    let duplicates = results
        .iter()
        .filter(|r| {
            matches!(
                r,
                Err(AccountStoreError::Allocation(
                    AllocationError::DuplicateIdempotencyKey(_)
                ))
            )
        })
        .count();
    assert_eq!((created, duplicates), (1, THREADS - 1), "{results:?}");
    assert_eq!(
        row_count(
            &db,
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
}

#[test]
fn a_principal_cannot_double_dip_through_two_member_accounts_even_when_racing() {
    let db = TempDb::new("double-dip");
    base(&db);
    let results = race(&db, |index, store| {
        // 偶数线程给 kid-1，奇数线程给 kid-2：同一个主体、同一个月，只能发一份
        let child = if index % 2 == 0 { "kid-1" } else { "kid-2" };
        store.create_allocation(
            &Allocation {
                id: AllocationId::new(format!("grant-{index}")).unwrap(),
                parent: aid("pool"),
                child: aid(child),
                ceiling: grant(5_000_000, Purpose::FreeTier),
                idempotency_key: Some(subsidy_key()),
            },
            now(),
        )
    });
    let created = results
        .iter()
        .filter(|r| matches!(r, Ok(AllocationOutcome::Created)))
        .count();
    assert_eq!(created, 1, "{results:?}");
    assert_eq!(
        row_count(
            &db,
            "SELECT count(*) FROM allocations WHERE purpose = 'free_tier'"
        ),
        1
    );
    // 其余要么是「同内容重放」（同一个子账户的线程），要么是冲突（另一个子账户的线程）；没有第二份被发出去
    for result in &results {
        match result {
            Ok(AllocationOutcome::Created | AllocationOutcome::Replayed(_)) => {}
            Err(AccountStoreError::Allocation(AllocationError::DuplicateIdempotencyKey(_))) => {}
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn racing_creation_of_the_same_account_id_makes_exactly_one() {
    let db = TempDb::new("same-id");
    base(&db);
    let results = race(&db, |_, store| {
        store.create_account(
            &account("contested", AccountKind::Member, Some("org")),
            now(),
        )
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let exists = results
        .iter()
        .filter(|r| {
            matches!(
                r,
                Err(AccountStoreError::AlreadyExists {
                    kind: "account",
                    ..
                })
            )
        })
        .count();
    assert_eq!((ok, exists), (1, THREADS - 1), "{results:?}");
}

#[test]
fn racing_platform_roots_leave_exactly_one_root() {
    let db = TempDb::new("roots");
    drop(AccountStore::open(db.path()).unwrap());
    let results = race(&db, |index, store| {
        store.create_account(
            &account(&format!("root-{index}"), AccountKind::Platform, None),
            now(),
        )
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let rejected = results
        .iter()
        .filter(|r| matches!(r, Err(AccountStoreError::Tree(TreeError::MultipleRoots))))
        .count();
    assert_eq!((ok, rejected), (1, THREADS - 1), "{results:?}");
    assert_eq!(
        row_count(&db, "SELECT count(*) FROM accounts WHERE kind = 'platform'"),
        1
    );
}

#[test]
fn racing_to_bind_one_payment_account_lets_exactly_one_membership_have_it() {
    let db = TempDb::new("bind");
    base(&db);
    {
        let s = AccountStore::open(db.path()).unwrap();
        s.create_account(
            &account("free-kid", AccountKind::Member, Some("org")),
            now(),
        )
        .unwrap();
        for index in 0..THREADS {
            s.ensure_principal(&pid(&format!("p-{index}")), now())
                .unwrap();
        }
    }
    let results = race(&db, |index, store| {
        store.create_membership(
            &Membership {
                id: MembershipId::new(format!("m-race-{index}")).unwrap(),
                principal: pid(&format!("p-{index}")),
                org: aid("org"),
                role: MembershipRole::Member,
                account: Some(aid("free-kid")),
                status: MembershipStatus::Active,
            },
            now(),
        )
    });
    let ok = results.iter().filter(|r| r.is_ok()).count();
    let bound = results
        .iter()
        .filter(|r| {
            matches!(
                r,
                Err(AccountStoreError::Directory(
                    DirectoryError::AccountAlreadyBound { .. }
                ))
            )
        })
        .count();
    assert_eq!((ok, bound), (1, THREADS - 1), "{results:?}");
    assert_eq!(
        row_count(
            &db,
            "SELECT count(*) FROM memberships WHERE account_id = 'free-kid'"
        ),
        1
    );
}

#[test]
fn racing_distinct_allocations_under_one_grantor_all_land_and_each_is_validated() {
    // 对照：没有争同一个名额的并发写入都能成功（锁只是串行化，不是互相否决）。
    let db = TempDb::new("distinct");
    base(&db);
    let results = race(&db, |index, store| {
        store.create_allocation(
            &Allocation {
                id: AllocationId::new(format!("top-up-{index}")).unwrap(),
                parent: aid("pool"),
                child: aid("kid-1"),
                ceiling: grant(1_000 + i64::try_from(index).unwrap(), Purpose::Prepaid),
                idempotency_key: None,
            },
            now(),
        )
    });
    assert!(
        results
            .iter()
            .all(|r| matches!(r, Ok(AllocationOutcome::Created))),
        "{results:?}"
    );
    assert_eq!(
        row_count(
            &db,
            "SELECT count(*) FROM allocations WHERE child_id = 'kid-1'"
        ),
        i64::try_from(THREADS).unwrap()
    );
}
