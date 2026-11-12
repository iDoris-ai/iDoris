//! 时钟：S0 的纯函数都显式收 `now: TimestampMs`，所以执行器里「现在」从这里取，测试可以拨。

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use idoris_compute_contracts::ids::TimestampMs;

/// 当前时间（UTC 毫秒）的来源。
pub trait Clock: Send + Sync {
    fn now(&self) -> TimestampMs;
}

/// 系统时钟。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> TimestampMs {
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
        TimestampMs(millis)
    }
}

/// 可以手动拨的时钟（测试用：许可表的核验过期是按「现在」判的）。
#[derive(Debug)]
pub struct ManualClock(AtomicI64);

impl ManualClock {
    pub fn new(now: TimestampMs) -> Self {
        Self(AtomicI64::new(now.0))
    }

    pub fn set(&self, now: TimestampMs) {
        self.0.store(now.0, Ordering::SeqCst);
    }

    pub fn advance_ms(&self, delta: i64) {
        self.0.fetch_add(delta, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> TimestampMs {
        TimestampMs(self.0.load(Ordering::SeqCst))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manual_clock_only_moves_when_told() {
        let clock = ManualClock::new(TimestampMs(1_000));
        assert_eq!(clock.now(), TimestampMs(1_000));
        clock.advance_ms(500);
        assert_eq!(clock.now(), TimestampMs(1_500));
        clock.set(TimestampMs(7));
        assert_eq!(clock.now(), TimestampMs(7));
    }

    #[test]
    fn the_system_clock_is_after_2026() {
        assert!(SystemClock.now().0 > 1_767_225_600_000); // 2026-01-01
    }
}
