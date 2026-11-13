//! idoris.ai/api 计算服务 **S7：本机 / 组织的云端档适配器**（Agent24 → 本机 iDoris → idoris.ai/api 云）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §2（Agent24 永远只连本机 iDoris；本机 iDoris 的**云端档**再去调
//! idoris.ai/api；节点主动调用云，云从不反向访问用户或组织的 Mac；`local_only` 请求一律失败即关闭，用户确认突破 1GB 余量
//! 也不等于授权走云端）、§3（云账号绑定走登录 / 配对；本地数据密钥、云凭证、Admin token 三者分开）、§9（三种形态；
//! 组织与云的对账；容量：本机放不下就走云端档）、§13「Agent24 走本地 → 云端档；`local_only` 不出网」。
//!
//! 这一片直接面对 Agent24 的现有用户和他们的隐私承诺：一处错了，就是本该只在本机处理的数据被送上了云，或者现有 Agent24
//! 用户的行为被悄悄改变。所以隐私和零回归在功能之前。
//!
//! # 默认关闭，没配置时逐字节不变
//!
//! 只有设了 [`CONFIG_ENV`]（`IDORIS_CLOUD_TIER_CONFIG`）才启用。不设：`AppState::cloud_tier` 是 `None`，
//! `chat_completions` 里每个钩子的第一件事就是「没有云端档就原样返回本机已经算好的响应」，不读文件、不开库表、不发请求。
//! 启用之后也**没有任何既有面被改**：`/health`、`/v1/models`、`/capabilities`、Admin v0 看不到云端档——它不是
//! `AppState::cards` 里的一张注册组件，而是一个独立的、只在 `chat_completions` 本机失败之后才被问到的回落通道。
//! 不改 wire 1.1.0、launcher 契约（§2.7）、Admin v0、`deploy_mode` 与 `X-iDoris-Tenant` 契约；`contract_version` 不变。
//!
//! # 什么时候用云端档（决策表）
//!
//! 云端档是**回落通道**：先走本机，本机服务不了，并且下面**每一条**都满足，才去云端。任何一条不满足，调用方拿到的响应
//! 和没有云端档时**逐字节相同**（状态码、错误体、响应头），云端收到 0 个请求。
//!
//! | # | 条件 | 来源 |
//! |---|---|---|
//! | 1 | 云端档已配置、凭证可用（个人：已配对；组织：Node 凭证文件可读） | 本模块 |
//! | 2 | 请求的**有效**隐私类别是 `any`（声明了 `any`，key 允许，且没有被内容检查钉成本地、没有被远程熔断、不是免 key 开发请求）。**没声明 = `local_only`** | 既有契约：隐私只收紧不放宽 |
//! | 3 | 路由策略对这个请求允许 `remote` 层（命中的动作里 `tiers` 有 `remote`），并且不是 `fail_closed` | **既有的**路由策略（`dispatch::policy_cards`） |
//! | 4 | **调用方显式同意**：`X-iDoris-Cloud-Fallback: allow`（见下） | 本模块新增（加法） |
//! | 5 | 不是流式请求 | 本模块（P0 范围：非流式） |
//! | 6 | 本机的失败属于「本机服务不了」：运行时不在线、放不下、没有能接的候选（见 `gate` 模块文档的表） | 本模块 |
//! | 7 | 这个请求有对应的云端模型名（配置的 `model` / `role_models`） | 本模块 |
//! | 8 | 既有的决策管道接受云端档这张合成的卡（`tier: remote`，`privacy_class: any`，`locality: remote`） | **既有的**决策管道 |
//!
//! 不触发云端档的本机结果：`HeadroomConfirmationRequired`（要问用户）、`Busy`（退避重试）、预算 402（商业约束）、
//! 调用方自己的错误（400）、本机自己的 bug。`X-iDoris-Headroom-Breach: confirmed` **不是**出网授权：它只在本机放得下的前提下
//! 让这一次加载动用保留余量；带了它而本机仍然放不下，`local_only` 请求照旧失败，`any` 请求也还要有第 4 条才能出网。
//!
//! ## 同意机制（第 4 条）
//!
//! 请求头 `X-iDoris-Cloud-Fallback: allow`：恰好一个值、恰好是 `allow`（ASCII 不分大小写）；只对这一个请求有效，不记忆。
//! 为什么不用现有的字段：`X-iDoris-Privacy: any` 只说「这份数据允许离开本机」，是策略层的前提，不是对「这次真的要送去云端」的
//! 选择；`X-iDoris-Fallback: next_in_chain` 是预算 / 角色降级的声明；余量确认头是内存的事。数据第一次离开用户的机器，
//! 按「拿不准时选需要显式同意」，必须有它自己的、显式的同意。代价：Agent24 要在用户打开「云端补位」时发这个头
//! （规范 §3 第 2 条写的「云端补位默认关闭」正好对应它）；不发就永远不会用云端档，所以现有的 Agent24 行为完全不变。
//!
//! # 响应
//!
//! 云端档答了：`200`，云端的响应体**原样**；`X-iDoris-Served-Locality: remote`、`X-iDoris-Degraded: true`（本机服务不了才回落，
//! 是降级，按 G-6「不静默」如实回传）、`X-iDoris-Reason: …,CloudTierFallback`，以及新增的可选头 `X-iDoris-Cloud-Record-Id`
//! （云端的请求 id，对账指针）。`X-iDoris-Record-Id` 仍然是本机的。云端的 `X-iDoris-Reason` / `X-iDoris-Tier` /
//! `X-iDoris-Max-Tokens` 以 `X-iDoris-Cloud-Reason` / `-Service-Tier` / `-Max-Tokens` 回传（不和本机的同名头混）。
//! 本机**不记** `X-iDoris-Cost-Minor`，也不写本机用量表：云端成本以云账本为准。
//!
//! 云端档失败（不可达、超时、401、402、403、429、5xx……）：调用方拿到的是**本机原来的失败响应**，只多一个可选头
//! `X-iDoris-Cloud-Attempt: <稳定码>`（数据可能已经离开本机时告诉调用方，不静默）。没有出网的拒绝（没同意、`local_only`……）
//! 连这个头都没有。
//!
//! # 对账与离线
//!
//! 每次出网先在本机库里落一行 `pending`（`idoris-tenancy::cloud_tier`：本机请求 id ↔ 云端 `X-iDoris-Record-Id`，加云端报告的 token
//! 用量；**没有金额、没有正文**），落不下就不出网；回来后恰好一次写成终态。云端不可达时只写一行没有金额的终态：
//! **不在本地预先记账、不增加任何可消费的额度**（表的 CHECK 约束保证没答复的行不可能有金额）。不排队、不重试、不重放：
//! 云端恢复后，离线期间的请求不会被补发。组织形态的 `QUOTA`（本机预算账本）完全不被云端档读写。
//!
//! # 云凭证与三类密钥
//!
//! 见 [`credential`]：本地数据密钥 / 云凭证 / Admin token 三个类型、三个存放位置，互不通用。个人形态用 `idoris cloud pair`
//! 兑换出来的设备凭证（0600 文件），组织形态用运维用云控制面签发的 Node 凭证（0600 文件或环境变量）。配置见 [`config`]。
//!
//! # 对设计稿 / 契约含糊处的理解（没有悄悄拍板，等 owner 确认）
//!
//! 1. **「云端档」不是注册组件**：设计稿说「加一个云端档 provider，和现有远程 provider 一样走现有的路由策略、隐私闸门」。这里把
//!    它做成合成的 `ComponentCard`，**每次回落时**交给既有的 `policy_cards` + 决策管道（而不是放进 `AppState::cards`
//!    让它和本机候选同台竞价）：同台竞价时 `pick` 按「准入、估价、id 字典序」选，云端档的 id 排在 `omlx` 前面，成本也是 0，
//!    会在本机完全正常时抢走请求；而放进 `cards` 还会让 `/health.components`、`/v1/models`、Admin v0 在启用后变样。
//! 2. **同意机制**见上：新增请求头，而不是复用 `X-iDoris-Fallback`。
//! 3. **「金额」**：云端的响应里没有价格，只有 `usage`。本机记 token 数（正好是组织 `QUOTA` 的「token 当量」单位）；金额以云账本为准。
//!    将来云端若在响应里加价格头，表是加法扩展。
//! 4. **「配对的账户 id」**：云端的兑换响应给的是**成员关系 id**（`membership`）和凭证 id，不是付款账户 id（设备凭证没有权限读账户）。
//!    `idoris cloud status` 显示成员关系 id，凭证 id 只显示前缀。
//! 5. **控制面地址**：云入口把 `/control/v1` 挂在公网监听的根上（只在 `control_plane.public: true` 时），所以缺省的控制面地址是
//!    数据面基址的源 + `/control/v1`（`https://idoris.ai/control/v1`），可以在配置里改，但必须同源。
//! 6. **`local_only` 之外的「不允许出网的隐私类别」**：现有契约里隐私类别只有 `local_only` 和 `any`；被内容检查钉成本地的
//!    请求、被远程熔断的请求、没声明隐私的请求，在这里都按 `local_only` 处理（有效隐私类别不是 `any`）。
//! 7. **`Upstream` 错误会触发回落**：适配器把运行时的 400 和 500 折成同一个 `Upstream` 变体，没有类型化的状态码，所以本机运行时
//!    拒绝了一个格式有问题的请求，也会（在有同意的前提下）把同一个请求发去云端，由云端自己判断。没有隐私后果（同意已经给了）。
//! 8. **resident `http_service` 和订阅中转的失败不触发回落**：那两条路径原样透传上游状态码，没有可靠的「本机不可用」信号；
//!    Agent24 用的 oMLX（Supervisor 路径）和 `mlx_lm.server` / `llama.cpp`（同样走 Supervisor）都覆盖了。
//! 9. **没有幂等缓存**：本机的 60 秒幂等缓存（`X-iDoris-Request-Id`）不用于云端档：调用方用同一个请求 id 重试，每次都是一次新的云端请求，
//!    各自一笔账。要幂等就要缓存云端的响应体，那是第二份留存正文的地方，不做。
//! 10. **个人 / 组织的凭证种类**：个人形态只用 `paired_device`，组织形态只用 `node_file` / `node_env`（S0 能力表的「云端认谁」）；
//!     配反了启动失败。存量的 `deploy_mode=tenant`（没声明 `IDORIS_FORM`）部署不启用云端档：那里的租户来自调用方自己写的头。

pub mod cli;
pub mod config;
pub mod credential;
pub mod forward;
pub mod gate;
pub mod pairing;

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use idoris_contracts::ComponentCard;
use idoris_contracts::common::{Capability, FallbackPolicy, PrivacyClass, Tier};
use idoris_contracts::component_card::{Egress, Form};
use idoris_contracts::load_policy::{Admission, Keepalive, LoadMode, LoadPolicy};
use idoris_contracts::provider::{Cost, Family, Locality, ProviderDescriptor};
use idoris_tenancy::cloud_tier::{
    CallFinish, CallOutcome, CallTicket, CloudTierStore, CloudTierStoreError,
};
use serde_json::Value;

use crate::cloud::{Clock, SystemClock};
use crate::connection::RequestLifecycle;
use crate::dispatch::{self, Selected};
use crate::profile::ParsedProfile;
use crate::startup_form::StartupForm;
use crate::{AppState, CompletedEventContext, DispatchedEventContext};

pub use config::{CONFIG_ENV, CloudTierConfig, CredentialSourceConfig};
pub use credential::{
    CloudCredential, CredentialError, CredentialProvider, DeviceStore, LocalSecretProbe,
};
pub use gate::{
    CONSENT_VALUE, Consent, Denial, GateInput, HEADER_CONSENT, LocalFailure, PrivacyFacts,
};

/// 合成的云端档组件卡的 provider id。出现在事件日志（`dispatched` / `completed`）里。
pub const PROVIDER_ID: &str = "idoris-cloud";

/// 新增的可选响应头：云端的 `X-iDoris-Record-Id`（对账指针）。只在云端档答了的响应上出现。
pub const HEADER_CLOUD_RECORD_ID: &str = "X-iDoris-Cloud-Record-Id";
/// 新增的可选响应头：云端档被尝试过但失败了（数据可能已经离开本机），值是稳定码（[`ATTEMPT_CODES`]）。
/// 出现在**本机原来的失败响应**上。
pub const HEADER_CLOUD_ATTEMPT: &str = "X-iDoris-Cloud-Attempt";
/// 云端的 `X-iDoris-Reason`，以这个名字回传（不和本机的同名头混）。
pub const HEADER_CLOUD_REASON: &str = "X-iDoris-Cloud-Reason";
/// 云端的 `X-iDoris-Tier`（normal / degraded / grace），以这个名字回传。
pub const HEADER_CLOUD_SERVICE_TIER: &str = "X-iDoris-Cloud-Service-Tier";
/// 云端的 `X-iDoris-Max-Tokens`（云端压低了 `max_tokens`，回答可能被截断），以这个名字回传。
pub const HEADER_CLOUD_MAX_TOKENS: &str = "X-iDoris-Cloud-Max-Tokens";

/// `X-iDoris-Cloud-Attempt` 的全部取值（稳定字符串，只能加不能改）：调用记录的结局（没答复 / 答了但失败的那些），
/// 加上没出网的几种内部原因。
pub const ATTEMPT_CODES: [&str; 15] = [
    "not_paired",
    "credential_unavailable",
    "saturated",
    "audit_unavailable",
    "store_unavailable",
    "unreachable",
    "timeout",
    "cancelled",
    "unauthorized",
    "forbidden",
    "budget_exhausted",
    "rate_limited",
    "rejected",
    "cloud_error",
    "invalid_response",
];

/// 注入一个 [`CloudTier`] 需要的全部部件（生产装配和测试都走它）。
pub struct CloudTierParts {
    pub config: CloudTierConfig,
    pub store: Arc<CloudTierStore>,
    pub provider: Arc<dyn CredentialProvider>,
    pub http: reqwest::Client,
    pub clock: Arc<dyn Clock>,
}

/// 云端档：配置、凭证来源、HTTP 客户端、调用记录、合成的组件卡。挂在 `AppState::cloud_tier` 上。
pub struct CloudTier {
    config: CloudTierConfig,
    store: Arc<CloudTierStore>,
    provider: Arc<dyn CredentialProvider>,
    http: reqwest::Client,
    clock: Arc<dyn Clock>,
    card: ComponentCard,
    slots: Arc<tokio::sync::Semaphore>,
}

impl std::fmt::Debug for CloudTier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudTier")
            .field("origin", &self.config.origin())
            .finish_non_exhaustive()
    }
}

impl CloudTier {
    pub fn new(parts: CloudTierParts) -> Result<Self, String> {
        let card = build_card(&parts.config)?;
        let slots = Arc::new(tokio::sync::Semaphore::new(parts.config.max_concurrent));
        Ok(Self {
            config: parts.config,
            store: parts.store,
            provider: parts.provider,
            http: parts.http,
            clock: parts.clock,
            card,
            slots,
        })
    }

    pub fn config(&self) -> &CloudTierConfig {
        &self.config
    }

    /// 调用记录（测试和对账方读）。
    pub fn store(&self) -> &Arc<CloudTierStore> {
        &self.store
    }

    pub fn provider(&self) -> &Arc<dyn CredentialProvider> {
        &self.provider
    }

    /// 合成的云端档组件卡（测试读）。
    pub fn card(&self) -> &ComponentCard {
        &self.card
    }

    fn now_ms(&self) -> i64 {
        self.clock.now().0
    }

    /// 启动用的入口：读环境变量 [`CONFIG_ENV`]。没设 = 关闭（`Ok(None)`，什么都不碰）。
    pub fn from_env_for_form(
        startup: &StartupForm,
        probe: Option<Arc<dyn LocalSecretProbe>>,
    ) -> Result<Option<Arc<Self>>, String> {
        let path = crate::config::optional_env_path(CONFIG_ENV)?;
        Self::resolve_for_form(
            startup,
            path,
            crate::storage::process_db_path,
            &|name| std::env::var(name).ok(),
            probe,
        )
    }

    /// [`Self::from_env_for_form`] 的可测版本：配置路径、数据库路径、环境变量读取由调用方给。
    ///
    /// 先看形态、后碰文件：不允许的形态下设了开关是启动错误（不读任何东西）。**启动时不发任何网络请求**
    /// （出网断言：启动后、处理任何请求前，零出网）。
    pub fn resolve_for_form(
        startup: &StartupForm,
        config_path: Option<PathBuf>,
        db_path: impl FnOnce() -> Result<PathBuf, String>,
        env: &dyn Fn(&str) -> Option<String>,
        probe: Option<Arc<dyn LocalSecretProbe>>,
    ) -> Result<Option<Arc<Self>>, String> {
        let Some(path) = config_path else {
            return Ok(None);
        };
        let form = startup.form();
        if !form.profile().uses_cloud_tier {
            return Err(format!(
                "{CONFIG_ENV} 不能在 {} 形态下启用：云端档属于个人和组织形态（节点主动调用云，云不会调用自己）",
                form.as_str()
            ));
        }
        if !startup.is_declared() && startup.deploy_mode() == idoris_contracts::DeployMode::Tenant {
            return Err(format!(
                "{CONFIG_ENV} 不能在存量的 tenant 部署（没有声明 IDORIS_FORM）下启用：那里的租户来自调用方自己写的头，\
                 不能决定谁的数据可以出网。组织请设置 IDORIS_FORM=organization"
            ));
        }
        let config = CloudTierConfig::load(&path)?;
        let db = db_path()?;
        let personal = form == idoris_compute_contracts::form::DeploymentForm::Personal;
        let provider: Arc<dyn CredentialProvider> = match (&config.credential, personal) {
            (CredentialSourceConfig::PairedDevice, true) => {
                Arc::new(credential::PairedDeviceProvider::new(
                    DeviceStore::new(config.state_dir_for(&db)),
                    config.origin(),
                    probe,
                ))
            }
            (CredentialSourceConfig::PairedDevice, false) => {
                return Err(
                    "组织形态用 Node 凭证（credential.source: node_file / node_env），不用个人设备配对：\
                     云端只认组织付款账户，由运维用云控制面签发受限凭证"
                        .to_string(),
                );
            }
            (CredentialSourceConfig::NodeFile(file), false) => {
                let provider = credential::NodeFileProvider::new(file.clone(), probe);
                fail_fast(&provider, "Node 凭证文件")?;
                Arc::new(provider)
            }
            (CredentialSourceConfig::NodeEnv(name), false) => {
                let provider = credential::NodeEnvProvider::new(env(name), probe);
                fail_fast(&provider, &format!("环境变量 {name}"))?;
                Arc::new(provider)
            }
            (CredentialSourceConfig::NodeFile(_) | CredentialSourceConfig::NodeEnv(_), true) => {
                return Err(
                    "个人形态用 idoris cloud pair 兑换出来的设备凭证（credential.source: paired_device），\
                     不用 Node 凭证：Node 凭证属于组织付款账户"
                        .to_string(),
                );
            }
        };
        if personal && let Some(warning) = device_file_warning(&provider) {
            eprintln!("[idoris] 警告：{warning}");
        }
        if let Some(parent) = db.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建数据库目录 \"{}\"：{error}", parent.display()))?;
        }
        let store = if startup.connection_profile()
            == idoris_tenancy::sqlite_conn::ConnectionProfile::Standard
        {
            let conn = idoris_tenancy::sqlite_conn::open_standard(&db)
                .map_err(|error| format!("无法设置数据库连接 \"{}\"：{error}", db.display()))?;
            CloudTierStore::new(conn)
        } else {
            CloudTierStore::open(&db)
        }
        .map_err(|error| format!("无法打开云端档调用记录 \"{}\"：{error}", db.display()))?;
        let clock: Arc<dyn Clock> = Arc::new(SystemClock);
        let interrupted = store
            .recover_interrupted(clock.now().0)
            .map_err(|error| format!("无法整理上次崩溃留下的云端档调用记录：{error}"))?;
        if interrupted > 0 {
            eprintln!(
                "[idoris] 云端档：上次进程崩溃时有 {interrupted} 次调用没有结局，已标记为 interrupted（请求可能已经到达云端，对账时需要人工核对）"
            );
        }
        let http = idoris_upstream::cloud::build_http_client(config.connect_timeout)
            .map_err(|error| format!("无法构造云端档的 HTTP 客户端：{error}"))?;
        let tier = Self::new(CloudTierParts {
            config,
            store: Arc::new(store),
            provider,
            http,
            clock,
        })?;
        Ok(Some(Arc::new(tier)))
    }

    /// 第 2 层闸门：把合成的卡交给**既有的**路由策略和决策管道。
    fn select(&self, scope: &FallbackScope<'_>) -> Result<Selected, Denial> {
        let routed = dispatch::policy_cards(
            std::slice::from_ref(&self.card),
            &scope.state.routing_policy,
            scope.parsed,
        );
        if routed.cards.is_empty() {
            return Err(Denial::PolicyForbidsRemote);
        }
        if routed.route.fail_closed {
            return Err(Denial::PolicyFailClosed);
        }
        dispatch::select_with_gate_snapshot_and_completion(
            &routed.cards,
            scope.parsed,
            scope.prompt,
            None,
            &BTreeSet::new(),
            &[false],
            scope.completion_tokens,
            scope.content_tightened,
        )
        .map_err(|_| Denial::PipelineRefused)
    }

    /// 要发给云端的请求体：调用方的请求体，`model` 换成云端模型名，再过一遍既有的身份字段清洗（`proxy::decorrelate`，
    /// 和发往任何非 loopback 提供方的请求体同一个）。本机的模型名和角色别名不出网。
    fn cloud_body(&self, request: &Value, cloud_model: &str) -> Value {
        let mut body = request.clone();
        body["model"] = Value::String(cloud_model.to_string());
        crate::proxy::decorrelate(&body)
    }

    async fn load_credential(&self) -> Result<credential::LoadedCredential, CredentialError> {
        let provider = self.provider.clone();
        tokio::task::spawn_blocking(move || provider.load())
            .await
            .unwrap_or(Err(CredentialError::Unreadable))
    }

    /// 先落一行 `pending`，落不下返回 `None`（调用方不出网）。返回的 [`PendingCall`] 看守着这一行：
    /// 在它被 [`PendingCall::disarm`] 之前被丢掉（调用方挂断，整个处理 future 被丢），这一行会被写成 `cancelled`。
    ///
    /// 写库在 `spawn_blocking` 里做；「行已经落下」和「看守已经存在」之间没有任何让出点：如果调用方正好在写库时挂断
    /// （接收端已经没了），看守在阻塞线程里当场被丢掉，同样把这一行写成 `cancelled`。
    async fn record_begin(&self, local_request_id: &str) -> Option<PendingCall> {
        let store = self.store.clone();
        let clock = self.clock.clone();
        let id = local_request_id.to_string();
        let now = self.now_ms();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        tokio::task::spawn_blocking(move || match store.begin_call(&id, 1, now) {
            Ok(ticket) => {
                // `Err(guard)`（接收端已经被丢掉）：guard 在这里被 drop，把刚落下的行写成 cancelled
                let _ = sender.send(PendingCall::armed(store, clock, ticket));
            }
            Err(error) => eprintln!("[idoris] 云端档：无法记录出网调用，不发送：{error}"),
        });
        receiver.await.ok()
    }

    async fn record_finish(
        &self,
        ticket: CallTicket,
        finish: OwnedFinish,
    ) -> Result<(), CloudTierStoreError> {
        let store = self.store.clone();
        let result = tokio::task::spawn_blocking(move || {
            store.finish_call(
                &ticket,
                &CallFinish {
                    outcome: finish.outcome,
                    finished_at_ms: finish.finished_at_ms,
                    cloud_record_id: finish.cloud_record_id.as_deref(),
                    http_status: finish.http_status,
                    cloud_reason: finish.cloud_reason.as_deref(),
                    tokens_in: finish.tokens_in,
                    tokens_out: finish.tokens_out,
                },
            )
        })
        .await;
        match result {
            Ok(outcome) => outcome,
            Err(_) => Err(CloudTierStoreError::Corrupt(
                "record worker failed".to_string(),
            )),
        }
    }
}

/// `finish_call` 要写的内容（拥有所有权，好送进 `spawn_blocking`）。
struct OwnedFinish {
    outcome: CallOutcome,
    finished_at_ms: i64,
    cloud_record_id: Option<String>,
    http_status: Option<u16>,
    cloud_reason: Option<String>,
    tokens_in: Option<u64>,
    tokens_out: Option<u64>,
}

/// 一行已经落下、还没有终态的 `pending` 调用记录的看守。
///
/// 调用方在回落途中挂断时，hyper 会把整个处理 future 丢掉（本机路径里 `ReservationGuard` / `CancelOnDrop` 应对的是同一件事）。
/// 没有这个看守，那一行会一直是 `pending`：对账游标不能越过 `pending` 行，要等到下次启动才会被标成 `interrupted`。
/// 出网的请求随 future 一起被丢掉（连接关闭），所以这时的结局是 `cancelled`：出网了、没有答复、没有金额。
///
/// 正常路径自己写终态：先 [`Self::disarm`]（之后 drop 什么都不做），紧接着（中间没有任何让出点）调 `record_finish`。
struct PendingCall {
    store: Arc<CloudTierStore>,
    clock: Arc<dyn Clock>,
    ticket: CallTicket,
    armed: bool,
}

impl PendingCall {
    fn armed(store: Arc<CloudTierStore>, clock: Arc<dyn Clock>, ticket: CallTicket) -> Self {
        Self {
            store,
            clock,
            ticket,
            armed: true,
        }
    }

    /// 拿回凭条，自己来写终态。
    fn disarm(&mut self) -> CallTicket {
        self.armed = false;
        self.ticket.clone()
    }
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let finish = CallFinish::without_answer(CallOutcome::Cancelled, self.clock.now().0);
        match self.store.finish_call(&self.ticket, &finish) {
            Ok(()) | Err(CloudTierStoreError::AlreadyFinal(_)) => {}
            Err(error) => eprintln!(
                "[idoris] 云端档：调用方挂断后无法把调用记录写成 cancelled（{error}），这一行会在下次启动时被标成 interrupted"
            ),
        }
    }
}

fn fail_fast(provider: &dyn CredentialProvider, what: &str) -> Result<(), String> {
    provider
        .load()
        .map(|_| ())
        .map_err(|error| format!("{what}不可用：{error}"))
}

/// 个人形态启动时的提醒：设备凭证文件在，但不可用（权限、格式、签发源不一致）。没有文件（还没配对）不是问题。
fn device_file_warning(provider: &Arc<dyn CredentialProvider>) -> Option<String> {
    match provider.load() {
        Ok(_) | Err(CredentialError::NotPaired) => None,
        Err(error) => Some(format!("云端档的设备凭证不可用，回落不会发生：{error}")),
    }
}

/// 合成云端档的组件卡：`tier: remote`、`privacy_class: any`、`locality: remote`、出站 `internet`。
/// 它**永远不会**被放进 `AppState::cards`，只在回落时交给既有的路由策略和决策管道（见模块文档的理解 1）。
///
/// `idoris_role_models` 只用来声明「这张卡能接哪些角色」（决策管道按它筛角色）；它的值是占位符：真正发往云端的模型名在
/// [`CloudTierConfig::cloud_model_for`]，因为云端的路由名（`idoris/agent`）长得像本机的角色别名，
/// 放进 `idoris_role_models` 会被它的校验拒绝。
fn build_card(config: &CloudTierConfig) -> Result<ComponentCard, String> {
    let mut role_models = serde_json::Map::new();
    for role in config.role_models.keys() {
        role_models.insert(role.clone(), Value::String("cloud".to_string()));
    }
    let mut extensions = serde_json::Map::new();
    extensions.insert(
        "_degradation".to_string(),
        Value::String("云端档失败时回落到本机原来的失败响应，不使用任何云端特有的特性".to_string()),
    );
    extensions.insert("idoris_role_models".to_string(), Value::Object(role_models));
    let card = ComponentCard {
        provider: ProviderDescriptor {
            id: PROVIDER_ID.to_string(),
            family: Family::Idoris,
            tier: Tier::Remote,
            capabilities: vec![Capability::Chat, Capability::Reasoning, Capability::Coding],
            privacy_class: PrivacyClass::Any,
            // 云端成本以云账本为准；本机预算（QUOTA）不读写云端档。
            cost: Cost {
                input_per_m: 0.0,
                output_per_m: 0.0,
            },
            locality: Locality::Remote,
            extensions: None,
        },
        form: Form::HttpService,
        endpoint: config.base_url.as_str().to_string(),
        version_pin: format!("cloud-tier@{}", env!("CARGO_PKG_VERSION")),
        privacy_class: PrivacyClass::Any,
        allowed_egress: vec![Egress::Internet],
        fallback_policy: FallbackPolicy::FailClosed,
        fail_closed: true,
        load_policy: Some(LoadPolicy {
            mode: LoadMode::Resident,
            keepalive: Keepalive::Pinned { pinned: true },
            admission: Admission::Coexist,
        }),
        extensions: Some(extensions),
    };
    use idoris_contracts::Contract;
    card.validate()
        .map_err(|error| format!("云端档的合成组件卡无效（不应该发生）：{error}"))?;
    let wrapped = idoris_policy::Card {
        component: card.clone(),
        roles: Vec::new(),
        experiment: false,
        min_ram_gb: 0.0,
        estimated_cost_minor: Some(0),
        admission_status: idoris_policy::AdmissionStatus::Ready,
    };
    idoris_policy::validate_registration(&[wrapped])
        .map_err(|error| format!("云端档的合成组件卡没有通过注册校验（不应该发生）：{error}"))?;
    Ok(card)
}

// ------------------------------------------------------------------------------------------------
// chat_completions 里的钩子
// ------------------------------------------------------------------------------------------------

/// `chat_completions` 在本机失败之后问云端档所需要的全部上下文。借用，不拥有；**只有配置了云端档才会构造**。
pub(crate) struct FallbackScope<'a> {
    state: &'a AppState,
    tier: &'a Arc<CloudTier>,
    headers: &'a HeaderMap,
    body: &'a Value,
    parsed: &'a ParsedProfile,
    /// 内容检查写回之后的有效隐私类别（请求进来时就定了，之后不会变）。
    effective_privacy: Option<PrivacyClass>,
    /// 内容检查把这个请求钉成了本地（同上，请求进来时就定了）。
    pinned_local_by_content: bool,
    content_tightened: bool,
    prompt: &'a str,
    completion_tokens: u64,
    record_id: &'a str,
    lifecycle: &'a RequestLifecycle,
    dev_no_key: bool,
    dispatched: Option<&'a DispatchedEventContext>,
    completed: Option<&'a CompletedEventContext>,
}

/// [`FallbackScope::new`] 的输入。
pub(crate) struct ScopeInputs<'a> {
    pub state: &'a AppState,
    pub headers: &'a HeaderMap,
    pub body: &'a Value,
    pub parsed: &'a ParsedProfile,
    /// 内容检查把请求钉成了本地（收紧、失败、不可检查）。
    pub pinned_local_by_content: bool,
    /// 决策管道看到的「内容收紧」标志（和主路径一致）。
    pub content_tightened: bool,
    pub prompt: &'a str,
    pub completion_tokens: u64,
    pub record_id: &'a str,
    pub lifecycle: &'a RequestLifecycle,
    pub dev_no_key: bool,
    pub dispatched: Option<&'a DispatchedEventContext>,
    pub completed: Option<&'a CompletedEventContext>,
}

impl<'a> FallbackScope<'a> {
    /// 没有配置云端档就是 `None`：钩子立刻原样返回本机的响应，什么都不做。
    pub(crate) fn new(inputs: ScopeInputs<'a>) -> Option<Self> {
        let tier = inputs.state.cloud_tier.as_ref()?;
        Some(Self {
            state: inputs.state,
            tier,
            headers: inputs.headers,
            body: inputs.body,
            parsed: inputs.parsed,
            effective_privacy: inputs.parsed.task.privacy,
            pinned_local_by_content: inputs.pinned_local_by_content,
            content_tightened: inputs.content_tightened,
            prompt: inputs.prompt,
            completion_tokens: inputs.completion_tokens,
            record_id: inputs.record_id,
            lifecycle: inputs.lifecycle,
            dev_no_key: inputs.dev_no_key,
            dispatched: inputs.dispatched,
            completed: inputs.completed,
        })
    }

    /// 远程熔断开关**此刻**的值。熔断是个共享的原子变量，随时可能被打开：每一次要做「能不能出网」的判断都现读，不留快照。
    /// （以前这里在请求进来时读一次存起来，回落时直接用：请求以 `any` + 同意进来、本机处理期间熔断被打开、本机随后失败，
    /// 回落仍然会把正文发往云端——审查 r4 High#1。）
    fn kill_switch_on(&self) -> bool {
        self.state
            .remote_kill_switch
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 闸门的隐私事实：隐私类别和内容检查的结论请求进来时就定了；熔断现读。
    fn privacy_facts(&self) -> PrivacyFacts {
        PrivacyFacts {
            effective: self.effective_privacy,
            pinned_local_by_content: self.pinned_local_by_content,
            kill_switch: self.kill_switch_on(),
        }
    }

    fn role_name(&self) -> Option<&'static str> {
        self.parsed
            .role
            .filter(|role| role.is_catalog_role())
            .map(idoris_policy::Role::as_str)
    }

    fn wants_stream(&self) -> bool {
        self.body
            .get("stream")
            .and_then(Value::as_bool)
            .unwrap_or(false)
    }
}

/// 在本机失败之后问云端档。**唯一**的入口。
///
/// - 没有云端档（`scope` 是 `None`）、闸门拒绝（`local_only`、没同意……）：原样返回 `original`，一个字节都不改；
/// - 通过闸门、云端档答了：返回云端的回答；
/// - 通过闸门、但没能拿到回答：返回 `original`，只多一个 `X-iDoris-Cloud-Attempt` 头。
pub(crate) async fn fallback(
    scope: Option<&FallbackScope<'_>>,
    failure: LocalFailure<'_>,
    original: Response,
) -> Response {
    let Some(scope) = scope else {
        return original;
    };
    let tier = scope.tier;
    let cloud_model = tier.config.cloud_model_for(scope.role_name());
    let input = GateInput {
        privacy: scope.privacy_facts(),
        dev_no_key: scope.dev_no_key,
        consent: gate::consent(scope.headers),
        stream: scope.wants_stream(),
        failure,
        cloud_model,
    };
    let Ok((permit, selected)) = gate::grant(&input, || tier.select(scope)) else {
        return original;
    };
    let Some(cloud_model) = cloud_model else {
        // `grant` 已经保证有；这里只是不用 unwrap
        return original;
    };

    // ---- 出网之前的准备：任何一步失败都不出网 ----
    let loaded = match tier.load_credential().await {
        Ok(loaded) => loaded,
        Err(CredentialError::NotPaired) => return annotate(original, "not_paired"),
        Err(error) => {
            eprintln!("[idoris] 云端档：云凭证不可用（{}），不发送", error.code());
            return annotate(original, "credential_unavailable");
        }
    };
    let Ok(_slot) = tier.slots.clone().try_acquire_owned() else {
        return annotate(original, "saturated");
    };
    // 出网前的第一次复核：闸门放行之后，读凭证、占名额期间远程熔断可能已经被打开。还没有任何副作用（没写审计事件、
    // 没落 pending 行），所以这里拦下来什么都不留；调用方拿到的和没有云端档时逐字节相同。
    if scope.kill_switch_on() {
        return original;
    }
    let body = tier.cloud_body(scope.body, cloud_model);
    if let Some(context) = scope.dispatched
        && crate::append_dispatched(context, &selected).await.is_err()
    {
        return annotate(original, "audit_unavailable");
    }
    let Some(mut pending) = tier.record_begin(scope.record_id).await else {
        // 前面的 dispatched 事件已经写了：补一个 completed（失败），事件流不留一个有头无尾的出网尝试
        complete_event(scope, &selected, "failure").await;
        return annotate(original, "store_unavailable");
    };

    // ---- 出网（一次，不重试）----
    // Not `cancellation_token()`: that one is cancelled when the local dispatch finishes (`dispatch::CancelOnDrop`),
    // which is already the case here. This sibling fires only when the caller's connection goes away.
    let cancel = scope.lifecycle.fallback_cancellation_token();
    let result = forward::send(
        &tier.http,
        &tier.config,
        permit,
        &loaded.secret,
        &body,
        &cancel,
        // 出网边界上的最后一道门：写审计事件、落 pending 行又隔了几个让出点，熔断可能在这期间被打开
        &|| !scope.kill_switch_on(),
    )
    .await;
    drop(loaded);
    // 这一步之前被丢掉（调用方挂断）：`pending` 把行写成 cancelled。从这里起终态由下面的 `record_finish` 写；
    // 它在第一次被 poll 时就把写库交给了阻塞线程池，之后即使 future 被丢掉，写库也会完成。
    let ticket = pending.disarm();

    match result {
        Ok(answer) => {
            let finish = OwnedFinish {
                outcome: CallOutcome::Served,
                finished_at_ms: tier.now_ms(),
                cloud_record_id: Some(answer.cloud_record_id.clone()),
                http_status: Some(answer.status),
                cloud_reason: answer.reason.clone(),
                tokens_in: answer.tokens_in,
                tokens_out: answer.tokens_out,
            };
            if let Err(error) = tier.record_finish(ticket.clone(), finish).await {
                // 同一个云端 record id 已经对应了别的本机请求：这个答复不可信，不交给调用方。
                if matches!(error, CloudTierStoreError::CloudRecordConflict) {
                    let retry = OwnedFinish {
                        outcome: CallOutcome::InvalidResponse,
                        finished_at_ms: tier.now_ms(),
                        cloud_record_id: None,
                        http_status: Some(answer.status),
                        cloud_reason: None,
                        tokens_in: None,
                        tokens_out: None,
                    };
                    let _ = tier.record_finish(ticket, retry).await;
                    complete_event(scope, &selected, "failure").await;
                    return annotate(original, "invalid_response");
                }
                // 记录终态失败（数据库出了问题）：回答已经拿到、数据已经出去了，交给调用方；pending 行启动时会被标成 interrupted。
                eprintln!("[idoris] 云端档：无法写入调用结局（{error}），回答照常交付");
            }
            complete_event(scope, &selected, "success").await;
            served_response(&selected, answer)
        }
        // 最后一道门把它拦下了：**没有任何字节离开本机**。行已经落下了（先写后出网），写成 cancelled（没有答复、没有云端 id、
        // 没有金额）；事件流补一个失败的 completed。调用方拿到本机原来的失败响应，一个字节都不改，连 X-iDoris-Cloud-Attempt
        // 都没有——那个头的意思是「数据可能已经离开本机」，这里没有。
        Err(forward::CloudFailure::Withheld) => {
            let withheld = OwnedFinish {
                outcome: CallOutcome::Cancelled,
                finished_at_ms: tier.now_ms(),
                cloud_record_id: None,
                http_status: None,
                cloud_reason: None,
                tokens_in: None,
                tokens_out: None,
            };
            if let Err(error) = tier.record_finish(ticket, withheld).await {
                eprintln!("[idoris] 云端档：熔断拦下了发送，但无法写入调用结局（{error}）");
            }
            complete_event(scope, &selected, "failure").await;
            original
        }
        Err(failure) => {
            let outcome = failure.outcome();
            let (status, reason, cloud_id) = match &failure {
                forward::CloudFailure::Answered {
                    status,
                    reason,
                    cloud_record_id,
                    ..
                } => (Some(*status), reason.clone(), cloud_record_id.clone()),
                _ => (None, None, None),
            };
            let mut finish = OwnedFinish {
                outcome,
                finished_at_ms: tier.now_ms(),
                cloud_record_id: cloud_id,
                http_status: status,
                cloud_reason: reason,
                tokens_in: None,
                tokens_out: None,
            };
            if let Err(error) = tier
                .record_finish(ticket.clone(), finish_clone(&finish))
                .await
            {
                // 云端 id 冲突之类：丢掉云端 id 再记一次，结局本身一定要落下来
                eprintln!("[idoris] 云端档：调用结局写入失败（{error}），去掉云端 id 重记");
                finish.cloud_record_id = None;
                let _ = tier.record_finish(ticket, finish).await;
            }
            eprintln!(
                "[idoris] 云端档：这次回落没有拿到回答（{}），调用方得到本机原来的失败响应",
                outcome.as_str()
            );
            complete_event(scope, &selected, "failure").await;
            annotate(original, outcome.as_str())
        }
    }
}

fn finish_clone(finish: &OwnedFinish) -> OwnedFinish {
    OwnedFinish {
        outcome: finish.outcome,
        finished_at_ms: finish.finished_at_ms,
        cloud_record_id: finish.cloud_record_id.clone(),
        http_status: finish.http_status,
        cloud_reason: finish.cloud_reason.clone(),
        tokens_in: finish.tokens_in,
        tokens_out: finish.tokens_out,
    }
}

async fn complete_event(scope: &FallbackScope<'_>, selected: &Selected, status: &'static str) {
    if let Some(context) = scope.completed
        && crate::append_completed(context, selected, status)
            .await
            .is_err()
    {
        eprintln!("[idoris] 云端档：completed 事件写入失败（回答照常交付）");
    }
}

/// 在失败响应上加 `X-iDoris-Cloud-Attempt`（只在数据可能已经离开本机、或者本该回落却没能回落时）。
fn annotate(mut response: Response, code: &'static str) -> Response {
    debug_assert!(ATTEMPT_CODES.contains(&code), "{code}");
    response.headers_mut().insert(
        HeaderName::from_static("x-idoris-cloud-attempt"),
        HeaderValue::from_static(code),
    );
    response
}

/// 云端档答了：云端的响应体原样，加上落点、降级和对账头。
fn served_response(selected: &Selected, answer: forward::CloudAnswer) -> Response {
    let mut response = (StatusCode::OK, answer.body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    // 推理实际发生在云端：如实回传（规范 §2.5）。`remote` 让 Agent24 的 Served-Locality 绊线对 local_only 调用方照旧生效。
    if let Ok(value) = HeaderValue::from_str(crate::locality_str(selected.served_locality)) {
        headers.insert(crate::HEADER_SERVED_LOCALITY, value);
    }
    // 本机服务不了才回落：这是降级（G-6 不静默）。
    headers.insert(crate::HEADER_DEGRADED, HeaderValue::from_static("true"));
    let reasons = dispatch::reason_header_value(&selected.decision.reason_codes);
    let reason = if reasons.is_empty() {
        "CloudTierFallback".to_string()
    } else {
        format!("{reasons},CloudTierFallback")
    };
    if let Ok(value) = HeaderValue::from_str(&reason) {
        headers.insert(crate::HEADER_REASON, value);
    }
    if let Ok(value) = HeaderValue::from_str(&answer.cloud_record_id) {
        headers.insert(HeaderName::from_static("x-idoris-cloud-record-id"), value);
    }
    for (name, value) in [
        ("x-idoris-cloud-reason", answer.reason.as_deref()),
        (
            "x-idoris-cloud-service-tier",
            answer.service_tier.as_deref(),
        ),
        ("x-idoris-cloud-max-tokens", answer.max_tokens.as_deref()),
    ] {
        if let Some(value) = value
            && let Ok(value) = HeaderValue::from_str(value)
        {
            headers.insert(HeaderName::from_static(name), value);
        }
    }
    // 没有 UsageFact：云端成本以云账本为准，本机用量表只记本机推理。
    response
}

/// 本机虚拟 key 库做的 [`LocalSecretProbe`]：这个密钥是不是本机一把有效的本地数据密钥。
/// **判不了（库不可用）按「是」处理**：宁可拒绝一次回落，也不把可能是本地密钥的东西发出去。
pub struct VirtualKeyProbe(
    pub Arc<std::sync::Mutex<idoris_tenancy::virtual_key::store::VirtualKeyStore>>,
);

impl LocalSecretProbe for VirtualKeyProbe {
    fn is_local_secret(&self, secret: &str) -> bool {
        let Ok(parsed) = idoris_tenancy::virtual_key::VirtualKeySecret::parse(secret) else {
            return false;
        };
        let now = SystemClock.now().0;
        match self.0.lock() {
            Ok(store) => store
                .authenticate(&parsed, now)
                .map_or(true, |found| found.is_some()),
            Err(_) => true,
        }
    }
}

#[cfg(test)]
mod tests;
