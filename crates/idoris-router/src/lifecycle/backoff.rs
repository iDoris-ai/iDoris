//! 引擎管理器的时间参数、重启退避与熔断窗口（接口规范 §2.7.6「状态机、退避与就绪探测」）。纯函数与纯数据，
//! 不碰进程、不碰网络，所以能用确定的时间逐条测试。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::register::JitterSource;

/// 引擎管理器的全部时间参数。生产用 [`Timings::default`]（规范给的数值）；测试缩短它们。
#[derive(Debug, Clone)]
pub struct Timings {
    /// 等一个引擎就绪的上限（规范 §2.7.6：oMLX 默认 90 秒）。
    pub startup_timeout: Duration,
    /// 等就绪时，探测间隔从这个值起，每轮翻倍，封顶 [`Timings::ready_max_interval`]（250 毫秒到 1 秒）。
    pub ready_initial_interval: Duration,
    pub ready_max_interval: Duration,
    /// 一次探测请求的总时限。
    pub probe_timeout: Duration,
    /// 就绪之后对引擎的周期探测间隔（与 `/health.upstreams` 的探测器一致：5 秒）。
    pub probe_interval: Duration,
    /// 就绪之后看自有引擎进程还在不在的间隔（便宜：自己的子进程只是一次 `waitid`）。进程退出在这个间隔里就被发现，
    /// 不必等下一次探测：退出码 5 的停止越早越好。
    pub liveness_interval: Duration,
    /// 重启退避：第 `n` 次失败等 `restart_backoff_base × 2^(n-1)`，封顶 `restart_backoff_cap`，±`restart_jitter`。
    pub restart_backoff_base: Duration,
    pub restart_backoff_cap: Duration,
    pub restart_jitter: f64,
    pub jitter: JitterSource,
    /// 熔断：`crash_window` 内失败超过 `crash_limit` 次就是 `failed` + `crash_loop`。
    pub crash_window: Duration,
    pub crash_limit: usize,
    /// `failed` 之后每隔这么久半开重试一次。
    pub half_open_interval: Duration,
    /// 停止自有引擎的宽限：SIGTERM 之后等整组消失，到点还在就 SIGKILL（规范：8 秒）。
    pub stop_grace: Duration,
    /// 排空的上限（IDORIS-CORE S6）：`stop`、`restart`（以及 `start` 一个还活着却不答的引擎）先关 Supervisor 的门，再等已经在
    /// 进行的加载、聊天做完，最多等这么久；到点还没做完就切断旧一代的凭证、照样停进程（它们之后发的每个请求都被拒绝）。
    pub drain_timeout: Duration,
    /// 身份读不出来的记录（`Recovery::Unverifiable`）：重读几次、每次隔多久，之后按失败处理。
    pub unverifiable_retries: u32,
    pub unverifiable_pause: Duration,
}

impl Default for Timings {
    fn default() -> Self {
        Self {
            startup_timeout: Duration::from_secs(90),
            ready_initial_interval: Duration::from_millis(250),
            ready_max_interval: Duration::from_secs(1),
            probe_timeout: Duration::from_secs(2),
            probe_interval: Duration::from_secs(5),
            liveness_interval: Duration::from_secs(1),
            restart_backoff_base: Duration::from_secs(1),
            restart_backoff_cap: Duration::from_secs(30),
            restart_jitter: 0.2,
            jitter: JitterSource::System,
            crash_window: Duration::from_secs(5 * 60),
            crash_limit: 5,
            half_open_interval: Duration::from_secs(60),
            stop_grace: Duration::from_secs(8),
            drain_timeout: Duration::from_secs(30),
            unverifiable_retries: 3,
            unverifiable_pause: Duration::from_millis(500),
        }
    }
}

/// 第 `failures`（从 1 起）次失败之后、重启之前的等待：`base × 2^(failures-1)`，封顶 `cap`，再抖动。
/// 规范：1、2、4 秒起翻倍，封顶 30 秒，±20%。`failures` 为 0 时不等。
pub fn restart_delay(failures: usize, timings: &Timings) -> Duration {
    if failures == 0 {
        return Duration::ZERO;
    }
    let doublings = u32::try_from(failures - 1).unwrap_or(u32::MAX).min(32);
    let raw = timings
        .restart_backoff_base
        .saturating_mul(1u32.checked_shl(doublings).unwrap_or(u32::MAX));
    timings
        .jitter
        .apply(raw.min(timings.restart_backoff_cap), timings.restart_jitter)
}

/// 滑动的失败窗口：`window` 内失败次数超过 `limit` 就是熔断。窗口按**时间**滑动（过期的失败不再计数），
/// 成功不清零（5 分钟内崩了又好、好了又崩，照样累加）；只有操作员显式 `start`、`restart` 才清空。
#[derive(Debug, Clone)]
pub struct CrashWindow {
    window: Duration,
    limit: usize,
    failures: VecDeque<Instant>,
}

impl CrashWindow {
    pub fn new(window: Duration, limit: usize) -> Self {
        Self {
            window,
            limit,
            failures: VecDeque::new(),
        }
    }

    fn expire(&mut self, now: Instant) {
        while let Some(oldest) = self.failures.front() {
            if now.saturating_duration_since(*oldest) >= self.window {
                self.failures.pop_front();
            } else {
                break;
            }
        }
    }

    /// 记一次失败，返回窗口内的失败次数（含这一次）。
    pub fn record(&mut self, now: Instant) -> usize {
        self.expire(now);
        self.failures.push_back(now);
        self.failures.len()
    }

    /// `now` 时窗口内的失败次数。
    pub fn count(&mut self, now: Instant) -> usize {
        self.expire(now);
        self.failures.len()
    }

    /// 失败次数超过上限（`limit` 次以内还是 `restarting`，**超过**才是 `failed`）。
    pub fn tripped(&mut self, now: Instant) -> bool {
        self.count(now) > self.limit
    }

    pub fn clear(&mut self) {
        self.failures.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact() -> Timings {
        Timings {
            jitter: JitterSource::None,
            ..Timings::default()
        }
    }

    #[test]
    fn the_defaults_are_the_specs_numbers() {
        let timings = Timings::default();
        assert_eq!(timings.startup_timeout, Duration::from_secs(90));
        assert_eq!(timings.ready_initial_interval, Duration::from_millis(250));
        assert_eq!(timings.ready_max_interval, Duration::from_secs(1));
        assert_eq!(timings.probe_interval, Duration::from_secs(5));
        assert_eq!(timings.liveness_interval, Duration::from_secs(1));
        assert_eq!(timings.restart_backoff_base, Duration::from_secs(1));
        assert_eq!(timings.restart_backoff_cap, Duration::from_secs(30));
        assert!((timings.restart_jitter - 0.2).abs() < f64::EPSILON);
        assert_eq!(timings.crash_window, Duration::from_secs(300));
        assert_eq!(timings.crash_limit, 5);
        assert_eq!(timings.half_open_interval, Duration::from_secs(60));
        assert_eq!(timings.stop_grace, Duration::from_secs(8));
    }

    #[test]
    fn the_backoff_doubles_from_one_second_and_stops_at_thirty() {
        let timings = exact();
        let seconds: Vec<u64> = (0..=9)
            .map(|failures| restart_delay(failures, &timings).as_secs())
            .collect();
        assert_eq!(seconds, [0, 1, 2, 4, 8, 16, 30, 30, 30, 30]);
        // The cap holds for absurd counts too (no overflow, no wrap to a short wait).
        assert_eq!(restart_delay(usize::MAX, &timings), Duration::from_secs(30));
        assert_eq!(restart_delay(64, &timings), Duration::from_secs(30));
    }

    #[test]
    fn jitter_stays_within_twenty_percent_of_the_exact_delay() {
        let timings = Timings::default();
        for failures in 1..=8 {
            let exact_delay = restart_delay(failures, &exact());
            for _ in 0..50 {
                let jittered = restart_delay(failures, &timings);
                assert!(
                    jittered >= exact_delay.mul_f64(0.8 - 1e-9)
                        && jittered <= exact_delay.mul_f64(1.2 + 1e-9),
                    "{failures}: {jittered:?} vs {exact_delay:?}"
                );
            }
        }
    }

    #[test]
    fn more_than_the_limit_inside_the_window_trips_and_exactly_the_limit_does_not() {
        let start = Instant::now();
        let mut window = CrashWindow::new(Duration::from_secs(300), 5);
        for index in 0..5 {
            assert_eq!(
                window.record(start + Duration::from_secs(index)),
                index as usize + 1
            );
            assert!(
                !window.tripped(start + Duration::from_secs(index)),
                "{index}"
            );
        }
        // The sixth failure inside five minutes is "more than 5".
        assert_eq!(window.record(start + Duration::from_secs(5)), 6);
        assert!(window.tripped(start + Duration::from_secs(5)));
    }

    #[test]
    fn failures_older_than_the_window_stop_counting() {
        let start = Instant::now();
        let mut window = CrashWindow::new(Duration::from_secs(300), 2);
        window.record(start);
        window.record(start + Duration::from_secs(10));
        window.record(start + Duration::from_secs(20));
        assert!(window.tripped(start + Duration::from_secs(20)));
        // At exactly `window` after a failure, that failure is out; just before, it is still in.
        assert_eq!(window.count(start + Duration::from_secs(299)), 3);
        assert_eq!(window.count(start + Duration::from_secs(300)), 2);
        assert!(!window.tripped(start + Duration::from_secs(300)));
        assert_eq!(window.count(start + Duration::from_secs(310)), 1);
        assert_eq!(window.count(start + Duration::from_secs(400)), 0);
        // A new failure after a long quiet spell starts from one, not from the old total.
        assert_eq!(window.record(start + Duration::from_secs(1000)), 1);
    }

    #[test]
    fn clearing_forgets_every_failure() {
        let start = Instant::now();
        let mut window = CrashWindow::new(Duration::from_secs(300), 1);
        window.record(start);
        window.record(start);
        assert!(window.tripped(start));
        window.clear();
        assert_eq!(window.count(start), 0);
        assert!(!window.tripped(start));
    }
}
