//! 令牌桶和有上限的键值表（计算服务 S5 入口限流的两块基础积木）。
//!
//! # 令牌桶：整数、精确、时钟可拨
//!
//! 速率写成「每分钟多少个请求」，桶容量是 `burst` 个令牌。令牌数不用浮点：内部单位是 `1/60000` 个令牌，
//! 这样「每毫秒回填 `requests_per_minute` 个单位」是**精确的整数运算**，没有余数丢失（每分钟 20 个请求的速率，
//! 逐毫秒回填，不会因为取整永远攒不满一个令牌）。时间由调用方传入（毫秒），所以测试可以拨时钟，
//! 不需要睡觉。
//!
//! **桶里的时间只前进**：同一段时间不能补两次额度。两个请求几乎同时取时、却按相反的顺序抢到桶的锁，晚到的那个带着
//! 更早的时间，如果让它把桶里的时间拨回去，下一个请求就会把「已经补过的那一小段」再补一次（间隔 1 毫秒的两个请求都成功）。
//! 所以倒退不超过 [`OUT_OF_ORDER_TOLERANCE_MS`] 的时间读数一律当作乱序：不回填，也不拨回桶里的时间。
//! 倒退得更多才是时钟被拨回（NTP 校时、手动改时间）：不回填、并重新对齐，不会因此冻结一个桶。
//! 调用方应当传**单调**时间（`Clock::monotonic_ms`），墙上时钟被拨回就不会走到这里。
//!
//! 拒绝时算得出**确切**的等待：还差多少单位 ÷ 每毫秒回填多少，向上取整到秒（至少 1 秒），放进 `Retry-After`。
//!
//! # 有上限的键值表
//!
//! 限流表的键有一部分来自攻击者（来源地址、凭证前缀），不能让它们决定内存用量。[`Bounded`] 满了之后先丢
//! **空闲**的条目（桶是满的、没有在途请求、窗口已过期——一个空闲条目和「从来没出现过」不可区分，丢掉不改变任何判断），
//! 还不够就丢最老的**可丢**条目（有在途请求的永远不丢）。键的哈希用 `std` 的 `RandomState`（每张表随机种子），
//! 攻击者没法构造哈希冲突。

use std::collections::HashMap;
use std::collections::hash_map::Entry as MapEntry;
use std::hash::Hash;

/// 一个令牌的内部单位数：`60_000` = 一分钟的毫秒数，所以每毫秒回填 `requests_per_minute` 个单位 = 精确。
pub const UNITS_PER_TOKEN: u64 = 60_000;

/// 时间读数倒退不超过这么多毫秒，当作乱序（见模块文档）：不拨回桶里的时间。
pub const OUT_OF_ORDER_TOLERANCE_MS: i64 = 5_000;

/// 速率上限：每分钟 `requests_per_minute` 个请求，桶容量 `burst`。两个数都至少是 1。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    requests_per_minute: u32,
    burst: u32,
}

impl RateLimit {
    pub fn new(requests_per_minute: u32, burst: u32) -> Result<Self, String> {
        if requests_per_minute == 0 {
            return Err("requests_per_minute must be at least 1".to_string());
        }
        if burst == 0 {
            return Err("burst must be at least 1".to_string());
        }
        Ok(Self {
            requests_per_minute,
            burst,
        })
    }

    pub fn requests_per_minute(&self) -> u32 {
        self.requests_per_minute
    }

    pub fn burst(&self) -> u32 {
        self.burst
    }

    fn capacity(&self) -> u64 {
        u64::from(self.burst) * UNITS_PER_TOKEN
    }
}

/// 一个令牌桶。限额不存在桶里（由持有者传入），所以同一个桶结构可以服务不同的限额。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bucket {
    units: u64,
    last_ms: i64,
}

impl Bucket {
    /// 一个满的桶：新出现的键从满桶开始（突发额度可用）。
    pub fn full(limit: &RateLimit, now_ms: i64) -> Self {
        Self {
            units: limit.capacity(),
            last_ms: now_ms,
        }
    }

    fn units_at(&self, limit: &RateLimit, now_ms: i64) -> u64 {
        if now_ms <= self.last_ms {
            return self.units;
        }
        let elapsed = u128::from(now_ms.saturating_sub(self.last_ms).unsigned_abs());
        let added = elapsed.saturating_mul(u128::from(limit.requests_per_minute));
        let total = u128::from(self.units).saturating_add(added);
        u64::try_from(total.min(u128::from(limit.capacity()))).unwrap_or(limit.capacity())
    }

    fn refill(&mut self, limit: &RateLimit, now_ms: i64) {
        self.units = self.units_at(limit, now_ms);
        // 桶里的时间只前进：乱序到达的较早读数（倒退不超过容忍范围）不拨回它，否则同一段时间会被补两次额度。
        // 倒退得更多是时钟被拨回：只对齐，不回填，也不让桶因此冻结
        if now_ms >= self.last_ms || self.last_ms.saturating_sub(now_ms) > OUT_OF_ORDER_TOLERANCE_MS
        {
            self.last_ms = now_ms;
        }
    }

    /// 取一个令牌。成功 `Ok`；桶里不够返回 `Err(还要等几秒)`（向上取整，至少 1）。失败不改变令牌数。
    pub fn try_take(&mut self, limit: &RateLimit, now_ms: i64) -> Result<(), u64> {
        self.refill(limit, now_ms);
        if self.units >= UNITS_PER_TOKEN {
            self.units -= UNITS_PER_TOKEN;
            return Ok(());
        }
        let deficit = UNITS_PER_TOKEN - self.units;
        let retry_ms = deficit.div_ceil(u64::from(limit.requests_per_minute));
        Err(retry_ms.div_ceil(1000).max(1))
    }

    /// 退回一个令牌（这次取用没有真正发生：比如后面一级拒绝了请求）。不会超过容量。
    pub fn give_back(&mut self, limit: &RateLimit) {
        self.units = self
            .units
            .saturating_add(UNITS_PER_TOKEN)
            .min(limit.capacity());
    }

    /// 在 `now_ms` 这一刻桶是不是满的（不改变桶）。满桶 = 空闲：和从没出现过的键一样。
    pub fn is_full_at(&self, limit: &RateLimit, now_ms: i64) -> bool {
        self.units_at(limit, now_ms) >= limit.capacity()
    }

    /// 此刻还有多少个整令牌（测试和诊断用）。
    pub fn whole_tokens_at(&self, limit: &RateLimit, now_ms: i64) -> u64 {
        self.units_at(limit, now_ms) / UNITS_PER_TOKEN
    }
}

struct Slot<V> {
    value: V,
    seq: u64,
}

/// 有上限的键值表，见模块文档。
pub struct Bounded<K, V> {
    map: HashMap<K, Slot<V>>,
    cap: usize,
    next_seq: u64,
}

impl<K: Hash + Eq + Clone, V> Bounded<K, V> {
    pub fn new(cap: usize) -> Self {
        Self {
            map: HashMap::new(),
            cap: cap.max(1),
            next_seq: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn get_mut(&mut self, key: &K) -> Option<&mut V> {
        self.map.get_mut(key).map(|entry| &mut entry.value)
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        self.map.remove(key).map(|entry| entry.value)
    }

    /// 取（没有就按 `make` 建）一个条目。表满了要建新条目时先腾地方：丢 `idle` 的，再丢最老的 `droppable` 的。
    /// 所有条目都不可丢时（全是在途请求）允许临时超出上限：在途请求的个数本来就被并发上限卡着。
    pub fn get_or_insert_with(
        &mut self,
        key: &K,
        make: impl FnOnce() -> V,
        idle: impl Fn(&V) -> bool,
        droppable: impl Fn(&V) -> bool,
    ) -> &mut V {
        if !self.map.contains_key(key) && self.map.len() >= self.cap {
            self.make_room(idle, droppable);
        }
        let seq = self.next_seq;
        match self.map.entry(key.clone()) {
            MapEntry::Occupied(slot) => &mut slot.into_mut().value,
            MapEntry::Vacant(slot) => {
                self.next_seq = self.next_seq.wrapping_add(1);
                &mut slot.insert(Slot { value: make(), seq }).value
            }
        }
    }

    fn make_room(&mut self, idle: impl Fn(&V) -> bool, droppable: impl Fn(&V) -> bool) {
        self.map.retain(|_, entry| !idle(&entry.value));
        if self.map.len() < self.cap {
            return;
        }
        let mut candidates: Vec<(u64, K)> = self
            .map
            .iter()
            .filter(|(_, entry)| droppable(&entry.value))
            .map(|(key, entry)| (entry.seq, key.clone()))
            .collect();
        candidates.sort_by_key(|(seq, _)| *seq);
        let drop_count = (self.cap / 8).max(1);
        for (_, key) in candidates.into_iter().take(drop_count) {
            self.map.remove(&key);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn limit(rpm: u32, burst: u32) -> RateLimit {
        RateLimit::new(rpm, burst).unwrap()
    }

    #[test]
    fn a_rate_limit_needs_positive_numbers() {
        assert!(RateLimit::new(0, 1).is_err());
        assert!(RateLimit::new(1, 0).is_err());
        let ok = limit(60, 5);
        assert_eq!((ok.requests_per_minute(), ok.burst()), (60, 5));
    }

    #[test]
    fn the_burst_is_available_at_once_then_the_bucket_is_empty() {
        let l = limit(60, 3);
        let mut bucket = Bucket::full(&l, 1_000);
        for _ in 0..3 {
            assert_eq!(bucket.try_take(&l, 1_000), Ok(()));
        }
        // 第 4 个：60/分钟 = 每秒 1 个，要等 1 秒
        assert_eq!(bucket.try_take(&l, 1_000), Err(1));
        // 失败不改变令牌数：同一毫秒再试，等待时间不变
        assert_eq!(bucket.try_take(&l, 1_000), Err(1));
    }

    #[test]
    fn refill_is_exact_with_no_rounding_loss_even_when_polled_every_millisecond() {
        // 每分钟 20 个 = 每 3000 毫秒 1 个。每毫秒轮询一次也必须在第 3000 毫秒攒出 1 个。
        let l = limit(20, 1);
        let mut bucket = Bucket::full(&l, 0);
        assert_eq!(bucket.try_take(&l, 0), Ok(()));
        for ms in 1..3_000 {
            assert!(bucket.try_take(&l, ms).is_err(), "{ms}");
        }
        assert_eq!(bucket.try_take(&l, 3_000), Ok(()));
        assert!(bucket.try_take(&l, 3_001).is_err());
    }

    #[test]
    fn retry_after_is_the_exact_wait_rounded_up_to_whole_seconds() {
        let l = limit(20, 1); // 每 3 秒 1 个
        let mut bucket = Bucket::full(&l, 0);
        bucket.try_take(&l, 0).unwrap();
        assert_eq!(bucket.try_take(&l, 0), Err(3));
        assert_eq!(bucket.try_take(&l, 1_000), Err(2));
        assert_eq!(bucket.try_take(&l, 2_001), Err(1));
        assert_eq!(
            bucket.try_take(&l, 2_999),
            Err(1),
            "never 0: at least one second"
        );
        assert_eq!(bucket.try_take(&l, 3_000), Ok(()));
        // 很慢的速率：每分钟 1 个 = 60 秒
        let slow = limit(1, 1);
        let mut bucket = Bucket::full(&slow, 0);
        bucket.try_take(&slow, 0).unwrap();
        assert_eq!(bucket.try_take(&slow, 0), Err(60));
    }

    #[test]
    fn the_bucket_never_overfills_and_a_long_idle_gap_does_not_overflow() {
        let l = limit(u32::MAX, u32::MAX);
        let mut bucket = Bucket::full(&l, 0);
        assert_eq!(bucket.try_take(&l, i64::MAX), Ok(()));
        let l = limit(600, 2);
        let mut bucket = Bucket::full(&l, 0);
        bucket.try_take(&l, 0).unwrap();
        bucket.try_take(&l, 0).unwrap();
        // 过了很久：只回到容量 2
        assert_eq!(bucket.whole_tokens_at(&l, 10_000_000), 2);
        assert!(bucket.is_full_at(&l, 10_000_000));
        assert!(!bucket.is_full_at(&l, 0));
    }

    #[test]
    fn a_clock_that_goes_backwards_neither_refills_nor_freezes_the_bucket() {
        let l = limit(60, 1);
        let mut bucket = Bucket::full(&l, 10_000);
        bucket.try_take(&l, 10_000).unwrap();
        // 时钟回拨一小时：不回填
        assert!(bucket.try_take(&l, 10_000 - 3_600_000).is_err());
        // 但从新的时刻重新开始走：1 秒之后就有一个令牌（没有冻结一小时）
        assert_eq!(bucket.try_take(&l, 10_000 - 3_600_000 + 1_000), Ok(()));
    }

    /// R2 Medium#5：乱序到达的较早时间读数不能把桶里的时间拨回去，否则同一段时间被补两次额度。
    /// 每分钟 20 个、突发 1：B 在 3000ms 用掉令牌；较早取时、较晚才到的 A（时间 0）被拒，并且不能把桶拨回 0；
    /// C 在 3001ms：只比 B 晚 1ms，不该再有一个令牌。
    #[test]
    fn an_out_of_order_reading_does_not_refill_the_same_span_twice() {
        let l = limit(20, 1);
        let mut bucket = Bucket::full(&l, 0);
        assert_eq!(bucket.try_take(&l, 3_000), Ok(())); // B
        assert!(bucket.try_take(&l, 0).is_err(), "A: stale and empty"); // A
        assert!(
            bucket.try_take(&l, 3_001).is_err(),
            "C: 1 ms after B must not get a token"
        );
        // 真正过了 3 秒（每分钟 20 个 = 每 3 秒 1 个）才有下一个
        assert_eq!(bucket.try_take(&l, 6_000), Ok(()));
    }

    #[test]
    fn give_back_restores_a_token_up_to_the_capacity_only() {
        let l = limit(60, 2);
        let mut bucket = Bucket::full(&l, 0);
        bucket.try_take(&l, 0).unwrap();
        bucket.give_back(&l);
        assert_eq!(bucket.whole_tokens_at(&l, 0), 2);
        bucket.give_back(&l);
        assert_eq!(bucket.whole_tokens_at(&l, 0), 2, "capped at the burst");
    }

    #[test]
    fn the_table_drops_idle_entries_first_then_the_oldest_droppable_and_never_an_in_flight_one() {
        // 值 = (空闲?, 在途?)
        let idle = |v: &(bool, bool)| v.0;
        let droppable = |v: &(bool, bool)| !v.1;
        let mut table: Bounded<u32, (bool, bool)> = Bounded::new(8);
        // 先填满：键 0 在途（不可丢），键 1..8 非空闲
        table.get_or_insert_with(&0, || (false, true), idle, droppable);
        for key in 1..8 {
            table.get_or_insert_with(&key, || (false, false), idle, droppable);
        }
        assert_eq!(table.len(), 8);
        // 再来一个新键：没有空闲的，丢最老的可丢条目（键 1），在途的键 0 还在
        table.get_or_insert_with(&100, || (false, false), idle, droppable);
        assert!(
            table.get_mut(&0).is_some(),
            "an in-flight entry is never evicted"
        );
        assert!(
            table.get_mut(&1).is_none(),
            "the oldest droppable entry went"
        );
        assert!(table.get_mut(&2).is_some());
        assert!(table.len() <= 8);
        // 把几个条目标成空闲：下一次腾地方时它们先被丢
        for key in [3, 4, 5] {
            if let Some(v) = table.get_mut(&key) {
                v.0 = true;
            }
        }
        for key in 200..208 {
            table.get_or_insert_with(&key, || (false, false), idle, droppable);
            assert!(table.len() <= 8, "{}", table.len());
        }
        assert!(table.get_mut(&0).is_some());
    }

    #[test]
    fn a_table_full_of_in_flight_entries_may_exceed_its_cap_rather_than_lose_one() {
        let idle = |_: &bool| false;
        let droppable = |in_flight: &bool| !*in_flight;
        let mut table: Bounded<u32, bool> = Bounded::new(4);
        for key in 0..4 {
            table.get_or_insert_with(&key, || true, idle, droppable);
        }
        table.get_or_insert_with(&99, || true, idle, droppable);
        assert_eq!(table.len(), 5);
        for key in [0, 1, 2, 3, 99] {
            assert!(table.get_mut(&key).is_some(), "{key}");
        }
    }

    #[test]
    fn the_table_memory_stays_bounded_under_a_flood_of_new_keys() {
        let idle = |_: &u8| true;
        let droppable = |_: &u8| true;
        let mut table: Bounded<u64, u8> = Bounded::new(1_000);
        for key in 0..50_000u64 {
            table.get_or_insert_with(&key, || 0, idle, droppable);
            assert!(table.len() <= 1_000);
        }
        assert!(!table.is_empty());
    }
}
