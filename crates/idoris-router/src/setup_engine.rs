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
//! - `--auto --managed` 找到了外部引擎时取保守的一边：采纳它，不托管（规范 §2.7.8，待 owner 确认）。
//! - `--auto` 遇到要 API key 的候选（没给 key，或给了被拒）：没有别的候选合格时报 `auth_required` 或 `auth_failed`，
//!   带 `--managed` 也一样，不在一个可能是同一种引擎的服务旁边再起一个（规范没写这一种情形，待 owner 确认）。
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
}

impl SetupError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    pub fn to_json(&self) -> Value {
        json!({"status": "error", "code": self.code, "message": self.message})
    }
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

    /// 本机已装的模型 id（离线核对 `--bind` 用）；`None` 表示定位不到模型目录。
    fn installed_models(&self) -> Option<Vec<String>>;

    /// 探测 `endpoint`。`lifecycle` 为真时还要求对端有生命周期状态接口（该引擎的完整形状，而不只是「OpenAI 兼容」）。
    /// 对端要鉴权时：没给 key 是 `auth_required`，给了 key 被拒是 `auth_failed`；不像这个引擎的错误码由引擎自己定。
    fn probe(
        &self,
        endpoint: &str,
        key: Option<&str>,
        lifecycle: bool,
    ) -> impl Future<Output = Result<Probed, SetupError>> + Send;

    /// 引擎卡在 `--out-dir` 里的文件名（如 `omlx.yaml`）。
    fn card_file(&self) -> &'static str;

    /// 引擎卡的模板；端点、加载策略、绑定与托管声明由通用流程填。
    fn card_template(&self) -> Result<ComponentCard, SetupError>;

    /// 与引擎卡一起写进 `--out-dir` 的固定文件（文件名，内容）。
    fn static_files(&self) -> &'static [(&'static str, &'static str)] {
        &[]
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
    let mut files = vec![(engine.card_file(), yaml.as_str())];
    files.extend(engine.static_files().iter().copied());
    let dir = write_validated(&options.out_dir, options.force, &files)?;

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

fn managed_file_names<E: SetupEngine>(engine: &E) -> Vec<&'static str> {
    std::iter::once(engine.card_file())
        .chain(engine.static_files().iter().map(|(name, _)| *name))
        .collect()
}

/// 不碰网络与文件系统写入的前置检查：角色名、out-dir。
fn precheck<E: SetupEngine>(engine: &E, options: &SetupOptions) -> Result<(), SetupError> {
    for (role, _) in &options.bindings {
        if !idoris_policy::ROLES.iter().any(|r| r.as_str() == role) {
            return Err(SetupError::new(
                "invalid_role",
                format!("未知角色 {role:?}"),
            ));
        }
    }
    check_out_dir(
        &options.out_dir,
        options.force,
        None,
        &managed_file_names(engine),
    )
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

/// `--auto`：按引擎给的顺序探测候选端口，取第一个形状是这种引擎的。
/// 要鉴权的候选记下来：没有一个合格时，它们比「没找到」更接近真相。
async fn scan<E: SetupEngine>(engine: &E, api_key: Option<&str>) -> Scan {
    let mut denied: Option<SetupError> = None;
    for port in engine.auto_ports() {
        let endpoint = format!("http://127.0.0.1:{port}");
        match engine.probe(&endpoint, api_key, true).await {
            Ok(probed) => return Scan::Found { endpoint, probed },
            Err(error) if matches!(error.code, "auth_required" | "auth_failed") => {
                if denied.is_none() {
                    let advice = if error.code == "auth_failed" {
                        "拒绝了提供的 API key"
                    } else {
                        "要求 API key，请用 --api-key-stdin 提供"
                    };
                    denied = Some(SetupError::new(
                        error.code,
                        format!(
                            "127.0.0.1:{port} 上的服务{advice}；如果它不是 {}，去掉 --auto，改用 --endpoint 或 --managed",
                            engine.display_name()
                        ),
                    ));
                }
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
    let mut degradation = Vec::new();
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
