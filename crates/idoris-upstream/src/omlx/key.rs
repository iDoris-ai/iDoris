//! oMLX 推理密钥的唯一解析入口（接口规范 §2.9「oMLX 密钥」）。
//!
//! 优先级：环境变量 `IDORIS_OMLX_API_KEY`（§2.7，保持不变）→ `~/.omlx/settings.json` 的
//! `auth.api_key` → 无鉴权连接。iDoris 连接 oMLX 的所有路径（生命周期适配器、常驻代理、模型列表）
//! 都只调用这里；本模块之外不应再有人直接读 `IDORIS_OMLX_API_KEY`。
//!
//! 约定：
//! - 环境变量为空字符串视为「没设」，继续往下找；非 UTF-8 的环境变量原样保留（[`resolve_omlx_api_key_os`]），
//!   由调用方按原有口径判为无效凭证，而不是悄悄换成文件里的 key。环境变量的值不做 trim（保持 §2.7 原语义）。
//! - 来自 `settings.json` 的 key 只发给 loopback 的 oMLX：非 loopback 的 base URL 下，文件 key 视为没有
//!   （环境变量的 key 维持原行为，不受此限）。[`KeySource`] 标明 key 来自哪里。
//! - `settings.json` 只从文件读取，最多读 64 KiB；必须是常规文件（符号链接、FIFO、设备一律不要，
//!   用 `symlink_metadata` 判断，不跟随链接）；读不了、不是 JSON、没有 `auth.api_key`、值不是非空字符串
//!   一律当作没有，往下走无鉴权，不报错、不记日志。返回的是 trim 后的值；trim 后含空白或控制字符同样当作没有。
//! - 不打印、不写日志、不进 argv 或响应体；错误信息里不带文件内容。
//! - 不硬编码任何默认 key。
//! - **引擎管理器接管的端点**（IDORIS-CORE S5，规范 §2.7.7）：[`govern_endpoint`] 登记之后，iDoris 自己拉起的引擎只用它的
//!   私有 key（[`EndpointKey::Private`]，优先于上面的顺序）；外部引擎的 `settings.json` key 要等端口属主校验通过
//!   （[`EndpointKey::External`]，#619）。没有登记的端点（非托管的卡）按上面的顺序，与以前完全一样。
//! - **封上的端点**（IDORIS-CORE S6，Codex 预审 3）：托管的端点在引擎管理器启动时就先登记成 [`EndpointKey::Blocked`]，引擎停了、
//!   正在被替换、外部引擎的监听者没了的时候也回到它：**什么凭证都不发**（环境变量、`settings.json`、私有 key 都不发），
//!   上面那条回退链**从不适用于托管的端点**；直到下一个监听者通过校验、被登记成私有或外部的口径。
//! - **代际**：每个端点的登记有一个只增的代际，口径每变一次（换了私有 key、验证状态变了、封上、撤销）加一。按请求取凭证的
//!   调用方把请求准入时的代际带进来（[`resolve_omlx_api_key_checked`]），代际不是当前的就拒绝：属于已经被换掉的引擎的请求，
//!   拿不到新引擎的凭证。

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::{PoisonError, RwLock};

use super::OMLX_API_KEY_ENV;

/// `settings.json` 的读取上限。
pub const SETTINGS_MAX_BYTES: u64 = 64 * 1024;

/// oMLX 开了鉴权而两处都没有 key（或 key 被拒）时给人看的说明；不属于契约，不含任何 key。
pub const OMLX_KEY_REMEDIATION: &str = "oMLX requires an API key: set the IDORIS_OMLX_API_KEY environment \
variable, or configure auth.api_key in ~/.omlx/settings.json (iDoris will not retry) \
/ oMLX 需要 API key：请设置环境变量 IDORIS_OMLX_API_KEY，或在 ~/.omlx/settings.json 的 auth.api_key 中配置；iDoris 不会重试";

/// key 的来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    Env,
    Settings,
}

/// `base_url` 的主机是否是 loopback（localhost / 127.0.0.0/8 / ::1）且协议是 http(s)、不带用户信息。
pub fn is_loopback_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(|host| {
            host.eq_ignore_ascii_case("localhost")
                || host
                    .trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        })
}

/// 当前进程环境下、发往 `base_url` 的 key（UTF-8）。引擎管理器接管的端点按它登记的口径（[`EndpointKey`]）。
pub fn resolve_omlx_api_key_for(base_url: &str) -> Option<String> {
    resolve_omlx_api_key_os_for(base_url)?.into_string().ok()
}

/// 同上，保留非 UTF-8 的环境变量，让调用方按「无效凭证」处理。所有按目标 URL 解析 key 的路径（生命周期适配器、
/// 常驻代理、模型列表、就绪探测）都经这里，所以引擎管理器登记的私有 key 与「外部引擎的 settings.json key 要先过
/// 端口属主校验」对它们同时生效。
pub fn resolve_omlx_api_key_os_for(base_url: &str) -> Option<OsString> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    resolve_for_endpoint(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        is_loopback_base_url(base_url),
        governed_endpoint(base_url).as_ref(),
    )
}

/// 只回答「`base_url` 是不是引擎管理器接管的端点」：是，给出发往它的 key（`None` = 不带 key）；不是，`None` 外面再包一层
/// （调用方沿用自己原来的 key 来源）。
pub fn resolve_governed_os(base_url: &str) -> Option<Option<OsString>> {
    let governed = governed_endpoint(base_url)?;
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    Some(resolve_for_endpoint(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        is_loopback_base_url(base_url),
        Some(&governed),
    ))
}

/// 引擎管理器接管的端点上，key 怎么给（接口规范 §2.7.7）。
#[derive(Clone, PartialEq, Eq)]
pub enum EndpointKey {
    /// iDoris 自己拉起的引擎：只用它的私有 key，不看环境变量与 `settings.json`（私有 key 优先，其余顺序对它不适用）。
    Private(String),
    /// 外部引擎（接管的、不是 iDoris 拉起的）：环境变量 `IDORIS_OMLX_API_KEY` 是调用方显式给的，照旧；`settings.json`
    /// 的 key 是 iDoris 自己读来的，**只在这个端口的监听者通过属主校验之后**才发
    /// （`settings_key_verified`，[#619](https://github.com/iDoris-ai/iDoris/issues/619)）。
    External { settings_key_verified: bool },
    /// **不发任何凭证**：这个端点是托管的，但此刻没有一个我们认得的引擎在它后面（引擎停了、正在被替换、监听者没了、端口上换了
    /// 别的东西）。没有环境变量的 key，没有 `settings.json` 的 key，没有私有 key，也不退回 §2.9 的顺序（那条回退链是给没人管的
    /// 端点用的）。按请求取凭证的适配器遇到它直接拒绝请求，什么都不发（请求体里有用户的提示词）。
    Blocked,
}

impl std::fmt::Debug for EndpointKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Private(_) => f.write_str("Private([REDACTED])"),
            Self::External {
                settings_key_verified,
            } => f
                .debug_struct("External")
                .field("settings_key_verified", settings_key_verified)
                .finish(),
            Self::Blocked => f.write_str("Blocked"),
        }
    }
}

/// 一个端点在登记表里的状态：此刻的口径（`None` = 没人管）与它的**代际**。代际只增不减，口径每变一次加一，所以「这个请求
/// 是在哪一代准入的」能和「现在是哪一代」比较。条目从不删除（撤销只是把口径清成 `None`，代际照加）：同一个端点的代际
/// 永远不会重复。
#[derive(Clone)]
struct Governance {
    generation: u64,
    key: Option<EndpointKey>,
}

/// 进程里被引擎管理器接管的端点（键是 [`endpoint_identity`]）。进程全局：三个调用点（适配器、代理、模型列表）各自
/// 按 URL 解析 key，没有一条共同的对象能把表传下去。没有登记的端点（非托管的卡）行为与以前完全一样。
static GOVERNED: RwLock<BTreeMap<String, Governance>> = RwLock::new(BTreeMap::new());

/// 端点的规范标识 `scheme://host[:port]`（host 小写），不含路径、凭据、查询、片段；不是 http(s) URL 时没有。
/// `http://127.0.0.1:19731`、`http://127.0.0.1:19731/` 与带路径的写法是同一个端点。
pub fn endpoint_identity(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// 登记（或更新）一个端点的 key 口径。口径变了（与此刻登记的不相等）代际加一；登记同一个口径不变。`base_url` 不是
/// http(s) URL 时什么也不做，返回 `false`。
pub fn govern_endpoint(base_url: &str, policy: EndpointKey) -> bool {
    let Some(identity) = endpoint_identity(base_url) else {
        return false;
    };
    let mut governed = GOVERNED.write().unwrap_or_else(PoisonError::into_inner);
    let slot = governed.entry(identity).or_insert(Governance {
        generation: 0,
        key: None,
    });
    if slot.key.as_ref() != Some(&policy) {
        slot.key = Some(policy);
        slot.generation += 1;
    }
    true
}

/// 封上一个端点：之后什么凭证都不发（[`EndpointKey::Blocked`]）。**引擎停了、正在被替换、监听者没了时用它，不是
/// [`release_endpoint`]**：撤销登记会让回退链（环境变量、`settings.json`）重新适用于这个端点，把用户的 key 发给之后占着
/// 这个端口、还没通过校验的别的东西。
pub fn block_endpoint(base_url: &str) -> bool {
    govern_endpoint(base_url, EndpointKey::Blocked)
}

/// 撤销登记：之后这个端点回到没人管的状态（按 §2.9 的顺序）。**只用在端点不再是托管的时候**（v0.6.0 里托管的端点永远不会
/// 变成非托管，引擎管理器用 [`block_endpoint`]）；代际加一。
pub fn release_endpoint(base_url: &str) {
    if let Some(identity) = endpoint_identity(base_url) {
        let mut governed = GOVERNED.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = governed.get_mut(&identity)
            && slot.key.take().is_some()
        {
            slot.generation += 1;
        }
    }
}

/// 一个端点当前的登记口径。
pub fn governed_endpoint(base_url: &str) -> Option<EndpointKey> {
    governance_of(base_url).0
}

/// 一个端点此刻的代际：口径每变一次加一，没登记过的端点是 0。
pub fn endpoint_generation(base_url: &str) -> u64 {
    governance_of(base_url).1
}

/// 同一次读里的口径与代际（两者一致）。
fn governance_of(base_url: &str) -> (Option<EndpointKey>, u64) {
    let Some(identity) = endpoint_identity(base_url) else {
        return (None, 0);
    };
    GOVERNED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&identity)
        .map_or((None, 0), |slot| (slot.key.clone(), slot.generation))
}

/// 为什么一个请求拿不到凭证（[`resolve_omlx_api_key_checked`]）。说明里没有任何 key。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefusal {
    /// 请求准入时绑定的引擎代际不是当前的：它属于一个已经被换掉的引擎进程，不能拿新引擎的凭证，请求体也不能发给它。
    StaleGeneration { admitted: u64, current: u64 },
    /// 端点被封上了（[`EndpointKey::Blocked`]）：没有一个通过校验的引擎在它后面，什么都不发。
    Blocked,
}

impl std::fmt::Display for KeyRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::StaleGeneration { admitted, current } => write!(
                f,
                "the engine behind this endpoint was replaced (this request was admitted for generation {admitted}, the current one is {current}): the request was not sent"
            ),
            Self::Blocked => f.write_str(
                "no verified engine is behind this endpoint right now (it is blocked): the request was not sent",
            ),
        }
    }
}

impl std::error::Error for KeyRefusal {}

/// 按请求取 key，并核对准入时的代际（IDORIS-CORE S6）：`admitted_generation` 是这个请求所属的操作被准入时端点的代际
/// （`None` = 不绑定，按此刻的口径）。代际不是当前的、或端点被封上时拒绝（`Err`），什么凭证都不给；否则 `Ok(key)`
/// （`None` = 不带 key）。口径与代际在同一次读里取，所以「核对通过」与「给出的 key」一致。
pub fn resolve_omlx_api_key_checked(
    base_url: &str,
    admitted_generation: Option<u64>,
) -> Result<Option<String>, KeyRefusal> {
    let (governed, generation) = governance_of(base_url);
    if let Some(admitted) = admitted_generation
        && admitted != generation
    {
        return Err(KeyRefusal::StaleGeneration {
            admitted,
            current: generation,
        });
    }
    if governed == Some(EndpointKey::Blocked) {
        return Err(KeyRefusal::Blocked);
    }
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    Ok(resolve_for_endpoint(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        is_loopback_base_url(base_url),
        governed.as_ref(),
    )
    .and_then(|key| key.into_string().ok()))
}

/// 纯解析：在 [`resolve_from_for`] 之上加「端点被引擎管理器接管」的口径（`governed`）。
/// 私有 key 一律优先；外部引擎的 `settings.json` key 只在校验过时给；没有登记就是原来的顺序。
pub fn resolve_for_endpoint(
    env: Option<OsString>,
    home: Option<&Path>,
    loopback: bool,
    governed: Option<&EndpointKey>,
) -> Option<OsString> {
    match governed {
        // 封上的端点：什么都不发，回退链（环境变量、`settings.json`）不适用。
        Some(EndpointKey::Blocked) => None,
        Some(EndpointKey::Private(key)) => Some(OsString::from(key)),
        Some(EndpointKey::External {
            settings_key_verified,
        }) => match resolve_with_source(env, home)? {
            (key, KeySource::Env) => Some(key),
            (key, KeySource::Settings) => (loopback && *settings_key_verified).then_some(key),
        },
        None => resolve_from_for(env, home, loopback),
    }
}

/// 同上，但直接给 `loopback`，并保留非 UTF-8 的环境变量，让调用方按「无效凭证」处理。
/// `loopback` 为 false 时 `settings.json` 的 key 不会被返回。
pub fn resolve_omlx_api_key_os(loopback: bool) -> Option<OsString> {
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    resolve_from_for(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        loopback,
    )
}

/// 纯解析：`env` 是环境变量值，`home` 是家目录。测试用它注入假 HOME。
pub fn resolve_with_source(
    env: Option<OsString>,
    home: Option<&Path>,
) -> Option<(OsString, KeySource)> {
    if let Some(value) = env.filter(|value| !value.is_empty()) {
        return Some((value, KeySource::Env));
    }
    settings_api_key(home?).map(|key| (OsString::from(key), KeySource::Settings))
}

/// [`resolve_with_source`] 再按目标是否 loopback 过滤：文件 key 只发给 loopback。
pub fn resolve_from_for(
    env: Option<OsString>,
    home: Option<&Path>,
    loopback: bool,
) -> Option<OsString> {
    match resolve_with_source(env, home)? {
        (key, KeySource::Env) => Some(key),
        (key, KeySource::Settings) => loopback.then_some(key),
    }
}

/// loopback 目标下的解析（等价于 `resolve_from_for(.., true)`）。
pub fn resolve_from(env: Option<OsString>, home: Option<&Path>) -> Option<OsString> {
    resolve_from_for(env, home, true)
}

/// `<home>/.omlx/settings.json` 的 `auth.api_key`；任何问题都是 `None`。
pub fn settings_api_key(home: &Path) -> Option<String> {
    let path = home.join(".omlx").join("settings.json");
    // 先看类型再打开：FIFO / 设备文件一打开就可能阻塞；symlink_metadata 不跟随符号链接。
    if !fs::symlink_metadata(&path).ok()?.is_file() {
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
    let key = key.trim();
    (!key.is_empty() && !key.chars().any(|c| c.is_control() || c.is_whitespace()))
        .then(|| key.to_owned())
}
