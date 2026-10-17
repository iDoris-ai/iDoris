//! M5 三粒度弹性（总体规划 §4.4、B-8）的纯状态机：provider 断路器
//! （CLOSED → OPEN → HALF_OPEN）、connection 冷却（遵守 `Retry-After`）、
//! 单模型锁定。**没有 IO、没有时钟**：调用方传入 `now_ms`，便于确定性测试。
//!
//! 三个粒度用同一个状态机、不同的键：provider id、connection（端点）、
//! `(provider, model)`；[`BreakerBook`] 按键分开保存。
//!
//! 默认参数取自 B-8：OAuth（订阅类）连续 3 次失败、API Key 5 次、本地 2 次
//! 即断开；恢复窗口分别 60s / 30s / 15s。窗口过后进入 HALF_OPEN，只放一个
//! 探测请求：成功则闭合，失败则以新的完整窗口重新断开。

use std::collections::HashMap;
use std::hash::Hash;

/// 决定断路器参数的凭据类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CredentialClass {
    OAuth,
    ApiKey,
    Local,
}

impl CredentialClass {
    pub const fn failure_threshold(self) -> u32 {
        match self {
            Self::OAuth => 3,
            Self::ApiKey => 5,
            Self::Local => 2,
        }
    }

    pub const fn recovery_ms(self) -> u64 {
        match self {
            Self::OAuth => 60_000,
            Self::ApiKey => 30_000,
            Self::Local => 15_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Closed {
        failures: u32,
    },
    Open {
        until_ms: u64,
    },
    /// 探测在途；它从 `since_ms` 开始。超过一个恢复窗口仍未回报的探测
    /// 视为丢失（请求被取消、本地 503 之类），允许再放一个探测，否则这个
    /// provider 会被永远卡在拒绝状态。
    HalfOpen {
        since_ms: u64,
    },
}

/// 一次准入判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    /// HALF_OPEN 下唯一放行的探测请求；结果必须回报。
    Probe,
    /// 断开中；`retry_after_ms` 是距离可再试的剩余时间（探测在途时为 0）。
    Rejected {
        retry_after_ms: u64,
    },
}

impl Admission {
    pub const fn admitted(self) -> bool {
        !matches!(self, Self::Rejected { .. })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Breaker {
    class: CredentialClass,
    state: State,
}

impl Breaker {
    pub const fn new(class: CredentialClass) -> Self {
        Self {
            class,
            state: State::Closed { failures: 0 },
        }
    }

    pub fn admit(&mut self, now_ms: u64) -> Admission {
        match self.state {
            State::Closed { .. } => Admission::Allowed,
            State::Open { until_ms } if now_ms < until_ms => Admission::Rejected {
                retry_after_ms: until_ms - now_ms,
            },
            State::HalfOpen { since_ms }
                if now_ms < since_ms.saturating_add(self.class.recovery_ms()) =>
            {
                Admission::Rejected { retry_after_ms: 0 }
            }
            State::Open { .. } | State::HalfOpen { .. } => {
                self.state = State::HalfOpen { since_ms: now_ms };
                Admission::Probe
            }
        }
    }

    /// What [`Self::admit`] would answer, without claiming a probe — for
    /// steering selection away from a provider before choosing one.
    pub fn would_admit(&self, now_ms: u64) -> bool {
        match self.state {
            State::Closed { .. } => true,
            State::Open { until_ms } => now_ms >= until_ms,
            State::HalfOpen { since_ms } => {
                now_ms >= since_ms.saturating_add(self.class.recovery_ms())
            }
        }
    }

    pub fn on_success(&mut self) {
        self.state = State::Closed { failures: 0 };
    }

    pub fn on_failure(&mut self, now_ms: u64) {
        self.state = match self.state {
            State::Closed { failures } if failures + 1 < self.class.failure_threshold() => {
                State::Closed {
                    failures: failures + 1,
                }
            }
            // A failure while the breaker is already open must not keep an
            // expired deadline: the Supervisor path never claims `admit`, so
            // nothing moves an expired Open to HalfOpen there. Keeping the old
            // `until_ms` would admit the next request immediately, i.e. a model
            // that keeps failing would be locked out for exactly one window.
            // A still-valid window keeps its deadline; an expired one reopens.
            State::Open { until_ms } if now_ms < until_ms => State::Open { until_ms },
            State::Open { .. } => State::Open {
                until_ms: now_ms.saturating_add(self.class.recovery_ms()),
            },
            State::Closed { .. } | State::HalfOpen { .. } => State::Open {
                until_ms: now_ms.saturating_add(self.class.recovery_ms()),
            },
        };
    }

    /// 上游明确给出 `Retry-After`：至少断开到那一刻（不会缩短已有窗口）。
    pub fn on_retry_after(&mut self, now_ms: u64, retry_after_ms: u64) {
        let until_ms = now_ms.saturating_add(retry_after_ms);
        self.state = match self.state {
            State::Open { until_ms: existing } => State::Open {
                until_ms: existing.max(until_ms),
            },
            _ => State::Open { until_ms },
        };
    }
}

/// `Retry-After` 的整数秒形式；HTTP-date 形式与非法值返回 `None`，调用方
/// 退回到凭据类别的恢复窗口。
pub fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    raw.trim().parse::<u64>().ok()?.checked_mul(1000)
}

/// 按键（provider / connection / model）分开的一组断路器。
#[derive(Debug, Default)]
pub struct BreakerBook<K: Eq + Hash> {
    breakers: HashMap<K, Breaker>,
}

impl<K: Eq + Hash + Clone> BreakerBook<K> {
    pub fn new() -> Self {
        Self {
            breakers: HashMap::new(),
        }
    }

    fn entry(&mut self, key: &K, class: CredentialClass) -> &mut Breaker {
        self.breakers
            .entry(key.clone())
            .or_insert_with(|| Breaker::new(class))
    }

    pub fn admit(&mut self, key: &K, class: CredentialClass, now_ms: u64) -> Admission {
        self.entry(key, class).admit(now_ms)
    }

    /// Read-only; a key never seen is admitted.
    pub fn would_admit(&self, key: &K, now_ms: u64) -> bool {
        self.breakers
            .get(key)
            .is_none_or(|breaker| breaker.would_admit(now_ms))
    }

    pub fn on_success(&mut self, key: &K, class: CredentialClass) {
        self.entry(key, class).on_success();
    }

    pub fn on_failure(&mut self, key: &K, class: CredentialClass, now_ms: u64) {
        self.entry(key, class).on_failure(now_ms);
    }

    pub fn on_retry_after(&mut self, key: &K, class: CredentialClass, now_ms: u64, ms: u64) {
        self.entry(key, class).on_retry_after(now_ms, ms);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tripped(class: CredentialClass, now_ms: u64) -> Breaker {
        let mut breaker = Breaker::new(class);
        for _ in 0..class.failure_threshold() {
            assert!(breaker.admit(now_ms).admitted());
            breaker.on_failure(now_ms);
        }
        breaker
    }

    #[test]
    fn thresholds_and_windows_follow_b8() {
        for (class, threshold, window) in [
            (CredentialClass::OAuth, 3, 60_000),
            (CredentialClass::ApiKey, 5, 30_000),
            (CredentialClass::Local, 2, 15_000),
        ] {
            let mut breaker = Breaker::new(class);
            for _ in 0..threshold - 1 {
                breaker.on_failure(1_000);
            }
            assert_eq!(
                breaker.admit(1_000),
                Admission::Allowed,
                "{class:?} opened early"
            );
            breaker.on_failure(1_000);
            assert_eq!(
                breaker.admit(1_000),
                Admission::Rejected {
                    retry_after_ms: window
                },
                "{class:?}"
            );
            assert_eq!(
                breaker.admit(1_000 + window - 1),
                Admission::Rejected { retry_after_ms: 1 }
            );
            assert_eq!(breaker.admit(1_000 + window), Admission::Probe);
        }
    }

    #[test]
    fn success_resets_the_consecutive_count() {
        let mut breaker = Breaker::new(CredentialClass::Local);
        breaker.on_failure(0);
        breaker.on_success();
        breaker.on_failure(0);
        assert_eq!(breaker.admit(0), Admission::Allowed);
    }

    #[test]
    fn half_open_admits_exactly_one_probe() {
        let mut breaker = tripped(CredentialClass::Local, 0);
        assert_eq!(breaker.admit(15_000), Admission::Probe);
        assert_eq!(
            breaker.admit(15_001),
            Admission::Rejected { retry_after_ms: 0 }
        );
        breaker.on_success();
        assert_eq!(breaker.admit(15_002), Admission::Allowed);
    }

    #[test]
    fn a_probe_that_never_reports_is_replaced_after_a_window() {
        let mut breaker = tripped(CredentialClass::Local, 0);
        assert_eq!(breaker.admit(15_000), Admission::Probe);
        assert_eq!(
            breaker.admit(29_999),
            Admission::Rejected { retry_after_ms: 0 }
        );
        assert_eq!(breaker.admit(30_000), Admission::Probe);
    }

    #[test]
    fn failed_probe_reopens_for_a_full_window() {
        let mut breaker = tripped(CredentialClass::ApiKey, 0);
        assert_eq!(breaker.admit(30_000), Admission::Probe);
        breaker.on_failure(30_000);
        assert_eq!(
            breaker.admit(30_000),
            Admission::Rejected {
                retry_after_ms: 30_000
            }
        );
    }

    #[test]
    fn late_failures_while_open_do_not_extend_the_window() {
        let mut breaker = tripped(CredentialClass::Local, 0);
        breaker.on_failure(10_000);
        assert_eq!(breaker.admit(15_000), Admission::Probe);
    }

    #[test]
    fn retry_after_opens_and_only_ever_extends() {
        let mut breaker = Breaker::new(CredentialClass::ApiKey);
        breaker.on_retry_after(0, 120_000);
        assert_eq!(
            breaker.admit(0),
            Admission::Rejected {
                retry_after_ms: 120_000
            }
        );
        breaker.on_retry_after(0, 5_000);
        assert_eq!(
            breaker.admit(60_000),
            Admission::Rejected {
                retry_after_ms: 60_000
            }
        );
        assert_eq!(parse_retry_after_ms(" 7 "), Some(7_000));
        for raw in ["", "-1", "1.5", "Wed, 21 Oct 2015 07:28:00 GMT"] {
            assert_eq!(parse_retry_after_ms(raw), None, "{raw}");
        }
    }

    #[test]
    fn would_admit_matches_admit_without_claiming_the_probe() {
        let mut breaker = tripped(CredentialClass::Local, 0);
        assert!(!breaker.would_admit(14_999));
        assert!(breaker.would_admit(15_000));
        assert!(breaker.would_admit(15_000), "peeking twice claims nothing");
        assert_eq!(breaker.admit(15_000), Admission::Probe);
        assert!(!breaker.would_admit(15_001));
        let book: BreakerBook<&str> = BreakerBook::new();
        assert!(book.would_admit(&"never-seen", 0));
    }

    #[test]
    fn book_keeps_granularities_apart() {
        let mut book: BreakerBook<(&str, &str)> = BreakerBook::new();
        let class = CredentialClass::Local;
        for _ in 0..2 {
            book.on_failure(&("omlx", "qwen-8b"), class, 0);
        }
        assert!(!book.admit(&("omlx", "qwen-8b"), class, 1).admitted());
        assert!(book.admit(&("omlx", "qwen-0.6b"), class, 1).admitted());
        book.on_success(&("omlx", "qwen-8b"), class);
        assert!(book.admit(&("omlx", "qwen-8b"), class, 2).admitted());
    }
}

#[cfg(test)]
mod lockout_window_tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::{Breaker, CredentialClass};

    /// Reviewed defect: the Supervisor path never claims `admit`, so an expired
    /// `Open` was never moved to `HalfOpen`. `on_failure` then kept the stale
    /// deadline and the next request was admitted — a model that keeps failing
    /// was locked out for exactly one window and then admitted indefinitely.
    #[test]
    fn repeated_failures_past_the_window_reopen_the_lockout() {
        let class = CredentialClass::Local;
        let window = class.recovery_ms();
        let mut breaker = Breaker::new(class);

        // 2 faults (threshold for Local) lock it.
        breaker.on_failure(0);
        breaker.on_failure(0);
        assert!(!breaker.would_admit(0), "two faults must lock the model");
        assert!(!breaker.would_admit(window - 1), "still inside the window");

        // The window expires, but nothing claimed a probe on this path.
        assert!(breaker.would_admit(window), "window expired -> selectable");

        // One more failure must re-lock, not inherit the expired deadline.
        breaker.on_failure(window);
        assert!(
            !breaker.would_admit(window),
            "a failure on the Supervisor path must reopen the lockout"
        );
        assert!(
            !breaker.would_admit(window + window - 1),
            "the reopened window still rejects"
        );

        // And once the reopened window passes, it is selectable again.
        assert!(
            breaker.would_admit(window + window),
            "after the reopened window the model is admitted again"
        );
    }

    /// The subscription relay path (`ChatProxy::report_outcome`) also never
    /// claims `admit`, so it depends on the same reopen rule. OAuth: 3 faults,
    /// 60 s window. Measured through the bare breaker because `open_providers`
    /// reads the wall clock and this path is not time-injectable.
    #[test]
    fn oauth_relay_faults_past_the_window_reopen_the_lockout() {
        let class = CredentialClass::OAuth;
        let window = class.recovery_ms();
        let mut breaker = Breaker::new(class);

        for _ in 0..class.failure_threshold() {
            breaker.on_failure(0);
        }
        assert!(!breaker.would_admit(0), "threshold faults must open it");

        // 61 s later the window has expired: selection peeks and admits.
        assert!(breaker.would_admit(window + 1_000), "expired window admits");

        // A relay fault now must re-lock instead of inheriting the stale deadline.
        breaker.on_failure(window + 1_000);
        assert!(
            !breaker.would_admit(window + 1_000),
            "a fault on the relay path must reopen the OAuth lockout"
        );
        assert!(
            !breaker.would_admit(window + 1_000 + window - 1),
            "the reopened window still rejects"
        );
        assert!(
            breaker.would_admit(window + 1_000 + window),
            "after the reopened window it is admitted again"
        );
    }

    /// Control: a failure inside a still-valid window keeps that deadline
    /// instead of extending it, so a single window is not stretched by noise.
    #[test]
    fn a_failure_inside_a_valid_window_keeps_its_deadline() {
        let class = CredentialClass::Local;
        let window = class.recovery_ms();
        let mut breaker = Breaker::new(class);
        breaker.on_failure(0);
        breaker.on_failure(0);
        assert!(!breaker.would_admit(0));

        breaker.on_failure(window / 2);
        assert!(
            !breaker.would_admit(window - 1),
            "deadline must not move later than the original window"
        );
        assert!(
            breaker.would_admit(window),
            "original window still ends on time"
        );
    }
}
