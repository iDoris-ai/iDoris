//! 配对码的生成、规范化、哈希，以及兑换失败的限速器。
//!
//! # 码
//!
//! 12 个 Crockford base32 字符（`0-9A-Z` 去掉 `I L O U`，肉眼不会认错），显示成 `XXXX-XXXX-XXXX`：60 位随机。
//! 随机数来自仓库已有的安全随机源——`Uuid::new_v4()`（平台 RNG；虚拟 key 和 Admin token 也是这么生成的）——
//! 只取 UUID 里**没有固定位**的字节（版本、变体所在的第 6、8 字节不用）。
//!
//! 60 位 + 短有效期（默认 10 分钟）+ 全局限速（见 [`FailureLimiter`]）：限速器**先占名额、再查库**——一分钟的窗口里
//! 最多 20 次「还没确认是不是猜错」的尝试能走到数据库查询，并发的猜码也一样（旧版本先检查、查完才记失败，
//! 200 个并发的错码全部走到了查询）。所以在一个码的有效期内攻击者最多能试 `20 × 有效期的分钟数` 次，默认是 200 次，
//! 空间是 2^60，不可行。库里只存 `SHA-256(域前缀 || 规范化后的 12 个字符)`，
//! 查表用哈希，查到之后存储层再用常数时间比较一次哈希（`subtle::ConstantTimeEq`）。
//!
//! 输入规范化：大小写不敏感，忽略空格和连字符，`I`/`L` 当作 `1`，`O` 当作 `0`（Crockford 的约定）；
//! 长度和字符集不对的输入当作「无效」，和「查无此码」同一个答案、同样计入失败限速。

use std::collections::VecDeque;
use std::sync::Mutex;

use ring::digest;
use uuid::Uuid;

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

/// 兑换的滑动窗口限速器：**先占名额，再查库**。
///
/// 每次兑换在碰数据库之前先 [`acquire`](Self::acquire) 一个名额（窗口内占着的名额已经满了就拒绝，不查库）；
/// 查完以后，只有「猜错」（查无此码、格式不对）的尝试一直占着名额直到窗口滑过去，其余的（兑换成功，或者码是认得出的——
/// 已用、已过期、已撤销、签发者失去资格……不是在猜）用 [`release`](Self::release) 把名额还回去。
///
/// 为什么不是「先检查、查完记失败」：失败要等查询返回才记，并发的猜码在记上之前全都过了检查，限速对并发的攻击者不起作用。
///
/// 这是**全局**的，不按来源地址分：控制面在反向代理后面时，来源地址全是代理的，按地址限速没有意义，
/// 而地址头不可信。代价是有人能用垃圾码把配对暂时堵住（拒绝服务，不是越权）；换来的是码的暴力破解
/// 在有效期内不可行。上限和窗口见 [`FailureLimiter::standard`]。
pub(super) struct FailureLimiter {
    window_ms: i64,
    max_failures: usize,
    state: Mutex<LimiterState>,
}

#[derive(Default)]
struct LimiterState {
    next_ticket: u64,
    /// （名额编号，占用的时刻），按时刻排序。
    held: VecDeque<(u64, i64)>,
}

/// 占到的一个名额。要么 [`release`](FailureLimiter::release) 还回去，要么留着（记为一次失败，窗口滑过去自动失效）。
#[derive(Debug)]
pub(super) struct Ticket(u64);

impl FailureLimiter {
    pub(super) fn new(window_ms: i64, max_failures: usize) -> Self {
        Self {
            window_ms,
            max_failures,
            state: Mutex::new(LimiterState::default()),
        }
    }

    /// 一分钟内最多 20 个名额。
    pub(super) fn standard() -> Self {
        Self::new(60_000, 20)
    }

    /// 占一个名额；窗口里的名额已满返回 `None`（调用方必须在查库之前就拒绝）。拿不到锁（中毒）一律不允许：失败即关闭。
    pub(super) fn acquire(&self, now_ms: i64) -> Option<Ticket> {
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        Self::prune(&mut state.held, now_ms, self.window_ms);
        if state.held.len() >= self.max_failures {
            return None;
        }
        let ticket = state.next_ticket;
        state.next_ticket = state.next_ticket.wrapping_add(1);
        state.held.push_back((ticket, now_ms));
        Some(Ticket(ticket))
    }

    /// 把名额还回去（这次尝试不是在猜）。已经因为窗口滑过去而失效的名额什么都不做。
    pub(super) fn release(&self, ticket: Ticket) {
        if let Ok(mut state) = self.state.lock() {
            state.held.retain(|(held, _)| *held != ticket.0);
        }
    }

    fn prune(held: &mut VecDeque<(u64, i64)>, now_ms: i64, window_ms: i64) {
        while held
            .front()
            .is_some_and(|(_, at)| now_ms.saturating_sub(*at) >= window_ms)
        {
            held.pop_front();
        }
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

    #[test]
    fn the_limiter_closes_after_the_limit_and_reopens_when_the_window_slides() {
        let limiter = FailureLimiter::new(1_000, 3);
        // 占满：三个名额，不管有没有人「失败」，第四个就是拒绝（先占名额，再查库）
        let tickets: Vec<_> = [0, 100, 200]
            .into_iter()
            .map(|at| limiter.acquire(at).expect("a free slot"))
            .collect();
        assert_eq!(tickets.len(), 3);
        assert!(
            limiter.acquire(300).is_none(),
            "three slots held inside the window: closed"
        );
        assert!(limiter.acquire(999).is_none());
        assert!(
            limiter.acquire(1_000).is_some(),
            "the oldest slot left the window (half-open)"
        );
        assert!(limiter.acquire(1_001).is_none());
        // 内存有上界
        for at in 0..1_000 {
            let _ = limiter.acquire(2_000 + at);
        }
        assert!(limiter.state.lock().unwrap().held.len() <= 3);
    }

    #[test]
    fn a_released_slot_is_free_again_and_releasing_twice_or_late_is_harmless() {
        let limiter = FailureLimiter::new(1_000, 2);
        let first = limiter.acquire(0).unwrap();
        let second = limiter.acquire(10).unwrap();
        assert!(limiter.acquire(20).is_none());
        // 还回去的名额马上能再占；只还一次那个编号的名额
        limiter.release(first);
        let third = limiter.acquire(30).unwrap();
        assert!(limiter.acquire(40).is_none());
        // 窗口滑过去之后再还：名额早已失效，不会误删别人新占的
        let fresh = limiter.acquire(5_000).unwrap();
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
        let limiter = Arc::new(FailureLimiter::new(60_000, 20));
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let limiter = limiter.clone();
                std::thread::spawn(move || (0..50).filter(|_| limiter.acquire(5).is_some()).count())
            })
            .collect();
        let granted: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert_eq!(granted, 20, "800 racing attempts, 20 slots");
    }
}
