//! 时钟：S0 的纯函数都显式收 `now: TimestampMs`，所以执行器里「现在」从这里取，测试可以拨。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use idoris_compute_contracts::ids::TimestampMs;

/// 当前时间（UTC 毫秒）的来源。
pub trait Clock: Send + Sync {
    fn now(&self) -> TimestampMs;

    /// **单调**时间（毫秒，起点任意）：只用来量时间差（限流的令牌桶、失败计数的窗口）。墙上时钟会被 NTP 校时拨来拨去，
    /// 限流不该受它影响：拨回去会让桶以为时间倒流，拨快了会凭空补出额度。默认和 [`Clock::now`] 一样
    /// （可以拨的测试时钟：拨一下，两个读数一起动）；[`SystemClock`] 用进程内的单调时钟。
    fn monotonic_ms(&self) -> i64 {
        self.now().0
    }
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

    fn monotonic_ms(&self) -> i64 {
        static START: OnceLock<Instant> = OnceLock::new();
        let elapsed = START.get_or_init(Instant::now).elapsed();
        i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX)
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

    #[test]
    fn the_monotonic_reading_never_goes_backwards_and_the_manual_clock_moves_both_together() {
        let mut last = SystemClock.monotonic_ms();
        for _ in 0..1_000 {
            let now = SystemClock.monotonic_ms();
            assert!(now >= last);
            last = now;
        }
        let clock = ManualClock::new(TimestampMs(1_000));
        assert_eq!(clock.monotonic_ms(), 1_000);
        clock.advance_ms(250);
        assert_eq!(clock.monotonic_ms(), 1_250);
    }
}
