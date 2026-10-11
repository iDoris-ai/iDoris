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
    Closed { failures: u32 },
    Open { until_ms: u64 },
    HalfOpen { probing: bool },
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
            State::Open { .. } | State::HalfOpen { probing: false } => {
                self.state = State::HalfOpen { probing: true };
                Admission::Probe
            }
            State::HalfOpen { probing: true } => Admission::Rejected { retry_after_ms: 0 },
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
            State::Open { until_ms } => State::Open { until_ms },
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
