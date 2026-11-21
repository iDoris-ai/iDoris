//! `support/child_guard.rs`：测试里起的真进程在测试失败（panic 展开）时不会留下来。
//!
//! 这里不起 `idoris`，起一个无害的长命进程（`sleep`），只验证守卫本身：`Drop` 在展开时杀掉子进程并收尸，
//! 收完之后那个 pid 在系统里已经不存在（`kill(pid, 0)` 给 `ESRCH`），而不只是「发过 SIGKILL」。
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

#[path = "support/child_guard.rs"]
mod child_guard;

use std::{
    os::unix::process::ExitStatusExt,
    panic::{AssertUnwindSafe, catch_unwind},
    process::Command,
    sync::mpsc,
    time::Duration,
};

use child_guard::SpawnGuarded;
use nix::{
    errno::Errno,
    sys::signal::{Signal, kill},
    unistd::Pid,
};

const WITHIN: Duration = Duration::from_secs(10);

/// 守卫里的子进程所在的线程 panic：展开时守卫的 `Drop` 杀掉并收走子进程，`kill(pid, 0)` 给 `ESRCH`。
/// 「已发 SIGKILL 但没收尸」（僵尸还在，`kill(pid, 0)` 成功）与「只 `wait` 不杀」（`Drop` 一直卡着，等 `sleep` 自己结束）
/// 都不算：前者断言失败，后者在限时之内等不到展开结束。
#[test]
fn a_test_that_panics_does_not_leave_its_child_running() {
    let (pid_sender, pid_receiver) = mpsc::channel::<i32>();
    let (unwound_sender, unwound_receiver) = mpsc::channel::<bool>();
    let worker = std::thread::spawn(move || {
        let panicked = catch_unwind(AssertUnwindSafe(|| {
            let child = Command::new("sleep").arg("3600").spawn_guarded().unwrap();
            pid_sender.send(child.id() as i32).unwrap();
            panic!("the test fails while its child is running");
        }))
        .is_err();
        unwound_sender.send(panicked).unwrap();
    });

    let pid = Pid::from_raw(
        pid_receiver
            .recv_timeout(WITHIN)
            .expect("the child started"),
    );
    let unwound = unwound_receiver.recv_timeout(WITHIN);
    let probe = kill(pid, None);
    if probe.is_ok() {
        // 守卫失灵时别在机器上留下一个 `sleep 3600`（也让卡在 `wait` 里的线程能结束）。
        let _ = kill(pid, Signal::SIGKILL);
    }
    assert_eq!(
        unwound,
        Ok(true),
        "the guard's drop must return promptly during the unwind"
    );
    assert_eq!(
        probe,
        Err(Errno::ESRCH),
        "the child must be gone (killed and reaped) once the panic has unwound"
    );
    worker.join().unwrap();
}

/// 有意 `kill -9` 来造崩溃残留的测试：自己杀、自己 `wait` 之后，守卫的 `Drop` 什么也不做，也不出错；
/// 守卫对 `Child` 的方法与字段是透明的。
#[test]
fn a_child_that_was_killed_and_waited_by_the_test_is_left_alone_by_the_drop() {
    let mut child = Command::new("sleep").arg("3600").spawn_guarded().unwrap();
    let pid = Pid::from_raw(child.id() as i32);
    assert_eq!(child.try_wait().unwrap(), None, "still running");
    child.kill().unwrap();
    let status = child.wait().unwrap();
    assert_eq!(status.signal(), Some(9));
    drop(child);
    assert_eq!(kill(pid, None), Err(Errno::ESRCH));
}
