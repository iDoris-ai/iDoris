//! 配对码的生成、规范化、哈希，以及兑换失败的限速器。
//!
//! # 码
//!
//! 12 个 Crockford base32 字符（`0-9A-Z` 去掉 `I L O U`，肉眼不会认错），显示成 `XXXX-XXXX-XXXX`：60 位随机。
//! 随机数来自仓库已有的安全随机源——`Uuid::new_v4()`（平台 RNG；虚拟 key 和 Admin token 也是这么生成的）——
//! 只取 UUID 里**没有固定位**的字节（版本、变体所在的第 6、8 字节不用）。
//!
//! 60 位 + 短有效期（默认 10 分钟）+ 全局失败限速（见 [`FailureLimiter`]）：在一个码的有效期内，
//! 攻击者最多能试几百次，空间是 2^60，不可行。库里只存 `SHA-256(域前缀 || 规范化后的 12 个字符)`，
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

/// 兑换失败的滑动窗口限速器：窗口内失败次数达到上限，就拒绝所有兑换，直到窗口滑过去。
///
/// 这是**全局**的，不按来源地址分：控制面在反向代理后面时，来源地址全是代理的，按地址限速没有意义，
/// 而地址头不可信。代价是有人能用垃圾码把配对暂时堵住（拒绝服务，不是越权）；换来的是码的暴力破解
/// 在有效期内不可行。上限和窗口见 [`FailureLimiter::standard`]。
pub(super) struct FailureLimiter {
    window_ms: i64,
    max_failures: usize,
    failures: Mutex<VecDeque<i64>>,
}

impl FailureLimiter {
    pub(super) fn new(window_ms: i64, max_failures: usize) -> Self {
        Self {
            window_ms,
            max_failures,
            failures: Mutex::new(VecDeque::new()),
        }
    }

    /// 一分钟内最多 20 次失败。
    pub(super) fn standard() -> Self {
        Self::new(60_000, 20)
    }

    /// 现在还允许兑换吗。拿不到锁（中毒）一律不允许：失败即关闭。
    pub(super) fn allow(&self, now_ms: i64) -> bool {
        let Ok(mut failures) = self.failures.lock() else {
            return false;
        };
        Self::prune(&mut failures, now_ms, self.window_ms);
        failures.len() < self.max_failures
    }

    pub(super) fn record_failure(&self, now_ms: i64) {
        if let Ok(mut failures) = self.failures.lock() {
            Self::prune(&mut failures, now_ms, self.window_ms);
            failures.push_back(now_ms);
            // 内存上界：只留最近 max 次（再多的失败对判定没有区别）。
            while failures.len() > self.max_failures {
                failures.pop_front();
            }
        }
    }

    fn prune(failures: &mut VecDeque<i64>, now_ms: i64, window_ms: i64) {
        while failures
            .front()
            .is_some_and(|at| now_ms.saturating_sub(*at) >= window_ms)
        {
            failures.pop_front();
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
        assert!(limiter.allow(0));
        for at in [0, 100, 200] {
            limiter.record_failure(at);
        }
        assert!(
            !limiter.allow(300),
            "three failures inside the window: closed"
        );
        assert!(!limiter.allow(999));
        assert!(
            limiter.allow(1_000),
            "the oldest failure left the window (half-open)"
        );
        limiter.record_failure(1_000);
        assert!(!limiter.allow(1_001));
        // 内存有上界
        for at in 0..1_000 {
            limiter.record_failure(2_000 + at);
        }
        assert!(limiter.failures.lock().unwrap().len() <= 3);
    }
}
