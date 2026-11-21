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
    /// 整个目录由这次 setup 生成（卡加 [`SetupEngine::static_files`]，`setup omlx`）：目录要是空的，`--force` 只允许
    /// 整体替换这些文件。
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
    let card = build_card(engine, &resolved, options)?;
    check_identity(engine, &card, resolved.managed)?;
    let yaml = serde_yaml::to_string(&card)
        .map_err(|e| SetupError::new("validation_failed", format!("生成组件卡失败：{e}")))?;
    let dir = match engine.out_dir_use() {
        OutDirUse::Owned => {
            let mut files = vec![(engine.card_file(), yaml.as_str())];
            files.extend(engine.static_files().iter().copied());
            write_validated(&options.out_dir, options.force, &files)?
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
    let mut next = json!({"env": {"IDORIS_COMPONENTS_DIR": dir.display().to_string()}});
    if auth_required {
        next["needs_env"] = json!([engine.key_env()]);
    }
    let mut engine_json = json!({
        "engine_kind": engine.kind(),
        "installed": report.installed,
        "executable": report.executable.map(|path| path.display().to_string()),
    });
    if let Some(version) = report.version {
        engine_json["version"] = json!(version);
    }
    Ok(json!({
        "status": "ok",
        "mode": if resolved.managed { "managed" } else { "adopt" },
        "endpoint": resolved.endpoint,
        "auth": auth,
        "models": models,
        "bindings": bindings,
        "engine": engine_json,
        "components_dir": dir.display().to_string(),
        "next": next,
    }))
}

fn managed_file_names<E: SetupEngine>(engine: &E) -> Vec<&str> {
    std::iter::once(engine.card_file())
        .chain(engine.static_files().iter().map(|(name, _)| *name))
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
    options: &SetupOptions,
) -> Result<ComponentCard, SetupError> {
    let mut card = engine.card_template()?;
    card.endpoint = resolved.endpoint.clone();
    if let Some(policy) = card.load_policy.as_mut() {
        policy.mode = options.load_mode;
        if options.load_mode == LoadMode::Resident {
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
    if !options.bindings.is_empty() {
        let models: Map<String, Value> = options
            .bindings
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
    let names: Vec<_> = entries
        .filter_map(Result::ok)
        .map(|e| e.file_name())
        .filter(|name| Some(name.as_os_str()) != own_stage)
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    if force && names.iter().all(|n| managed.iter().any(|m| n == m)) {
        return Ok(());
    }
    Err(not_empty(&format!(
        "out-dir 非空；--force 只允许覆盖 {}",
        managed.join(" / ")
    )))
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

/// Stage in a sibling dir, run the server's loader on it, then move into
/// place. Nothing in `out_dir` changes unless validation passed, and replacing
/// an existing dir is all-or-nothing: the old managed files are renamed aside
/// first and restored if any later step fails. `out_dir` is re-checked right
/// before replacing (the probe may have taken seconds), so a dir that gained
/// foreign files meanwhile is not clobbered. Write failures are `write_failed`.
/// `files` 是 `(文件名, 内容)`；它们的文件名就是 `--force` 允许覆盖的集合。
pub fn write_validated(
    out_dir: &Path,
    force: bool,
    files: &[(&str, &str)],
) -> Result<PathBuf, SetupError> {
    let io = |e: std::io::Error| SetupError::new("write_failed", format!("写入失败：{e}"));
    let names: Vec<&str> = files.iter().map(|(name, _)| *name).collect();
    let stage = create_stage(out_dir)?;
    let result = (|| {
        for (name, content) in files {
            std::fs::write(stage.join(name), content).map_err(io)?;
        }
        crate::components::load_components(&stage, false).map_err(|e| {
            SetupError::new(
                "validation_failed",
                e.to_string()
                    .replace(&stage.display().to_string(), "<staging>"),
            )
        })?;
        check_out_dir(out_dir, force, stage.file_name(), &names)?;
        if out_dir.exists() {
            replace_managed(&stage, out_dir, &names).map_err(io)?;
        } else {
            std::fs::rename(&stage, out_dir).map_err(io)?;
        }
        out_dir.canonicalize().map_err(io)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// Moves the staged managed files into an existing `out_dir`, all or nothing.
fn replace_managed(stage: &Path, out_dir: &Path, names: &[&str]) -> std::io::Result<()> {
    let tag = format!(".idoris-bak-{}", std::process::id());
    let mut backed_up: Vec<&str> = Vec::new();
    let mut placed: Vec<&str> = Vec::new();
    let attempt = (|| {
        for name in names.iter().copied() {
            let target = out_dir.join(name);
            if target.exists() {
                std::fs::rename(&target, out_dir.join(format!("{name}{tag}")))?;
                backed_up.push(name);
            }
        }
        for name in names.iter().copied() {
            std::fs::rename(stage.join(name), out_dir.join(name))?;
            placed.push(name);
        }
        Ok(())
    })();
    let Err(err) = attempt else {
        for name in &backed_up {
            let _ = std::fs::remove_file(out_dir.join(format!("{name}{tag}")));
        }
        return Ok(());
    };
    for name in &placed {
        let _ = std::fs::rename(out_dir.join(name), stage.join(name));
    }
    let stranded: Vec<String> = backed_up
        .iter()
        .filter(|name| {
            std::fs::rename(out_dir.join(format!("{name}{tag}")), out_dir.join(name)).is_err()
        })
        .map(|name| format!("{name}{tag}"))
        .collect();
    if stranded.is_empty() {
        return Err(err);
    }
    Err(std::io::Error::other(format!(
        "{err}；回滚未完成，旧文件保留为 {}",
        stranded.join("、")
    )))
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
/// 不变。新卡要当默认（`idoris_default: true`）时，还核对目录里没有别的默认卡（规范 §2.12.3：至多一张）。
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
    let io = |e: std::io::Error| SetupError::new("write_failed", format!("写入失败：{e}"));
    check_card_file_name(file)?;
    let stage = create_stage(out_dir)?;
    let result = (|| {
        if out_dir.is_dir() {
            mirror_cards(out_dir, &stage, file)?;
        }
        std::fs::write(stage.join(file), content).map_err(io)?;
        let cards = crate::components::load_components(&stage, false)
            .map_err(|e| added_load_failure(e, &stage, out_dir, &card.provider.id))?;
        check_single_default(&cards, card)?;
        if !out_dir.exists() {
            // 没有现有的卡可镜像，暂存目录里只有新卡。
            std::fs::rename(&stage, out_dir).map_err(io)?;
        } else if force {
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
        out_dir.canonicalize().map_err(io)
    })();
    let _ = std::fs::remove_dir_all(&stage);
    result
}

/// 把 `from` 里加载器会读的文件复制进暂存目录，`skip`（要被替换的同名卡）除外。
fn mirror_cards(from: &Path, stage: &Path, skip: &str) -> Result<(), SetupError> {
    let entries = std::fs::read_dir(from)
        .map_err(|_| SetupError::new("out_dir_not_empty", "out-dir 不是可读目录"))?;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        let name = entry.file_name();
        if !crate::components::is_card_file(&path) || name == skip {
            continue;
        }
        std::fs::copy(&path, stage.join(&name)).map_err(|e| {
            SetupError::new(
                "validation_failed",
                format!(
                    "out-dir 里已有的 {} 读不了（{}）",
                    name.to_string_lossy(),
                    e.kind()
                ),
            )
        })?;
    }
    Ok(())
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

/// 新卡要当默认时，目录里不能已经有别的默认卡（规范 §2.12.3：至多一张卡带 `idoris_default`）。新卡不当默认时不查：
/// 目录里本来就有几张默认卡，不是这次加卡造成的。
fn check_single_default(cards: &[ComponentCard], new: &ComponentCard) -> Result<(), SetupError> {
    if !crate::engine_kind::declares_default(new) {
        return Ok(());
    }
    let others: Vec<&str> = cards
        .iter()
        .filter(|card| {
            card.provider.id != new.provider.id && crate::engine_kind::declares_default(card)
        })
        .map(|card| card.provider.id.as_str())
        .collect();
    if others.is_empty() {
        return Ok(());
    }
    Err(SetupError::new(
        "validation_failed",
        format!(
            "out-dir 里已经有默认上游 {}（extensions.idoris_default）：至多一张卡可以是默认，先把那张改掉，或者不加 --default",
            others.join("、")
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
        let err = write_validated(&out, true, &files()).unwrap_err();
        assert_eq!(err.code, "out_dir_not_empty");
        assert_eq!(
            std::fs::read_to_string(out.join("mine.txt")).unwrap(),
            "keep"
        );
        assert_eq!(
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
        replace_managed(&stage, &out, &["omlx.yaml", "subscription.yaml"]).unwrap_err();
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
    fn force_covers_the_names_the_caller_passes_and_no_others() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("omlx.yaml"), "x").unwrap();
        assert!(check_out_dir(tmp.path(), true, None, &["omlx.yaml", "subscription.yaml"]).is_ok());
        let err = check_out_dir(tmp.path(), true, None, &["fakeserve.yaml"]).unwrap_err();
        assert_eq!(err.code, "out_dir_not_empty");
        assert!(err.message.contains("fakeserve.yaml"), "{}", err.message);
        // 没有 --force：任何已有文件都不行。
        let err = check_out_dir(tmp.path(), false, None, &["omlx.yaml"]).unwrap_err();
        assert_eq!(err.code, "out_dir_not_empty");
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
