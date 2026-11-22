//! 配对码的生成、规范化、哈希，以及兑换失败的限速器。
//!
//! # 码
//!
//! 12 个 Crockford base32 字符（`0-9A-Z` 去掉 `I L O U`，肉眼不会认错），显示成 `XXXX-XXXX-XXXX`：60 位随机。
//! 随机数来自仓库已有的安全随机源——`Uuid::new_v4()`（平台 RNG；虚拟 key 和 Admin token 也是这么生成的）——
//! 只取 UUID 里**没有固定位**的字节（版本、变体所在的第 6、8 字节不用）。
//!
//! 60 位 + 短有效期（默认 10 分钟，最长 15 分钟）+ 两层限速（见 [`FailureLimiter`]）：限速器**先占名额、再查库**——
//! 一分钟的窗口里，每个来源最多 20 次、所有来源合起来最多 600 次「还没确认是不是猜错」的尝试能走到数据库查询，
//! 并发的猜码也一样（旧版本先检查、查完才记失败，200 个并发的错码全部走到了查询）。
//!
//! **总猜测速率的上界是 `min(来源数 × 20, 600)` 次每分钟，和攻击者有多少个来源无关**：一个码在有效期内（最长 15 分钟）
//! 最多被试 `15 × 600 = 9 000` 次，空间是 2^60（约 1.15 × 10^18），对一个指定的码命中概率不到 10^-14；
//! 同时有一百万个码有效，也不到 10^-8。不可行。（原来是全局 20 次每分钟，一个来源就能把所有租户的配对堵死；
//! 现在单个来源堵住的是它自己的份额，要堵所有人至少需要 30 个不同的来源（IPv6 是 30 个不同的 /64：一个 /56 就有 256 个，
//! 见设计稿 S2-12 的残余风险），#603 N3。）
//! 库里只存 `SHA-256(域前缀 || 规范化后的 12 个字符)`，查表用哈希，查到之后存储层再用常数时间比较一次哈希
//! （`subtle::ConstantTimeEq`）。
//!
//! 输入规范化：大小写不敏感，忽略空格和连字符，`I`/`L` 当作 `1`，`O` 当作 `0`（Crockford 的约定）；
//! 长度和字符集不对的输入当作「无效」，和「查无此码」同一个答案、同样计入失败限速。

use std::collections::VecDeque;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;

use axum::extract::FromRequestParts;
use axum::extract::connect_info::ConnectInfo;
use axum::http::request::Parts;
use ring::digest;
use uuid::Uuid;

use crate::cloud::ingress::abuse::source_key;
use crate::cloud::ingress::net::ClientAddr;
use crate::connection::ConnectionInfo;

const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// 码的字符数（不含连字符）。
pub(super) const CODE_CHARS: usize = 12;
/// 哈希的域前缀：这个哈希只用于配对码，不会和别的哈希（虚拟 key 的 SHA-256）混淆。
const HASH_DOMAIN: &[u8] = b"idoris-pairing-code-v1\0";

/// 一个新生成的码。`display` 只在创建的响应里出现一次，库里只存 `hash`。
pub(super) struct NewCode {
    pub pairing_id: String,
    pub display: String,
    pub hash: [u8; 32],
}

/// 从 `Uuid::new_v4()` 里取 `N` 个没有固定位的随机字节。
fn random_bytes<const N: usize>() -> [u8; N] {
    let mut out = [0_u8; N];
    let mut filled = 0;
    while filled < N {
        let uuid = Uuid::new_v4();
        for (index, byte) in uuid.as_bytes().iter().enumerate() {
            // 第 6 字节高 4 位是版本、第 8 字节高 2 位是变体：不是随机的，整字节不用。
            if index == 6 || index == 8 {
                continue;
            }
            if filled < N {
                out[filled] = *byte;
                filled += 1;
            }
        }
    }
    out
}

pub(super) fn mint() -> NewCode {
    let bytes = random_bytes::<9>(); // 72 位，取前 60 位
    let mut bits: u128 = 0;
    for byte in bytes {
        bits = (bits << 8) | u128::from(byte);
    }
    let mut normalized = String::with_capacity(CODE_CHARS);
    for index in 0..CODE_CHARS {
        let shift = 72 - 5 * (index + 1);
        let symbol = usize::try_from((bits >> shift) & 0x1f).unwrap_or(0);
        normalized.push(char::from(ALPHABET[symbol]));
    }
    let display = format!(
        "{}-{}-{}",
        &normalized[0..4],
        &normalized[4..8],
        &normalized[8..12]
    );
    let suffix = random_bytes::<8>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    NewCode {
        pairing_id: format!("pc_{suffix}"),
        hash: hash_normalized(&normalized),
        display,
    }
}

/// 把用户输入规范化成 12 个字符；不合法返回 `None`。
pub(super) fn normalize(input: &str) -> Option<String> {
    if input.len() > 64 {
        return None;
    }
    let mut out = String::with_capacity(CODE_CHARS);
    for ch in input.chars() {
        let ch = ch.to_ascii_uppercase();
        match ch {
            ' ' | '-' => continue,
            'I' | 'L' => out.push('1'),
            'O' => out.push('0'),
            c if c.is_ascii() && ALPHABET.contains(&(c as u8)) => out.push(c),
            _ => return None,
        }
    }
    (out.len() == CODE_CHARS).then_some(out)
}

fn hash_normalized(normalized: &str) -> [u8; 32] {
    let mut context = digest::Context::new(&digest::SHA256);
    context.update(HASH_DOMAIN);
    context.update(normalized.as_bytes());
    let digest = context.finish();
    let mut out = [0_u8; 32];
    out.copy_from_slice(digest.as_ref());
    out
}

/// 用户输入的码 -> 库里存的哈希。输入不合法返回 `None`。
pub(super) fn hash_input(input: &str) -> Option<[u8; 32]> {
    normalize(input).map(|normalized| hash_normalized(&normalized))
}

/// 限速窗口：一分钟。
const WINDOW_MS: i64 = 60_000;
/// 单个来源在窗口里最多占着几个「猜错」的名额。owner 2026-11-22 已拍板保持这个数值（设计稿 S2-12）。
const PER_SOURCE_MAX: usize = 20;
/// 所有来源合起来在窗口里最多占着几个名额（总闸）。owner 2026-11-22 已拍板保持这个数值（设计稿 S2-12）。
///
/// 取单来源上限的 30 倍：一个来源打不满它，要堵住所有人至少需要 30 个不同的来源（IPv6 是 30 个不同的 /64）；同时它仍然是暴力破解的总速率上界
/// （见模块文档：一个码在有效期内最多被试 9 000 次，空间 2^60）。
const GLOBAL_MAX: usize = 600;

/// 兑换的滑动窗口限速器：**先占名额，再查库**，名额按来源分桶，另有一个全局总闸。
///
/// 每次兑换在碰数据库之前先 [`acquire`](Self::acquire) 一个名额：这个来源在窗口里占着的名额已经满了，或者全部来源合起来
/// 占着的名额已经满了，就拒绝，不查库。查完以后，只有「猜错」（查无此码、格式不对）的尝试一直占着名额直到窗口滑过去，
/// 其余的（兑换成功，或者码是认得出的——已用、已过期、已撤销、签发者失去资格……不是在猜）用
/// [`release`](Self::release) 把名额还回去。
///
/// 为什么不是「先检查、查完记失败」：失败要等查询返回才记，并发的猜码在记上之前全都过了检查，限速对并发的攻击者不起作用。
///
/// # 为什么分来源，又为什么还要总闸
///
/// 原来只有一个全局的桶（一分钟 20 次）：一个来源每分钟发约 20 个垃圾码，所有租户的合法配对就都拿 429（#603 N3）。
/// 现在每个来源（[`RedeemSource`]；IPv6 取 /64）有自己的上限，一个来源的失败只堵它自己，别的来源的合法码照常兑换。
/// **总闸保留**，因为码的空间只有 60 位，总闸才是暴力破解的总速率上界：来源是攻击者可以变的东西（有多少个地址就有多少个来源），
/// 没有总闸，猜测速率就是「来源数 × 20」，没有上界。总闸取单来源上限的 30 倍，所以一个来源打不满它；
/// 要靠失败把所有人堵住，至少需要 30 个不同的来源（IPv6 是 30 个不同的 /64）同时在猜——那时仍然只是拒绝服务（429），不是越权，
/// 窗口滑过去自动恢复。数值见 [`PER_SOURCE_MAX`]、[`GLOBAL_MAX`]。
///
/// 内存有上界：占着的名额最多 [`GLOBAL_MAX`] 个（总闸拒绝时不再记新的），来源只存在于名额里，没有单独的来源表。
pub(super) struct FailureLimiter {
    window_ms: i64,
    per_source_max: usize,
    global_max: usize,
    state: Mutex<LimiterState>,
}

#[derive(Default)]
struct LimiterState {
    next_ticket: u64,
    /// 占着的名额，按占用的时刻排序（最老的在前）。长度永远不超过总闸。
    held: VecDeque<Held>,
}

struct Held {
    ticket: u64,
    at: i64,
    /// 规范化之后的来源（[`source_key`]）。
    source: IpAddr,
}

/// 占到的一个名额。要么 [`release`](FailureLimiter::release) 还回去，要么留着（记为一次失败，窗口滑过去自动失效）。
#[derive(Debug)]
pub(super) struct Ticket(u64);

impl FailureLimiter {
    pub(super) fn new(window_ms: i64, per_source_max: usize, global_max: usize) -> Self {
        Self {
            window_ms,
            per_source_max,
            global_max,
            state: Mutex::new(LimiterState::default()),
        }
    }

    /// 一分钟内：每个来源最多 [`PER_SOURCE_MAX`] 个名额，所有来源合起来最多 [`GLOBAL_MAX`] 个。
    pub(super) fn standard() -> Self {
        Self::new(WINDOW_MS, PER_SOURCE_MAX, GLOBAL_MAX)
    }

    /// 为 `source` 占一个名额；这个来源在窗口里的名额已满、或者总闸已满，返回 `None`（调用方必须在查库之前就拒绝）。
    /// `source` 在这里规范化（[`source_key`]：IPv6 取 /64，IPv4 映射地址还原成 IPv4），调用方传原始地址即可。
    /// 拿不到锁（中毒）一律不允许：失败即关闭。
    pub(super) fn acquire(&self, source: IpAddr, now_ms: i64) -> Option<Ticket> {
        let source = source_key(source);
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        Self::prune(&mut state.held, now_ms, self.window_ms);
        if state.held.len() >= self.global_max {
            return None;
        }
        // 名额最多总闸那么多个（几百个），数一遍比另外维护一张按来源的表简单，也不会和 `held` 对不上
        let mine = state
            .held
            .iter()
            .filter(|held| held.source == source)
            .count();
        if mine >= self.per_source_max {
            return None;
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.held.push_back(Held {
            ticket,
            at: now_ms,
            source,
        });
        Some(Ticket(ticket))
    }

    /// 把名额还回去（这次尝试不是在猜）。已经因为窗口滑过去而失效的名额什么都不做。
    pub(super) fn release(&self, ticket: Ticket) {
        if let Ok(mut state) = self.state.lock() {
            state.held.retain(|held| held.ticket != ticket.0);
        }
    }

    fn prune(held: &mut VecDeque<Held>, now_ms: i64, window_ms: i64) {
        while held
            .front()
            .is_some_and(|front| now_ms.saturating_sub(front.at) >= window_ms)
        {
            held.pop_front();
        }
    }
}

/// 一个兑换请求的来源：只用来给 [`FailureLimiter`] 分桶，**不参与任何授权**（兑换本来就没有凭证，码本身是凭据）。
///
/// 取法和云入口的来源软配额是同一套（`cloud::ingress::net::resolve_client`，受信代理的规则只有一份）：
///
/// 1. 云形态里，控制面的边缘（`control_edge`）已经算出了客户端来源（`ClientAddr`）：TCP 对端不在 `trusted_proxies` 里，
///    就是对端地址本身，它自己写的 `X-Forwarded-For` 一律不信；对端是受信代理时，才取代理写下的真实客户端地址。
/// 2. 没有这一层（组织形态：没有受信代理的配置，也没有云入口）：只用 TCP 对端地址。头里写什么都不看；
///    控制面在没有声明代理的反向代理后面时，所有请求的对端都是代理，退化成和原来一样的单个桶，不会更糟。
/// 3. 连对端都没有（测试里直接驱动路由）：`0.0.0.0`，同一个「来源未知」的桶。
///
/// 这段读来源的代码放在这里，而不是处理函数里：`handlers.rs` / `mod.rs` / `wire.rs` / `bootstrap.rs` 有一条测试
/// （`the_control_plane_sources_never_look_at_where_a_request_came_from`）钉着「控制面的授权路径不看请求从哪来」。
pub(super) struct RedeemSource(pub IpAddr);

impl<S: Send + Sync> FromRequestParts<S> for RedeemSource {
    type Rejection = Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let resolved = parts.extensions.get::<ClientAddr>().map(|client| client.ip);
        let peer = || {
            parts
                .extensions
                .get::<ConnectInfo<ConnectionInfo>>()
                .map(|ConnectInfo(connection)| connection.peer().ip())
        };
        Ok(Self(
            resolved
                .or_else(peer)
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        ))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn minted_codes_have_the_documented_shape_and_hash_back_from_their_display_form() {
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            let code = mint();
            assert_eq!(code.display.len(), 14, "{}", code.display);
            assert_eq!(code.display.as_bytes()[4], b'-');
            assert_eq!(code.display.as_bytes()[9], b'-');
            assert!(code.pairing_id.starts_with("pc_") && code.pairing_id.len() == 19);
            assert!(
                code.display
                    .bytes()
                    .all(|b| b == b'-' || ALPHABET.contains(&b)),
                "{}",
                code.display
            );
            // 显示形式规范化再哈希，得到存进库里的那个哈希
            assert_eq!(hash_input(&code.display), Some(code.hash));
            assert!(
                seen.insert(code.display),
                "60 random bits do not repeat in 200 draws"
            );
        }
    }

    #[test]
    fn input_normalization_follows_the_crockford_conventions_and_rejects_everything_else() {
        let code = mint();
        let plain = code.display.replace('-', "");
        for variant in [
            code.display.clone(),
            plain.clone(),
            plain.to_ascii_lowercase(),
            format!("  {}  ", code.display),
            plain
                .chars()
                .collect::<Vec<_>>()
                .chunks(3)
                .map(|c| c.iter().collect::<String>())
                .collect::<Vec<_>>()
                .join(" "),
        ] {
            assert_eq!(hash_input(&variant), Some(code.hash), "{variant:?}");
        }
        // I/L -> 1, O -> 0
        assert_eq!(normalize("OOOO-IIII-LLLL").unwrap(), "000011111111");
        for bad in [
            "",
            "ABCD-EFGH-JKM",    // 11 个字符
            "ABCD-EFGH-JKMN-P", // 13 个字符
            "ABCD-EFGH-JKMU",   // U 不在字母表里
            "ABCD-EFGH-JKM!",
            "ABCD-EFGH-JKM\u{e9}", // 非 ASCII
            &"A".repeat(100),
        ] {
            assert_eq!(normalize(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn the_hash_is_domain_separated_from_a_plain_sha256() {
        let normalized = "ABCDEFGHJKMN";
        let plain = digest::digest(&digest::SHA256, normalized.as_bytes());
        assert_ne!(hash_normalized(normalized).as_slice(), plain.as_ref());
    }

    #[test]
    fn the_random_bytes_skip_the_fixed_uuid_positions() {
        // 一次取 30 个字节要用掉三个 UUID（每个 14 个可用字节），不会 panic、不会取到固定位。
        let bytes = random_bytes::<30>();
        assert_eq!(bytes.len(), 30);
        assert_ne!(random_bytes::<16>(), random_bytes::<16>());
    }

    fn ip(text: &str) -> IpAddr {
        text.parse().unwrap()
    }

    /// 单一来源的测试请求。
    fn src() -> IpAddr {
        ip("198.51.100.1")
    }

    // 下面三条是原来全局限速器的测试：单来源上限 = 总闸，用同一个来源，行为和原来的全局限速器一模一样（断言没有变，
    // 只是 `acquire` 多了来源参数、`new` 多了一个上限参数）。

    #[test]
    fn the_limiter_closes_after_the_limit_and_reopens_when_the_window_slides() {
        let limiter = FailureLimiter::new(1_000, 3, 3);
        // 占满：三个名额，不管有没有人「失败」，第四个就是拒绝（先占名额，再查库）
        let tickets: Vec<_> = [0, 100, 200]
            .into_iter()
            .map(|at| limiter.acquire(src(), at).expect("a free slot"))
            .collect();
        assert_eq!(tickets.len(), 3);
        assert!(
            limiter.acquire(src(), 300).is_none(),
            "three slots held inside the window: closed"
        );
        assert!(limiter.acquire(src(), 999).is_none());
        assert!(
            limiter.acquire(src(), 1_000).is_some(),
            "the oldest slot left the window (half-open)"
        );
        assert!(limiter.acquire(src(), 1_001).is_none());
        // 内存有上界
        for at in 0..1_000 {
            let _ = limiter.acquire(src(), 2_000 + at);
        }
        assert!(limiter.state.lock().unwrap().held.len() <= 3);
    }

    #[test]
    fn a_released_slot_is_free_again_and_releasing_twice_or_late_is_harmless() {
        let limiter = FailureLimiter::new(1_000, 2, 2);
        let first = limiter.acquire(src(), 0).unwrap();
        let second = limiter.acquire(src(), 10).unwrap();
        assert!(limiter.acquire(src(), 20).is_none());
        // 还回去的名额马上能再占；只还一次那个编号的名额
        limiter.release(first);
        let third = limiter.acquire(src(), 30).unwrap();
        assert!(limiter.acquire(src(), 40).is_none());
        // 窗口滑过去之后再还：名额早已失效，不会误删别人新占的
        let fresh = limiter.acquire(src(), 5_000).unwrap();
        limiter.release(second);
        limiter.release(third);
        assert_eq!(
            limiter.state.lock().unwrap().held.len(),
            1,
            "only the fresh slot is held"
        );
        limiter.release(fresh);
        assert!(limiter.state.lock().unwrap().held.is_empty());
    }

    #[test]
    fn parallel_acquires_never_hand_out_more_than_the_limit() {
        use std::sync::Arc;
        let limiter = Arc::new(FailureLimiter::new(60_000, 20, 20));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let limiter = limiter.clone();
                std::thread::spawn(move || {
                    (0..50)
                        .filter(|_| limiter.acquire(src(), 5).is_some())
                        .count()
                })
            })
            .collect();
        let granted: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(granted, 20, "800 racing attempts, 20 slots");
    }

    // ---------- #603 N3：按来源分桶 + 全局总闸 ----------

    #[test]
    fn a_source_is_held_to_its_own_share_and_the_global_gate_caps_all_sources_together() {
        // 每个来源 2 个，总闸 5 个
        let limiter = FailureLimiter::new(1_000, 2, 5);
        let (a, b, c, d) = (
            ip("198.51.100.1"),
            ip("198.51.100.2"),
            ip("198.51.100.3"),
            ip("198.51.100.4"),
        );
        assert!(limiter.acquire(a, 0).is_some());
        assert!(limiter.acquire(a, 1).is_some());
        assert!(
            limiter.acquire(a, 2).is_none(),
            "a is at its own limit, the global gate is nowhere near full"
        );
        // 别的来源不受 a 的影响
        assert!(limiter.acquire(b, 3).is_some());
        assert!(limiter.acquire(b, 4).is_some());
        assert!(limiter.acquire(b, 5).is_none());
        // 总闸：5 个名额占满（a 2 + b 2 + c 1），c 自己的份额还有，但总闸已满；没出现过的来源 d 也进不来
        assert!(limiter.acquire(c, 6).is_some());
        assert!(
            limiter.acquire(c, 7).is_none(),
            "five slots held in all: the global gate is closed even for a source that is inside its own share"
        );
        assert!(limiter.acquire(d, 8).is_none());
        // 窗口滑过去：a 最老的两个（0、1）先失效，a 重新有自己的份额，总闸也腾出了位置
        assert!(limiter.acquire(a, 1_001).is_some());
        assert!(limiter.acquire(a, 1_002).is_some());
        assert!(
            limiter.acquire(a, 1_003).is_none(),
            "a is back at its own share"
        );
        assert!(
            limiter.acquire(d, 1_004).is_some(),
            "and the global gate has room again"
        );
    }

    #[test]
    fn a_release_frees_only_that_sources_slot_and_the_global_one() {
        let limiter = FailureLimiter::new(60_000, 2, 3);
        let (a, b) = (ip("198.51.100.1"), ip("198.51.100.2"));
        let a1 = limiter.acquire(a, 0).unwrap();
        let _a2 = limiter.acquire(a, 1).unwrap();
        let _b1 = limiter.acquire(b, 2).unwrap();
        assert!(limiter.acquire(a, 3).is_none(), "a is at its share");
        assert!(limiter.acquire(b, 3).is_none(), "the global gate is full");
        limiter.release(a1);
        // a 自己的份额又有了一个位置（总闸也腾出一个，a 拿走了它）；b 占着的还是 1 个，没有被动过
        assert!(
            limiter.acquire(a, 4).is_some(),
            "a's own share has room again"
        );
        assert!(
            limiter.acquire(b, 5).is_none(),
            "the global slot that the release freed is taken"
        );
        let state = limiter.state.lock().unwrap();
        assert_eq!(state.held.iter().filter(|held| held.source == b).count(), 1);
    }

    #[test]
    fn the_held_slots_are_bounded_by_the_global_gate_whatever_the_sources() {
        // 攻击者换很多来源、每个来源猜很多次：占着的名额永远不超过总闸，每个来源永远不超过自己的上限
        let limiter = FailureLimiter::new(60_000, 20, 600);
        for n in 0..5_000u32 {
            let source = IpAddr::from(Ipv4Addr::from(0xC633_6400 + n % 200));
            let _ = limiter.acquire(source, i64::from(n));
        }
        let state = limiter.state.lock().unwrap();
        assert_eq!(state.held.len(), 600);
        for n in 0..200u32 {
            let source = IpAddr::from(Ipv4Addr::from(0xC633_6400 + n));
            let mine = state
                .held
                .iter()
                .filter(|held| held.source == source)
                .count();
            assert!(mine <= 20, "{source}: {mine}");
        }
    }

    #[test]
    fn ipv6_sources_collapse_to_their_64_bit_prefix_and_mapped_addresses_to_ipv4() {
        let limiter = FailureLimiter::new(60_000, 2, 100);
        // 同一个 /64 的三个地址：共用 2 个名额
        assert!(limiter.acquire(ip("2001:db8:1:2:aaaa::1"), 0).is_some());
        assert!(limiter.acquire(ip("2001:db8:1:2:bbbb::7"), 1).is_some());
        assert!(limiter.acquire(ip("2001:db8:1:2:cccc::3"), 2).is_none());
        assert!(
            limiter.acquire(ip("2001:db8:1:3::1"), 3).is_some(),
            "another /64"
        );
        // IPv4 映射地址就是它的 IPv4 地址
        assert!(limiter.acquire(ip("203.0.113.7"), 4).is_some());
        assert!(limiter.acquire(ip("::ffff:203.0.113.7"), 5).is_some());
        assert!(limiter.acquire(ip("203.0.113.7"), 6).is_none());
    }

    #[test]
    fn parallel_acquires_across_many_sources_never_exceed_either_limit() {
        use std::sync::Arc;
        // 8 个来源各自 100 次尝试（共 800 次）并发地抢：每个来源最多 20 个，合起来最多 100 个（总闸），而且恰好占满总闸
        let limiter = Arc::new(FailureLimiter::new(60_000, 20, 100));
        let handles: Vec<_> = (0..8_u8)
            .map(|index| {
                let limiter = limiter.clone();
                std::thread::spawn(move || {
                    let source = IpAddr::from(Ipv4Addr::new(198, 51, 100, index + 1));
                    (0..100)
                        .filter(|_| limiter.acquire(source, 5).is_some())
                        .count()
                })
            })
            .collect();
        let granted: Vec<usize> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert!(granted.iter().all(|count| *count <= 20), "{granted:?}");
        assert_eq!(granted.iter().sum::<usize>(), 100, "{granted:?}");
    }

    // ---------- 来源怎么取 ----------

    async fn source_of(extend: impl FnOnce(&mut Parts)) -> IpAddr {
        let (mut parts, ()) = axum::http::Request::builder()
            .header("x-forwarded-for", "198.51.100.200")
            .header("x-real-ip", "198.51.100.201")
            .body(())
            .unwrap()
            .into_parts();
        extend(&mut parts);
        let RedeemSource(source) = RedeemSource::from_request_parts(&mut parts, &())
            .await
            .unwrap();
        source
    }

    #[tokio::test]
    async fn the_redeem_source_is_the_resolved_client_then_the_tcp_peer_then_unknown_and_never_a_header()
     {
        // 没有任何来源信息：同一个「来源未知」的桶；请求头里写什么都不看
        assert_eq!(source_of(|_| {}).await, ip("0.0.0.0"));
        // 只有 TCP 对端（组织形态）：就是对端地址，头里的 `X-Forwarded-For` 一律不信
        let peer = ip("203.0.113.9");
        let with_peer = |parts: &mut Parts| {
            parts
                .extensions
                .insert(ConnectInfo(ConnectionInfo::for_peer(
                    std::net::SocketAddr::new(peer, 50_000),
                )));
        };
        assert_eq!(source_of(with_peer).await, peer);
        // 云形态：边缘已经按受信代理的规则算出了客户端来源，它优先于对端地址（对端是代理）
        let client = ip("198.51.100.7");
        let resolved = source_of(|parts| {
            with_peer(parts);
            parts.extensions.insert(ClientAddr {
                ip: client,
                peer: Some(ip("10.0.0.2")),
                forwarded: true,
            });
        })
        .await;
        assert_eq!(resolved, client);
    }
}
