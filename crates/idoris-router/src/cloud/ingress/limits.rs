//! 平台 / 租户 / 成员三级上限（计算服务 S5，设计稿 §12 的 S5 一行，评审 H10）。
//!
//! # 和 S3 的花费闸门互补
//!
//! S3 的准入账本（`idoris_tenancy::accounts::ledger`）限的是**钱**：分钟 / 小时花费上限、在途总额上限、补贴池总上限。
//! 这里限的是**请求个数和并发**：每分钟多少个请求、同时在途多少个。两套闸门各管各的、不共用状态、不重复：
//! 一个请求要两边都过。码也不同（`request_rate_limited` / `concurrency_limited` 对 `rate_limited` / `in_flight_limited`），
//! 调用方一眼看得出是哪道闸门拦的。
//!
//! # 三级
//!
//! | 级别 | 键 | 保护谁 |
//! |---|---|---|
//! | 成员 | （租户，主体）；没有主体的节点凭证用（租户，凭证） | 一个孩子 / 一个 agent 把整个租户的额度刷光 |
//! | 租户 | 从付款账户链推导出来的顶层客户 | 一个客户把整个平台占满 |
//! | 平台 | 全局 | 整个节点和上游的容量 |
//!
//! 每一级都有速率（令牌桶，见 `bucket`）和并发（在途个数）两个上限。被限时返回稳定的原因码
//! （`request_rate_limited` / `concurrency_limited`）、`X-iDoris-Limit-Scope`（哪一级）和 `Retry-After`。
//!
//! # 顺序：成员 → 租户 → 平台，下一级拒绝就把上一级退回去
//!
//! 先查最内层。一个被限的成员**不会碰**租户和平台的桶：它被拒的请求如果也消耗外层的令牌，一个刷屏的成员会
//! 把整个租户、整个平台的额度偷偷耗光，最后拒绝掉的反而是别人。外层拒绝时，已经拿到的内层令牌和并发名额原样退回
//! （一个没有被服务的请求不占任何一级）。
//!
//! 成功的请求拿到一个 [`Permit`]，它占着三级的并发名额，直到被丢弃：非流式回答处理完就丢，流式响应的响应体
//! 持有它，整个流持续期间（直到发完或客户端断开）都算在途。速率令牌不退（请求已经被服务了）。
//!
//! # 为什么不按 IP
//!
//! 设计稿 §9 第 3 条：「资格由开通方证明，IP 和设备只作辅助信号，避免学校共享出口误伤一整个班」。这里的键
//! 全部来自**凭证推导出来的身份**，来源地址不在其中：一个学校的共享出口下，每个学生各有各的成员桶，
//! 互不影响。

use std::hash::Hash;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use super::bucket::{Bounded, Bucket, RateLimit};

/// 租户表、成员表的容量上限。键来自**已经认证并通过付款上下文校验**的请求，所以增长受账户库的人数约束；
/// 上限只是再加一道保险，超出时按 `Bounded` 的规则丢空闲条目。
const MAX_TENANT_ENTRIES: usize = 4_096;
const MAX_MEMBER_ENTRIES: usize = 131_072;

/// 一级的限额：速率加并发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LevelLimits {
    pub rate: RateLimit,
    pub max_concurrent: u32,
}

impl LevelLimits {
    pub fn new(requests_per_minute: u32, burst: u32, max_concurrent: u32) -> Result<Self, String> {
        if max_concurrent == 0 {
            return Err("max_concurrent must be at least 1".to_string());
        }
        Ok(Self {
            rate: RateLimit::new(requests_per_minute, burst)?,
            max_concurrent,
        })
    }
}

/// 一个租户的租户级和成员级限额（平台级是全局的，在 [`Limiter`] 里）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CallerLimits {
    pub tenant: LevelLimits,
    pub member: LevelLimits,
}

/// 被哪一级拦下的。顺序 = 检查顺序。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Member,
    Tenant,
    Platform,
}

impl Scope {
    /// 线上字符串（响应头 `X-iDoris-Limit-Scope`）。**不可变**。
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Member => "member",
            Scope::Tenant => "tenant",
            Scope::Platform => "platform",
        }
    }
}

/// 拦下的是速率还是并发。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitKind {
    Rate,
    Concurrency,
}

/// 一次被限的详情。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied {
    pub scope: Scope,
    pub kind: LimitKind,
    /// `Retry-After` 的秒数（至少 1）。速率限制是确切的等待；并发限制没有确切的等待，给 1 秒。
    pub retry_after_secs: u64,
}

/// 一级的状态：令牌桶 + 在途个数。
#[derive(Debug)]
struct Level {
    limits: LevelLimits,
    bucket: Bucket,
    in_flight: u32,
}

impl Level {
    fn new(limits: LevelLimits, now_ms: i64) -> Self {
        Self {
            limits,
            bucket: Bucket::full(&limits.rate, now_ms),
            in_flight: 0,
        }
    }

    /// 先看并发（满了就拒绝，**不**消耗令牌），再取令牌（不够就拒绝，**不**占并发名额）。
    fn acquire(&mut self, now_ms: i64) -> Result<(), (LimitKind, u64)> {
        if self.in_flight >= self.limits.max_concurrent {
            return Err((LimitKind::Concurrency, 1));
        }
        self.bucket
            .try_take(&self.limits.rate, now_ms)
            .map_err(|retry| (LimitKind::Rate, retry))?;
        self.in_flight += 1;
        Ok(())
    }

    fn release(&mut self) {
        self.in_flight = self.in_flight.saturating_sub(1);
    }

    /// 撤销一次 `acquire`：名额和令牌都退回（外层拒绝了这个请求）。
    fn undo(&mut self) {
        self.release();
        self.bucket.give_back(&self.limits.rate);
    }

    /// 空闲 = 没有在途请求、桶是满的：和从没出现过一样，丢掉不改变任何判断。
    fn is_idle(&self, now_ms: i64) -> bool {
        self.in_flight == 0 && self.bucket.is_full_at(&self.limits.rate, now_ms)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // 持锁时 panic 只可能发生在这个文件的几行整数运算里；状态永远是一致的（每次修改都是单步），所以直接接着用。
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 一个请求在三级里的键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitKeys {
    /// 租户 id（顶层客户的账户 id）。
    pub tenant: String,
    /// 租户内的成员键：`p:<主体>`，或没有主体时 `c:<凭证>`。
    pub member: String,
}

/// 三级限流器。
pub struct Limiter {
    platform: Mutex<Level>,
    tenants: Mutex<Bounded<String, Level>>,
    members: Mutex<Bounded<(String, String), Level>>,
}

impl std::fmt::Debug for Limiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Limiter").finish_non_exhaustive()
    }
}

impl Limiter {
    pub fn new(platform: LevelLimits, now_ms: i64) -> Arc<Self> {
        Arc::new(Self {
            platform: Mutex::new(Level::new(platform, now_ms)),
            tenants: Mutex::new(Bounded::new(MAX_TENANT_ENTRIES)),
            members: Mutex::new(Bounded::new(MAX_MEMBER_ENTRIES)),
        })
    }

    fn acquire_in<K: Hash + Eq + Clone>(
        table: &Mutex<Bounded<K, Level>>,
        key: &K,
        limits: LevelLimits,
        now_ms: i64,
    ) -> Result<(), (LimitKind, u64)> {
        let mut table = lock(table);
        table
            .get_or_insert_with(
                key,
                || Level::new(limits, now_ms),
                |level| level.is_idle(now_ms),
                |level| level.in_flight == 0,
            )
            .acquire(now_ms)
    }

    fn with_level<K: Hash + Eq + Clone>(
        table: &Mutex<Bounded<K, Level>>,
        key: &K,
        f: impl FnOnce(&mut Level),
    ) {
        if let Some(level) = lock(table).get_mut(key) {
            f(level);
        }
    }

    /// 三级依次准入（成员 → 租户 → 平台），都过才返回 [`Permit`]。被拒时一个名额、一个令牌都不占。
    pub fn admit(
        self: &Arc<Self>,
        keys: &LimitKeys,
        limits: &CallerLimits,
        now_ms: i64,
    ) -> Result<Permit, Denied> {
        let member_key = (keys.tenant.clone(), keys.member.clone());
        let denied = |scope, (kind, retry_after_secs)| Denied {
            scope,
            kind,
            retry_after_secs,
        };

        Self::acquire_in(&self.members, &member_key, limits.member, now_ms)
            .map_err(|why| denied(Scope::Member, why))?;

        if let Err(why) = Self::acquire_in(&self.tenants, &keys.tenant, limits.tenant, now_ms) {
            Self::with_level(&self.members, &member_key, Level::undo);
            return Err(denied(Scope::Tenant, why));
        }

        if let Err(why) = lock(&self.platform).acquire(now_ms) {
            Self::with_level(&self.tenants, &keys.tenant, Level::undo);
            Self::with_level(&self.members, &member_key, Level::undo);
            return Err(denied(Scope::Platform, why));
        }

        Ok(Permit {
            limiter: Arc::clone(self),
            tenant: keys.tenant.clone(),
            member: member_key,
        })
    }

    /// 当前三级各自的在途个数（测试和诊断用）。
    pub fn in_flight(&self, keys: &LimitKeys) -> (u32, u32, u32) {
        let member_key = (keys.tenant.clone(), keys.member.clone());
        let member = lock(&self.members)
            .get_mut(&member_key)
            .map_or(0, |level| level.in_flight);
        let tenant = lock(&self.tenants)
            .get_mut(&keys.tenant)
            .map_or(0, |level| level.in_flight);
        let platform = lock(&self.platform).in_flight;
        (member, tenant, platform)
    }
}

/// 三级的并发名额。丢弃时释放（令牌不退）。
#[must_use = "dropping the permit releases the concurrency slots immediately"]
pub struct Permit {
    limiter: Arc<Limiter>,
    tenant: String,
    member: (String, String),
}

impl std::fmt::Debug for Permit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Permit").finish_non_exhaustive()
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        Limiter::with_level(&self.limiter.members, &self.member, Level::release);
        Limiter::with_level(&self.limiter.tenants, &self.tenant, Level::release);
        lock(&self.limiter.platform).release();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn level(rpm: u32, burst: u32, concurrent: u32) -> LevelLimits {
        LevelLimits::new(rpm, burst, concurrent).unwrap()
    }

    fn keys(tenant: &str, member: &str) -> LimitKeys {
        LimitKeys {
            tenant: tenant.to_string(),
            member: member.to_string(),
        }
    }

    /// 宽松的限额：只有被测的那一级会触发。
    fn roomy() -> LevelLimits {
        level(60_000, 10_000, 10_000)
    }

    fn caller(tenant: LevelLimits, member: LevelLimits) -> CallerLimits {
        CallerLimits { tenant, member }
    }

    #[test]
    fn scope_strings_are_pinned() {
        assert_eq!(Scope::Member.as_str(), "member");
        assert_eq!(Scope::Tenant.as_str(), "tenant");
        assert_eq!(Scope::Platform.as_str(), "platform");
        assert!(Scope::Member < Scope::Tenant && Scope::Tenant < Scope::Platform);
    }

    #[test]
    fn a_zero_concurrency_or_rate_is_a_configuration_error() {
        assert!(LevelLimits::new(60, 5, 0).is_err());
        assert!(LevelLimits::new(0, 5, 1).is_err());
        assert!(LevelLimits::new(60, 0, 1).is_err());
    }

    #[test]
    fn the_member_level_rate_triggers_alone_and_names_its_scope_and_wait() {
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(roomy(), level(20, 2, 100)); // 每 3 秒 1 个，突发 2
        let k = keys("eduai", "p:student-1");
        drop(limiter.admit(&k, &limits, 0).unwrap());
        drop(limiter.admit(&k, &limits, 0).unwrap());
        let denied = limiter.admit(&k, &limits, 0).unwrap_err();
        assert_eq!(
            denied,
            Denied {
                scope: Scope::Member,
                kind: LimitKind::Rate,
                retry_after_secs: 3
            }
        );
        // 等够了就恢复
        assert!(limiter.admit(&k, &limits, 3_000).is_ok());
    }

    #[test]
    fn the_member_level_concurrency_triggers_alone_and_does_not_burn_a_rate_token() {
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(roomy(), level(60, 2, 1));
        let k = keys("eduai", "p:student-1");
        let first = limiter.admit(&k, &limits, 0).unwrap();
        // 在途 1 个，并发上限 1：第二个被并发拒绝，而不是速率
        let denied = limiter.admit(&k, &limits, 0).unwrap_err();
        assert_eq!(
            (denied.scope, denied.kind, denied.retry_after_secs),
            (Scope::Member, LimitKind::Concurrency, 1)
        );
        drop(first);
        // 并发拒绝没有消耗令牌：突发 2 里只用了 1 个，所以现在还能再来 1 个而不是被速率拒绝
        let again = limiter.admit(&k, &limits, 0).unwrap();
        drop(again);
        let denied = limiter.admit(&k, &limits, 0).unwrap_err();
        assert_eq!(
            (denied.scope, denied.kind),
            (Scope::Member, LimitKind::Rate)
        );
    }

    #[test]
    fn the_tenant_level_triggers_alone_when_every_member_is_within_its_own_limit() {
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(level(60, 3, 100), roomy()); // 租户突发 3
        for member in ["p:a", "p:b", "p:c"] {
            drop(limiter.admit(&keys("eduai", member), &limits, 0).unwrap());
        }
        // 第四个成员（自己的桶是满的）被租户拦下
        let denied = limiter
            .admit(&keys("eduai", "p:d"), &limits, 0)
            .unwrap_err();
        assert_eq!(
            (denied.scope, denied.kind, denied.retry_after_secs),
            (Scope::Tenant, LimitKind::Rate, 1)
        );
        // 另一个租户不受影响
        assert!(limiter.admit(&keys("agent24", "p:a"), &limits, 0).is_ok());
    }

    #[test]
    fn the_tenant_level_concurrency_counts_every_member_together() {
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(level(60_000, 10_000, 2), roomy());
        let _a = limiter.admit(&keys("eduai", "p:a"), &limits, 0).unwrap();
        let _b = limiter.admit(&keys("eduai", "p:b"), &limits, 0).unwrap();
        let denied = limiter
            .admit(&keys("eduai", "p:c"), &limits, 0)
            .unwrap_err();
        assert_eq!(
            (denied.scope, denied.kind),
            (Scope::Tenant, LimitKind::Concurrency)
        );
    }

    #[test]
    fn the_platform_level_triggers_alone_across_tenants() {
        let limiter = Limiter::new(level(60, 2, 100), 0); // 平台突发 2
        let limits = caller(roomy(), roomy());
        drop(limiter.admit(&keys("eduai", "p:a"), &limits, 0).unwrap());
        drop(limiter.admit(&keys("agent24", "p:a"), &limits, 0).unwrap());
        let denied = limiter
            .admit(&keys("third", "p:a"), &limits, 0)
            .unwrap_err();
        assert_eq!(
            (denied.scope, denied.kind, denied.retry_after_secs),
            (Scope::Platform, LimitKind::Rate, 1)
        );
        // 平台并发
        let limiter = Limiter::new(level(60_000, 10_000, 1), 0);
        let _held = limiter.admit(&keys("eduai", "p:a"), &limits, 0).unwrap();
        let denied = limiter
            .admit(&keys("agent24", "p:a"), &limits, 0)
            .unwrap_err();
        assert_eq!(
            (denied.scope, denied.kind),
            (Scope::Platform, LimitKind::Concurrency)
        );
    }

    #[test]
    fn a_member_that_hits_its_own_limit_never_drains_the_tenant_or_the_platform() {
        // 租户突发 3、平台突发 3。一个成员（自己的突发是 1）连续刷 100 次：只有第一次被服务，
        // 其余 99 次在成员级就被拒绝，不碰外层——所以别的成员还能用满外层的 3 个令牌里剩下的 2 个。
        let limiter = Limiter::new(level(60, 3, 100), 0);
        let limits = caller(level(60, 3, 100), level(60, 1, 100));
        let hog = keys("eduai", "p:hog");
        drop(limiter.admit(&hog, &limits, 0).unwrap());
        for _ in 0..99 {
            let denied = limiter.admit(&hog, &limits, 0).unwrap_err();
            assert_eq!(denied.scope, Scope::Member);
        }
        for member in ["p:x", "p:y"] {
            assert!(
                limiter.admit(&keys("eduai", member), &limits, 0).is_ok(),
                "{member} must still get a token: the hog's refusals cost the outer levels nothing"
            );
        }
        // 此时租户的 3 个令牌（hog 1 + x + y）正好用完
        let denied = limiter
            .admit(&keys("eduai", "p:z"), &limits, 0)
            .unwrap_err();
        assert_eq!(denied.scope, Scope::Tenant);
    }

    #[test]
    fn an_outer_refusal_gives_back_what_the_inner_levels_already_took() {
        // 租户并发 1：第二个成员被租户级拒绝；它在成员级拿到的令牌和名额必须原样退回。
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(level(60_000, 10_000, 1), level(60, 1, 5)); // 成员突发 1
        let a = keys("eduai", "p:a");
        let b = keys("eduai", "p:b");
        let held = limiter.admit(&a, &limits, 0).unwrap();
        let denied = limiter.admit(&b, &limits, 0).unwrap_err();
        assert_eq!(denied.scope, Scope::Tenant);
        assert_eq!(limiter.in_flight(&b).0, 0, "the member slot was given back");
        drop(held);
        // b 的成员级突发只有 1：如果它被拒绝的那次没有退回令牌，这里会被成员级速率拒绝
        assert!(limiter.admit(&b, &limits, 0).is_ok());
        // 同样的检查，平台拒绝：成员和租户都退回
        let limiter = Limiter::new(level(60_000, 10_000, 1), 0);
        let limits = caller(level(60, 1, 5), level(60, 1, 5));
        let _held = limiter.admit(&keys("t1", "p:a"), &limits, 0).unwrap();
        let denied = limiter.admit(&keys("t2", "p:b"), &limits, 0).unwrap_err();
        assert_eq!(denied.scope, Scope::Platform);
        assert_eq!(limiter.in_flight(&keys("t2", "p:b")), (0, 0, 1));
        drop(_held);
        assert!(limiter.admit(&keys("t2", "p:b"), &limits, 0).is_ok());
    }

    #[test]
    fn the_permit_holds_three_slots_until_it_is_dropped() {
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(roomy(), roomy());
        let k = keys("eduai", "p:a");
        let first = limiter.admit(&k, &limits, 0).unwrap();
        let second = limiter.admit(&k, &limits, 0).unwrap();
        assert_eq!(limiter.in_flight(&k), (2, 2, 2));
        drop(first);
        assert_eq!(limiter.in_flight(&k), (1, 1, 1));
        drop(second);
        assert_eq!(limiter.in_flight(&k), (0, 0, 0));
    }

    #[test]
    fn two_members_of_one_tenant_and_the_same_member_in_two_tenants_have_separate_buckets() {
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(roomy(), level(60, 1, 5));
        // 同租户两个成员：各一个
        drop(limiter.admit(&keys("eduai", "p:a"), &limits, 0).unwrap());
        drop(limiter.admit(&keys("eduai", "p:b"), &limits, 0).unwrap());
        // 同一个主体在两个租户里：各有各的桶（老师同时在两所学校）
        drop(limiter.admit(&keys("school-a", "p:t"), &limits, 0).unwrap());
        drop(limiter.admit(&keys("school-b", "p:t"), &limits, 0).unwrap());
        // 但同一个租户里同一个成员只有一个
        assert!(limiter.admit(&keys("eduai", "p:a"), &limits, 0).is_err());
    }

    #[test]
    fn idle_entries_are_forgotten_without_changing_any_decision() {
        // 小表：member 表容量是常量，这里只验证 Level::is_idle 的语义
        let limits = level(60, 2, 3);
        let mut level = Level::new(limits, 0);
        assert!(level.is_idle(0));
        level.acquire(0).unwrap();
        assert!(!level.is_idle(0), "in flight");
        level.release();
        assert!(!level.is_idle(0), "bucket not full yet");
        assert!(level.is_idle(1_000), "one second later the token is back");
    }

    #[test]
    fn concurrent_admissions_never_exceed_the_concurrency_limit() {
        use std::sync::atomic::{AtomicU32, Ordering};
        let limiter = Limiter::new(roomy(), 0);
        let limits = caller(roomy(), level(60_000, 10_000, 4));
        let k = keys("eduai", "p:a");
        let in_progress = Arc::new(AtomicU32::new(0));
        let peak = Arc::new(AtomicU32::new(0));
        let admitted = Arc::new(AtomicU32::new(0));
        std::thread::scope(|scope| {
            for _ in 0..16 {
                let limiter = limiter.clone();
                let k = k.clone();
                let (in_progress, peak, admitted) =
                    (in_progress.clone(), peak.clone(), admitted.clone());
                scope.spawn(move || {
                    for _ in 0..200 {
                        if let Ok(permit) = limiter.admit(&k, &limits, 0) {
                            admitted.fetch_add(1, Ordering::SeqCst);
                            let now = in_progress.fetch_add(1, Ordering::SeqCst) + 1;
                            peak.fetch_max(now, Ordering::SeqCst);
                            std::thread::yield_now();
                            in_progress.fetch_sub(1, Ordering::SeqCst);
                            drop(permit);
                        }
                    }
                });
            }
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 4,
            "{}",
            peak.load(Ordering::SeqCst)
        );
        assert!(admitted.load(Ordering::SeqCst) > 0);
        assert_eq!(limiter.in_flight(&k), (0, 0, 0));
    }
}
