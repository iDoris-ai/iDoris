//! idoris.ai/api 计算服务 **S5：云入口**（路由层）。
//!
//! 设计依据：`docs/design/compute-service-design.md` §2（部署拓扑）、§3（付款上下文由凭证决定）、§9 第 3 条与
//! 「数据面的租户」、§12 的 S5 一行、§13 的 P0 验收前两条。这是**公网入口**：一处错了，就是别人能冒用付款上下文，
//! 或者一个学校的共享出口把整个班误伤。所以每一条规则都落在一个有测试钉住的纯函数或小模块里：
//!
//! | 模块 | 内容 |
//! |---|---|
//! | [`config`] | 云入口配置（YAML）：受信反向代理、各级上限、防滥用、CORS、控制面是否对公网开放、**每个租户的数据处理要求** |
//! | [`net`] | 客户端来源：只信任配置里列出的反向代理的 `X-Forwarded-For`，其它来源的一律不信 |
//! | [`caller`] | 从凭证推导付款上下文、租户、数据类别（成员 id 逐次验证，失败不回落） |
//! | [`limits`] | 平台 / 租户 / 成员三级的请求速率与并发上限（和 S3 的花费闸门互补） |
//! | [`abuse`] | 公网防滥用：没有凭证请求的限流、按凭证前缀的认证失败计数；来源地址只是辅助信号 |
//! | [`bucket`] | 令牌桶（精确整数、时钟可拨）和有上限的键值表 |
//! | [`cors`] | 跨域：默认不开放，开放时只对配置里列出的来源、暴露 S3 的 `EXPOSED_HEADERS` |
//! | [`bind`] | 云形态的公网监听：只有云形态允许非 loopback；Admin v0 永远只听 loopback |
//! | `guard` | 中间件：一个请求依次要过的门 |
//! | [`reason`] | 入口自己的稳定原因码 |
//!
//! # 只在云形态加载
//!
//! 云入口只有在 `IDORIS_FORM=cloud` 时才会被构造（`AppState::cloud_ingress` 为 `Some`），此时 [`crate::build_app`]
//! 交给 [`build_cloud_app`]，整个数据面换成下面的路由表。个人形态、组织形态、存量 tenant 部署永远走原来的 `build_app`，
//! 一个字符都没变。本地租户模式的 `X-iDoris-Tenant` 契约不变。
//!
//! # 路由表（云形态）
//!
//! | 方法 + 路径 | 谁 | 认证 |
//! |---|---|---|
//! | `GET /health` | 既有的健康检查（不变） | 无 |
//! | `GET /api/v1/models` | 云路由簿里的模型名（OpenAI 的 `{"object":"list","data":[…]}`） | 凭证 |
//! | `POST /api/v1/chat/completions` | S4 的云处理函数 | 凭证 |
//! | `/control/v1/*` | S2 控制面——**只有**配置里 `control_plane.public: true` 才挂 | 控制面自己（凭证 + 授权） |
//! | 其它一切 | 404 `{"error":{"type":"not_found"}}`，不认证、不碰库 | — |
//!
//! 本地流水线的路由（`/v1/*`、`/capabilities`、`/idoris/tenants/*`、`/admin/*`）在云形态下**不存在**：
//! 它们是本地形态的契约，要么依赖 `X-iDoris-Tenant`，要么暴露本机信息。不保留 `/v1/*` 的理由：
//! 公开的基址是 `https://idoris.ai/api/v1`，普通 OpenAI 客户端把它当 `base_url` 用；再留一套 `/v1/*` 就是两个入口、
//! 两份要守的规则，而且 `/v1/*` 在本地形态里的含义（租户头、本地模型）在云上是错的。反向代理必须把 `/api/*` 原样转发
//! （不剥前缀）。
//!
//! # 一个请求的付款上下文怎么来的
//!
//! 见 [`caller`] 的模块文档。要点：凭证决定付款人；**Service 凭证**（eduai 后端）每个请求必须带 `X-iDoris-Member`
//! （[`HEADER_MEMBER`]）提交假名成员 id，用 `AccountStore::payment_context` 逐次验证它在这张凭证的范围内，
//! 失败一律拒绝；**成员凭证**不用带（带了必须是自己）；**节点凭证**不能带。租户和数据类别永远从付款账户推导，
//! 请求没有任何办法指定。
//!
//! # 交接给 S6 / S7
//!
//! - **S6（儿童安全）**：处理函数能从请求扩展里取到两样东西。[`crate::cloud::CallerPolicy`]（和 S4 一样）——
//!   `caller.tenant` 就是租户的 `TenantDataPolicy`，`caller.tenant.handles_child_data()` 是「这是不是 `child_edu`
//!   租户」，`caller.tenant.data_classes()` 是完整的类别集合，来源是云入口配置里该租户的声明；以及
//!   [`IngressContext`]（推导出来的租户账户 id、主体、凭证 id）。数据类别绑定在租户上，不在模型别名上，
//!   换具体模型名绕不过去。
//! - **S7（本地到云的云端档）**：组织节点用 `Node` 凭证调云，不带 `X-iDoris-Member`；个人云账号用成员凭证。
//!   两者都只需要「地址加密钥」，调 `POST /api/v1/chat/completions`。节点凭证没有主体，成员级上限按（租户，凭证）算。
//!
//! # 对设计稿含糊处的理解
//!
//! 列在交付说明里，没有悄悄拍板。

use std::sync::Arc;

use axum::extract::State;
use axum::middleware::{from_fn, from_fn_with_state};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use idoris_tenancy::accounts::AccountStore;

use super::clock::Clock;
use crate::auth::VirtualKeyAuthenticator;
use crate::models::{ModelEntry, ModelsResponse};
use crate::{AppState, control};

pub mod abuse;
pub mod bind;
pub mod bucket;
pub mod caller;
pub mod config;
pub mod cors;
mod guard;
pub mod limits;
pub mod net;
pub mod reason;

pub use caller::{CallerResolver, IngressContext, Refusal, ResolvedCaller};
pub use config::{IngressConfig, TenantProfile};
pub use limits::{CallerLimits, LevelLimits, LimitKeys, Limiter, Scope};
pub use net::{ClientAddr, TrustedProxies};
pub use reason::IngressReason;

/// 提交假名成员 id 的请求头。**Service 凭证每个请求都必须带**（eduai 后端代理）；成员凭证带了必须是自己；节点凭证不能带。
/// 值是开通方提供的稳定假名 id（`compute-contracts::ids::PrincipalId` 的字符集），不是姓名、邮箱。
pub const HEADER_MEMBER: &str = "X-iDoris-Member";

/// 被限流时说明哪一级拦的（`member` / `tenant` / `platform`），浏览器可读（见 [`cors`]）。
pub const HEADER_LIMIT_SCOPE: &str = "X-iDoris-Limit-Scope";

/// 云入口：配置、认证器、调用方解析、三级限流、防滥用、时钟。挂在 `AppState::cloud_ingress` 上。
pub struct CloudIngress {
    pub(crate) config: Arc<IngressConfig>,
    pub(crate) authenticator: VirtualKeyAuthenticator,
    pub(crate) resolver: Arc<CallerResolver>,
    pub(crate) limiter: Arc<Limiter>,
    pub(crate) anonymous: abuse::AnonymousLimiter,
    pub(crate) failures: abuse::FailureThrottle,
    pub(crate) clock: Arc<dyn Clock>,
}

impl std::fmt::Debug for CloudIngress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudIngress").finish_non_exhaustive()
    }
}

impl CloudIngress {
    /// 组装。`cloud` 用来做一致性检查：云入口生效时，付款上下文只能由凭证解析，
    /// **静态调用方簿（`callers:`，S4 的测试替身）必须是空的**——留着它只会让运维以为它还在起作用。
    pub fn new(
        config: IngressConfig,
        accounts: Arc<AccountStore>,
        authenticator: VirtualKeyAuthenticator,
        clock: Arc<dyn Clock>,
        cloud: &super::CloudUpstream,
    ) -> Result<Self, String> {
        if !cloud.callers.is_empty() {
            return Err(
                "云上游配置里还有静态调用方 callers:：云入口生效时付款上下文只由凭证解析，\
                 静态调用方簿只是 S4 的测试夹具，请把它从生产配置里删掉（写 callers: [] 或整段去掉）"
                    .to_string(),
            );
        }
        let config = Arc::new(config);
        let now = clock.now();
        Ok(Self {
            resolver: Arc::new(CallerResolver::new(accounts, Arc::clone(&config))),
            limiter: Limiter::new(config.platform, now.0),
            anonymous: abuse::AnonymousLimiter::new(
                config.anonymous_per_source,
                config.anonymous_global,
                now.0,
            ),
            failures: abuse::FailureThrottle::new(
                config.auth_failure_max,
                config.auth_failure_window_secs,
            ),
            config,
            authenticator,
            clock,
        })
    }

    /// 读环境变量 [`config::CONFIG_ENV`] 指向的文件。**云形态必须配**：没设就是启动错误。
    pub fn from_env(
        accounts: Arc<AccountStore>,
        authenticator: VirtualKeyAuthenticator,
        cloud: &super::CloudUpstream,
    ) -> Result<Arc<Self>, String> {
        let Some(path) = crate::config::optional_env_path(config::CONFIG_ENV)? else {
            return Err(format!(
                "云形态（IDORIS_FORM=cloud）必须设置 {}：它声明受信反向代理、各级上限、防滥用和每个租户的数据处理要求",
                config::CONFIG_ENV
            ));
        };
        let config = IngressConfig::load(&path)?;
        let ingress = Self::new(
            config,
            accounts,
            authenticator,
            Arc::new(super::SystemClock),
            cloud,
        )?;
        Ok(Arc::new(ingress))
    }

    pub fn config(&self) -> &IngressConfig {
        &self.config
    }

    /// 启动时的体检，每条是一句给运维看的警告（不是错误：运维可能先写配置、后建账户）：
    /// 配置里声明的租户在账户库里不存在，或者不是账户树里平台根之下的顶层客户——后者的声明永远不会生效，
    /// 因为租户总是从付款账户往上数的顶层客户（见 [`caller`]）。
    pub fn startup_warnings(&self, accounts: &AccountStore) -> Vec<String> {
        let mut warnings = Vec::new();
        for tenant in self.config.tenants() {
            match accounts.ancestors(&tenant.account) {
                Ok(chain) => {
                    let top = caller::derive_tenant(&chain).map(|account| account.id.clone());
                    if top.as_ref() != Some(&tenant.account) {
                        warnings.push(format!(
                            "租户 {} 不是账户树里平台根之下的顶层客户（它的顶层客户是 {}）：这份声明永远不会生效，\
                             请把声明写在顶层客户上",
                            tenant.account,
                            top.map_or_else(|| "?".to_string(), |id| id.to_string())
                        ));
                    }
                }
                Err(idoris_tenancy::accounts::AccountStoreError::NotFound { .. }) => {
                    warnings.push(format!(
                        "租户 {} 在云入口配置里声明了，但账户库里还没有这个账户：账户建好之前它的请求会被拒绝",
                        tenant.account
                    ));
                }
                Err(error) => {
                    warnings.push(format!("无法检查租户 {}：{error}", tenant.account));
                }
            }
        }
        warnings
    }

    /// 三级限流器（测试和诊断用）。
    pub fn limiter(&self) -> &Arc<Limiter> {
        &self.limiter
    }
}

/// `GET /api/v1/models`：云路由簿里的模型名，OpenAI 的列表形状。
async fn models(State(state): State<Arc<AppState>>) -> Response {
    let Some(cloud) = state.cloud.as_ref() else {
        return crate::error_envelope(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            reason::IngressReason::CloudUnavailable.as_str(),
            "no cloud upstream is configured",
        );
    };
    let data = cloud
        .routes
        .models()
        .map(|id| ModelEntry {
            id: id.to_string(),
            object: "model",
            owned_by: "idoris".to_string(),
        })
        .collect();
    Json(ModelsResponse {
        object: "list",
        data,
    })
    .into_response()
}

/// 云形态的整个 HTTP 应用。见模块文档的路由表和中间件顺序（`guard`）。
///
/// 控制面（`/control/v1`）合并在数据面的层**之后**，和 `build_app` 一样：它不经过数据面的中间件，自己认证、自己授权。
pub fn build_cloud_app(state: AppState, ingress: Arc<CloudIngress>) -> Router {
    let state = Arc::new(state);
    // `MethodRouter::route_layer`：认证只包住真正匹配到方法的处理函数，不包住 `fallback`。所以没有的路径、
    // 路径对但方法不对（包括 OPTIONS）一律是 404，不认证、不碰库，也不会把 404 变成 401。
    let guard = from_fn_with_state(Arc::clone(&ingress), guard::data_plane_guard);
    let mut app = Router::new().route("/health", get(crate::health)).route(
        "/api/v1/models",
        get(models)
            .fallback(crate::not_found)
            .route_layer(guard.clone()),
    );
    if state.cloud.is_some() {
        app = app.route(
            "/api/v1/chat/completions",
            post(super::chat_completions)
                .fallback(crate::not_found)
                .route_layer(guard),
        );
    }
    let mut app = app
        .fallback(crate::not_found)
        .layer(from_fn_with_state(Arc::clone(&ingress), guard::edge_guard))
        .layer(from_fn(crate::connection::request_lifecycle_middleware))
        .layer(from_fn_with_state(
            Arc::clone(&state),
            crate::record_id_middleware,
        ))
        .layer(from_fn(guard::strip_untrusted_headers))
        .with_state(Arc::clone(&state));

    if ingress.config.control_plane_public
        && let Some(plane) = state.control.clone()
    {
        app = app.merge(control::router(plane).layer(from_fn_with_state(
            Arc::clone(&ingress),
            guard::control_body_deadline,
        )));
    }
    app
}
