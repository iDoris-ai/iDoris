//! 引擎私有 API key（设计稿 §2.7.10）。
//!
//! iDoris 自己拉起的引擎，每次拉起生成一个随机 key，经环境变量交给子进程（变量名由各引擎的配方决定，
//! oMLX 是 `OMLX_API_KEY`；通用层不允许它出现在 argv 里）、记进所有权记录（0600）；别的进程不知道它，
//! 打这个引擎得到 401。本模块只负责生成与承载：谁来用这个 key 说话（设计稿里放在 `omlx/key.rs` 的「私有引擎 key 表」）
//! 是 S5 的事。不支持私有 key 的引擎不设它（[`super::LaunchInput::api_key`] 为 `None`）。
//!
//! - 32 字节取自 `/dev/urandom`（macOS 与 Linux 都有；不引入新依赖），小写 hex 编码成 64 字符。
//! - `Debug` 一律打印 `[REDACTED]`；值只能经 [`EngineKey::expose`] 取出，调用点一眼可查。

use std::io::Read;

/// key 的 hex 长度（32 字节）。
pub const ENGINE_KEY_HEX_LEN: usize = 64;

#[derive(Debug, thiserror::Error, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    #[error("failed to read system randomness")]
    Random,
    #[error("engine key must be {ENGINE_KEY_HEX_LEN} lowercase hex characters")]
    Format,
}

/// 一个引擎实例的私有 API key。
#[derive(Clone, PartialEq, Eq)]
pub struct EngineKey(String);

impl EngineKey {
    /// 新生成一个随机 key。
    pub fn generate() -> Result<Self, KeyError> {
        let bytes = random_bytes::<32>().map_err(|_| KeyError::Random)?;
        Ok(Self(hex(&bytes)))
    }

    /// 从 64 位小写 hex 构造（所有权记录里读回来的，或测试里要确定值的）。
    pub fn from_hex(value: &str) -> Result<Self, KeyError> {
        if value.len() == ENGINE_KEY_HEX_LEN
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            Ok(Self(value.to_owned()))
        } else {
            Err(KeyError::Format)
        }
    }

    /// 明文。调用点：交给子进程环境（配方）、写所有权记录、放进 `Authorization` 头（`wait_ready`）、在 `EngineLaunch::new` 里扫描 argv 有没有泄露。
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for EngineKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EngineKey([REDACTED])")
    }
}

/// 从 `/dev/urandom` 读 `N` 字节。
pub(crate) fn random_bytes<const N: usize>() -> std::io::Result<[u8; N]> {
    let mut bytes = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes)
}

/// 小写 hex。
pub(crate) fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn generated_keys_are_64_lowercase_hex_and_differ() {
        let a = EngineKey::generate().unwrap();
        let b = EngineKey::generate().unwrap();
        assert_eq!(a.expose().len(), ENGINE_KEY_HEX_LEN);
        assert!(EngineKey::from_hex(a.expose()).is_ok());
        assert_ne!(a, b, "every launch gets its own key");
    }

    #[test]
    fn from_hex_is_strict() {
        let good = "0123456789abcdef".repeat(4);
        assert!(EngineKey::from_hex(&good).is_ok());
        for bad in [
            "".to_owned(),
            good[..63].to_owned(),
            format!("{good}0"),
            good.to_uppercase(),
            format!("g{}", &good[1..]),
            format!(" {}", &good[1..]),
        ] {
            assert_eq!(EngineKey::from_hex(&bad), Err(KeyError::Format), "{bad:?}");
        }
    }

    #[test]
    fn debug_never_prints_the_key() {
        let key = EngineKey::generate().unwrap();
        let shown = format!("{key:?} {:?}", Some(&key));
        assert!(!shown.contains(key.expose()));
        assert!(shown.contains("REDACTED"));
    }
}
