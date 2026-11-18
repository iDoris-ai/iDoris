//! oMLX 推理密钥的唯一解析入口（接口规范 §2.9「oMLX 密钥」）。
//!
//! 优先级：环境变量 `IDORIS_OMLX_API_KEY`（§2.7，保持不变）→ `~/.omlx/settings.json` 的
//! `auth.api_key` → 无鉴权连接。iDoris 连接 oMLX 的所有路径（生命周期适配器、常驻代理、模型列表）
//! 都只调用这里；本模块之外不应再有人直接读 `IDORIS_OMLX_API_KEY`。
//!
//! 约定：
//! - 环境变量为空字符串视为「没设」，继续往下找；非 UTF-8 的环境变量原样保留（[`resolve_omlx_api_key_os`]），
//!   由调用方按原有口径判为无效凭证，而不是悄悄换成文件里的 key。
//! - `settings.json` 只从文件读取，最多读 64 KiB；不是常规文件（FIFO、设备）、读不了、不是 JSON、
//!   没有 `auth.api_key`、值不是非空字符串、含控制字符，一律当作没有，往下走无鉴权，不报错、不记日志。
//! - 不打印、不写日志、不进 argv 或响应体；错误信息里不带文件内容。
//! - 不硬编码任何默认 key。

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::Path;

use super::OMLX_API_KEY_ENV;

/// `settings.json` 的读取上限。
pub const SETTINGS_MAX_BYTES: u64 = 64 * 1024;

/// oMLX 开了鉴权而两处都没有 key（或 key 被拒）时给人看的说明；不属于契约，不含任何 key。
pub const OMLX_KEY_REMEDIATION: &str = "oMLX 需要 API key：请设置环境变量 IDORIS_OMLX_API_KEY，\
或在 ~/.omlx/settings.json 的 auth.api_key 中配置；iDoris 不会重试";

/// 当前进程环境下的 key（UTF-8）。
pub fn resolve_omlx_api_key() -> Option<String> {
    resolve_omlx_api_key_os()?.into_string().ok()
}

/// 同上，但保留非 UTF-8 的环境变量，让调用方按「无效凭证」处理。
pub fn resolve_omlx_api_key_os() -> Option<OsString> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    resolve_from(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
    )
}

/// 纯解析：`env` 是环境变量值，`home` 是家目录。测试用它注入假 HOME。
pub fn resolve_from(env: Option<OsString>, home: Option<&Path>) -> Option<OsString> {
    if let Some(value) = env.filter(|value| !value.is_empty()) {
        return Some(value);
    }
    settings_api_key(home?).map(OsString::from)
}

/// `<home>/.omlx/settings.json` 的 `auth.api_key`；任何问题都是 `None`。
pub fn settings_api_key(home: &Path) -> Option<String> {
    let path = home.join(".omlx").join("settings.json");
    // 先看类型再打开：FIFO / 设备文件一打开就可能阻塞。
    if !fs::metadata(&path).ok()?.is_file() {
        return None;
    }
    let mut raw = Vec::new();
    fs::File::open(&path)
        .ok()?
        .take(SETTINGS_MAX_BYTES + 1)
        .read_to_end(&mut raw)
        .ok()?;
    if raw.len() as u64 > SETTINGS_MAX_BYTES {
        return None;
    }
    let value: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    let key = value.get("auth")?.get("api_key")?.as_str()?;
    (!key.trim().is_empty() && !key.chars().any(char::is_control)).then(|| key.to_owned())
}
