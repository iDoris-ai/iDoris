//! 公网防滥用（计算服务 S5，设计稿 §9 第 3 条）：只在云形态加载。
//!
//! > 资格由开通方证明，IP 和设备只作辅助信号，避免学校共享出口误伤一整个班。
//!
//! 落到代码上是两条硬规则：
//!
//! 1. **资格 = 凭证。** 带着有效凭证的请求，不论它从哪个地址来、这个地址刚刚有多少人认证失败，都不会被任何
//!    和来源地址有关的东西拦住。三级上限（`limits`）的键全部来自凭证推导出来的身份，没有地址。
//!    **唯一的例外是「冷」的凭证**（还没认证成功过：刚签发、进程重启之后、被挤出「已知」表）：认证之前分不出它和随机假凭证，
//!    只能和同一个来源上其它「没认证成功过」的请求共用这个来源在 [`AuthBudget`] 里的份额（#603 N4，设计稿 S5-12）。
//!    它认证成功一次，之后就是「已知」的，永远不再看来源。
//! 2. **地址只用在「没有资格的请求」上。** 没有凭证（或凭证格式根本不对）的请求没有任何人需要保护，
//!    对它们按来源地址分一个软配额；这个软配额是**速率**，不是封禁：窗口过去就恢复，不记仇，不影响同一个地址上
//!    带着有效凭证的请求。
//!
//! # 三个限流器
//!
//! | | 谁被它限制 | 键 | 作用 |
//! |---|---|---|---|
//! | [`AnonymousLimiter`] | 没有凭证、或者认证失败的请求 | 来源地址（IPv6 取 /64）的软配额 + 全局总配额 | 防扫描、防海量垃圾请求把入口占满。**永远不会被带着有效凭证的请求碰到。** |
//! | [`FailureThrottle`] | 带着格式正确、但认证失败的凭证的请求 | **凭证前缀**（凭证明文 `idk_` 之后的前 16 个十六进制字符，64 位，加盐哈希之后做键） | 防爆破，也防一个带着已撤销凭证死循环重试的客户端把库打满。同一个前缀在窗口内失败太多次，之后同前缀的请求先在内存里被拒绝，不碰数据库。 |
//! | [`AuthBudget`] | **每一个**要开始认证查询的请求（认证**之前**） | 全局的在途个数 + 「最近没有认证成功过的凭证前缀」的全局查询预算（总闸）+ 同样条件下**按来源**的份额（来源地址，IPv6 取 /64；先扣份额、再扣总闸） | 防**随机假凭证**：攻击者持续发格式合法、前缀各不相同的凭证，上面两个限流器都只在认证**失败之后**才计数，每个假凭证照样进 `spawn_blocking`、抢认证存储的锁、查一次 SQLite。这里把「认证查询」本身的并发和总量卡住，满了直接拒绝、不碰库；按来源的份额让一个来源耗不光总闸，别的来源冷的合法凭证不会跟着被拒。 |
//!
//! 失败计数**不按纯 IP**：按凭证前缀。一个学校里一个学生把密钥输错，只会让「这个错的密钥」被限流，同一个出口后面
//! 其他学生的密钥（前缀不同）完全不受影响。
//!
//! # 为什么是 64 位前缀、加盐哈希
//!
//! 凭证明文是 256 位随机量。前缀取前 64 位，用每个进程随机的盐哈希之后才做键：内存里不留任何明文凭证的片段。
//! 64 位而不是更短，是为了防**定向投毒**：失败前缀是在认证**之前**预检的，如果前缀短到可以从「masked key」的显示
//! （许多控制台只显示 `idk_abcd1234…`）里读出来，别人就能对着这个前缀发几个垃圾凭证，把真凭证的请求也挡在预检外面。
//! 64 位前缀只靠显示出来的头几个字符猜不出来（要猜出剩下的位，得先发出天文数字的请求，而那些请求自己就先被
//! 来源软配额挡住了）。用前缀而不是整个凭证的哈希，是为了让「同一把钥匙输错了最后几位再试」也能聚到一起。
//!
//! # 表的大小
//!
//! 这些表的键（地址、前缀）有一部分来自攻击者，所以都是 [`Bounded`]：满了先丢空闲条目，再丢最老的，
//! 内存用量不由攻击者决定（见 `bucket`）。「最老」有两种：来源软配额表和失败计数表按进表先后（`Bounded::new`）；
//! `AuthBudget` 的两张表（按来源的份额、最近认证成功过的前缀）按最近使用（`Bounded::lru`）——热的条目每次取用都刷新，
//! 不会因为表被别的键挤满就被第一个丢掉。

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::bucket::{Bounded, Bucket, RateLimit};

/// 来源软配额表的容量。
const MAX_SOURCE_ENTRIES: usize = 16_384;
/// 失败计数表的容量。
const MAX_FAILURE_ENTRIES: usize = 16_384;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// 软配额用的来源键：IPv4 原样；IPv6 取 /64（一个终端用户通常拿到一整个 /64，按单个地址限等于没限）。
/// IPv4 映射的 IPv6 地址先还原成 IPv4。
pub fn source_key(ip: IpAddr) -> IpAddr {
    match ip.to_canonical() {
        IpAddr::V6(v6) => {
            let bits = u128::from(v6) & !((1u128 << 64) - 1);
            IpAddr::V6(Ipv6Addr::from(bits))
        }
        v4 => v4,
    }
}

/// 没有凭证的请求的限流器。见模块文档。
pub struct AnonymousLimiter {
    source_limit: RateLimit,
    global_limit: RateLimit,
    sources: Mutex<Bounded<IpAddr, Bucket>>,
    global: Mutex<Bucket>,
}

impl std::fmt::Debug for AnonymousLimiter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnonymousLimiter").finish_non_exhaustive()
    }
}

impl AnonymousLimiter {
    /// `per_source` 是单个来源的软配额，`global` 是所有没有凭证的请求合起来的总配额。
    pub fn new(per_source: RateLimit, global: RateLimit, now_ms: i64) -> Self {
        Self {
            source_limit: per_source,
            global_limit: global,
            sources: Mutex::new(Bounded::new(MAX_SOURCE_ENTRIES)),
            global: Mutex::new(Bucket::full(&global, now_ms)),
        }
    }

    /// 一个没有资格的请求来了：先扣这个来源的软配额，再扣全局总配额。`Err(秒数)` = 该等多久。
    ///
    /// 来源的软配额先查：一个吵闹的来源被自己的配额挡住，**不会**碰全局配额，所以它刷不光别人的份额；
    /// 全局配额拒绝时，刚扣掉的来源令牌退回去。
    pub fn admit(&self, source: IpAddr, now_ms: i64) -> Result<(), u64> {
        let key = source_key(source);
        {
            let mut sources = lock(&self.sources);
            let limit = self.source_limit;
            sources
                .get_or_insert_with(
                    &key,
                    || Bucket::full(&limit, now_ms),
                    |bucket| bucket.is_full_at(&limit, now_ms),
                    |_| true,
                )
                .try_take(&limit, now_ms)?;
        }
        let global = lock(&self.global).try_take(&self.global_limit, now_ms);
        if let Err(retry) = global {
            if let Some(bucket) = lock(&self.sources).get_mut(&key) {
                bucket.give_back(&self.source_limit);
            }
            return Err(retry);
        }
        Ok(())
    }
}

/// 最近认证成功过的凭证前缀表的容量。
const MAX_KNOWN_ENTRIES: usize = 16_384;

/// 认证之前的有界工作（R2 High#4；#603 N4 加上按来源的份额）：见模块文档。
///
/// 三道闸，都在**碰库之前**：
///
/// 1. **在途个数**：同时进行的认证查询不超过 `max_concurrent`。满了直接拒绝，不排队（排队就是把攻击者的请求
///    攒在内存里，等库有空了再一个个查）。名额（[`AuthSlot`]）**跟着真正执行查询的阻塞任务走**：认证查询和其后的
///    调用方解析（`CallerResolver::resolve`）共用同一个名额，两段阻塞任务都结束才丢弃。
///    不能留在请求 future 上：请求被取消时 future 先丢，名额先还，而阻塞任务还在等库的锁、查 SQLite，
///    反复「发起再取消」就能攒出远超上限的阻塞任务（R3 N2；解析那段是 L-5）。
/// 2. **全局查询预算（总闸）**：一个令牌桶，只有**最近没有认证成功过的凭证前缀**才消耗它。随机假凭证的前缀永远是新的，
///    所以它们合起来每分钟最多只能让库查 `unknown` 这么多次；而最近认证成功过的前缀（真实在用的凭证，比如 eduai 后端的
///    服务凭证）不消耗它，只受第 1 条的在途个数约束——攻击者把预算耗光，在用的凭证照样能认证。
///    认证失败（凭证被撤销、过期）会把前缀从「成功过」里去掉。
/// 3. **按来源的份额**：每个来源（IPv6 取 /64）有自己的一个令牌桶，只有同样「没有认证成功过的前缀」才消耗它，
///    而且**在全局预算之前扣**：一个来源先用自己的份额，用完了就被自己的份额挡住，**不会碰全局预算**；全局预算拒绝时，
///    刚扣的来源令牌退回去。这样一个来源刷随机 `idk_` key，最多耗掉它自己那一份，耗不光全局预算——别的来源冷的合法凭证
///    （重启之后、刚签发、被挤出「已知」表）照常拿得到全局预算里的令牌（#603 N4：原来全局预算只有一个桶，
///    一个来源 10 秒就能耗光 200 个令牌，之后 100/100 个冷的合法 key 全拿 429）。
///
/// **来源地址的作用到此为止**：它只决定一个「没有认证成功过的前缀」从哪个桶里扣令牌。最近认证成功过的前缀完全不看来源
/// （学校共享出口下，一个学生在用的凭证不会因为同一个出口上别人在发垃圾请求而被拒绝，设计稿 §9 第 3 条）。
/// 代价写在设计稿 S5-12：同一个来源下**冷的**凭证（还没认证成功过，认证之前分不出它和随机假凭证）要和那个来源上的垃圾请求
/// 共用这一份额；它认证成功一次，之后就是「已知」的，不再受影响。成员 / 租户 / 平台三级限流（`limits`）照旧在认证之后。
///
/// 数据库查询次数的上界：`min(全局突发 + 全局速率 × t, 来源数 × (单来源突发 + 单来源速率 × t))`，和请求个数无关。
pub struct AuthBudget {
    slots: Arc<Semaphore>,
    unknown_limit: RateLimit,
    unknown: Mutex<Bucket>,
    source_limit: RateLimit,
    sources: Mutex<Bounded<IpAddr, Bucket>>,
    /// 最近认证成功过的前缀。LRU：热凭证每个请求都在被用，表满了丢的是最久没用的（见 [`Bounded::lru`]）。
    known: Mutex<Bounded<u64, ()>>,
}

impl std::fmt::Debug for AuthBudget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthBudget")
            .field("available_slots", &self.slots.available_permits())
            .finish_non_exhaustive()
    }
}

/// 一次认证查询占着的名额；丢弃时释放。要交给执行查询的阻塞任务持有（见 [`AuthBudget`]），不能留在会被取消的 future 上。
#[must_use = "dropping the slot releases the in-flight authentication slot"]
pub struct AuthSlot {
    _permit: OwnedSemaphorePermit,
}

impl std::fmt::Debug for AuthSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthSlot").finish_non_exhaustive()
    }
}

impl AuthBudget {
    /// `unknown` 是全局的查询预算（总闸），`per_source` 是单个来源的份额。后者应当明显小于前者（见 [`AuthBudget`]：
    /// 一个来源耗不光总闸靠的就是这个比例；启动时 `IngressConfig::budget_warnings` 会对不满足的配置打警告）。
    pub fn new(
        max_concurrent: usize,
        unknown: RateLimit,
        per_source: RateLimit,
        now_ms: i64,
    ) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(max_concurrent.max(1))),
            unknown_limit: unknown,
            unknown: Mutex::new(Bucket::full(&unknown, now_ms)),
            source_limit: per_source,
            sources: Mutex::new(Bounded::lru(MAX_SOURCE_ENTRIES)),
            known: Mutex::new(Bounded::lru(MAX_KNOWN_ENTRIES)),
        }
    }

    /// 一个请求要开始认证查询了。`source` 是请求的客户端来源（受信代理规则解析过的，没有来源信息时是 `0.0.0.0`），
    /// `prefix` 是 [`FailureThrottle::key_of`] 给的键（格式不对没有键，按「没成功过」算）。
    /// `Err(秒数)` = 满了，直接拒绝（该等多久，至少 1）；没有碰库。
    ///
    /// 「没成功过」的前缀依次过三关，每一关拒绝时前面扣的都退回去：来源自己的份额 -> 全局预算 -> 在途名额。
    /// 来源的份额排在全局预算**前面**：被自己份额挡住的来源根本碰不到全局预算。一次只持一把锁，不嵌套。
    pub fn admit(&self, source: IpAddr, prefix: Option<u64>, now_ms: i64) -> Result<AuthSlot, u64> {
        // 「已知」表是 LRU：这一次取用同时把前缀标成最近用过，热凭证不会被别的凭证挤出表
        let known = prefix.is_some_and(|prefix| lock(&self.known).get_mut(&prefix).is_some());
        let mut charged = None;
        if !known {
            let key = source_key(source);
            self.take_from_source(key, now_ms)?;
            if let Err(retry) = lock(&self.unknown).try_take(&self.unknown_limit, now_ms) {
                self.give_back_to_source(key);
                return Err(retry);
            }
            charged = Some(key);
        }
        match Arc::clone(&self.slots).try_acquire_owned() {
            Ok(permit) => Ok(AuthSlot { _permit: permit }),
            Err(_) => {
                if let Some(key) = charged {
                    // 没有真的开始查：两个令牌都退回去
                    lock(&self.unknown).give_back(&self.unknown_limit);
                    self.give_back_to_source(key);
                }
                Err(1)
            }
        }
    }

    fn take_from_source(&self, key: IpAddr, now_ms: i64) -> Result<(), u64> {
        let limit = self.source_limit;
        lock(&self.sources)
            .get_or_insert_with(
                &key,
                || Bucket::full(&limit, now_ms),
                |bucket| bucket.is_full_at(&limit, now_ms),
                |_| true,
            )
            .try_take(&limit, now_ms)
    }

    fn give_back_to_source(&self, key: IpAddr) {
        if let Some(bucket) = lock(&self.sources).get_mut(&key) {
            bucket.give_back(&self.source_limit);
        }
    }

    /// 这个前缀认证成功了：之后它的请求不再消耗查询预算。
    pub fn record_success(&self, prefix: u64) {
        lock(&self.known).get_or_insert_with(&prefix, || (), |_| false, |_| true);
    }

    /// 这个前缀认证失败了（撤销、过期、被冻结）：不再算「成功过」。
    pub fn record_failure(&self, prefix: u64) {
        lock(&self.known).remove(&prefix);
    }

    /// 当前空闲的认证名额（测试和诊断用）。
    pub fn available_slots(&self) -> usize {
        self.slots.available_permits()
    }
}

/// 失败计数用的前缀长度：明文凭证 `idk_` 之后的前 16 个十六进制字符（64 位）。
pub const PREFIX_HEX_CHARS: usize = 16;

#[derive(Debug, Clone, Copy)]
struct Failures {
    window_start_ms: i64,
    count: u32,
}

/// 认证失败的计数。见模块文档。
pub struct FailureThrottle {
    max_failures: u32,
    window_ms: i64,
    /// 每个进程随机的盐：键是 `盐 + 前缀` 的哈希，内存里没有任何明文凭证的片段，也没人能预先算出一个前缀的键。
    salt: RandomState,
    table: Mutex<Bounded<u64, Failures>>,
}

impl std::fmt::Debug for FailureThrottle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FailureThrottle")
            .field("max_failures", &self.max_failures)
            .field("window_ms", &self.window_ms)
            .finish_non_exhaustive()
    }
}

impl FailureThrottle {
    /// 同一个前缀在 `window_secs` 秒内失败 `max_failures` 次之后，窗口内的后续请求被限流。
    pub fn new(max_failures: u32, window_secs: u64) -> Self {
        Self {
            max_failures: max_failures.max(1),
            window_ms: i64::try_from(window_secs.saturating_mul(1000)).unwrap_or(i64::MAX),
            salt: RandomState::new(),
            table: Mutex::new(Bounded::new(MAX_FAILURE_ENTRIES)),
        }
    }

    /// 一个明文凭证（`VirtualKeySecret::expose_secret`，格式已经校验过）的失败计数键：前 [`PREFIX_HEX_CHARS`] 个十六进制
    /// 字符的加盐哈希。格式不对返回 `None`。
    pub fn key_of(&self, secret: &str) -> Option<u64> {
        let payload = secret.strip_prefix(idoris_tenancy::virtual_key::VIRTUAL_KEY_PREFIX)?;
        let head = payload.get(..PREFIX_HEX_CHARS)?;
        head.bytes()
            .all(|byte| byte.is_ascii_hexdigit())
            .then(|| self.salt.hash_one(head))
    }

    fn expired(&self, entry: &Failures, now_ms: i64) -> bool {
        now_ms.saturating_sub(entry.window_start_ms) >= self.window_ms
    }

    /// 认证之前先问一次：这个前缀已经被限流了吗？`Err(秒数)` = 被限流，该等多久（至少 1）。
    pub fn check(&self, prefix: u64, now_ms: i64) -> Result<(), u64> {
        let mut table = lock(&self.table);
        let Some(entry) = table.get_mut(&prefix) else {
            return Ok(());
        };
        if self.expired(entry, now_ms) || entry.count < self.max_failures {
            return Ok(());
        }
        let remaining_ms = entry
            .window_start_ms
            .saturating_add(self.window_ms)
            .saturating_sub(now_ms);
        Err(u64::try_from(remaining_ms)
            .unwrap_or(0)
            .div_ceil(1000)
            .max(1))
    }

    /// 一次认证失败。
    pub fn record_failure(&self, prefix: u64, now_ms: i64) {
        let window_ms = self.window_ms;
        let mut table = lock(&self.table);
        let entry = table.get_or_insert_with(
            &prefix,
            || Failures {
                window_start_ms: now_ms,
                count: 0,
            },
            |entry| now_ms.saturating_sub(entry.window_start_ms) >= window_ms,
            |_| true,
        );
        if now_ms.saturating_sub(entry.window_start_ms) >= window_ms {
            *entry = Failures {
                window_start_ms: now_ms,
                count: 0,
            };
        }
        entry.count = entry.count.saturating_add(1);
    }

    /// 这个前缀认证成功了（比如被冻结的凭证被解冻）：清掉它的失败记录。
    pub fn record_success(&self, prefix: u64) {
        lock(&self.table).remove(&prefix);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    fn limit(rpm: u32, burst: u32) -> RateLimit {
        RateLimit::new(rpm, burst).unwrap()
    }

    /// 不会成为约束的按来源份额（只看全局预算的测试用它）。
    fn roomy() -> RateLimit {
        limit(600_000, 100_000)
    }

    /// 单一来源的测试请求。
    fn src() -> IpAddr {
        ip("203.0.113.9")
    }

    // ---------- 来源键 ----------

    #[test]
    fn ipv6_sources_share_a_64_bit_prefix_and_ipv4_mapped_addresses_collapse_to_ipv4() {
        assert_eq!(source_key(ip("203.0.113.9")), ip("203.0.113.9"));
        assert_eq!(source_key(ip("::ffff:203.0.113.9")), ip("203.0.113.9"));
        assert_eq!(
            source_key(ip("2001:db8:1:2:aaaa:bbbb:cccc:dddd")),
            ip("2001:db8:1:2::")
        );
        assert_eq!(
            source_key(ip("2001:db8:1:2:1::1")),
            source_key(ip("2001:db8:1:2:2::2"))
        );
        assert_ne!(
            source_key(ip("2001:db8:1:2::1")),
            source_key(ip("2001:db8:1:3::1"))
        );
    }

    // ---------- 没有凭证的请求 ----------

    #[test]
    fn a_noisy_source_is_limited_by_its_own_soft_quota_and_nobody_else_is() {
        let limiter = AnonymousLimiter::new(limit(60, 3), limit(60_000, 10_000), 0);
        let noisy = ip("203.0.113.9");
        for _ in 0..3 {
            assert!(limiter.admit(noisy, 0).is_ok());
        }
        assert_eq!(limiter.admit(noisy, 0), Err(1));
        // 另一个来源完全不受影响（共享出口只是同一个来源；别的来源有自己的配额）
        assert!(limiter.admit(ip("198.51.100.1"), 0).is_ok());
        // 过一秒，吵闹的来源恢复一个：软配额是速率，不是封禁，不记仇
        assert!(limiter.admit(noisy, 1_000).is_ok());
    }

    #[test]
    fn the_global_quota_caps_all_anonymous_traffic_together() {
        let limiter = AnonymousLimiter::new(limit(60_000, 10_000), limit(60, 4), 0);
        for last in 1..=4 {
            assert!(limiter.admit(ip(&format!("198.51.100.{last}")), 0).is_ok());
        }
        // 第 5 个来源：自己的配额是满的，但全局用完了
        assert_eq!(limiter.admit(ip("198.51.100.5"), 0), Err(1));
    }

    #[test]
    fn a_source_refused_by_its_own_quota_never_touches_the_global_quota() {
        // 全局只有 2 个令牌。吵闹的来源（自己的突发是 1）刷 50 次：只有第 1 次扣了全局，其余 49 次被自己的配额挡住。
        let limiter = AnonymousLimiter::new(limit(60, 1), limit(60, 2), 0);
        let noisy = ip("203.0.113.9");
        assert!(limiter.admit(noisy, 0).is_ok());
        for _ in 0..49 {
            assert!(limiter.admit(noisy, 0).is_err());
        }
        // 全局还剩 1 个令牌，别的来源用得上
        assert!(limiter.admit(ip("198.51.100.1"), 0).is_ok());
        assert!(limiter.admit(ip("198.51.100.2"), 0).is_err());
    }

    #[test]
    fn a_global_refusal_gives_the_source_token_back() {
        let limiter = AnonymousLimiter::new(limit(60, 1), limit(60, 1), 0);
        let a = ip("198.51.100.1");
        let b = ip("198.51.100.2");
        assert!(limiter.admit(a, 0).is_ok()); // 全局用完
        assert!(limiter.admit(b, 0).is_err()); // b 被全局拒绝，b 的令牌必须退回
        // 一秒后全局恢复 1 个：b 的软配额没有被那次被拒的请求吃掉，所以 b 能过
        assert!(limiter.admit(b, 1_000).is_ok());
    }

    // ---------- 认证之前的有界工作 ----------

    #[test]
    fn the_in_flight_authentication_slots_are_bounded_and_released_on_drop() {
        let budget = AuthBudget::new(2, limit(60_000, 10_000), roomy(), 0);
        let a = budget.admit(src(), Some(1), 0).unwrap();
        let b = budget.admit(src(), Some(2), 0).unwrap();
        assert_eq!(budget.available_slots(), 0);
        // 满了：直接拒绝，不排队
        assert_eq!(budget.admit(src(), Some(3), 0).unwrap_err(), 1);
        drop(a);
        assert_eq!(budget.available_slots(), 1);
        let c = budget.admit(src(), Some(3), 0).unwrap();
        drop((b, c));
        assert_eq!(budget.available_slots(), 2);
    }

    #[test]
    fn a_flood_of_unknown_prefixes_is_capped_by_the_budget_and_a_refusal_gives_the_token_back() {
        // 预算：突发 3，每分钟 60（每秒 1 个）
        let budget = AuthBudget::new(100, limit(60, 3), roomy(), 0);
        for prefix in 0..3u64 {
            drop(budget.admit(src(), Some(prefix), 0).unwrap());
        }
        // 第 4 个随机前缀：预算空了，直接拒绝，等 1 秒
        assert_eq!(budget.admit(src(), Some(1_000), 0).unwrap_err(), 1);
        assert_eq!(
            budget.admit(src(), None, 0).unwrap_err(),
            1,
            "no prefix counts as unknown"
        );
        // 过一秒恢复一个
        assert!(budget.admit(src(), Some(1_001), 1_000).is_ok());

        // 名额满了拒绝时，刚取的令牌退回去（这次查询没有真的开始）
        let budget = AuthBudget::new(1, limit(60, 1), roomy(), 0);
        let held = budget.admit(src(), Some(1), 0).unwrap(); // 取走唯一的令牌和唯一的名额
        assert!(budget.admit(src(), Some(2), 1_000).is_err()); // 令牌回来了一个，但名额满了
        drop(held);
        assert!(
            budget.admit(src(), Some(3), 1_000).is_ok(),
            "the refused attempt did not eat the refilled token"
        );
    }

    #[test]
    fn a_prefix_that_recently_authenticated_is_not_charged_and_a_failure_forgets_it() {
        let budget = AuthBudget::new(100, limit(60, 1), roomy(), 0);
        let legit = 0xAAAA_u64;
        drop(budget.admit(src(), Some(legit), 0).unwrap()); // 预算的唯一令牌
        budget.record_success(legit);
        // 攻击者把预算耗光：新前缀进不来
        assert!(budget.admit(src(), Some(7), 0).is_err());
        // 在用的凭证照样能认证，一次又一次
        for _ in 0..50 {
            drop(budget.admit(src(), Some(legit), 0).unwrap());
        }
        // 它被撤销（认证失败）之后，又要和别的未知前缀一样消耗预算
        budget.record_failure(legit);
        assert!(budget.admit(src(), Some(legit), 0).is_err());
    }

    #[test]
    fn the_known_prefix_table_stays_bounded() {
        let budget = AuthBudget::new(10, limit(60_000, 10_000), roomy(), 0);
        for prefix in 0..50_000u64 {
            budget.record_success(prefix);
        }
        assert!(lock(&budget.known).len() <= MAX_KNOWN_ENTRIES);
    }

    /// #603 N4：「已知前缀」表满了淘汰的是**最久没用过**的（LRU），不是最早进表的（FIFO）。热凭证每次请求都在被用，
    /// 表被别的凭证挤满之后它必须还在表里；原来按进表先后淘汰，最早进表的热凭证第一个被丢，下一次请求当成「冷」的去抢
    /// 未知前缀预算——预算被攻击者耗光时，它就拿 429。
    #[test]
    fn a_hot_prefix_stays_known_after_the_table_overflows_and_the_least_recently_used_one_goes() {
        // 未知前缀的预算只有一个令牌，下面第一步就把它用掉：之后 `admit` 成功 = 这个前缀此刻在「已知」表里
        let budget = AuthBudget::new(10, limit(60, 1), roomy(), 0);
        drop(budget.admit(src(), Some(1), 0).unwrap());
        assert!(
            budget.admit(src(), Some(2), 0).is_err(),
            "the budget is spent"
        );

        let hot = u64::MAX;
        let first_cold = 1_000_000_u64;
        budget.record_success(hot);
        // 往表里连续塞进三倍容量的别的凭证（都是成功认证过的）；热凭证夹在中间被一直使用
        for round in 0..(3 * MAX_KNOWN_ENTRIES as u64) {
            budget.record_success(first_cold + round);
            if round.is_multiple_of(64) {
                assert!(
                    budget.admit(src(), Some(hot), 0).is_ok(),
                    "the hot prefix fell out of the known table after {round} other insertions"
                );
            }
        }
        assert!(budget.admit(src(), Some(hot), 0).is_ok());
        assert!(lock(&budget.known).len() <= MAX_KNOWN_ENTRIES);
        // 最久没用过的那个才是被丢的：第一个塞进去、之后再没碰过的冷凭证早就不在表里了
        assert!(
            budget.admit(src(), Some(first_cold), 0).is_err(),
            "an entry nobody touched since the start is the one that leaves"
        );
    }

    // ---------- 认证之前：按来源的份额（#603 N4） ----------

    /// 一个来源先用自己的份额；用完了被自己的份额挡住，不会碰全局预算——它刷不光全局预算，别的来源还有。
    #[test]
    fn a_source_spends_its_own_share_first_and_can_never_drain_the_global_budget() {
        // 全局 3 个令牌（突发 3，每秒 1 个），每个来源 2 个（突发 2）
        let budget = AuthBudget::new(100, limit(60, 3), limit(60, 2), 0);
        let (noisy, other, third) = (ip("198.51.100.1"), ip("198.51.100.2"), ip("198.51.100.3"));
        let mut admitted = 0;
        for prefix in 0..50u64 {
            if budget.admit(noisy, Some(prefix), 0).map(drop).is_ok() {
                admitted += 1;
            }
        }
        assert_eq!(admitted, 2, "the noisy source got exactly its own share");
        // 全局预算只被用掉 2 个，不是 3 个：被自己份额挡住的那 48 次没有碰它
        assert!(budget.admit(other, Some(1_000), 0).is_ok());
        // 现在全局空了（总闸是真的）：第三个来源（它自己的份额是满的）也进不来
        assert_eq!(budget.admit(third, Some(1_001), 0).unwrap_err(), 1);
        // 过一秒全局补一个，第三个来源就能进（它被全局拒绝的那一次没有吃掉它自己的令牌）
        assert!(budget.admit(third, Some(1_001), 1_000).is_ok());
    }

    #[test]
    fn a_refusal_by_the_global_budget_gives_the_source_token_back() {
        // 全局只有 1 个令牌，每个来源 1 个
        let budget = AuthBudget::new(100, limit(60, 1), limit(60, 1), 0);
        let (a, b) = (ip("198.51.100.1"), ip("198.51.100.2"));
        assert!(budget.admit(a, Some(1), 0).is_ok()); // 全局用完
        assert!(budget.admit(b, Some(2), 0).is_err()); // b 被全局拒绝：它自己的令牌必须退回
        // 一秒后全局恢复 1 个。b 的份额没有被那次被拒的请求吃掉，所以 b 能过
        assert!(budget.admit(b, Some(3), 1_000).is_ok());
    }

    #[test]
    fn a_refusal_for_lack_of_a_slot_gives_both_tokens_back() {
        // 只有 1 个在途名额；全局 1 个令牌、来源 1 个令牌
        let budget = AuthBudget::new(1, limit(60, 1), limit(60, 1), 0);
        let source = ip("198.51.100.1");
        budget.record_success(77);
        let held = budget.admit(source, Some(77), 0).unwrap(); // 已知前缀：只占名额，不动令牌
        assert!(
            budget.admit(source, Some(1), 0).is_err(),
            "no slot: refused before the lookup"
        );
        drop(held);
        assert!(
            budget.admit(source, Some(1), 0).is_ok(),
            "neither the global nor the source token was eaten by the refused attempt"
        );
    }

    /// 最近认证成功过的前缀完全不看来源：来源的份额和全局预算都空了，它照样进。
    #[test]
    fn a_recently_authenticated_prefix_ignores_the_source_entirely() {
        let budget = AuthBudget::new(100, limit(60, 1), limit(60, 1), 0);
        let source = ip("198.51.100.1");
        budget.record_success(7);
        drop(budget.admit(source, Some(1), 0).unwrap()); // 令牌（全局和来源的）都用光
        assert!(budget.admit(source, Some(2), 0).is_err());
        for _ in 0..50 {
            drop(budget.admit(source, Some(7), 0).unwrap());
        }
        // 别的来源带着这个前缀（比如同一张凭证从另一个出口来）同样不看来源
        drop(budget.admit(ip("203.0.113.9"), Some(7), 0).unwrap());
    }

    #[test]
    fn the_source_share_is_keyed_by_the_normalised_source_and_the_table_stays_bounded() {
        let budget = AuthBudget::new(100, limit(600_000, 100_000), limit(60, 2), 0);
        // IPv6 取 /64：同一个 /64 里的两个地址是同一个来源（共用 2 个令牌）
        assert!(budget.admit(ip("2001:db8:1:2:aaaa::1"), Some(1), 0).is_ok());
        assert!(budget.admit(ip("2001:db8:1:2:bbbb::7"), Some(2), 0).is_ok());
        assert!(
            budget
                .admit(ip("2001:db8:1:2:cccc::3"), Some(3), 0)
                .is_err()
        );
        assert!(budget.admit(ip("2001:db8:1:3::1"), Some(4), 0).is_ok());
        // IPv4 映射地址就是它的 IPv4 地址
        assert!(budget.admit(ip("203.0.113.7"), Some(5), 0).is_ok());
        assert!(budget.admit(ip("::ffff:203.0.113.7"), Some(6), 0).is_ok());
        assert!(budget.admit(ip("203.0.113.7"), Some(7), 0).is_err());
        // 来源表有上限：攻击者没法靠换来源把内存撑大
        let many = AuthBudget::new(100, limit(600_000, 100_000), limit(60, 2), 0);
        for n in 0..50_000u32 {
            let source = IpAddr::from(std::net::Ipv4Addr::from(0x0A00_0000 + n));
            drop(many.admit(source, Some(u64::from(n)), 0));
        }
        assert!(lock(&many.sources).len() <= MAX_SOURCE_ENTRIES);
    }

    // ---------- 凭证前缀 ----------

    #[test]
    fn the_failure_key_covers_the_first_sixteen_hex_digits_and_is_salted() {
        let throttle = FailureThrottle::new(3, 60);
        let secret = |tail: &str| format!("idk_0123456789abcdef{tail}");
        let tail = "0".repeat(48);
        let base = throttle.key_of(&secret(&tail)).unwrap();
        // 前 16 位之后的字符不同：同一个键（输错最后几位再试，聚在一起）
        assert_eq!(base, throttle.key_of(&secret(&"f".repeat(48))).unwrap());
        // 前 16 位里任何一位不同：不同的键（只知道 masked 显示的前 8 位，投不了毒）
        let other_head = format!("idk_0123456789abcdee{tail}");
        assert_ne!(base, throttle.key_of(&other_head).unwrap());
        let other_start = format!("idk_1123456789abcdef{tail}");
        assert_ne!(base, throttle.key_of(&other_start).unwrap());
        // 加盐：另一个实例（另一个进程）对同一个前缀算出的键不同，没人能预先算出键
        assert_ne!(
            base,
            FailureThrottle::new(3, 60).key_of(&secret(&tail)).unwrap()
        );
        // 格式不对没有键
        for bad in [
            "idk_short",
            "sk_0123456789abcdef0123",
            "idk_zzzzzzzzzzzzzzzz0123",
            "",
        ] {
            assert_eq!(throttle.key_of(bad), None, "{bad:?}");
        }
    }

    // ---------- 认证失败 ----------

    #[test]
    fn failures_throttle_only_their_own_prefix_and_only_after_the_limit() {
        let throttle = FailureThrottle::new(3, 60);
        let (bad, other) = (0xDEAD_BEEF_u64, 0x1234_5678_u64);
        for _ in 0..3 {
            assert_eq!(throttle.check(bad, 0), Ok(()));
            throttle.record_failure(bad, 0);
        }
        // 第 4 次：同一个前缀被限流，等到窗口结束（60 秒）
        assert_eq!(throttle.check(bad, 0), Err(60));
        assert_eq!(throttle.check(bad, 59_001), Err(1));
        // 另一个前缀（同一个出口后面的另一个学生）完全不受影响
        assert_eq!(throttle.check(other, 0), Ok(()));
        // 窗口结束后恢复
        assert_eq!(throttle.check(bad, 60_000), Ok(()));
    }

    #[test]
    fn the_failure_window_resets_and_a_success_forgives() {
        let throttle = FailureThrottle::new(2, 10);
        let prefix = 7_u64;
        throttle.record_failure(prefix, 0);
        throttle.record_failure(prefix, 5_000);
        assert!(throttle.check(prefix, 6_000).is_err());
        // 窗口过了：新的一次失败从 1 开始计数，不是接着上一个窗口的 2
        throttle.record_failure(prefix, 10_001);
        assert_eq!(throttle.check(prefix, 10_002), Ok(()));
        throttle.record_failure(prefix, 10_003);
        assert!(throttle.check(prefix, 10_004).is_err());
        // 成功清零
        throttle.record_success(prefix);
        assert_eq!(throttle.check(prefix, 10_005), Ok(()));
    }

    #[test]
    fn a_flood_of_distinct_bad_prefixes_neither_throttles_a_valid_one_nor_grows_the_table() {
        let throttle = FailureThrottle::new(5, 60);
        let valid_prefix = 0xAAAA_0001_u64;
        // 攻击者发 50000 个各不相同的垃圾前缀，每个失败一次
        for junk in 0..50_000u64 {
            throttle.record_failure(junk, 1_000);
        }
        // 这些垃圾前缀没有一个攒到 5 次：有效凭证的前缀不受影响
        assert_eq!(throttle.check(valid_prefix, 1_000), Ok(()));
        assert!(lock(&throttle.table).len() <= MAX_FAILURE_ENTRIES);
    }
}
