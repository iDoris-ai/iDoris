//! `idoris setup <engine>` 的通用流程（IDORIS-CORE S4；接口规范 §2.7.8、§2.12.1）。
//!
//! 一个 `setup` 子命令做的事：确定引擎从哪里来（采纳端点上已有的引擎、自动探测、或交给 iDoris 托管），
//! 核对 `--bind`，报告可执行文件的情况，生成组件卡，用服务启动时同一个 loader 校验之后才落盘。
//! 这条流程不认识任何具体的引擎：引擎相关的全部由 [`SetupEngine`] 给出（种类、去哪里找可执行文件、
//! 候选端口、怎么探测、卡的模板、模型目录），第一个实现是 `setup_omlx::OmlxSetup`。加一种引擎的
//! `setup <engine_kind>`（规范 §2.12.1）是写一个新的 [`SetupEngine`] 实现，这里的流程不改；
//! `tests/setup_engine_generic.rs` 用一个与 oMLX 毫无关系的假引擎走了一遍。
//!
//! # 三种目标（[`SetupTarget`]）
//!
//! - **采纳**（`--endpoint <url>`，`--auto` 找到了）：卡不带 `idoris_managed`，输出 `mode: "adopt"`。
//!   探测过的才知道鉴权与模型列表；`--no-probe` 不连引擎，`--bind` 改为对照本机已装的模型。
//! - **托管**（`--managed`，或 `--auto --managed` 没找到时）：卡带 `extensions.idoris_managed`，输出 `mode: "managed"`。
//!   端点取 `--managed-port`，缺省在私有端口范围里挑一个此刻空闲的。托管模式不连引擎（引擎还没起），
//!   必须找到可执行文件（[`SetupEngine::discover_executable`]，含签名校验），找不到是 `engine_not_installed`。
//!   这里只生成卡片：拉起引擎、准备引擎的 base 目录是后面的引擎管理器的事（S5）。
//! - `--auto --managed` 找到了外部引擎时取保守的一边：采纳它，不托管（规范 §2.7.8）。
//! - `--auto` 的探测先发匿名请求：候选端口是猜的，别的进程可能占着它。带 `--api-key-stdin` 时（#619 落地之后），候选要求
//!   API key 的才继续：先确认现在监听这个端口的就是这种引擎（[`SetupEngine::verify_listener`]：同一个用户、签名团队对），
//!   通过了才用新建的连接带 key 再探一次；没通过就**不发 key**，当作「要 key 而没法给」。没有给 key，或没通过校验，
//!   而没有别的候选合格时，报 `auth_required`，错误 JSON 带 `endpoint`（那个候选的地址），调用方确认它是这种引擎之后改用
//!   `--endpoint <该地址> --api-key-stdin`（那是调用方明确指定的端点，不校验属主）；带 `--managed` 也一样，不在一个可能
//!   是同一种引擎的服务旁边再起一个。
//! - 托管引擎的模型目录是 [`SetupEngine::managed_models_dir`]（S5 拉起时显式传 `--model-dir`），离线核对 `--bind`
//!   读的也是它。
//!
//! # `--out-dir` 的两种用法（[`OutDirUse`]）
//!
//! - **整个目录由这次 setup 生成**（`setup omlx`，缺省）：卡加引擎的固定文件，目录要是空的；`--force` 只允许整体替换
//!   这些文件。
//! - **往已有的组件目录里加一张卡**（`setup upstream`）：目录可以不存在（创建），也可以已经有别的卡。写的只有
//!   [`SetupEngine::card_file`] 这一个文件，别的文件原样不动；同名卡已经在时要 `--force` 才替换，替换的也只是它。
//!   落盘之前把「现有的卡加新卡」放进暂存目录，用服务启动时同一个加载器校验整个目录：`provider.id` 与别的文件重复是
//!   `id_conflict`，现有的卡本身加载不了或别的校验不过是 `validation_failed`，两种情况下 `--out-dir` 都不变。新卡要当默认
//!   （`idoris_default`）时，目录里不能已经有别的默认卡。
//!
//! # 副作用
//!
//! 写入的位置是 `--out-dir` 与它同级的 `.idoris-setup-*` 暂存目录（规范 §2.7「组件配置」行）；状态目录、引擎的
//! base 目录都不创建。探测连 loopback；找端口、核对已装模型、查可执行文件都是本机的操作（文件系统、loopback 套接字，
//! 托管模式还会运行 codesign 与引擎自己的 `--version`），不需要网络。
//!
//! # 输出
//!
//! 既有字段不变（`status`、`endpoint`、`auth`、`models`、`bindings`、`components_dir`、`next`），新增 `mode` 与
//! `engine`（`{engine_kind, installed, executable, version?}`）。没有探测过的输出，`auth` 是 `"unknown"`、`models` 是空数组。
//! `engine.installed` 在托管模式是「找到并通过了校验」（含签名）；在采纳模式只看位置上有没有一个通过文件检查的可执行文件，
//! 不校验签名、不执行它（所以没有 `version`）：采纳不需要拉起它。

use std::collections::BTreeSet;
use std::future::Future;
use std::io::{BufRead, Read};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};
use std::time::Duration;

use idoris_contracts::ComponentCard;
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode};
use serde_json::{Map, Value, json};

use crate::cli::{ManagedOptions, SetupOptions, SetupTarget};
use crate::engine_kind::engine_identity;

/// 托管引擎的私有端口范围（规范 §2.7.8）：不占 oMLX 官方默认的 8000，避免用户之后启动 oMLX.app 时端口冲突。
pub const MANAGED_PORT_RANGE: RangeInclusive<u16> = 19000..=19999;
/// 取版本号的时限（[`recipe_backed::discover`]）；超时只是少一个 `engine.version`，不是错误。
pub const VERSION_TIMEOUT: Duration = Duration::from_secs(10);
/// 判断端口有没有人在听时的连接时限（loopback，正常是立刻拒绝）。
const PORT_CONNECT_TIMEOUT: Duration = Duration::from_millis(200);
const MAX_KEY_BYTES: u64 = 4096;

#[derive(Debug)]
pub struct SetupError {
    pub code: &'static str,
    pub message: String,
    /// 与错误相关的端点：`--auto` 遇到要 API key 的候选时是那个候选的地址，调用方据此改用 `--endpoint`。
    pub endpoint: Option<String>,
}

impl SetupError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            endpoint: None,
        }
    }

    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    pub fn to_json(&self) -> Value {
        let mut value = json!({"status": "error", "code": self.code, "message": self.message});
        if let Some(endpoint) = &self.endpoint {
            value["endpoint"] = json!(endpoint);
        }
        value
    }
}

/// 探测请求的总时限。
pub(crate) const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
/// 探测响应体的上限；超过就当作不是这种引擎。
pub(crate) const MAX_BODY_BYTES: usize = 2 * 1024 * 1024;

/// 探测响应的状态码要求。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Accept {
    /// 任何 2xx（`setup omlx` 的探测，与 S4 一致）。
    AnySuccess,
    /// 恰好 200（`setup upstream` 的探测，规范 §2.7.8 写的是「要求 200」）。
    Ok200,
}

impl Accept {
    fn allows(self, status: reqwest::StatusCode) -> bool {
        match self {
            Self::AnySuccess => status.is_success(),
            Self::Ok200 => status == reqwest::StatusCode::OK,
        }
    }
}

/// 一次探测请求的回答：JSON，或者对端要求鉴权（401 / 403）。
pub(crate) enum Reply {
    Ok(Value),
    Denied,
}

/// 探测用的 HTTP 客户端：不走代理，不跟随重定向（探测的对象是 `--endpoint` 本身，不是它指到的别处），有总时限。
/// 各引擎的 [`SetupEngine::probe`] 共用它，边界只在这里定义一次。
pub(crate) fn probe_client() -> Result<reqwest::Client, SetupError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(PROBE_TIMEOUT)
        .build()
        .map_err(|_| SetupError::new("unreachable", "无法创建 HTTP 客户端"))
}

/// `GET <url>`，`key` 有值就带 Bearer。401 / 403 是 [`Reply::Denied`]；连不上是 `unreachable`（说明里带
/// `engine_name`）；状态码不合 `accept`（401 / 403 除外）、响应体超过 [`MAX_BODY_BYTES`]、不是 JSON，都是
/// `not_engine()` 给的错误。
pub(crate) async fn get_json(
    client: &reqwest::Client,
    url: &str,
    key: Option<&str>,
    engine_name: &str,
    accept: Accept,
    not_engine: impl Fn() -> SetupError,
) -> Result<Reply, SetupError> {
    let unreachable = || SetupError::new("unreachable", format!("无法连接 {engine_name} endpoint"));
    let mut request = client.get(url);
    if let Some(key) = key {
        request = request.bearer_auth(key);
    }
    let mut response = request.send().await.map_err(|_| unreachable())?;
    let status = response.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Ok(Reply::Denied);
    }
    if !accept.allows(status) {
        return Err(not_engine());
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unreachable())? {
        body.extend_from_slice(&chunk);
        if body.len() > MAX_BODY_BYTES {
            return Err(not_engine());
        }
    }
    serde_json::from_slice(&body)
        .map(Reply::Ok)
        .map_err(|_| not_engine())
}

/// Reads one bounded line (the API key). The key is never part of an error.
pub fn read_api_key(reader: &mut impl Read) -> Result<String, SetupError> {
    let bad = || {
        SetupError::new(
            "invalid_api_key_input",
            "stdin 必须提供单行、非空、无空白的 API key",
        )
    };
    let mut line = String::new();
    std::io::BufReader::new(reader.take(MAX_KEY_BYTES + 1))
        .read_line(&mut line)
        .map_err(|_| bad())?;
    let key = line.trim_end_matches(['\n', '\r']);
    if key.is_empty()
        || line.len() as u64 > MAX_KEY_BYTES
        || key.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(bad());
    }
    Ok(key.to_string())
}

const LOOPBACK_HOSTS: [&str; 3] = ["127.0.0.1", "[::1]", "localhost"];

pub fn normalize_endpoint(raw: &str) -> Result<String, SetupError> {
    let bad = |why: &str| SetupError::new("invalid_endpoint", format!("endpoint 无效：{why}"));
    let url = reqwest::Url::parse(raw).map_err(|_| bad("无法解析"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad("仅支持 http/https"));
    }
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(bad("不得包含凭据、路径、查询或片段"));
    }
    if !url.host_str().is_some_and(|h| LOOPBACK_HOSTS.contains(&h)) {
        return Err(bad("必须是 loopback（127.0.0.1 / [::1] / localhost）"));
    }
    Ok(url.as_str().trim_end_matches('/').to_string())
}

/// 一次成功的探测：引擎的模型列表，以及它是否要求 API key。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Probed {
    pub models: Vec<String>,
    pub auth_required: bool,
}

/// 托管模式找到的可执行文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundExecutable {
    pub path: PathBuf,
    /// 取不到（命令失败、超时）时是 `None`，不是错误。
    pub version: Option<String>,
}

/// 托管模式没找到可用的可执行文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FindFailure {
    /// 一个候选位置上都没有。
    NotFound,
    /// 有候选，但没通过校验；每项是 `<路径>：<原因码>`。
    Rejected(Vec<String>),
    /// 这个平台上不能托管引擎进程。
    Unsupported,
}

/// `--out-dir` 的用法：整个目录由这次 setup 生成，还是往一个已有的组件目录里加一张卡。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutDirUse {
    /// 这次 setup 生成一组自己的文件（卡、[`SetupEngine::static_files`]、伴生卡，`setup omlx`）：目录要是空的；
    /// 非空时要 `--force`，而它只替换这些文件，目录里别的卡与文件原样保留（#626）。保留下来的卡与新卡一起过加载器，
    /// 有冲突（重复的 `provider.id`、保留的卡本身加载不了、多张默认卡）就整体失败，目录一个字节不变。
    Owned,
    /// 往一个已有的组件目录里加一张卡（`setup upstream`）：写 [`SetupEngine::card_file`] 这一个文件，目录里别的文件
    /// 都不动，`--force` 替换的也是这一个文件。整个目录（现有的卡加新卡）过服务启动时的加载器之后才放进去。
    /// 这种用法的引擎没有固定文件（[`SetupEngine::static_files`] 为空）。
    AddCard,
}

/// 一种引擎在 `setup` 里的全部专属知识。流程见模块文档。
pub trait SetupEngine: Sync {
    /// 引擎种类（`engine_kind`，规范 §2.12.1，如 `omlx`）：写进输出的 `engine.engine_kind` 与托管卡的
    /// `idoris_managed.engine`。格式合不合规范由生成的卡过引擎身份校验时判定。
    fn kind(&self) -> &str;

    /// 给人看的名字，错误说明里用（如 `oMLX`）。
    fn display_name(&self) -> &'static str;

    /// 找不到可执行文件时给用户的引导（一句人话，说明怎么装、装到哪）。
    fn install_hint(&self) -> &'static str;

    /// 运行期提供引擎 API key 的环境变量名；探测发现要鉴权时写进输出的 `next.needs_env`。
    fn key_env(&self) -> &'static str;

    /// 落盘事务的测试挂钩（[`CommitHook`]）。生产路径恒为 `None`。
    fn commit_hook(&self) -> Option<CommitHook> {
        None
    }

    /// 写进 `next.key_note` 的一句人话：key 是可选的，运行期按什么顺序取。默认只说环境变量；引擎还有别的来源时覆盖它。
    fn key_note(&self) -> String {
        format!(
            "API key 可选：运行期取环境变量 {}，没有就不带鉴权连接",
            self.key_env()
        )
    }

    /// 采纳模式：位置上有没有可执行文件。只做文件检查，不校验签名、不执行它：采纳不需要拉起它，
    /// 校验签名要读整个 `.app`，在这里是白花时间。
    fn locate_executable(&self) -> impl Future<Output = Option<PathBuf>> + Send;

    /// 托管模式：找到可执行文件并完整校验（含签名），并取它的版本；之后拉起的就是它。
    fn discover_executable(
        &self,
    ) -> impl Future<Output = Result<FoundExecutable, FindFailure>> + Send;

    /// `--auto` 依次探测的 loopback 端口，先到先得。
    fn auto_ports(&self) -> &[u16];

    /// 托管时缺省端口的候选范围。
    fn managed_port_range(&self) -> RangeInclusive<u16> {
        MANAGED_PORT_RANGE
    }

    /// 本机已装的模型 id（离线核对 `--bind` 用）；`None` 表示定位不到模型目录。读的是
    /// [`SetupEngine::managed_models_dir`] 的那个目录。
    fn installed_models(&self) -> Option<Vec<String>>;

    /// `--auto` 带了 API key 时，向要鉴权的候选端口发 key 之前，先确认现在监听这个端口的就是这种引擎
    /// （[#619](https://github.com/iDoris-ai/iDoris/issues/619)：候选端口是猜的，本机别的进程可能抢先占住它，拿到 key）。
    /// 缺省是「不能确认」：不实现的引擎，key 永远不会发给没有校验过的端口。
    fn verify_listener(&self, _port: u16) -> impl Future<Output = bool> + Send {
        async { false }
    }

    /// 托管引擎的模型目录：拉起时显式传给引擎的 `--model-dir`（引擎管理器 S5 直接用这个函数），也是离线核对
    /// `--bind` 的目录。取运行用户的约定位置；定位不到、或这个路径引擎吃不了时报错，托管模式在找端口、找引擎
    /// 之前就拒绝。
    fn managed_models_dir(&self) -> Result<PathBuf, SetupError>;

    /// 探测 `endpoint`。`lifecycle` 为真时还要求对端有生命周期状态接口（该引擎的完整形状，而不只是「OpenAI 兼容」）。
    /// 对端要鉴权时：没给 key 是 `auth_required`，给了 key 被拒是 `auth_failed`；不像这个引擎的错误码由引擎自己定。
    fn probe(
        &self,
        endpoint: &str,
        key: Option<&str>,
        lifecycle: bool,
    ) -> impl Future<Output = Result<Probed, SetupError>> + Send;

    /// 引擎卡在 `--out-dir` 里的文件名（如 `omlx.yaml`）。文件名由引擎实例决定（`setup upstream` 的取决于 `--id`），
    /// 所以生命周期跟着 `&self`。
    fn card_file(&self) -> &str;

    /// 引擎卡的模板；端点、加载策略、绑定与托管声明由通用流程填。模板自带的 `extensions` 保留，模板自带的
    /// `_degradation` 说明在合并时放在通用说明之前。
    fn card_template(&self) -> Result<ComponentCard, SetupError>;

    /// 与引擎卡一起写进 `--out-dir` 的固定文件（文件名，内容）。
    fn static_files(&self) -> &'static [(&'static str, &'static str)] {
        &[]
    }

    /// `--out-dir` 的用法（[`OutDirUse`]）。缺省是整个目录由这次 setup 生成。
    fn out_dir_use(&self) -> OutDirUse {
        OutDirUse::Owned
    }

    /// 对 `--bind` 的引擎专属核对：在任何探测与写入之前执行，排在通用的「角色名合法」之后。缺省不加限制。
    fn check_requested_bindings(&self, _bindings: &[(String, String)]) -> Result<(), SetupError> {
        Ok(())
    }

    /// 伴生卡（IDORIS-CORE S7）可能写出的文件名。它们和 [`SetupEngine::card_file`]、[`SetupEngine::static_files`]
    /// 一样属于 `--force` 的整体替换范围：这次没生成伴生卡、上次生成过的，`--force` 会把它删掉，不留指向旧模型的残卡。
    /// 缺省没有伴生卡。
    fn companion_files(&self) -> &'static [&'static str] {
        &[]
    }

    /// 由伴生卡承载的角色：`--bind` 里出现这些角色时，这些绑定从主卡挪到伴生卡（只有一张卡带着这个角色，
    /// 选卡没有歧义）。缺省没有。
    fn companion_roles(&self) -> &'static [&'static str] {
        &[]
    }

    /// 生成伴生卡：`main` 是已经填好的主卡（同 endpoint），`bindings` 是 [`SetupEngine::companion_roles`] 里的那些
    /// 绑定（只在它们非空时才被调用）。返回 `(文件名, 卡)`；文件名必须在 [`SetupEngine::companion_files`] 里。
    /// 落盘、整体校验（目录里所有卡一起过服务启动时的加载器）、`--force` 的范围由通用的 [`run_setup`] 负责，
    /// 引擎这边不写任何文件。缺省不生成。
    fn companion_card(
        &self,
        _main: &ComponentCard,
        _bindings: &[(String, String)],
    ) -> Result<Option<(String, ComponentCard)>, SetupError> {
        Ok(None)
    }
}

/// 引擎有 [`idoris_upstream::engine::EngineRecipe`]（规范 §2.12.1）时，[`SetupEngine`] 两个可执行文件方法的通用实现：
/// 配方是引擎管理器（S5）也要用的那一份，去哪里找、怎么校验在配方里定义一次。引擎进程管理在 unix 以外的平台没有，
/// 所以这个模块也一样；通用流程本身不依赖它。
#[cfg(unix)]
pub mod recipe_backed {
    use idoris_upstream::engine::{
        CodesignPolicy, DiscoveryError, EngineRecipe, discover_executable,
    };

    use super::{FindFailure, FoundExecutable, VERSION_TIMEOUT};
    use std::path::{Path, PathBuf};

    /// 配方的发现计划去掉签名检查之后，找得到就是「装了」。`explicit` 给了就只看它。
    pub async fn locate(recipe: &dyn EngineRecipe, explicit: Option<&Path>) -> Option<PathBuf> {
        let mut plan = recipe.discovery_plan();
        plan.codesign = CodesignPolicy::Off;
        discover_executable(&plan, explicit)
            .await
            .ok()
            .map(|found| found.path)
    }

    /// 配方的完整发现（含签名检查），再执行 `<可执行文件> --version` 取版本（取不到不是错误）。
    pub async fn discover(
        recipe: &dyn EngineRecipe,
        explicit: Option<&Path>,
    ) -> Result<FoundExecutable, FindFailure> {
        match recipe.discover(explicit).await {
            Ok(found) => {
                let version = recipe.version(&found.path, VERSION_TIMEOUT).await.ok();
                Ok(FoundExecutable {
                    path: found.path,
                    version,
                })
            }
            Err(DiscoveryError::NotFound) => Err(FindFailure::NotFound),
            Err(DiscoveryError::Rejected(rejections)) => Err(FindFailure::Rejected(
                rejections
                    .iter()
                    .map(|r| format!("{}：{}", r.path.display(), r.reason.code()))
                    .collect(),
            )),
        }
    }
}

/// 流程的第一阶段：引擎在哪里。
struct Resolved {
    endpoint: String,
    /// 探测的结果；`None` 是没有探测（`--no-probe`，或托管）。
    probed: Option<Probed>,
    managed: bool,
}

/// `engine` 输出对象的内容。
struct EngineReport {
    installed: bool,
    executable: Option<PathBuf>,
    version: Option<String>,
}

/// `--auto` 的探测结果。
enum Scan {
    Found {
        endpoint: String,
        probed: Probed,
    },
    /// 有候选端口要求 API key（没给，或给了被拒）：不是「没有引擎」，不能当作没找到。
    Denied(SetupError),
    Absent,
}

/// 入口。`api_key` 只用于探测。
pub async fn run_setup<E: SetupEngine>(
    engine: &E,
    options: &SetupOptions,
    api_key: Option<&str>,
) -> Result<Value, SetupError> {
    let resolved = resolve(engine, options, api_key).await?;
    check_bindings(engine, &options.bindings, &resolved)?;
    let report = if resolved.managed {
        discover(engine).await?
    } else {
        presence(engine).await
    };
    let (main_bindings, companion_bindings): (Vec<_>, Vec<_>) = if options
        .bindings
        .iter()
        .any(|(role, _)| is_companion_role(engine, role))
    {
        options
            .bindings
            .iter()
            .cloned()
            .partition(|(role, _)| !is_companion_role(engine, role))
    } else {
        (options.bindings.clone(), Vec::new())
    };
    let card = build_card(engine, &resolved, options.load_mode, &main_bindings)?;
    check_identity(engine, &card, resolved.managed)?;
    let yaml = serde_yaml::to_string(&card)
        .map_err(|e| SetupError::new("validation_failed", format!("生成组件卡失败：{e}")))?;
    let companion = if companion_bindings.is_empty() {
        None
    } else {
        engine.companion_card(&card, &companion_bindings)?
    };
    let companion = match companion {
        Some((file, companion_card)) => {
            if !engine.companion_files().contains(&file.as_str()) {
                return Err(SetupError::new(
                    "validation_failed",
                    format!("伴生卡的文件名 {file:?} 不在引擎声明的 companion_files 里"),
                ));
            }
            // 伴生卡共用主卡的引擎，自己没有生命周期：不带托管声明，种类与主卡一致。
            check_identity(engine, &companion_card, false)?;
            let companion_yaml = serde_yaml::to_string(&companion_card).map_err(|e| {
                SetupError::new("validation_failed", format!("生成组件卡失败：{e}"))
            })?;
            Some((file, companion_yaml))
        }
        None => None,
    };
    let dir = match engine.out_dir_use() {
        OutDirUse::Owned => {
            let mut files = vec![(engine.card_file(), yaml.as_str())];
            files.extend(engine.static_files().iter().copied());
            if let Some((file, companion_yaml)) = &companion {
                files.push((file.as_str(), companion_yaml.as_str()));
            }
            let hook = engine.commit_hook();
            write_validated_hooked(
                &options.out_dir,
                options.force,
                &files,
                engine.companion_files(),
                hook.as_ref(),
            )?
        }
        OutDirUse::AddCard if companion.is_some() => {
            return Err(SetupError::new(
                "validation_failed",
                "引擎要生成伴生卡，却又要往已有目录里只加一张卡：两者不能同时用",
            ));
        }
        OutDirUse::AddCard => write_added(
            &options.out_dir,
            options.force,
            engine.card_file(),
            &yaml,
            &card,
        )?,
    };

    let bindings: Map<String, Value> = options
        .bindings
        .iter()
        .map(|(r, m)| (r.clone(), json!(m)))
        .collect();
    let auth_required = resolved.probed.as_ref().is_some_and(|p| p.auth_required);
    let auth = match &resolved.probed {
        Some(p) if p.auth_required => "required",
        Some(_) => "none",
        None => "unknown",
    };
    let models = match resolved.probed {
        Some(p) => p.models,
        None => Vec::new(),
    };
    let companion_file = companion.map(|(file, _)| file);
    let mut next = json!({"env": {"IDORIS_COMPONENTS_DIR": dir.display().to_string()}});
    if auth_required {
        // `needs_env` 的形状不变（它是优先级最高的那个变量名）；`key_note` 说明它其实是可选的、有哪些回退来源。
        next["needs_env"] = json!([engine.key_env()]);
        next["key_note"] = json!(engine.key_note());
    }
    let mut engine_json = json!({
        "engine_kind": engine.kind(),
        "installed": report.installed,
        "executable": report.executable.map(|path| path.display().to_string()),
    });
    if let Some(version) = report.version {
        engine_json["version"] = json!(version);
    }
    let mut output = json!({
        "status": "ok",
        "mode": if resolved.managed { "managed" } else { "adopt" },
        "endpoint": resolved.endpoint,
        "auth": auth,
        "models": models,
        "bindings": bindings,
        "engine": engine_json,
        "components_dir": dir.display().to_string(),
        "next": next,
    });
    // 只增不改：这次生成了伴生卡才有这个字段，没生成时输出与以前逐字节一致。
    if let Some(file) = companion_file {
        output["companion_card_file"] = json!(file);
    }
    Ok(output)
}

fn is_companion_role<E: SetupEngine>(engine: &E, role: &str) -> bool {
    engine.companion_roles().contains(&role)
}

fn managed_file_names<E: SetupEngine>(engine: &E) -> Vec<&str> {
    std::iter::once(engine.card_file())
        .chain(engine.static_files().iter().map(|(name, _)| *name))
        .chain(engine.companion_files().iter().copied())
        .collect()
}

/// 不碰网络与文件系统写入的前置检查：角色名、引擎对 `--bind` 的专属核对、out-dir。
fn precheck<E: SetupEngine>(engine: &E, options: &SetupOptions) -> Result<(), SetupError> {
    for (role, _) in &options.bindings {
        if !idoris_policy::ROLES.iter().any(|r| r.as_str() == role) {
            return Err(SetupError::new(
                "invalid_role",
                format!("未知角色 {role:?}"),
            ));
        }
    }
    engine.check_requested_bindings(&options.bindings)?;
    match engine.out_dir_use() {
        OutDirUse::Owned => check_out_dir(
            &options.out_dir,
            options.force,
            None,
            &managed_file_names(engine),
        ),
        OutDirUse::AddCard => {
            // 加一张卡的用法只写一个文件：引擎若还声明了固定文件，它们在这条路径上不会被写出，拒绝而不是悄悄丢掉。
            if !engine.static_files().is_empty() {
                return Err(SetupError::new(
                    "validation_failed",
                    "引擎声明了固定文件，却又要往已有目录里只加一张卡：两者不能同时用",
                ));
            }
            check_add_target(&options.out_dir, options.force, engine.card_file())
        }
    }
}

async fn resolve<E: SetupEngine>(
    engine: &E,
    options: &SetupOptions,
    api_key: Option<&str>,
) -> Result<Resolved, SetupError> {
    match &options.target {
        SetupTarget::Endpoint { url, probe } => {
            let endpoint = normalize_endpoint(url)?;
            precheck(engine, options)?;
            let probed = if *probe {
                let lifecycle = options.load_mode == LoadMode::OnDemand;
                Some(engine.probe(&endpoint, api_key, lifecycle).await?)
            } else {
                None
            };
            Ok(Resolved {
                endpoint,
                probed,
                managed: false,
            })
        }
        SetupTarget::Auto { managed } => {
            precheck(engine, options)?;
            match scan(engine, api_key).await {
                Scan::Found { endpoint, probed } => Ok(Resolved {
                    endpoint,
                    probed: Some(probed),
                    managed: false,
                }),
                Scan::Denied(error) => Err(error),
                Scan::Absent => match managed {
                    Some(managed) => managed_target(engine, *managed),
                    None => Err(SetupError::new(
                        "unreachable",
                        format!(
                            "未在 127.0.0.1 的 {} 端口上找到 {}；要交给 iDoris 托管，请加 --managed",
                            engine
                                .auto_ports()
                                .iter()
                                .map(u16::to_string)
                                .collect::<Vec<_>>()
                                .join("、"),
                            engine.display_name()
                        ),
                    )),
                },
            }
        }
        SetupTarget::Managed(managed) => {
            precheck(engine, options)?;
            managed_target(engine, *managed)
        }
    }
}

fn managed_target<E: SetupEngine>(
    engine: &E,
    managed: ManagedOptions,
) -> Result<Resolved, SetupError> {
    // 托管引擎拉起时要显式传模型目录：先确认它定位得到、引擎吃得了，再选端口（两步都没有副作用）。
    engine.managed_models_dir()?;
    let port = match managed.port {
        Some(port) if port_is_free(port) => port,
        Some(port) => {
            return Err(SetupError::new(
                "port_in_use",
                format!(
                    "端口 {port} 已被占用：换一个 --managed-port，或不给它，让 iDoris 在 {} 里挑一个空闲端口",
                    describe_range(&engine.managed_port_range())
                ),
            ));
        }
        None => engine
            .managed_port_range()
            .find(|port| port_is_free(*port))
            .ok_or_else(|| {
                SetupError::new(
                    "port_in_use",
                    format!(
                        "{} 里没有空闲端口：释放其中一个，或用 --managed-port 指定别的端口",
                        describe_range(&engine.managed_port_range())
                    ),
                )
            })?,
    };
    Ok(Resolved {
        endpoint: format!("http://127.0.0.1:{port}"),
        probed: None,
        managed: true,
    })
}

fn describe_range(range: &RangeInclusive<u16>) -> String {
    format!("{}–{}", range.start(), range.end())
}

/// 此刻有没有人占着 `127.0.0.1:<port>`：绑不上就是被占了；绑得上还要看有没有人在听，
/// 因为有的系统允许在一个通配地址的监听之上再绑回环地址。
fn port_is_free(port: u16) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    if TcpListener::bind(addr).is_err() {
        return false;
    }
    TcpStream::connect_timeout(&addr, PORT_CONNECT_TIMEOUT).is_err()
}

/// `--auto`：按引擎给的顺序探测候选端口，取第一个形状是这种引擎的。探测先发匿名请求（不带 key）。
/// 候选要鉴权（匿名请求被拒）而调用方给了 `api_key` 时：先过端口属主校验（[`SetupEngine::verify_listener`]，#619），
/// 通过了才带 key 再探一次（[`SetupEngine::probe`] 每次新建 HTTP 客户端，这一次请求不会复用校验之前的连接）；
/// 没通过就**不发 key**。要鉴权而没法进去的候选记下来：没有一个合格时，它们比「没找到」更接近真相，错误里带上它的地址。
async fn scan<E: SetupEngine>(engine: &E, api_key: Option<&str>) -> Scan {
    let mut denied: Option<SetupError> = None;
    for port in engine.auto_ports() {
        let endpoint = format!("http://127.0.0.1:{port}");
        match engine.probe(&endpoint, None, true).await {
            Ok(probed) => return Scan::Found { endpoint, probed },
            Err(error) if error.code == "auth_required" => {
                let name = engine.display_name();
                let refusal = match api_key {
                    None => SetupError::new(
                        "auth_required",
                        format!(
                            "{endpoint} 上的服务要求 API key：如果它是 {name}，请加 --api-key-stdin（--auto 会先确认监听这个端口的进程是 {name}，再发 key），或改用 --endpoint {endpoint} --api-key-stdin；如果不是，去掉 --auto，改用别的 --endpoint 或 --managed"
                        ),
                    ),
                    Some(key) => {
                        if !engine.verify_listener(*port).await {
                            SetupError::new(
                                "auth_required",
                                format!(
                                    "{endpoint} 上的服务要求 API key，已提供的 key 没有发送：没能确认监听这个端口的进程是 {name}（要求是当前用户自己的、{name} 签名团队的进程）。如果确认它是 {name}，请改用 --endpoint {endpoint} --api-key-stdin；如果不是，去掉 --auto，改用别的 --endpoint 或 --managed"
                                ),
                            )
                        } else {
                            match engine.probe(&endpoint, Some(key), true).await {
                                Ok(probed) => return Scan::Found { endpoint, probed },
                                Err(error) if error.code == "auth_failed" => SetupError::new(
                                    "auth_failed",
                                    format!("{endpoint} 上的 {name} 拒绝了提供的 API key"),
                                ),
                                // 验过属主之后又读不到了（引擎退了、换了）：和别的探测失败一样，这个候选不算。
                                Err(_) => continue,
                            }
                        }
                    }
                };
                denied.get_or_insert_with(|| refusal.with_endpoint(endpoint));
            }
            Err(_) => {}
        }
    }
    denied.map_or(Scan::Absent, Scan::Denied)
}

/// 探测过的对照引擎给的模型列表；没探测的对照本机已装的模型。
fn check_bindings<E: SetupEngine>(
    engine: &E,
    bindings: &[(String, String)],
    resolved: &Resolved,
) -> Result<(), SetupError> {
    if let Some(probed) = &resolved.probed {
        for (role, model) in bindings {
            if !probed.models.contains(model) {
                return Err(SetupError::new(
                    "model_not_found",
                    format!(
                        "角色 {role} 绑定的模型不在 {} 的模型列表中",
                        engine.display_name()
                    ),
                ));
            }
        }
        return Ok(());
    }
    if bindings.is_empty() {
        return Ok(());
    }
    let Some(installed) = engine.installed_models() else {
        return Err(SetupError::new(
            "model_not_installed",
            format!(
                "无法定位 {} 的模型目录，不能核对 --bind 的模型是否已安装",
                engine.display_name()
            ),
        ));
    };
    let installed: BTreeSet<String> = installed.into_iter().collect();
    for (role, model) in bindings {
        if !installed.contains(model) {
            return Err(SetupError::new(
                "model_not_installed",
                format!(
                    "角色 {role} 绑定的模型没有安装在本机的 {} 模型目录里",
                    engine.display_name()
                ),
            ));
        }
    }
    Ok(())
}

/// 托管模式：可执行文件要找得到、通过校验（含签名），因为之后拉起的就是它。
async fn discover<E: SetupEngine>(engine: &E) -> Result<EngineReport, SetupError> {
    match engine.discover_executable().await {
        Ok(found) => Ok(EngineReport {
            installed: true,
            executable: Some(found.path),
            version: found.version,
        }),
        Err(failure) => {
            let name = engine.display_name();
            let hint = engine.install_hint();
            let message = match failure {
                FindFailure::NotFound => format!("未找到 {name}：{hint}"),
                FindFailure::Rejected(rejections) => format!(
                    "找到了 {name}，但没有通过校验（{}）：{hint}",
                    rejections.join("；")
                ),
                FindFailure::Unsupported => format!("这个平台上不能由 iDoris 托管 {name}"),
            };
            Err(SetupError::new("engine_not_installed", message))
        }
    }
}

/// 采纳模式：只报告位置上有没有可执行文件（[`SetupEngine::locate_executable`]）。
async fn presence<E: SetupEngine>(engine: &E) -> EngineReport {
    let executable = engine.locate_executable().await;
    EngineReport {
        installed: executable.is_some(),
        executable,
        version: None,
    }
}

fn build_card<E: SetupEngine>(
    engine: &E,
    resolved: &Resolved,
    load_mode: LoadMode,
    bindings: &[(String, String)],
) -> Result<ComponentCard, SetupError> {
    let mut card = engine.card_template()?;
    card.endpoint = resolved.endpoint.clone();
    if let Some(policy) = card.load_policy.as_mut() {
        policy.mode = load_mode;
        if load_mode == LoadMode::Resident {
            // The card contract ties resident to pinned keepalive.
            policy.keepalive = Keepalive::Pinned { pinned: true };
            policy.admission = Admission::Coexist;
        }
    }
    let mut extensions = match card.extensions.take() {
        Some(existing) => existing,
        None => Map::new(),
    };
    // 模板自带的降级说明放在最前面，通用流程加的接在后面（它们说明的是各自加进来的键）。
    let mut degradation: Vec<String> = extensions
        .get("_degradation")
        .and_then(Value::as_str)
        .map(str::to_string)
        .into_iter()
        .collect();
    if resolved.managed {
        extensions.insert(
            "idoris_managed".to_string(),
            json!({"engine": engine.kind(), "mode": "auto"}),
        );
        degradation.push(format!(
            "未托管时退回外部 {}：iDoris 只探测，不拉起、不停止。",
            engine.display_name()
        ));
    }
    if !bindings.is_empty() {
        let models: Map<String, Value> = bindings
            .iter()
            .map(|(r, m)| (r.clone(), json!(m)))
            .collect();
        extensions.insert("idoris_role_models".to_string(), Value::Object(models));
        degradation
            .push("Removing these bindings disables role aliases on this backend.".to_string());
    }
    if !degradation.is_empty() {
        extensions.insert("_degradation".to_string(), json!(degradation.join(" ")));
    }
    if !extensions.is_empty() {
        card.extensions = Some(extensions);
    }
    Ok(card)
}

/// 生成的卡要过服务启动时同一道引擎身份校验，且种类、托管与否和这次 setup 说的一致。
fn check_identity<E: SetupEngine>(
    engine: &E,
    card: &ComponentCard,
    managed: bool,
) -> Result<(), SetupError> {
    let invalid =
        |why: String| SetupError::new("validation_failed", format!("生成的组件卡不合规范：{why}"));
    let identity = engine_identity(card).map_err(|e| invalid(e.to_string()))?;
    if identity.engine_kind != engine.kind() {
        return Err(invalid(format!(
            "卡的引擎种类 {:?} 与 setup 的引擎种类 {:?} 不一致",
            identity.engine_kind,
            engine.kind()
        )));
    }
    if identity.managed != managed {
        return Err(invalid("卡的托管声明与 setup 的模式不一致".to_string()));
    }
    Ok(())
}

/// `own_stage` is this run's staging dir: with `--out-dir .` it sits inside
/// `dir` and must not count as a foreign file. `managed` 是 `--force` 允许覆盖的文件名。
fn check_out_dir(
    dir: &Path,
    force: bool,
    own_stage: Option<&std::ffi::OsStr>,
    managed: &[&str],
) -> Result<(), SetupError> {
    let not_empty = |m: &str| SetupError::new("out_dir_not_empty", m.to_string());
    if !dir.exists() {
        return Ok(());
    }
    let entries = std::fs::read_dir(dir).map_err(|_| not_empty("out-dir 不是可读目录"))?;
    let names: Vec<std::ffi::OsString> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|name| Some(name.as_os_str()) != own_stage)
        .collect();
    // 目录非空时：不带 `--force` 一律拒绝；带 `--force` 放行，替换的只有 `managed` 这些文件，目录里别的卡与文件
    // 原样保留（保留下来的卡是否与新卡相容，由暂存目录里的整体校验判定，#626）。
    if names.is_empty() {
        return Ok(());
    }
    if !force {
        return Err(not_empty(&format!(
            "out-dir 非空；加 --force 只替换 {}，目录里别的卡原样保留",
            managed.join(" / ")
        )));
    }
    // 用户的文件不能和要生成的受管文件是同一个路径（见 `check_name_collisions`）。
    check_name_collisions(dir, managed)?;
    for name in &names {
        let lossy = name.to_string_lossy();
        let Some(target) = managed.iter().find(|m| **m == lossy) else {
            continue;
        };
        // 受管名字上必须是普通文件：目录、符号链接等不替换，更不能挪走或删除（与 `setup upstream` 的 `check_add_target` 同口径）。
        let regular =
            std::fs::symlink_metadata(dir.join(name)).is_ok_and(|m| m.file_type().is_file());
        if !regular {
            return Err(not_empty(&format!(
                "out-dir 里的 {target} 不是普通文件（目录或符号链接），不替换：请先移走它"
            )));
        }
    }
    // 目录非空、没有任何组件卡、也没有这次要替换的文件：多半是 `--out-dir` 写错了，`--force` 不放行。
    let looks_like_a_component_dir = names.iter().any(|name| {
        crate::components::is_card_file(Path::new(name))
            || managed
                .iter()
                .any(|m| m.eq_ignore_ascii_case(&name.to_string_lossy()))
    });
    if !looks_like_a_component_dir {
        return Err(not_empty(
            "out-dir 非空，但里面没有任何组件卡（*.yaml / *.yml），也没有这次要替换的文件：这多半不是组件目录，请检查 --out-dir 是不是写错了",
        ));
    }
    Ok(())
}

/// 目录里有没有和 `names`（这次要写的文件名）撞成同一个路径的、名字不同的条目：
/// - 只差 ASCII 大小写：大小写不敏感的文件系统（macOS 默认的 APFS）上 `OMLX.yaml` 与 `omlx.yaml` 是同一个文件；
///   大小写敏感的文件系统上是两个文件，也一样拒绝，让同一个目录在不同机器上行为一致；
/// - 文件系统自己的折叠规则认为是同一个文件（APFS 把 `ſubscription.yaml` 与 `subscription.yaml` 当同一个文件，
///   ASCII 比较抓不到）：直接比较两个路径背后的文件身份（设备与 inode），不依赖我们自己猜折叠规则。
///
/// 撞上了就 `validation_failed`，点名两个文件名；用户的文件不被替换、不被删除。
/// （同一个文件的硬链接别名也会被这样拒绝：它们是同一个 inode，替换其中一个就改了另一个。）
fn check_name_collisions(dir: &Path, names: &[&str]) -> Result<(), SetupError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|_| SetupError::new("out_dir_not_empty", "out-dir 不是可读目录"))?;
    let existing: Vec<std::ffi::OsString> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .collect();
    for target in names {
        let target_key = std::fs::symlink_metadata(dir.join(target))
            .ok()
            .and_then(|m| file_key(&m));
        for name in &existing {
            let lossy = name.to_string_lossy();
            if lossy == *target {
                continue;
            }
            let same_path = lossy.eq_ignore_ascii_case(target)
                || target_key.is_some_and(|key| {
                    std::fs::symlink_metadata(dir.join(name))
                        .ok()
                        .and_then(|m| file_key(&m))
                        == Some(key)
                });
            if same_path {
                return Err(SetupError::new(
                    "validation_failed",
                    format!(
                        "out-dir 里的 {lossy} 与这次要写的 {target} 是同一个路径（大小写不同，或文件系统把它们当成同一个文件）：请先改名或移走 {lossy}"
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// 文件身份（设备与 inode）；取不到（非 unix）就没有，调用方只按名字比。
fn file_key(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((meta.dev(), meta.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        None
    }
}

/// 在 `out_dir` 同级建一个暂存目录（`out_dir` 的父目录不存在就先建）。与 `out_dir` 同级，所以之后的改名是同一个
/// 文件系统内的操作。
fn create_stage(out_dir: &Path) -> Result<PathBuf, SetupError> {
    let io = |e: std::io::Error| SetupError::new("write_failed", format!("写入失败：{e}"));
    let parent = match out_dir.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    std::fs::create_dir_all(&parent).map_err(io)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let stage = parent.join(format!(".idoris-setup-{}-{nanos}", std::process::id()));
    std::fs::create_dir(&stage).map_err(io)?;
    Ok(stage)
}

/// 事务进行到哪一步（只给测试挂钩用，生产路径不传钩子）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitPhase {
    /// 暂存目录已经校验通过，还没有对 out-dir 做最后一次核对与落盘：测试在这里改动被保留的卡，证明会被发现。
    Validated,
    /// 要被替换的旧文件已经挪进事务目录，新文件还没有放进 out-dir：测试在这里让放入或恢复失败。
    BackedUp,
}

/// 测试挂钩：`(阶段, out-dir)`。
pub type CommitHook = std::sync::Arc<dyn Fn(CommitPhase, &Path) + Send + Sync>;

fn fire(hook: Option<&CommitHook>, phase: CommitPhase, out_dir: &Path) {
    if let Some(hook) = hook {
        hook(phase, out_dir);
    }
}

/// 对一个组件目录的独占建议锁（`flock`）。拿着它的期间，别的 `setup omlx` / `setup upstream` 对同一个目录的写入会排队，
/// 所以「快照保留的卡、校验、落盘」这一整段不会和另一次 setup 交错。Drop 时释放。
///
/// 只防「同一用户并发跑 setup」与手滑造成的改动，不防恶意写入者：建议锁对不遵守它的进程没有约束力，
/// 而最后一次核对与落盘（`hard_link`、`rename`）之间仍有一个很短的窗口，恶意写入者可以在窗口里动手。
/// setup 是用户自己运行的命令行，这个剩余风险是接受的。
pub struct DirLock {
    #[cfg(unix)]
    _flock: nix::fcntl::Flock<std::fs::File>,
}

/// 锁 `out_dir`（已存在时）；还不存在时锁它的父目录（先建出来），拿到锁之后再看一眼：期间被别人建出来了，
/// 就放掉父目录的锁、改锁 `out_dir` 本身。目录本身当锁文件：不在用户目录里留下任何锁文件。
#[cfg(unix)]
pub fn lock_out_dir(out_dir: &Path) -> Result<DirLock, SetupError> {
    use nix::fcntl::{Flock, FlockArg};
    let io = |e: &dyn std::fmt::Display| {
        SetupError::new("write_failed", format!("无法锁定 out-dir（{e}）"))
    };
    loop {
        let (target, is_dir) = if out_dir.is_dir() {
            (out_dir.to_path_buf(), true)
        } else {
            let parent = match out_dir.parent() {
                Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
                _ => PathBuf::from("."),
            };
            std::fs::create_dir_all(&parent).map_err(|e| io(&e))?;
            (parent, false)
        };
        let file = std::fs::File::open(&target).map_err(|e| io(&e))?;
        let flock = Flock::lock(file, FlockArg::LockExclusive).map_err(|(_, e)| io(&e))?;
        if is_dir || !out_dir.is_dir() {
            return Ok(DirLock { _flock: flock });
        }
    }
}

#[cfg(not(unix))]
pub fn lock_out_dir(_out_dir: &Path) -> Result<DirLock, SetupError> {
    Ok(DirLock {})
}

/// 目录里一个「加载器会读的文件」的状态：类型、身份（设备、inode）、大小、mtime、符号链接的指向与目标的身份、内容的 SHA-256。
/// 校验时读到的内容就是这里哈希的内容；落盘前重新取一遍，任何一项不同都说明目录在校验之后被改过。
#[derive(Debug, Clone, PartialEq, Eq)]
struct EntryState {
    kind: u8,
    identity: (u64, u64, u64, i128),
    link: Option<PathBuf>,
    target_identity: Option<(u64, u64, u64, i128)>,
    hash: Option<Vec<u8>>,
}

type Snapshot = std::collections::BTreeMap<std::ffi::OsString, EntryState>;

fn identity(meta: &std::fs::Metadata) -> (u64, u64, u64, i128) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            meta.dev(),
            meta.ino(),
            meta.size(),
            i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
        )
    }
    #[cfg(not(unix))]
    {
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_nanos() as i128);
        (0, 0, meta.len(), mtime)
    }
}

/// 读一个条目：跟随符号链接读内容（和加载器一样，`read_to_string` 跟随链接），所以指向卡的符号链接按卡算；
/// 指向不存在的目标的链接、名字像卡的目录，内容读不出来，返回错误。
fn read_entry(path: &Path) -> (EntryState, std::io::Result<Vec<u8>>) {
    let meta = std::fs::symlink_metadata(path);
    let (kind, id) = match &meta {
        Ok(m) => (
            if m.file_type().is_symlink() {
                1
            } else if m.is_dir() {
                2
            } else if m.is_file() {
                0
            } else {
                3
            },
            identity(m),
        ),
        Err(_) => (9, (0, 0, 0, 0)),
    };
    let link = if kind == 1 {
        std::fs::read_link(path).ok()
    } else {
        None
    };
    let target_identity = if kind == 1 {
        std::fs::metadata(path).ok().map(|m| identity(&m))
    } else {
        None
    };
    let content = std::fs::read(path);
    let hash = content.as_ref().ok().map(|bytes| {
        ring::digest::digest(&ring::digest::SHA256, bytes)
            .as_ref()
            .to_vec()
    });
    (
        EntryState {
            kind,
            identity: id,
            link,
            target_identity,
            hash,
        },
        content,
    )
}

/// 取 `dir` 里加载器会读的文件的快照（`skip` 里的名字除外：它们要被替换）。`copy_to` 有值时顺便把内容复制进暂存目录，
/// 校验用的就是这份内容；读不了的条目（断掉的符号链接、名字像卡的目录）是 `validation_failed`。
/// 不是卡的文件（没有 `.yaml` / `.yml` 扩展名）不在快照里，也不会被碰。
fn snapshot_cards(
    dir: &Path,
    skip: &[&str],
    copy_to: Option<&Path>,
) -> Result<Snapshot, SetupError> {
    let entries = std::fs::read_dir(dir)
        .map_err(|_| SetupError::new("out_dir_not_empty", "out-dir 不是可读目录"))?;
    let mut snapshot = Snapshot::new();
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        // 只排除真的会被替换的条目（名字完全相同）：别的名字即使「像」被替换的文件，也是被保留的卡，要参与校验。
        if !crate::components::is_card_file(&path) || skip.iter().any(|s| name == *s) {
            continue;
        }
        reject_link_into_replaced_files(dir, &path, &name, skip)?;
        let (state, content) = read_entry(&path);
        if let Some(stage) = copy_to {
            let bytes = content.map_err(|e| {
                SetupError::new(
                    "validation_failed",
                    format!(
                        "out-dir 里已有的 {} 读不了（{}）",
                        name.to_string_lossy(),
                        e.kind()
                    ),
                )
            })?;
            std::fs::write(stage.join(&name), bytes)
                .map_err(|e| SetupError::new("write_failed", format!("写入失败：{e}")))?;
        }
        snapshot.insert(name, state);
    }
    Ok(snapshot)
}

/// 被保留的符号链接若指向这次要替换或删除的文件（`skip`，例如 `alias.yaml -> omlx-embed.yaml`）：暂存目录里放的是
/// 解引用后的内容，校验会通过，落盘之后目标没了或变了，链接要么悬空、要么指向没校验过的内容，所以整体失败。
fn reject_link_into_replaced_files(
    dir: &Path,
    path: &Path,
    name: &std::ffi::OsStr,
    skip: &[&str],
) -> Result<(), SetupError> {
    if !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        return Ok(());
    }
    let (Ok(target), Ok(root)) = (std::fs::canonicalize(path), std::fs::canonicalize(dir)) else {
        return Ok(());
    };
    let hits_replaced = target.parent() == Some(root.as_path())
        && target
            .file_name()
            .is_some_and(|t| skip.iter().any(|s| t == *s));
    if !hits_replaced {
        return Ok(());
    }
    let target_name = target.file_name().unwrap_or_default().to_string_lossy();
    Err(SetupError::new(
        "validation_failed",
        format!(
            "out-dir 里的 {} 是指向 {target_name} 的符号链接，而这次 setup 会替换或删除 {target_name}：先移走这个链接再运行",
            name.to_string_lossy(),
        ),
    ))
}

/// 落盘前的最后一次核对：被保留的卡与校验时相比，条目集合、文件身份、大小、mtime、内容哈希都没变。
/// 变了就整体放弃（`validation_failed`，什么也没写），说明里点名变了的文件。
fn recheck_kept(dir: &Path, skip: &[&str], before: &Snapshot) -> Result<(), SetupError> {
    let now = snapshot_cards(dir, skip, None)?;
    if &now == before {
        return Ok(());
    }
    let mut changed: Vec<String> = before
        .keys()
        .chain(now.keys())
        .filter(|name| before.get(*name) != now.get(*name))
        .map(|name| name.to_string_lossy().into_owned())
        .collect();
    changed.sort();
    changed.dedup();
    Err(SetupError::new(
        "validation_failed",
        format!(
            "out-dir 在 setup 运行期间被改动（{}）：为了不写出一个没校验过的目录，什么也没写，请确认后重新运行",
            changed.join("、")
        ),
    ))
}

/// Stage in a sibling dir, run the server's loader on it, then move into
/// place. Nothing in `out_dir` changes unless validation passed, and replacing
/// an existing dir is all-or-nothing: the old managed files are moved into a
/// private backup directory first and restored if any later step fails.
/// Write failures are `write_failed`.
///
/// 整段（快照保留的卡、校验、落盘）在 `out_dir` 的独占锁里（[`lock_out_dir`]）；落盘前核对被保留的卡没有变
/// （[`recheck_kept`]）。备份放在暂存目录里独占创建的 0700 子目录，不往用户目录里放任何可能撞名的备份文件。
/// `files` 是 `(文件名, 内容)`；它们的文件名就是 `--force` 替换的集合，目录里别的卡原样保留。
pub fn write_validated(
    out_dir: &Path,
    force: bool,
    files: &[(&str, &str)],
) -> Result<PathBuf, SetupError> {
    write_validated_with(out_dir, force, files, &[])
}

/// [`write_validated`]，另有 `also_managed`：这次不写、但属于 `--force` 替换范围的文件名（伴生卡）。
/// 它们在目标目录里已经有时一并清掉（和别的被替换的文件一样先挪开，失败时整体恢复），不留旧版本。
pub fn write_validated_with(
    out_dir: &Path,
    force: bool,
    files: &[(&str, &str)],
    also_managed: &[&str],
) -> Result<PathBuf, SetupError> {
    write_validated_hooked(out_dir, force, files, also_managed, None)
}

/// [`write_validated_with`]，带测试挂钩（生产路径传 `None`）。
pub fn write_validated_hooked(
    out_dir: &Path,
    force: bool,
    files: &[(&str, &str)],
    also_managed: &[&str],
    hook: Option<&CommitHook>,
) -> Result<PathBuf, SetupError> {
    let io = |e: std::io::Error| SetupError::new("write_failed", format!("写入失败：{e}"));
    let names: Vec<&str> = files.iter().map(|(name, _)| *name).collect();
    // 只清不写：属于整体替换范围、这次没有生成的文件（上次的伴生卡）。
    let clear_only: Vec<&str> = also_managed
        .iter()
        .copied()
        .filter(|name| !names.contains(name))
        .collect();
    let scope: Vec<&str> = names.iter().chain(clear_only.iter()).copied().collect();
    let _lock = lock_out_dir(out_dir)?;
    let stage = create_stage(out_dir)?;
    let keep_stage = std::cell::Cell::new(false);
    let result = (|| {
        // `--force` 在非空目录上只替换 `scope` 里的文件：别的卡原样留在 `out_dir`，但它们要和新卡一起过加载器，
        // 所以先快照并复制进暂存目录。目录不存在时没有保留的卡。
        let kept = if force && out_dir.is_dir() {
            snapshot_cards(out_dir, &scope, Some(&stage))?
        } else {
            Snapshot::new()
        };
        for (name, content) in files {
            std::fs::write(stage.join(name), content).map_err(io)?;
        }
        // 说明里的暂存目录路径换成 `out_dir`：暂存目录里的文件名与 `out_dir` 里的一一对应，保留下来的卡出问题时点得到名。
        let cards = crate::components::load_components(&stage, false).map_err(|e| {
            SetupError::new(
                "validation_failed",
                e.to_string()
                    .replace(&stage.display().to_string(), &out_dir.display().to_string()),
            )
        })?;
        // 最终的集合（保留的加新的）里至多一张默认卡。
        check_at_most_one_default(&cards)?;
        // serve 启动时还会做的纯配置校验（托管卡的计划、引擎种类）：在发布之前先过一遍，免得 setup 成功而 serve 起不来。
        check_serve_preflight(&cards)?;
        fire(hook, CommitPhase::Validated, out_dir);
        check_out_dir(out_dir, force, stage.file_name(), &scope)?;
        if out_dir.exists() {
            recheck_kept(out_dir, &scope, &kept)?;
            let backup = stage.join(BACKUP_DIR);
            make_private_dir(&backup).map_err(io)?;
            replace_managed(&stage, out_dir, &names, &clear_only, &backup, hook).map_err(
                |failure| {
                    keep_stage.set(!failure.stranded.is_empty());
                    failure.into_setup_error()
                },
            )?;
        } else {
            std::fs::rename(&stage, out_dir).map_err(io)?;
        }
        out_dir.canonicalize().map_err(io)
    })();
    // 回滚没做完时，暂存目录里的备份子目录是用户手工恢复的依据，不能删。
    if !keep_stage.get() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    result
}

/// 暂存目录里放备份的子目录名。加载器只读 `*.yaml` / `*.yml` 文件，这个目录不会被当成卡。
const BACKUP_DIR: &str = ".idoris-backup";

/// 独占创建（已存在就失败）、只有属主可进入的目录。
fn make_private_dir(path: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}

/// 替换失败。`stranded` 为空表示已经全部恢复；否则每一项说明一个没能放回原处的文件在哪里（路径，不含任何内容）。
#[derive(Debug)]
struct CommitFailure {
    error: std::io::Error,
    stranded: Vec<String>,
}

impl CommitFailure {
    fn into_setup_error(self) -> SetupError {
        if self.stranded.is_empty() {
            return SetupError::new("write_failed", format!("写入失败：{}", self.error));
        }
        SetupError::new(
            "write_failed",
            format!(
                "写入失败：{}；回滚没有完成，下面这些文件不在原处，请按说明手工恢复：{}",
                self.error,
                self.stranded.join("；")
            ),
        )
    }
}

/// Moves the staged managed files into an existing `out_dir`, all or nothing.
/// `names` 必须都在暂存目录里；`clear_only` 只在目标目录里挪开（成功后随暂存目录一起删除），不从暂存目录放入。
///
/// 旧文件先挪进 `backup`（独占创建的私有目录，名字不会和用户目录里的任何文件相撞），再用 `hard_link` 放入新文件：
/// 目标名上若又冒出了别的文件，放入失败而不是被覆盖。失败时：删掉已放入的新文件，再把旧文件放回原处（普通文件用
/// `hard_link`，不覆盖；别的类型先确认目标名空着再 `rename`）。每一步的失败都记下来；有任何文件没能放回，
/// 就返回它们现在的确切位置，调用方保留 `backup` 所在的暂存目录。
fn replace_managed(
    stage: &Path,
    out_dir: &Path,
    names: &[&str],
    clear_only: &[&str],
    backup: &Path,
    hook: Option<&CommitHook>,
) -> Result<(), CommitFailure> {
    let mut backed_up: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    let attempt = (|| {
        for name in names.iter().chain(clear_only.iter()).copied() {
            let target = out_dir.join(name);
            match std::fs::symlink_metadata(&target) {
                Ok(_) => {
                    std::fs::rename(&target, backup.join(name))?;
                    backed_up.push(name);
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        fire(hook, CommitPhase::BackedUp, out_dir);
        for name in names.iter().copied() {
            std::fs::hard_link(stage.join(name), out_dir.join(name))?;
            placed.push(name);
        }
        Ok(())
    })();
    let Err(error) = attempt else {
        return Ok(());
    };
    let mut stranded: Vec<String> = Vec::new();
    for name in &placed {
        let path = out_dir.join(name);
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            stranded.push(format!(
                "本次新写的 {} 没能删掉（{e}），它还在 out-dir 里",
                path.display()
            ));
        }
    }
    for name in &backed_up {
        let (from, to) = (backup.join(name), out_dir.join(name));
        let restored = match std::fs::symlink_metadata(&from) {
            Ok(meta) if meta.is_file() => std::fs::hard_link(&from, &to).map(|()| {
                let _ = std::fs::remove_file(&from);
            }),
            _ => match std::fs::symlink_metadata(&to) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::fs::rename(&from, &to),
                _ => Err(std::io::Error::other("原位置上已经有别的文件")),
            },
        };
        if let Err(e) = restored {
            stranded.push(format!(
                "原有的 {} 没能放回 {}（{e}），它的原内容在 {}",
                name,
                to.display(),
                from.display()
            ));
        }
    }
    Err(CommitFailure { error, stranded })
}

/// 卡的文件名要是一个纯文件名（没有路径分隔符，不以 `.` 开头），并且带加载器会读的扩展名：不带的话卡写出去也不会被加载。
fn check_card_file_name(file: &str) -> Result<(), SetupError> {
    let plain = !file.is_empty()
        && !file.starts_with('.')
        && !file.contains(['/', '\\', '\0'])
        && crate::components::is_card_file(Path::new(file));
    if plain {
        return Ok(());
    }
    Err(SetupError::new(
        "validation_failed",
        format!("卡的文件名 {file:?} 不合规：要是不带路径的 *.yaml / *.yml 文件名"),
    ))
}

/// [`OutDirUse::AddCard`] 的前置核对（不碰文件系统写入）：`dir` 不存在，或是目录；目录里没有同名的卡，
/// 有同名的卡时要 `force`，而且它得是普通文件（符号链接、目录都不替换）。
fn check_add_target(dir: &Path, force: bool, file: &str) -> Result<(), SetupError> {
    check_card_file_name(file)?;
    let unreadable = || SetupError::new("out_dir_not_empty", "out-dir 不是可读目录");
    match std::fs::symlink_metadata(dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => return Err(unreadable()),
        Ok(_) if !dir.is_dir() => return Err(unreadable()),
        Ok(_) => {}
    }
    check_name_collisions(dir, &[file])?;
    let conflict = |why: String| SetupError::new("id_conflict", why);
    match std::fs::symlink_metadata(dir.join(file)) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(unreadable()),
        Ok(_) if !force => Err(conflict(format!(
            "out-dir 里已经有 {file}：要替换它就加 --force，否则换一个 --id"
        ))),
        Ok(meta) if meta.file_type().is_file() => Ok(()),
        Ok(_) => Err(conflict(format!(
            "out-dir 里的 {file} 不是普通文件，不替换"
        ))),
    }
}

/// 往 `out_dir` 里加一张卡（[`OutDirUse::AddCard`]），目录里别的文件都不动。
///
/// 在与 `out_dir` 同级的暂存目录里放「`out_dir` 里现有的卡（加载器读的那些文件，除了要被替换的同名卡）加上新卡」，
/// 用服务启动时同一个加载器校验这个整体：重复的 `provider.id`、现有的卡本身加载不了，都在这里失败，`out_dir` 一个字节
/// 不变。最终的集合里默认卡（`idoris_default: true`）超过一张也失败（规范 §2.12.3：至多一张）。
/// 整段在 `out_dir` 的独占锁里（与 `setup omlx` 同一把，[`lock_out_dir`]），落盘前核对现有的卡没有变。
///
/// 通过之后：`out_dir` 不存在就把暂存目录整个改名过去；存在的话，`force` 时用 `rename` 原子替换这一个文件，
/// 否则用 `hard_link`（目标已存在就失败，不会盖掉刚出现的同名文件）。
pub fn write_added(
    out_dir: &Path,
    force: bool,
    file: &str,
    content: &str,
    card: &ComponentCard,
) -> Result<PathBuf, SetupError> {
    write_added_hooked(out_dir, force, file, content, card, None)
}

/// [`write_added`]，带测试挂钩（生产路径传 `None`）。
pub fn write_added_hooked(
    out_dir: &Path,
    force: bool,
    file: &str,
    content: &str,
    card: &ComponentCard,
    hook: Option<&CommitHook>,
) -> Result<PathBuf, SetupError> {
    let io = |e: std::io::Error| SetupError::new("write_failed", format!("写入失败：{e}"));
    check_card_file_name(file)?;
    let _lock = lock_out_dir(out_dir)?;
    if out_dir.is_dir() {
        check_name_collisions(out_dir, &[file])?;
    }
    let stage = create_stage(out_dir)?;
    let result = (|| {
        let kept = if out_dir.is_dir() {
            snapshot_cards(out_dir, &[file], Some(&stage))?
        } else {
            Snapshot::new()
        };
        std::fs::write(stage.join(file), content).map_err(io)?;
        let cards = crate::components::load_components(&stage, false)
            .map_err(|e| added_load_failure(e, &stage, out_dir, &card.provider.id))?;
        check_at_most_one_default(&cards)?;
        check_serve_preflight(&cards)?;
        fire(hook, CommitPhase::Validated, out_dir);
        if !out_dir.exists() {
            // 没有现有的卡可镜像，暂存目录里只有新卡。
            std::fs::rename(&stage, out_dir).map_err(io)?;
        } else {
            if out_dir.is_dir() {
                recheck_kept(out_dir, &[file], &kept)?;
            }
            if force {
                std::fs::rename(stage.join(file), out_dir.join(file)).map_err(io)?;
            } else {
                std::fs::hard_link(stage.join(file), out_dir.join(file)).map_err(|e| {
                    if e.kind() == std::io::ErrorKind::AlreadyExists {
                        SetupError::new(
                            "id_conflict",
                            format!(
                                "out-dir 里刚刚出现了 {file}：要替换它就加 --force，否则换一个 --id"
                            ),
                        )
                    } else {
                        io(e)
                    }
                })?;
            }
        }
        out_dir.canonicalize().map_err(io)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// 整个目录加载失败：别的文件里已经有同一个 `provider.id` 是 `id_conflict`，其余是 `validation_failed`。
/// 说明里的暂存目录路径换成 `out_dir`（暂存目录里的文件名与 `out_dir` 里的一一对应）。
fn added_load_failure(
    error: crate::components::LoadError,
    stage: &Path,
    out_dir: &Path,
    id: &str,
) -> SetupError {
    use crate::components::LoadError;
    if let LoadError::Registration(idoris_policy::RegistrationError::DuplicateProviderId {
        id: duplicated,
    }) = &error
        && duplicated == id
    {
        return SetupError::new(
            "id_conflict",
            format!(
                "out-dir 里另一个文件已经声明了 provider.id {id:?}：换一个 --id，或先移走那张卡"
            ),
        );
    }
    SetupError::new(
        "validation_failed",
        error
            .to_string()
            .replace(&stage.display().to_string(), &out_dir.display().to_string()),
    )
}

/// serve 启动时对卡集合做的纯配置校验（[`crate::preflight`]）：不过就是 `validation_failed`，说明就是 serve 会报的那一句。
fn check_serve_preflight(cards: &[ComponentCard]) -> Result<(), SetupError> {
    crate::preflight::validate_card_set(cards).map_err(|why| {
        SetupError::new(
            "validation_failed",
            format!("这个目录里的卡放在一起，iDoris 启动时会失败，所以没有写入：{why}"),
        )
    })
}

/// 最终的卡集合（保留的加新写的）里至多一张默认卡（规范 §2.12.3：至多一张卡带 `idoris_default`）。
/// 数的是整个集合，不只看新卡：目录里手写出两张默认卡，这次写入也不放行，要先改掉多余的。
fn check_at_most_one_default(cards: &[ComponentCard]) -> Result<(), SetupError> {
    let defaults: Vec<&str> = cards
        .iter()
        .filter(|card| crate::engine_kind::declares_default(card))
        .map(|card| card.provider.id.as_str())
        .collect();
    if defaults.len() <= 1 {
        return Ok(());
    }
    Err(SetupError::new(
        "validation_failed",
        format!(
            "out-dir 里有 {} 张默认上游 {}（extensions.idoris_default）：至多一张卡可以是默认，先把多余的改掉，或者不加 --default",
            defaults.len(),
            defaults.join("、")
        ),
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const OMLX_CARD: &str = include_str!("../../../config/components/omlx.yaml");
    const SUBSCRIPTION_CARD: &str = include_str!("../../../config/components/subscription.yaml");

    /// 两个文件的组：与 `setup omlx` 写出的相同，端点是模板里的 8000。
    fn files() -> [(&'static str, &'static str); 2] {
        [
            ("omlx.yaml", OMLX_CARD),
            ("subscription.yaml", SUBSCRIPTION_CARD),
        ]
    }

    #[test]
    fn foreign_file_appearing_after_the_first_check_is_not_clobbered() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("comp");
        std::fs::create_dir(&out).unwrap();
        // Simulates a dir populated while the probe was in flight.
        std::fs::write(out.join("mine.txt"), "keep").unwrap();
        std::fs::write(out.join("omlx.yaml"), "old").unwrap();
        // 不带 --force：非空就拒绝，什么也不变。
        let err = write_validated(&out, false, &files()).unwrap_err();
        assert_eq!(err.code, "out_dir_not_empty");
        assert_eq!(
            std::fs::read_to_string(out.join("mine.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "old"
        );
        // 带 --force：只替换自己的文件，别的文件不碰（#626）。
        write_validated(&out, true, &files()).unwrap();
        assert_eq!(
            std::fs::read_to_string(out.join("mine.txt")).unwrap(),
            "keep"
        );
        assert_ne!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "old"
        );
    }

    #[test]
    fn mid_replace_failure_restores_the_previous_files() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("comp");
        let stage = tmp.path().join("stage");
        std::fs::create_dir(&out).unwrap();
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(out.join("omlx.yaml"), "old-omlx").unwrap();
        std::fs::write(out.join("subscription.yaml"), "old-sub").unwrap();
        std::fs::write(stage.join("omlx.yaml"), "new-omlx").unwrap();
        // subscription.yaml missing from the stage: the second move fails
        // after omlx.yaml was already replaced.
        let backup = stage.join(BACKUP_DIR);
        make_private_dir(&backup).unwrap();
        replace_managed(
            &stage,
            &out,
            &["omlx.yaml", "subscription.yaml"],
            &[],
            &backup,
            None,
        )
        .unwrap_err();
        assert_eq!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "old-omlx"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("subscription.yaml")).unwrap(),
            "old-sub"
        );
        let mut names: Vec<_> = std::fs::read_dir(&out)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(names, ["omlx.yaml", "subscription.yaml"]);
    }

    /// 只清不写的文件（上次的伴生卡）：成功时被删掉；替换中途失败时和别的文件一起恢复。
    #[test]
    fn clear_only_files_are_removed_on_success_and_restored_on_failure() {
        let setup = || {
            let tmp = tempfile::tempdir().unwrap();
            let out = tmp.path().join("comp");
            let stage = tmp.path().join("stage");
            std::fs::create_dir(&out).unwrap();
            std::fs::create_dir(&stage).unwrap();
            std::fs::write(out.join("omlx.yaml"), "old-omlx").unwrap();
            std::fs::write(out.join("omlx-embed.yaml"), "old-embed").unwrap();
            std::fs::write(stage.join("omlx.yaml"), "new-omlx").unwrap();
            (tmp, out, stage)
        };
        let listing = |out: &Path| {
            let mut names: Vec<_> = std::fs::read_dir(out)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        };
        let (_tmp, out, stage) = setup();
        let backup = stage.join(BACKUP_DIR);
        make_private_dir(&backup).unwrap();
        replace_managed(
            &stage,
            &out,
            &["omlx.yaml"],
            &["omlx-embed.yaml"],
            &backup,
            None,
        )
        .unwrap();
        assert_eq!(listing(&out), ["omlx.yaml"]);
        assert_eq!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "new-omlx"
        );

        let (_tmp, out, stage) = setup();
        // 暂存目录里缺 subscription.yaml：放入阶段失败，两个旧文件都要回来。
        let backup = stage.join(BACKUP_DIR);
        make_private_dir(&backup).unwrap();
        replace_managed(
            &stage,
            &out,
            &["omlx.yaml", "subscription.yaml"],
            &["omlx-embed.yaml"],
            &backup,
            None,
        )
        .unwrap_err();
        assert_eq!(listing(&out), ["omlx-embed.yaml", "omlx.yaml"]);
        assert_eq!(
            std::fs::read_to_string(out.join("omlx-embed.yaml")).unwrap(),
            "old-embed"
        );
        assert_eq!(
            std::fs::read_to_string(out.join("omlx.yaml")).unwrap(),
            "old-omlx"
        );
    }

    /// 快照只排除名字完全相同的条目（真的会被替换的）：名字「像」被替换文件的卡（只差大小写）是被保留的卡，要进快照、
    /// 复制进暂存目录参与校验，不能被悄悄漏掉（#633 评审）。大小写不敏感的文件系统上没有「只差大小写的两个文件」，
    /// 这时用一个叫 `other.yaml` 的卡对照，同样要求它在快照里。
    #[test]
    fn the_snapshot_skips_only_exactly_named_entries() {
        let dir = tempfile::tempdir().unwrap();
        let stage = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("LM.yaml"), "a: 1").unwrap();
        std::fs::write(dir.path().join("other.yaml"), "b: 2").unwrap();
        let snapshot = snapshot_cards(dir.path(), &["lm.yaml"], Some(stage.path())).unwrap();
        let names: Vec<_> = snapshot
            .keys()
            .map(|n| n.to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"other.yaml".to_string()), "{names:?}");
        assert!(
            names.contains(&"LM.yaml".to_string()),
            "名字不完全相同的条目不能被当成要替换的而漏掉：{names:?}"
        );
        assert!(stage.path().join("LM.yaml").exists());
        // 名字完全相同的才被排除。
        let snapshot = snapshot_cards(dir.path(), &["LM.yaml"], None).unwrap();
        assert!(!snapshot.contains_key(std::ffi::OsStr::new("LM.yaml")));
    }

    #[test]
    fn io_failure_is_write_failed_not_validation_failed() {
        let tmp = tempfile::tempdir().unwrap();
        let blocker = tmp.path().join("file");
        std::fs::write(&blocker, "x").unwrap();
        let err = write_validated(&blocker.join("comp"), false, &files()).unwrap_err();
        assert_eq!(err.code, "write_failed");
    }

    /// `--force` 放行的文件名由调用方传入：别的引擎的 setup 有自己的名字，不沿用 oMLX 的。
    #[test]
    fn force_lets_a_non_empty_dir_through_and_without_force_nothing_existing_does() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("omlx.yaml"), "x").unwrap();
        // --force：不管目录里是什么都放行（只替换调用方给的文件名，别的保留，整体校验另算）。
        assert!(check_out_dir(tmp.path(), true, None, &["omlx.yaml", "subscription.yaml"]).is_ok());
        assert!(check_out_dir(tmp.path(), true, None, &["fakeserve.yaml"]).is_ok());
        // 没有 --force：任何已有文件都不行，说明里点出会被替换的文件名。
        let err = check_out_dir(tmp.path(), false, None, &["fakeserve.yaml"]).unwrap_err();
        assert_eq!(err.code, "out_dir_not_empty");
        assert!(err.message.contains("fakeserve.yaml"), "{}", err.message);
        // 自己的暂存目录不算已有文件。
        assert!(check_out_dir(tmp.path(), false, Some("omlx.yaml".as_ref()), &[]).is_ok());
    }

    fn listener() -> (TcpListener, u16) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        (listener, port)
    }

    #[test]
    fn a_port_somebody_listens_on_is_not_free_and_is_free_again_after_it_stops() {
        let (held, port) = listener();
        assert!(!port_is_free(port));
        drop(held);
        assert!(port_is_free(port));
    }

    /// 通配地址上的监听：有的系统还允许在它上面再绑回环地址，所以光靠绑定判断不够，还要看有没有人在听。
    #[test]
    fn a_wildcard_listener_also_makes_the_port_not_free() {
        let held = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let port = held.local_addr().unwrap().port();
        assert!(!port_is_free(port));
    }
}
