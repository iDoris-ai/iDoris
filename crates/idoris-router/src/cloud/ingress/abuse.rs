//! 公网防滥用（计算服务 S5，设计稿 §9 第 3 条）：只在云形态加载。
//!
//! > 资格由开通方证明，IP 和设备只作辅助信号，避免学校共享出口误伤一整个班。
//!
//! 落到代码上是两条硬规则：
//!
//! 1. **资格 = 凭证。** 带着有效凭证的请求，不论它从哪个地址来、这个地址刚刚有多少人认证失败，都不会被任何
//!    和来源地址有关的东西拦住。三级上限（`limits`）的键全部来自凭证推导出来的身份，没有地址。
//! 2. **地址只用在「没有资格的请求」上。** 没有凭证（或凭证格式根本不对）的请求没有任何人需要保护，
//!    对它们按来源地址分一个软配额；这个软配额是**速率**，不是封禁：窗口过去就恢复，不记仇，不影响同一个地址上
//!    带着有效凭证的请求。
//!
//! # 两个限流器
//!
//! | | 谁被它限制 | 键 | 作用 |
//! |---|---|---|---|
//! | [`AnonymousLimiter`] | 没有凭证、或者认证失败的请求 | 来源地址（IPv6 取 /64）的软配额 + 全局总配额 | 防扫描、防海量垃圾请求把入口占满。**永远不会被带着有效凭证的请求碰到。** |
//! | [`FailureThrottle`] | 带着格式正确、但认证失败的凭证的请求 | **凭证前缀**（凭证明文 `idk_` 之后的前 16 个十六进制字符，64 位，加盐哈希之后做键） | 防爆破，也防一个带着已撤销凭证死循环重试的客户端把库打满。同一个前缀在窗口内失败太多次，之后同前缀的请求先在内存里被拒绝，不碰数据库。 |
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
//! 两张表的键（地址、前缀）有一部分来自攻击者，所以都是 [`Bounded`]：满了先丢空闲条目，再丢最老的，
//! 内存用量不由攻击者决定（见 `bucket`）。

use std::collections::hash_map::RandomState;
use std::hash::BuildHasher;
use std::net::{IpAddr, Ipv6Addr};
use std::sync::{Mutex, MutexGuard, PoisonError};

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
