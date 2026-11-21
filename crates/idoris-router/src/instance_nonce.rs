//! `instance_nonce`（接口规范 §2.1 凭证表第五行，修订 1.6）。
//!
//! 每次进程启动用 CSPRNG 生成 256 位，小写 hex 64 字符。**它是秘密**：只存在于进程内存和 0600 的
//! 发现文件里（`discovery`），不进 `/health`、Admin、日志、argv、环境变量、错误信息，也不在 UDS 上明文传输
//! （登记协议 `register` 只把它当 HMAC 密钥，见 [`InstanceNonce::hmac_key`]）。所以这个类型故意：
//!
//! - 没有 `Serialize`：不可能被不小心塞进某个 JSON 响应；
//! - `Debug`、`Display` 一律输出 `[REDACTED]`：不可能被 `{:?}` / `{}` 写进日志或错误信息；
//! - 取原文只有两个显式入口：[`InstanceNonce::expose_hex`]（写发现文件）与 [`InstanceNonce::hmac_key`]
//!   （登记的 HMAC 证明，只吐出 `ring` 的不透明密钥），调用点一眼可查。
//!
//! 它挡的是连错实例、读不到该目录的沙箱进程冒充，不是对抗同 UID 恶意进程的屏障（规范 §2.1）。

use ring::hmac;
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
pub struct InstanceNonce {
    hex: String,
    bytes: [u8; NONCE_BYTES],
}

impl InstanceNonce {
    /// 进程启动时调用一次：`ring::rand::SystemRandom` 取 32 字节，编码成小写 hex。
    pub fn generate() -> Result<Self, NonceError> {
        let mut bytes = [0_u8; NONCE_BYTES];
        SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| NonceError)?;
        Ok(Self {
            hex: hex_lower(&bytes),
            bytes,
        })
    }

    /// 只接受 64 位小写 hex（launcher 帧的 `instance_nonce` 字段）。规范约定生产不给这个字段、只给确定性测试用；
    /// 代码不拒绝它。
    pub fn from_hex(value: &str) -> Option<Self> {
        let bytes = decode_lower_hex::<NONCE_BYTES>(value)?;
        Some(Self {
            hex: value.to_string(),
            bytes,
        })
    }

    /// 显式取出原文，只有发现文件的序列化需要它。
    pub fn expose_hex(&self) -> &str {
        &self.hex
    }

    /// 登记协议（接口规范 §2.10.5）的 HMAC-SHA256 密钥：`instance_nonce` 解码后的 **32 个原始字节**，
    /// 不是 64 个 hex 字符的文本。返回的是 `ring` 的不透明密钥，调用方拿不到原文。
    pub fn hmac_key(&self) -> hmac::Key {
        hmac::Key::new(hmac::HMAC_SHA256, &self.bytes)
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

/// 恰好 `2 * N` 个小写 hex 字符解码成 `N` 个字节；其他（长度不对、大写、非 hex 字符）一律 `None`。
pub fn decode_lower_hex<const N: usize>(value: &str) -> Option<[u8; N]> {
    if !is_lower_hex(value, N * 2) {
        return None;
    }
    let nibble = |byte: u8| match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    };
    let mut out = [0_u8; N];
    for (slot, pair) in out.iter_mut().zip(value.as_bytes().chunks_exact(2)) {
        *slot = (nibble(*pair.first()?)? << 4) | nibble(*pair.get(1)?)?;
    }
    Some(out)
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

    #[test]
    fn decode_lower_hex_round_trips_and_rejects_everything_else() {
        assert_eq!(decode_lower_hex::<0>(""), Some([]));
        assert_eq!(
            decode_lower_hex::<5>("000ff0ffab"),
            Some([0x00, 0x0f, 0xf0, 0xff, 0xab])
        );
        let every_byte = (0..=255_u8).collect::<Vec<_>>();
        let encoded = hex_lower(&every_byte);
        assert_eq!(
            decode_lower_hex::<256>(&encoded).unwrap().to_vec(),
            every_byte
        );
        for invalid in [
            "000ff0ffa",
            "000ff0ffabc",
            "000FF0FFAB",
            "000ff0ffag",
            " 00ff0ffab",
            "",
        ] {
            assert!(decode_lower_hex::<5>(invalid).is_none(), "{invalid:?}");
        }
    }

    /// 密钥是 hex 解码后的 32 个原始字节，不是 64 个 hex 字符的文本：规范 §2.10.5 的测试向量（`ack` 的 `proof`
    /// `099e173f…`）只有按原始字节做密钥才得到；把文本当密钥会得到完全不同的值。
    #[test]
    fn hmac_key_is_the_decoded_raw_bytes_not_the_hex_text() {
        let hex = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
        let nonce = InstanceNonce::from_hex(hex).unwrap();
        let transcript = b"idoris-ack/v1\n0123456789abcdef\n00112233445566778899aabbccddeeff\na1b2c3d4e5f60718293a4b5c6d7e8f90";
        let by_raw_bytes = hmac::sign(&nonce.hmac_key(), transcript);
        assert_eq!(
            hex_lower(by_raw_bytes.as_ref()),
            "099e173f6990315a62de5c9426ecbf0ed395a58e47728c5453e3e5a483cc0967"
        );
        let by_text = hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, hex.as_bytes()),
            transcript,
        );
        assert_ne!(by_raw_bytes.as_ref(), by_text.as_ref());
    }
}
