#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use idoris_recommender::single_flight::SingleFlight;

/// #609（Codex）：并发的调用者共用同一次进行中的计算——这里的「工作」只执行一次。领头者等到其余调用者都在排队
/// 之后才结束（用 `waiters()` 同步，不靠睡眠碰运气）。
#[test]
fn concurrent_callers_share_one_in_flight_computation() {
    const CALLERS: usize = 8;
    let flight = Arc::new(SingleFlight::<Arc<Vec<u32>>>::default());
    let runs = Arc::new(AtomicUsize::new(0));
    let handles: Vec<_> = (0..CALLERS)
        .map(|_| {
            let (flight, runs) = (flight.clone(), runs.clone());
            std::thread::spawn(move || {
                let inner = flight.clone();
                flight
                    .run(|| {
                        runs.fetch_add(1, Ordering::SeqCst);
                        let started = Instant::now();
                        while inner.waiters() < CALLERS - 1 {
                            assert!(
                                started.elapsed() < Duration::from_secs(20),
                                "waiters never arrived"
                            );
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Arc::new(vec![1, 2, 3])
                    })
                    .unwrap()
            })
        })
        .collect();
    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    assert_eq!(runs.load(Ordering::SeqCst), 1, "只遍历一次");
    assert!(results.iter().all(|r| **r == vec![1, 2, 3]));
    // 共用的是同一份结果，不是各算各的。
    assert!(results.iter().all(|r| Arc::ptr_eq(r, &results[0])));
}

/// 不缓存：空闲时再来的调用者重新做。
#[test]
fn a_caller_arriving_after_completion_runs_again() {
    let flight = SingleFlight::<u32>::default();
    let runs = AtomicUsize::new(0);
    for expected in 1..=3 {
        let value = flight
            .run(|| {
                runs.fetch_add(1, Ordering::SeqCst);
                7
            })
            .unwrap();
        assert_eq!(value, 7);
        assert_eq!(runs.load(Ordering::SeqCst), expected);
    }
}

/// 领头者 panic：等待者得到 `None` 而不是永远挂着，之后的调用照常。
#[test]
fn a_panicking_leader_releases_the_waiters() {
    let flight = Arc::new(SingleFlight::<u32>::default());
    let in_work = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let leader = {
        let (flight, in_work) = (flight.clone(), in_work.clone());
        std::thread::spawn(move || {
            let inner = flight.clone();
            let _ = flight.run(|| {
                in_work.store(true, Ordering::SeqCst);
                let started = Instant::now();
                while inner.waiters() < 1 {
                    assert!(started.elapsed() < Duration::from_secs(20));
                    std::thread::sleep(Duration::from_millis(1));
                }
                panic!("leader fails");
            });
        })
    };
    // 等领头者进入工作后再加入。
    while !in_work.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(1));
    }
    let waiter = {
        let flight = flight.clone();
        std::thread::spawn(move || flight.run(|| 99))
    };
    assert_eq!(waiter.join().unwrap(), None);
    assert!(leader.join().is_err());
    assert_eq!(flight.run(|| 5), Some(5));
}
