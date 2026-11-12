//! S1：真实的 key id 和模型 id 进入账本记录（取代路由里写死的 `default` 与 `provider_id`）之后，
//! 账本的根本性质不能变：
//!
//! - 预留和结算按**真实**的 key / 模型落账；
//! - 租户级的额度仍然是**一份**：换 key、换模型不会拿到新的额度，历史上记在旧占位符（`default`、模型等于供应商）
//!   下的已结算金额照样计入同一个租户总额；
//! - 多个线程各用不同的 key、不同的模型同时对同一个租户预留和结算：总额守恒、不超发。
//!
//! 第三条是这次改动特有的风险：以前所有请求共用一个 `(default, provider, provider)` 子范围，
//! 现在每个请求的范围都可能不一样，如果某处聚合不小心按 key 或模型切分，每个 key 就会各得一份额度。

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use idoris_tenancy::budget::{BudgetError, BudgetLedger, BudgetScope, Price, SpendGate};
use rusqlite::Connection;

struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        Self(std::env::temp_dir().join(format!(
            "idoris-real-identity-{tag}-{}-{}.sqlite3",
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

fn scalar(db: &TempDb, sql: &str) -> i64 {
    Connection::open(db.path())
        .unwrap()
        .query_row(sql, [], |r| r.get(0))
        .unwrap()
}

#[test]
fn a_reservation_and_its_settlement_are_recorded_under_the_real_key_and_model() {
    let db = TempDb::new("recorded");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    ledger
        .configure_tenant("acme", 10_000, "UTC", SpendGate::PaidOnly)
        .unwrap();
    let scope = BudgetScope::new(
        "acme",
        "vk_0123456789abcdef",
        "omlx",
        "mlx-community/Qwen3-4B-4bit",
    );
    let id = ledger.reserve(&scope, Price::Known(500)).unwrap();

    let conn = Connection::open(db.path()).unwrap();
    let (key, provider, model, status): (String, String, String, String) = conn
        .query_row(
            "SELECT key_id, provider_id, model_id, status FROM reservations WHERE id = ?1",
            [&id.0],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (
            key.as_str(),
            provider.as_str(),
            model.as_str(),
            status.as_str()
        ),
        (
            "vk_0123456789abcdef",
            "omlx",
            "mlx-community/Qwen3-4B-4bit",
            "active"
        )
    );

    // 结算不需要再传一遍 key 和模型：它按预留行上记下的真实值入账。
    ledger.settle("acme", &id, 400).unwrap();
    let spent: i64 = conn
        .query_row(
            "SELECT spent_minor FROM budget_periods \
             WHERE tenant_id='acme' AND key_id='vk_0123456789abcdef' AND provider_id='omlx' \
               AND model_id='mlx-community/Qwen3-4B-4bit'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(spent, 400);
    // 没有任何东西记到旧占位符下
    let legacy: i64 = conn
        .query_row(
            "SELECT count(*) FROM budget_periods WHERE key_id = 'default'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(legacy, 0);
}

#[test]
fn the_tenant_limit_is_one_pool_across_keys_models_and_historical_default_rows() {
    let db = TempDb::new("one-pool");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    ledger
        .configure_tenant("acme", 1_000, "UTC", SpendGate::PaidOnly)
        .unwrap();
    // 历史数据：旧代码把所有请求记在 `(default, provider, provider)` 下
    let legacy = BudgetScope::new("acme", "default", "omlx", "omlx");
    let id = ledger.reserve(&legacy, Price::Known(500)).unwrap();
    ledger.settle("acme", &id, 400).unwrap();
    let before = ledger.tenant_readview("acme").unwrap();
    assert_eq!((before.spent_minor, before.reserved_minor), (400, 0));

    // 换成真实的 key 和模型：历史上花掉的 400 仍然计入同一份租户额度
    let real = BudgetScope::new("acme", "vk_new_key", "omlx", "qwen3-4b");
    match ledger.reserve(&real, Price::Known(700)) {
        Err(BudgetError::Exceeded { balance_minor, .. }) => assert_eq!(balance_minor, 600),
        other => panic!("{other:?}"),
    }
    let ok = ledger.reserve(&real, Price::Known(600)).unwrap();
    // 另一个 key 的预留也占用同一份额度
    let another = BudgetScope::new("acme", "vk_other_key", "omlx", "other-model");
    assert!(matches!(
        ledger.reserve(&another, Price::Known(1)),
        Err(BudgetError::Exceeded { .. })
    ));
    ledger.settle("acme", &ok, 600).unwrap();
    let after = ledger.tenant_readview("acme").unwrap();
    assert_eq!(
        (
            after.spent_minor,
            after.reserved_minor,
            after.available_minor
        ),
        (1_000, 0, 0)
    );
    // 对照：历史行和新行各自完整地留在表里
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM budget_periods WHERE tenant_id='acme'"
        ),
        2
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT sum(spent_minor) FROM budget_periods WHERE tenant_id='acme'"
        ),
        scalar(
            &db,
            "SELECT spent_minor FROM tenant_periods WHERE tenant_id='acme'"
        )
    );
}

const THREADS: usize = 16;
const PER_RESERVE: i64 = 100;
const LIMIT: i64 = 1_000;

/// 每个线程一条自己的连接（靠 SQLite 的锁串行化，不是进程内的互斥）、一个自己的 key 和一个自己的模型，
/// 屏障放行后对同一个租户先预留再结算。`settle_to` 是结算金额。返回每个线程的预留结果。
fn run_threads(db: &TempDb, tenant: &'static str, settle_to: i64) -> Vec<Result<(), BudgetError>> {
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|index| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let ledger = BudgetLedger::open(&path).unwrap();
                let scope = BudgetScope::new(
                    tenant,
                    format!("vk_thread_{index:02}"),
                    "omlx",
                    format!("model-{index:02}"),
                );
                barrier.wait();
                match ledger.reserve(&scope, Price::Known(PER_RESERVE)) {
                    Ok(id) => {
                        ledger.settle(tenant, &id, settle_to).unwrap();
                        Ok(())
                    }
                    Err(error) => Err(error),
                }
            })
        })
        .collect();
    handles.into_iter().map(|h| h.join().unwrap()).collect()
}

#[test]
fn concurrent_reserve_and_settle_under_distinct_keys_never_overspend_the_one_tenant_limit() {
    let db = TempDb::new("conserve");
    {
        let ledger = BudgetLedger::open(db.path()).unwrap();
        ledger
            .configure_tenant("acme", LIMIT, "UTC", SpendGate::PaidOnly)
            .unwrap();
    }
    // 监视线程：任何时刻 `reserved + spent` 都不能超过限额
    let stop = Arc::new(AtomicBool::new(false));
    let monitor = {
        let path = db.path().to_path_buf();
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let ledger = BudgetLedger::open(&path).unwrap();
            let mut samples = 0_u32;
            while !stop.load(Ordering::SeqCst) {
                let view = ledger.tenant_readview("acme").unwrap();
                assert!(
                    view.reserved_minor + view.spent_minor <= LIMIT,
                    "in flight: reserved {} + spent {} > limit {LIMIT}",
                    view.reserved_minor,
                    view.spent_minor
                );
                samples += 1;
            }
            samples
        })
    };

    // 结算金额等于预留金额：结算不释放额度，所以不论怎么交错，恰好 LIMIT / PER_RESERVE 个预留成功。
    let results = run_threads(&db, "acme", PER_RESERVE);
    stop.store(true, Ordering::SeqCst);
    let samples = monitor.join().unwrap();
    assert!(samples > 0);

    let ok = results.iter().filter(|r| r.is_ok()).count();
    let exceeded = results
        .iter()
        .filter(|r| matches!(r, Err(BudgetError::Exceeded { .. })))
        .count();
    let expected = usize::try_from(LIMIT / PER_RESERVE).unwrap();
    assert_eq!(
        (ok, exceeded),
        (expected, THREADS - expected),
        "{results:?}"
    );
    for result in &results {
        if let Err(error) = result {
            assert!(
                matches!(error, BudgetError::Exceeded { .. }),
                "unexpected error: {error}"
            );
        }
    }

    // 总额守恒：租户级已结算 == 各个 (key, 模型) 子范围已结算之和 == 成功次数 * 单价，没有在途
    let ledger = BudgetLedger::open(db.path()).unwrap();
    let view = ledger.tenant_readview("acme").unwrap();
    assert_eq!(
        (view.spent_minor, view.reserved_minor, view.available_minor),
        (LIMIT, 0, 0)
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT sum(spent_minor) FROM budget_periods WHERE tenant_id='acme'"
        ),
        LIMIT
    );
    // 每个成功的线程都记在自己真实的 key 和模型下
    assert_eq!(
        scalar(
            &db,
            "SELECT count(DISTINCT key_id) FROM reservations WHERE tenant_id='acme' AND status='settled'"
        ),
        i64::try_from(expected).unwrap()
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM reservations WHERE tenant_id='acme' AND key_id NOT LIKE 'vk_thread_%'"
        ),
        0
    );
    assert_eq!(
        scalar(
            &db,
            "SELECT count(*) FROM reservations WHERE tenant_id='acme' AND model_id = provider_id"
        ),
        0,
        "no row may fall back to the provider id as its model"
    );
}

#[test]
fn concurrent_settles_that_release_headroom_conserve_the_total() {
    // 结算金额小于预留：先结算的线程会释放额度，后面的线程可能因此预留成功。成功多少取决于调度，
    // 但不变量不变：已结算总额 == 成功次数 * 实际单价，且不超过限额，没有残留的在途。
    const ACTUAL: i64 = 60;
    let db = TempDb::new("headroom");
    {
        let ledger = BudgetLedger::open(db.path()).unwrap();
        ledger
            .configure_tenant("acme", LIMIT, "UTC", SpendGate::PaidOnly)
            .unwrap();
    }
    let results = run_threads(&db, "acme", ACTUAL);
    let ok = i64::try_from(results.iter().filter(|r| r.is_ok()).count()).unwrap();
    assert!(
        ok >= LIMIT / PER_RESERVE,
        "at least the reservations that fit while all are in flight"
    );
    let ledger = BudgetLedger::open(db.path()).unwrap();
    let view = ledger.tenant_readview("acme").unwrap();
    assert_eq!(view.spent_minor, ok * ACTUAL);
    assert_eq!(view.reserved_minor, 0);
    assert!(view.spent_minor <= LIMIT);
    assert_eq!(
        scalar(
            &db,
            "SELECT sum(spent_minor) FROM budget_periods WHERE tenant_id='acme'"
        ),
        view.spent_minor
    );
}

#[test]
fn two_tenants_sharing_keys_and_models_never_share_a_limit() {
    // 真实 key / 模型进来以后，两个租户可能碰巧用同一个模型名：额度仍然各算各的。
    let db = TempDb::new("tenants");
    let ledger = BudgetLedger::open(db.path()).unwrap();
    ledger
        .configure_tenant("acme", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    ledger
        .configure_tenant("beta", 100, "UTC", SpendGate::PaidOnly)
        .unwrap();
    let a = BudgetScope::new("acme", "vk_same", "omlx", "same-model");
    let b = BudgetScope::new("beta", "vk_same", "omlx", "same-model");
    let id = ledger.reserve(&a, Price::Known(100)).unwrap();
    ledger.settle("acme", &id, 100).unwrap();
    assert!(matches!(
        ledger.reserve(&a, Price::Known(1)),
        Err(BudgetError::Exceeded { .. })
    ));
    ledger.reserve(&b, Price::Known(100)).unwrap();
}
