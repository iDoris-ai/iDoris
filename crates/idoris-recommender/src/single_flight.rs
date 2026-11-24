//! 单飞（single-flight）：同一时刻只有一个调用者真的做事，期间到达的其他调用者等它做完、共用它的结果。
//! 不缓存：结果只给「做事期间到达的」调用者，下一个在空闲时到达的调用者会重新做。

use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

struct State<T> {
    running: bool,
    waiters: usize,
    generation: u64,
    last: Option<T>,
}

pub struct SingleFlight<T> {
    state: Mutex<State<T>>,
    done: Condvar,
}

impl<T> std::fmt::Debug for SingleFlight<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SingleFlight")
    }
}

impl<T> Default for SingleFlight<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(State {
                running: false,
                waiters: 0,
                generation: 0,
                last: None,
            }),
            done: Condvar::new(),
        }
    }
}

/// 领头者结束（含 panic）时收尾：记下结果、换代、叫醒等待者。
struct Finish<'a, T> {
    flight: &'a SingleFlight<T>,
    result: Option<T>,
}

impl<T> Drop for Finish<'_, T> {
    fn drop(&mut self) {
        let mut state = self
            .flight
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        state.running = false;
        state.generation = state.generation.wrapping_add(1);
        state.last = self.result.take();
        self.flight.done.notify_all();
    }
}

impl<T: Clone> SingleFlight<T> {
    fn lock(&self) -> MutexGuard<'_, State<T>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// 正在等领头者的调用者个数（测试用，让「并发」不靠睡眠）。
    pub fn waiters(&self) -> usize {
        self.lock().waiters
    }

    /// 没有进行中的计算就执行 `work` 并返回它的结果；有就等它做完、返回同一份结果的拷贝。领头者 panic 时，
    /// 等待者得到 `None`（领头者自己的 panic 照常向上传播）。
    pub fn run(&self, work: impl FnOnce() -> T) -> Option<T> {
        let mut state = self.lock();
        if state.running {
            state.waiters += 1;
            let generation = state.generation;
            while state.generation == generation {
                state = self
                    .done
                    .wait(state)
                    .unwrap_or_else(PoisonError::into_inner);
            }
            state.waiters -= 1;
            return state.last.clone();
        }
        state.running = true;
        drop(state);
        let mut finish = Finish {
            flight: self,
            result: None,
        };
        let value = work();
        finish.result = Some(value.clone());
        drop(finish);
        Some(value)
    }
}
