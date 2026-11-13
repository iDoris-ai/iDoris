//! 同步的账本 / 尝试记录操作（SQLite）在异步上下文里怎么跑。
//!
//! 准入、写状态、结算都是同步的 SQLite 事务：数据库配了 5 秒的 busy timeout，库的连接又在一把 `Mutex` 后面。
//! 直接在异步任务里调用，另一个连接持有写事务（或者多条请求同时进入准入 / 收尾）时，运行时的 worker 线程就阻塞在
//! SQLite 或互斥锁上：健康检查、网络读写、取消计时、别的请求的处理都排不上（R2 Medium#2）。
//!
//! [`blocking`] 在多线程运行时里用 `tokio::task::block_in_place`：当前 worker 线程专心跑这段同步代码，
//! 运行时把这个线程上排着的其它任务挪给别的线程。它保持原来的借用关系（`&mut RunState`、`&self` 都不用搬进
//! `'static` 的闭包），也不改变取消语义：事务一旦开始就跑完（`block_in_place` 里的代码不会被取消），所以「先落库再写字节」、
//! 收尾恰好一次、`Drop` 里的收尾这些保证原样成立。
//!
//! 单线程运行时（`#[tokio::test]` 默认）和没有运行时的线程（`spawn_blocking` 线程、同步测试）直接跑：`block_in_place` 在
//! 单线程运行时里会 panic，在运行时之外没有意义。
//!
//! **范围**：执行器的准入、尝试记录（`run`）、收尾，和处理函数的发布 / 拒绝。儿童安全检查的成本计量
//! （`safety::LedgerExtraCost`，也是同步的账本操作）不在这里：它在 `safety/` 里。

use tokio::runtime::{Handle, RuntimeFlavor};

/// 跑一段同步的、可能阻塞的代码（SQLite 事务、互斥锁）而不饿死异步运行时。见模块文档。
pub(crate) fn blocking<R>(work: impl FnOnce() -> R) -> R {
    match Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use super::*;

    #[test]
    fn it_runs_the_work_without_a_runtime_and_on_a_current_thread_runtime() {
        assert_eq!(blocking(|| 7), 7);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        assert_eq!(runtime.block_on(async { blocking(|| 8) }), 8);
    }

    /// 只有一个 worker 线程的多线程运行时：一段阻塞 300 ms 的同步代码在别的任务旁边跑。
    /// 不用 `blocking`，worker 被占死，旁边的心跳任务在这 300 ms 里一次都跑不了；用了，心跳照常走。
    #[test]
    fn a_slow_synchronous_section_does_not_starve_the_other_tasks_of_the_runtime() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_time()
            .build()
            .unwrap();
        let beats = Arc::new(AtomicUsize::new(0));
        runtime.block_on(async {
            let heartbeat = {
                let beats = Arc::clone(&beats);
                tokio::spawn(async move {
                    loop {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                        beats.fetch_add(1, Ordering::SeqCst);
                    }
                })
            };
            // 让心跳先跑起来
            tokio::time::sleep(Duration::from_millis(60)).await;
            let before = beats.load(Ordering::SeqCst);
            let worker = tokio::spawn(async {
                blocking(|| std::thread::sleep(Duration::from_millis(300)));
            });
            worker.await.unwrap();
            let during = beats.load(Ordering::SeqCst) - before;
            heartbeat.abort();
            assert!(
                during >= 10,
                "the heartbeat kept running while the synchronous section blocked: {during} beats"
            );
        });
    }
}
