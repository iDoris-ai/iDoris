//! M5 三粒度弹性（总体规划 §4.4、B-8）的纯状态机：provider 断路器
//! （CLOSED → OPEN → HALF_OPEN）、connection 冷却（遵守 `Retry-After`）、
//! 单模型锁定。**没有 IO、没有时钟**：调用方传入 `now_ms`，便于确定性测试。
//!
//! 三个粒度用同一个状态机、不同的键：provider id、connection（端点）、
//! `(provider, model)`；[`BreakerBook`] 按（凭据类别，键）分开保存。
//!
//! 默认参数取自 B-8：OAuth（订阅类）连续 3 次失败、API Key 5 次、本地 2 次
//! 即断开；恢复窗口分别 60s / 30s / 15s。窗口过后进入 HALF_OPEN，只放一个
//! 探测请求：成功则闭合，失败则以新的完整窗口重新断开。
//!
//! 每次放行交回一枚一次性[`AdmissionTicket`]。回报必须移交给这枚票据；
//! 世代已经前进的迟到成功不能提前闭合，迟到失败也不能重新打开已经恢复的
//! 断路器。只有当前这一次探测的票据能闭合 HALF_OPEN。

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};

fn next_issuer() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    NEXT.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .unwrap_or_else(|_| panic!("breaker issuer IDs exhausted"))
}

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

/// 一次准入判定。拒绝不带票据。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    Allowed,
    /// HALF_OPEN 下唯一放行的探测请求；结果必须用配套票据回报。
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

/// A pair is refused before either admission is claimed.
#[derive(Debug)]
pub enum PairReject {
    Duplicate,
    Gate { first: Admission, second: Admission },
}

/// 一次放行的不透明票据。故意没有 `Copy`/`Clone`：同一次请求只能把票据
/// 移交给一个回报方法，从类型上避免成功和失败各报一次。
///
/// 字段不公开，调用方不能自己造票据；过期与否只由断路器判断。
#[derive(Debug)]
#[must_use = "move this ticket into exactly one outcome report"]
pub struct AdmissionTicket {
    issuer: u64,
    generation: u64,
    probe: bool,
}

/// [`Breaker::admit`] 的结果。拒绝时 `ticket` 为 `None`。
#[derive(Debug)]
#[must_use = "keep the ticket from an admitted request and report its outcome"]
pub struct Admit {
    pub admission: Admission,
    pub ticket: Option<AdmissionTicket>,
}

impl Admit {
    pub const fn admitted(&self) -> bool {
        self.admission.admitted()
    }
}

/// Tickets belong to this instance. Cloning retains state but creates a new issuer.
///
/// Pre-release API refinement: `Breaker` is no longer `Copy`, and `new` is no
/// longer `const`, because every new or cloned instance needs a unique identity.
#[derive(Debug, PartialEq, Eq)]
pub struct Breaker {
    issuer: u64,
    class: CredentialClass,
    state: State,
    /// 当前世代。进入 OPEN、以及探测成功回到 CLOSED 时加一。
    generation: u64,
    /// 打开这一轮 OPEN 时，仍在途的票据世代。这些票据还能提交更长的
    /// `Retry-After`，但不能再把断路器闭合。
    open_cohort: Option<u64>,
}

impl Clone for Breaker {
    fn clone(&self) -> Self {
        Self {
            issuer: next_issuer(),
            class: self.class,
            state: self.state,
            generation: self.generation,
            open_cohort: self.open_cohort,
        }
    }
}

impl Breaker {
    pub fn new(class: CredentialClass) -> Self {
        Self {
            issuer: next_issuer(),
            class,
            state: State::Closed { failures: 0 },
            generation: 0,
            open_cohort: None,
        }
    }

    /// Inspect admission without allocating an issuer or claiming a probe.
    pub fn readiness(&self, now_ms: u64) -> Admission {
        match self.state {
            State::Closed { .. } => Admission::Allowed,
            State::Open { until_ms } if now_ms < until_ms => Admission::Rejected {
                retry_after_ms: until_ms - now_ms,
            },
            State::Open { .. } | State::HalfOpen { probing: false } => Admission::Probe,
            State::HalfOpen { probing: true } => Admission::Rejected { retry_after_ms: 0 },
        }
    }

    pub fn admit(&mut self, now_ms: u64) -> Admit {
        let (admission, probe) = match self.state {
            State::Closed { .. } => (Admission::Allowed, Some(false)),
            State::Open { until_ms } if now_ms < until_ms => (
                Admission::Rejected {
                    retry_after_ms: until_ms - now_ms,
                },
                None,
            ),
            State::Open { .. } | State::HalfOpen { probing: false } => {
                self.state = State::HalfOpen { probing: true };
                (Admission::Probe, Some(true))
            }
            State::HalfOpen { probing: true } => (Admission::Rejected { retry_after_ms: 0 }, None),
        };
        let ticket = probe.map(|probe| AdmissionTicket {
            issuer: self.issuer,
            generation: self.generation,
            probe,
        });
        Admit { admission, ticket }
    }

    pub fn on_success(&mut self, ticket: AdmissionTicket) {
        let AdmissionTicket {
            issuer,
            generation,
            probe,
        } = ticket;
        if issuer != self.issuer {
            return;
        }
        if generation != self.generation {
            return;
        }
        match self.state {
            State::HalfOpen { .. } if probe => {
                self.state = State::Closed { failures: 0 };
                self.open_cohort = None;
                self.generation = self.generation.saturating_add(1);
            }
            State::Closed { .. } => {
                self.state = State::Closed { failures: 0 };
            }
            State::HalfOpen { .. } | State::Open { .. } => {}
        }
    }

    pub fn on_failure(&mut self, ticket: AdmissionTicket, now_ms: u64) {
        self.on_failure_metadata(&ticket, now_ms);
    }

    fn on_failure_metadata(&mut self, ticket: &AdmissionTicket, now_ms: u64) {
        let AdmissionTicket {
            issuer,
            generation,
            probe,
        } = *ticket;
        if issuer != self.issuer {
            return;
        }
        if generation != self.generation {
            return;
        }
        if matches!(self.state, State::HalfOpen { .. }) && !probe {
            return;
        }
        match self.state {
            State::Closed { failures } if failures + 1 < self.class.failure_threshold() => {
                self.state = State::Closed {
                    failures: failures + 1,
                };
            }
            State::Open { .. } => {}
            State::Closed { .. } | State::HalfOpen { .. } => {
                self.open_cohort = Some(generation);
                self.generation = self.generation.saturating_add(1);
                self.state = State::Open {
                    until_ms: now_ms.saturating_add(self.class.recovery_ms()),
                };
            }
        }
    }

    /// Count failure and apply its header atomically, consuming one ticket.
    ///
    /// ```compile_fail
    /// use idoris_policy::breaker::{Breaker, CredentialClass};
    /// let mut b = Breaker::new(CredentialClass::Local);
    /// let ticket = b.admit(0).ticket.unwrap();
    /// b.on_failure_with_retry_after(ticket, 0, Some(1000));
    /// b.on_success(ticket);
    /// ```
    pub fn on_failure_with_retry_after(
        &mut self,
        ticket: AdmissionTicket,
        now_ms: u64,
        retry_after_ms: Option<u64>,
    ) {
        self.on_failure_metadata(&ticket, now_ms);
        if let Some(ms) = retry_after_ms {
            self.on_retry_after_metadata(&ticket, now_ms, ms);
        }
    }

    /// 上游明确给出 `Retry-After`：至少断开到那一刻（不会缩短已有窗口）。
    ///
    /// 票据只能使用一次。带 `Retry-After` 的响应只调用本方法，不要再调用
    /// [`Self::on_failure`]：探测票据会同时按探测失败处理，窗口至少是凭据
    /// 类别的完整恢复时间，header 更长时再延长。同一打开世代里、另一枚
    /// 尚未回报的票据仍可以延长窗口，但不能缩短。
    pub fn on_retry_after(&mut self, ticket: AdmissionTicket, now_ms: u64, retry_after_ms: u64) {
        self.on_retry_after_metadata(&ticket, now_ms, retry_after_ms);
    }

    fn on_retry_after_metadata(
        &mut self,
        ticket: &AdmissionTicket,
        now_ms: u64,
        retry_after_ms: u64,
    ) {
        let AdmissionTicket {
            issuer,
            generation,
            probe,
        } = *ticket;
        if issuer != self.issuer {
            return;
        }
        if matches!(self.state, State::HalfOpen { .. }) && !probe {
            return;
        }
        let fresh = generation == self.generation;
        let cohort_extend =
            matches!(self.state, State::Open { .. }) && self.open_cohort == Some(generation);
        if !fresh && !cohort_extend {
            return;
        }
        let mut until_ms = now_ms.saturating_add(retry_after_ms);
        if probe {
            until_ms = until_ms.max(now_ms.saturating_add(self.class.recovery_ms()));
        }
        if let State::Open { until_ms: existing } = self.state {
            until_ms = existing.max(until_ms);
        }
        self.state = State::Open { until_ms };
        if fresh {
            self.open_cohort = Some(generation);
            self.generation = self.generation.saturating_add(1);
        }
    }
}

/// `Retry-After` 的整数秒形式；HTTP-date 形式与非法值返回 `None`，调用方
/// 退回到凭据类别的恢复窗口。ASCII delta-seconds saturate on overflow;
/// this parser returns raw milliseconds without a network-boundary cap.
pub fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let digits = raw.trim().as_bytes();
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(
        digits
            .iter()
            .fold(0_u64, |seconds, digit| {
                seconds
                    .saturating_mul(10)
                    .saturating_add(u64::from(digit - b'0'))
            })
            .saturating_mul(1000),
    )
}

/// 按（凭据类别，键）分开的一组断路器。同一个键的不同类别各有各的状态。
#[derive(Debug, Default)]
pub struct BreakerBook<K: Eq + Hash> {
    breakers: HashMap<(CredentialClass, K), Breaker>,
}

impl<K: Eq + Hash + Clone> BreakerBook<K> {
    pub fn new() -> Self {
        Self {
            breakers: HashMap::new(),
        }
    }

    /// Missing keys are ready without creating an entry or allocating an issuer.
    pub fn readiness(&self, key: &K, class: CredentialClass, now_ms: u64) -> Admission {
        self.breakers
            .get(&(class, key.clone()))
            .map_or(Admission::Allowed, |breaker| breaker.readiness(now_ms))
    }

    /// Claim two distinct class/key gates in input order at the same time.
    ///
    /// Every `Err` preserves entries and probes. Issuer allocation for both
    /// missing entries precedes any probe claim. Clone/Hash/Eq user code and
    /// allocation can panic; this is not a general panic/OOM rollback guarantee.
    /// Both mutable lookups complete before claiming, with no user code between
    /// the two calls to the existing admission transition.
    pub fn admit_pair(
        &mut self,
        first: (&K, CredentialClass),
        second: (&K, CredentialClass),
        now_ms: u64,
    ) -> Result<[Admit; 2], PairReject> {
        if first.1 == second.1 && first.0 == second.0 {
            return Err(PairReject::Duplicate);
        }
        let assessments = (
            self.readiness(first.0, first.1, now_ms),
            self.readiness(second.0, second.1, now_ms),
        );
        if !assessments.0.admitted() || !assessments.1.admitted() {
            return Err(PairReject::Gate {
                first: assessments.0,
                second: assessments.1,
            });
        }
        let keys = [(first.1, first.0.clone()), (second.1, second.0.clone())];
        let prepared = keys
            .each_ref()
            .map(|key| (!self.breakers.contains_key(key)).then(|| Breaker::new(key.0)));
        for (key, breaker) in keys.iter().zip(prepared) {
            if let Some(breaker) = breaker {
                self.breakers.entry(key.clone()).or_insert(breaker);
            }
        }
        let [Some(first), Some(second)] = self.breakers.get_disjoint_mut([&keys[0], &keys[1]])
        else {
            panic!("prepared pair gates must exist");
        };
        Ok([first.admit(now_ms), second.admit(now_ms)])
    }

    pub fn admit(&mut self, key: &K, class: CredentialClass, now_ms: u64) -> Admit {
        self.breakers
            .entry((class, key.clone()))
            .or_insert_with(|| Breaker::new(class))
            .admit(now_ms)
    }

    pub fn on_success(&mut self, key: &K, class: CredentialClass, ticket: AdmissionTicket) {
        if let Some(breaker) = self.breakers.get_mut(&(class, key.clone())) {
            breaker.on_success(ticket);
        }
    }

    pub fn on_failure(
        &mut self,
        key: &K,
        class: CredentialClass,
        ticket: AdmissionTicket,
        now_ms: u64,
    ) {
        if let Some(breaker) = self.breakers.get_mut(&(class, key.clone())) {
            breaker.on_failure(ticket, now_ms);
        }
    }

    pub fn on_failure_with_retry_after(
        &mut self,
        key: &K,
        class: CredentialClass,
        ticket: AdmissionTicket,
        now_ms: u64,
        ms: Option<u64>,
    ) {
        if let Some(breaker) = self.breakers.get_mut(&(class, key.clone())) {
            breaker.on_failure_with_retry_after(ticket, now_ms, ms);
        }
    }

    pub fn on_retry_after(
        &mut self,
        key: &K,
        class: CredentialClass,
        ticket: AdmissionTicket,
        now_ms: u64,
        ms: u64,
    ) {
        if let Some(breaker) = self.breakers.get_mut(&(class, key.clone())) {
            breaker.on_retry_after(ticket, now_ms, ms);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod pair_internal_tests {
    use super::*;

    #[test]
    fn readiness_preserves_all_fields_including_unclaimed_half_open() {
        let mut breaker = Breaker::new(CredentialClass::Local);
        for state in [
            State::Closed { failures: 1 },
            State::Open { until_ms: 10 },
            State::HalfOpen { probing: false },
            State::HalfOpen { probing: true },
        ] {
            breaker.state = state;
            breaker.generation = 7;
            breaker.open_cohort = Some(6);
            let before = (
                breaker.issuer,
                breaker.state,
                breaker.generation,
                breaker.open_cohort,
            );
            for _ in 0..4 {
                let expected = breaker.readiness(10);
                assert_eq!(breaker.readiness(10), expected);
                assert_eq!(
                    (
                        breaker.issuer,
                        breaker.state,
                        breaker.generation,
                        breaker.open_cohort
                    ),
                    before
                );
            }
            assert_eq!(breaker.readiness(10), breaker.admit(10).admission);
        }
    }

    #[test]
    fn absent_readiness_and_refusals_do_not_insert_or_replace() {
        let mut book = BreakerBook::new();
        let class = CredentialClass::Local;
        for _ in 0..4 {
            assert_eq!(book.readiness(&"missing", class, 0), Admission::Allowed);
        }
        assert!(book.breakers.is_empty());
        assert!(matches!(
            book.admit_pair((&"missing", class), (&"missing", class), 0),
            Err(PairReject::Duplicate)
        ));
        assert!(book.breakers.is_empty());
        let owned = book.admit(&"blocked", class, 0).ticket.unwrap();
        book.on_retry_after(&"blocked", class, owned, 0, 10);
        let issuer = book.breakers[&(class, "blocked")].issuer;
        for reversed in [false, true] {
            let (a, b) = if reversed {
                ("missing", "blocked")
            } else {
                ("blocked", "missing")
            };
            assert!(matches!(
                book.admit_pair((&a, class), (&b, class), 0),
                Err(PairReject::Gate { .. })
            ));
            assert_eq!(book.breakers.len(), 1);
            assert_eq!(book.breakers[&(class, "blocked")].issuer, issuer);
        }
        let [a, b] = book
            .admit_pair((&"blocked", class), (&"missing", class), 10)
            .unwrap();
        assert_eq!(book.breakers[&(class, "blocked")].issuer, issuer);
        book.on_success(&"blocked", class, a.ticket.unwrap());
        book.on_success(&"missing", class, b.ticket.unwrap());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket_of(admit: Admit) -> AdmissionTicket {
        assert!(admit.admission.admitted(), "{:?}", admit.admission);
        let Some(ticket) = admit.ticket else {
            panic!("admitted request must carry a ticket");
        };
        ticket
    }

    fn tripped(class: CredentialClass, now_ms: u64) -> Breaker {
        let mut breaker = Breaker::new(class);
        for _ in 0..class.failure_threshold() {
            let ticket = ticket_of(breaker.admit(now_ms));
            breaker.on_failure(ticket, now_ms);
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
                let ticket = ticket_of(breaker.admit(1_000));
                breaker.on_failure(ticket, 1_000);
            }
            assert_eq!(
                breaker.admit(1_000).admission,
                Admission::Allowed,
                "{class:?} opened early"
            );
            let ticket = ticket_of(breaker.admit(1_000));
            breaker.on_failure(ticket, 1_000);
            assert_eq!(
                breaker.admit(1_000).admission,
                Admission::Rejected {
                    retry_after_ms: window
                },
                "{class:?}"
            );
            assert_eq!(
                breaker.admit(1_000 + window - 1).admission,
                Admission::Rejected { retry_after_ms: 1 }
            );
            assert_eq!(breaker.admit(1_000 + window).admission, Admission::Probe);
        }
    }

    #[test]
    fn success_resets_the_consecutive_count() {
        let mut breaker = Breaker::new(CredentialClass::Local);
        let first = ticket_of(breaker.admit(0));
        breaker.on_failure(first, 0);
        let second = ticket_of(breaker.admit(0));
        breaker.on_success(second);
        let third = ticket_of(breaker.admit(0));
        breaker.on_failure(third, 0);
        assert_eq!(breaker.admit(0).admission, Admission::Allowed);
    }

    #[test]
    fn half_open_admits_exactly_one_probe() {
        let mut breaker = tripped(CredentialClass::Local, 0);
        let admit = breaker.admit(15_000);
        assert_eq!(admit.admission, Admission::Probe);
        let probe = ticket_of(admit);
        assert_eq!(
            breaker.admit(15_001).admission,
            Admission::Rejected { retry_after_ms: 0 }
        );
        breaker.on_success(probe);
        assert_eq!(breaker.admit(15_002).admission, Admission::Allowed);
    }

    #[test]
    fn failed_probe_reopens_for_a_full_window() {
        let mut breaker = tripped(CredentialClass::ApiKey, 0);
        let admit = breaker.admit(30_000);
        assert_eq!(admit.admission, Admission::Probe);
        breaker.on_failure(ticket_of(admit), 30_000);
        assert_eq!(
            breaker.admit(30_000).admission,
            Admission::Rejected {
                retry_after_ms: 30_000
            }
        );
    }

    #[test]
    fn late_failures_while_open_do_not_extend_the_window() {
        let mut breaker = Breaker::new(CredentialClass::Local);
        let early = ticket_of(breaker.admit(0));
        for _ in 0..CredentialClass::Local.failure_threshold() {
            let ticket = ticket_of(breaker.admit(0));
            breaker.on_failure(ticket, 0);
        }
        breaker.on_failure(early, 10_000);
        assert_eq!(breaker.admit(15_000).admission, Admission::Probe);
    }

    #[test]
    fn retry_after_opens_and_only_ever_extends() {
        let mut breaker = Breaker::new(CredentialClass::ApiKey);
        let first = ticket_of(breaker.admit(0));
        let second = ticket_of(breaker.admit(0));
        breaker.on_retry_after(first, 0, 120_000);
        assert_eq!(
            breaker.admit(0).admission,
            Admission::Rejected {
                retry_after_ms: 120_000
            }
        );
        breaker.on_retry_after(second, 0, 5_000);
        assert_eq!(
            breaker.admit(60_000).admission,
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
    fn probe_retry_after_never_undercuts_the_recovery_window() {
        let mut short = tripped(CredentialClass::ApiKey, 0);
        let probe = ticket_of(short.admit(30_000));
        short.on_retry_after(probe, 30_000, 5_000);
        assert_eq!(
            short.admit(30_000).admission,
            Admission::Rejected {
                retry_after_ms: 30_000
            }
        );

        let mut long = tripped(CredentialClass::ApiKey, 0);
        let probe = ticket_of(long.admit(30_000));
        long.on_retry_after(probe, 30_000, 120_000);
        assert_eq!(
            long.admit(30_000).admission,
            Admission::Rejected {
                retry_after_ms: 120_000
            }
        );
    }

    #[test]
    fn book_keeps_granularities_apart() {
        let mut book: BreakerBook<(&str, &str)> = BreakerBook::new();
        let class = CredentialClass::Local;
        for _ in 0..2 {
            let ticket = ticket_of(book.admit(&("omlx", "qwen-8b"), class, 0));
            book.on_failure(&("omlx", "qwen-8b"), class, ticket, 0);
        }
        assert!(!book.admit(&("omlx", "qwen-8b"), class, 1).admitted());
        assert!(book.admit(&("omlx", "qwen-0.6b"), class, 1).admitted());
        let admit = book.admit(&("omlx", "qwen-8b"), class, 15_000);
        assert_eq!(admit.admission, Admission::Probe);
        book.on_success(&("omlx", "qwen-8b"), class, ticket_of(admit));
        assert!(book.admit(&("omlx", "qwen-8b"), class, 15_000).admitted());
    }
}
