//! 测试里起的真进程（`idoris` 二进制）不能在测试失败时留下来占着端口。
//!
//! `std::process::Child` 被丢弃时不会杀子进程：断言失败、`wait_exit` 超时、任何 panic 展开之后，进程都还在，
//! 而且 `--launcher-stdin` 的实例不会因为 stdin 关闭而退出（那不代表 launcher 不在），于是一直占着端口。
//! `ChildGuard` 在 `Drop` 时（正常结束、断言失败、panic 展开都一样）：子进程还活着就 SIGKILL，再 `wait` 收尸，不留僵尸；
//! 错误一律忽略，`Drop` 里不 panic。已经退出并被 `wait`/`try_wait` 收过尸的子进程什么也不用做，所以有意 `kill -9`
//! 来造崩溃残留、再 `wait_exit` 的测试行为不变。
//!
//! 用法：`command.spawn_guarded()` 代替 `command.spawn()`；之后和 `Child` 一样用（`Deref`/`DerefMut`：`id()`、`try_wait()`、
//! `stdin`/`stdout`/`stderr` 字段，传给要 `&Child` 或 `&mut Child` 的辅助函数都行）。
//!
//! 整个测试进程被 SIGKILL 时（OOM、`kill -9`、变异驱动超时）`Drop` 不会跑，守卫管不到；那一层见各测试文件里
//! `orphan_timeout_s` 的说明。
#![allow(dead_code)]

use std::{
    io,
    ops::{Deref, DerefMut},
    process::{Child, Command, Output},
};

/// 被守卫的子进程；`Drop` 时若还活着就杀掉并收尸。
/// 内部用 `Option`：`wait_with_output` 取走子进程（它按值消费 `Child`），取走之后 `Drop` 什么也不做。
pub struct ChildGuard(Option<Child>);

impl ChildGuard {
    pub fn new(child: Child) -> Self {
        Self(Some(child))
    }

    /// 等它跑完并取走输出（`Child::wait_with_output` 的守卫版本）。`wait_with_output` 本身就收了尸，取走之后 `Drop` 不再杀。
    pub fn wait_with_output(mut self) -> io::Result<Output> {
        self.0
            .take()
            .expect("the child is only taken here")
            .wait_with_output()
    }
}

impl Deref for ChildGuard {
    type Target = Child;

    fn deref(&self) -> &Child {
        self.0.as_ref().expect("the child is only taken by value")
    }
}

impl DerefMut for ChildGuard {
    fn deref_mut(&mut self) -> &mut Child {
        self.0.as_mut().expect("the child is only taken by value")
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        // `try_wait` 给出 `Some`：已经退出并收过尸，不用再杀（std 的 `kill` 对已收尸的子进程也不会发信号，pid 可能已被复用）。
        // 其余（还活着，或状态查不出来）：SIGKILL；然后 `wait` 收尸。
        if !matches!(child.try_wait(), Ok(Some(_))) {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

/// `Command::spawn` 的守卫版本。
pub trait SpawnGuarded {
    fn spawn_guarded(&mut self) -> io::Result<ChildGuard>;
}

impl SpawnGuarded for Command {
    fn spawn_guarded(&mut self) -> io::Result<ChildGuard> {
        self.spawn().map(ChildGuard::new)
    }
}
