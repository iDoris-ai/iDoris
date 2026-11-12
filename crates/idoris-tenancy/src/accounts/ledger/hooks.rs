//! 变异测试钩子（只在 `mutation-test-hooks` 特性下编译，默认关闭）。
//!
//! 和 `budget::test_hooks` 同一个思路：用一个确定性的方法证明「检查和写预留不在同一个事务里」这一类退化会被
//! 并发测试抓住，而不是靠手改代码再加一个 `sleep` 把竞态窗口撑大。启用（[`arm`]）之后，
//! `AccountStore::admit_attempt` 在做完检查（`decide`）、写预留（`apply`）之前**提交**检查用的事务，所有参与的线程在
//! 屏障上会合——也就是所有线程都已经通过了自己的检查——再各自开一个**新**事务去写预留。这正是账本存在要防的
//! 非原子的先检查后扣款。屏障保证没有线程能「碰巧」在别人检查之前写完，所以超发是确定性的。
//!
//! 关闭特性时这个模块不存在，`admit_attempt` 里对应的分支编译掉，没有任何生产代价或风险。

use std::sync::{Barrier, OnceLock};

static BARRIER: OnceLock<Barrier> = OnceLock::new();

/// 为 `thread_count` 个参与线程启用钩子。每个线程必须恰好调用一次 `admit_attempt`，屏障才会放行。
/// 每个进程只能启用一次（它是进程全局状态，所以用它的测试单独放在一个测试二进制里）。
pub fn arm(thread_count: usize) {
    BARRIER
        .set(Barrier::new(thread_count))
        .unwrap_or_else(|_| panic!("ledger test_hooks::arm called more than once per process"));
}

/// 没启用就是 `None`：`admit_attempt` 完全跳过钩子。
pub(super) fn barrier() -> Option<&'static Barrier> {
    BARRIER.get()
}
