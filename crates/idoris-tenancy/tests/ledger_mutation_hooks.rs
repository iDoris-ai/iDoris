//! 证明 `ledger_concurrency.rs` 里的并发测试**确实抓得住**「检查和写预留不在同一个事务里」这一类退化。
//!
//! 与 `budget_mutation_hooks.rs` 同一个思路：不靠手改代码、加 `sleep` 把竞态窗口撑大，而是用特性
//! `mutation-test-hooks` 下的一个确定性钩子（`accounts::ledger::hooks`）在检查和写预留之间提交检查事务，
//! 所有线程在屏障上会合（即都通过了自己的检查）后各开新事务写预留。屏障保证没有线程能「碰巧」在别人检查之前写完，
//! 所以超发是确定性的：线程数个预留全部放行，容量只有 3 个。
//!
//! 钩子是进程全局状态，所以这个测试单独放在一个测试二进制里（`Cargo.toml` 里 `required-features`），
//! 不会污染别的并发测试。特性关闭时 `admit_attempt` 里对应的分支编译掉，生产代码没有任何代价。
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/ledger_fixture.rs"]
mod fixture;

use std::sync::{Arc, Barrier};
use std::thread;

use fixture::*;
use idoris_tenancy::accounts::AccountStore;
use idoris_tenancy::accounts::ledger::hooks;

const THREADS: usize = 12;
const R: i64 = 210;

#[test]
fn splitting_the_check_from_the_reservation_over_issues_deterministically() {
    let db = TempDb::new("mutation-split-tx");
    let store = world(&db);
    // 容量：kid-a 的额度只有 3 个预留；其余祖先充足
    allocate_chain(&store, "x", &["pool", "school", "teacher"], 1_000_000_000);
    allocate(&store, "a-own", "teacher", "kid-a", 3 * R);
    store
        .set_pool_cap(&aid("pool"), month(2026, 11), &thb(1_000_000_000), t0())
        .unwrap();
    let requests: Vec<Req> = (0..THREADS)
        .map(|i| Req::new(&store, &format!("race-{i}"), 0, "cred-a", 100))
        .collect();

    hooks::arm(THREADS);
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = requests
        .into_iter()
        .map(|request| {
            let path = db.path().to_path_buf();
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                let store = AccountStore::open(&path).unwrap();
                barrier.wait();
                store.admit_attempt(&request.input()).unwrap()
            })
        })
        .collect();
    let admitted = handles
        .into_iter()
        .map(|h| h.join().unwrap())
        .filter(|d| d.is_admitted())
        .count();

    // 有钩子：每个线程都在别人写预留之前通过了检查，12 个全放行——远超容量 3。这就是 `ledger_concurrency.rs`
    // 要防住的超发；没有钩子（正常路径）时那里的测试断言恰好 3 个。
    assert_eq!(
        admitted, THREADS,
        "the split transaction must over-admit every racer"
    );
    assert!(admitted > 3);
    let remaining = store
        .account_view(&aid("kid-a"), t0())
        .unwrap()
        .ceiling_remaining
        .unwrap();
    assert!(
        remaining.micros < 0,
        "the allowance is overdrawn: {remaining:?}"
    );
}
