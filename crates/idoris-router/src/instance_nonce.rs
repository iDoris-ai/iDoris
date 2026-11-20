//! `instance_nonce`（接口规范 §2.1 凭证表第五行，修订 1.6）。
//!
//! 每次进程启动用 CSPRNG 生成 256 位，小写 hex 64 字符。**它是秘密**：只存在于进程内存和 0600 的
//! 发现文件里（`discovery`），不进 `/health`、Admin、日志、argv、环境变量、错误信息，也不在 UDS 上明文传输
//! （S2 只把它当 HMAC 密钥）。所以这个类型故意：
//!
//! - 没有 `Serialize`：不可能被不小心塞进某个 JSON 响应；
//! - `Debug`、`Display` 一律输出 `[REDACTED]`：不可能被 `{:?}` / `{}` 写进日志或错误信息；
//! - 取原文只有一个显式入口 [`InstanceNonce::expose_hex`]，调用点一眼可查。
//!
//! 它挡的是连错实例、读不到该目录的沙箱进程冒充，不是对抗同 UID 恶意进程的屏障（规范 §2.1）。

use ring::rand::{SecureRandom, SystemRandom};

/// 原始字节数（256 位）。
pub const NONCE_BYTES: usize = 32;
/// 小写 hex 的字符数。
pub const NONCE_HEX_LEN: usize = NONCE_BYTES * 2;

/// 系统随机数源不可用。不带任何细节：不会有秘密可泄露，也没有可以给调用方的有用信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NonceError;

impl std::fmt::Display for NonceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("无法从系统随机数源生成实例随机数")
    }
}

impl std::error::Error for NonceError {}

#[derive(Clone)]
pub struct InstanceNonce(String);

impl InstanceNonce {
    /// 进程启动时调用一次：`ring::rand::SystemRandom` 取 32 字节，编码成小写 hex。
    pub fn generate() -> Result<Self, NonceError> {
        let mut bytes = [0_u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| NonceError)?;
        Ok(Self(hex_lower(&bytes)))
    }

    /// 只接受 64 位小写 hex（launcher 帧的 `instance_nonce` 字段）。规范约定生产不给这个字段、只给确定性测试用；
    /// 代码不拒绝它。
    pub fn from_hex(value: &str) -> Option<Self> {
        is_lower_hex(value, NONCE_HEX_LEN).then(|| Self(value.to_string()))
    }

    /// 显式取出原文，只有发现文件的序列化（以及以后 S2 的 HMAC 密钥）需要它。
    pub fn expose_hex(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for InstanceNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("InstanceNonce([REDACTED])")
    }
}

impl std::fmt::Display for InstanceNonce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[REDACTED]")
    }
}

/// `value` 恰好是 `len` 个小写 hex 字符。
pub fn is_lower_hex(value: &str, len: usize) -> bool {
    value.len() == len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

/// 小写 hex 编码。
pub fn hex_lower(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(DIGITS[usize::from(byte >> 4)]));
        out.push(char::from(DIGITS[usize::from(byte & 0x0f)]));
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn generated_nonce_is_64_lowercase_hex_and_unique_per_call() {
        let first = InstanceNonce::generate().unwrap();
        let second = InstanceNonce::generate().unwrap();
        assert!(is_lower_hex(first.expose_hex(), NONCE_HEX_LEN));
        assert!(is_lower_hex(second.expose_hex(), NONCE_HEX_LEN));
        assert_ne!(first.expose_hex(), second.expose_hex());
    }

    #[test]
    fn from_hex_accepts_only_exactly_64_lowercase_hex_characters() {
        let valid = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        assert_eq!(InstanceNonce::from_hex(valid).unwrap().expose_hex(), valid);
        for invalid in [
            "",
            &valid[..63],
            &format!("{valid}0"),
            &valid.to_uppercase(),
            &format!("{}g", &valid[..63]),
            &format!(" {}", &valid[..63]),
            &format!("{}\n", &valid[..63]),
        ] {
            assert!(InstanceNonce::from_hex(invalid).is_none(), "{invalid:?}");
        }
    }

    #[test]
    fn debug_and_display_never_render_the_secret() {
        let nonce = InstanceNonce::generate().unwrap();
        let secret = nonce.expose_hex().to_string();
        for rendered in [
            format!("{nonce:?}"),
            format!("{nonce}"),
            format!("{:#?}", Some(&nonce)),
        ] {
            assert!(!rendered.contains(&secret), "{rendered}");
            assert!(!rendered.contains(&secret[..16]), "{rendered}");
            assert!(rendered.contains("REDACTED"), "{rendered}");
        }
    }

    #[test]
    fn hex_lower_encodes_every_nibble() {
        assert_eq!(hex_lower(&[]), "");
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xf0, 0xff, 0xab]), "000ff0ffab");
    }
}
