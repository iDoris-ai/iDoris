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
//! - `settings.json` 只从文件读取，最多读 64 KiB；必须是常规文件（最后一段是符号链接、或是 FIFO、设备，一律不要）：
//!   以 HOME 为起点，逐段用 `O_NOFOLLOW` 打开 `.omlx` 与 `settings.json`，再用 `fstat` 核对文件类型、有效 UID 与权限位。
//!   文件必须属于当前用户，组和其他用户不能有写权限。解析失败时等 10 毫秒，重新打开并读取一次；再次失败会拒绝发送。
//!   文件缺失、或合法 JSON 中没有可用的 `auth.api_key` 时，可以匿名连接。key 取 trim 后的值，含空白或控制字符视为没有。
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
//! - **发送前的授权**（IDORIS-CORE S6，Codex 终审 H1；owner 2026-11-22 拍板）：对一个托管的端点，每个要发出去的请求（带 key 的
//!   与不带的）都经 [`authorize_omlx_request`]：在**同一个任务里、紧挨着发送之前**核对代际与口径，并让登记时一起交来的校验守卫
//!   （[`EndpointGuard`]）**重新**确认此刻监听这个端口的还是它登记时认的那个监听者；确认不了（不一样，或者读不出来）就拒绝，什么
//!   都不发。这把「校验通过」与「发送」之间的间隙从「管理器的一个监管节拍」缩到「一次校验到一次发送之间的毫秒」，没有消除：
//!   本机**另一个用户**的进程恰好在这几毫秒里抢到端口，仍能收到那一个请求（同一个用户的进程本来就读得到 `settings.json` 与进程
//!   环境，什么也没多得；libproc 与 `lsof` 都看不到别的用户的套接字，所以这个校验对它们是盲的）。不做逐连接的对端进程核对。授权的结果带着守卫**核验过的
//!   拨号地址**（[`Authorization::dial`]），发送端只拨它（[`Authorization::dial_url`]：卡里写的 `localhost`、`[::1]` 换成它），并用不保留空闲连接的客户端
//!   （[`Authorization::client`]），凭证只走授权之后新开的连接。守卫每个请求只问登记时认的那个进程此刻还握着一个拨号地址连得到的 LISTEN 套接字（原生，不起
//!   子进程；读不了的那一次改问贵的），「端口上只有它一个监听者」由管理器的监督节拍问（`engine::guard`）。授权同时登记一个在途的操作（[`FlightGuard`]），
//!   引擎管理器排空时等它们做完。
//! - **由端点决定，不由卡决定**（Codex 预审 L2 的 H1）：一个端点是不是被管理的（[`is_governed_endpoint`]）看登记表里有没有它的规范化标识。同一个端口上不能带 key 的卡
//!   （`idoris_upstream` 的 resident 卡、IPv4 映射写法的卡）发往它的请求照样过 [`authorize_omlx_request_without_key`]：同一套代际、封上、守卫、核验过的拨号地址、
//!   在途登记，只是不带凭证。key 的资格（[`card_may_carry_omlx_key`]）与登记表用同一个 loopback 判据（[`is_loopback_host`]）。
//! - **授权不是通行证**（预审 L2 的 H3）：授权之后到真的发出之间，调用方还要建请求、序列化请求体；发出前最后一刻同步地再看一眼端点
//!   （[`Authorization::ensure_current`]），代际变了或端点封上了就拒绝。外部引擎的 `settings.json` 在最后的核对之前读好。
//! - **端点的标识**（Codex 终审 H5）：loopback 的各种写法（`localhost`、`127.x.y.z`、`::1`）与协议不区分，同一个端口是同一个标识
//!   （[`endpoint_identity`]）：管理器登记的是 `127.0.0.1:<port>`，卡里写成 `localhost:<port>` 的伴生卡也落在同一条登记上（继承它的
//!   封上与失败），**不会**因为主机名的写法不同而逃出登记、落进 §2.9 的回退链。

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use async_trait::async_trait;

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

/// `base_url` 的主机是否是 loopback（localhost / 127.0.0.0/8 / ::1，含 IPv4 映射的写法 `::ffff:127.x.y.z`）且协议是 http(s)、不带用户信息。
/// 与端点标识（[`endpoint_identity`]）是同一个判据（[`is_loopback_host`]）：登记表把映射的写法并成同一个端口，key 的资格与「发往 loopback 才读
/// `settings.json`」也必须这样认，否则一张映射写法的卡落在被管理的端点上，却被当成「不是 loopback、不带 key、不归登记表」（IDORIS-CORE S6，
/// Codex 预审 L2 的 H1）。
pub fn is_loopback_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some_and(is_loopback_host)
}

/// 卡声明自己是自定义 OpenAI 兼容上游的扩展名（`setup upstream` 的卡必带）。与 `idoris-router` 的
/// `engine_kind::UPSTREAM_EXTENSION` 是同一个字符串（router 的测试钉住两处一致）。
pub const UPSTREAM_EXTENSION: &str = "idoris_upstream";

/// 这张卡能不能带 oMLX 的 key（IDORIS-CORE S6，Codex 审计 H2）。**代理、模型列表、就绪探测、预热与生命周期适配器的工厂共用这
/// 一处判断**，所以没有哪一条路径会对一张手写的、伪装成 oMLX 的卡发 key。
///
/// 规则按卡识别：本地族、loopback、`version_pin` 的产品名是 `omlx`、端点是不带凭据的 loopback 主机，**且卡不带
/// `extensions.idoris_upstream`**。`omlx` 与伴生卡 `omlx-embed` 都满足；自定义 OpenAI 兼容上游（`setup upstream` 的卡必带
/// `idoris_upstream`）即使把 `flavor`、`version_pin` 写成 oMLX、端口写成 oMLX 的，也不带 key。key 的来源不在这里（见本模块
/// 的其余部分）。
pub fn card_may_carry_omlx_key(card: &idoris_contracts::ComponentCard) -> bool {
    if card.provider.family != idoris_contracts::provider::Family::Local
        || card.provider.locality != idoris_contracts::provider::Locality::Loopback
        || !card.version_pin.starts_with("omlx@")
        || card
            .extensions
            .as_ref()
            .is_some_and(|ext| ext.contains_key(UPSTREAM_EXTENSION))
    {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(&card.endpoint) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.host_str().is_some_and(is_loopback_host)
}

static RESOLUTIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// 到目前为止，这个进程从环境与 `settings.json` 解析过几次 oMLX API key（每次读环境、可能读用户的密钥文件）。诊断与测试用：
/// 启动时对适配器的干跑**不该**解析任何凭证（凭证只在真正建适配器时才解析），测试靠它数。
pub fn resolutions() -> usize {
    RESOLUTIONS.load(std::sync::atomic::Ordering::SeqCst)
}

fn note_resolution() {
    RESOLUTIONS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
}

/// 当前进程环境下、发往 `base_url` 的 key（UTF-8）。引擎管理器接管的端点按它登记的口径（[`EndpointKey`]）。
pub fn resolve_omlx_api_key_for(base_url: &str) -> Option<String> {
    resolve_omlx_api_key_os_for(base_url)?.into_string().ok()
}

/// 同上，保留非 UTF-8 的环境变量，让调用方按「无效凭证」处理。所有按目标 URL 解析 key 的路径（生命周期适配器、
/// 常驻代理、模型列表、就绪探测）都经这里，所以引擎管理器登记的私有 key 与「外部引擎的 settings.json key 要先过
/// 端口属主校验」对它们同时生效。
pub fn resolve_omlx_api_key_os_for(base_url: &str) -> Option<OsString> {
    note_resolution();
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    resolve_for_endpoint(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        is_loopback_base_url(base_url),
        governed_endpoint(base_url).as_ref(),
    )
}

/// [`resolve_omlx_api_key_os_for`] 的带拒绝版本：`settings.json` 不可核验（权限不对、两次读到不完整的 JSON）是 `Err`，不折成「没有 key」。
/// 固定 key 的适配器构造器用它，免得匿名请求在配置文件损坏时悄悄放行。
pub fn resolve_omlx_api_key_for_checked(base_url: &str) -> Result<Option<String>, KeyRefusal> {
    note_resolution();
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    resolve_checked_in(
        base_url,
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
    )
}

/// [`resolve_omlx_api_key_for_checked`] with the environment value and the home given (tests cannot change the process environment).
pub(crate) fn resolve_checked_in(
    base_url: &str,
    env: Option<OsString>,
    home: Option<&Path>,
) -> Result<Option<String>, KeyRefusal> {
    Ok(resolve_for_endpoint_checked(
        env,
        home,
        is_loopback_base_url(base_url),
        governed_endpoint(base_url).as_ref(),
    )?
    .and_then(|key| key.into_string().ok()))
}

/// [`resolve_checked_in`] for the request path: the same decision (this endpoint's governedscope included), with the `settings.json` read off the
/// runtime worker and through the stat-keyed cache. A fixed-key adapter that was built while the settings could not be verified uses it on
/// every request until the file can be verified again.
pub(crate) async fn resolve_checked_async_in(
    base_url: &str,
    env: Option<OsString>,
    home: Option<&Path>,
) -> Result<Option<String>, KeyRefusal> {
    let governed = governed_endpoint(base_url);
    match plan_for_endpoint(env, home, is_loopback_base_url(base_url), governed.as_ref()) {
        Plan::Done(key) => Ok(key.and_then(|key| key.into_string().ok())),
        Plan::Settings(home) => read_settings_key_async(&home)
            .await
            .map_err(|_| KeyRefusal::SettingsUnverifiable),
    }
}

/// 只回答「`base_url` 是不是引擎管理器接管的端点」：是，给出发往它的 key（`None` = 不带 key）；不是，`None` 外面再包一层
/// （调用方沿用自己原来的 key 来源）。
pub fn resolve_governed_os(base_url: &str) -> Option<Option<OsString>> {
    note_resolution();
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

/// 守卫对「此刻监听这个端口的，还是登记时认的那个监听者吗」的回答（[`EndpointGuard::verify`]）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verification {
    /// 是：监听者就是登记时认的那个（自有引擎：它的进程是这个端口上唯一的监听者；外部引擎：每个监听者的身份——pid 加启动时间——
    /// 都与校验通过时的一样，并且重新过了 uid 与签名团队）。
    Verified,
    /// 不是了：没有监听者，监听者换了，或者端口上多了别的监听者。
    Mismatch,
    /// 读不出来（`lsof`、`ps` 超时或出错）：既不能说是，也不能说不是；按「不是」处理，拒绝发送。
    Unverifiable,
}

/// 引擎管理器登记口径（私有 key、外部）时一起交来的校验守卫：每个要发往这个端点的请求，在发出之前由 [`authorize_omlx_request`]
/// 调它，**重新**确认此刻监听这个端口的还是登记时认的那个监听者（IDORIS-CORE S6，Codex 终审 H1）。守卫只在口径是私有或外部的
/// 时候有；封上、撤销都清掉它。
#[async_trait]
pub trait EndpointGuard: Send + Sync + std::fmt::Debug {
    async fn verify(&self) -> Verification;

    /// Clear positive fallback evidence synchronously. Called under the endpoint registry lock.
    fn invalidate_positive(&self) {}

    /// **经过核验的拨号地址**：守卫验证的就是「登记时认的进程握着一个**这个地址**连得到的 LISTEN 套接字」，授权结果把它带给发送端，
    /// 发送端只拨它（[`Authorization::dial_url`]），不拨卡里写的别名（`localhost`、`[::1]`、`127.0.0.2`：登记表把它们并成同一个端口
    /// 身份，但它们不是同一个地址，各自可以有不同的监听者；Codex 专项评审 H1）。`None`：这个守卫不限定地址（测试里的假守卫）。
    fn dial(&self) -> Option<std::net::SocketAddr> {
        None
    }
}

/// 在途计数（[`Governance::flights`]）的最高位：这个端点的在途登记**封上**了（[`seal_endpoint_flights_if_idle`]）。封上之后不再有新的
/// [`FlightGuard`]，已经持有的照常放掉。「在途数为零」与「封上」是同一个原子操作（对计数做一次 `compare_exchange(0, SEALED)`），所以排空的
/// 管理器看到零的那一刻就封上，没有「看到零、还没封，一个新的请求挤进来」的间隙（IDORIS-CORE S6，Codex 预审 L3 的 M5）。
const FLIGHTS_SEALED: usize = 1 << (usize::BITS - 1);

/// 一个端点在登记表里的状态：此刻的口径（`None` = 没人管）、它的**代际**、校验守卫、在途的操作数。代际只增不减，口径（或守卫）
/// 每变一次加一，所以「这个请求是在哪一代准入的」能和「现在是哪一代」比较。条目从不删除（撤销只是把口径清成 `None`，代际照加）：
/// 同一个端点的代际永远不会重复。
#[derive(Clone)]
struct Governance {
    generation: u64,
    key: Option<EndpointKey>,
    guard: Option<Arc<dyn EndpointGuard>>,
    /// 此刻持有 [`FlightGuard`] 的操作数（低位，[`endpoint_flights`]），加上最高位的「封上」标志（[`FLIGHTS_SEALED`]）。封上只活在
    /// 一代之内：口径（或守卫）变了、代际前进的那一刻解开（[`Governance::advance`]）。
    flights: Arc<AtomicUsize>,
}

impl Governance {
    fn new() -> Self {
        Self {
            generation: 0,
            key: None,
            guard: None,
            flights: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// 口径（或守卫）变了：代际前进，在途登记的封上解开。封上是给「这个引擎正在被停止」的那一代的（排空成功之后、进程确认消失之前，
    /// 不再让新的请求进来）；下一代（封上的端点、新引擎的私有 key）有它自己的口径来决定放不放请求。
    fn advance(&mut self) {
        self.generation += 1;
        self.flights.fetch_and(!FLIGHTS_SEALED, Ordering::SeqCst);
    }
}

/// 进程里被引擎管理器接管的端点（键是 [`endpoint_identity`]）。进程全局：几个调用点（适配器、代理、模型列表、就绪探测）各自
/// 按 URL 取凭证，没有一条共同的对象能把表传下去。没有登记的端点（非托管的卡）行为与以前完全一样。
static GOVERNED: RwLock<BTreeMap<String, Governance>> = RwLock::new(BTreeMap::new());

/// loopback 的主机写法：`localhost`、`127.0.0.0/8`、`::1`（含 IPv4 映射的写法）。**唯一的判据**：端点标识、key 的资格（[`card_may_carry_omlx_key`]）、
/// `settings.json` 只发往 loopback（[`is_loopback_base_url`]）、代理的出站统计都用它，所以同一个套接字的各种写法在每一处得到同一个答案。
pub fn is_loopback_host(host: &str) -> bool {
    // `localhost.`（带点的完全限定写法）是同一个名字（S8 的预审也发现了它）。
    host.eq_ignore_ascii_case("localhost")
        || host.eq_ignore_ascii_case("localhost.")
        || host
            .trim_matches(['[', ']'])
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.to_canonical().is_loopback())
}

/// 端点的规范标识，不含路径、凭据、查询、片段；不是 http(s) URL 时没有。
///
/// - **loopback**（`localhost`、`127.x.y.z`、`::1`，http 或 https）：`loopback:<port>`。同一个端口上监听的是谁，与请求写的是哪个
///   loopback 别名无关（IDORIS-CORE S6，Codex 终审 H5）：管理器登记的是 `127.0.0.1:<port>`，伴生卡写成 `localhost:<port>` 或
///   `[::1]:<port>` 的，落在同一条登记上，继承它的封上与失败，不会按主机名的写法逃出去、走 §2.9 的回退链。
/// - 别的主机：`scheme://host[:port]`（host 小写）。
pub fn endpoint_identity(base_url: &str) -> Option<String> {
    let url = reqwest::Url::parse(base_url).ok()?;
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    let host = url.host_str()?.to_ascii_lowercase();
    if is_loopback_host(&host) {
        return Some(format!("loopback:{}", url.port_or_known_default()?));
    }
    Some(match url.port() {
        Some(port) => format!("{}://{host}:{port}", url.scheme()),
        None => format!("{}://{host}", url.scheme()),
    })
}

/// 登记（或更新）一个端点的 key 口径，没有校验守卫。口径变了（与此刻登记的不相等，或者守卫不同）代际加一；登记同一个口径不变。
/// `base_url` 不是 http(s) URL 时什么也不做，返回 `false`。
pub fn govern_endpoint(base_url: &str, policy: EndpointKey) -> bool {
    govern_endpoint_guarded(base_url, policy, None)
}

/// 同 [`govern_endpoint`]，带校验守卫：之后发往这个端点的每个请求，发送前都要让守卫重新确认监听者（[`authorize_omlx_request`]）。
/// 守卫换了（不是同一个对象）也算口径变了，代际加一。
pub fn govern_endpoint_guarded(
    base_url: &str,
    policy: EndpointKey,
    guard: Option<Arc<dyn EndpointGuard>>,
) -> bool {
    let Some(identity) = endpoint_identity(base_url) else {
        return false;
    };
    let mut governed = GOVERNED.write().unwrap_or_else(PoisonError::into_inner);
    let slot = governed.entry(identity).or_insert_with(Governance::new);
    let same_guard = match (&slot.guard, &guard) {
        (None, None) => true,
        (Some(current), Some(next)) => Arc::ptr_eq(current, next),
        _ => false,
    };
    if slot.key.as_ref() != Some(&policy) || !same_guard {
        if let Some(previous) = &slot.guard {
            previous.invalidate_positive();
        }
        slot.key = Some(policy);
        slot.guard = guard;
        slot.advance();
    }
    true
}

/// 封上一个端点：之后什么凭证都不发（[`EndpointKey::Blocked`]）。**引擎停了、正在被替换、监听者没了时用它，不是撤销登记**：
/// 撤销登记会让回退链（环境变量、`settings.json`）重新适用于这个端点，把用户的 key 发给之后占着这个端口、还没通过校验的别的东西。
pub fn block_endpoint(base_url: &str) -> bool {
    govern_endpoint(base_url, EndpointKey::Blocked)
}

/// 撤销登记：之后这个端点回到没人管的状态（按 §2.9 的顺序）；代际加一。**只在测试里用**（IDORIS-CORE S6，Codex 终审 L3）：
/// v0.6.0 里托管的端点永远不会变成非托管，引擎管理器用 [`block_endpoint`]；生产代码里没有任何路径能把一个托管的端点交还给回退链。
/// 测试用它在用例之间清理进程全局的登记表（`test-support` 特性，或本 crate 自己的单元测试）。
#[cfg(any(test, feature = "test-support"))]
pub fn release_endpoint(base_url: &str) {
    if let Some(identity) = endpoint_identity(base_url) {
        let mut governed = GOVERNED.write().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = governed.get_mut(&identity)
            && (slot.key.take().is_some() || slot.guard.take().is_some())
        {
            slot.guard = None;
            slot.advance();
        }
    }
}

/// 测试清理：把一个端点从进程全局的登记表里**整条删掉**（口径、守卫、代际、在途计数一起），之后它与「从没登记过」一模一样（代际 0）。
///
/// 与 [`release_endpoint`] 不同：那个撤销口径、代际加一、条目留着（所以测试里「撤销之后代际不会重复」才测得出来）。登记表是进程全局的，
/// 操作系统很快会把同一个端口复用给同一个进程里别的用例的假上游；用例结束时只撤销口径、留着代际不是 0 的条目，会带进下一个用例（那个用例
/// 若断言「没登记过的端点代际是 0」就会错）。用例的清理用这个。**只在测试里用**（同 [`release_endpoint`]）：生产里条目从不删除，一个端点的
/// 代际永远不会重复；删条目的时候还有操作持着这个端点旧的在途登记也没关系（它们各自持有自己那份计数）。
#[cfg(any(test, feature = "test-support"))]
pub fn forget_endpoint(base_url: &str) {
    if let Some(identity) = endpoint_identity(base_url) {
        GOVERNED
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&identity);
    }
}

/// 一个端点当前的登记口径。
pub fn governed_endpoint(base_url: &str) -> Option<EndpointKey> {
    snapshot_of(base_url).key
}

/// 这个端点是不是引擎管理器接管的（登记过口径，封上也算）。**由端点（规范化的标识）决定，与哪张卡、卡能不能带 key 无关**（IDORIS-CORE S6，Codex 预审
/// L2 的 H1）：同一个端口上的另一张卡（`extensions.idoris_upstream` 的 resident 卡、IPv4 映射写法的卡）不能带 key，但发往这个端口的请求照样要过
/// 代际、封上、监听者的核对，照样发往核验过的拨号地址，照样登记在途。卡的资格（[`card_may_carry_omlx_key`]）只管凭证**带不带**。
pub fn is_governed_endpoint(base_url: &str) -> bool {
    snapshot_of(base_url).key.is_some()
}

/// 一个端点此刻的代际：口径每变一次加一，没登记过的端点是 0。
pub fn endpoint_generation(base_url: &str) -> u64 {
    snapshot_of(base_url).generation
}

/// 此刻有几个操作持有这个端点的 [`FlightGuard`]：引擎管理器排空时等它归零（IDORIS-CORE S6，Codex 终审 M1）。不含「封上」标志。
pub fn endpoint_flights(base_url: &str) -> usize {
    snapshot_of(base_url).flights.map_or(0, |flights| {
        flights.load(Ordering::SeqCst) & !FLIGHTS_SEALED
    })
}

/// **在途数为零就封上**这个端点的在途登记，一个原子操作：返回 `true` = 此刻一个在途的操作也没有，并且从这一刻起不再有新的
/// （之后的每个请求都被 [`authorize_omlx_request`] 以 [`KeyRefusal::Draining`] 拒绝，一个字节也不发给引擎）；`false` = 还有在途的
/// （什么也没改，调用方接着等）。
///
/// 引擎管理器排空用它代替「读在途数、看见零」（IDORIS-CORE S6，Codex 预审 L3 的 M5）：分开读与封，中间一个早先抓到这张托管卡的请求
/// （比如公共模型列表逐卡走到了它）会挤进来，在进程还在宽限期里监听时登记新的在途，并被随后的 SIGTERM 打断。封上只活在这一代之内：
/// 口径（或守卫）变了、代际前进（进程确认消失之后端点封上，新引擎登记它的私有 key）的时候解开。没有登记的端点没有在途登记可封：`true`。
pub fn seal_endpoint_flights_if_idle(base_url: &str) -> bool {
    snapshot_of(base_url).flights.is_none_or(|flights| {
        flights
            .compare_exchange(0, FLIGHTS_SEALED, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
            // 已经封上了（前一轮的排空，同一代之内）而且一个在途的也没有：同样是「空的、封着的」。
            || flights.load(Ordering::SeqCst) == FLIGHTS_SEALED
    })
}

/// 一次读里的口径、代际、守卫与在途计数（彼此一致；`sealed` 是读那一刻在途登记有没有封上）。
struct Snapshot {
    key: Option<EndpointKey>,
    generation: u64,
    guard: Option<Arc<dyn EndpointGuard>>,
    flights: Option<Arc<AtomicUsize>>,
    sealed: bool,
}

fn snapshot_of(base_url: &str) -> Snapshot {
    let empty = Snapshot {
        key: None,
        generation: 0,
        guard: None,
        flights: None,
        sealed: false,
    };
    let Some(identity) = endpoint_identity(base_url) else {
        return empty;
    };
    GOVERNED
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&identity)
        .map_or(empty, |slot| Snapshot {
            key: slot.key.clone(),
            generation: slot.generation,
            guard: slot.guard.clone(),
            sealed: slot.flights.load(Ordering::SeqCst) & FLIGHTS_SEALED != 0,
            flights: Some(slot.flights.clone()),
        })
}

/// 为什么一个请求拿不到凭证（[`resolve_omlx_api_key_checked`]、[`authorize_omlx_request`]）。说明里没有任何 key。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyRefusal {
    /// 请求准入时绑定的引擎代际不是当前的：它属于一个已经被换掉的引擎进程，不能拿新引擎的凭证，请求体也不能发给它。
    StaleGeneration { admitted: u64, current: u64 },
    /// 端点被封上了（[`EndpointKey::Blocked`]）：没有一个通过校验的引擎在它后面，什么都不发。
    Blocked,
    /// 排空已经完成、这个引擎正要被停止（[`seal_endpoint_flights_if_idle`]）：不再接收新的操作，什么都不发（IDORIS-CORE S6，
    /// Codex 预审 L3 的 M5）。进程确认消失之后端点会封上（[`Self::Blocked`]），新引擎登记之后才再放请求进来。
    Draining,
    /// 发送前的重新校验说：此刻监听这个端口的已经不是登记时认的那个监听者了（Codex 终审 H1）。
    ListenerChanged,
    /// 发送前的重新校验读不出来（工具超时、出错）：确认不了就不发（fail closed）。
    Unverified,
    /// The selected settings file could not be read and verified.
    SettingsUnverifiable,
    /// 托管端点的专用 HTTP 客户端（不保留空闲连接）建不出来：不退到别的客户端（池里可能有通往旧进程的连接，调用方注入的客户端还可能有别的
    /// 代理、重定向策略），什么都不发（Codex 预审 L2 的 M4）。
    TransportUnavailable,
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
            Self::Draining => f.write_str(
                "the engine behind this endpoint is being stopped (its in-flight work has drained and no new work is admitted): the request was not sent",
            ),
            Self::ListenerChanged => f.write_str(
                "what listens on this endpoint is no longer the engine it was verified for: the request was not sent",
            ),
            Self::Unverified => f.write_str(
                "what listens on this endpoint could not be confirmed right before sending: the request was not sent",
            ),
            Self::SettingsUnverifiable => f.write_str(
                "oMLX settings.json could not be verified (for example: not valid JSON, not owned by this user or root, writable by group or others, an ACL grants others write, an untrusted directory on its path, or a read error): the request was not sent",
            ),
            Self::TransportUnavailable => f.write_str(
                "the HTTP client for managed endpoints could not be built: the request was not sent",
            ),
        }
    }
}

impl std::error::Error for KeyRefusal {}

/// 只核对代际与封上，不取 key、不重新校验监听者、不登记在途：代理在占熔断名额**之前**用它预检（被拒绝的发送留下的不是提供方的
/// 失败）；真正的发送前核对在 [`authorize_omlx_request`]。
pub fn check_endpoint_admission(
    base_url: &str,
    admitted_generation: Option<u64>,
) -> Result<(), KeyRefusal> {
    let snapshot = snapshot_of(base_url);
    refuse_stale_or_blocked(&snapshot, admitted_generation)
}

fn refuse_stale_or_blocked(
    snapshot: &Snapshot,
    admitted_generation: Option<u64>,
) -> Result<(), KeyRefusal> {
    if let Some(admitted) = admitted_generation
        && admitted != snapshot.generation
    {
        return Err(KeyRefusal::StaleGeneration {
            admitted,
            current: snapshot.generation,
        });
    }
    if snapshot.key == Some(EndpointKey::Blocked) {
        return Err(KeyRefusal::Blocked);
    }
    if snapshot.sealed {
        return Err(KeyRefusal::Draining);
    }
    Ok(())
}

/// 按请求取 key，并核对准入时的代际（IDORIS-CORE S6）：`admitted_generation` 是这个请求所属的操作被准入时端点的代际
/// （`None` = 不绑定，按此刻的口径）。代际不是当前的、或端点被封上时拒绝（`Err`），什么凭证都不给；否则 `Ok(key)`
/// （`None` = 不带 key）。口径与代际在同一次读里取，所以「核对通过」与「给出的 key」一致。
///
/// **不重新校验监听者，也不登记在途的操作**：它是纯解析，给单元测试与不发请求的调用方用。每一个真的要发出去的请求都用
/// [`authorize_omlx_request`]。
pub fn resolve_omlx_api_key_checked(
    base_url: &str,
    admitted_generation: Option<u64>,
) -> Result<Option<String>, KeyRefusal> {
    let snapshot = snapshot_of(base_url);
    refuse_stale_or_blocked(&snapshot, admitted_generation)?;
    Ok(resolve_from_snapshot(base_url, snapshot.key.as_ref())?
        .and_then(|key| key.into_string().ok()))
}

/// [`resolve_from_snapshot`] for the request path: the same decision, with the `settings.json` read off the runtime worker.
async fn resolve_from_snapshot_async(
    base_url: &str,
    governed: Option<&EndpointKey>,
) -> Result<Option<OsString>, KeyRefusal> {
    note_resolution();
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    match plan_for_endpoint(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        is_loopback_base_url(base_url),
        governed,
    ) {
        Plan::Done(key) => Ok(key),
        Plan::Settings(home) => read_settings_key_async(&home)
            .await
            .map(|key| key.map(OsString::from))
            .map_err(|_| KeyRefusal::SettingsUnverifiable),
    }
}

fn resolve_from_snapshot(
    base_url: &str,
    governed: Option<&EndpointKey>,
) -> Result<Option<OsString>, KeyRefusal> {
    note_resolution();
    let home = std::env::var_os("HOME").filter(|home| !home.is_empty());
    resolve_for_endpoint_checked(
        std::env::var_os(OMLX_API_KEY_ENV),
        home.as_deref().map(Path::new),
        is_loopback_base_url(base_url),
        governed,
    )
}

/// 一个在途的操作（IDORIS-CORE S6，Codex 终审 M1）：持有它期间，这个端点的 [`endpoint_flights`] 加一。流式响应的字节流、后台
/// 预热、模型列表的请求、聊天……凡是发往托管端点的请求都经 [`authorize_omlx_request`]，所以排空要等的「在途」不会漏掉哪一种。
#[derive(Debug)]
pub struct FlightGuard {
    flights: Arc<AtomicUsize>,
}

impl FlightGuard {
    /// 登记一个在途的操作；在途登记已经封上了（[`seal_endpoint_flights_if_idle`]）就不登记，`None`。登记与「有没有封上」是同一个原子
    /// 更新，不会在封上之后还让一个请求挤进去。
    fn enter(flights: &Arc<AtomicUsize>) -> Option<Self> {
        flights
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                (value & FLIGHTS_SEALED == 0).then_some(value + 1)
            })
            .ok()
            .map(|_| Self {
                flights: flights.clone(),
            })
    }
}

impl Drop for FlightGuard {
    fn drop(&mut self) {
        self.flights.fetch_sub(1, Ordering::SeqCst);
    }
}

/// [`authorize_omlx_request`] 的结果：这个请求可以发了，凭证是 `key`（没有 = 不带），`generation` 是授权那一刻端点的代际，在途
/// 登记（`_flight`）活到这个值被丢弃为止：**调用方要把它握到响应读完（流式的握到流结束）**。
#[must_use = "hold the authorization until the response has been read"]
pub struct Authorization {
    key: Option<OsString>,
    generation: u64,
    /// 授权的端点（卡里写的 URL；[`Self::ensure_current`] 用它重新读登记）。
    endpoint: String,
    /// 守卫核验过的拨号地址（托管的端点）；`None`：非托管的端点，请求照它自己的写法发。
    dial: Option<std::net::SocketAddr>,
    /// 托管端点（有 `dial`）的专用客户端，授权的时候就取到了：取不到授权就失败（[`KeyRefusal::TransportUnavailable`]），发送端不会悄悄换成共用的
    /// 客户端（Codex 预审 L2 的 M4）。
    client: Option<reqwest::Client>,
    _flight: Option<FlightGuard>,
}

/// **没有 key 的 `Debug`**（Codex 预审 L2 的 L1）：派生的实现会把 bearer 原样打进诊断、断言失败的消息里。
impl std::fmt::Debug for Authorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Authorization")
            .field("key", &self.key.as_ref().map(|_| "[REDACTED]"))
            .field("generation", &self.generation)
            .field("endpoint", &"[omitted]")
            .field("dial", &self.dial)
            .finish_non_exhaustive()
    }
}

/// 测试钩子：一个请求**建好之后、发出前最后一道核对（[`Authorization::ensure_current`]）之前**调用，让测试在这个确切的位置换掉引擎（Codex 复审 L2 的 M1'）。
/// 只在测试里存在；按请求 URL 的前缀匹配，进程里别的用例的请求不触发。
#[cfg(any(test, feature = "test-support"))]
pub mod test_hooks {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    type Hook = Arc<dyn Fn() + Send + Sync>;

    static HOOKS: Mutex<Vec<(u64, String, Hook)>> = Mutex::new(Vec::new());
    static NEXT: AtomicU64 = AtomicU64::new(1);

    /// 钩子活到它被丢弃为止。
    #[must_use = "the hook is removed when this is dropped"]
    pub struct HookGuard(u64);

    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOKS
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .retain(|(id, _, _)| *id != self.0);
        }
    }

    /// 请求的 URL 以 `url_prefix` 开头时，在它建好之后、发出前最后一道核对之前调 `hook`。
    pub fn between_build_and_send_for(
        url_prefix: &str,
        hook: impl Fn() + Send + Sync + 'static,
    ) -> HookGuard {
        let id = NEXT.fetch_add(1, Ordering::SeqCst);
        HOOKS.lock().unwrap_or_else(PoisonError::into_inner).push((
            id,
            url_prefix.to_owned(),
            Arc::new(hook),
        ));
        HookGuard(id)
    }

    /// 发送路径在建好请求之后、最后一眼之前调它（适配器的发送函数，以及路由里模型列表与就绪探测的发送；路由那两处只在它自己的单元测试里调）。
    pub fn between_build_and_send(url: &str) {
        let matching: Vec<Hook> = HOOKS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|(_, prefix, _)| url.starts_with(prefix.as_str()))
            .map(|(_, _, hook)| hook.clone())
            .collect();
        for hook in matching {
            hook();
        }
    }
}

/// 发往托管端点的、带凭证的请求用的 HTTP 客户端：**不保留空闲连接**（[`crate::engine::fresh_connection_client`]）。进程里只建一个，建成了才缓存：
/// 建失败的那一次不缓存（下一个授权再试），也不退到调用方给的客户端（Codex 预审 L2 的 M4）。
static FRESH_CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();

/// 测试：让下一批授权取不到托管客户端（造「客户端建不出来」）。只在测试里用。
#[cfg(any(test, feature = "test-support"))]
static FRESH_CLIENT_FAILS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// 测试：之后（直到再次调用 `false`）托管客户端建不出来。进程全局，所以只放在自己独占的测试二进制里用。
#[cfg(any(test, feature = "test-support"))]
pub fn make_the_managed_client_unbuildable(fails: bool) {
    FRESH_CLIENT_FAILS.store(fails, Ordering::SeqCst);
}

fn fresh_client() -> Result<reqwest::Client, KeyRefusal> {
    #[cfg(any(test, feature = "test-support"))]
    if FRESH_CLIENT_FAILS.load(Ordering::SeqCst) {
        return Err(KeyRefusal::TransportUnavailable);
    }
    if let Some(client) = FRESH_CLIENT.get() {
        return Ok(client.clone());
    }
    let built =
        crate::engine::fresh_connection_client().map_err(|_| KeyRefusal::TransportUnavailable)?;
    Ok(FRESH_CLIENT.get_or_init(|| built).clone())
}

impl Authorization {
    /// 没有登记的端点（非托管）、没有守卫：只是按 §2.9 的顺序取到的 key，没有在途登记。
    fn unguarded(
        key: Option<OsString>,
        generation: u64,
        endpoint: &str,
        dial: Option<std::net::SocketAddr>,
        client: Option<reqwest::Client>,
        flight: Option<FlightGuard>,
    ) -> Self {
        Self {
            key,
            generation,
            endpoint: endpoint.to_owned(),
            dial,
            client,
            _flight: flight,
        }
    }

    /// **发出前最后一刻**再核对一次（同步，不 await）：授权之后到真的发出之间，调用方还要做事（建请求、序列化请求体），端点可能已经被封上、换了代
    /// （重启越过了一个已经通过代际核对的请求：Codex 预审 L2 的 H3）。代际不是授权那一刻的、或端点被封上了，拒绝，什么都不发。调用方在
    /// `send()` 之前紧挨着调它。剩下的缝是从这一步到内核建立连接的几微秒，与「重新确认与发送之间」那个已知的间隙同级；而旧进程被停掉、新进程被拉起并绑上端口
    /// 要几百毫秒，所以这几微秒里不会有一个被越过的请求拨到替代者。
    pub fn ensure_current(&self) -> Result<(), KeyRefusal> {
        let now = snapshot_of(&self.endpoint);
        if now.generation != self.generation {
            return Err(KeyRefusal::StaleGeneration {
                admitted: self.generation,
                current: now.generation,
            });
        }
        if now.key == Some(EndpointKey::Blocked) {
            return Err(KeyRefusal::Blocked);
        }
        Ok(())
    }

    /// Invalidate only the guard for the generation whose connection failed.
    pub(crate) fn connection_failed(&self) {
        let Some(identity) = endpoint_identity(&self.endpoint) else {
            return;
        };
        let governed = GOVERNED.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = governed.get(&identity)
            && slot.generation == self.generation
            && let Some(guard) = &slot.guard
        {
            guard.invalidate_positive();
        }
    }

    /// 守卫核验过的拨号地址（托管的端点）。
    pub fn dial(&self) -> Option<std::net::SocketAddr> {
        self.dial
    }

    /// 这个请求**必须发往**的地址：托管的端点把 `url` 的主机与端口换成核验过的拨号地址（路径、查询不动），所以卡里写的 `localhost`、
    /// `[::1]` 之类的别名不会让请求落到守卫没验证过的那个监听者上（Codex 专项评审 H1）。非托管的端点原样返回。换不了（不是带主机的
    /// URL，登记表认的 http(s) URL 不会）就返回空串：任何 HTTP 客户端都发不出去，宁可不发。
    pub fn dial_url(&self, url: &str) -> String {
        let Some(dial) = self.dial else {
            return url.to_owned();
        };
        let Ok(mut parsed) = reqwest::Url::parse(url) else {
            return String::new();
        };
        if parsed.set_ip_host(dial.ip()).is_err() || parsed.set_port(Some(dial.port())).is_err() {
            return String::new();
        }
        parsed.into()
    }

    /// 发这个请求用的 HTTP 客户端：托管的端点一律用**不保留空闲连接**的客户端——带凭证的请求只走授权**之后**新开的连接，不会复用
    /// 更早（引擎换代之前、监听者换主人之前）留在池里的、通往旧进程的连接（Codex 专项评审 H6）。非托管的端点用调用方给的那个。
    pub fn client(&self, shared: &reqwest::Client) -> reqwest::Client {
        // 托管端点的专用客户端在授权的时候就取到了（取不到就没有授权）；这里不再有「建失败就换成共用的」那条路。
        self.client.clone().unwrap_or_else(|| shared.clone())
    }

    /// UTF-8 的 key（非 UTF-8 的环境变量当作没有，同 [`resolve_omlx_api_key_checked`]）。
    pub fn key(&self) -> Option<&str> {
        self.key.as_deref().and_then(OsStr::to_str)
    }

    /// 保留非 UTF-8 的 key，让调用方按「无效凭证」处理。
    pub fn key_os(&self) -> Option<&OsStr> {
        self.key.as_deref()
    }

    /// 授权那一刻这个端点的代际。
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

/// Invalidates the positive fallback cache of the authorization's generation when dropped while armed: a send cancelled from outside (the
/// caller's own deadline, the client going away) was stopped in a phase nobody knows, possibly the connect phase, which would have been
/// reported as a connection error had the transport been allowed to finish. Treated like one (conservative: the cost of a wrong guess is one
/// more listener check).
struct CancelledSend<'a> {
    authorization: Option<&'a Authorization>,
    armed: bool,
}

impl Drop for CancelledSend<'_> {
    fn drop(&mut self) {
        if self.armed
            && let Some(authorization) = self.authorization
        {
            authorization.connection_failed();
        }
    }
}

/// Observe a reqwest send before the caller can retry. A connection error invalidates
/// the corresponding generation's positive fallback cache; response and body errors do not.
/// A send that is dropped before it finished (an outer timeout, a cancelled request) invalidates it too
/// (Codex pre-review M3: a timeout in the connect phase must not leave a stale "yes").
pub async fn send_authorized(
    authorization: Option<&Authorization>,
    send: impl std::future::Future<Output = Result<reqwest::Response, reqwest::Error>>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut pending = CancelledSend {
        authorization,
        armed: true,
    };
    let result = send.await;
    pending.armed = false;
    result
        .inspect(|response| {
            // An upstream refused the credential: the key in the settings cache may be stale (a rotation the timestamps did not show).
            if response.status() == reqwest::StatusCode::UNAUTHORIZED {
                invalidate_settings_cache();
            }
        })
        .inspect_err(|error| {
            // A connection error, or a timeout before any response (it may have been in the connect phase, and reqwest reports a request
            // deadline as a plain timeout): the listener evidence is not trusted any more.
            if (error.is_connect() || error.is_timeout())
                && let Some(authorization) = authorization
            {
                authorization.connection_failed();
            }
        })
}

/// **每个要发往 oMLX 端点的请求在发出之前**调用（IDORIS-CORE S6，Codex 终审 H1；owner 2026-11-22 拍板）：在同一个任务里，紧挨着
/// 发送之前——
///
/// 1. 核对准入时的代际与封上（[`KeyRefusal::StaleGeneration`]、[`KeyRefusal::Blocked`]）；
/// 2. 登记一个在途的操作（[`FlightGuard`]），排空会等它；
/// 3. 让登记时交来的守卫**重新**确认监听者（[`EndpointGuard::verify`]）：不是了、读不出来都拒绝（[`KeyRefusal::ListenerChanged`]、
///    [`KeyRefusal::Unverified`]），什么都不发；
/// 4. 再读一次登记：校验期间口径被撤销或换了（代际变了），拒绝；
/// 5. 取 key。
///
/// 没有登记的端点（非托管的卡）没有守卫、没有代际，key 照 §2.9 的顺序取，行为与以前一样。
///
/// **key 在最后的核对之前取好**（Codex 预审 L2 的 H3）：外部引擎的 `settings.json` 是同步读文件，放在重读代际之后就是一段本来不必有的、
/// 发出前的延迟；现在监听者的重新确认与代际的重读是授权的最后两步。
pub async fn authorize_omlx_request(
    base_url: &str,
    admitted_generation: Option<u64>,
) -> Result<Authorization, KeyRefusal> {
    authorize(base_url, admitted_generation, true).await
}

/// 同 [`authorize_omlx_request`]，但**不取任何 key**（`Authorization::key` 恒为 `None`，也不读环境变量与 `settings.json`）。给那些发往被管理的端点、
/// 却不能带 oMLX key 的卡用（IDORIS-CORE S6，Codex 预审 L2 的 H1：同一个端口上的、`extensions.idoris_upstream` 的 resident 卡，IPv4 映射写法的卡）：
/// 它们的请求照样要过代际、封上、监听者的重新确认，照样发往核验过的拨号地址、登记在途，只是不带凭证。
pub async fn authorize_omlx_request_without_key(
    base_url: &str,
    admitted_generation: Option<u64>,
) -> Result<Authorization, KeyRefusal> {
    authorize(base_url, admitted_generation, false).await
}

async fn authorize(
    base_url: &str,
    admitted_generation: Option<u64>,
    with_key: bool,
) -> Result<Authorization, KeyRefusal> {
    let before = snapshot_of(base_url);
    refuse_stale_or_blocked(&before, admitted_generation)?;
    let guarded = matches!(
        before.key,
        Some(EndpointKey::Private(_) | EndpointKey::External { .. })
    );
    // 托管端点的专用客户端先取：取不到就是本地拒绝，之前没有登记任何在途（Codex 预审 L2 的 M4）。
    let client = if guarded { Some(fresh_client()?) } else { None };
    // 登记口径一旦是私有或外部，key 在这里按这一次读到的口径取好；校验之后口径若变了，下面的代际重读会拒绝，key 不会被用。
    let key = if with_key {
        resolve_from_snapshot_async(base_url, before.key.as_ref()).await?
    } else {
        None
    };
    // 先登记在途、后校验：此刻开始排空的管理器要么等到这个请求，要么在下面的重读里让它被拒绝，不会两头都漏掉。排空成功的那一刻
    // 在途登记已经封上（零与封上是同一个原子操作）：之后到达的请求在这里就被拒绝，不会在进程的宽限期里挤进来（Codex 预审 L3 的 M5）。
    let flight = match before.flights.as_ref() {
        Some(flights) => Some(FlightGuard::enter(flights).ok_or(KeyRefusal::Draining)?),
        None => None,
    };
    let mut dial = None;
    if guarded && let Some(guard) = &before.guard {
        match guard.verify().await {
            Verification::Verified => {}
            Verification::Mismatch => return Err(KeyRefusal::ListenerChanged),
            Verification::Unverifiable => return Err(KeyRefusal::Unverified),
        }
        let after = snapshot_of(base_url);
        if after.generation != before.generation {
            return Err(KeyRefusal::StaleGeneration {
                admitted: before.generation,
                current: after.generation,
            });
        }
        // 守卫验证的地址，就是发送端要拨的地址。
        dial = guard.dial();
    }
    // 没有守卫的托管端点（登记时没带守卫的口径，测试里有）没有核验过的拨号地址，也就不用专用客户端：照旧用调用方的。
    let client = client.filter(|_| dial.is_some());
    Ok(Authorization::unguarded(
        key,
        before.generation,
        base_url,
        dial,
        client,
        flight,
    ))
}

/// 纯解析：在 [`resolve_from_for`] 之上加「端点被引擎管理器接管」的口径（`governed`）。
/// 私有 key 一律优先；外部引擎的 `settings.json` key 只在校验过时给；没有登记就是原来的顺序。
pub fn resolve_for_endpoint(
    env: Option<OsString>,
    home: Option<&Path>,
    loopback: bool,
    governed: Option<&EndpointKey>,
) -> Option<OsString> {
    resolve_for_endpoint_checked(env, home, loopback, governed)
        .ok()
        .flatten()
}

fn resolve_for_endpoint_checked(
    env: Option<OsString>,
    home: Option<&Path>,
    loopback: bool,
    governed: Option<&EndpointKey>,
) -> Result<Option<OsString>, KeyRefusal> {
    match plan_for_endpoint(env, home, loopback, governed) {
        Plan::Done(key) => Ok(key),
        Plan::Settings(home) => settings_api_key(&home).map(|key| key.map(OsString::from)),
    }
}

/// What a resolution still needs: the answer, or a read of `settings.json` under this home. The decision is pure, so the sync and the async
/// callers share it and differ only in how they read the file.
enum Plan {
    Done(Option<OsString>),
    Settings(std::path::PathBuf),
}

fn plan_for_endpoint(
    env: Option<OsString>,
    home: Option<&Path>,
    loopback: bool,
    governed: Option<&EndpointKey>,
) -> Plan {
    match governed {
        Some(EndpointKey::Blocked) => return Plan::Done(None),
        Some(EndpointKey::Private(key)) => return Plan::Done(Some(OsString::from(key))),
        _ => {}
    }
    if let Some(key) = env.filter(|value| !value.is_empty()) {
        return Plan::Done(Some(key));
    }
    let settings_allowed = loopback
        && !matches!(
            governed,
            Some(EndpointKey::External {
                settings_key_verified: false
            })
        );
    match home.filter(|_| settings_allowed) {
        Some(home) => Plan::Settings(home.to_owned()),
        None => Plan::Done(None),
    }
}

/// 同上，但直接给 `loopback`，并保留非 UTF-8 的环境变量，让调用方按「无效凭证」处理。
/// `loopback` 为 false 时 `settings.json` 的 key 不会被返回。
pub fn resolve_omlx_api_key_os(loopback: bool) -> Option<OsString> {
    note_resolution();
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
    settings_api_key(home?)
        .ok()
        .flatten()
        .map(|key| (OsString::from(key), KeySource::Settings))
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

/// Who the settings file and the directories on its way must belong to (three uids: the file, the link and its directory, the link
/// target). Production passes the effective uid for all three; tests pass a different one to exercise each refusal without a second account.
#[cfg(unix)]
#[derive(Debug, Clone, Copy)]
struct Trust {
    file_uid: u32,
    /// The directory the link is in (`$HOME`) and the link itself.
    dir_uid: u32,
    /// The directory the link points at (`~/.omlx` after following it).
    target_uid: u32,
}

#[cfg(unix)]
impl Trust {
    fn current() -> Self {
        let uid = crate::engine::effective_uid();
        Self {
            file_uid: uid,
            dir_uid: uid,
            target_uid: uid,
        }
    }
}

/// A directory we accept as the place of a symlink or as its target: owned by `uid`, not writable by others, and without an ACL entry
/// that gives somebody else write access. Group write is tolerated (a user-private-group umask of 002 makes it common): replacing
/// `settings.json` there produces a file owned by someone else, which the file check refuses.
#[cfg(unix)]
fn trusted_directory(metadata: &fs::Metadata, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o002 == 0
}

/// Is the opened object free of an extended ACL entry that grants somebody other than its owner a write-like permission? macOS only, by
/// `acl_get_fd_np` (`idoris-sockprobe`; no subprocess). Where the platform cannot tell (Linux, or a failed call) the answer is yes: the
/// permission bits were already checked, and an ACL check that cannot run must not make a legitimate file unusable.
#[cfg(unix)]
fn acl_is_clean(file: &fs::File) -> bool {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;
    let Ok(metadata) = file.metadata() else {
        return true;
    };
    idoris_sockprobe::acl_grants_others_write(file.as_raw_fd(), metadata.uid()) != Some(true)
}

/// Pin the directory `~/.omlx` and open `settings.json` inside it. `~/.omlx` may be a symlink (dotfile managers, an encrypted volume):
/// the opened directory is what is verified and pinned, and the file is opened relative to it (`openat`), so a later swap of the path
/// cannot redirect the read. A symlink is accepted only when the directory it sits in and the directory it leads to are ours (owner,
/// not writable by others, no foreign ACL write grant) and the link itself is ours. `settings.json` itself is never followed.
#[cfg(unix)]
fn open_settings_file(home: &Path, trust: Trust) -> std::io::Result<fs::File> {
    use nix::errno::Errno;
    use nix::fcntl::{OFlag, open, openat};
    use nix::sys::stat::Mode;

    let denied = || std::io::Error::from(std::io::ErrorKind::PermissionDenied);
    let directory = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC;
    let home_fd = open(home, directory, Mode::empty())?;
    let link_path = home.join(".omlx");
    let base = match openat(
        &home_fd,
        ".omlx",
        directory | OFlag::O_NOFOLLOW,
        Mode::empty(),
    ) {
        Ok(base) => fs::File::from(base),
        Err(Errno::ELOOP | Errno::ENOTDIR) => {
            // Maybe a symlink: its location and the link must be ours before it is followed at all.
            let link = fs::symlink_metadata(&link_path)?;
            if !link.file_type().is_symlink() {
                return Err(Errno::ENOTDIR.into());
            }
            let place = fs::File::from(home_fd.try_clone()?).metadata()?;
            {
                use std::os::unix::fs::MetadataExt;
                if link.uid() != trust.dir_uid || !trusted_directory(&place, trust.dir_uid) {
                    return Err(denied());
                }
            }
            if !acl_is_clean(&fs::File::from(home_fd.try_clone()?))
                || idoris_sockprobe::acl_grants_others_write_at(
                    &link_path,
                    std::os::unix::fs::MetadataExt::uid(&link),
                ) == Some(true)
            {
                return Err(denied());
            }
            let base = fs::File::from(openat(&home_fd, ".omlx", directory, Mode::empty())?);
            if !trusted_directory(&base.metadata()?, trust.target_uid) || !acl_is_clean(&base) {
                return Err(denied());
            }
            base
        }
        Err(error) => return Err(error.into()),
    };
    let flags = OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC;
    let file = openat(&base, "settings.json", flags, Mode::empty()).map(fs::File::from)?;
    if !acl_is_clean(&file) {
        return Err(denied());
    }
    Ok(file)
}

#[cfg(not(unix))]
fn open_settings_file(home: &Path) -> std::io::Result<fs::File> {
    let path = home.join(".omlx/settings.json");
    if !fs::symlink_metadata(&path)?.is_file() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    let file = fs::File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::ErrorKind::PermissionDenied.into());
    }
    Ok(file)
}

#[cfg(unix)]
fn trusted_settings(metadata: &fs::Metadata, uid: u32) -> bool {
    use std::os::unix::fs::MetadataExt;
    metadata.is_file() && settings_owner_and_mode_ok(metadata.uid(), metadata.mode(), uid)
}

/// 属主须是 iDoris 的有效 UID，或 root（`sudo omlx …` 且保留 HOME 会留下 root 属主的文件，root 不构成攻击者信号）；
/// 无论谁拥有，组或其他用户可写都拒绝。
#[cfg(unix)]
fn settings_owner_and_mode_ok(owner: u32, mode: u32, euid: u32) -> bool {
    (owner == euid || owner == 0) && mode & 0o022 == 0
}

#[derive(Debug, PartialEq, Eq)]
enum SettingsError {
    Unreadable,
    InvalidJson,
}

/// The homes whose settings file was opened and parsed (tests count how often).
#[cfg(test)]
static FILE_READS: Mutex<Vec<std::path::PathBuf>> = Mutex::new(Vec::new());

fn read_settings_once(
    home: &Path,
    missing_is_absent: bool,
) -> Result<Option<String>, SettingsError> {
    #[cfg(unix)]
    return read_settings_once_with(home, missing_is_absent, Trust::current());
    #[cfg(not(unix))]
    read_settings_once_with(home, missing_is_absent)
}

fn read_settings_once_with(
    home: &Path,
    missing_is_absent: bool,
    #[cfg(unix)] trust: Trust,
) -> Result<Option<String>, SettingsError> {
    #[cfg(unix)]
    return read_settings_stamped(home, missing_is_absent, trust).0;
    #[cfg(not(unix))]
    read_settings_stamped(home, missing_is_absent).0
}

/// [`read_settings_once_with`], and the state of the **opened** file after it was read (`fstat` of the descriptor: what was read is what
/// this describes), for the cache to compare with what the path said before and after.
fn read_settings_stamped(
    home: &Path,
    missing_is_absent: bool,
    #[cfg(unix)] trust: Trust,
) -> (Result<Option<String>, SettingsError>, Option<FileStamp>) {
    let mut opened_stamp = None;
    let result = read_settings_inner(
        home,
        missing_is_absent,
        #[cfg(unix)]
        trust,
        &mut opened_stamp,
    );
    (result, opened_stamp)
}

fn read_settings_inner(
    home: &Path,
    missing_is_absent: bool,
    #[cfg(unix)] trust: Trust,
    opened_stamp: &mut Option<FileStamp>,
) -> Result<Option<String>, SettingsError> {
    #[cfg(test)]
    FILE_READS
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(home.to_owned());
    #[cfg(unix)]
    let opened = open_settings_file(home, trust);
    #[cfg(not(unix))]
    let opened = open_settings_file(home);
    let file = match opened {
        Ok(file) => file,
        Err(error) if missing_is_absent && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None);
        }
        Err(_) => return Err(SettingsError::Unreadable),
    };
    #[cfg(unix)]
    if !trusted_settings(
        &file.metadata().map_err(|_| SettingsError::Unreadable)?,
        trust.file_uid,
    ) {
        return Err(SettingsError::Unreadable);
    }
    let mut raw = Vec::new();
    #[cfg(unix)]
    let file_after_read = file.try_clone().map_err(|_| SettingsError::Unreadable)?;
    file.take(SETTINGS_MAX_BYTES + 1)
        .read_to_end(&mut raw)
        .map_err(|_| SettingsError::Unreadable)?;
    if raw.len() as u64 > SETTINGS_MAX_BYTES {
        return Err(SettingsError::Unreadable);
    }
    #[cfg(unix)]
    {
        *opened_stamp = file_after_read.metadata().ok().map(|m| FileStamp::of(&m));
    }
    let value: serde_json::Value =
        serde_json::from_slice(&raw).map_err(|_| SettingsError::InvalidJson)?;
    Ok(value
        .get("auth")
        .and_then(|auth| auth.get("api_key"))
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|key| !key.is_empty() && !key.chars().any(|c| c.is_control() || c.is_whitespace()))
        .map(str::to_owned))
}

fn read_settings_key(home: &Path) -> Result<Option<String>, SettingsError> {
    retry_invalid_settings(home, || {
        std::thread::sleep(std::time::Duration::from_millis(10))
    })
}

fn retry_invalid_settings(
    home: &Path,
    pause: impl FnOnce(),
) -> Result<Option<String>, SettingsError> {
    match read_settings_once(home, true) {
        Err(SettingsError::InvalidJson) => {
            pause();
            // The first read established that a file existed. If it vanishes during
            // the retry, the configuration is still unsettled for this request.
            read_settings_once(home, false)
        }
        result => result,
    }
}

/// What identifies the state of the settings file and of the path leading to it, from a handful of `stat`s: device, inode, mode, owner,
/// size, modification and change time (nanoseconds) of the home directory, of `.omlx` itself (a symlink is not followed) and of what it
/// leads to, and of `settings.json` itself (not followed either). An in-place rewrite changes modification and change time, a `chmod`,
/// an ACL edit or a swapped directory changes the change time or the identity: any difference is a miss.
#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct SettingsStamp([FileStamp; 4]);

#[cfg(unix)]
#[derive(Debug, Clone, PartialEq, Eq)]
struct FileStamp {
    dev: u64,
    ino: u64,
    mode: u32,
    uid: u32,
    size: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
}

#[cfg(unix)]
impl FileStamp {
    fn of(metadata: &fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        Self {
            dev: metadata.dev(),
            ino: metadata.ino(),
            mode: metadata.mode(),
            uid: metadata.uid(),
            size: metadata.size(),
            mtime: (metadata.mtime(), metadata.mtime_nsec()),
            ctime: (metadata.ctime(), metadata.ctime_nsec()),
        }
    }
}

#[cfg(unix)]
fn settings_stamp(home: &Path) -> Option<SettingsStamp> {
    let base = home.join(".omlx");
    Some(SettingsStamp([
        FileStamp::of(&fs::metadata(home).ok()?),
        FileStamp::of(&fs::symlink_metadata(&base).ok()?),
        FileStamp::of(&fs::metadata(&base).ok()?),
        FileStamp::of(&fs::symlink_metadata(base.join("settings.json")).ok()?),
    ]))
}

/// How long a cached parse may be reused at most, whatever the stamps say: a file system with coarse timestamps can leave every stamp field
/// equal across an in-place rewrite of the same length within one tick, so a rotated key is picked up within this bound at the latest.
#[cfg(unix)]
const SETTINGS_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(1);

#[cfg(unix)]
struct CachedSettings {
    home: std::path::PathBuf,
    stamp: SettingsStamp,
    key: Option<String>,
    at: std::time::Instant,
}

/// The parsed result of the last successful read per home, valid while the [`SettingsStamp`] is unchanged **and** younger than
/// [`SETTINGS_CACHE_TTL`]: the request path asks the file system only a few `stat`s instead of opening, checking and parsing the file every
/// time. A change (which includes a rewrite in place, on a file system with fine timestamps) is seen by the very next request; an upstream
/// 401 drops the cache at once ([`invalidate_settings_cache`]). Failures are never cached.
#[cfg(unix)]
static SETTINGS_CACHE: Mutex<Vec<CachedSettings>> = Mutex::new(Vec::new());

/// How many times the cache was dropped on purpose (tests bound the reads they expect by it).
#[cfg(unix)]
static SETTINGS_INVALIDATIONS: AtomicUsize = AtomicUsize::new(0);

/// Forget every cached `settings.json` parse: an upstream answered 401 to a request that carried (or should have carried) a key from there,
/// so the next request reads the file again whatever the timestamps say.
pub(crate) fn invalidate_settings_cache() {
    #[cfg(unix)]
    {
        SETTINGS_INVALIDATIONS.fetch_add(1, Ordering::SeqCst);
        SETTINGS_CACHE
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }
}

#[cfg(unix)]
fn cached_settings(home: &Path, stamp: &SettingsStamp) -> Option<Option<String>> {
    SETTINGS_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .find(|entry| {
            entry.home == home && &entry.stamp == stamp && entry.at.elapsed() < SETTINGS_CACHE_TTL
        })
        .map(|entry| entry.key.clone())
}

/// May the result of a read be cached? Only when the file that was **opened and read** is the file the path named before the read and still
/// names after it: the descriptor's state after the read equals the path's `settings.json` stamp before and after, and the whole stamp
/// (directories included) did not move.
#[cfg(unix)]
fn cacheable(
    before: &SettingsStamp,
    opened: Option<&FileStamp>,
    after: Option<&SettingsStamp>,
) -> bool {
    match (opened, after) {
        (Some(opened), Some(after)) => before == after && *opened == before.0[3],
        _ => false,
    }
}

#[cfg(unix)]
fn remember_settings(home: &Path, stamp: SettingsStamp, key: &Option<String>) {
    let mut cache = SETTINGS_CACHE
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    cache.retain(|entry| entry.home != home);
    if cache.len() >= 16 {
        cache.clear();
    }
    cache.push(CachedSettings {
        home: home.to_owned(),
        stamp,
        key: key.clone(),
        at: std::time::Instant::now(),
    });
}

/// The async twin of [`read_settings_key`] for the request path: an unchanged, fresh file (a few `stat`s, no blocking pool) answers from the
/// cache; otherwise every file operation runs on the blocking pool and the retry pause is a timer, so no runtime worker is held while a
/// truncated snapshot settles.
async fn read_settings_key_async(home: &Path) -> Result<Option<String>, SettingsError> {
    // The stamp is taken **before** the read: a change after it makes the next request's stamp differ, never the other way round.
    #[cfg(unix)]
    let stamp = settings_stamp(home);
    #[cfg(unix)]
    if let Some(stamp) = &stamp
        && let Some(hit) = cached_settings(home, stamp)
    {
        return Ok(hit);
    }
    let first = home.to_owned();
    let (result, opened) = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        return read_settings_stamped(&first, true, Trust::current());
        #[cfg(not(unix))]
        (read_settings_once(&first, true), None::<()>)
    })
    .await
    .map_err(|_| SettingsError::Unreadable)?;
    #[cfg(unix)]
    let mut opened = opened;
    let result = match result {
        Err(SettingsError::InvalidJson) => {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            let again = home.to_owned();
            let (result, again_opened) = tokio::task::spawn_blocking(move || {
                #[cfg(unix)]
                return read_settings_stamped(&again, false, Trust::current());
                #[cfg(not(unix))]
                (read_settings_once(&again, false), None::<()>)
            })
            .await
            .map_err(|_| SettingsError::Unreadable)?;
            #[cfg(unix)]
            {
                opened = again_opened;
            }
            let _ = &opened;
            result
        }
        result => result,
    };
    #[cfg(unix)]
    if let (Some(stamp), Ok(key)) = (stamp, &result)
        && cacheable(&stamp, opened.as_ref(), settings_stamp(home).as_ref())
    {
        remember_settings(home, stamp, key);
    }
    result
}

/// 读取 `<home>/.omlx/settings.json` 的 `auth.api_key`。文件缺失或合法 JSON 中没有可用的 key 时返回 `Ok(None)`。
pub fn settings_api_key(home: &Path) -> Result<Option<String>, KeyRefusal> {
    read_settings_key(home).map_err(|_| KeyRefusal::SettingsUnverifiable)
}

#[cfg(all(test, unix))]
mod settings_tests {
    #![allow(clippy::unwrap_used)]

    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::*;

    #[test]
    fn settings_owner_and_mode_predicate() {
        // euid 501: own file and root-owned file pass, other users' files do not.
        assert!(settings_owner_and_mode_ok(501, 0o100600, 501));
        assert!(settings_owner_and_mode_ok(0, 0o100644, 501));
        assert!(!settings_owner_and_mode_ok(502, 0o100600, 501));
        // group or other write is refused whoever owns the file.
        assert!(!settings_owner_and_mode_ok(501, 0o100620, 501));
        assert!(!settings_owner_and_mode_ok(0, 0o100602, 501));
        assert!(!settings_owner_and_mode_ok(0, 0o100666, 501));
    }

    #[test]
    fn settings_rejects_foreign_ownership() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let metadata = file.as_file().metadata().unwrap();
        assert!(!trusted_settings(&metadata, metadata.uid().wrapping_add(1)));
    }

    #[test]
    fn settings_rejects_group_or_world_write_bits() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".omlx");
        fs::create_dir(&base).unwrap();
        let path = base.join("settings.json");
        fs::write(&path, r#"{"auth":{"api_key":"key"}}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_settings_key(home.path()).unwrap().as_deref(),
            Some("key")
        );
        for mode in [0o620, 0o602] {
            fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
            assert_eq!(
                read_settings_key(home.path()),
                Err(SettingsError::Unreadable)
            );
        }
    }

    #[test]
    fn invalid_json_is_retried_once_then_reported_as_unverified() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".omlx");
        fs::create_dir(&base).unwrap();
        let path = base.join("settings.json");
        fs::write(&path, "{").unwrap();
        let result = retry_invalid_settings(home.path(), || {
            fs::write(&path, r#"{"auth":{"api_key":"finished"}}"#).unwrap();
        });
        assert_eq!(result.unwrap().as_deref(), Some("finished"));

        fs::write(&path, "{").unwrap();
        let mut pauses = 0;
        let result = retry_invalid_settings(home.path(), || pauses += 1);
        assert_eq!(pauses, 1);
        assert_eq!(result, Err(SettingsError::InvalidJson));
        assert_eq!(
            resolve_for_endpoint_checked(
                None,
                Some(home.path()),
                true,
                Some(&EndpointKey::External {
                    settings_key_verified: true
                })
            ),
            Err(KeyRefusal::SettingsUnverifiable)
        );
        fs::write(&path, "{}").unwrap();
        assert_eq!(read_settings_key(home.path()), Ok(None));
    }

    fn write_settings(base: &Path) -> std::path::PathBuf {
        fs::create_dir_all(base).unwrap();
        let path = base.join("settings.json");
        fs::write(&path, r#"{"auth":{"api_key":"k-link"}}"#).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        path
    }

    fn symlinked_home() -> (tempfile::TempDir, std::path::PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let target = home.path().join("dotfiles-omlx");
        write_settings(&target);
        std::os::unix::fs::symlink(&target, home.path().join(".omlx")).unwrap();
        (home, target)
    }

    #[test]
    fn a_symlinked_omlx_directory_of_ours_is_accepted() {
        let (home, _target) = symlinked_home();
        assert_eq!(
            read_settings_key(home.path()).unwrap().as_deref(),
            Some("k-link")
        );
    }

    #[test]
    fn a_symlinked_omlx_directory_owned_by_someone_else_is_refused() {
        let (home, _target) = symlinked_home();
        let uid = crate::engine::effective_uid();
        let foreign_dirs = Trust {
            file_uid: uid,
            dir_uid: uid.wrapping_add(1),
            target_uid: uid,
        };
        assert_eq!(
            read_settings_once_with(home.path(), true, foreign_dirs),
            Err(SettingsError::Unreadable)
        );
        let ours = Trust {
            file_uid: uid,
            dir_uid: uid,
            target_uid: uid,
        };
        assert_eq!(
            read_settings_once_with(home.path(), true, ours)
                .unwrap()
                .as_deref(),
            Some("k-link")
        );
    }

    #[test]
    fn a_symlink_of_ours_to_a_directory_owned_by_someone_else_is_refused() {
        // The link and the directory it is in are ours (so the link-owner check passes); only the target directory belongs to someone else.
        let (home, _target) = symlinked_home();
        let uid = crate::engine::effective_uid();
        let foreign_target = Trust {
            file_uid: uid,
            dir_uid: uid,
            target_uid: uid.wrapping_add(1),
        };
        assert_eq!(
            read_settings_once_with(home.path(), true, foreign_target),
            Err(SettingsError::Unreadable)
        );
    }

    #[test]
    fn a_symlinked_target_writable_by_others_is_refused() {
        let (home, target) = symlinked_home();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            read_settings_key(home.path()),
            Err(SettingsError::Unreadable)
        );
    }

    #[test]
    fn the_settings_file_itself_is_never_followed() {
        let home = tempfile::tempdir().unwrap();
        let base = home.path().join(".omlx");
        let real = write_settings(&home.path().join("elsewhere"));
        fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(real, base.join("settings.json")).unwrap();
        assert_eq!(
            read_settings_key(home.path()),
            Err(SettingsError::Unreadable)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_foreign_acl_write_grant_on_the_settings_file_is_refused() {
        let home = tempfile::tempdir().unwrap();
        let path = write_settings(&home.path().join(".omlx"));
        // control: no ACL
        assert_eq!(
            read_settings_key(home.path()).unwrap().as_deref(),
            Some("k-link")
        );
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow write"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            read_settings_key(home.path()),
            Err(SettingsError::Unreadable)
        );
        // the verdict follows the ACL: removing it restores acceptance (the cache is keyed by change time)
        let status = std::process::Command::new("/bin/chmod")
            .args(["-N"])
            .arg(&path)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            read_settings_key(home.path()).unwrap().as_deref(),
            Some("k-link")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_foreign_acl_write_grant_on_a_symlink_target_directory_is_refused() {
        let (home, target) = symlinked_home();
        assert!(read_settings_key(home.path()).is_ok());
        let status = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file"])
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            read_settings_key(home.path()),
            Err(SettingsError::Unreadable)
        );
    }

    /// Codex re-review H2: an inherit-only write entry on the directory `~/.omlx` leads to is a template for what gets created inside it; it
    /// gives nobody access to the directory or to the existing, protected `settings.json`, and must not make the engine unusable.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_inherit_only_acl_entry_on_the_target_directory_is_accepted() {
        let (home, target) = symlinked_home();
        let status = std::process::Command::new("/bin/chmod")
            .args([
                "+a",
                "everyone allow add_file,add_subdirectory,file_inherit,directory_inherit,only_inherit",
            ])
            .arg(&target)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            read_settings_key(home.path()).unwrap().as_deref(),
            Some("k-link")
        );
    }

    fn reads_of(home: &Path) -> usize {
        FILE_READS
            .lock()
            .unwrap()
            .iter()
            .filter(|read| read.as_path() == home)
            .count()
    }

    /// An unchanged settings file is read and parsed once; the request path then only `stat`s. Every kind of change is seen by the very next
    /// request: an in-place rewrite (same size), a different size, a `chmod` that makes the file group-writable (now refused), and the
    /// file's disappearance.
    #[tokio::test]
    async fn an_unchanged_settings_file_is_read_once_and_every_change_is_seen_at_once() {
        use std::os::unix::fs::PermissionsExt;
        let home = tempfile::tempdir().unwrap();
        let path = write_settings(&home.path().join(".omlx"));
        let invalidations = SETTINGS_INVALIDATIONS.load(Ordering::SeqCst);
        let first = read_settings_key_async(home.path()).await.unwrap();
        assert_eq!(first.as_deref(), Some("k-link"));
        for _ in 0..50 {
            assert_eq!(
                read_settings_key_async(home.path()).await,
                Ok(first.clone())
            );
        }
        assert!(
            reads_of(home.path())
                <= 1 + (SETTINGS_INVALIDATIONS.load(Ordering::SeqCst) - invalidations),
            "fifty more requests read nothing"
        );

        // rewritten in place with the same length
        fs::write(&path, r#"{"auth":{"api_key":"k-lonk"}}"#).unwrap();
        assert_eq!(
            read_settings_key_async(home.path())
                .await
                .unwrap()
                .as_deref(),
            Some("k-lonk")
        );
        // a different size
        fs::write(&path, r#"{"auth":{"api_key":"k-much-longer"}}"#).unwrap();
        assert_eq!(
            read_settings_key_async(home.path())
                .await
                .unwrap()
                .as_deref(),
            Some("k-much-longer")
        );
        // permissions change: the cached verdict must not outlive the trust it was based on
        fs::set_permissions(&path, fs::Permissions::from_mode(0o664)).unwrap();
        assert_eq!(
            read_settings_key_async(home.path()).await,
            Err(SettingsError::Unreadable)
        );
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            read_settings_key_async(home.path())
                .await
                .unwrap()
                .as_deref(),
            Some("k-much-longer")
        );
        // a completed rename-replace (a new inode, as editors and dotfile managers write it)
        let staged = home.path().join("staged.json");
        fs::write(&staged, r#"{"auth":{"api_key":"k-renamed"}}"#).unwrap();
        fs::set_permissions(&staged, fs::Permissions::from_mode(0o600)).unwrap();
        fs::rename(&staged, &path).unwrap();
        assert_eq!(
            read_settings_key_async(home.path())
                .await
                .unwrap()
                .as_deref(),
            Some("k-renamed")
        );
        // the file replaced by a symlink to an identical file is not followed, cache or not
        let copy = home.path().join("copy.json");
        fs::copy(&path, &copy).unwrap();
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(&copy, &path).unwrap();
        assert_eq!(
            read_settings_key_async(home.path()).await,
            Err(SettingsError::Unreadable)
        );
        fs::remove_file(&path).unwrap();
        assert_eq!(read_settings_key_async(home.path()).await, Ok(None));
    }

    /// The cache has two more bounds than the timestamps: a cached parse is dropped when an upstream answers 401 (a rotation the timestamps
    /// did not show), and it is never older than [`SETTINGS_CACHE_TTL`].
    #[tokio::test]
    async fn a_401_or_the_ttl_drops_the_cached_settings() {
        let home = tempfile::tempdir().unwrap();
        write_settings(&home.path().join(".omlx"));
        let reads = || reads_of(home.path());
        read_settings_key_async(home.path()).await.unwrap();
        read_settings_key_async(home.path()).await.unwrap();
        let steady = reads();

        // a 401 seen by `send_authorized`
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::any())
            .respond_with(wiremock::ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let client = reqwest::Client::new();
        let response = send_authorized(None, client.get(server.uri()).send())
            .await
            .unwrap();
        assert_eq!(response.status(), 401);
        read_settings_key_async(home.path()).await.unwrap();
        assert!(
            reads() > steady,
            "a 401 must make the next request read the file"
        );

        // the TTL, with the stamps unchanged
        let steady = reads();
        read_settings_key_async(home.path()).await.unwrap();
        tokio::time::sleep(SETTINGS_CACHE_TTL + std::time::Duration::from_millis(100)).await;
        let before = reads();
        read_settings_key_async(home.path()).await.unwrap();
        assert!(
            reads() > before,
            "a parse older than the TTL is read again ({steady} -> {before} -> {})",
            reads()
        );
    }

    /// What is cached must be what was read: the descriptor's state after the read has to be the state the path named before **and** after.
    #[test]
    fn a_parse_is_cacheable_only_if_the_opened_file_is_the_one_the_path_named() {
        let home = tempfile::tempdir().unwrap();
        let path = write_settings(&home.path().join(".omlx"));
        let before = settings_stamp(home.path()).unwrap();
        let opened = FileStamp::of(&fs::metadata(&path).unwrap());
        assert!(cacheable(&before, Some(&opened), Some(&before)));
        // another file was opened than the one the path named before
        let other = FileStamp {
            ino: opened.ino + 1,
            ..opened.clone()
        };
        assert!(!cacheable(&before, Some(&other), Some(&before)));
        // the opened file changed while it was read (its state moved)
        let moved = FileStamp {
            size: opened.size + 1,
            ..opened.clone()
        };
        assert!(!cacheable(&before, Some(&moved), Some(&before)));
        // the path now names something else
        fs::write(&path, r#"{"auth":{"api_key":"k-other-longer"}}"#).unwrap();
        let after = settings_stamp(home.path()).unwrap();
        assert!(!cacheable(&before, Some(&opened), Some(&after)));
        // nothing was opened (a failed read), nothing to cache
        assert!(!cacheable(&before, None, Some(&before)));
    }

    /// With the clock paused, a retry pause that is a runtime timer advances the virtual clock by exactly its length, while a thread
    /// sleep (which holds a runtime worker) leaves it where it was.
    #[tokio::test(start_paused = true)]
    async fn the_async_retry_pause_is_a_timer_not_a_blocked_worker() {
        let home = tempfile::tempdir().unwrap();
        let path = write_settings(&home.path().join(".omlx"));
        fs::write(&path, "{").unwrap();
        let before = tokio::time::Instant::now();
        assert_eq!(
            read_settings_key_async(home.path()).await,
            Err(SettingsError::InvalidJson)
        );
        assert!(
            before.elapsed() >= std::time::Duration::from_millis(10),
            "the pause did not go through the runtime timer"
        );
        fs::write(&path, r#"{"auth":{"api_key":"settled"}}"#).unwrap();
        assert_eq!(
            read_settings_key_async(home.path())
                .await
                .unwrap()
                .as_deref(),
            Some("settled")
        );
    }
}
